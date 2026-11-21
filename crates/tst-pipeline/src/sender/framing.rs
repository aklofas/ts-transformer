//! TS sync-acquisition and loss-of-sync recovery state machine.
//!
//! Standalone — no transport dependency. Caller pushes arbitrary-aligned
//! bytes; framer emits 7-packet (1316-byte) bundles whenever enough
//! aligned packets have accumulated.
//!
//! Acquisition (UNSYNCED): scan input for 0x47; for each candidate
//! position P, verify by checking 0x47 at P+188 and P+376 (three
//! consecutive sync bytes, ~1/65,536 false positive rate for uniformly
//! random byte streams). On verify, transition to SYNCED with P as
//! packet boundary 0.
//!
//! A stream that has been aligned since its first byte does not have to
//! wait for the third packet: while no byte has been skipped and no
//! boundary has failed, [`TsFraming::flush`] sends the whole packets held
//! when every boundary present reads 0x47 (the check STRICT mode makes on
//! every push), so a stream of one or two packets is delivered. After the
//! first skipped byte or failed boundary only the three-packet rule sends
//! anything.
//!
//! Loss detection (SYNCED): every 188-byte boundary in the staging
//! buffer must read 0x47. When boundary `i` of a candidate bundle fails,
//! the `i` packets before it (each already seen to start with 0x47) go
//! out as one short message, resync_events is incremented and the state
//! returns to UNSYNCED with the rest of the buffer kept: the scan resumes
//! at the failed boundary, so a valid stream after the damage is not
//! thrown away with it.
//!
//! End of stream: [`TsFraming::finish`] flushes and then goes on scanning
//! what is left with the same three-packet rule, because no later push
//! will. It scans everything held: the limit that makes a push give up on
//! input that does not look like TS is not applied, so the result does not
//! depend on where the limit happened to interrupt a push. What still
//! cannot be verified (a partial packet, fewer than three packets behind
//! damage, bytes that are not TS) stays buffered and is counted by
//! [`TsFraming::buffered_bytes`]; it is never sent.
//!
//! STRICT mode checks every boundary of a call before committing any of
//! it, so a rejected call leaves the framer exactly as it was.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use thiserror::Error;
use tst_core::mpegts::common::{
    SRT_TS_BUNDLE_BYTES as SRT_BUNDLE_BYTES, SRT_TS_BUNDLE_PACKETS as SRT_BUNDLE_PACKETS,
    TS_PACKET_SIZE, TS_SYNC_BYTE as SYNC_BYTE,
};
use tst_core::mpegts::sync::{TsSync, aligned_packets_at_end};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TsFramingMode {
    /// Default: skip misaligned prefix until sync; auto-resync on loss.
    #[default]
    Recover,
    /// Any non-0x47 at expected boundary → error immediately.
    Strict,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TsFramingError {
    #[error("TS sync byte not found at expected boundary (offset {offset})")]
    SyncLost { offset: u64 },
    /// RECOVER mode has consumed more than
    /// [`super::SenderConfig::max_unsynced_bytes`] without acquiring sync
    /// (it routes to [`crate::shell_error::ShellErrorKind::InputMalformed`]).
    /// The unsynced-byte counter resets after the error is raised, so the
    /// watchdog fires again only after another full `max_unsynced_bytes`
    /// of garbage.
    #[error(
        "exceeded max_unsynced_bytes ({max}) without acquiring sync; \
         input does not look like a TS stream"
    )]
    NoSyncAfterLimit { max: u64 },
    /// The stream ended with `bytes` bytes the framer could not send as
    /// whole aligned TS packets: a partial last packet, packets behind
    /// damage that fewer than three sync bytes confirm, or bytes that are
    /// not TS (it routes to
    /// [`crate::shell_error::ShellErrorKind::InputMalformed`]). Everything
    /// that could be verified was sent first.
    #[error(
        "{bytes} byte(s) at the end of the stream were not sent: \
         not whole aligned TS packets"
    )]
    UnsentTail { bytes: u64 },
}

#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SenderStats {
    pub bytes_pushed: u64,
    pub bytes_skipped_for_sync: u64,
    pub resync_events: u64,
    pub packets_sent: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Unsynced,
    Synced,
}

pub struct TsFraming {
    max_unsynced_bytes: usize,
    state: State,
    /// Bytes pending: in UNSYNCED these are the scan window; in SYNCED
    /// they're whole-packet aligned and accumulating until a 7-packet
    /// bundle is ready.
    buffer: VecDeque<u8>,
    /// In UNSYNCED: count of bytes consumed scanning for sync.
    unsynced_consumed: usize,
    /// Set by [`Self::push_strict`]: every boundary in `buffer` has been
    /// checked, so `flush` may emit it before the sync threshold.
    strict: bool,
    /// RECOVER mode: true until a byte is skipped. While it holds and the
    /// state is UNSYNCED, the buffer starts at the stream's first byte.
    /// A boundary that fails after sync needs no write here: its byte
    /// stays at the buffer's head, where it refuses the early flush until
    /// the scan skips it.
    aligned_from_start: bool,
    /// Test builds only: bytes the sync scan has looked at, to pin that
    /// its work is linear in its input.
    #[cfg(test)]
    scan_steps: u64,
    stats: SenderStats,
}

impl TsFraming {
    pub fn new(max_unsynced_bytes: usize) -> Self {
        Self {
            max_unsynced_bytes,
            state: State::Unsynced,
            buffer: VecDeque::new(),
            unsynced_consumed: 0,
            strict: false,
            aligned_from_start: true,
            #[cfg(test)]
            scan_steps: 0,
            stats: SenderStats::default(),
        }
    }

    pub fn is_synced(&self) -> bool {
        self.state == State::Synced
    }

    pub fn stats(&self) -> SenderStats {
        self.stats.clone()
    }

    /// Zero all stats counters. The framing-state machine (sync-byte
    /// recovery state, partial-bundle buffer) is NOT reset — only the
    /// counters on top of it.
    pub fn reset_stats(&mut self) {
        self.stats = SenderStats::default();
    }

    /// Push bytes (RECOVER mode): returns any complete 7-packet bundles
    /// emitted. Sync is acquired silently; loss-of-sync is silently
    /// recovered. Stats reflect events.
    ///
    /// # Errors
    /// [`TsFramingError::NoSyncAfterLimit`] if scanning for sync consumes
    /// more than `max_unsynced_bytes` (see [`Self::new`]) without
    /// acquiring it. The error is raised only by a call that has framed no
    /// bundle: a call that framed one before the limit was crossed stops
    /// scanning there and returns its bundles, and the next call raises
    /// the error. A bundle framed before the loss is therefore always
    /// returned, however the input is chunked.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(Vec<Vec<u8>>, &SenderStats), TsFramingError> {
        self.stats.bytes_pushed += bytes.len() as u64;
        self.buffer.extend(bytes.iter().copied());
        let mut bundles = Vec::new();
        loop {
            match self.state {
                State::Unsynced => {
                    // Raising the watchdog error here would discard the
                    // bundles already framed in this call, and the caller
                    // is told not to resend consumed input.
                    if !self.try_acquire(!bundles.is_empty())? {
                        break;
                    }
                }
                State::Synced => {
                    if !self.try_emit(&mut bundles) {
                        break;
                    }
                }
            }
        }
        Ok((bundles, &self.stats))
    }

    /// Push bytes (STRICT mode): returns any bundles emitted. Errors on
    /// any misalignment — no recovery.
    ///
    /// # Errors
    /// [`TsFramingError::SyncLost`] when any packet boundary whose first
    /// byte is present (the bytes still buffered from earlier calls
    /// followed by `bytes`) does not read 0x47. `offset` counts from the
    /// first byte still buffered before this call. On error nothing
    /// changes — buffer, stats and sync state are as they were — so the
    /// caller may retry with corrected input.
    pub fn push_strict(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, TsFramingError> {
        // Validate before committing anything: the buffered bytes start on
        // a packet boundary (everything buffered here was validated by an
        // earlier call), so the boundaries of the whole call sit at
        // multiples of 188 into `buffer ∥ bytes`.
        if let Some(k) = self
            .buffer
            .iter()
            .chain(bytes)
            .step_by(TS_PACKET_SIZE)
            .position(|&b| b != SYNC_BYTE)
        {
            return Err(TsFramingError::SyncLost {
                offset: (k * TS_PACKET_SIZE) as u64,
            });
        }
        self.strict = true;
        self.stats.bytes_pushed += bytes.len() as u64;
        self.buffer.extend(bytes.iter().copied());
        // Sync is declared once the first three boundaries are present;
        // with fewer the bytes wait for flush, which emits them anyway
        // because every boundary was checked above.
        if self.state == State::Unsynced && self.buffer.len() > 2 * TS_PACKET_SIZE {
            self.state = State::Synced;
        }
        let mut bundles = Vec::new();
        if self.state == State::Synced {
            while self.buffer.len() >= SRT_BUNDLE_BYTES {
                bundles.push(self.buffer.drain(..SRT_BUNDLE_BYTES).collect());
                self.stats.packets_sent += SRT_BUNDLE_PACKETS as u64;
            }
        }
        Ok(bundles)
    }

    /// In UNSYNCED: try to acquire sync via 3-byte verify. Returns
    /// `Ok(true)` if state changed (so the caller's outer loop
    /// re-evaluates), `Ok(false)` if we don't yet have enough bytes to
    /// verify, `Err` if `max_unsynced_bytes` was exceeded first. With
    /// `defer_limit`, exceeding the limit stops the scan with `Ok(false)`
    /// instead, leaving the counter over the limit so the next call
    /// raises the error before scanning.
    fn try_acquire(&mut self, defer_limit: bool) -> Result<bool, TsFramingError> {
        // Scan from current buffer head for a candidate position. Need at
        // least 2*188+1 = 377 bytes after the candidate to verify P, P+188, P+376.
        const NEEDED: usize = 2 * TS_PACKET_SIZE + 1;
        const ACQUIRE: TsSync = TsSync::strict(3);
        if self.unsynced_limit_hit(defer_limit)? {
            return Ok(false);
        }
        while !self.buffer.is_empty() {
            // Find next 0x47 in buffer.
            let candidate_idx = match self.buffer.iter().position(|&b| b == SYNC_BYTE) {
                Some(i) => i,
                None => {
                    // No sync byte anywhere; discard everything we've buffered.
                    let n = self.buffer.len();
                    #[cfg(test)]
                    {
                        self.scan_steps += n as u64;
                    }
                    self.buffer.clear();
                    self.unsynced_consumed += n;
                    self.aligned_from_start = false;
                    self.stats.bytes_skipped_for_sync += n as u64;
                    self.unsynced_limit_hit(defer_limit)?;
                    return Ok(false);
                }
            };
            #[cfg(test)]
            {
                self.scan_steps += candidate_idx as u64 + 1;
            }
            // Discard everything before the candidate.
            if candidate_idx > 0 {
                for _ in 0..candidate_idx {
                    self.buffer.pop_front();
                }
                self.unsynced_consumed += candidate_idx;
                self.aligned_from_start = false;
                self.stats.bytes_skipped_for_sync += candidate_idx as u64;
            }
            // Now buffer[0] == 0x47. Need 377 bytes to verify.
            if self.buffer.len() < NEEDED {
                self.unsynced_limit_hit(defer_limit)?;
                return Ok(false);
            }
            if ACQUIRE.verify(self.buffer.make_contiguous(), 0) == Some(true) {
                self.state = State::Synced;
                self.unsynced_consumed = 0;
                return Ok(true);
            }
            // False candidate. Discard just the leading 0x47 and rescan.
            self.buffer.pop_front();
            self.unsynced_consumed += 1;
            self.aligned_from_start = false;
            self.stats.bytes_skipped_for_sync += 1;
            if self.unsynced_limit_hit(defer_limit)? {
                return Ok(false);
            }
        }
        Ok(false)
    }

    /// In SYNCED: try to emit one bundle. Returns true if the caller
    /// should loop (a bundle went out, or loss-of-sync returned the state
    /// to UNSYNCED), false if there are not enough bytes yet.
    fn try_emit(&mut self, bundles: &mut Vec<Vec<u8>>) -> bool {
        if self.buffer.len() < SRT_BUNDLE_BYTES {
            return false;
        }
        // Verify every 188-byte boundary in the candidate bundle. The
        // decision waits for a full bundle's bytes even when an earlier
        // boundary could be checked sooner, so how the input is chunked
        // never changes which messages go out.
        let bad = (0..SRT_BUNDLE_PACKETS).find(|&i| self.buffer[i * TS_PACKET_SIZE] != SYNC_BYTE);
        let good = bad.unwrap_or(SRT_BUNDLE_PACKETS);
        if good > 0 {
            bundles.push(self.buffer.drain(..good * TS_PACKET_SIZE).collect());
            self.stats.packets_sent += good as u64;
        }
        if bad.is_some() {
            // Sync lost at the failed boundary. Keep the rest: try_acquire
            // skips (and counts) the damaged bytes up to the next verified
            // sync point, so a valid stream after the damage survives.
            self.state = State::Unsynced;
            self.stats.resync_events += 1;
        }
        true
    }

    /// Errors once `unsynced_consumed` exceeds `max_unsynced_bytes`, then
    /// resets the counter so the watchdog can fire again after another
    /// full `max_unsynced_bytes` of unrecovered garbage. With `defer`, an
    /// exceeded limit returns `Ok(true)` (stop scanning) and keeps the
    /// counter for the next call.
    fn unsynced_limit_hit(&mut self, defer: bool) -> Result<bool, TsFramingError> {
        if self.unsynced_consumed <= self.max_unsynced_bytes {
            return Ok(false);
        }
        if defer {
            return Ok(true);
        }
        self.unsynced_consumed = 0;
        Err(TsFramingError::NoSyncAfterLimit {
            max: self.max_unsynced_bytes as u64,
        })
    }

    /// Emit any pending whole packets as a partial bundle (1-6 packets).
    /// Called explicitly via flush() or implicitly on close.
    ///
    /// Only packets whose sync byte reads 0x47 go out: the boundaries
    /// past the third have not been checked yet (a bundle is checked only
    /// once all seven packets are present). Damage at flush counts one
    /// `resync_events`, returns the state to UNSYNCED and leaves everything
    /// from the damaged boundary on buffered, so the next push skips (and
    /// counts) only the damaged bytes and resyncs on the packets after
    /// them. A clean flush keeps a partial tail buffered and stays SYNCED.
    ///
    /// In STRICT mode the whole packets go out even before sync is
    /// declared: `push_strict` checked every boundary, so a stream of one
    /// or two packets still reaches the wire. RECOVER mode does the same
    /// for a stream aligned since its first byte: before sync, with no
    /// byte skipped and no boundary failed so far, the whole packets held
    /// go out when every boundary present reads 0x47, the first byte of a
    /// partial packet at the end included. Once a byte was skipped or a
    /// boundary failed, flush emits nothing until three consecutive sync
    /// bytes have confirmed the alignment again.
    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        if self.state == State::Unsynced && !self.strict {
            // Fewer than 377 bytes are held here (a push verifies or
            // skips as soon as it has them), so the slice is short.
            if !self.aligned_from_start
                || aligned_packets_at_end(self.buffer.make_contiguous()) == 0
            {
                return Vec::new();
            }
            self.state = State::Synced;
        }
        let whole = self.buffer.len() / TS_PACKET_SIZE;
        let bad = (0..whole).find(|&i| self.buffer[i * TS_PACKET_SIZE] != SYNC_BYTE);
        let good = bad.unwrap_or(whole);
        let mut out = Vec::new();
        if good > 0 {
            out.push(self.buffer.drain(..good * TS_PACKET_SIZE).collect());
            self.stats.packets_sent += good as u64;
        }
        if bad.is_some() {
            // Flush may be called mid-stream (e.g. after every frame), so
            // the good packets after the damage must survive: keep the
            // bytes for try_acquire, which counts what it skips.
            self.stats.resync_events += 1;
            self.state = State::Unsynced;
        }
        out
    }

    /// End of stream: [`Self::flush`], then go on scanning what is left
    /// with the ordinary three-packet rule until nothing more verifies,
    /// because no later push will do it. Whole bundles leave as a push
    /// would have framed them, the last packets as a flush emits them.
    ///
    /// What still cannot be verified stays buffered, is never sent and is
    /// counted by [`Self::buffered_bytes`]: a partial last packet, fewer
    /// than three packets behind skipped bytes, bytes that are not TS.
    ///
    /// The scan limit of [`Self::new`] does not stop this pass, whether a
    /// push left it pending or the pass itself crosses it: everything held
    /// is scanned, so what is sent does not depend on where the limit
    /// interrupted a push. The work is linear in the bytes held (a false
    /// candidate costs one look one packet on; a run without a sync byte
    /// is skipped in one step).
    pub fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = self.flush();
        loop {
            match self.state {
                State::Unsynced => match self.try_acquire(false) {
                    Ok(true) => {}
                    Ok(false) => break,
                    // The limit: its counter starts again, the scan goes
                    // on. Every such turn has skipped at least one byte.
                    Err(_) => {}
                },
                State::Synced => {
                    if !self.try_emit(&mut out) {
                        out.extend(self.flush());
                        if self.state == State::Synced {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// Bytes held that have not been sent: a partial packet waiting for
    /// its rest, packets waiting for a full bundle or a flush, or bytes
    /// that have not confirmed their alignment.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic stream of N TS packets with sync byte at every
    /// 188-byte boundary. PID is irrelevant — just need 0x47 markers.
    fn synthetic_ts_stream(n_packets: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n_packets * 188);
        for i in 0..n_packets {
            buf.push(0x47);
            // Fill the remaining 187 bytes with a recognizable pattern.
            for j in 1..188 {
                buf.push(((i & 0xFF) as u8).wrapping_add(j as u8));
            }
        }
        buf
    }

    #[test]
    fn unsynced_acquires_after_three_sync_bytes() {
        let mut framing = TsFraming::new(18800);
        let ts = synthetic_ts_stream(3);
        let (out, stats) = framing.push(&ts).unwrap();
        // 3 packets ≠ 7, so no bundle emitted; framing acquired sync.
        assert!(out.is_empty());
        let bytes_pushed = stats.bytes_pushed;
        let bytes_skipped = stats.bytes_skipped_for_sync;
        assert!(framing.is_synced());
        assert_eq!(bytes_pushed, ts.len() as u64);
        assert_eq!(bytes_skipped, 0);
    }

    #[test]
    fn unsynced_skips_misaligned_prefix() {
        let mut framing = TsFraming::new(18800);
        // 50 bytes of garbage prefix, then a valid TS stream.
        let prefix: Vec<u8> = (0..50).map(|i| 0x80 | (i as u8)).collect();
        let ts = synthetic_ts_stream(3);
        let mut input = prefix.clone();
        input.extend_from_slice(&ts);
        let (out, stats) = framing.push(&input).unwrap();
        assert!(out.is_empty());
        // Copy needed values before stats borrow ends (stats borrows framing).
        let bytes_skipped = stats.bytes_skipped_for_sync;
        let _ = stats; // ensure stats is not used after this point
        assert!(framing.is_synced());
        // The prefix that doesn't contain 0x47 (the test prefix uses
        // 0x80..0xB1 which avoids 0x47) is fully discarded.
        assert!(bytes_skipped >= 50);
    }

    #[test]
    fn emits_one_bundle_per_seven_packets() {
        let mut framing = TsFraming::new(18800);
        let ts = synthetic_ts_stream(7);
        let (out, _stats) = framing.push(&ts).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 1316);
    }

    #[test]
    fn emits_two_bundles_per_fourteen_packets() {
        let mut framing = TsFraming::new(18800);
        let ts = synthetic_ts_stream(14);
        let (out, _stats) = framing.push(&ts).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), 1316);
        assert_eq!(out[1].len(), 1316);
    }

    #[test]
    fn one_byte_at_a_time_acquires_sync() {
        let mut framing = TsFraming::new(18800);
        let ts = synthetic_ts_stream(7);
        let mut accumulated_out = Vec::new();
        for &b in &ts {
            let (mut out, _stats) = framing.push(&[b]).unwrap();
            accumulated_out.append(&mut out);
        }
        assert_eq!(
            accumulated_out.len(),
            1,
            "should bundle exactly one 7-packet message"
        );
        assert_eq!(accumulated_out[0].len(), 1316);
    }

    /// A packet with a zero where its sync byte should be.
    fn damaged_packet(i: usize) -> Vec<u8> {
        let mut p = synthetic_ts_stream(i + 1)[i * 188..].to_vec();
        p[0] = 0x00;
        p
    }

    #[test]
    fn recover_keeps_verified_packets_and_the_suffix_after_garbage() {
        let ts1 = synthetic_ts_stream(7);
        let good3 = synthetic_ts_stream(3);
        let ts2 = synthetic_ts_stream(7);
        let mut input = ts1.clone();
        input.extend_from_slice(&good3);
        input.extend_from_slice(&[0x00; 100]);
        input.extend_from_slice(&ts2);

        let mut one_call = TsFraming::new(18_800);
        let (out, stats) = one_call.push(&input).unwrap();
        assert_eq!(out, vec![ts1.clone(), good3.clone(), ts2.clone()]);
        assert_eq!(stats.resync_events, 1);
        assert_eq!(stats.bytes_skipped_for_sync, 100);
        assert_eq!(stats.packets_sent, 17);

        let mut byte_by_byte = TsFraming::new(18_800);
        let mut out2 = Vec::new();
        for b in &input {
            out2.extend(byte_by_byte.push(&[*b]).unwrap().0);
        }
        assert_eq!(out2, out, "chunking must not change what survives");
    }

    #[test]
    fn strict_error_after_one_bundle_changes_nothing_and_a_clean_retry_sends() {
        let good7 = synthetic_ts_stream(7);
        let mut bad7 = synthetic_ts_stream(7);
        bad7[3 * 188] = 0x00;
        let mut input = good7.clone();
        input.extend_from_slice(&bad7);
        let mut f = TsFraming::new(18_800);
        assert_eq!(
            f.push_strict(&input),
            Err(TsFramingError::SyncLost {
                offset: (7 + 3) * 188
            })
        );
        assert_eq!(
            f.stats(),
            SenderStats::default(),
            "a rejected call is not counted"
        );
        assert_eq!(f.push_strict(&good7).unwrap(), vec![good7]);
    }

    #[test]
    fn strict_rejected_garbage_prefix_does_not_poison_later_input() {
        let mut f = TsFraming::new(18_800);
        assert!(f.push_strict(&[0x00; 10]).is_err());
        let good3 = synthetic_ts_stream(3);
        assert!(f.push_strict(&good3).unwrap().is_empty());
        assert_eq!(f.flush(), vec![good3]);
    }

    #[test]
    fn strict_rejects_a_damaged_fourth_packet_before_flush() {
        let mut input = synthetic_ts_stream(3);
        input.extend(damaged_packet(3));
        let mut f = TsFraming::new(18_800);
        assert_eq!(
            f.push_strict(&input),
            Err(TsFramingError::SyncLost { offset: 3 * 188 })
        );
        assert!(f.flush().is_empty());
    }

    #[test]
    fn recover_flush_emits_the_verified_prefix_and_keeps_the_rest() {
        let good3 = synthetic_ts_stream(3);
        let mut input = good3.clone();
        input.extend(damaged_packet(3));
        let mut f = TsFraming::new(18_800);
        assert!(f.push(&input).unwrap().0.is_empty());
        assert_eq!(f.flush(), vec![good3]);
        assert_eq!(f.stats().bytes_skipped_for_sync, 0);
        assert_eq!(f.stats().resync_events, 1);
        assert!(!f.is_synced());

        let good7 = synthetic_ts_stream(7);
        let (out, stats) = f.push(&good7).unwrap();
        assert_eq!(out, vec![good7]);
        assert_eq!(stats.bytes_skipped_for_sync, 188);
        assert_eq!(stats.resync_events, 1);
    }

    #[test]
    fn mid_stream_flush_after_damage_keeps_the_good_packets_that_follow() {
        let all = synthetic_ts_stream(16);
        let pkt = |i: usize| &all[i * 188..(i + 1) * 188];
        let mut f = TsFraming::new(18_800);
        assert_eq!(
            f.push(&all[..7 * 188]).unwrap().0,
            vec![all[..7 * 188].to_vec()]
        );

        let mut six = all[7 * 188..13 * 188].to_vec();
        six[188] = 0x00; // packet 8 loses its sync byte
        assert!(f.push(&six).unwrap().0.is_empty());
        assert_eq!(f.flush(), vec![pkt(7).to_vec()]);

        let (out, stats) = f.push(&all[13 * 188..]).unwrap();
        assert_eq!(out, vec![all[9 * 188..16 * 188].to_vec()]);
        assert_eq!(stats.bytes_skipped_for_sync, 188);
        assert_eq!(stats.resync_events, 1);
        assert_eq!(stats.packets_sent, 7 + 1 + 7);
    }

    #[test]
    fn clean_flush_keeps_a_partial_tail_buffered_and_synced() {
        let all = synthetic_ts_stream(10);
        let mut f = TsFraming::new(18_800);
        assert!(f.push(&all[..3 * 188 + 100]).unwrap().0.is_empty());
        assert_eq!(f.flush(), vec![all[..3 * 188].to_vec()]);
        assert!(f.is_synced());

        let (out, stats) = f.push(&all[3 * 188 + 100..]).unwrap();
        assert_eq!(out, vec![all[3 * 188..].to_vec()]);
        assert_eq!(stats.bytes_skipped_for_sync, 0);
        assert_eq!(stats.resync_events, 0);
    }

    #[test]
    fn watchdog_after_a_framed_bundle_returns_the_bundle_first() {
        let good7 = synthetic_ts_stream(7);
        let mut input = good7.clone();
        input.extend_from_slice(&[0x00; 20_000]);

        let mut one_call = TsFraming::new(18_800);
        let (out, _) = one_call.push(&input).unwrap();
        assert_eq!(out, vec![good7.clone()]);
        assert!(matches!(
            one_call.push(&[]),
            Err(TsFramingError::NoSyncAfterLimit { max: 18_800 })
        ));

        let mut byte_by_byte = TsFraming::new(18_800);
        let mut out2 = Vec::new();
        let mut watchdog_errors = 0;
        for b in &input {
            match byte_by_byte.push(&[*b]) {
                Ok((bundles, _)) => out2.extend(bundles),
                Err(TsFramingError::NoSyncAfterLimit { max: 18_800 }) => watchdog_errors += 1,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert_eq!(out2, vec![good7]);
        assert_eq!(watchdog_errors, 1);

        let (a, b) = (one_call.stats(), byte_by_byte.stats());
        assert_eq!(a.packets_sent, 7);
        assert_eq!(a.packets_sent, b.packets_sent);
        assert_eq!(a.bytes_skipped_for_sync, b.bytes_skipped_for_sync);
        assert_eq!(a.resync_events, b.resync_events);
    }

    #[test]
    fn strict_mode_rejects_misalignment() {
        let mut framing = TsFraming::new(18800);
        let prefix = vec![0x80, 0x81, 0x82];
        let ts = synthetic_ts_stream(3);
        let mut input = prefix;
        input.extend_from_slice(&ts);
        let result = framing.push_strict(&input);
        assert!(result.is_err(), "strict mode must reject prefix garbage");
    }

    #[test]
    fn strict_mode_rejects_non_sync_first_byte() {
        // STRICT mode errors immediately on the first non-0x47 byte. The
        // max_unsynced_bytes knob is RECOVER-mode-only and is not
        // exercised here — see recover_mode_errors_after_exceeding_max_unsynced_bytes
        // below for RECOVER mode's own threshold behavior.
        let mut framing = TsFraming::new(200);
        let no_sync = vec![0x00; 300]; // 300 bytes, never 0x47
        let result = framing.push_strict(&no_sync);
        assert!(
            result.is_err(),
            "STRICT mode must error on the first non-sync byte"
        );
    }

    #[test]
    fn strict_flush_emits_a_stream_shorter_than_three_packets() {
        for n in [1, 2] {
            let mut f = TsFraming::new(18800);
            let ts = synthetic_ts_stream(n);
            assert!(f.push_strict(&ts).unwrap().is_empty());
            assert_eq!(f.flush(), vec![ts], "{n} packet(s)");
            assert_eq!(f.stats().packets_sent, n as u64);
        }
    }

    /// `n` packets with no sync byte inside them, each one different.
    fn clean_packets(from: usize, n: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n * 188);
        for i in from..from + n {
            buf.push(0x47);
            buf.push(0x80 | (i & 0x7F) as u8);
            buf.push(0x80 | ((i >> 7) & 0x7F) as u8);
            buf.extend_from_slice(&[0xA5; 185]);
        }
        buf
    }

    /// `(bytes_pushed, bytes_skipped_for_sync, resync_events)`.
    fn counters(f: &TsFraming) -> (u64, u64, u64) {
        let s = f.stats();
        (s.bytes_pushed, s.bytes_skipped_for_sync, s.resync_events)
    }

    /// A RECOVER framer after `input` went in as one push that framed no
    /// bundle.
    fn pushed(input: &[u8]) -> TsFraming {
        let mut f = TsFraming::new(18_800);
        assert!(f.push(input).unwrap().0.is_empty());
        f
    }

    #[test]
    fn recover_flush_sends_one_aligned_packet() {
        let ts = clean_packets(0, 1);
        let mut f = pushed(&ts);
        assert_eq!(f.flush(), vec![ts]);
        assert_eq!(f.buffered_bytes(), 0);
        assert_eq!(counters(&f), (188, 0, 0));
        assert_eq!(f.stats().packets_sent, 1);
        assert!(f.finish().is_empty());
    }

    #[test]
    fn recover_flush_sends_two_aligned_packets() {
        let ts = clean_packets(0, 2);
        let mut f = pushed(&ts);
        assert_eq!(f.flush(), vec![ts]);
        assert_eq!(f.buffered_bytes(), 0);
        assert_eq!(counters(&f), (376, 0, 0));
        assert_eq!(f.stats().packets_sent, 2);
        assert!(f.finish().is_empty());
    }

    #[test]
    fn the_end_pass_alone_sends_a_short_aligned_stream() {
        for n in [1, 2] {
            let ts = clean_packets(0, n);
            let mut f = pushed(&ts);
            assert_eq!(f.finish(), vec![ts], "{n} packet(s)");
            assert_eq!(f.buffered_bytes(), 0);
            assert_eq!(counters(&f), (n as u64 * 188, 0, 0));
        }
    }

    #[test]
    fn three_to_five_aligned_packets_are_flushed_as_before() {
        for n in 3..=5 {
            let ts = clean_packets(0, n);
            let mut f = pushed(&ts);
            assert!(f.is_synced());
            assert_eq!(f.flush(), vec![ts], "{n} packets");
            assert_eq!(f.buffered_bytes(), 0);
            assert_eq!(counters(&f), (n as u64 * 188, 0, 0));
        }
    }

    #[test]
    fn a_flush_after_every_packet_sends_every_packet() {
        let mut f = TsFraming::new(18_800);
        for i in 0..4 {
            let pkt = clean_packets(i, 1);
            assert!(f.push(&pkt).unwrap().0.is_empty());
            assert_eq!(f.flush(), vec![pkt], "flush {i}");
        }
        assert_eq!(f.buffered_bytes(), 0);
        assert_eq!(counters(&f), (4 * 188, 0, 0));
        assert_eq!(f.stats().packets_sent, 4);
    }

    #[test]
    fn packets_behind_garbage_wait_for_three() {
        for n in [1, 2] {
            let mut input = vec![0x00; 100];
            input.extend(clean_packets(0, n));
            let mut f = pushed(&input);
            assert!(f.flush().is_empty(), "{n} packet(s)");
            assert!(f.finish().is_empty(), "{n} packet(s)");
            assert_eq!(f.buffered_bytes(), n * 188);
            assert_eq!(counters(&f), (100 + n as u64 * 188, 100, 0));
        }

        let good3 = clean_packets(0, 3);
        let mut input = vec![0x00; 100];
        input.extend_from_slice(&good3);
        let mut f = pushed(&input);
        assert_eq!(f.flush(), vec![good3]);
        assert_eq!(f.buffered_bytes(), 0);
        assert_eq!(counters(&f), (664, 100, 0));
    }

    #[test]
    fn every_kind_of_skipped_byte_ends_the_early_rule() {
        // Garbage in a push of its own, discarded whole before the packet
        // arrives.
        let mut f = pushed(&[0x00; 100]);
        assert_eq!(f.buffered_bytes(), 0);
        assert!(f.push(&clean_packets(0, 1)).unwrap().0.is_empty());
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 188);
        assert_eq!(counters(&f), (288, 100, 0));

        // One stray sync byte right before two packets: it is dropped as
        // a false candidate and the packets sit at the buffer's head.
        let mut input = vec![0x47];
        input.extend(clean_packets(0, 2));
        let mut f = pushed(&input);
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 376);
        assert_eq!(counters(&f), (377, 1, 0));
    }

    #[test]
    fn a_sync_byte_inside_the_garbage_does_not_release_the_packets_behind_it() {
        let mut input = vec![0x00; 100];
        input[50] = 0x47;
        input.extend(clean_packets(0, 2));
        let mut f = pushed(&input);
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 376);
        assert_eq!(counters(&f), (476, 100, 0));
    }

    #[test]
    fn garbage_after_a_flushed_first_packet_ends_the_early_rule_for_good() {
        let first = clean_packets(0, 1);
        let mut f = pushed(&first);
        assert_eq!(f.flush(), vec![first]);

        let mut rest = vec![0x00; 100];
        rest.extend(clean_packets(1, 1));
        assert!(f.push(&rest).unwrap().0.is_empty());
        assert!(f.flush().is_empty(), "one packet behind garbage");
        assert!(f.push(&clean_packets(2, 1)).unwrap().0.is_empty());
        assert!(f.flush().is_empty(), "two packets behind garbage");
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 376);
        assert_eq!(counters(&f), (188 + 100 + 376, 100, 1));
        assert_eq!(f.stats().packets_sent, 1);
    }

    #[test]
    fn a_partial_tail_stays_buffered() {
        // One packet and the head of the next: the packet goes out on the
        // two sync bytes present.
        let all = clean_packets(0, 4);
        let mut f = pushed(&all[..188 + 100]);
        assert_eq!(f.flush(), vec![all[..188].to_vec()]);
        assert_eq!(f.buffered_bytes(), 100);
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 100);
        assert_eq!(counters(&f), (288, 0, 0));

        for n in [2, 3] {
            let mut f = pushed(&all[..n * 188 + 100]);
            assert_eq!(f.finish(), vec![all[..n * 188].to_vec()], "{n} packets");
            assert_eq!(f.buffered_bytes(), 100);
            assert_eq!(counters(&f), (n as u64 * 188 + 100, 0, 0));
        }

        // Less than one packet: nothing to send.
        let mut f = pushed(&all[..100]);
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 100);
        assert_eq!(counters(&f), (100, 0, 0));
    }

    #[test]
    fn strict_keeps_its_results_and_counts_a_partial_tail() {
        let all = clean_packets(0, 6);
        for n in 1..=5 {
            let mut f = TsFraming::new(18_800);
            assert!(f.push_strict(&all[..n * 188 + 100]).unwrap().is_empty());
            assert_eq!(f.finish(), vec![all[..n * 188].to_vec()], "{n} packet(s)");
            assert_eq!(f.buffered_bytes(), 100);
            assert_eq!(counters(&f), (n as u64 * 188 + 100, 0, 0));
        }
    }

    #[test]
    fn one_stray_byte_after_a_short_stream_holds_all_of_it_back() {
        let mut input = clean_packets(0, 1);
        input.push(0x00);
        let mut f = pushed(&input);
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 189);
        assert_eq!(counters(&f), (189, 0, 0));
    }

    /// Seven packets (one bundle), 100 bytes of garbage, then `behind`
    /// packets, in one push.
    fn bundle_garbage_then(behind: usize) -> (TsFraming, Vec<u8>) {
        let all = clean_packets(0, 7 + behind);
        let mut input = all[..7 * 188].to_vec();
        input.extend_from_slice(&[0x00; 100]);
        input.extend_from_slice(&all[7 * 188..]);
        let mut f = TsFraming::new(18_800);
        assert_eq!(f.push(&input).unwrap().0, vec![all[..7 * 188].to_vec()]);
        (f, all[7 * 188..].to_vec())
    }

    #[test]
    fn the_end_pass_recovers_three_packets_behind_garbage() {
        for behind in 3..=6 {
            let (mut f, tail) = bundle_garbage_then(behind);
            assert_eq!(f.finish(), vec![tail], "{behind} packets behind");
            assert_eq!(f.buffered_bytes(), 0);
            let pushed = (7 + behind as u64) * 188 + 100;
            assert_eq!(counters(&f), (pushed, 100, 1));
            assert_eq!(f.stats().packets_sent, 7 + behind as u64);
        }

        // A flush alone still leaves them for the next push.
        let (mut f, tail) = bundle_garbage_then(3);
        assert!(f.flush().is_empty());
        assert_eq!(f.buffered_bytes(), 100 + 564);
        assert_eq!(f.finish(), vec![tail]);
    }

    #[test]
    fn the_end_pass_leaves_two_packets_behind_garbage() {
        let (mut f, _) = bundle_garbage_then(2);
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 376);
        assert_eq!(counters(&f), (9 * 188 + 100, 100, 1));
        assert_eq!(f.stats().packets_sent, 7);
    }

    /// A sync byte with nothing one packet on, 60 zeros, the same again
    /// and 399 zeros: 461 bytes that a scan limit of 50 stops in.
    fn garbage_with_two_false_candidates() -> Vec<u8> {
        let mut garbage = vec![0x00; 461];
        garbage[0] = 0x47;
        garbage[61] = 0x47;
        garbage
    }

    #[test]
    fn the_end_pass_frames_whole_bundles_like_a_push() {
        // Twenty packets behind garbage that a push gave up on: the end
        // pass hands them out in the messages a push would have made, not
        // as one.
        let packets = clean_packets(0, 20);
        let mut input = garbage_with_two_false_candidates();
        input.extend_from_slice(&packets);
        let mut f = TsFraming::new(50);
        assert_eq!(
            f.push(&input),
            Err(TsFramingError::NoSyncAfterLimit { max: 50 })
        );
        assert_eq!(f.buffered_bytes(), 399 + 20 * 188);
        assert_eq!(
            f.finish(),
            vec![
                packets[..7 * 188].to_vec(),
                packets[7 * 188..14 * 188].to_vec(),
                packets[14 * 188..].to_vec(),
            ]
        );
        assert_eq!(f.buffered_bytes(), 0);
        assert_eq!(counters(&f), (input.len() as u64, 461, 0));
    }

    #[test]
    fn the_end_pass_recovers_packets_behind_a_limit_the_last_push_left_pending() {
        // Garbage behind a bundle, with one false candidate past a scan
        // limit of 50: the push returns its bundle and leaves the limit's
        // error for the next call. No next call comes; the end pass scans
        // on and finds the five packets.
        let all = clean_packets(0, 12);
        let mut input = all[..7 * 188].to_vec();
        input.extend_from_slice(&[0x00; 100]);
        input.push(0x47);
        input.extend_from_slice(&[0x00; 399]);
        input.extend_from_slice(&all[7 * 188..]);
        let mut f = TsFraming::new(50);
        assert_eq!(f.push(&input).unwrap().0, vec![all[..7 * 188].to_vec()]);
        assert_eq!(f.buffered_bytes(), 399 + 5 * 188);
        assert_eq!(counters(&f), (input.len() as u64, 101, 1));

        assert_eq!(f.finish(), vec![all[7 * 188..].to_vec()]);
        assert_eq!(f.buffered_bytes(), 0);
        assert_eq!(counters(&f), (input.len() as u64, 500, 1));

        // One more push, which raises the pending error, changes nothing
        // about what the end pass sends.
        let mut g = TsFraming::new(50);
        assert_eq!(g.push(&input).unwrap().0.len(), 1);
        assert_eq!(
            g.push(&[]),
            Err(TsFramingError::NoSyncAfterLimit { max: 50 })
        );
        assert_eq!(g.finish(), vec![all[7 * 188..].to_vec()]);
        assert_eq!(counters(&g), (input.len() as u64, 500, 1));
    }

    #[test]
    fn non_ts_bytes_starting_with_sync_are_never_sent() {
        for len in [300, 376] {
            let mut input = vec![0x11; len];
            input[0] = 0x47;
            let mut f = pushed(&input);
            assert!(f.flush().is_empty(), "{len} bytes");
            assert!(f.finish().is_empty(), "{len} bytes");
            assert_eq!(f.buffered_bytes(), len);
            assert_eq!(counters(&f), (len as u64, 0, 0));
            assert_eq!(f.stats().packets_sent, 0);
        }
    }

    #[test]
    fn good_packets_before_a_damaged_third_are_not_sent() {
        let mut input = clean_packets(0, 3);
        input[376] = 0x00;
        let mut f = pushed(&input);
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        // The first packet was skipped as a false candidate; the second
        // and the damaged third are what is left.
        assert_eq!(f.buffered_bytes(), 376);
        assert_eq!(counters(&f), (564, 188, 0));
        assert_eq!(f.stats().packets_sent, 0);
    }

    #[test]
    fn a_non_ts_input_flushed_inside_its_first_packet_sends_that_packet_and_no_more() {
        let mut input = vec![0x11; 300];
        input[0] = 0x47;
        let mut f = pushed(&input[..188]);
        assert_eq!(f.flush(), vec![input[..188].to_vec()]);

        assert!(f.push(&input[188..]).unwrap().0.is_empty());
        assert!(f.flush().is_empty());
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), 112);
        assert_eq!(counters(&f), (300, 0, 0));
        assert_eq!(f.stats().packets_sent, 1);
    }

    #[test]
    fn a_flush_inside_a_start_that_only_looks_aligned_costs_the_packet_it_covers() {
        // A sync byte, 50 bytes of junk, then five real packets: the real
        // stream sits 51 bytes off the grid of the first byte.
        let real = clean_packets(0, 5);
        let mut stream = vec![0x11; 51];
        stream[0] = 0x47;
        stream.extend_from_slice(&real);

        let mut uncut = pushed(&stream);
        assert_eq!(uncut.finish(), vec![real.clone()]);
        assert_eq!(counters(&uncut), (991, 51, 0));

        // A flush after 188 bytes declares them a packet: they are sent,
        // and the real packet they cut into cannot be recovered.
        let mut flushed = pushed(&stream[..188]);
        assert_eq!(flushed.flush(), vec![stream[..188].to_vec()]);
        assert!(flushed.push(&stream[188..]).unwrap().0.is_empty());
        assert_eq!(flushed.finish(), vec![real[188..].to_vec()]);
        assert_eq!(flushed.buffered_bytes(), 0);
        assert_eq!(counters(&flushed), (991, 51, 1));
    }

    #[test]
    fn the_end_pass_scans_a_hostile_buffer_once() {
        // 188 candidates, none with a sync byte one packet on, then 376
        // zeros, over and over: nothing in it ever verifies. A push gives
        // up at the scan limit and leaves the rest buffered.
        const LEN: usize = 4 * 1024 * 1024;
        let hostile: Vec<u8> = (0..LEN)
            .map(|i| if i % 564 < 188 { 0x47 } else { 0x00 })
            .collect();
        let mut f = TsFraming::new(18_800);
        assert_eq!(
            f.push(&hostile),
            Err(TsFramingError::NoSyncAfterLimit { max: 18_800 })
        );
        assert!(f.buffered_bytes() > LEN - 20_000);

        // Nothing is sent, and every byte is skipped or left, once.
        assert!(f.finish().is_empty());
        assert_eq!(f.stats().packets_sent, 0);
        let left = f.buffered_bytes();
        assert!(left < 377, "{left} left");
        assert_eq!(f.stats().bytes_skipped_for_sync, (LEN - left) as u64);
        assert!(
            f.scan_steps <= 3 * LEN as u64,
            "{} scan steps for {LEN} bytes",
            f.scan_steps
        );

        // A second pass has nothing to do.
        let steps = f.scan_steps;
        assert!(f.finish().is_empty());
        assert_eq!(f.buffered_bytes(), left);
        assert_eq!(f.stats().bytes_skipped_for_sync, (LEN - left) as u64);
        assert!(f.scan_steps - steps <= 377);
    }

    mod chunking {
        use super::*;
        use proptest::prelude::*;

        /// What a run handed out and what it left behind.
        #[derive(Debug, PartialEq, Eq)]
        struct Run {
            sent: Vec<u8>,
            left: usize,
            counters: (u64, u64, u64),
        }

        /// Push `stream` cut at `cuts`, flushing after a chunk whose flag
        /// is set, then end it.
        fn run(stream: &[u8], cuts: &[(usize, bool)], max_unsynced: usize) -> Run {
            let mut f = TsFraming::new(max_unsynced);
            let mut sent = Vec::new();
            let mut at = 0;
            for &(cut, flush) in cuts.iter().chain(&[(stream.len(), false)]) {
                // The scan limit's error loses nothing: the bytes are in
                // the buffer and the call framed no bundle.
                if let Ok((bundles, _)) = f.push(&stream[at..cut]) {
                    sent.extend(bundles.concat());
                }
                if flush {
                    sent.extend(f.flush().concat());
                }
                at = cut;
            }
            sent.extend(f.finish().concat());
            Run {
                sent,
                left: f.buffered_bytes(),
                counters: counters(&f),
            }
        }

        #[derive(Debug, Clone)]
        enum Piece {
            Packets(usize),
            Garbage(Vec<u8>),
        }

        fn stream_of(pieces: &[Piece], tail: Option<usize>) -> Vec<u8> {
            let mut stream = Vec::new();
            let mut next = 0;
            for piece in pieces {
                match piece {
                    Piece::Packets(n) => {
                        stream.extend(clean_packets(next, *n));
                        next += n;
                    }
                    Piece::Garbage(bytes) => stream.extend_from_slice(bytes),
                }
            }
            if let Some(len) = tail {
                stream.extend_from_slice(&clean_packets(next, 1)[..len]);
            }
            stream
        }

        fn pieces(
            garbage_byte: impl Strategy<Value = u8> + Clone,
        ) -> impl Strategy<Value = Vec<Piece>> {
            proptest::collection::vec(
                prop_oneof![
                    (1usize..=9).prop_map(Piece::Packets),
                    proptest::collection::vec(garbage_byte, 1..400).prop_map(Piece::Garbage),
                ],
                0..8,
            )
        }

        fn tail() -> impl Strategy<Value = Option<usize>> {
            proptest::option::of(1usize..188)
        }

        fn cuts(len: usize, picks: &[(proptest::sample::Index, bool)]) -> Vec<(usize, bool)> {
            let mut cuts: Vec<(usize, bool)> = picks
                .iter()
                .map(|(at, flush)| (at.index(len + 1), *flush))
                .collect();
            cuts.sort_unstable();
            cuts
        }

        fn picks() -> impl Strategy<Value = Vec<(proptest::sample::Index, bool)>> {
            proptest::collection::vec((any::<proptest::sample::Index>(), any::<bool>()), 0..8)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            /// Without a flush between the chunks, how a stream is cut and
            /// where the scan limit interrupts a push change nothing.
            #[test]
            fn cutting_a_stream_without_a_flush_changes_nothing(
                pieces in pieces(any::<u8>()),
                tail in tail(),
                picks in picks(),
                max_unsynced in prop_oneof![Just(0usize), Just(64), Just(18_800)],
            ) {
                let stream = stream_of(&pieces, tail);
                let cuts: Vec<(usize, bool)> = cuts(stream.len(), &picks)
                    .into_iter()
                    .map(|(at, _)| (at, false))
                    .collect();
                prop_assert_eq!(
                    run(&stream, &cuts, max_unsynced),
                    run(&stream, &[], usize::MAX)
                );
            }

            /// A stream of packets and a partial tail comes out the same
            /// wherever it is flushed.
            #[test]
            fn flushing_a_clean_stream_between_chunks_changes_nothing(
                packets in 0usize..24,
                tail in tail(),
                picks in picks(),
            ) {
                let stream = stream_of(&[Piece::Packets(packets)], tail);
                let cuts = cuts(stream.len(), &picks);
                prop_assert_eq!(
                    run(&stream, &cuts, 18_800),
                    run(&stream, &[], 18_800)
                );
            }

            /// A flush declares the packets held so far a stream: on a
            /// damaged stream it can add the first one or two packets,
            /// which the uncut stream never verified, and nothing else.
            #[test]
            fn flushing_a_damaged_stream_can_only_add_its_first_packets(
                pieces in pieces(any::<u8>().prop_map(|b| if b == 0x47 { 0x48 } else { b })),
                tail in tail(),
                picks in picks(),
            ) {
                let stream = stream_of(&pieces, tail);
                let uncut = run(&stream, &[], 18_800).sent;
                let flushed = run(&stream, &cuts(stream.len(), &picks), 18_800).sent;
                let added = flushed.len() - uncut.len().min(flushed.len());
                prop_assert!(added == 0 || added == 188 || added == 376, "added {}", added);
                prop_assert_eq!(&flushed[..added], &stream[..added]);
                prop_assert_eq!(&flushed[added..], &uncut[..]);
            }
        }
    }

    #[test]
    fn flush_emits_pending_partial_bundle() {
        let mut framing = TsFraming::new(18800);
        let ts = synthetic_ts_stream(3);
        let (out, _stats) = framing.push(&ts).unwrap();
        assert!(out.is_empty(), "3 < 7 → no bundle yet");
        let flushed = framing.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].len(), 188 * 3);
    }

    #[test]
    fn recover_mode_errors_after_exceeding_max_unsynced_bytes() {
        let mut framing = TsFraming::new(500);
        // 501 bytes, never 0x47 — one past the limit.
        let garbage = vec![0x00u8; 501];
        let result = framing.push(&garbage);
        match result {
            Err(TsFramingError::NoSyncAfterLimit { max }) => assert_eq!(max, 500),
            other => panic!("expected NoSyncAfterLimit, got {other:?}"),
        }
    }

    #[test]
    fn recover_mode_does_not_error_when_sync_found_under_limit() {
        let mut framing = TsFraming::new(500);
        // 490 bytes of garbage (under the 500 limit), then a valid stream.
        let prefix = vec![0x00u8; 490];
        let ts = synthetic_ts_stream(3);
        let mut input = prefix;
        input.extend_from_slice(&ts);
        let (out, _stats) = framing
            .push(&input)
            .expect("must not error when sync is found under the limit");
        assert!(out.is_empty());
        assert!(framing.is_synced());
    }
}
