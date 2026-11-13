//! Demuxer sync-ingress state machine: byte-aligned 188-byte packet
//! detection + sync-recovery buffer compaction + per-packet PCR / CC
//! anomaly checks.
//!
//! Hosts the module-level constants (`MAX_SYNC_BUF_BYTES`
//! caps adversarial-input memory growth; `SYNC_SEARCH_WINDOW` bounds
//! per-feed sync-hunt work; `PCR_ANOMALY_THRESHOLD` discriminates real PCR
//! jumps from steady-state drift; `SYNC_REACQ_N` / `SYNC_REACQ_M` set the
//! N-of-M sync re-acquisition rule); constants live with their consumers.
//!
//! Helper methods are `pub(super)` so the `Demuxer` coordinator (`demuxer.rs`)
//! can call them; the module itself is private (`mod sync_ingress` in
//! `mpegts/demux/mod.rs`).

use crate::mpegts::common::{pcr_diff_27mhz, pid};
use crate::mpegts::demux::event::{DiscontinuityKind, NonConformantIssue, StreamId, StreamKind};
use crate::mpegts::demux::pid_state::PidState;
use crate::mpegts::sync::TsSync;

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
/// The 188-byte stride is fixed (no other packet size is probed). Per
/// H.222.0 §2.4.3.2 a real 188-byte aligned stream MUST satisfy this;
/// a 0x47 byte inside a PES payload almost never aligns 5-of-7 times at
/// 188-byte stride.
pub(super) const SYNC_REACQ_N: usize = 5;
pub(super) const SYNC_REACQ_M: usize = 7;

/// The N-of-M re-acquisition rule as a [`TsSync`] check: `Some(true)` accepts
/// the candidate, `Some(false)` rejects it, `None` waits for more bytes.
pub(super) const SYNC_REACQ: TsSync = TsSync::n_of_m(SYNC_REACQ_N, SYNC_REACQ_M);

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
    /// `PCR_PID` (0 if none has).
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
        // via its declared PCR PID. PCR comparisons MUST stay within a single
        // PID's timeline; comparing across PIDs in a multi-program TS produces
        // spurious PcrAnomaly events. Key the last-seen map by `pkt.pid` (the
        // on-wire PCR PID) — that's the canonical identifier of a time base.
        // Only a PID that some program currently declares as `PCR_PID` is
        // tracked, and its entry lives exactly as long as that declaration
        // does.
        //
        // Malformed-PCR check fires first: if the on-wire
        // PCR field violated H.222.0 §2.4.3.5 syntax, the parser already
        // dropped the decoded value (`pcr_27mhz = None`) and recorded the
        // reason in `pcr_malformed`. Surface that here as a separate issue
        // so lenient receivers see the corruption while strict-mode timing
        // categories escalate to StrictRejection.
        if let Some(kind) = pkt.pcr_malformed {
            let stream = self.pcr_stream_id(pkt.pid);
            self.queue_nonconformant(stream, NonConformantIssue::PcrMalformed { kind });
        }
        // Only a packet with a PCR or the flag needs the declaration, so the
        // program scan stays off the per-packet path.
        let declared = (pkt.pcr_27mhz.is_some() || pkt.discontinuity_indicator)
            && self.programs.values().any(|t| t.pcr_pid == Some(pkt.pid));
        // H.222.0 §2.4.3.5: `discontinuity_indicator = 1` on the PCR_PID means
        // the next PCR on that PID samples a new system time clock. That PCR
        // may ride this packet or a later one, so the expectation is held
        // until it arrives.
        // A spec-legal duplicate repeats the original's indicator, not its
        // reset: the PES the original opened is already on the new clock.
        if declared && pkt.discontinuity_indicator && !self.is_spec_duplicate(pkt) {
            self.pcr.expect_new_timebase(pkt.pid);
        }
        if let Some(now) = pkt.pcr_27mhz {
            // A time base belongs to a program, and a program names the ONE
            // PID that carries it (`PCR_PID`, H.222.0 §2.4.4.9). A PCR on any
            // other PID is not a timeline this demuxer owes a report on: a
            // stray or corrupted packet (a damaged PID field can name any
            // 13-bit value) must neither seed `last_pcr_by_pid` nor, hours
            // later, fire a `PcrAnomaly` against it — under
            // `StrictMode::TimingOnly` that anomaly is a `StrictRejection`,
            // so one corrupted PID field on the wire could end a session.
            // Measured on a 72-h soak: 61 PCR-carrying packets
            // rewritten to 0x1FFE produced ~50 anomalies on a PID no PMT
            // ever declared.
            //
            // Nothing is retired here. The topology side drops a PID's entry
            // as soon as no program declares it
            // (`retire_undeclared_pcr_history`), so an undeclared PID never
            // has one.
            if !declared {
                return;
            }
            if self.pcr.take_new_timebase(pkt.pid) {
                // A signalled new time base: the jump is the splice, not an
                // anomaly, and the program's PTS history belongs to the old
                // clock.
                self.pcr.record(pkt.pid, now);
                self.forget_program_pts(pkt.pid);
                return;
            }
            if let Some(last) = self.pcr.last(pkt.pid) {
                let diff = pcr_diff_27mhz(now, last);
                if diff.abs() > PCR_ANOMALY_THRESHOLD {
                    // Dedicated PCR PIDs have no `lookup_stream` entry —
                    // resolve through the PMT's PCR_PID instead of
                    // dropping the issue.
                    let stream = self.pcr_stream_id(pkt.pid);
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::PcrAnomaly { delta: diff },
                    );
                }
            }
            self.pcr.record(pkt.pid, now);
        }
    }

    /// Forget the last PTS of every stream in every program clocked by
    /// `pcr_pid`, so the first PTS on the new time base is not judged against
    /// the old one. A PES already in flight started on the old clock: it
    /// keeps the old baseline, and the baseline is dropped once it ends
    /// (`settle_after_pes`).
    pub(super) fn forget_program_pts(&mut self, pcr_pid: u16) {
        let Self {
            programs,
            pid_state,
            pes,
            ..
        } = self;
        for stream in programs
            .values()
            .filter(|t| t.pcr_pid == Some(pcr_pid))
            .flat_map(|t| &t.streams)
        {
            if let Some(state) = pid_state.get_mut(&stream.pid) {
                // The run's PCR latch is a value on the PCR clock, not part
                // of the PTS baseline: it goes now, even with a PES in
                // flight, or that PES would be timed across two clocks.
                state.pts_less_run_pcr = None;
                state.pts_less_run_reported = false;
                if pes.has_pid(stream.pid) {
                    state.pts_baseline_clears_after_pes = true;
                } else {
                    state.clear_pts_baseline();
                }
            }
        }
    }

    /// The PES that straddled a signalled reset on `pid` has ended (emitted
    /// or dropped): drop the old-clock PTS baseline it was judged against,
    /// and the old source's AU cells in progress and LOAS record position
    /// (ISO/IEC 14496-3 §1.7.2), which that PES may have just advanced.
    pub(super) fn settle_after_pes(&mut self, pid: u16) {
        let Some(state) = self.pid_state.get_mut(&pid) else {
            return;
        };
        if core::mem::take(&mut state.pts_baseline_clears_after_pes) {
            state.clear_pts_baseline();
        }
        state.source_reset_in_pes = false;
        if core::mem::take(&mut state.source_reset_after_pes) {
            state.forget_latm_position();
            self.discard_au_in_progress(pid);
        }
    }

    /// `true` when `check_continuity` will suppress `pkt` as the one allowed
    /// duplicate of the previous packet.
    fn is_spec_duplicate(&self, pkt: &crate::mpegts::demux::ts::TsPacket<'_>) -> bool {
        pkt.pid != pid::NULL
            && pkt.has_payload
            && self
                .pid_state
                .get(&pkt.pid)
                .is_some_and(|s| is_identical_repeat(s, pkt) && !s.dup)
    }

    /// The continuity verdict for `pkt`, and the PES dispatch kind of its PID.
    ///
    /// `jumped` is `true` when a CC gap was observed AND not suppressed by
    /// `discontinuity_indicator`. The caller (`process_packet`) uses this
    /// signal to gate strict-mode PSI reassembly drops in `handle_psi`.
    ///
    /// `duplicate` is `true` when the packet is a spec-legal duplicate per
    /// H.222.0 §2.4.3.3 (same CC as the preceding payload packet, byte-
    /// identical apart from the PCR). A duplicate in the discontinuity state
    /// counts too: §2.4.3.5 lets its second copy be discarded, and identical
    /// bytes carry the same `discontinuity_indicator` as the original. The
    /// caller MUST suppress all payload routing for such packets — they
    /// carry no new data. A THIRD packet with
    /// the same CC on the same PID IS a discontinuity (the "only-two" rule is
    /// enforced via `PidState::dup`).
    ///
    /// `kind` is what `Demuxer::stream_kind` answers for the PID of a
    /// payload packet. The PID's state is looked up once here and the kind
    /// read from it, so routing the packet needs no second lookup.
    ///
    /// Side effect: clears `self.last_psi_cc_jump` at entry, sets it to
    /// `Some((expected, observed))` when a real jump fires. `handle_psi`
    /// drains it via `.take()` when emitting `PsiCcDiscontinuity`.
    pub(super) fn check_continuity(
        &mut self,
        pkt: &crate::mpegts::demux::ts::TsPacket<'_>,
    ) -> Continuity {
        self.last_psi_cc_jump = None;
        // Per ITU-T H.222.0 §2.4.3.3, the continuity_counter field on null PID
        // (0x1FFF) packets is undefined and MUST NOT be validated. Null packets
        // are >50% of bytes in CBR feeds, so tracking them would grow
        // `pid_state` with a sentinel entry that's never useful and could
        // spuriously fire ContinuityJump. PCR tracking is intentionally NOT
        // skipped — PCR may legitimately ride null packets per §2.4.3.5 — that
        // path lives in `check_pcr` and is keyed on `pcr_27mhz.is_some()`.
        if pkt.pid == pid::NULL {
            return Continuity {
                jumped: false,
                duplicate: false,
                kind: self.stream_kind(pkt.pid),
            };
        }
        // Adaptation-field-only packets (afc='10') do not carry a payload and
        // do not increment the continuity_counter (H.222.0 §2.4.3.3), so their
        // counter is never compared. Their discontinuity_indicator is still
        // honoured (§2.4.3.5): the baseline is forgotten so the next payload
        // packet seeds a new one.
        if !pkt.has_payload {
            if pkt.discontinuity_indicator {
                let state = self.pid_state_mut(pkt.pid);
                state.clear_continuity();
                state.source_reset_after_pes = true;
                state.source_reset_in_pes = true;
                if let Some(stream) = state.stream_id(pkt.pid) {
                    self.record_discontinuity(stream, DiscontinuityKind::AdaptationFieldFlag);
                }
            }
            return Continuity {
                jumped: false,
                duplicate: false,
                kind: None,
            };
        }
        // Every payload packet leaves its counter and bytes in the PID's
        // state, so the entry is created here if the PID is new. The events
        // are queued once the borrow of the state has ended.
        let state = self.pid_state.entry(pkt.pid).or_default();
        let kind = state.stream_kind;
        let mut jump = None;
        if let Some(prev_cc) = state.cc {
            let expected = (prev_cc + 1) & 0x0F;

            // Spec-legal duplicate: same CC AND byte-identical content (so the
            // same discontinuity_indicator as the original; in the
            // discontinuity state §2.4.3.5 permits discarding the copy).
            // H.222.0 §2.4.3.3 — "a packet may be sent exactly twice with the
            // same continuity_counter value; such a duplicate shall not cause
            // discontinuity", and "each byte of
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
            if is_identical_repeat(state, pkt) {
                if state.dup {
                    // Third (or later) identical packet with the same CC —
                    // beyond the spec's only-two allowance. Surface exactly
                    // ONE discontinuity for this packet and route its payload
                    // (the pre-duplicate-support behavior). `PidState::dup`
                    // stays SET and the stored original bytes stay in place,
                    // so EVERY further identical packet in the run lands here
                    // too — the only-two enforcement does not reset mid-run.
                    // Early return keeps the generic CC-jump block below from
                    // recording the same jump a second time. (the stored cc would
                    // be re-inserted with the identical value — skipping the
                    // tail is a no-op for it.)
                    // Lost bytes leave the LOAS record position unknown;
                    // `latm_unsynced_bytes` stays, a gap is more evidence of trouble.
                    state.forget_latm_position();
                    let stream = state.stream_id(pkt.pid);
                    self.record_continuity_jump(stream, expected, pkt.continuity_counter);
                    return Continuity {
                        jumped: true,
                        duplicate: false,
                        kind,
                    };
                }
                // First duplicate — suppress and mark.
                state.dup = true;
                // Do NOT update the stored cc / raw bytes — the next
                // non-duplicate packet on this PID must still expect
                // prev_cc + 1, and a second duplicate must compare against
                // the ORIGINAL packet's bytes.
                return Continuity {
                    jumped: false,
                    duplicate: true,
                    kind,
                };
            }
            // Normal advance, discontinuity_indicator, or a same-CC packet
            // with differing bytes (routed as an ordinary CC jump below) —
            // clear any pending duplicate state for this PID.
            state.dup = false;

            // Per ISO/IEC 13818-1 §2.4.3.5, when discontinuity_indicator=1
            // the CC is explicitly permitted to be discontinuous on this
            // packet. Suppress the ContinuityJump (matches ffmpeg
            // mpegts.c:3075-3078); the separate `AdaptationFieldFlag`
            // event below already surfaces the discontinuity hint to
            // consumers, so emitting both would double-count.
            if expected != pkt.continuity_counter && !pkt.discontinuity_indicator {
                jump = Some(expected);
                // Lost bytes leave the LOAS record position unknown;
                // `latm_unsynced_bytes` stays, a gap is more evidence of trouble.
                state.forget_latm_position();
            }
        } else {
            // First packet ever seen on this PID — no prev state, no duplicate
            // check possible. A duplicate flag cannot be set without a cc
            // (they clear together), so there is nothing to clear.
        }
        if pkt.discontinuity_indicator {
            state.source_reset_after_pes = true;
            state.source_reset_in_pes |= !pkt.payload_unit_start;
        }
        state.cc = Some(pkt.continuity_counter);
        state.last_pkt_raw = Some(*pkt.raw);
        // Only a packet that queues an event needs the stream's identity.
        if jump.is_some() || pkt.discontinuity_indicator {
            let stream = state.stream_id(pkt.pid);
            if let Some(expected) = jump {
                self.record_continuity_jump(stream, expected, pkt.continuity_counter);
            }
            if pkt.discontinuity_indicator
                && let Some(stream) = stream
            {
                self.record_discontinuity(stream, DiscontinuityKind::AdaptationFieldFlag);
            }
        }
        Continuity {
            jumped: jump.is_some(),
            duplicate: false,
            kind,
        }
    }

    /// A real continuity jump on the packet being routed: remembered for
    /// `handle_psi`, and reported on a PID a PMT declared.
    fn record_continuity_jump(&mut self, stream: Option<StreamId>, expected: u8, observed: u8) {
        self.last_psi_cc_jump = Some((expected, observed));
        if let Some(stream) = stream {
            self.record_discontinuity(
                stream,
                DiscontinuityKind::ContinuityJump { expected, observed },
            );
        }
    }
}

/// What `Demuxer::check_continuity` found for one packet.
pub(super) struct Continuity {
    /// A continuity gap that `discontinuity_indicator` did not announce.
    pub(super) jumped: bool,
    /// The one allowed duplicate of the previous packet: route nothing.
    pub(super) duplicate: bool,
    /// The PES dispatch kind a PMT declared for the PID of a payload packet.
    pub(super) kind: Option<StreamKind>,
}

/// `true` when `pkt` repeats the last payload packet of its PID, whose state
/// is `state`: same CC and identical bytes apart from the PCR (H.222.0
/// §2.4.3.3). The counter test comes first: it settles almost every packet
/// without the byte compare.
fn is_identical_repeat(state: &PidState, pkt: &crate::mpegts::demux::ts::TsPacket<'_>) -> bool {
    state.cc == Some(pkt.continuity_counter)
        && state
            .last_pkt_raw
            .as_ref()
            .is_some_and(|prev| pcr_masked_identical(prev, pkt.raw))
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
        // varying CC values — `pid_state` must remain empty.
        let mut demuxer = Demuxer::new();
        for i in 0..100u8 {
            demuxer
                .feed_aligned(&payload_packet(pid::NULL, i & 0x0F))
                .unwrap();
        }
        assert_eq!(
            demuxer.pid_state.len(),
            0,
            "null PID packets must not populate pid_state"
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
