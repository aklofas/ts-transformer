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
//!    see rule 4) after appending this packet's payload.
//! 3. **Timestamp change without a marker**: §4.2 — a receiver that observes
//!    a new RTP timestamp knows the previous KLVunit is complete even absent
//!    a marker. Close the open unit first (emitting it, unless poisoned),
//!    then open a new one at the new timestamp.
//! 4. **Sequence gap** (`delta != 1`; duplicates, `delta == 0`, are ignored):
//!    the open unit is dropped (`units_dropped` ticks once for this event).
//!    Whatever unit starts accumulating from this point on — whether it is
//!    the gap-revealing packet itself or, if that packet's payload exceeds
//!    the oversize cap, is skipped per rule 6 — cannot be confirmed to
//!    truly be a fresh KLVunit's first fragment (we cannot tell; RFC 6597
//!    offers no way to know), so it is marked **poisoned**: it keeps
//!    accumulating state (so later timestamp/marker boundaries are still
//!    detected correctly) but its bytes are never buffered and, when it
//!    reaches its own boundary, it is silently discarded — not pushed to
//!    the ready queue, and not counted again (the one `units_dropped` tick
//!    for this whole gap event already happened above).
//! 5. **SSRC change**: a source restart. The open unit is dropped
//!    (`units_dropped` ticks) and sequence-number tracking resets; unlike
//!    rule 4, the packet that revealed the SSRC change is a genuinely fresh,
//!    trustworthy start (a new SSRC is an unambiguous RTP source boundary),
//!    so it opens a clean, unpoisoned unit.
//! 6. **Oversize unit** (open unit length would exceed [`MAX_KLV_UNIT_BYTES`]):
//!    drop the unit, tick both `units_dropped_oversize` and `units_dropped`,
//!    and ignore the remainder of that unit up to and including its marker
//!    (a KLVunit this large cannot be a conformant encoding; continuing to
//!    accumulate would just grow memory for bytes that are discarded
//!    anyway).
//! 7. **Empty payload**: ignored — ticks no counter and touches no other
//!    state, since the packet is not malformed, merely vacuous.

use std::collections::VecDeque;

use crate::packet::RtpHeader;

/// Maximum accumulated size of one open KLVunit. See rule 6 in the
/// [module docs](self).
// Not yet referenced outside tests — consumed by the elementary publish adapter.
#[allow(dead_code)]
pub(crate) const MAX_KLV_UNIT_BYTES: usize = 1024 * 1024;

/// One fully reassembled KLVunit (RFC 6597 §4.1): the raw bytes of a single
/// KLV Local Set or Universal Set encoding, plus the RTP timestamp shared by
/// every packet that contributed to it.
// Not yet referenced outside tests — consumed by the elementary publish adapter.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KlvUnit {
    pub(crate) bytes: Vec<u8>,
    pub(crate) rtp_timestamp: u32,
}

/// Counters for monitoring [`KlvDepacketizer`].
///
/// Returned by value from [`KlvDepacketizer::stats`] (the struct is `Copy`).
// Not yet referenced outside tests — consumed by the elementary publish adapter.
#[allow(dead_code)]
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
/// See the [module-level doc](self) for the full 7-rule state-machine
/// contract.
// Not yet referenced outside tests — consumed by the elementary publish adapter.
#[allow(dead_code)]
pub(crate) struct KlvDepacketizer {
    /// RTP timestamp of the open unit, if one is open.
    unit_ts: Option<u32>,
    /// Accumulated bytes for the open unit. Left empty for a unit poisoned
    /// by rule 4 — it will be silently discarded at its next boundary
    /// regardless of content, so there is no point copying bytes into it.
    unit_buf: Vec<u8>,
    /// True if the open unit must be silently discarded (not emitted, not
    /// separately counted) at its next boundary — set when a sequence gap
    /// starts a new unit whose first-fragment status cannot be confirmed
    /// (rule 4).
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
    /// Set after an oversize drop (rule 6): ignore all further payload for
    /// the doomed unit until (and including) the packet carrying the
    /// marker.
    skip_until_marker: bool,
    /// Fully reassembled units waiting to be consumed.
    ready: VecDeque<KlvUnit>,
    stats: KlvDepayStats,
}

impl KlvDepacketizer {
    /// Construct a new, empty depacketizer.
    // Not yet referenced outside tests — consumed by the elementary publish adapter.
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self {
            unit_ts: None,
            unit_buf: Vec::new(),
            unit_poisoned: false,
            last_seq: None,
            ssrc: None,
            gap_pending: false,
            skip_until_marker: false,
            ready: VecDeque::new(),
            stats: KlvDepayStats::default(),
        }
    }

    /// Feed one RTP packet into the depacketizer.
    // Not yet referenced outside tests — consumed by the elementary publish adapter.
    #[allow(dead_code)]
    pub(crate) fn feed(&mut self, header: &RtpHeader, payload: &[u8]) {
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
                self.skip_until_marker = false;
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
                self.skip_until_marker = false;
            }
        }
        self.last_seq = Some(header.seq);

        // ── Rule 6 continuation: an already-doomed unit, just waiting for
        // its marker to resync. ────────────────────────────────────────────
        if self.skip_until_marker {
            if header.marker {
                self.skip_until_marker = false;
            }
            return;
        }

        // ── Rule 3: a timestamp change closes whatever is open first. ─────
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
            // Bytes are discarded unconditionally — see rule 4.
        } else {
            // ── Rule 6: size cap, checked before the append actually grows
            // the buffer past the limit. ─────────────────────────────────
            if self.unit_buf.len().saturating_add(payload.len()) > MAX_KLV_UNIT_BYTES {
                self.unit_ts = None;
                self.unit_buf = Vec::new();
                self.stats.units_dropped_oversize += 1;
                self.stats.units_dropped += 1;
                self.skip_until_marker = !header.marker;
                return;
            }
            self.unit_buf.extend_from_slice(payload);
        }

        // ── Rule 2: marker closes the unit. ────────────────────────────────
        if header.marker {
            self.close_unit();
        }
    }

    /// Pull the next completed KLVunit, if one is available.
    // Not yet referenced outside tests — consumed by the elementary publish adapter.
    #[allow(dead_code)]
    pub(crate) fn next_unit(&mut self) -> Option<KlvUnit> {
        self.ready.pop_front()
    }

    /// Force completion of any open unit and return it.
    ///
    /// The caller should drain [`Self::next_unit`] before calling this.
    // Not yet referenced outside tests — consumed by the elementary publish adapter.
    #[allow(dead_code)]
    pub(crate) fn flush(&mut self) -> Option<KlvUnit> {
        self.close_unit();
        self.ready.pop_front()
    }

    /// Return a snapshot of the current statistics.
    // Not yet referenced outside tests — consumed by the elementary publish adapter.
    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> KlvDepayStats {
        self.stats
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Discard the open unit, if any, resetting all per-unit state. Returns
    /// `true` if a unit was actually open (so the caller can decide whether
    /// to tick a counter — rules 4 and 5 both drop-and-count, but via
    /// different call sites with different follow-up state changes).
    fn take_open(&mut self) -> bool {
        if self.unit_ts.take().is_some() {
            self.unit_buf = Vec::new();
            self.unit_poisoned = false;
            true
        } else {
            false
        }
    }

    /// Complete the open unit, if any (rules 2 and 3). A poisoned unit
    /// (rule 4) is discarded silently — no ready push, no counter tick,
    /// since its one `units_dropped` tick already happened when the gap
    /// that poisoned it was detected.
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
        } // 17 × 65 000 > 1 MiB
        d.feed(&h(17, 1000, true), b"end");
        assert!(d.next_unit().is_none());
        assert_eq!(d.stats().units_dropped_oversize, 1);
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
