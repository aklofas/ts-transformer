//! TS sync state machine. Locks on the 0x47 sync byte spaced 188 bytes apart.
//!
//! ## States
//!
//! - **HUNT** — scanning forward byte-by-byte for the first 0x47.
//! - **VERIFY** — found a candidate 0x47; peeking ahead 188*N bytes to
//!   confirm N more sync bytes at the expected positions before committing.
//!   We require 4 confirming sync bytes total (count 1..=4) before locking.
//!   VERIFY is peek-only: no bytes are consumed from the buffer. That means
//!   every byte fed during verification is still available for LOCKED to drain
//!   as real packets.
//! - **LOCKED** — confirmed alignment. Emit 188-byte packets one at a time,
//!   consuming from the front of the buffer. If the front byte ever stops
//!   being 0x47, fall back to HUNT.
//!
//! This mirrors the posture of libavformat's `mpegts_resync` and ffmpeg's
//! two-pass sync strategy: don't declare lock until you've seen several
//! back-to-back aligned sync bytes.

use alloc::vec::Vec;
use tst_core::mpegts::common::{SRT_TS_BUNDLE_BYTES, TS_PACKET_SIZE, TS_SYNC_BYTE};

/// Current state of the sync state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// Looking for the first 0x47 sync byte.
    Hunt,
    /// Found a candidate 0x47 at `buf[0]`. `count` is the number of confirmations
    /// seen so far — starts at 1 (the initial 0x47 in HUNT transitions here).
    /// After 4 total confirmations (`new_count >= 4`) we transition to LOCKED.
    Verify { count: u8 },
    /// Confirmed 0x47 alignment. Emitting 188-byte packets from buf[0..188].
    Locked,
}

/// Stateful TS sync recoverer.
///
/// Feed bytes via [`push`][Self::push] and drain aligned 188-byte packets via
/// [`next_packet`][Self::next_packet]. Bytes not yet consumed by a packet
/// stay in the internal buffer; push more data when `next_packet` returns
/// `None`.
///
/// Internally uses a ring buffer with a `head` cursor: consuming a packet
/// advances `head` by 188 without moving any memory. A compaction pass
/// (one memmove of the live bytes only) runs inside [`Self::push`] when the dead
/// prefix exceeds a threshold, amortising the cost across many emitted
/// packets rather than paying a per-packet memmove as `drain` does.
#[derive(Debug)]
pub struct Syncer {
    state: SyncState,
    /// Raw storage. Invariant: `buf.len() == head + len` at all times.
    /// The live (unconsumed) window is `buf[head .. head + len]`.
    /// The dead prefix `buf[0 .. head]` is reclaimed lazily by `compact`.
    buf: Vec<u8>,
    /// Index of the first unconsumed byte in `buf`.
    head: usize,
    /// Number of unconsumed bytes.
    len: usize,
    /// Bytes drained while scanning for alignment (HUNT skips + VERIFY
    /// single-byte drops on failed confirmation).
    pub(crate) bytes_skipped_for_sync: u64,
    /// Number of times the syncer has transitioned from HUNT/VERIFY to
    /// LOCKED — counts successful lock acquisitions (initial lock-on and
    /// re-locks after sync loss).
    pub(crate) resync_events: u64,
}

impl Syncer {
    /// Create a new [`Syncer`] starting in the HUNT state with an empty buffer.
    pub fn new() -> Self {
        Self {
            state: SyncState::Hunt,
            buf: Vec::new(),
            head: 0,
            len: 0,
            bytes_skipped_for_sync: 0,
            resync_events: 0,
        }
    }

    /// Move live bytes to the front of `buf` to reclaim the dead prefix.
    ///
    /// After this call: `head == 0`, `buf[0..len]` is the live window,
    /// and `buf.len() == len` (tail trimmed).
    #[inline]
    fn compact(&mut self) {
        self.buf.copy_within(self.head..self.head + self.len, 0);
        self.buf.truncate(self.len);
        self.head = 0;
    }

    /// Append incoming bytes to the internal buffer.
    ///
    /// Compacts (memmoves the live region to offset 0) when the dead prefix
    /// exceeds one SRT-datagram worth of bytes (~1316). In the steady-state
    /// receive loop — where each `push` delivers one SRT datagram and
    /// `next_packet` drains ~7 packets before the next push — this means
    /// one memmove per ~7 packet emits rather than one memmove per packet.
    pub fn push(&mut self, bytes: &[u8]) {
        // Compact when the dead prefix is getting large. The threshold of SRT_TS_BUNDLE_BYTES
        // (one SRT datagram) amortises compaction to approximately once per
        // push. Unbounded growth in pure HUNT is prevented by the HUNT-arm
        // discard (a sync-free window is dropped, growing `head` so this
        // compaction reclaims it) — not by this threshold alone.
        if self.head >= SRT_TS_BUNDLE_BYTES {
            self.compact();
        }
        // Invariant: buf.len() == head + len. extend_from_slice appends at
        // buf.len() = head + len, making the new live window
        // buf[head .. head + len + bytes.len()]. Correct.
        self.buf.extend_from_slice(bytes);
        self.len += bytes.len();
    }

    /// Reset to HUNT and discard any buffered bytes.
    ///
    /// Intended for reconnect scenarios where the underlying transport has
    /// been re-established: bytes left over from a dead connection cannot
    /// straddle the reconnect boundary and must not contribute to the new
    /// alignment search. Higher-level shells (a future `ManagedReceiver`)
    /// call this after a transport rebuild before feeding fresh bytes.
    ///
    /// Does NOT reset stat counters — those are owned by
    /// [`Receiver`][crate::Receiver] and reset via
    /// [`Receiver::reset_stats`][crate::Receiver::reset_stats].
    pub fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
        self.buf.clear();
        self.state = SyncState::Hunt;
    }

    /// Zero the sync-recovery counters. Called by `Receiver::reset_stats`.
    pub(crate) fn reset_stats(&mut self) {
        self.bytes_skipped_for_sync = 0;
        self.resync_events = 0;
    }

    /// Pull the next 188-byte aligned packet from the buffer, if one is ready.
    ///
    /// Returns `None` when more bytes must be [`push`][Self::push]ed before
    /// the next packet can be emitted. The caller should feed more data and
    /// call `next_packet` again.
    ///
    /// VERIFY state is peek-only: bytes are not consumed while confirming
    /// alignment, so all bytes pushed during the verification window remain
    /// available for LOCKED to emit as real packets.
    ///
    /// Returns `[u8; 188]` by value — no heap allocation on the hot path.
    pub fn next_packet(&mut self) -> Option<[u8; 188]> {
        loop {
            match self.state {
                SyncState::Hunt => {
                    // Scan the live region for the first 0x47 sync byte.
                    let live = &self.buf[self.head..self.head + self.len];
                    let pos = match live.iter().position(|&b| b == TS_SYNC_BYTE) {
                        Some(p) => p,
                        None => {
                            // No sync byte anywhere in the live window — none of
                            // these bytes can begin a packet. Discard the whole
                            // window instead of letting it accumulate without
                            // bound (a hostile 0x47-free stream would OOM the
                            // receiver). Mirrors the sender-side framer's
                            // no-sync discard.
                            self.bytes_skipped_for_sync += self.len as u64;
                            self.head += self.len;
                            self.len = 0;
                            return None;
                        }
                    };
                    self.bytes_skipped_for_sync += pos as u64;
                    // Advance head past skipped bytes — no memmove needed.
                    self.head += pos;
                    self.len -= pos;
                    // buf[head] is now 0x47. One confirmation seen.
                    self.state = SyncState::Verify { count: 1 };
                    // Fall through to VERIFY on the next loop iteration.
                }
                SyncState::Verify { count } => {
                    // We have a candidate 0x47 at buf[head]. Check whether
                    // buf[head + 188 * count] is also 0x47.
                    //
                    // `need` ensures the index we're about to peek at actually
                    // exists in the buffer before we read it: the candidate
                    // plus `count` full strides. Exactly four packets (752
                    // bytes) therefore lock — no extra look-ahead byte, which
                    // used to strand a four-packet stream and the last packets
                    // before EOF after any resync (CORR-18).
                    let need = TS_PACKET_SIZE * (count as usize + 1);
                    if self.len < need {
                        return None;
                    }
                    if self.buf[self.head + TS_PACKET_SIZE * count as usize] == TS_SYNC_BYTE {
                        let new_count = count + 1;
                        if new_count >= 4 {
                            // Four confirmations — alignment is solid.
                            self.state = SyncState::Locked;
                            // Count each lock acquisition (initial + re-locks
                            // after sync loss) as one resync event, mirroring
                            // the sender-side framing convention.
                            self.resync_events += 1;
                        } else {
                            self.state = SyncState::Verify { count: new_count };
                        }
                        // Loop: either emit from LOCKED or check next confirmation.
                    } else {
                        // Candidate failed. Drop one byte and re-hunt.
                        // We can't just advance 188 bytes because buf[head+1..head+188]
                        // may contain a real sync byte for a different alignment.
                        self.bytes_skipped_for_sync += 1;
                        self.head += 1;
                        self.len -= 1;
                        self.state = SyncState::Hunt;
                    }
                }
                SyncState::Locked => {
                    if self.len < TS_PACKET_SIZE {
                        return None;
                    }
                    if self.buf[self.head] != TS_SYNC_BYTE {
                        // Lost sync — corrupted stream or a gap in the source.
                        // Fall back to HUNT; the loop will scan for the next
                        // 0x47 without consuming the non-sync byte here
                        // (HUNT will drain up to it).
                        self.state = SyncState::Hunt;
                        continue;
                    }
                    // Copy 188 bytes into a stack-allocated array — no heap alloc.
                    let mut pkt = [0u8; 188];
                    pkt.copy_from_slice(&self.buf[self.head..self.head + TS_PACKET_SIZE]);
                    // Advance the head cursor — no memmove needed.
                    self.head += TS_PACKET_SIZE;
                    self.len -= TS_PACKET_SIZE;
                    return Some(pkt);
                }
            }
        }
    }

    /// Current sync state. Exposed for testing and diagnostics.
    #[cfg(test)]
    pub fn state(&self) -> SyncState {
        self.state
    }

    /// Length of the internal storage Vec. Exposed for testing boundedness.
    #[cfg(test)]
    fn buf_len(&self) -> usize {
        self.buf.len()
    }
}

impl Default for Syncer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a syntactically valid 188-byte TS packet with the given PID.
    /// Payload bytes are 0xFF (don't-care filler).
    fn ts_packet(pid: u16) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47; // sync byte
        buf[1] = 0x40 | ((pid >> 8) as u8 & 0x1F); // PUSI + PID high
        buf[2] = (pid & 0xFF) as u8; // PID low
        buf[3] = 0x10; // no adaptation field, has payload
        buf
    }

    /// Feed N packets, push all at once, drain everything. The Syncer is in
    /// peek-only VERIFY mode, so all 6 bytes fed during alignment confirmation
    /// remain in the buffer and are emitted once LOCKED.
    #[test]
    fn locks_after_four_packets() {
        let mut s = Syncer::new();
        let mut buf = Vec::new();
        for i in 0..6 {
            buf.extend_from_slice(&ts_packet(i));
        }
        s.push(&buf);
        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        // VERIFY is peek-only: the 4 confirmation bytes aren't consumed, so all
        // 6 packets are available once the syncer transitions to LOCKED.
        assert_eq!(got, 6);
        assert_eq!(s.state(), SyncState::Locked);
    }

    /// Garbage prefix before the first valid TS stream — HUNT scans past it.
    #[test]
    fn skips_garbage_prefix() {
        let mut s = Syncer::new();
        let mut buf = vec![0xAAu8; 100]; // 100 bytes of garbage before any 0x47
        for i in 0..5 {
            buf.extend_from_slice(&ts_packet(i));
        }
        s.push(&buf);
        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        assert_eq!(got, 5);
    }

    /// If alignment is lost mid-VERIFY (a sync byte at the expected position
    /// is missing), the syncer drops one byte and re-hunts rather than
    /// discarding a potentially valid alignment just 1 byte forward.
    #[test]
    fn recovers_from_interrupted_verify() {
        let mut s = Syncer::new();
        // Start with a fake 0x47 (only one, no subsequent sync bytes 188 apart).
        // Then 400 bytes of filler followed by 5 real packets.
        let mut buf = vec![0x47u8]; // triggers Verify { count: 1 }
        buf.extend_from_slice(&[0xBBu8; 187]); // not a real packet
        buf.extend_from_slice(&[0xCCu8; 213]); // more garbage (no 0x47 at 188)
        for i in 0..5 {
            buf.extend_from_slice(&ts_packet(i));
        }
        s.push(&buf);
        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        assert_eq!(got, 5);
    }

    /// After locking, a corrupted packet (missing sync byte) causes a
    /// re-hunt. The syncer recovers and emits the remaining valid packets.
    #[test]
    fn recovers_after_sync_loss_in_locked() {
        let mut s = Syncer::new();
        let mut buf = Vec::new();
        // 5 good packets to establish lock.
        for i in 0..5u16 {
            buf.extend_from_slice(&ts_packet(i));
        }
        // One corrupted packet: first byte is 0x00 instead of 0x47. None of
        // the remaining bytes in this packet are 0x47 (pid=200 → high byte
        // 0x40|0=0x40, low byte 0xC8; filler is 0xFF).
        let mut bad = ts_packet(200);
        bad[0] = 0x00;
        buf.extend_from_slice(&bad);
        // 6 more good packets after the bad one — more than enough for
        // VERIFY to re-lock (needs 4 confirmations = 4 packets = 752 bytes).
        for i in 0..6u16 {
            buf.extend_from_slice(&ts_packet(i + 100));
        }
        s.push(&buf);

        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        // 5 leading packets + 6 trailing packets (bad packet's 188 bytes are
        // consumed by HUNT as it scans for the next 0x47).
        assert_eq!(got, 11);
    }

    /// A hostile peer streaming bytes with no 0x47 sync byte (e.g. continuous
    /// 0x00) must not make the internal buffer grow without bound. The HUNT arm
    /// discards a sync-free live window instead of accumulating it. After all
    /// that garbage, the syncer must still lock on a subsequent run of real
    /// packets.
    #[test]
    fn hostile_zero_stream_does_not_grow_unbounded() {
        let mut s = Syncer::new();
        // Feed 2000 SRT-datagram-sized chunks of 0x00 — none contain 0x47.
        let garbage = vec![0x00u8; SRT_TS_BUNDLE_BYTES];
        for _ in 0..2000 {
            s.push(&garbage);
            // Drain: there is no sync byte, so this returns None each time.
            assert!(s.next_packet().is_none());
        }
        // Without the HUNT-arm discard, buf would hold ~2.6 MB. With it, the
        // dead prefix is reclaimed by push()'s compaction, so the buffer stays
        // bounded to a small multiple of one SRT datagram.
        assert!(
            s.buf_len() < 3 * SRT_TS_BUNDLE_BYTES,
            "buffer grew to {} bytes (hostile sync-free stream not discarded)",
            s.buf_len()
        );
        // Sanity: the syncer still locks on real packets after the garbage.
        let mut buf = Vec::new();
        for i in 0..5u16 {
            buf.extend_from_slice(&ts_packet(i));
        }
        s.push(&buf);
        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        assert_eq!(got, 5);
    }

    /// Verify that the ring-buffer head-cursor invariant holds across multiple
    /// interleaved push/drain cycles. Specifically: push must append correctly
    /// even when head > 0 (dead prefix has accumulated from previous drains).
    #[test]
    fn incremental_push_drain() {
        let mut s = Syncer::new();
        // Push 5 packets at once to lock the syncer.
        let mut init = Vec::new();
        for i in 0..5u16 {
            init.extend_from_slice(&ts_packet(i));
        }
        s.push(&init);
        // Drain all 5.
        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        assert_eq!(got, 5);
        // head is now 5 * 188 = 940 (below the 1316 compaction threshold).
        // Push 3 more one at a time and drain each — exercises push with head > 0.
        for i in 5u16..8 {
            s.push(&ts_packet(i));
            let pkt = s.next_packet();
            assert!(pkt.is_some(), "packet {i} should be emitted");
            assert_eq!(pkt.unwrap()[0], 0x47);
        }
    }

    /// CORR-18: four aligned packets are exactly the four confirmations
    /// VERIFY needs. The old `+ 1` look-ahead demanded one byte of a fifth
    /// packet, so a four-packet stream (or the last four packets before
    /// EOF after any resync) never emitted at all.
    #[test]
    fn four_packets_lock_and_emit_without_a_fifth_byte() {
        let mut s = Syncer::new();
        let mut buf = Vec::new();
        for i in 0..4u16 {
            buf.extend_from_slice(&ts_packet(i));
        }
        s.push(&buf);
        let mut got = 0;
        while s.next_packet().is_some() {
            got += 1;
        }
        assert_eq!(
            got, 4,
            "all four packets must emit once VERIFY reaches count 4"
        );
        assert_eq!(s.state(), SyncState::Locked);
    }
}
