//! RFC 6597 ("RTP Payload Format for SMPTE 336M Encoded Data") KLV
//! depacketizer — KLVunit reassembly state machine.
//!
//! Follows the `feed()` / `next_unit()` idiom used by
//! [`H264Depacketizer`](crate::h264::H264Depacketizer) and
//! [`tst_core::mpegts::demux::Demuxer`]: call [`feed`](KlvDepacketizer::feed)
//! for each received RTP packet, then drain
//! [`next_unit`](KlvDepacketizer::next_unit) until it returns `None`.
//!
//! # KLVunit boundary rules (state-machine contract)
//!
//! RFC 6597 §4.1 defines a KLVunit as one or more RTP packets carrying a
//! single KLV Local Set (or Universal Set) encoding. All packets of a
//! KLVunit share one RTP timestamp; the marker bit is set on the packet
//! carrying the last fragment.
//!
//! 1. **Same timestamp, no sequence gap**: append the payload to the open
//!    unit.
//! 2. **Marker bit set**: close the open unit (emit it, unless poisoned —
//!    see rules 4 and 6) after appending this packet's payload.
//! 3. **Timestamp change without a marker**: §4.2 — a receiver that observes
//!    a new RTP timestamp knows the previous KLVunit is complete even absent
//!    a marker. Close the open unit first (emitting it, unless poisoned),
//!    then open a new one at the new timestamp. This check runs
//!    unconditionally, even while the open unit is poisoned (rules 4, 6) —
//!    a poisoned unit must still respond to a timestamp change, or a sender
//!    that advances the timestamp without ever setting a marker (or whose
//!    marker packet is lost) would wedge the depacketizer permanently.
//! 4. **Sequence gap** (`delta != 1`; duplicates, `delta == 0`, are ignored):
//!    the open unit is dropped. Whatever unit starts accumulating from this
//!    point on — the gap-revealing packet itself — cannot be confirmed to
//!    truly be a fresh KLVunit's first fragment (we cannot tell; RFC 6597
//!    offers no way to know), so it is marked **poisoned**: it keeps
//!    accumulating state (so later timestamp/marker boundaries are still
//!    detected correctly, per rule 3) but its bytes are never buffered and,
//!    when it reaches its own boundary, it is silently discarded — not
//!    pushed to the ready queue, and not counted again. `units_dropped`
//!    ticks exactly once per loss episode: dropping an open unit that is
//!    *already* an empty poisoned placeholder (e.g. a second gap, or an
//!    SSRC change, before the first poisoned placeholder ever reached a
//!    boundary) does not tick again — there is nothing new being lost,
//!    just the same ongoing episode continuing.
//! 5. **SSRC change**: a source restart. The open unit is dropped (ticking
//!    `units_dropped` under the same single-tick-per-episode discipline as
//!    rule 4) and sequence-number tracking resets; unlike rule 4, the
//!    packet that revealed the SSRC change is a genuinely fresh,
//!    trustworthy start (a new SSRC is an unambiguous RTP source boundary),
//!    so it opens a clean, unpoisoned unit.
//! 6. **Oversize unit** (open unit length would exceed [`MAX_KLV_UNIT_BYTES`]):
//!    release the accumulated bytes immediately and mark the unit
//!    **poisoned in place** (ticking `units_dropped_oversize` and
//!    `units_dropped` once) — exactly the same "poisoned but still open"
//!    handling as rule 4, so rule 3's timestamp-boundary detection and rule
//!    2's marker detection keep resolving it normally. A KLVunit this large
//!    cannot be a conformant encoding, so there is no point accumulating
//!    further bytes for it, but boundary detection must never stop.
//! 7. **Empty payload**: ignored — ticks no counter and touches no other
//!    state, since the packet is not malformed, merely vacuous.

use std::collections::VecDeque;

use crate::packet::RtpHeader;

/// Maximum accumulated size of one open KLVunit. See rule 6 in the
/// [module docs](self).
///
/// This is the publish muxer's own KLV ceiling: `Muxer::push_klv` refuses
/// a `PrivateData` unit that carries a PTS once it overflows the 16-bit
/// `PES_packet_length` (65 535 minus 3 bytes of PES header flags and 5 of
/// PTS) with `MuxError::KlvTooLarge`. A larger unit could never be muxed,
/// so it is dropped here instead of being reassembled and held first.
pub(crate) const MAX_KLV_UNIT_BYTES: usize = u16::MAX as usize - 3 - 5;

/// One fully reassembled KLVunit (RFC 6597 §4.1): the raw bytes of a single
/// KLV Local Set or Universal Set encoding, plus the RTP timestamp shared by
/// every packet that contributed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KlvUnit {
    pub(crate) bytes: Vec<u8>,
    pub(crate) rtp_timestamp: u32,
}

/// Counters for monitoring [`KlvDepacketizer`].
///
/// Returned by value from [`KlvDepacketizer::stats`] (the struct is `Copy`).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct KlvDepayStats {
    /// Number of complete, unpoisoned KLVunits emitted.
    pub(crate) units_emitted: u64,
    /// Number of KLVunits discarded (sequence gap, SSRC change, or
    /// oversize). Oversize drops are also counted in
    /// `units_dropped_oversize`.
    pub(crate) units_dropped: u64,
    /// Number of KLVunits dropped specifically for exceeding
    /// [`MAX_KLV_UNIT_BYTES`]. Every oversize drop also increments
    /// `units_dropped` at the normal boundary-tick site.
    pub(crate) units_dropped_oversize: u64,
}

/// RFC 6597 KLV RTP depacketizer.
///
/// Call [`feed`](Self::feed) for each received RTP packet (in order of
/// arrival), then drain [`next_unit`](Self::next_unit) until it returns
/// `None`.
///
/// `feed` never panics on any input byte pattern — adversarial payloads are
/// handled by silently discarding and ticking the appropriate stat counter.
///
/// See the `klv_depacketizer` module doc for the full 7-rule state-machine
/// contract.
pub struct KlvDepacketizer {
    /// RTP timestamp of the open unit, if one is open.
    unit_ts: Option<u32>,
    /// Accumulated bytes for the open unit. Left empty for a unit poisoned
    /// by rule 4 — it will be silently discarded at its next boundary
    /// regardless of content, so there is no point copying bytes into it.
    unit_buf: Vec<u8>,
    /// True if the open unit must be silently discarded (not emitted, not
    /// separately counted) at its next boundary — set when a sequence gap
    /// starts a new unit whose first-fragment status cannot be confirmed
    /// (rule 4), or when the open unit has already exceeded
    /// [`MAX_KLV_UNIT_BYTES`] (rule 6). In both cases `unit_ts` is left
    /// `Some` (the unit stays open) so timestamp/marker boundary detection
    /// keeps working normally — only the bytes are abandoned.
    unit_poisoned: bool,
    /// Last RTP sequence number seen (rule 4 gap/duplicate detection).
    last_seq: Option<u16>,
    /// Latched SSRC (rule 5 source-restart detection).
    ssrc: Option<u32>,
    /// Sticky poison to apply to the next unit opened. Set when a gap is
    /// detected (rule 4) and consumed the moment a unit is next opened —
    /// always within the same `feed` call, since a non-empty payload always
    /// ends up opening or continuing a unit by the end of `feed`.
    gap_pending: bool,
    /// Fully reassembled units waiting to be consumed.
    ready: VecDeque<KlvUnit>,
    stats: KlvDepayStats,
}

impl Default for KlvDepacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl KlvDepacketizer {
    /// Construct a new, empty depacketizer.
    pub fn new() -> Self {
        Self {
            unit_ts: None,
            unit_buf: Vec::new(),
            unit_poisoned: false,
            last_seq: None,
            ssrc: None,
            gap_pending: false,
            ready: VecDeque::new(),
            stats: KlvDepayStats::default(),
        }
    }

    /// Feed one RTP packet into the depacketizer.
    pub fn feed(&mut self, header: &RtpHeader, payload: &[u8]) {
        // ── Rule 7: empty payload is ignored outright ─────────────────────
        if payload.is_empty() {
            return;
        }

        // ── Rule 5: SSRC change ────────────────────────────────────────────
        match self.ssrc {
            Some(known) if known != header.ssrc => {
                if self.take_open() {
                    self.stats.units_dropped += 1;
                }
                self.last_seq = None;
                self.gap_pending = false;
                self.ssrc = Some(header.ssrc);
            }
            Some(_) => {}
            None => self.ssrc = Some(header.ssrc),
        }

        // ── Rule 4: sequence gap / duplicate, checked before boundary
        // handling below. ──────────────────────────────────────────────────
        if let Some(last) = self.last_seq {
            let delta = header.seq.wrapping_sub(last);
            if delta == 0 {
                // Duplicate packet — ignored entirely, including not
                // updating `last_seq` (it already holds this value).
                return;
            }
            if delta != 1 {
                if self.take_open() {
                    self.stats.units_dropped += 1;
                }
                self.gap_pending = true;
            }
        }
        self.last_seq = Some(header.seq);

        // ── Rule 3: a timestamp change closes whatever is open first. This
        // runs unconditionally — even a poisoned unit (rules 4, 6) must
        // respond to a timestamp change, or a sender that never sets a
        // marker again (or whose marker packet is lost) would wedge this
        // depacketizer permanently. ─────────────────────────────────────────
        if self.unit_ts.is_some_and(|ts| ts != header.timestamp) {
            self.close_unit();
        }

        // Open a fresh unit if none is open, inheriting any pending gap
        // poison (rule 4) and then consuming it.
        if self.unit_ts.is_none() {
            self.unit_ts = Some(header.timestamp);
            self.unit_poisoned = self.gap_pending;
            self.gap_pending = false;
        }

        if self.unit_poisoned {
            // Bytes are discarded unconditionally — see rules 4 and 6.
        } else if self.unit_buf.len().saturating_add(payload.len()) > MAX_KLV_UNIT_BYTES {
            // ── Rule 6: oversize, checked before the append actually grows
            // the buffer past the limit. Poison in place — `unit_ts` stays
            // `Some` so rule 3's timestamp-boundary detection and rule 2's
            // marker detection keep resolving this unit normally; this is
            // exactly rule 4's "poisoned but still open" handling. ────────
            self.unit_buf = Vec::new();
            self.unit_poisoned = true;
            self.stats.units_dropped_oversize += 1;
            self.stats.units_dropped += 1;
        } else {
            self.unit_buf.extend_from_slice(payload);
        }

        // ── Rule 2: marker closes the unit. ────────────────────────────────
        if header.marker {
            self.close_unit();
        }
    }

    /// Pull the next completed KLVunit, if one is available.
    pub fn next_unit(&mut self) -> Option<KlvUnit> {
        self.ready.pop_front()
    }

    /// Force completion of any open unit and return it.
    ///
    /// The caller should drain [`Self::next_unit`] before calling this.
    pub fn flush(&mut self) -> Option<KlvUnit> {
        self.close_unit();
        self.ready.pop_front()
    }

    /// Return a snapshot of the current statistics.
    pub(crate) fn stats(&self) -> KlvDepayStats {
        self.stats
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Discard the open unit, if any, resetting all per-unit state. Returns
    /// `true` if a *real* unit was dropped — one that was not already a
    /// poisoned, empty placeholder left open by an earlier loss in the same
    /// episode (rules 4 and 6 poison a unit in place rather than closing it
    /// immediately; dropping that placeholder a second time — e.g. a
    /// second gap, or an SSRC change, before it ever reaches a boundary —
    /// must not tick `units_dropped` again for what is really one ongoing
    /// loss episode). Called by rule 4 (sequence gap) and rule 5 (SSRC
    /// change), both of which tick the counter only when this returns
    /// `true`.
    fn take_open(&mut self) -> bool {
        if self.unit_ts.take().is_none() {
            return false;
        }
        let was_poisoned = self.unit_poisoned;
        self.unit_buf = Vec::new();
        self.unit_poisoned = false;
        !was_poisoned
    }

    /// Complete the open unit, if any (rules 2 and 3). A poisoned unit
    /// (rule 4 or rule 6) is discarded silently — no ready push, no counter
    /// tick, since its one `units_dropped` tick already happened when it
    /// was poisoned.
    fn close_unit(&mut self) {
        let Some(rtp_timestamp) = self.unit_ts.take() else {
            return;
        };
        let poisoned = self.unit_poisoned;
        let bytes = std::mem::take(&mut self.unit_buf);
        self.unit_poisoned = false;
        if poisoned {
            return;
        }
        self.ready.push_back(KlvUnit {
            bytes,
            rtp_timestamp,
        });
        self.stats.units_emitted += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::RtpHeader;
    fn h(seq: u16, ts: u32, m: bool) -> RtpHeader {
        let mut h = RtpHeader::new(seq, ts, 0x1234);
        h.marker = m;
        h.payload_type = 97;
        h
    }

    #[test]
    fn single_packet_unit() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, true), &[0x06, 0x0E, 0x2B, 0x34, 0x02]);
        assert_eq!(
            d.next_unit().unwrap(),
            KlvUnit {
                bytes: vec![0x06, 0x0E, 0x2B, 0x34, 0x02],
                rtp_timestamp: 1000
            }
        );
        assert!(d.next_unit().is_none());
    }

    #[test]
    fn three_fragments_closed_by_marker() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        d.feed(&h(2, 1000, false), b"cd");
        assert!(d.next_unit().is_none(), "not closed yet");
        d.feed(&h(3, 1000, true), b"ef");
        assert_eq!(d.next_unit().unwrap().bytes, b"abcdef");
        assert_eq!(d.stats().units_emitted, 1);
    }

    #[test]
    fn timestamp_change_without_marker_closes_the_unit() {
        // Review Focus #5
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        d.feed(&h(2, 1000, false), b"cd");
        d.feed(&h(3, 2000, true), b"xy"); // new timestamp: previous closes, this is a clean unit
        assert_eq!(
            d.next_unit().unwrap(),
            KlvUnit {
                bytes: b"abcd".to_vec(),
                rtp_timestamp: 1000
            }
        );
        assert_eq!(
            d.next_unit().unwrap(),
            KlvUnit {
                bytes: b"xy".to_vec(),
                rtp_timestamp: 2000
            }
        );
    }

    #[test]
    fn gap_drops_the_open_unit_and_the_next_unit_is_clean() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        d.feed(&h(3, 1000, true), b"cd"); // seq 2 lost
        assert!(d.next_unit().is_none());
        assert_eq!(d.stats().units_dropped, 1);
        d.feed(&h(4, 2000, true), b"ok");
        assert_eq!(d.next_unit().unwrap().bytes, b"ok");
    }

    #[test]
    fn second_gap_on_a_poisoned_unit_does_not_double_count() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        d.feed(&h(3, 1000, false), b"cd"); // first gap (seq 2 lost): drops "ab", opens a poisoned placeholder
        assert_eq!(d.stats().units_dropped, 1);
        d.feed(&h(6, 1000, false), b"ef"); // second gap (seq 4,5 lost) while that placeholder is still open
        assert_eq!(
            d.stats().units_dropped,
            1,
            "one ongoing loss episode must tick once, not twice"
        );
        assert!(d.next_unit().is_none());
    }

    #[test]
    fn duplicate_packet_is_ignored() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        d.feed(&h(1, 1000, false), b"ab");
        d.feed(&h(2, 1000, true), b"cd");
        assert_eq!(d.next_unit().unwrap().bytes, b"abcd");
    }

    #[test]
    fn ssrc_change_resets() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        let mut other = h(900, 5, true);
        other.ssrc = 0x9999;
        d.feed(&other, b"zz");
        assert_eq!(d.next_unit().unwrap().bytes, b"zz");
        assert_eq!(d.stats().units_dropped, 1);
    }

    #[test]
    fn oversize_unit_is_dropped_and_counted() {
        let mut d = KlvDepacketizer::new();
        let chunk = vec![0u8; 65_000];
        for i in 0..17u16 {
            d.feed(&h(i, 1000, false), &chunk);
        } // 17 × 65 000 > MAX_KLV_UNIT_BYTES (two chunks already are)
        d.feed(&h(17, 1000, true), b"end");
        assert!(d.next_unit().is_none());
        assert_eq!(d.stats().units_dropped_oversize, 1);
    }

    #[test]
    fn oversize_skip_ends_on_timestamp_change_without_marker() {
        let mut d = KlvDepacketizer::new();
        let chunk = vec![0u8; 65_000];
        for i in 0..17u16 {
            d.feed(&h(i, 1000, false), &chunk);
        } // oversize triggers inside this loop, same as oversize_unit_is_dropped_and_counted
        // The doomed unit's own marker never arrives; the sender instead
        // moves on to a new timestamp, and the new unit's first packet
        // doesn't carry the marker either.
        d.feed(&h(17, 2000, false), b"xy");
        assert!(
            d.next_unit().is_none(),
            "new unit not closed yet — no marker seen"
        );
        d.feed(&h(18, 2000, true), b"zw");
        let unit = d.next_unit().expect(
            "the timestamp change must have ended the oversize-skipped unit, \
             letting this new one open cleanly",
        );
        assert_eq!(unit.bytes, b"xyzw");
        assert_eq!(unit.rtp_timestamp, 2000);
        assert_eq!(d.stats().units_dropped_oversize, 1);
        assert_eq!(d.stats().units_dropped, 1);
    }

    #[test]
    fn flush_emits_the_open_unit() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, false), b"ab");
        assert_eq!(d.flush().unwrap().bytes, b"ab");
        assert!(d.flush().is_none());
    }

    #[test]
    fn empty_payload_is_ignored() {
        let mut d = KlvDepacketizer::new();
        d.feed(&h(1, 1000, true), b"");
        assert!(d.next_unit().is_none());
        assert_eq!(d.stats().units_emitted, 0);
    }
}
