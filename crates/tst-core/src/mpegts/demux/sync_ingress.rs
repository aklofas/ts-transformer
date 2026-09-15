//! Demuxer sync-ingress state machine: byte-aligned 188-byte packet
//! detection + sync-recovery buffer compaction + per-packet PCR / CC
//! anomaly checks.
//!
//! Hosts three module-level constants tuned in Phase 4 (`MAX_SYNC_BUF_BYTES`
//! caps adversarial-input memory growth; `SYNC_SEARCH_WINDOW` bounds
//! per-feed sync-hunt work; `PCR_ANOMALY_THRESHOLD` discriminates real PCR
//! jumps from steady-state drift). Per Wave 6.B Decision DB7, constants
//! follow their consumers.
//!
//! Helper methods are `pub(super)` so the `Demuxer` coordinator (`demuxer.rs`)
//! can call them; the module itself is private (`mod sync_ingress` in
//! `mpegts/demux/mod.rs`).

use crate::mpegts::common::{pcr_diff_27mhz, pid};
use crate::mpegts::demux::event::{DiscontinuityKind, NonConformantIssue, StreamId};

/// Maximum bytes the demuxer scans during sync recovery before declaring
/// the stream unrecoverable.
pub(super) const SYNC_SEARCH_WINDOW: usize = crate::mpegts::common::TS_PACKET_SIZE * 32;

/// Default ceiling on `Demuxer::sync_buf` — the value used when
/// [`crate::mpegts::demux::DemuxerConfig::sync_buf_cap`] is `None`. `feed`
/// always enforces the cap BEFORE `extend_from_slice` so an oversized
/// single-call feed (multi-GB of garbage) cannot allocate the whole input
/// before the check fires. The 4 MiB value matches ffmpeg's
/// `MpegTSSectionFilter` ceiling and is comfortably larger than
/// `SYNC_SEARCH_WINDOW` (~6 KiB), so well-formed streams are unaffected.
/// Callers feeding whole-file inputs larger than 4 MiB should raise the
/// ceiling via `DemuxerConfig::sync_buf_cap`.
pub(super) const MAX_SYNC_BUF_BYTES: usize = 4 << 20;

/// PCR jump threshold beyond which we emit `PcrAnomaly`. 1 second @ 27 MHz.
pub(super) const PCR_ANOMALY_THRESHOLD: i64 = 27_000_000;

/// Sync re-acquisition N-of-M: when a candidate sync byte is found, only
/// declare sync acquired if at least `SYNC_REACQ_N` of the next
/// `SYNC_REACQ_M` strides at +188, +376, ... also carry 0x47. This
/// rejects false sync on a stray 0x47 inside PES payload, descriptors,
/// or other in-band data.
///
/// Values match ffmpeg `libavformat/mpegts.c::mpegts_resync` (5 of 7),
/// which is the long-standing tested value for the same problem. ffmpeg's
/// `MAX_RESYNC_SIZE` is exposed via the `resync_size` AVOption; we match
/// the upstream default. Per H.222.0 §2.4.3.2 a real 188-byte aligned
/// stream MUST satisfy this; a 0x47 byte inside a PES payload almost
/// never aligns 5-of-7 times at 188-byte stride.
pub(super) const SYNC_REACQ_N: usize = 5;
pub(super) const SYNC_REACQ_M: usize = 7;

/// Possible outcomes of the N-of-M re-acquisition check at a candidate
/// `live[0] == 0x47` byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NofMResult {
    /// Candidate matched ≥ `SYNC_REACQ_N` of `SYNC_REACQ_M` strides at
    /// 188-byte boundaries. Accept as the next packet's sync byte.
    Accept,
    /// Candidate matched < `SYNC_REACQ_N` of the full `SYNC_REACQ_M`
    /// window. Reject; advance past this byte and keep searching.
    Reject,
    /// Buffer didn't contain enough bytes to complete the full window,
    /// AND the partial evidence isn't conclusive (matches < N but the
    /// remaining unchecked strides could still bring the total to N or
    /// above). Caller should pause (return `Ok(())`) and wait for more
    /// bytes before deciding.
    NeedMoreBytes,
}

/// Inspect `live` (starting at a candidate 0x47) and decide whether
/// enough 188-byte strides also carry 0x47 to declare sync acquired.
/// See [`NofMResult`] for the three outcomes.
///
/// The decision must be sound under three buffer-fill cases:
///
/// 1. **Full window** (`live.len() >= SYNC_REACQ_M * 188`): count all
///    strides, accept if `matches >= SYNC_REACQ_N`.
/// 2. **Partial window with enough evidence to accept** (matches at the
///    candidate + checked strides already >= N): accept immediately.
///    Avoids waiting on bytes we don't need.
/// 3. **Partial window with enough evidence to reject** (matches +
///    remaining-unchecked < N): reject. Remaining strides can't lift
///    the count above N regardless.
/// 4. **Partial window, undecided**: return `NeedMoreBytes`. Caller
///    keeps the candidate's bytes buffered and re-checks on the next
///    `feed`.
pub(super) fn sync_n_of_m_check(live: &[u8]) -> NofMResult {
    debug_assert_eq!(live.first(), Some(&crate::mpegts::common::TS_SYNC_BYTE));
    let stride = crate::mpegts::common::TS_PACKET_SIZE;
    // The candidate itself counts as the first match.
    let mut matches: usize = 1;
    let mut checked: usize = 1;
    for k in 1..SYNC_REACQ_M {
        let off = k * stride;
        if off >= live.len() {
            break;
        }
        checked += 1;
        if live[off] == crate::mpegts::common::TS_SYNC_BYTE {
            matches += 1;
        }
    }
    if matches >= SYNC_REACQ_N {
        return NofMResult::Accept;
    }
    // Could the remaining unchecked strides push us over N?
    let remaining = SYNC_REACQ_M - checked;
    if matches + remaining < SYNC_REACQ_N {
        return NofMResult::Reject;
    }
    // Indeterminate with current buffer — wait for more bytes.
    NofMResult::NeedMoreBytes
}

impl super::demuxer::Demuxer {
    /// Reclaim the consumed prefix of `sync_buf` once it grows past half
    /// the live size (or 1 MiB, whichever is larger). The half-and-compact
    /// rule keeps total memmove work amortized-linear in bytes fed; the
    /// 1 MiB floor avoids churn on tiny live regions.
    pub(super) fn compact_sync_buf(&mut self) {
        let consumed = self.sync_consumed;
        let live = self.sync_buf.len() - consumed;
        if consumed >= live.max(1 << 20) {
            self.sync_buf.drain(..consumed);
            self.sync_consumed = 0;
        }
    }

    /// Identity to attach to a PCR-carrying packet's timing issues.
    /// Elementary PIDs resolve through `lookup_stream`; a dedicated PCR
    /// PID (H.222.0 §2.4.4.9 lets the PCR ride a PID that carries no
    /// elementary stream — common in broadcast and hardware muxes) has no
    /// stream entry, so fall back to an anonymous id whose
    /// `program_number` is the program that declared `pid` as its
    /// `PCR_PID` (0 if none has). CORR-13.
    pub(super) fn pcr_stream_id(&self, pid: u16) -> StreamId {
        self.lookup_stream(pid).unwrap_or_else(|| {
            let program_number = self
                .programs
                .values()
                .find(|t| t.pcr_pid == Some(pid))
                .map_or(0, |t| t.program_number);
            StreamId::anonymous(pid, program_number)
        })
    }

    pub(super) fn check_pcr(&mut self, pkt: &crate::mpegts::demux::ts::TsPacket<'_>) {
        // Per ITU-T H.222.0 §2.4.3.5, each program carries its own time base
        // via its declared PCR PID. PCR comparisons MUST stay within a
        // single PID's timeline; comparing across PIDs in a multi-program TS
        // produces spurious PcrAnomaly events (validate-1 B1 / Codex
        // TS-TIME-01). Key the last-seen map by `pkt.pid` (the on-wire PCR
        // PID) — that's the canonical identifier of a time base.
        //
        // Malformed-PCR check fires first (validate-1 B12): if the on-wire
        // PCR field violated H.222.0 §2.4.3.5 syntax, the parser already
        // dropped the decoded value (`pcr_27mhz = None`) and recorded the
        // reason in `pcr_malformed`. Surface that here as a separate issue
        // so lenient receivers see the corruption while strict-mode timing
        // categories escalate to StrictRejection.
        if let Some(kind) = pkt.pcr_malformed {
            let stream = self.pcr_stream_id(pkt.pid);
            self.queue_nonconformant(stream, NonConformantIssue::PcrMalformed { kind });
        }
        // Nested if-let (not let-chain) for MSRV 1.85 — let-chains require 1.88.
        if let Some(now) = pkt.pcr_27mhz {
            if let Some(&last) = self.last_pcr_by_pid.get(&pkt.pid) {
                let diff = pcr_diff_27mhz(now, last);
                if diff.abs() > PCR_ANOMALY_THRESHOLD {
                    // Dedicated PCR PIDs have no `lookup_stream` entry —
                    // resolve through the PMT's PCR_PID instead of
                    // dropping the issue (CORR-13).
                    let stream = self.pcr_stream_id(pkt.pid);
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::PcrAnomaly { delta: diff },
                    );
                }
            }
            self.last_pcr_by_pid.insert(pkt.pid, now);
        }
    }

    /// Returns `(cc_jumped, is_duplicate)`.
    ///
    /// `cc_jumped` is `true` when a CC gap was observed AND not suppressed by
    /// `discontinuity_indicator`. The caller (`process_packet`) uses this
    /// signal to gate strict-mode PSI reassembly drops in `handle_psi`.
    ///
    /// `is_duplicate` is `true` when the packet is a spec-legal duplicate per
    /// H.222.0 §2.4.3.3 (same CC as the preceding payload packet, no
    /// `discontinuity_indicator`). The caller MUST suppress all payload
    /// routing for such packets — they carry no new data. A THIRD packet with
    /// the same CC on the same PID IS a discontinuity (the "only-two" rule is
    /// enforced via `self.dup_by_pid`).
    ///
    /// Side effect: clears `self.last_psi_cc_jump` at entry, sets it to
    /// `Some((expected, observed))` when a real jump fires. `handle_psi`
    /// drains it via `.take()` when emitting `PsiCcDiscontinuity`.
    pub(super) fn check_continuity(
        &mut self,
        pkt: &crate::mpegts::demux::ts::TsPacket<'_>,
    ) -> (bool, bool) {
        self.last_psi_cc_jump = None;
        // Per ITU-T H.222.0 §2.4.3.3, the continuity_counter field on null
        // PID (0x1FFF) packets is undefined and MUST NOT be validated. Null
        // packets are >50% of bytes in CBR feeds, so tracking them would
        // grow `cc_by_pid` with a sentinel entry that's never useful and
        // could spuriously fire ContinuityJump (validate-1 act-now Slice 06
        // M-02). PCR tracking is intentionally NOT skipped — PCR may
        // legitimately ride null packets per §2.4.3.5 — that path lives in
        // `check_pcr` and is keyed on `pcr_27mhz.is_some()`.
        if pkt.pid == pid::NULL {
            return (false, false);
        }
        // Adaptation-field-only packets (afc='10') do not carry a payload and
        // do not increment the continuity_counter — they are neither duplicates
        // nor CC advances. Skip CC validation entirely to avoid misclassification.
        if !pkt.has_payload {
            return (false, false);
        }
        let mut real_jump = false;
        if let Some(prev_cc) = self.cc_by_pid.get(&pkt.pid).copied() {
            let expected = (prev_cc + 1) & 0x0F;

            // Spec-legal duplicate: same CC, no discontinuity_indicator, AND
            // byte-identical content. H.222.0 §2.4.3.3 — "a packet may be
            // sent exactly twice with the same continuity_counter value; such
            // a duplicate shall not cause discontinuity", and "each byte of
            // the original packet shall be duplicated" with the sole
            // exception of the PCR field, which a duplicate may refresh —
            // `pcr_masked_identical` implements exactly that compare. A
            // same-CC packet whose OTHER bytes differ is NOT a duplicate
            // (non-conformant input, e.g. an encoder that forgot to advance
            // the counter): it falls through to the ordinary CC-jump path and
            // its payload IS routed, so differing data is never swallowed.
            // (`check_pcr` runs before this fn, so a suppressed duplicate's
            // refreshed PCR is still tracked.) Only one duplicate is allowed
            // ("only-two" rule). A third identical packet with the same CC IS
            // a real discontinuity — treat it like a CC jump.
            let is_identical_dup = pkt.continuity_counter == prev_cc
                && !pkt.discontinuity_indicator
                && self
                    .last_pkt_raw_by_pid
                    .get(&pkt.pid)
                    .is_some_and(|prev| pcr_masked_identical(prev, pkt.raw));
            if is_identical_dup {
                if self.dup_by_pid.contains(&pkt.pid) {
                    // Third (or later) identical packet with the same CC —
                    // beyond the spec's only-two allowance. Surface exactly
                    // ONE discontinuity for this packet and route its payload
                    // (the pre-duplicate-support behavior). `dup_by_pid`
                    // stays SET and the stored original bytes stay in place,
                    // so EVERY further identical packet in the run lands here
                    // too — the only-two enforcement does not reset mid-run.
                    // Early return keeps the generic CC-jump block below from
                    // recording the same jump a second time. (cc_by_pid would
                    // be re-inserted with the identical value — skipping the
                    // tail is a no-op for it.)
                    self.last_psi_cc_jump = Some((expected, pkt.continuity_counter));
                    if let Some(stream) = self.lookup_stream(pkt.pid) {
                        self.record_discontinuity(
                            stream,
                            DiscontinuityKind::ContinuityJump {
                                expected,
                                observed: pkt.continuity_counter,
                            },
                        );
                    }
                    return (true, false);
                }
                // First duplicate — suppress and mark.
                self.dup_by_pid.insert(pkt.pid);
                // Do NOT update cc_by_pid / last_pkt_raw_by_pid — the next
                // non-duplicate packet on this PID must still expect
                // prev_cc + 1, and a second duplicate must compare against
                // the ORIGINAL packet's bytes.
                return (false, true);
            }
            // Normal advance, discontinuity_indicator, or a same-CC packet
            // with differing bytes (routed as an ordinary CC jump below) —
            // clear any pending duplicate state for this PID.
            self.dup_by_pid.remove(&pkt.pid);

            // Per ISO/IEC 13818-1 §2.4.3.5, when discontinuity_indicator=1
            // the CC is explicitly permitted to be discontinuous on this
            // packet. Suppress the ContinuityJump (matches ffmpeg
            // mpegts.c:3075-3078); the separate `AdaptationFieldFlag`
            // event below already surfaces the discontinuity hint to
            // consumers, so emitting both would double-count.
            if expected != pkt.continuity_counter && !pkt.discontinuity_indicator {
                real_jump = true;
                self.last_psi_cc_jump = Some((expected, pkt.continuity_counter));
                if let Some(stream) = self.lookup_stream(pkt.pid) {
                    self.record_discontinuity(
                        stream,
                        DiscontinuityKind::ContinuityJump {
                            expected,
                            observed: pkt.continuity_counter,
                        },
                    );
                }
            }
        } else {
            // First packet ever seen on this PID — no prev state, no duplicate
            // check possible. Clear dup state defensively (shouldn't be set
            // since cc_by_pid and dup_by_pid are always cleared together, but
            // this keeps invariants tight).
            self.dup_by_pid.remove(&pkt.pid);
        }
        if pkt.discontinuity_indicator {
            if let Some(stream) = self.lookup_stream(pkt.pid) {
                self.record_discontinuity(stream, DiscontinuityKind::AdaptationFieldFlag);
            }
        }
        self.cc_by_pid.insert(pkt.pid, pkt.continuity_counter);
        self.last_pkt_raw_by_pid.insert(pkt.pid, *pkt.raw);
        (real_jump, false)
    }
}

/// Byte-compare two 188-byte TS packets for the H.222.0 §2.4.3.3 duplicate
/// rule: every byte must match EXCEPT the 6-byte `program_clock_reference`
/// field, which a legal duplicate may refresh ("each byte of the original
/// packet shall be duplicated, with the exception that in the program clock
/// reference fields, if present, a valid value shall be encoded"). The PCR
/// bytes (offsets 6..12) are masked only when BOTH packets carry adaptation
/// fields with `PCR_flag = 1` and room for the field; any other divergence
/// (header flags, adaptation length, payload bytes) means "not a duplicate".
fn pcr_masked_identical(a: &[u8; 188], b: &[u8; 188]) -> bool {
    if a == b {
        return true;
    }
    let has_pcr = |p: &[u8; 188]| {
        let afc = (p[3] >> 4) & 0b11;
        afc & 0b10 != 0 && p[4] >= 7 && (p[5] & 0x10) != 0
    };
    if !(has_pcr(a) && has_pcr(b)) {
        return false;
    }
    a[..6] == b[..6] && a[12..] == b[12..]
}

#[cfg(test)]
mod tests {
    use super::super::demuxer::Demuxer;
    use crate::mpegts::common::pid;

    /// Build a payload-only TS packet on the given `pid` with CC=0.
    /// Mirrors `payload_packet_for_test` in `demuxer.rs` tests.
    fn payload_packet(pid: u16, cc: u8) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (pid >> 8) as u8 & 0x1F;
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x10 | (cc & 0x0F); // payload-only + CC
        buf
    }

    #[test]
    fn null_pid_does_not_grow_cc_map() {
        // H.222.0 §2.4.3.3: continuity_counter on null PID (0x1FFF) is
        // undefined and MUST NOT be tracked. Feed 100 null packets with
        // varying CC values — `cc_by_pid` must remain empty.
        let mut demuxer = Demuxer::new();
        for i in 0..100u8 {
            demuxer
                .feed_aligned(&payload_packet(pid::NULL, i & 0x0F))
                .unwrap();
        }
        assert_eq!(
            demuxer.cc_by_pid.len(),
            0,
            "null PID packets must not populate cc_by_pid"
        );
    }

    /// Build a packet with adaptation field + PCR (afc='11') on `pid`.
    /// PCR bytes 6..12 carry `pcr_seed`; payload follows the 8-byte AF.
    fn pcr_packet(pid: u16, cc: u8, pcr_seed: u8) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = (pid >> 8) as u8 & 0x1F;
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x30 | (cc & 0x0F); // AF + payload
        buf[4] = 7; // adaptation_field_length: flags + 6 PCR bytes
        buf[5] = 0x10; // PCR_flag
        for b in &mut buf[6..12] {
            *b = pcr_seed;
        }
        buf
    }

    #[test]
    fn pcr_masked_identical_exact_match() {
        let a = payload_packet(0x0100, 3);
        assert!(super::pcr_masked_identical(&a, &a));
    }

    #[test]
    fn pcr_masked_identical_rejects_payload_difference() {
        let a = payload_packet(0x0100, 3);
        let mut b = a;
        b[100] ^= 0x01;
        assert!(!super::pcr_masked_identical(&a, &b));
    }

    #[test]
    fn pcr_masked_identical_allows_refreshed_pcr() {
        // H.222.0 §2.4.3.3: a legal duplicate may carry an updated PCR.
        let a = pcr_packet(0x0100, 3, 0xAA);
        let b = pcr_packet(0x0100, 3, 0xBB);
        assert!(super::pcr_masked_identical(&a, &b));
    }

    #[test]
    fn pcr_masked_identical_rejects_pcr_asymmetry() {
        // One packet has a PCR, the other does not → header/AF bytes differ
        // and the mask must NOT apply.
        let a = pcr_packet(0x0100, 3, 0xAA);
        let b = payload_packet(0x0100, 3);
        assert!(!super::pcr_masked_identical(&a, &b));
    }

    #[test]
    fn pcr_masked_identical_rejects_header_flag_difference() {
        // Same payload but PUSI flipped → not a duplicate.
        let a = payload_packet(0x0100, 3);
        let mut b = a;
        b[1] |= 0x40; // set payload_unit_start
        assert!(!super::pcr_masked_identical(&a, &b));
    }
}
