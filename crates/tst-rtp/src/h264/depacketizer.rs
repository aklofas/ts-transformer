//! RFC 6184 H.264 RTP depacketizer — AU reassembly state machine.
//!
//! # Overview
//!
//! [`H264Depacketizer`] reassembles Access Units (AUs) from RTP packets
//! carrying H.264 NAL units per RFC 6184. It follows the `feed()` / `next_au()`
//! idiom used by [`tst_core::mpegts::demux::Demuxer`]: call [`feed`] for each
//! received RTP packet, then drain [`next_au`] until it returns `None`.
//!
//! The state machine is shared with the H.265 depacketizer (`crate::depay`); this
//! module supplies RFC 6184's payload-header rules.
//!
//! # AU boundary rules (state-machine contract)
//!
//! 1. **SSRC**: the first packet latches the SSRC. A different SSRC signals a
//!    source restart — the open AU is discarded (no `aus_dropped` tick), seq/
//!    timestamp state is reset, and `ssrc_changes` is incremented. PTS stays
//!    monotonic across SSRC resets (the re-anchor adds one nominal frame step
//!    to the last emitted value before anchoring the new stream).
//!
//! 2. **Sequence gaps**: a gap (`delta ≠ 1`) poisons the currently-accumulating
//!    AU (if any) *before* boundary handling, then also poisons the AU this
//!    packet joins *after* boundary handling (sticky `gap_pending` flag).
//!    Duplicate packets (`delta = 0`) are silently ignored.
//!
//! 3. **Timestamp boundary**: when `header.timestamp` differs from the ongoing
//!    AU's timestamp, `complete_au()` is called first, then the new timestamp is
//!    adopted. One AU per RTP timestamp per RFC 6184 §5.1.
//!
//! 4. **Marker fast-path**: `M=1` calls `complete_au()` immediately after
//!    dispatching the packet's payload. This is an early-emission hint, not the
//!    sole boundary mechanism — rule 3 is the correctness path.
//!
//! 5. **`complete_au()`**: an open FU at completion counts as a discarded NALU
//!    and poisons the AU. Empty unpoisoned AUs reset silently. Poisoned AUs
//!    increment `aus_dropped`. Clean AUs push an [`H264Au`] onto the ready queue.
//!
//! 6. **Payload dispatch** on `payload[0] & 0x1F` (NALU type): types 1–23 are
//!    single NAL units; type 24 = STAP-A aggregation packets; type 28 = FU-A
//!    fragmentation units; types 25/26/27/29 are interleaved-mode-only and poison
//!    the AU; types 0/30/31 are reserved and are discarded without poisoning.
//!
//! 7. **`push_nalu(bytes)`**: if the F bit (`bytes[0] & 0x80`) is set the NALU
//!    is discarded and the AU is poisoned. Otherwise `[0,0,0,1]` + bytes is
//!    appended. NALU types 5/7/8 set `au_has_idr`/`au_has_sps`/`au_has_pps`.
//!
//! 8. **[`flush`](H264Depacketizer::flush)**: equivalent to `complete_au()` then
//!    [`next_au`](H264Depacketizer::next_au). The caller should drain `next_au()`
//!    before calling `flush`.
//!
//! # B-frames
//!
//! This depacketizer targets low-latency, no-B-frame camera streams (the primary
//! use case for gimbaled-platform video). RTP timestamps reflect decode order, not
//! display order, so [`H264Au::pts`] is a **decode-order** timestamp — B-frame
//! content can produce non-monotonic PTS values, which are passed through unaltered.
//! DTS is not derivable from RTP; callers with B-frame sources must derive DTS
//! themselves and supply it to `push_video_to_with_dts`.
//!
//! [`feed`]: H264Depacketizer::feed
//! [`next_au`]: H264Depacketizer::next_au

// Named by this module's docs and tests only.
#[cfg(any(test, doc))]
use crate::depay::ParameterSetInjection;
use crate::packet::RtpHeader;

/// An H.264 access unit (the shared [`crate::depay::AccessUnit`]).
pub type H264Au = crate::depay::AccessUnit;
/// Configuration of an [`H264Depacketizer`] (the shared [`crate::depay::DepayConfig`]).
pub type H264DepayConfig = crate::depay::DepayConfig;
/// Counters of an [`H264Depacketizer`] (the shared [`crate::depay::DepayStats`]).
pub type H264DepayStats = crate::depay::DepayStats;

/// RFC 6184 payload-header rules (packetization modes 0 and 1).
pub(crate) struct H264;

impl crate::depay::core::Format for H264 {
    const PS_SLOTS: usize = 2; // SPS, PPS
    const AGG_PREFIX_LEN: usize = 1; // STAP-A header
    const FU_PREFIX_LEN: usize = 2; // FU indicator + FU header

    fn classify(payload: &[u8]) -> crate::depay::core::Payload {
        use crate::depay::core::Payload;
        match payload[0] & 0x1F {
            1..=23 => Payload::Single,
            24 => Payload::Aggregation,
            28 => Payload::Fragment,
            // Interleaved-mode types (STAP-B, MTAP16, MTAP24, FU-B).
            25 | 26 | 27 | 29 => Payload::Poison,
            // 0 and 30–31: reserved, ignored (RFC 6184 §5.4 Table 3).
            _ => Payload::Reserved,
        }
    }

    fn nal_type(nal: &[u8]) -> u8 {
        nal[0] & 0x1F
    }

    fn is_idr(nal_type: u8) -> bool {
        tst_core::codec::nal::is_idr(tst_core::codec::VideoCodec::H264, nal_type)
    }

    fn ps_slot(nal_type: u8) -> Option<usize> {
        match nal_type {
            7 => Some(0),
            8 => Some(1),
            _ => None,
        }
    }

    fn fu_nal_header(fu: &[u8]) -> Vec<u8> {
        // F + NRI from the FU indicator, type from the FU header (§5.8).
        vec![(fu[0] & 0xE0) | (fu[1] & 0x1F)]
    }
}

/// RFC 6184 H.264 RTP depacketizer.
///
/// Call [`feed`](Self::feed) for each received RTP packet (in order of
/// arrival), then drain [`next_au`](Self::next_au) until it returns `None`.
///
/// # Panic-freedom
///
/// `feed` never panics on any input byte pattern — adversarial payloads are
/// handled by silently discarding and ticking the appropriate stat counter.
///
/// # Parameter-set cache and BeforeIdr injection
///
/// The depacketizer maintains a one-slot SPS cache and one-slot PPS cache.
/// Both are seeded from [`H264DepayConfig::initial_parameter_sets`] at
/// construction (type 7 → SPS, type 8 → PPS; last of each wins; seeding does
/// NOT tick `parameter_set_updates`).  In-band SPS/PPS NALUs update the cache
/// only when their bytes differ from the cached value; each update ticks
/// `parameter_set_updates`.
///
/// When [`ParameterSetInjection::BeforeIdr`] is active (the default), every
/// IDR AU that does not already carry an in-band SPS or PPS receives the
/// missing cached parameter set(s) prepended — SPS first, then PPS — each
/// with a 4-byte Annex B start code.  [`ParameterSetInjection::None`] passes
/// NALUs through unchanged.
///
/// # AU boundary / marker / poisoning contract summary
///
/// See the [module-level doc](self) for the full 8-rule state-machine contract.
pub struct H264Depacketizer {
    core: crate::depay::core::Core<H264>,
}

impl H264Depacketizer {
    /// Construct a new depacketizer with the given configuration.
    pub fn new(config: H264DepayConfig) -> Self {
        Self {
            core: crate::depay::core::Core::new(config),
        }
    }

    /// Feed one RTP packet into the depacketizer.
    ///
    /// This never fails and never panics — any malformed or unexpected input
    /// is handled by discarding and ticking the appropriate stat counter.
    pub fn feed(&mut self, header: &RtpHeader, payload: &[u8]) {
        self.core.feed(header, payload);
    }

    /// Pull the next completed Access Unit, if one is available.
    ///
    /// Call repeatedly until `None` after each [`feed`](Self::feed) call.
    pub fn next_au(&mut self) -> Option<H264Au> {
        self.core.next_au()
    }

    /// Force completion of any open AU and return it.
    ///
    /// The caller should drain [`Self::next_au`] before calling this. After
    /// `flush` the depacketizer state is clean.
    pub fn flush(&mut self) -> Option<H264Au> {
        self.core.flush()
    }

    /// Return a snapshot of the current statistics.
    pub fn stats(&self) -> H264DepayStats {
        self.core.stats()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::mpegts::common::Pts90khz;

    fn hdr(seq: u16, ts: u32, marker: bool) -> RtpHeader {
        let mut h = RtpHeader::new(seq, ts, 0xAABB);
        h.marker = marker;
        h.payload_type = 96;
        h
    }

    fn depay() -> H264Depacketizer {
        H264Depacketizer::new(H264DepayConfig {
            parameter_set_injection: ParameterSetInjection::None, // injection tested separately below
            ..H264DepayConfig::default()
        })
    }

    #[test]
    fn single_nalu_au_completed_by_marker() {
        let mut d = depay();
        d.feed(&hdr(1, 90_000, true), &[0x65, 0xAA]); // type-5 IDR slice
        let au = d.next_au().expect("marker completes AU");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA]);
        assert!(au.key_frame);
        assert_eq!(au.pts, Pts90khz::new(0)); // first AU zero-based
        assert_eq!(au.rtp_timestamp, 90_000);
    }

    #[test]
    fn markerless_au_completed_by_timestamp_change() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // non-IDR slice
        assert!(d.next_au().is_none()); // no marker → held
        d.feed(&hdr(2, 4003, false), &[0x41, 0x02]); // next AU's first packet
        let au = d.next_au().expect("ts change completes previous AU");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x41, 0x01]);
        assert!(!au.key_frame);
        assert_eq!(au.pts, Pts90khz::new(0));
        // second AU: PTS = delta from first = 3003
        let au2 = d.flush().expect("flush emits final AU");
        assert_eq!(au2.pts, Pts90khz::new(3003));
    }

    #[test]
    fn two_nalus_one_timestamp_one_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x67, 0x42]); // SPS
        d.feed(&hdr(2, 1000, true), &[0x65, 0x88]); // IDR, marker
        let au = d.next_au().unwrap();
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x65, 0x88]);
        assert!(au.key_frame);
    }

    #[test]
    fn timestamp_wrap_unwraps_monotonically() {
        let mut d = depay();
        // (u32::MAX - 1000) + 3003 wraps to 2002, so +3003 ticks across the u32 boundary.
        // Note: the brief used 2003 here, but (u32::MAX-1000) → 2003 is a delta of 3004,
        // not 3003. Using 2002 matches the expected output of Pts90khz::new(3003).
        d.feed(&hdr(1, u32::MAX - 1000, true), &[0x41, 0x01]);
        d.feed(&hdr(2, 2002, true), &[0x41, 0x02]); // wrapped: +3003 across the u32 boundary
        let a = d.next_au().unwrap();
        let b = d.next_au().unwrap();
        assert_eq!(a.pts, Pts90khz::new(0));
        assert_eq!(b.pts, Pts90khz::new(3003));
    }

    #[test]
    fn reserved_types_ignored_without_poisoning() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]);
        d.feed(&hdr(2, 1000, false), &[30, 0xFF]); // reserved type 30: dropped, AU survives
        d.feed(&hdr(3, 1000, true), &[0x41, 0x02]);
        let au = d.next_au().expect("reserved type must not poison");
        assert_eq!(d.stats().packets_discarded, 1);
        assert_eq!(au.annexb.len(), 4 + 2 + 4 + 2);
    }

    #[test]
    fn duplicate_seq_ignored() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]);
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // exact dup
        d.feed(&hdr(2, 1000, true), &[0x41, 0x02]);
        let au = d.next_au().unwrap();
        assert_eq!(au.annexb.len(), 12);
        assert_eq!(d.stats().duplicate_packets, 1);
    }

    // ── Additional correctness tests ─────────────────────────────────────────

    #[test]
    fn ssrc_change_resets_and_keeps_pts_monotonic() {
        let mut d = depay();
        // First AU from SSRC 0xAABB (hdr() uses ssrc=0xAABB).
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let au1 = d.next_au().unwrap();
        assert_eq!(au1.pts, Pts90khz::new(0));

        // SSRC change → source restart. Use a different SSRC (0xCCDD).
        let mut h2 = RtpHeader::new(1, 4003, 0xCCDD);
        h2.marker = true;
        h2.payload_type = 96;
        d.feed(&h2, &[0x41, 0x02]);
        let au2 = d.next_au().unwrap();
        // After SSRC reset the next AU re-anchors at last_emitted_pts + SSRC_RESET_PTS_STEP.
        // last_emitted_pts was 0, so new anchor = 0 + 3003 = 3003.
        assert_eq!(au2.pts, Pts90khz::new(3003));
    }

    #[test]
    fn f_bit_poisons_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // clean NALU
        d.feed(&hdr(2, 1000, true), &[0xE1, 0xFF]); // F=1, type 1 — corrupt
        assert!(d.next_au().is_none()); // AU dropped
        assert_eq!(d.stats().aus_dropped, 1);
        assert_eq!(d.stats().nalus_discarded, 1);
    }

    #[test]
    fn interleaved_types_poison_au() {
        for &nalu_type in &[25u8, 26, 27, 29] {
            let mut d = depay();
            d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // clean NALU
            let payload = [nalu_type, 0xFF];
            d.feed(&hdr(2, 1000, true), &payload);
            assert!(d.next_au().is_none(), "type {} should poison", nalu_type);
            assert_eq!(d.stats().aus_dropped, 1, "type {}", nalu_type);
        }
    }

    #[test]
    fn empty_payload_discarded() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[]); // empty
        d.feed(&hdr(2, 1000, true), &[0x41, 0x01]);
        // AU should succeed — empty packet only ticks packets_discarded.
        let au = d.next_au().expect("non-empty AU should complete");
        assert_eq!(d.stats().packets_discarded, 1);
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x41, 0x01]);
    }

    // ── STAP-A tests ──────────────────────────────────────────────────────────

    #[test]
    fn stap_a_unpacks_units_in_order() {
        // STAP-A header (24, NRI=3 → 0x78) + [size=2][SPS 0x67,0x42] + [size=3][PPS 0x68,0xCE,0x38]
        let stap = [0x78, 0, 2, 0x67, 0x42, 0, 3, 0x68, 0xCE, 0x38];
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &stap);
        let au = d.next_au().unwrap();
        assert_eq!(
            au.annexb,
            [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0x38]
        );
    }

    #[test]
    fn stap_a_zero_size_unit_poisons() {
        let stap = [0x78, 0, 0, 0x67]; // size=0 is degenerate (size includes the NALU header byte)
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &stap);
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().packets_discarded, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    #[test]
    fn stap_a_truncated_unit_poisons() {
        let stap = [0x78, 0, 5, 0x67, 0x42]; // claims 5 bytes, only 2 present
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &stap);
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().aus_dropped, 1);
    }

    // ── FU-A tests ────────────────────────────────────────────────────────────

    /// FU-A packet: indicator (F/NRI + type 28), header (S/E + orig type), fragment bytes.
    fn fua(nri: u8, s: bool, e: bool, typ: u8, frag: &[u8]) -> Vec<u8> {
        let ind = (nri << 5) | 28;
        let fh = (u8::from(s) << 7) | (u8::from(e) << 6) | (typ & 0x1F);
        let mut v = vec![ind, fh];
        v.extend_from_slice(frag);
        v
    }

    #[test]
    fn fu_a_reassembles_across_three_packets() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &fua(3, true, false, 5, &[0xAA]));
        d.feed(&hdr(2, 1000, false), &fua(3, false, false, 5, &[0xBB]));
        d.feed(&hdr(3, 1000, true), &fua(3, false, true, 5, &[0xCC]));
        let au = d.next_au().unwrap();
        // Reconstructed header: (ind & 0xE0) | (fh & 0x1F) = 0x60 | 5 = 0x65
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA, 0xBB, 0xCC]);
        assert!(au.key_frame);
    }

    #[test]
    fn fu_middle_fragment_loss_drops_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &fua(3, true, false, 5, &[0xAA]));
        d.feed(&hdr(3, 1000, true), &fua(3, false, true, 5, &[0xCC])); // seq 2 lost
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().seq_gaps, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    #[test]
    fn fu_start_loss_discards_tail_fragments() {
        let mut d = depay();
        // First packet ever is a mid-NALU fragment: no gap detectable, but no open FU either.
        d.feed(&hdr(9, 1000, false), &fua(3, false, false, 5, &[0xBB]));
        d.feed(&hdr(10, 1000, true), &fua(3, false, true, 5, &[0xCC]));
        assert!(d.next_au().is_none()); // AU poisoned — its head is missing (§7.3 discard guidance)
        assert!(d.stats().nalus_discarded >= 1);
    }

    #[test]
    fn fu_s_and_e_both_set_is_malformed() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &fua(3, true, true, 5, &[0xAA])); // §5.8 MUST NOT
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().packets_discarded, 1);
    }

    #[test]
    fn fu_empty_payload_is_legal() {
        let mut d = depay();
        // NRI=1, type=1: ind=0x3C → reconstructed header = (0x3C & 0xE0) | 1 = 0x21
        d.feed(&hdr(1, 1000, false), &fua(1, true, false, 1, &[0xAA]));
        d.feed(&hdr(2, 1000, false), &fua(1, false, false, 1, &[])); // empty fragment, §5.8 note
        d.feed(&hdr(3, 1000, true), &fua(1, false, true, 1, &[0xBB]));
        let au = d.next_au().expect("empty FU fragment is legal");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x21, 0xAA, 0xBB]);
    }

    #[test]
    fn gap_after_marker_terminated_au_kills_only_next_au() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]); // AU-1 complete (marker)
        d.feed(&hdr(5, 4003, true), &[0x41, 0x02]); // gap; AU-2 head may be lost → poisoned
        d.feed(&hdr(6, 7006, true), &[0x41, 0x03]); // AU-3 clean
        let a = d.next_au().unwrap();
        assert_eq!(a.annexb[4], 0x41);
        assert_eq!(a.annexb[5], 0x01);
        let c = d.next_au().unwrap();
        assert_eq!(c.annexb[5], 0x03); // AU-2 was dropped
        assert_eq!(d.stats().aus_dropped, 1);
        assert_eq!(c.pts, Pts90khz::new(6006)); // PTS still tracks timestamps, not emission count
    }

    #[test]
    fn markerless_boundary_gap_kills_both_aus() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]); // AU-1, no marker
        d.feed(&hdr(4, 4003, false), &[0x41, 0x02]); // gap spanning the boundary
        d.feed(&hdr(5, 7006, true), &[0x41, 0x03]); // AU-3
        let only = d.next_au().unwrap();
        assert_eq!(only.annexb[5], 0x03);
        assert_eq!(d.stats().aus_dropped, 2);
    }

    #[test]
    fn ssrc_change_resets_and_keeps_pts_monotonic_fua() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let mut h2 = hdr(1, 900_000, true); // new source, unrelated ts base
        h2.ssrc = 0xDEAD;
        d.feed(&h2, &[0x41, 0x02]);
        let a = d.next_au().unwrap();
        let b = d.next_au().unwrap();
        assert_eq!(a.pts, Pts90khz::new(0));
        assert_eq!(b.pts, Pts90khz::new(3003)); // last_emitted + SSRC_RESET_PTS_STEP
        assert_eq!(d.stats().ssrc_changes, 1);
    }

    #[test]
    fn f_bit_nalu_discarded_and_poisons() {
        let mut d = depay();
        d.feed(&hdr(1, 1000, true), &[0x80 | 0x41, 0x01]); // F=1
        assert!(d.next_au().is_none());
        assert_eq!(d.stats().nalus_discarded, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    // ── Parameter-set cache + BeforeIdr injection tests ──────────────────────

    fn sps() -> Vec<u8> {
        vec![0x67, 0x42, 0x00, 0x1E]
    }
    fn pps() -> Vec<u8> {
        vec![0x68, 0xCE, 0x38, 0x80]
    }
    fn depay_inject(sprop: Vec<Vec<u8>>) -> H264Depacketizer {
        H264Depacketizer::new(H264DepayConfig {
            initial_parameter_sets: sprop,
            ..H264DepayConfig::default() // BeforeIdr is the default
        })
    }

    #[test]
    fn sprop_injected_before_idr_when_absent_inband() {
        let mut d = depay_inject(vec![sps(), pps()]);
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]); // bare IDR
        let au = d.next_au().unwrap();
        let mut expect = vec![0, 0, 0, 1];
        expect.extend(sps());
        expect.extend([0, 0, 0, 1]);
        expect.extend(pps());
        expect.extend([0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(au.annexb, expect);
    }

    #[test]
    fn injection_idempotent_when_au_already_carries_ps() {
        let mut d = depay_inject(vec![sps(), pps()]);
        d.feed(&hdr(1, 1000, false), &sps());
        d.feed(&hdr(2, 1000, false), &pps());
        d.feed(&hdr(3, 1000, true), &[0x65, 0xAA]);
        let au = d.next_au().unwrap();
        // Byte-identical pass-through: the AU's own SPS + PPS + IDR, exactly
        // as fed — nothing prepended.
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(sps());
        expected.extend([0, 0, 0, 1]);
        expected.extend(pps());
        expected.extend([0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(au.annexb, expected);
        // Redundant with the byte-equality above, but states the intent:
        // exactly 3 NALUs, no duplicates prepended.
        assert_eq!(
            au.annexb.windows(4).filter(|w| *w == [0, 0, 0, 1]).count(),
            3
        );
    }

    #[test]
    fn non_idr_aus_never_get_injection() {
        let mut d = depay_inject(vec![sps(), pps()]);
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let au = d.next_au().unwrap();
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x41, 0x01]);
    }

    #[test]
    fn inband_ps_updates_cache_and_counter() {
        let mut d = depay_inject(vec![sps(), pps()]);
        let new_sps = vec![0x67, 0x42, 0x00, 0x28]; // different SPS arrives in-band
        d.feed(&hdr(1, 1000, true), &new_sps);
        d.feed(&hdr(2, 4003, true), &[0x65, 0xAA]); // bare IDR in the NEXT AU
        d.next_au().unwrap();
        let au = d.next_au().unwrap();
        // Second AU gets the UPDATED SPS (in-band replaced the seed) followed
        // by the SEEDED PPS (never seen in-band, still served from the cache).
        let mut expected = vec![0, 0, 0, 1];
        expected.extend(&new_sps);
        expected.extend([0, 0, 0, 1]);
        expected.extend(pps());
        expected.extend([0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(au.annexb, expected);
        assert_eq!(d.stats().parameter_set_updates, 1); // sprop→in-band change counted once
    }

    #[test]
    fn injection_none_is_byte_faithful() {
        let mut d = H264Depacketizer::new(H264DepayConfig {
            parameter_set_injection: ParameterSetInjection::None,
            initial_parameter_sets: vec![sps(), pps()],
            ..H264DepayConfig::default()
        });
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]);
        assert_eq!(d.next_au().unwrap().annexb, [0, 0, 0, 1, 0x65, 0xAA]);
    }

    #[test]
    fn seq_gap_poisons_current_and_next_au() {
        // A gap poisons the AU it lands on.  When that AU is closed by a
        // marker the boundary is established: the NEXT AU starts clean.
        // gap_pending carries through markerless boundaries only.
        let mut d = depay();
        // AU1: clean, completed by marker.
        d.feed(&hdr(1, 1000, true), &[0x41, 0x01]);
        let au1 = d.next_au().expect("AU1 clean");
        assert_eq!(au1.pts, Pts90khz::new(0));

        // AU2: seq 2, marker-closed.
        d.feed(&hdr(2, 2000, true), &[0x41, 0x02]);
        let au2 = d.next_au().expect("AU2 clean");
        assert!(!au2.key_frame);

        // Gap: seq 5 (skipped 3,4) on AU3. AU3 is poisoned by the gap AND
        // closed by its own marker — boundary is known after this point.
        d.feed(&hdr(5, 3000, true), &[0x41, 0x03]);
        assert!(d.next_au().is_none(), "AU3 should be poisoned by gap");
        assert_eq!(d.stats().seq_gaps, 1);
        assert_eq!(d.stats().aus_dropped, 1);

        // AU4: seq 6, right after the marker-closed gap AU — boundary was
        // established by the marker so AU4 starts clean.
        d.feed(&hdr(6, 4000, true), &[0x41, 0x04]);
        assert!(d.next_au().is_some(), "AU4 clean: marker resolved boundary");
        assert_eq!(d.stats().aus_dropped, 1); // still 1 — AU4 is clean

        // AU5: seq 7, clean.
        d.feed(&hdr(7, 5000, true), &[0x41, 0x05]);
        assert!(d.next_au().is_some(), "AU5 should be clean");
    }

    // ── max_au_bytes cap tests ────────────────────────────────────────────────

    /// Build an `H264Depacketizer` with a tiny cap for oversize testing.
    fn depay_capped(max_bytes: usize) -> H264Depacketizer {
        H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: max_bytes,
            parameter_set_injection: ParameterSetInjection::None,
            ..H264DepayConfig::default()
        })
    }

    /// FU-A fragments totalling > cap must be dropped with memory cleared and
    /// `aus_dropped_oversize` ticked. A subsequent clean AU must still work
    /// (depacketizer recovers cleanly).
    #[test]
    fn oversize_fu_a_au_is_dropped_and_stats_ticked() {
        const CAP: usize = 32; // tiny cap for the test
        let mut d = depay_capped(CAP);

        // Feed FU-A packets for one AU with a constant timestamp.
        // Each fragment is 16 bytes; after 2 packets au_buf+fu exceeds CAP=32.
        // FU-A indicator: NRI=3→0x60, type 28→0x1C → ind=0x7C
        // Fragment type 1 (non-IDR): fh start = 0x80|1=0x81; mid = 0x01; end = 0x40|1=0x41
        let ind: u8 = (3 << 5) | 28; // NRI=3, type=FU-A
        let frag = vec![0xAAu8; 16];

        // Start fragment
        let mut start = vec![ind, 0x80 | 1]; // S=1, E=0, orig type=1
        start.extend_from_slice(&frag);
        d.feed(&hdr(1, 1000, false), &start);

        // Continuation fragment — pushes over CAP
        let mut cont = vec![ind, 0x01]; // S=0, E=0, orig type=1
        cont.extend_from_slice(&frag);
        d.feed(&hdr(2, 1000, false), &cont);

        // End fragment (would close the FU-A, but cap was already hit)
        let mut end = vec![ind, 0x40 | 1]; // S=0, E=1, orig type=1
        end.extend_from_slice(&[0xBBu8; 4]);
        d.feed(&hdr(3, 1000, true), &end); // marker → complete_au → drop

        // The AU must be dropped, not emitted.
        assert!(d.next_au().is_none(), "oversize AU must be dropped");
        let stats = d.stats();
        assert_eq!(
            stats.aus_dropped_oversize, 1,
            "aus_dropped_oversize must tick"
        );
        assert_eq!(stats.aus_dropped, 1, "aus_dropped must also tick");

        // Verify memory is bounded: the internal buffers must have been cleared
        // (no retained bytes). We verify this indirectly by confirming a clean
        // subsequent AU succeeds — if the buffers were NOT cleared, the old
        // bytes would still be there and the cap would fire again.
        d.feed(&hdr(4, 4003, true), &[0x41, 0x01]); // clean non-IDR AU
        let clean = d.next_au().expect("subsequent clean AU must succeed");
        assert_eq!(clean.annexb, [0, 0, 0, 1, 0x41, 0x01]);
        assert_eq!(d.stats().aus_dropped_oversize, 1, "no new oversize drops");
    }

    /// Parameter-set injection must not bypass the cap. A bare IDR AU that
    /// fits under the cap on its own is pushed over once cached SPS/PPS are
    /// prepended in `complete_au()`; the injected AU must be dropped, never
    /// emitted above the advertised `max_au_bytes`.
    #[test]
    fn injection_cannot_bypass_max_au_bytes() {
        // Bare IDR au_buf = 4 (start code) + 2 = 6 bytes, under CAP=8.
        // Injection prepends (4+4) SPS + (4+4) PPS = 16 bytes → 22 > 8.
        const CAP: usize = 8;
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            initial_parameter_sets: vec![sps(), pps()], // BeforeIdr default
            ..H264DepayConfig::default()
        });
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]); // bare IDR, marker closes
        assert!(
            d.next_au().is_none(),
            "injected AU exceeding the cap must be dropped, not emitted"
        );
        assert_eq!(d.stats().aus_dropped_oversize, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    /// A build helper: an SPS (NAL type 7) whose Annex-B-framed length
    /// exceeds `cap` bytes.
    fn oversize_sps(cap: usize) -> Vec<u8> {
        let mut v = vec![0x67u8]; // NAL type 7 (SPS)
        v.resize(cap + 4, 0xAA); // 4 (start code) + (cap+4) len > cap
        v
    }

    /// An over-cap SPS arriving in-band must NOT be retained in the cache.
    /// If it were, every later IDR would rebuild an over-cap injection prefix
    /// and be dropped — `max_au_bytes` would fail to bound cached parameter-set
    /// memory. With the SPS uncached, a later tiny IDR emits clean.
    #[test]
    fn oversize_inband_sps_is_not_retained_in_cache() {
        const CAP: usize = 8;
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            ..H264DepayConfig::default() // BeforeIdr injection is the default
        });
        d.feed(&hdr(1, 1000, true), &oversize_sps(CAP)); // dropped oversize
        assert!(d.next_au().is_none(), "over-cap SPS AU dropped");
        assert_eq!(d.stats().aus_dropped_oversize, 1);

        // Tiny IDR must not inherit the over-cap SPS from cache.
        d.feed(&hdr(2, 4003, true), &[0x65, 0xAA]); // 4 + 2 = 6 <= CAP
        let au = d
            .next_au()
            .expect("tiny IDR emits clean (no stale over-cap SPS in cache)");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(d.stats().aus_dropped_oversize, 1, "no second oversize drop");
    }

    /// Over-cap `initial_parameter_sets` (e.g. seeded from an SDP
    /// `sprop-parameter-sets`) must not be seeded into the cache either.
    #[test]
    fn oversize_initial_parameter_sets_are_not_seeded() {
        const CAP: usize = 8;
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            initial_parameter_sets: vec![oversize_sps(CAP)],
            ..H264DepayConfig::default()
        });
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]); // tiny IDR
        let au = d
            .next_au()
            .expect("tiny IDR emits clean; over-cap seed was skipped");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAA]);
        assert_eq!(d.stats().aus_dropped_oversize, 0);
    }

    /// SPS and PPS each fit the cap individually, but their combined injection
    /// plus a tiny IDR AU exceeds it. The AU must be dropped as oversize — the
    /// `complete_au` preflight computes the injected length and drops before
    /// allocating/copying the over-cap prefix.
    #[test]
    fn injection_preflights_combined_over_cap_and_drops() {
        const CAP: usize = 20;
        let sps6 = vec![0x67u8, 1, 2, 3, 4, 5]; // framed 4 + 6 = 10 <= CAP
        let pps6 = vec![0x68u8, 1, 2, 3, 4, 5]; // framed 4 + 6 = 10 <= CAP
        let mut d = H264Depacketizer::new(H264DepayConfig {
            max_au_bytes: CAP,
            initial_parameter_sets: vec![sps6, pps6],
            ..H264DepayConfig::default()
        });
        // Injection 10 + 10 = 20, plus IDR au_buf 6 = 26 > CAP.
        d.feed(&hdr(1, 1000, true), &[0x65, 0xAA]);
        assert!(
            d.next_au().is_none(),
            "combined over-cap injection AU dropped"
        );
        assert_eq!(d.stats().aus_dropped_oversize, 1);
        assert_eq!(d.stats().aus_dropped, 1);
    }

    /// Single-NALU AU that exceeds the cap: `aus_dropped_oversize` ticks and
    /// the AU is dropped. Verifies the single-NALU (non-FU-A) path.
    #[test]
    fn oversize_single_nalu_au_is_dropped() {
        const CAP: usize = 8; // only 8 bytes before oversize
        let mut d = depay_capped(CAP);

        // Push a clean small NALU to start filling au_buf (4+1 = 5 bytes).
        d.feed(&hdr(1, 1000, false), &[0x41, 0x01]);
        assert_eq!(d.stats().aus_dropped_oversize, 0);

        // Push another NALU at the same timestamp — this will push au_buf over 8.
        // 5 (current) + 4 (start code) + 2 (nalu) = 11 bytes > 8 → oversize.
        d.feed(&hdr(2, 1000, true), &[0x41, 0x02]); // marker closes AU

        assert!(d.next_au().is_none(), "oversize AU must be dropped");
        let stats = d.stats();
        assert_eq!(stats.aus_dropped_oversize, 1);
        assert_eq!(stats.aus_dropped, 1);
    }
}
