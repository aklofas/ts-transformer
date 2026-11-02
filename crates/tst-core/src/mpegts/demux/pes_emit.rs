//! PES reassembly dispatch + complete-PES-to-DemuxEvent conversion.
//!
//! Hosts these helper methods on `Demuxer`:
//!
//! - `handle_pes_packet` — feeds a TS packet's PES bytes into the
//!   reassembler, surfaces oversize / total-cap discontinuities,
//!   converts complete PES results into events.
//! - `handle_complete_pes` — emits one completed PES, with its damage
//!   marker (if any) directly before its first `Sample`/`Metadata`.
//! - `emit_complete_pes` — the central event-construction site.
//!   Dispatches by `StreamKind`; constructs `DemuxEvent::Sample` /
//!   `DemuxEvent::Metadata` with codec-specific payload shapes
//!   (raw AU pass-through for video — parsing into NAL/OBU units is the
//!   consumer's opt-in `split_video` call; envelope strip for DVB-sub;
//!   AU cell peel for sync KLV; raw pass-through for audio / async KLV /
//!   unknown).
//!
//! `handle_pes_packet` and `handle_complete_pes` are `pub(super)`; the rest is private.

use crate::codec::{AudioCodec, VideoCodec};
use crate::mpegts::common::{Pts90khz, StreamTypeCode, pts_diff_33bit};
use crate::mpegts::demux::event::{
    DemuxEvent, DiscontinuityKind, MetadataKind, MultiCellAuReason, NonConformantIssue,
    SamplePayload, StreamId, StreamKind, SubtitleCodec,
};
use crate::mpegts::demux::payload::{
    DvbSubStripResult, KlvShape, classify_klv, iter_au_cells, strip_dvb_sub_envelope,
    teletext_data_field_ok,
};
use crate::mpegts::demux::pes::{PesPayload, ReassemblyOutcome, is_damage_reason};
use crate::mpegts::demux::pid_state::set_once;
use crate::mpegts::demux::strict::StrictMode;
use crate::mpegts::demux::timestamp::TimestampUnwrapper;
use crate::mpegts::stats::CodecKind;
use crate::shared::SharedBytes;
use alloc::string::ToString;
use alloc::vec::Vec;

/// H.222.0 §2.7.4: consecutive coded PTS of one audio or video stream are at
/// most 0.7 s apart.
const PTS_SPACING_LIMIT_90KHZ: i64 = 63_000;

/// H.222.0 §2.4.2.7: the T-STD delay of a byte through the decoder buffers
/// is at most 1 s for most elementary streams.
const DECODER_DELAY_1S_90KHZ: i64 = 90_000;

/// H.222.0 §2.4.2.7: ISO/IEC 14496 streams, HEVC and VVC are allowed up to
/// 10 s of T-STD delay.
const DECODER_DELAY_10S_90KHZ: i64 = 900_000;

/// How far the program's PCR may advance across a PTS-less run on a stream
/// of `kind` before the run is reported. The PCR at a PES's arrival trails
/// its PTS by the decoder delay H.222.0 §2.4.2.7 bounds, so a run whose
/// coded PTS keep the §2.7.4 0.7 s spacing can span 0.7 s plus that delay
/// of PCR. The delay is 10 s for ISO/IEC 14496 streams (H.264 is 14496-10,
/// AAC-LATM is 14496-3), HEVC and VVC, and 1 s for everything else,
/// including ADTS AAC on `stream_type` 0x0F, which is ISO/IEC 13818-7. AV1
/// has no H.222.0 clause; it takes the lenient 10 s bound, because this is
/// a diagnostic and AV1 buffering models follow the 14496 shape.
fn pcr_run_limit_90khz(kind: &StreamKind) -> i64 {
    let delay = match kind {
        StreamKind::Video(
            VideoCodec::H264 | VideoCodec::H265 | VideoCodec::H266 | VideoCodec::Av1,
        )
        | StreamKind::Audio(AudioCodec::AacLatm) => DECODER_DELAY_10S_90KHZ,
        _ => DECODER_DELAY_1S_90KHZ,
    };
    PTS_SPACING_LIMIT_90KHZ + delay
}

/// `MalformedPes` reason for a DVB subtitle PES whose PES_data_field
/// envelope does not parse.
pub(crate) const DVB_SUB_ENVELOPE_MALFORMED: &str = "DVB subtitle PES_data_field envelope malformed (EN 300 743 Table 3: data_identifier 0x20, subtitle_stream_id 0x00, end marker 0xFF)";

/// `MalformedPes` reason for a teletext PES whose size or PES_data_field
/// breaks EN 300 472.
pub(crate) const TELETEXT_DATA_FIELD_MALFORMED: &str =
    "teletext PES_data_field malformed (EN 300 472 §4.2/§4.3)";

/// Which PTS rule a PES on a stream is judged by.
enum PtsRule {
    /// Every PES must carry a PTS.
    PerPes,
    /// PES may omit the PTS, but the coded PTS around a PTS-less run must
    /// stay within [`PTS_SPACING_LIMIT_90KHZ`]; until a coded PTS closes the
    /// run, the program's PCR may advance at most [`pcr_run_limit_90khz`]
    /// across it: 0.7 s plus the codec's decoder delay of H.222.0 §2.4.2.7
    /// (1 s, or 10 s for ISO/IEC 14496 streams, HEVC, VVC and AV1).
    Spacing,
    /// No PTS requirement.
    None,
}

/// The PTS rule for a stream of `kind` declared with PMT `stream_type`.
///
/// - Audio and video: H.222.0 §2.7.4 only bounds the spacing of coded PTS
///   (0.7 s); a PES without one is fine as long as that bound holds. A run
///   of PTS-less PES is judged by the coded PTS that closes it, and is also
///   timed by the program's PCR from its first PES, so a run that never
///   closes is caught once the PCR advances more than 0.7 s plus the
///   codec's decoder delay of §2.4.2.7 (see [`pcr_run_limit_90khz`]). A gap between
///   coded PTS with no PTS-less PES inside it is not diagnosed: the
///   still-picture and low-frame-rate exceptions of §2.7.4 cannot be told
///   apart from such a gap.
/// - DVB subtitles: EN 300 743 requires a PTS on every subtitle PES.
/// - KLV on `stream_type` 0x15 (synchronous metadata): ST 1402-07 requires a
///   PTS on every PES. KLV on any other stream type is asynchronous and
///   carries no timing contract.
fn pts_rule(kind: &StreamKind, stream_type: u8) -> PtsRule {
    match kind {
        StreamKind::Video(_) | StreamKind::Audio(_) => PtsRule::Spacing,
        StreamKind::Subtitle(SubtitleCodec::DvbSubtitling) => PtsRule::PerPes,
        StreamKind::KlvSync { .. } | StreamKind::KlvAsync if stream_type == 0x15 => PtsRule::PerPes,
        _ => PtsRule::None,
    }
}

/// Tolerance-mode validator for orphan Middle/Last AU cells.
///
/// Returns `true` when `payload` looks like a single complete KLV record:
/// SMPTE 336M Universal Label prefix (`06 0e 2b 34`), followed by a 12-byte
/// UL completion, followed by a BER length that describes exactly the
/// remainder of the payload. Stricter than just "starts with UL" because
/// a real fragment would not have a self-consistent BER length.
///
/// This is the gate behind
/// [`DemuxerConfig::cfi_tolerance`](crate::mpegts::demux::DemuxerConfig::cfi_tolerance) —
/// the demuxer reframes an orphan cell as Complete only if this returns
/// `true`. A real loss / mid-stream-join / fragment fails the BER check
/// and falls through to the existing
/// [`NonConformantIssue::MultiCellAu`] `{ reason = Orphan }` path.
fn orphan_validates_as_complete_klv(payload: &[u8]) -> bool {
    // SMPTE 336M Universal Label is 16 bytes; first 4 are the registered
    // prefix `06 0e 2b 34` (per SMPTE 298M Registered SMPTE Universal Label).
    // Anything else is not a recognizable KLV record.
    if payload.len() < 17 || &payload[0..4] != b"\x06\x0e\x2b\x34" {
        return false;
    }
    // BER length lives immediately after the 16-byte UL. The decoded
    // length must equal the available payload bytes after the length
    // field — i.e. the BER length describes exactly the value portion
    // and there is no trailing junk that would suggest a fragment.
    match crate::klv::length::read_ber(&payload[16..]) {
        Ok((declared_len, rest)) => declared_len == rest.len(),
        Err(_) => false,
    }
}

impl super::demuxer::Demuxer {
    pub(super) fn handle_pes_packet(
        &mut self,
        pkt: &crate::mpegts::demux::ts::TsPacket<'_>,
    ) -> Result<(), crate::error::DemuxError> {
        // Zero PES_packet_length is legal only for video. Key the
        // decision on the PMT-declared stream type, not the PES stream_id — the
        // PID's kind is always known here (process_packet only routes to
        // handle_pes_packet when a stream kind is declared for pkt.pid). A PID
        // tst-core recognizes as video is `StreamKind::Video(_)`; a video
        // codec it does NOT parse (MPEG-1/2/4) is `StreamKind::Unknown(t)` with
        // a video `stream_type` `t` — both are video elementary streams per
        // H.222.0 §2.4.3.7 and may carry an unbounded PES, so neither is
        // flagged. Everything else (audio/KLV/subtitle/non-video Unknown) is.
        let is_video = match self.stream_kind(pkt.pid) {
            Some(StreamKind::Video(_)) => true,
            Some(StreamKind::Unknown(stream_type)) => {
                super::pmt_classify::is_video_stream_type(stream_type)
            }
            _ => false,
        };
        // A PUSI with no PES in flight means the PES that straddled a
        // time-base reset was already dropped (cap, damage): settle before
        // the push, or a PES that starts and ends in this packet is judged
        // against the old clock.
        if pkt.payload_unit_start && !self.pes.has_pid(pkt.pid) {
            self.settle_after_pes(pkt.pid);
        }
        // A PES in flight shorter than a PES header ends here with no
        // outcome and no cell: an AU in progress cannot be known to continue
        // across it.
        if pkt.payload_unit_start
            && self.pes.has_pid(pkt.pid)
            && !self.pes.in_flight_holds_header(pkt.pid)
        {
            self.discard_au_in_progress(pkt.pid);
        }
        let (outcomes, old_count) = self.pes.push_split(
            pkt.pid,
            pkt.payload,
            pkt.payload_unit_start,
            pkt.random_access_indicator,
            is_video,
        );
        // A damage `Malformed` is always the outcome right before the
        // `Complete` it describes. Lenient mode carries the reason into that
        // PES's emission; a strict mode applies its verdict now and drops
        // the PES's events (its timing still counts).
        // A PUSI ends whatever PES was in flight on the PID — completed,
        // dropped (cap, damage), unparseable or too short to parse — so a
        // reset that waited on it settles once that PES's outcomes are done:
        // before the first outcome of the PES that starts in this packet, or
        // after the loop when there is none.
        let settles = pkt.payload_unit_start;
        let mut drop_next_complete: Option<u16> = None;
        let mut damage_next_complete: Option<(u16, &'static str)> = None;
        for (idx, outcome) in outcomes.into_iter().enumerate() {
            if settles && idx == old_count {
                self.settle_after_pes(pkt.pid);
            }
            match outcome {
                ReassemblyOutcome::Complete(pes) => {
                    if drop_next_complete == Some(pes.pid) {
                        drop_next_complete = None;
                        self.drop_damaged_pes(pes);
                        continue;
                    }
                    let damage = damage_next_complete
                        .take_if(|(pid, _)| *pid == pes.pid)
                        .map(|(_, reason)| reason);
                    let gap = damage.and_then(|_| self.pes.take_gap(pes.pid));
                    self.handle_complete_pes(pes, damage, gap);
                }
                ReassemblyOutcome::Overflow { pid } => {
                    // A cap drop loses bytes without a continuity gap, so the
                    // LOAS record position is unknown (ISO/IEC 14496-3
                    // §1.7.2); `latm_unsynced_bytes` stays, as for the other losses.
                    if let Some(state) = self.pid_state.get_mut(&pid) {
                        state.forget_latm_position();
                    }
                    // The dropped PES may hold a cell of the AU in progress.
                    self.discard_au_in_progress(pid);
                    if let Some(stream) = self.lookup_stream(pid) {
                        self.record_discontinuity(stream, DiscontinuityKind::PesOversize { pid });
                    }
                }
                ReassemblyOutcome::OverflowTotal => {
                    // The aggregate cap drops the partials of every PID, so
                    // each of them lost a PES.
                    for pid in self.pes.take_total_overflow_pids() {
                        // As for `Overflow`: the record position is unknown.
                        if let Some(state) = self.pid_state.get_mut(&pid) {
                            state.forget_latm_position();
                        }
                        self.discard_au_in_progress(pid);
                        if let Some(stream) = self.lookup_stream(pid) {
                            self.record_discontinuity(stream, DiscontinuityKind::PesTotalOversize);
                        }
                    }
                }
                ReassemblyOutcome::ZeroLengthNonVideo { pid, stream_id } => {
                    let stream = self.lookup_stream(pid).unwrap_or_else(|| {
                        StreamId::anonymous(pid, self.program_number_for_pid(pid))
                    });
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::ZeroLengthPesNonVideo { pid, stream_id },
                    );
                    // The partial is dropped with its cells.
                    self.discard_au_in_progress(pid);
                }
                ReassemblyOutcome::Malformed { pid, reason }
                    if is_damage_reason(reason) && self.options.strict == StrictMode::Off =>
                {
                    damage_next_complete = Some((pid, reason));
                }
                ReassemblyOutcome::Malformed { pid, reason } => {
                    // Wire order with the completions beside it: a lenient
                    // demuxer queues the NonConformant and keeps going; a
                    // strict one returns the error here and the remaining
                    // outcomes of this packet go with it (strict is fatal
                    // for the stream anyway).
                    if let Err(err) = self.surface_malformed_pes(pid, reason) {
                        // The same rule on the early return: the PUSI still
                        // ends the PES in flight, so settle a pending
                        // time-base reset first and a caller that keeps
                        // feeding is judged on the new clock. The PES is
                        // never walked, so the AU in progress goes with it.
                        if settles {
                            self.settle_after_pes(pkt.pid);
                        }
                        self.discard_au_in_progress(pid);
                        return Err(err);
                    }
                    if is_damage_reason(reason) {
                        drop_next_complete = Some(pid);
                    } else {
                        // Unparseable: no `Complete` follows, so the PES and
                        // any cell of the AU in progress it carried are lost.
                        self.discard_au_in_progress(pid);
                    }
                }
            }
        }
        if settles {
            self.settle_after_pes(pkt.pid);
        }
        Ok(())
    }

    /// Emit the events of one completed PES. `damage` (lenient mode only)
    /// is the reason its payload is missing bytes: the `MalformedPes` marker
    /// is queued directly before the PES's first `Sample`/`Metadata`, after
    /// any other issue the PES raised, or on its own if the PES yields no
    /// event (a damaged synchronous-KLV PES whose AU cells only buffer gets
    /// a standalone marker). `gap` is the payload offset where a continuity
    /// gap lost bytes, if one did.
    pub(super) fn handle_complete_pes(
        &mut self,
        pes: PesPayload,
        damage: Option<&'static str>,
        gap: Option<usize>,
    ) {
        let pid = pes.pid;
        let damaged = damage.is_some();
        let mut damage = damage;
        self.emit_complete_pes(pes, &mut damage, gap);
        // Judged on its own clock above; a pending reset applies from the
        // next PES on.
        self.settle_after_pes(pid);
        if let Some(reason) = damage {
            let stream = self
                .lookup_stream(pid)
                .unwrap_or_else(|| StreamId::anonymous(pid, self.program_number_for_pid(pid)));
            self.queue_nonconformant(stream, NonConformantIssue::MalformedPes { pid, reason });
        }
        // The PES lost bytes, so a cell of the AU in progress may be among
        // them: the walk above may have buffered a fragment that must not
        // join the next PES's cells.
        if damaged {
            self.discard_au_in_progress(pid);
        }
    }

    /// Packets of `pid` were lost or a new source began: discard every AU
    /// cell reassembly in progress on the PID and, when one was held, report
    /// the loss as `MultiCellAu { SequenceGap }` (H.222.0 §2.12.4.2).
    pub(super) fn discard_au_in_progress(&mut self, pid: u16) {
        if let Some(dropped_bytes) = self.au_reassembler.discard_pid(pid) {
            let stream = self
                .lookup_stream(pid)
                .unwrap_or_else(|| StreamId::anonymous(pid, self.program_number_for_pid(pid)));
            self.queue_nonconformant(
                stream,
                NonConformantIssue::MultiCellAu {
                    pid,
                    dropped_bytes,
                    reason: MultiCellAuReason::SequenceGap,
                },
            );
        }
    }

    /// Queue a PES's `Sample`/`Metadata`, preceded by its damage marker if
    /// one is still pending.
    fn push_pes_event(&mut self, damage: &mut Option<&'static str>, event: DemuxEvent) {
        if let Some(reason) = damage.take()
            && let DemuxEvent::Sample { stream, .. } | DemuxEvent::Metadata { stream, .. } = &event
        {
            let stream = *stream;
            self.queue_nonconformant(
                stream,
                NonConformantIssue::MalformedPes {
                    pid: stream.pid,
                    reason,
                },
            );
        }
        self.queue.push_back(event);
    }

    /// The timing step of one completed PES: the PTS checks (backward step,
    /// spacing, per-PES rules, the synchronous-metadata DTS) and the
    /// `last_pts` and unwrap baselines. Returns the `(pts, dts)` the PES's
    /// events carry. The caller settles a pending time-base reset after it.
    fn record_pes_timing(
        &mut self,
        pes: &PesPayload,
        stream: StreamId,
    ) -> (Pts90khz, Option<Pts90khz>) {
        // PTS distinct from PCR. Only update `last_pts` when an actual PTS
        // arrived; never write 0 as a fallback (that corrupts the
        // monotonicity check for streams that omit PTS sporadically).
        let pts = pes.pts.unwrap_or(Pts90khz::new(0));
        // The stream_type byte comes from the PMT: KLV's PTS contract hangs
        // on the declared type (0x15), not on the payload shape.
        let stream_type = self
            .pid_state
            .get(&pes.pid)
            .and_then(|s| s.stream_type)
            .unwrap_or(0);
        match (pts_rule(&stream.kind, stream_type), pes.pts) {
            (PtsRule::PerPes, None) => self.queue_nonconformant(
                stream,
                NonConformantIssue::MissingRequiredPts { pid: pes.pid },
            ),
            (PtsRule::Spacing, None) => {
                // H.222.0 §2.7.4 bounds the interval between coded PTS. A
                // run that never ends in a coded PTS (a stream that never
                // stamps one, or stops stamping) breaks that bound, and the
                // PCR is the only clock left to measure it: the run is timed
                // from the PCR seen at its first PES, against a bound that
                // allows for the decoder-buffer delay.
                let pcr_now = self.program_pcr_90khz(stream.program_number);
                let state = self.pid_state_mut(pes.pid);
                state.pts_less_since_pts = true;
                let overdue = match (state.pts_less_run_pcr, pcr_now) {
                    _ if state.pts_less_run_reported => false,
                    (None, now) => {
                        state.pts_less_run_pcr = now;
                        false
                    }
                    (Some(start), Some(now)) => {
                        pts_diff_33bit(now, start) > pcr_run_limit_90khz(&stream.kind)
                    }
                    (Some(_), None) => false,
                };
                if overdue {
                    state.pts_less_run_pcr = None;
                    state.pts_less_run_reported = true;
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::MissingRequiredPts { pid: pes.pid },
                    );
                }
            }
            (PtsRule::Spacing, Some(now)) => {
                // A PTS-less run is closed by this PTS: judge the spacing of
                // the coded PTS around it once, before this PES's sample,
                // unless the PCR timer already reported the run.
                let state = self.pid_state_mut(pes.pid);
                let closes_run = core::mem::take(&mut state.pts_less_since_pts);
                let reported = core::mem::take(&mut state.pts_less_run_reported);
                state.pts_less_run_pcr = None;
                if closes_run
                    && !reported
                    && let Some(last) = state.last_pts
                    && pts_diff_33bit(now.as_ticks() as u64, last as u64) > PTS_SPACING_LIMIT_90KHZ
                {
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::MissingRequiredPts { pid: pes.pid },
                    );
                }
            }
            _ => {}
        }
        // ST 1402-08: synchronous metadata PES carry a PTS only
        // (PTS_DTS_flags = '10'). Reported once per PID.
        if matches!(
            stream.kind,
            StreamKind::KlvSync { .. } | StreamKind::KlvAsync
        ) && stream_type == 0x15
            && pes.dts.is_some()
            && set_once(&mut self.pid_state_mut(pes.pid).sync_pes_shape_emitted)
        {
            self.queue_nonconformant(
                stream,
                NonConformantIssue::Other(
                    "sync metadata PES carries a DTS; ST 1402-08 requires PTS_DTS_flags = '10'"
                        .to_string(),
                ),
            );
        }
        if let Some(observed_pts) = pes.pts {
            if let Some(last) = self.pid_state.get(&pes.pid).and_then(|s| s.last_pts) {
                let delta = pts_diff_33bit(observed_pts.as_ticks() as u64, last as u64);
                if delta < -90_000 {
                    // PTS anomaly is its own variant (90 kHz / per-PID
                    // elementary stream), distinct from PcrAnomaly
                    // (27 MHz / per-program PCR PID).
                    self.queue_nonconformant(stream, NonConformantIssue::PtsAnomaly { delta });
                }
            }
            self.pid_state_mut(pes.pid).last_pts = Some(observed_pts.as_ticks());
        }
        // Opt-in PTS/DTS unwrap (`DemuxerConfig::unwrap_timestamps`,
        // default off — the branch below is skipped entirely and `pts`/`dts`
        // stay exactly as computed above, so the default emit path is
        // byte-for-byte unchanged). Every event of the PES (`Sample` for
        // every `StreamKind`, and every `Metadata` KLV shape) carries the
        // pair returned here. A synthesized `pts = 0` for a PES without a
        // PTS is never fed to the accumulator and is left as 0.
        if self.options.unwrap_timestamps {
            let unwrapped_pts = match pes.pts {
                Some(raw) => self.unwrap_pts(pes.pid, raw),
                None => pts,
            };
            // DTS unwraps relative to THIS PES's own raw PTS, not the
            // bare per-PID offset — the 33-bit boundary can fall between
            // one AU's DTS and PTS (DTS <= PTS always), and applying the
            // just-advanced offset directly would put a pre-wrap DTS a
            // full epoch too high. See `TimestampUnwrapper::dts_with_pts`. The
            // DTS-with-no-PTS branch is spec-illegal but tolerated
            // defensively via the older bare-offset fallback.
            let unwrapped_dts = match (pes.pts, pes.dts) {
                (Some(pts_raw), Some(dts_raw)) => Some(TimestampUnwrapper::dts_with_pts(
                    unwrapped_pts,
                    pts_raw,
                    dts_raw,
                )),
                (None, Some(dts_raw)) => Some(TimestampUnwrapper::secondary(
                    self.pid_state.get(&pes.pid).and_then(|s| s.unwrap),
                    dts_raw,
                )),
                (_, None) => None,
            };
            (unwrapped_pts, unwrapped_dts)
        } else {
            (pts, pes.dts)
        }
    }

    /// The last PCR of `program`'s PCR_PID as 33-bit 90 kHz ticks (the PCR
    /// base, H.222.0 §2.4.3.5); `None` before its first PCR.
    fn program_pcr_90khz(&self, program: u16) -> Option<u64> {
        let pcr_pid = self.programs.get(&program)?.pcr_pid?;
        self.pcr.last(pcr_pid).map(|pcr| pcr / 300)
    }

    /// A damaged PES a strict mode drops from delivery. Damage loses payload
    /// bytes; the header and its PTS arrived, so the stream's timeline
    /// includes the PES. It takes the same timing step as a delivered PES,
    /// and only its events are skipped.
    pub(super) fn drop_damaged_pes(&mut self, pes: PesPayload) {
        if let Some(stream) = self.lookup_stream(pes.pid) {
            // No events to stamp: the returned timestamps go unused.
            let _ = self.record_pes_timing(&pes, stream);
        }
        // Its payload is not walked, so the LOAS record position is lost.
        // `latm_unsynced_bytes` stays: lost bytes are more evidence of trouble.
        if let Some(state) = self.pid_state.get_mut(&pes.pid) {
            state.forget_latm_position();
        }
        self.settle_after_pes(pes.pid);
        // Its cells are not walked, and the bytes it lost may hold a cell of
        // the AU in progress.
        self.discard_au_in_progress(pes.pid);
    }

    /// Walk the LOAS records of an AAC-LATM PES and update the PID's carry
    /// (ISO/IEC 14496-3 §1.7.2: a record may continue into the next PES).
    /// Returns `true` when strict mode rejects a framing issue it raised.
    ///
    /// - Carry known (`Some(n)`): a PES of at most `n` bytes is still inside
    ///   the record; a longer one is walked from `n`, and a missing syncword
    ///   is flagged at once. A header split across PES is completed from the
    ///   next PES first.
    /// - Carry unknown, PES aligned (`data_alignment_indicator`, an explicit
    ///   promise of a record start) or starting with a syncword: walked from
    ///   0, a missing syncword flagged at once.
    /// - Carry unknown otherwise (after a gap, or a PID's first PES with
    ///   `data_alignment_indicator` = 0): the PES may open inside a record,
    ///   so it resyncs on its first syncword candidate (`resync_loas`):
    ///   - Confirmed (a whole record follows it inside the PES): the walk's
    ///     carry is adopted.
    ///   - Unconfirmed (its record, or its header, runs past the PES): the
    ///     carry is held tentatively. The next PES confirms it when a
    ///     syncword (or a lone first header byte) sits where the record
    ///     ends, and the carry is then enforced like any other; otherwise
    ///     the candidate was false and is dropped without an issue, and that
    ///     PES is resynced as one whose position is unknown.
    ///   - No candidate: the payload counts towards the unsynced bytes. A
    ///     LOAS stream starts a record within every `MAX_LOAS_RECORD_BYTES`
    ///     (`3 + 0x1FFF`) window and a record start is always a candidate,
    ///     so more than that many consecutive candidate-free bytes cannot be
    ///     LOAS: `MissingSyncword` is raised once when the count first
    ///     exceeds it, and not again until a confirmed walk resets it. A
    ///     count of PES would not do: several small PES fit inside one
    ///     record.
    /// - A damaged PES lost bytes, so it is not walked and the carry is lost.
    fn check_latm_framing(&mut self, pes: &PesPayload, damaged: bool, stream: StreamId) -> bool {
        use crate::codec::aac::latm::{
            LatmFramingKind, LoasCarry, LoasResync, MAX_LOAS_RECORD_BYTES, opens_loas_header,
            resync_loas, starts_with_loas_sync, walk_loas,
        };
        let state = self.pid_state_mut(pes.pid);
        let len = pes.payload.len();
        let tentative = core::mem::take(&mut state.latm_carry_tentative);
        // The record boundary this PES is walked from, when the carry
        // knows it.
        let known = match state.latm_carry.take() {
            _ if damaged => return false,
            Some(carry) => match carry.resolve(&pes.payload) {
                Ok(LoasCarry::Body(n)) if len <= n => {
                    state.latm_carry = Some(LoasCarry::Body(n - len));
                    state.latm_carry_tentative = tentative;
                    return false;
                }
                // The candidate's record ends here without a syncword, or
                // its header completes without one: it was false. Dropped
                // silently, and this PES is resynced like one whose
                // position is unknown.
                Ok(LoasCarry::Body(n)) if tentative && !opens_loas_header(&pes.payload[n..]) => {
                    None
                }
                Ok(LoasCarry::Body(n)) => Some(n),
                Ok(header) => {
                    state.latm_carry = Some(header);
                    state.latm_carry_tentative = tentative;
                    return false;
                }
                Err(_) if tentative => None,
                Err(kind) => return self.raise_latm_framing(pes.pid, kind, stream),
            },
            None => None,
        };
        let start = match known {
            Some(n) => n,
            None if pes.data_alignment_indicator || starts_with_loas_sync(&pes.payload) => 0,
            None => match resync_loas(&pes.payload) {
                LoasResync::Confirmed(carry) => {
                    state.latm_carry = carry;
                    state.latm_unsynced_bytes = 0;
                    return false;
                }
                LoasResync::Unconfirmed(carry) => {
                    state.latm_carry = Some(carry);
                    state.latm_carry_tentative = true;
                    return false;
                }
                LoasResync::NoCandidate => {
                    let before = state.latm_unsynced_bytes;
                    state.latm_unsynced_bytes = before.saturating_add(len);
                    if before > MAX_LOAS_RECORD_BYTES
                        || state.latm_unsynced_bytes <= MAX_LOAS_RECORD_BYTES
                    {
                        return false;
                    }
                    return self.raise_latm_framing(
                        pes.pid,
                        LatmFramingKind::MissingSyncword,
                        stream,
                    );
                }
            },
        };
        match walk_loas(&pes.payload, start) {
            Ok(carry) => {
                state.latm_carry = carry;
                state.latm_unsynced_bytes = 0;
                false
            }
            Err(kind) => self.raise_latm_framing(pes.pid, kind, stream),
        }
    }

    /// Queue a `LatmFraming` issue; `true` when strict mode rejects it.
    fn raise_latm_framing(
        &mut self,
        pid: u16,
        kind: crate::codec::aac::latm::LatmFramingKind,
        stream: StreamId,
    ) -> bool {
        let issue = NonConformantIssue::LatmFraming { pid, kind };
        let reject = self.options.strict.rejects(&issue);
        self.queue_nonconformant(stream, issue);
        reject
    }

    fn emit_complete_pes(
        &mut self,
        pes: PesPayload,
        damage: &mut Option<&'static str>,
        gap: Option<usize>,
    ) {
        let kind = match self.stream_kind(pes.pid) {
            Some(k) => k,
            None => return,
        };
        let program_number = self.program_number_for_pid(pes.pid);
        let stream = StreamId {
            pid: pes.pid,
            kind,
            program_number,
        };
        // Surface PES header structural issues collected during
        // parse_complete. These travel through the strict-mode cascade
        // like any other NonConformantIssue. We process them BEFORE the
        // PTS / DTS dispatch so consumers see the issue alongside the
        // (possibly-truncated) sample event.
        for kind_violation in &pes.header_issues {
            self.queue_nonconformant(
                stream,
                NonConformantIssue::PesHeaderMalformed {
                    pid: pes.pid,
                    kind: *kind_violation,
                },
            );
        }
        let (pts, dts) = self.record_pes_timing(&pes, stream);
        match kind {
            StreamKind::Video(codec) => {
                // Raw-first: the demuxer no longer parses the video elementary
                // stream. It emits the encoded access unit verbatim; splitting
                // NAL/OBU units is the consumer's opt-in call via
                // `split_video`. ES-content conformance (NAL/OBU header bits,
                // OBU framing) moved off the demuxer's StrictMode and onto
                // `split_video_strict`.
                let rai = pes.random_access_indicator;

                // PES-layer (not ES-content) AV1 binding check: §3.4 mandates
                // PES `stream_id = 0xBD` (private_stream_1) in binding mode.
                // This inspects the PES/TS-layer stream_id, is independent of
                // OBU splitting, and stays at the demux layer. In `InteropRawObu`
                // mode the demuxer tolerates `stream_id = 0xE0` silently.
                if codec == VideoCodec::Av1 {
                    use crate::mpegts::common::Av1CarriageMode;
                    let binding_mode =
                        matches!(self.options.av1_carriage, Av1CarriageMode::Mpeg2TsBinding);
                    if binding_mode && pes.stream_id != 0xBD {
                        self.queue_nonconformant(
                            stream,
                            NonConformantIssue::Av1WrongStreamId {
                                pid: stream.pid,
                                observed: pes.stream_id,
                            },
                        );
                    }
                }

                // Wrap the AU once and emit it. One Arc allocation + one copy
                // of the bytes from the PES payload buffer into the Arc.
                let raw = SharedBytes::from_slice(&pes.payload);
                let raw_len = raw.len();

                self.record_item(&stream, program_number, raw_len);
                // `nals_or_obus` is no longer counted here (it required the
                // split the demuxer no longer performs). The `random_access_aus`
                // counter still increments from the PES_start RAI bit.
                let ra_count = if rai { 1 } else { 0 };
                if ra_count > 0 {
                    self.pid_state_mut(stream.pid)
                        .codec_mut(CodecKind::Video)
                        .add_video(0, ra_count);
                }
                self.push_pes_event(
                    damage,
                    DemuxEvent::Sample {
                        stream,
                        pts,
                        dts,
                        payload: SamplePayload::Video {
                            codec,
                            raw,
                            random_access_indicator: rai,
                            av1_carriage: if codec == VideoCodec::Av1 {
                                Some(self.options.av1_carriage)
                            } else {
                                None
                            },
                        },
                    },
                );
            }
            StreamKind::KlvSync { .. } | StreamKind::KlvAsync => {
                let shape = classify_klv(&pes.payload);

                // A PES not walked as AU cells (async or other shape) may
                // have been the next part of an AU in progress on a sync PID:
                // that AU cannot continue across it.
                if !matches!(shape, KlvShape::Sync) {
                    self.discard_au_in_progress(pes.pid);
                }

                // Async-shape PIDs (bare SMPTE UL) — bypass the AU cell
                // reassembler entirely. One emit per PES.
                if let KlvShape::Async { klv } = shape {
                    if matches!(kind, StreamKind::KlvSync { .. })
                        && self.klv_mismatch_insert(pes.pid)
                    {
                        self.queue_nonconformant(
                            stream,
                            NonConformantIssue::StreamTypeMismatchAsyncOnSyncPid,
                        );
                    }
                    let meta_len = klv.len();
                    self.record_item(&stream, program_number, meta_len);
                    self.pid_state_mut(stream.pid)
                        .codec_mut(CodecKind::Klv)
                        .add_klv(1);
                    self.push_pes_event(
                        damage,
                        DemuxEvent::Metadata {
                            stream,
                            pts,
                            kind: MetadataKind::KlvAsync,
                            payload: klv,
                        },
                    );
                    return;
                }

                // Other-shape PIDs (neither AU cell nor SMPTE UL) — pass
                // through as raw Unknown samples for forensic visibility.
                if matches!(shape, KlvShape::Other) {
                    let payload_len = pes.payload.len();
                    let raw = pes.payload;
                    self.record_item(&stream, program_number, payload_len);
                    self.push_pes_event(
                        damage,
                        DemuxEvent::Sample {
                            stream,
                            pts,
                            dts,
                            payload: SamplePayload::Unknown {
                                stream_type: StreamTypeCode::from_byte(
                                    self.pid_state
                                        .get(&pes.pid)
                                        .and_then(|s| s.stream_type)
                                        .unwrap_or(0x15),
                                ),
                                raw: SharedBytes::from_slice(&raw),
                            },
                        },
                    );
                    return;
                }

                // Sync shape — walk every cell in this PES, feed each into
                // the per-PID reassembler, emit Metadata on Emit and
                // NonConformant on Failure. Multi-cell AUs (First → 0..n
                // Middle → Last) collapse into one Metadata event with
                // `was_reassembled=true, cell_count=N`. Single-cell
                // (Complete) AUs emit immediately with cell_count=1.
                if matches!(kind, StreamKind::KlvAsync) && self.klv_mismatch_insert(pes.pid) {
                    self.queue_nonconformant(
                        stream,
                        NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid,
                    );
                }

                // Collect cells eagerly into a Vec so the iterator's borrow
                // on `pes.payload` is released before we re-borrow self
                // mutably to call process_cell + queue.push_back. Each
                // `inner` slice still borrows from pes.payload; ownership
                // of the outer Vec doesn't change that.
                // H.222.0 Table 2-156: sequence_number counts the cells of
                // one metadata_AU_wrapper, so each PES starts a new count.
                self.au_reassembler.begin_wrapper(pes.pid);
                let cells: Vec<_> = iter_au_cells(&pes.payload).collect();
                let base = pes.payload.as_ptr().addr();
                let mut gap = gap;
                for cell_result in cells {
                    let (header, inner) = match cell_result {
                        Ok(pair) => pair,
                        Err(_) => {
                            // Truncated / malformed trailing cell. Stop
                            // walking — earlier cells in this PES already
                            // emitted or buffered; the partial tail is
                            // dropped, and with it any AU still in progress.
                            self.discard_au_in_progress(pes.pid);
                            break;
                        }
                    };
                    // A cell ending past the continuity gap follows the lost
                    // bytes, which may have held a cell of the AU in
                    // progress; the wrapper's first cell sets the sequence,
                    // so the gap would go unseen. Cells wholly before the
                    // gap arrived intact and still complete the AU.
                    if gap.is_some_and(|g| inner.as_ptr().addr() - base + inner.len() > g) {
                        gap = None;
                        self.discard_au_in_progress(pes.pid);
                    }

                    // Replay loop: process_cell may report ConcurrentFirst
                    // (a new First/Complete arrived while a buffer was
                    // already open for this PID and service) or a
                    // SequenceGap on a First/Complete cell (a lost cell
                    // dropped every buffer on the PID). The reassembler
                    // dropped the buffers and emitted Failure; we surface
                    // the NonConformant and re-process the triggering cell
                    // against the now-empty state. Bounded to one re-entry
                    // per cell — the replayed cell repeats the PID's last
                    // sequence number (no gap) and finds its key empty, so
                    // the second call hits the empty-state row of the table
                    // (Buffered or Emit).
                    let mut to_process = Some((header, inner));
                    while let Some((h, current_inner)) = to_process.take() {
                        let outcome = self.au_reassembler.process_cell(pes.pid, h, current_inner);
                        match outcome {
                            crate::mpegts::demux::au_reassemble::ReassembleOutcome::Emit {
                                header: emit_header,
                                payload: emit_payload,
                                cell_count,
                            } => {
                                let was_reassembled = cell_count > 1;
                                let payload_vec = emit_payload.to_vec();
                                // Drain the buffer (drops the borrow on
                                // emit_payload). Safe to no-op for the
                                // single-cell case since the buffer is
                                // never populated then.
                                if was_reassembled {
                                    self.au_reassembler
                                        .clear_after_emit(pes.pid, emit_header.metadata_service_id);
                                }
                                let meta_len = payload_vec.len();
                                self.record_item(&stream, program_number, meta_len);
                                self.pid_state_mut(stream.pid)
                                    .codec_mut(CodecKind::Klv)
                                    .add_klv(1);
                                self.push_pes_event(damage, DemuxEvent::Metadata {
                                    stream,
                                    // PES PTS reused for every AU emitted
                                    // from this PES: H.222.0 §2.12.4.2
                                    // (ST 1402-10) requires one PTS for
                                    // all AUs in the PES.
                                    pts,
                                    kind: MetadataKind::KlvSyncAuCell {
                                        metadata_service_id: emit_header.metadata_service_id,
                                        sequence_number: emit_header.sequence_number,
                                        // Always Complete on emit — the
                                        // reassembler collapses First/
                                        // Middle/Last into Complete on the
                                        // emit path.
                                        cell_fragment_indication:
                                            crate::mpegts::au_cell::CellFragmentIndication::Complete,
                                        decoder_config_flag: emit_header.decoder_config_flag,
                                        random_access_indicator: emit_header
                                            .random_access_indicator,
                                        was_reassembled,
                                        cell_count,
                                    },
                                    payload: payload_vec,
                                });
                            }
                            crate::mpegts::demux::au_reassemble::ReassembleOutcome::Buffered => {
                                // Wait for more cells (either later in
                                // this PES, or in a subsequent PES on the
                                // same PID).
                            }
                            crate::mpegts::demux::au_reassemble::ReassembleOutcome::Failure {
                                reason,
                                dropped_bytes,
                            } => {
                                use crate::mpegts::au_cell::CellFragmentIndication;
                                use crate::mpegts::demux::event::MultiCellAuReason;

                                // Tolerance branch: when configured AND the
                                // failure was an orphan Middle/Last AND the
                                // orphan payload is a self-consistent KLV
                                // record, emit it as Complete + a dedicated
                                // diagnostic so the malformation is loud but
                                // the data flows. On by default; callers
                                // may turn it off via
                                // DemuxerConfig::cfi_tolerance.
                                let observed_cfi = h.cell_fragment_indication;
                                let orphan_continuation =
                                    matches!(reason, MultiCellAuReason::Orphan)
                                        && matches!(
                                            observed_cfi,
                                            CellFragmentIndication::Middle
                                                | CellFragmentIndication::Last
                                        );
                                let tolerated = orphan_continuation
                                    && self.options.cfi_tolerance
                                    && orphan_validates_as_complete_klv(current_inner);

                                if tolerated {
                                    let payload_vec = current_inner.to_vec();
                                    let meta_len = payload_vec.len();
                                    self.record_item(&stream, program_number, meta_len);
                                    self.pid_state_mut(stream.pid)
                                        .codec_mut(CodecKind::Klv)
                                        .add_klv(1);
                                    self.push_pes_event(
                                        damage,
                                        DemuxEvent::Metadata {
                                            stream,
                                            pts,
                                            kind: MetadataKind::KlvSyncAuCell {
                                                metadata_service_id: h.metadata_service_id,
                                                sequence_number: h.sequence_number,
                                                // Substituted: the wire said
                                                // Middle/Last, but we surface as
                                                // Complete since the payload is
                                                // self-consistent.
                                                cell_fragment_indication:
                                                    CellFragmentIndication::Complete,
                                                decoder_config_flag: h.decoder_config_flag,
                                                random_access_indicator: h.random_access_indicator,
                                                was_reassembled: false,
                                                cell_count: 1,
                                            },
                                            payload: payload_vec,
                                        },
                                    );
                                    self.queue_nonconformant(
                                        stream,
                                        NonConformantIssue::CfiTolerated {
                                            pid: pes.pid,
                                            observed_cfi,
                                            treated_as: CellFragmentIndication::Complete,
                                        },
                                    );
                                    // Do NOT also queue MultiCellAu{Orphan} —
                                    // the cell was rescued, not dropped.
                                } else {
                                    self.queue_nonconformant(
                                        stream,
                                        NonConformantIssue::MultiCellAu {
                                            pid: pes.pid,
                                            dropped_bytes,
                                            reason,
                                        },
                                    );
                                    // ConcurrentFirst, and a SequenceGap
                                    // detected on a First or Complete cell,
                                    // dropped only earlier buffers: replay
                                    // the triggering cell against the
                                    // now-empty state. Other failures
                                    // (Orphan / continuation-cell
                                    // SequenceGap / Overflow) terminate the
                                    // AU outright.
                                    let replay = match reason {
                                        MultiCellAuReason::ConcurrentFirst => true,
                                        MultiCellAuReason::SequenceGap => matches!(
                                            observed_cfi,
                                            CellFragmentIndication::First
                                                | CellFragmentIndication::Complete
                                        ),
                                        _ => false,
                                    };
                                    if replay {
                                        to_process = Some((h, current_inner));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            StreamKind::Unknown(stream_type) => {
                let payload_len = pes.payload.len();
                self.record_item(&stream, program_number, payload_len);
                self.push_pes_event(
                    damage,
                    DemuxEvent::Sample {
                        stream,
                        pts,
                        dts,
                        payload: SamplePayload::Unknown {
                            stream_type: StreamTypeCode::from_byte(stream_type),
                            raw: SharedBytes::from_slice(&pes.payload),
                        },
                    },
                );
            }
            StreamKind::Audio(codec) => {
                let payload_len = pes.payload.len();
                self.record_item(&stream, program_number, payload_len);
                // For AAC-LATM (stream_type 0x11) validate the LOAS
                // syncword at every record boundary. Without a framing
                // check, malformed streams produce opaque Sample events
                // that downstream decoders can't parse. Lenient mode
                // surfaces the NonConformantIssue alongside the Sample
                // (callers may want the raw bytes for forensic analysis);
                // strict mode (Full) suppresses the sample.
                let latm_rejected = if codec == AudioCodec::AacLatm {
                    self.check_latm_framing(&pes, damage.is_some(), stream)
                } else {
                    false
                };
                if latm_rejected {
                    return;
                }
                // Codec-specific counter bump. AAC-ADTS + MP2 have frame
                // iterators in `codec::*`; LATM + AC-3 don't (their
                // `stream_codec_stats` accessor falls back to
                // `StreamCodecStats::Unknown` via the stats-only
                // path).
                //
                // Use the resync variants (`frames_with_resync`) so a single
                // malformed syncframe in the middle of a PES payload doesn't
                // drop the rest of the frame count — the strict `frames()`
                // iterators terminate on first parse error and undercount
                // stats. Strict `frames()` remains available for fail-fast
                // conformance callers (fuzzers, spec tests).
                let frames_delta: u64 = match codec {
                    AudioCodec::Aac => crate::codec::aac::frames_with_resync(&pes.payload)
                        .filter_map(Result::ok)
                        .count() as u64,
                    AudioCodec::Mp2 => crate::codec::mpegaudio::frames_with_resync(&pes.payload)
                        .filter_map(Result::ok)
                        .count() as u64,
                    _ => 0, // AacLatm / Ac3 — no iterator yet
                };
                if frames_delta > 0 {
                    self.pid_state_mut(stream.pid)
                        .codec_mut(CodecKind::Audio)
                        .add_audio(frames_delta);
                }
                // AC-3 syncframe alignment enforcement.
                //
                // ATSC A/52:2018 §A.6.3 mandates `data_alignment_indicator=1`
                // for every AC-3 PES, which requires the payload to be
                // byte-aligned to a syncframe, not one syncframe per PES.
                // We check only the two sync-word bytes (0x0B77). Surface a NonConformantIssue when the alignment flag is
                // set but the payload doesn't begin with the syncword;
                // strict mode (Full) suppresses the sample so consumers
                // can fail closed.
                let ac3_sync_rejected = if matches!(codec, AudioCodec::Ac3)
                    && pes.data_alignment_indicator
                    && !(pes.payload.len() >= 2 && pes.payload[0] == 0x0B && pes.payload[1] == 0x77)
                {
                    let issue = NonConformantIssue::Ac3SyncMissing { pid: pes.pid };
                    let reject = self.options.strict.rejects(&issue);
                    self.queue_nonconformant(stream, issue);
                    reject
                } else {
                    false
                };
                if !ac3_sync_rejected {
                    self.push_pes_event(
                        damage,
                        DemuxEvent::Sample {
                            stream,
                            pts,
                            dts: None,
                            payload: SamplePayload::Audio {
                                codec,
                                frames: SharedBytes::from_slice(&pes.payload),
                            },
                        },
                    );
                }
            }
            StreamKind::Subtitle(codec) => {
                let payload_len = pes.payload.len();
                if set_once(&mut self.pid_state_mut(stream.pid).subtitle_seen) {
                    self.subtitle_streams_seen_count += 1;
                }
                let entry = self.record_item(&stream, program_number, payload_len);
                entry.label.get_or_insert_with(|| {
                    crate::mpegts::stats::demux_subtitle_codec_label(codec).to_string()
                });
                // EN 300 743 §6.2 (DVB-sub) + EN 300 472 §4.2
                // (teletext) mandate `data_alignment_indicator = 1`.
                // CEA-708 standalone and WebVTT-in-TS don't formally
                // require it but conventionally set it. Surface a
                // NonConformant issue when absent on the DVB pair;
                // strict mode (Full) suppresses the sample.
                let needs_alignment = matches!(
                    codec,
                    SubtitleCodec::DvbSubtitling | SubtitleCodec::DvbTeletext
                );
                let alignment_rejected = if needs_alignment && !pes.data_alignment_indicator {
                    let issue = NonConformantIssue::SubtitleAlignmentMissing { pid: pes.pid };
                    let reject = self.options.strict.rejects(&issue);
                    self.queue_nonconformant(stream, issue);
                    reject
                } else {
                    false
                };
                // For DVB subtitling, strip the EN 300 743 §6.2 PES_data_field
                // envelope (data_identifier + subtitle_stream_id + segments +
                // 0xFF end_marker) so callers see just the segment bytes —
                // matching what libavcodec's dvbsubdec expects (it rejects
                // anything that doesn't begin with a segment sync_byte 0x0F).
                // Other subtitle codecs (teletext, CEA-708 standalone, WebVTT)
                // do not have this wrapper; pass through verbatim.
                //
                // §6.2 Table 3 binds DVB-subtitle data_identifier to exactly
                // 0x20. The strip helper distinguishes Conformant (== 0x20),
                // NonConformantDataId (in the legacy permissive range
                // 0x20..=0x3F | 0x70..=0x7F but != 0x20), and Malformed
                // (anything else). For NonConformantDataId, lenient mode
                // strips + emits the sample alongside the
                // DvbSubDataIdentifier issue; strict mode suppresses the
                // sample so consumers can fail closed.
                let raw = &pes.payload;
                let teletext_ebu_profile = self
                    .pid_state
                    .get(&pes.pid)
                    .is_some_and(|s| s.teletext_ebu_profile);
                let surfaced_payload = if alignment_rejected {
                    // Strict-mode alignment rejection suppresses the sample so
                    // the receive loop can fail closed (parallel to the
                    // DvbSubDataIdentifier strict-mode path).
                    None
                } else {
                    match codec {
                        SubtitleCodec::DvbSubtitling => match strip_dvb_sub_envelope(raw) {
                            DvbSubStripResult::Conformant(s) => Some(s.to_vec()),
                            DvbSubStripResult::NonConformantDataId { observed, stripped } => {
                                let stripped = stripped.to_vec();
                                let issue = NonConformantIssue::DvbSubDataIdentifier { observed };
                                let reject = self.options.strict.rejects(&issue);
                                self.queue_nonconformant(stream, issue);
                                if reject { None } else { Some(stripped) }
                            }
                            DvbSubStripResult::Malformed => {
                                let issue = NonConformantIssue::MalformedPes {
                                    pid: pes.pid,
                                    reason: DVB_SUB_ENVELOPE_MALFORMED,
                                };
                                let reject = self.options.strict.rejects(&issue);
                                self.queue_nonconformant(stream, issue);
                                if reject { None } else { Some(raw.to_vec()) }
                            }
                        },
                        SubtitleCodec::DvbTeletext
                            if teletext_ebu_profile && !teletext_data_field_ok(raw) =>
                        {
                            let issue = NonConformantIssue::MalformedPes {
                                pid: pes.pid,
                                reason: TELETEXT_DATA_FIELD_MALFORMED,
                            };
                            let reject = self.options.strict.rejects(&issue);
                            self.queue_nonconformant(stream, issue);
                            if reject { None } else { Some(raw.to_vec()) }
                        }
                        _ => Some(raw.to_vec()),
                    }
                };
                if let Some(surfaced_payload) = surfaced_payload {
                    self.push_pes_event(
                        damage,
                        DemuxEvent::Sample {
                            stream,
                            pts,
                            dts: None,
                            payload: SamplePayload::Subtitle {
                                codec,
                                payload: SharedBytes::from_slice(&surfaced_payload),
                            },
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DemuxError;
    use crate::mpegts::demux::Demuxer;
    use crate::mpegts::demux::pes::{DAMAGE_CC_GAP, DAMAGE_SHORT_PES};
    use crate::mpegts::demux::test_support::{
        PktSpec, demuxer_with_streams, drain_all_events, feed_packets, issues, pes_bytes,
        pes_to_packets, ts_packet,
    };
    use crate::mpegts::demux::types::DemuxerConfig;

    /// Build a minimal-valid sync-KLV body: 16-byte SMPTE UAS Datalink UL,
    /// 1-byte BER short-form length, then `value_len` value bytes
    /// (total `17 + value_len`). Caller must keep `value_len < 128`.
    fn synth_klv_record(value_len: usize) -> Vec<u8> {
        assert!(value_len < 128, "BER short-form only — use < 128");
        let mut buf = Vec::with_capacity(17 + value_len);
        // MISB ST 0601 UAS Datalink Local Set UL prefix.
        buf.extend_from_slice(&[
            0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01, 0x0e, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ]);
        buf.push(value_len as u8); // BER short-form length
        buf.extend(core::iter::repeat_n(0x00u8, value_len));
        buf
    }

    #[test]
    fn validator_accepts_minimal_consistent_klv() {
        let payload = synth_klv_record(32);
        assert!(orphan_validates_as_complete_klv(&payload));
    }

    #[test]
    fn validator_rejects_short_payload() {
        // < 17 bytes can't carry UL + length.
        assert!(!orphan_validates_as_complete_klv(&[]));
        assert!(!orphan_validates_as_complete_klv(&[0x06; 16]));
    }

    #[test]
    fn validator_rejects_wrong_ul_prefix() {
        let mut payload = synth_klv_record(8);
        payload[0] = 0xFF; // first byte of UL is not 0x06
        assert!(!orphan_validates_as_complete_klv(&payload));
    }

    #[test]
    fn validator_rejects_ber_length_mismatch_short() {
        let mut payload = synth_klv_record(32);
        payload[16] = 64; // declares 64 bytes but only 32 follow
        assert!(!orphan_validates_as_complete_klv(&payload));
    }

    #[test]
    fn validator_rejects_ber_length_mismatch_long() {
        let mut payload = synth_klv_record(32);
        payload[16] = 16; // declares 16 bytes but 32 follow
        assert!(!orphan_validates_as_complete_klv(&payload));
    }

    #[test]
    fn validator_accepts_ber_long_form() {
        // 200-byte value with BER long-form length (one length byte).
        let value_len = 200usize;
        let mut buf = Vec::with_capacity(2 + 16 + value_len);
        // UL prefix (same MISB ST 0601 UL as above).
        buf.extend_from_slice(&[
            0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01, 0x0e, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ]);
        // BER long-form: 0x81 (length-of-length=1), then 200.
        buf.push(0x81);
        buf.push(value_len as u8);
        buf.extend(core::iter::repeat_n(0x00u8, value_len));
        assert!(orphan_validates_as_complete_klv(&buf));
    }

    fn audio_demuxer(mode: StrictMode) -> Demuxer {
        let mut b = DemuxerConfig::builder();
        b.strict(mode);
        demuxer_with_streams(b.build(), 0x101, &[(0x0F, 0x101, &[])])
    }

    fn three_packet_pes(pts: u64) -> Vec<u8> {
        pes_bytes(0xC0, Some(pts), None, true, &[0xAA; 400], true)
    }

    /// Two bounded audio PES on 0x101; the first loses its middle packet
    /// (CC 0, 2), the second arrives whole (CC 3..5).
    fn cc_gap_packets() -> Vec<[u8; 188]> {
        let mut pkts = pes_to_packets(0x101, 0, &three_packet_pes(90_000));
        pkts.remove(1);
        pkts.extend(pes_to_packets(0x101, 3, &three_packet_pes(92_000)));
        pkts
    }

    fn marker_index(ev: &[DemuxEvent], want: &str) -> Option<usize> {
        ev.iter().position(|e| {
            matches!(e, DemuxEvent::NonConformant {
                issue: NonConformantIssue::MalformedPes { pid: 0x101, reason }, ..
            } if *reason == want)
        })
    }

    fn sample_index(ev: &[DemuxEvent], pid: u16, want_pts: i64) -> Option<usize> {
        ev.iter().position(|e| {
            matches!(e, DemuxEvent::Sample { stream, pts, .. }
                if stream.pid == pid && pts.as_ticks() == want_pts)
        })
    }

    /// H.222.0 §2.4.3.3: a continuity_counter gap on a PES PID means packets were
    /// lost. Lenient mode delivers the damaged PES, marked by MalformedPes.
    #[test]
    fn cc_gap_inside_pes_marks_the_sample_in_lenient_mode() {
        let mut d = audio_demuxer(StrictMode::Off);
        feed_packets(&mut d, &cc_gap_packets()).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let marker = marker_index(&ev, DAMAGE_CC_GAP);
        let sample = sample_index(&ev, 0x101, 90_000);
        assert!(
            matches!((marker, sample), (Some(m), Some(s)) if m + 1 == s),
            "{ev:?}"
        );
        let samples = ev
            .iter()
            .filter(|e| matches!(e, DemuxEvent::Sample { .. }))
            .count();
        assert_eq!(samples, 2, "lenient mode keeps every sample");
    }

    #[test]
    fn damaged_pes_is_dropped_but_reported_in_timing_only() {
        let mut d = audio_demuxer(StrictMode::TimingOnly);
        feed_packets(&mut d, &cc_gap_packets()).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(marker_index(&ev, DAMAGE_CC_GAP).is_some(), "{ev:?}");
        assert!(sample_index(&ev, 0x101, 90_000).is_none(), "{ev:?}");
        assert!(sample_index(&ev, 0x101, 92_000).is_some(), "{ev:?}");
    }

    #[test]
    fn damaged_pes_fails_feed_in_full_mode() {
        let mut d = audio_demuxer(StrictMode::Full);
        let err = feed_packets(&mut d, &cc_gap_packets()).unwrap_err();
        assert!(
            matches!(err, DemuxError::MalformedPes { pid: 0x101, reason } if reason == DAMAGE_CC_GAP),
            "{err:?}"
        );
        let ev = drain_all_events(&mut d);
        assert!(sample_index(&ev, 0x101, 90_000).is_none(), "{ev:?}");
    }

    #[test]
    fn short_bounded_pes_at_flush_is_marked() {
        let mut d = audio_demuxer(StrictMode::Off);
        let pkts = pes_to_packets(0x101, 0, &three_packet_pes(90_000));
        feed_packets(&mut d, &pkts[..2]).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let marker = marker_index(&ev, DAMAGE_SHORT_PES);
        let sample = sample_index(&ev, 0x101, 90_000);
        assert!(
            matches!((marker, sample), (Some(m), Some(s)) if m + 1 == s),
            "{ev:?}"
        );
    }

    /// The marker sits directly before the damaged sample, after the other
    /// issues that PES raised (here a backward PTS).
    #[test]
    fn damage_marker_follows_the_pes_own_issues_in_lenient_mode() {
        let mut d = audio_demuxer(StrictMode::Off);
        let mut pkts = pes_to_packets(0x101, 0, &three_packet_pes(900_000));
        let mut damaged = pes_to_packets(0x101, 3, &three_packet_pes(90_000));
        damaged.remove(1); // CC 3, 5
        pkts.extend(damaged);
        pkts.extend(pes_to_packets(0x101, 6, &three_packet_pes(92_000)));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let anomaly = ev.iter().position(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PtsAnomaly { .. },
                    ..
                }
            )
        });
        let marker = marker_index(&ev, DAMAGE_CC_GAP);
        let sample = sample_index(&ev, 0x101, 90_000);
        assert!(
            matches!((anomaly, marker, sample), (Some(a), Some(m), Some(s)) if a < m && m + 1 == s),
            "{ev:?}"
        );
    }

    #[test]
    fn short_bounded_pes_at_flush_is_dropped_in_timing_only() {
        for mode in [StrictMode::TimingOnly, StrictMode::DescriptorsOnly] {
            let mut d = audio_demuxer(mode);
            let pkts = pes_to_packets(0x101, 0, &three_packet_pes(90_000));
            feed_packets(&mut d, &pkts[..2]).unwrap();
            d.flush();
            let ev = drain_all_events(&mut d);
            assert!(
                marker_index(&ev, DAMAGE_SHORT_PES).is_some(),
                "{mode:?}: {ev:?}"
            );
            assert!(
                sample_index(&ev, 0x101, 90_000).is_none(),
                "{mode:?}: {ev:?}"
            );
        }
    }

    #[test]
    fn unbounded_video_pes_completing_at_pusi_is_not_marked() {
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x100,
            &[(0x1B, 0x100, &[])],
        );
        let first = pes_bytes(0xE0, Some(90_000), None, true, &[0xAA; 400], false);
        let second = pes_bytes(0xE0, Some(93_000), None, true, &[0xAA; 40], false);
        let mut pkts = pes_to_packets(0x100, 0, &first);
        assert_eq!(pkts.len(), 3);
        pkts.extend(pes_to_packets(0x100, 3, &second));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(sample_index(&ev, 0x100, 90_000).is_some(), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::MalformedPes { .. })),
            "{ev:?}"
        );
    }

    #[test]
    fn aggregate_overflow_records_a_discontinuity_for_every_dropped_pid() {
        let mut b = DemuxerConfig::builder();
        b.pes_cap_total(400);
        let mut d = demuxer_with_streams(
            b.build(),
            0x101,
            &[(0x0F, 0x101, &[]), (0x0F, 0x102, &[]), (0x0F, 0x103, &[])],
        );
        let pkts: Vec<[u8; 188]> = [0x101, 0x102, 0x103]
            .iter()
            .map(|&pid| pes_to_packets(pid, 0, &three_packet_pes(90_000))[0])
            .collect();
        feed_packets(&mut d, &pkts).unwrap();
        let ev = drain_all_events(&mut d);
        let pids: Vec<u16> = ev
            .iter()
            .filter_map(|e| match e {
                DemuxEvent::Discontinuity {
                    stream,
                    kind: DiscontinuityKind::PesTotalOversize,
                } => Some(stream.pid),
                _ => None,
            })
            .collect();
        assert_eq!(pids, vec![0x101, 0x102, 0x103], "{ev:?}");
    }

    /// Backward PTS after a new time base is the splice, not a PTS anomaly.
    #[test]
    fn pts_after_signalled_timebase_reset_is_not_a_pts_anomaly() {
        let mut b = DemuxerConfig::builder();
        b.strict(StrictMode::TimingOnly);
        let mut d = demuxer_with_streams(b.build(), 0x100, &[(0x1B, 0x100, &[])]);
        let pcr = |disc, v| {
            ts_packet(&PktSpec {
                pid: 0x100,
                discontinuity: disc,
                pcr: Some(v),
                ..Default::default()
            })
        };
        // Bounded, so each PES completes on its own last packet rather than
        // at the next PUSI: the first one is judged before the new time base.
        let before = pes_bytes(0xE0, Some(900_000), None, true, &[0xAA; 40], true);
        let after = pes_bytes(0xE0, Some(90_000), None, true, &[0xAA; 40], true);
        let mut pkts = vec![pcr(false, 0)];
        pkts.extend(pes_to_packets(0x100, 0, &before));
        // H.222.0 §2.4.3.5: a new system time base on the PCR_PID.
        pkts.push(pcr(true, 2_700_000_000));
        pkts.extend(pes_to_packets(0x100, 1, &after));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(sample_index(&ev, 0x100, 90_000).is_some(), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
            "{ev:?}"
        );
    }

    /// An unbounded video PES that started on the old time base completes
    /// only at the PUSI that carries the new-base PCR: it is judged on the old
    /// clock, and the PES after it starts the new PTS baseline.
    #[test]
    fn pts_after_signalled_timebase_reset_with_unbounded_video_is_not_a_pts_anomaly() {
        let mut b = DemuxerConfig::builder();
        b.strict(StrictMode::TimingOnly);
        let mut d = demuxer_with_streams(b.build(), 0x100, &[(0x1B, 0x100, &[])]);
        let before = pes_bytes(0xE0, Some(900_000), None, true, &[0xAA; 300], false);
        let after = pes_bytes(0xE0, Some(90_000), None, true, &[0xAA; 40], false);
        let next = pes_bytes(0xE0, Some(93_000), None, true, &[0xAA; 40], false);
        let mut pkts = vec![ts_packet(&PktSpec {
            pid: 0x100,
            pcr: Some(0),
            ..Default::default()
        })];
        let old = pes_to_packets(0x100, 0, &before);
        assert_eq!(old.len(), 2);
        pkts.extend(old);
        // H.222.0 §2.4.3.5: the new time base rides the PUSI that starts the
        // first PES on it, and that PUSI is what completes the old PES.
        pkts.push(ts_packet(&PktSpec {
            pid: 0x100,
            cc: 2,
            pusi: true,
            discontinuity: true,
            pcr: Some(2_700_000_000),
            payload: &after,
            ..Default::default()
        }));
        pkts.extend(pes_to_packets(0x100, 3, &next));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(sample_index(&ev, 0x100, 900_000).is_some(), "{ev:?}");
        assert!(sample_index(&ev, 0x100, 90_000).is_some(), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
            "{ev:?}"
        );
    }

    /// H.222.0 §2.4.3.3: a duplicate of the packet carrying the new time base
    /// repeats the signal, not the reset; the PES the original opened keeps
    /// its baseline, so a backward PTS after it is still an anomaly.
    #[test]
    fn duplicate_of_a_timebase_reset_packet_does_not_reset_again() {
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x100,
            &[(0x1B, 0x100, &[])],
        );
        let before = pes_bytes(0xE0, Some(900_000), None, true, &[0xAA; 300], false);
        let after = pes_bytes(0xE0, Some(270_000), None, true, &[0xAA; 40], false);
        let backward = pes_bytes(0xE0, Some(90_000), None, true, &[0xAA; 40], false);
        let mut pkts = vec![ts_packet(&PktSpec {
            pid: 0x100,
            pcr: Some(0),
            ..Default::default()
        })];
        pkts.extend(pes_to_packets(0x100, 0, &before));
        let reset = ts_packet(&PktSpec {
            pid: 0x100,
            cc: 2,
            pusi: true,
            discontinuity: true,
            pcr: Some(2_700_000_000),
            payload: &after,
            ..Default::default()
        });
        pkts.extend([reset, reset]);
        pkts.extend(pes_to_packets(0x100, 3, &backward));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(
            issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
            "{ev:?}"
        );
    }

    /// H.222.0 §2.4.3.5: in the discontinuity state the second of two identical
    /// same-CC packets may be discarded; its payload must not be appended twice.
    #[test]
    fn identical_duplicate_with_discontinuity_indicator_is_not_replayed() {
        let es: Vec<u8> = (0..400u16).map(|i| i as u8).collect();
        let pes = pes_bytes(0xC0, Some(0), None, true, &es, true);
        // The packet carrying discontinuity_indicator needs adaptation-field
        // room, so it holds 182 payload bytes instead of 184.
        let (head, rest) = pes.split_at(184);
        let (middle, tail) = rest.split_at(182);
        let first = ts_packet(&PktSpec {
            pid: 0x101,
            cc: 0,
            pusi: true,
            payload: head,
            ..Default::default()
        });
        let flagged = ts_packet(&PktSpec {
            pid: 0x101,
            cc: 1,
            discontinuity: true,
            payload: middle,
            ..Default::default()
        });
        let last = ts_packet(&PktSpec {
            pid: 0x101,
            cc: 2,
            payload: tail,
            ..Default::default()
        });
        let mut d = audio_demuxer(StrictMode::Off);
        feed_packets(&mut d, &[first, flagged, flagged, last]).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let frames = ev.iter().find_map(|e| match e {
            DemuxEvent::Sample {
                payload: SamplePayload::Audio { frames, .. },
                ..
            } => Some(frames.to_vec()),
            _ => None,
        });
        assert_eq!(frames.as_deref(), Some(&es[..]), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::MalformedPes { .. })),
            "{ev:?}"
        );
    }

    /// One bounded audio PES (20-byte ES) on 0x101, CC from `cc`, which
    /// advances past the packets returned.
    fn audio_pes(cc: &mut u8, pts: Option<u64>) -> Vec<[u8; 188]> {
        let pkts = pes_to_packets(
            0x101,
            *cc,
            &pes_bytes(0xC0, pts, None, true, &[0xAA; 20], true),
        );
        *cc = cc.wrapping_add(pkts.len() as u8) & 0x0F;
        pkts
    }

    fn missing_pts_count(ev: &[DemuxEvent], pid: u16) -> usize {
        issues(ev)
            .iter()
            .filter(|i| matches!(i, NonConformantIssue::MissingRequiredPts { pid: p } if *p == pid))
            .count()
    }

    /// Feed audio PES with `pts` in order on a TimingOnly demuxer; the feed
    /// result and every event.
    fn feed_audio_timing_only(pts: &[Option<u64>]) -> (Result<(), DemuxError>, Vec<DemuxEvent>) {
        let mut d = audio_demuxer(StrictMode::TimingOnly);
        let mut cc = 0;
        let pkts: Vec<[u8; 188]> = pts.iter().flat_map(|p| audio_pes(&mut cc, *p)).collect();
        let result = feed_packets(&mut d, &pkts);
        d.flush();
        (result, drain_all_events(&mut d))
    }

    #[test]
    fn pts_less_audio_pes_within_0_7_s_raises_nothing() {
        let (result, ev) = feed_audio_timing_only(&[Some(0), None, Some(3_000)]);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// H.222.0 §2.7.4: consecutive coded PTS of one A/V stream differ by at most
    /// 0.7 s. A PTS-less run whose bracketing PTS differ by more raises once.
    #[test]
    fn pts_less_run_longer_than_0_7_s_raises_missing_required_pts_once() {
        let mut d = audio_demuxer(StrictMode::Off);
        let mut cc = 0;
        let mut pkts = audio_pes(&mut cc, Some(0));
        for _ in 0..10 {
            pkts.extend(audio_pes(&mut cc, None));
        }
        pkts.extend(audio_pes(&mut cc, Some(63_001)));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(missing_pts_count(&ev, 0x101), 1, "{ev:?}");
        let issue = ev.iter().position(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::MissingRequiredPts { pid: 0x101 },
                    ..
                }
            )
        });
        let sample = sample_index(&ev, 0x101, 63_001);
        assert!(
            matches!((issue, sample), (Some(i), Some(s)) if i + 1 == s),
            "{ev:?}"
        );
    }

    #[test]
    fn pts_spacing_of_exactly_0_7_s_is_allowed() {
        let (result, ev) = feed_audio_timing_only(&[Some(0), None, Some(63_000)]);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// A 1 fps stream that timestamps every PES is conformant.
    #[test]
    fn one_fps_stream_with_every_pes_timestamped_raises_nothing() {
        let (result, ev) = feed_audio_timing_only(&[Some(0), Some(90_000), Some(180_000)]);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    #[test]
    fn pts_spacing_baseline_resets_on_a_signalled_timebase_discontinuity() {
        let mut d = audio_demuxer(StrictMode::TimingOnly);
        let pcr = |disc, v| {
            ts_packet(&PktSpec {
                pid: 0x101,
                discontinuity: disc,
                pcr: Some(v),
                ..Default::default()
            })
        };
        let mut cc = 0;
        let mut pkts = vec![pcr(false, 0)];
        pkts.extend(audio_pes(&mut cc, Some(0)));
        pkts.extend(audio_pes(&mut cc, None));
        // H.222.0 §2.4.3.5: a new system time base on the PCR_PID.
        pkts.push(pcr(true, 2_700_000_000));
        pkts.extend(audio_pes(&mut cc, Some(900_000)));
        let result = feed_packets(&mut d, &pkts);
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// Audio PES on 0x101 with `pts`, each preceded by a PCR on 0x101 (the
    /// program's PCR_PID) stepping evenly from 0 to `pcr_span_27mhz`.
    fn audio_run_under_pcr(pts: &[Option<u64>], pcr_span_27mhz: u64) -> Vec<[u8; 188]> {
        let steps = pts.len().saturating_sub(1).max(1) as u64;
        let mut cc = 0;
        let mut pkts = Vec::new();
        for (i, p) in pts.iter().enumerate() {
            pkts.push(ts_packet(&PktSpec {
                pid: 0x101,
                pcr: Some(pcr_span_27mhz * i as u64 / steps),
                ..Default::default()
            }));
            pkts.extend(audio_pes(&mut cc, *p));
        }
        pkts
    }

    /// H.222.0 §2.7.4: an audio stream that never stamps a PTS breaks the
    /// 0.7 s bound; it is reported once the PCR has advanced past 1.7 s.
    #[test]
    fn pts_less_stream_longer_than_1_7_s_of_pcr_raises_missing_required_pts_once() {
        let pkts = audio_run_under_pcr(&[None; 11], 54_000_000);
        let (result, ev) = feed_audio(StrictMode::Off, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 1, "{ev:?}");
    }

    #[test]
    fn pts_less_stream_within_0_7_s_of_pcr_raises_nothing() {
        let pkts = audio_run_under_pcr(&[None; 11], 13_500_000);
        let (result, ev) = feed_audio(StrictMode::TimingOnly, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// A PCR span above 0.7 s but within the 1 s decoder-buffer allowance
    /// (H.222.0 §2.4.2.7) is not evidence of a broken PTS bound.
    #[test]
    fn pts_less_stream_within_1_7_s_of_pcr_raises_nothing() {
        let pkts = audio_run_under_pcr(&[None; 11], 32_400_000);
        let (result, ev) = feed_audio(StrictMode::TimingOnly, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// A run that follows a coded PTS and never closes is timed by the PCR
    /// too; a PTS that closes it late does not report it a second time.
    #[test]
    fn unclosed_pts_less_run_after_a_coded_pts_is_timed_by_the_pcr() {
        let mut pts = vec![Some(0)];
        pts.extend([None; 10]);
        let pkts = audio_run_under_pcr(&pts, 81_000_000);
        let (result, ev) = feed_audio(StrictMode::Off, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 1, "{ev:?}");

        pts.push(Some(200_000));
        let pkts = audio_run_under_pcr(&pts, 81_000_000);
        let (result, ev) = feed_audio(StrictMode::Off, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 1, "{ev:?}");
    }

    #[test]
    fn one_fps_stream_with_every_pes_timestamped_under_a_pcr_raises_nothing() {
        let pkts = audio_run_under_pcr(&[Some(0), Some(90_000), Some(180_000)], 54_000_000);
        let (result, ev) = feed_audio(StrictMode::TimingOnly, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// The PCR latch of a PTS-less run is a value on the PCR clock: a
    /// signalled new time base clears it at once, even with a PES in flight,
    /// so the PES that straddles the reset is not timed across two clocks.
    #[test]
    fn pts_less_run_across_a_signalled_pcr_jump_is_not_timed_across_clocks() {
        let mut d = audio_demuxer(StrictMode::TimingOnly);
        let pcr = |disc, v| {
            ts_packet(&PktSpec {
                pid: 0x101,
                discontinuity: disc,
                pcr: Some(v),
                ..Default::default()
            })
        };
        let mut cc = 0;
        let mut pkts = vec![pcr(false, 0)];
        pkts.extend(audio_pes(&mut cc, None));
        let straddling = pes_to_packets(
            0x101,
            cc,
            &pes_bytes(0xC0, None, None, true, &[0xAA; 400], true),
        );
        assert_eq!(straddling.len(), 3);
        pkts.push(straddling[0]);
        // H.222.0 §2.4.3.5: a new system time base 10 s ahead, with the
        // PTS-less PES in flight.
        pkts.push(pcr(true, 270_000_000));
        pkts.extend_from_slice(&straddling[1..]);
        let result = feed_packets(&mut d, &pkts);
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
    }

    /// One single-packet PES on 0x101 (its own PCR_PID) carrying the PCR at
    /// `pcr_s` seconds; `pts` `None` makes it PTS-less.
    fn pcr_stamped_pes(cc: &mut u8, pcr_s: u64, pts: Option<u64>, es: &[u8]) -> [u8; 188] {
        let pkt = ts_packet(&PktSpec {
            pid: 0x101,
            cc: *cc,
            pusi: true,
            pcr: Some(pcr_s * 27_000_000),
            payload: &pes_bytes(0xC0, pts, None, false, es, true),
            ..Default::default()
        });
        *cc = (*cc + 1) & 0x0F;
        pkt
    }

    /// ISO/IEC 14496-3 LATM: §2.4.2.7 allows a 10 s decoder delay, so a PTS-less
    /// run spanning 5 s of PCR with legal coded-PTS spacing raises nothing.
    #[test]
    fn pts_less_run_on_a_14496_stream_is_allowed_a_ten_second_decoder_delay() {
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder()
                .strict(StrictMode::TimingOnly)
                .build(),
            0x101,
            &[(0x11, 0x101, &[])],
        );
        // PES with PTS 0 at PCR 0; PTS-less PES at PCR 1 s, 2 s, 3 s, 4 s, 5 s;
        // closing PTS 60_000 (0.67 s).
        let rec = loas_record(20);
        let mut cc = 0;
        let mut pkts = vec![pcr_stamped_pes(&mut cc, 0, Some(0), &rec)];
        for s in 1..=5 {
            pkts.push(pcr_stamped_pes(&mut cc, s, None, &rec));
        }
        pkts.push(pcr_stamped_pes(&mut cc, 6, Some(60_000), &rec));
        let result = feed_packets(&mut d, &pkts);
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
        assert_eq!(latm_framing_count(&ev), 0, "{ev:?}");
    }

    /// The same run on the 0x0F (ISO/IEC 13818-7) fixture keeps the 1 s allowance.
    #[test]
    fn pts_less_run_on_a_13818_7_stream_keeps_the_one_second_decoder_delay() {
        let mut d = audio_demuxer(StrictMode::Off);
        let mut cc = 0;
        let mut pkts = vec![pcr_stamped_pes(&mut cc, 0, Some(0), &[0xAA; 20])];
        for s in 1..=3 {
            pkts.push(pcr_stamped_pes(&mut cc, s, None, &[0xAA; 20]));
        }
        let result = feed_packets(&mut d, &pkts);
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 1, "{ev:?}");
    }

    /// Audio 0x101 (also the PCR_PID). PES A (PTS 900_000) completes. PES B
    /// starts bounded with PES_packet_length 0x0100 (so it stays in flight
    /// through the next packet and is closed only by the PUSI after it) and an
    /// unparseable header (`PES_header_data_length` 0xFF past the bytes that
    /// arrive). A packet with discontinuity_indicator and a PCR announces a new
    /// clock (`forget_program_pts`: B is in flight, so the clear is deferred).
    /// The next PUSI packet ends B — parse error, no `Complete` — and carries
    /// the start of PES C with PTS 1_000 on the new clock; C completes either
    /// inside that packet or in the packet after it. C must not be judged
    /// against A's PTS (judged so, it is a backward step past the 1 s
    /// `PtsAnomaly` threshold).
    #[test]
    fn unparseable_pes_at_a_pusi_settles_the_pending_reset_before_the_new_pes() {
        let run = |mode, c_len: usize| {
            let mut d = audio_demuxer(mode);
            let a = pes_to_packets(
                0x101,
                0,
                &pes_bytes(0xC0, Some(900_000), None, true, &[0xAA; 20], true),
            );
            let b_bad = ts_packet(&PktSpec {
                pid: 0x101,
                cc: 1,
                pusi: true,
                discontinuity: false,
                rai: false,
                pcr: None,
                scrambling: 0,
                payload: &[
                    0x00, 0x00, 0x01, 0xC0, 0x01, 0x00, 0x80, 0x80, 0xFF, 0x00, 0x00,
                ],
            });
            let splice = ts_packet(&PktSpec {
                pid: 0x101,
                cc: 2,
                pusi: false,
                discontinuity: true,
                rai: false,
                pcr: Some(300 * 1_000),
                scrambling: 0,
                payload: &[0xFF; 10],
            });
            let c = pes_to_packets(
                0x101,
                3,
                &pes_bytes(0xC0, Some(1_000), None, true, &vec![0xCC; c_len], true),
            );
            let next_cc = (3 + c.len() as u8) & 0x0F;
            let later = pes_to_packets(
                0x101,
                next_cc,
                &pes_bytes(0xC0, Some(2_000), None, true, &[0xDD; 20], true),
            );
            let mut pkts = a;
            pkts.extend([b_bad, splice]);
            pkts.extend(c);
            pkts.extend(later);
            let r = feed_packets(&mut d, &pkts);
            d.flush();
            (r, drain_all_events(&mut d))
        };
        // 20 bytes: C completes inside its PUSI packet; 300 bytes: in the next.
        for c_len in [20, 300] {
            let (r, ev) = run(StrictMode::Off, c_len);
            assert!(r.is_ok(), "c_len {c_len}: {r:?}");
            assert!(
                !issues(&ev)
                    .iter()
                    .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
                "c_len {c_len}: {ev:?}"
            );
            assert!(
                issues(&ev)
                    .iter()
                    .any(|i| matches!(i, NonConformantIssue::MalformedPes { pid: 0x101, .. })),
                "c_len {c_len}: B's parse error is still reported: {ev:?}"
            );
            assert!(
                ev.iter().any(|e| matches!(
                    e,
                    DemuxEvent::Sample { stream, pts, .. }
                        if stream.pid == 0x101 && *pts == Pts90khz::new(1_000)
                )),
                "c_len {c_len}: C is delivered: {ev:?}"
            );
            let (r, _) = run(StrictMode::TimingOnly, c_len);
            assert!(
                r.is_ok(),
                "c_len {c_len}: TimingOnly must not reject C's PTS on the new clock: {r:?}"
            );
        }
    }

    /// Full mode returns the damage error at the PUSI that ends the PES
    /// straddling a time-base reset; the PUSI still settles the old-clock
    /// baseline, so a caller that keeps feeding is judged on the new clock.
    #[test]
    fn strict_damage_error_still_settles_a_pending_timebase_reset() {
        let mut d = audio_demuxer(StrictMode::Full);
        let pcr = |disc, v| {
            ts_packet(&PktSpec {
                pid: 0x101,
                discontinuity: disc,
                pcr: Some(v),
                ..Default::default()
            })
        };
        let mut cc = 0;
        let mut pkts = vec![pcr(false, 0)];
        pkts.extend(audio_pes(&mut cc, Some(900_000)));
        let damaged = pes_to_packets(0x101, cc, &three_packet_pes(903_000));
        pkts.push(damaged[0]);
        // H.222.0 §2.4.3.5: a new system time base with the old-clock PES in
        // flight; that PES then loses its middle packet.
        pkts.push(pcr(true, 2_700_000_000));
        pkts.push(damaged[2]);
        cc = (cc + 3) & 0x0F;
        // The next PES's PUSI ends the damaged one; it is still in flight
        // when `feed` returns the error.
        let next = pes_to_packets(0x101, cc, &three_packet_pes(90_000));
        pkts.push(next[0]);
        let err = feed_packets(&mut d, &pkts).unwrap_err();
        assert!(
            matches!(err, DemuxError::MalformedPes { pid: 0x101, .. }),
            "{err:?}"
        );
        drain_all_events(&mut d);

        let result = feed_packets(&mut d, &next[1..]);
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(result.is_ok(), "{result:?}");
        assert!(sample_index(&ev, 0x101, 90_000).is_some(), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
            "{ev:?}"
        );
    }

    /// EN 300 743: every DVB subtitle PES carries a PTS.
    #[test]
    fn dvb_subtitle_pes_without_pts_raises_missing_required_pts() {
        // subtitling_descriptor: "eng", type 0x10, composition page 1, ancillary page 1.
        let subtitling: &[u8] = &[0x59, 0x08, b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01];
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x102,
            &[(0x06, 0x102, subtitling)],
        );
        // data_identifier 0x20, subtitle_stream_id 0x00, one empty segment
        // (sync 0x0F, type 0x10, page 1, length 0), end_of_PES 0xFF.
        let es = [0x20, 0x00, 0x0F, 0x10, 0x00, 0x01, 0x00, 0x00, 0xFF];
        let pkts = pes_to_packets(0x102, 0, &pes_bytes(0xBD, None, None, true, &es, true));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(missing_pts_count(&ev, 0x102), 1, "{ev:?}");
    }

    /// A payload that is neither an AU cell nor a SMPTE UL on a 0x06 KLV PID
    /// is passed through tagged with the PMT's own stream_type byte.
    #[test]
    fn other_shaped_payload_on_a_0x06_klv_pid_reports_stream_type_0x06() {
        let klva_registration: &[u8] = &[0x05, 0x04, b'K', b'L', b'V', b'A'];
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x101,
            &[(0x06, 0x101, klva_registration)],
        );
        let es = [0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF];
        let pkts = pes_to_packets(
            0x101,
            0,
            &pes_bytes(0xBD, Some(90_000), None, true, &es, true),
        );
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let tag = ev.iter().find_map(|e| match e {
            DemuxEvent::Sample {
                payload: SamplePayload::Unknown { stream_type, .. },
                ..
            } => Some(*stream_type),
            _ => None,
        });
        assert_eq!(tag, Some(StreamTypeCode::from_byte(0x06)), "{ev:?}");
    }

    fn sync_klv_demuxer() -> Demuxer {
        demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x103,
            &[(0x15, 0x103, &[])],
        )
    }

    /// One AU-cell-wrapped KLV record, as a synchronous metadata PES body.
    fn au_cell_es() -> Vec<u8> {
        let mut es = Vec::new();
        crate::mpegts::au_cell::write_metadata_au_cell(
            &mut es,
            crate::mpegts::au_cell::AuCellHeader {
                metadata_service_id: 0,
                sequence_number: 0,
                cell_fragment_indication: crate::mpegts::au_cell::CellFragmentIndication::Complete,
                decoder_config_flag: false,
                random_access_indicator: true,
            },
            &synth_klv_record(8),
        )
        .unwrap();
        es
    }

    /// ST 1402-07: each synchronous-metadata PES carries a PTS.
    #[test]
    fn sync_klv_pes_without_pts_raises_missing_required_pts() {
        let mut d = sync_klv_demuxer();
        let pkts = pes_to_packets(
            0x103,
            0,
            &pes_bytes(0xFC, None, None, true, &au_cell_es(), true),
        );
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(missing_pts_count(&ev, 0x103), 1, "{ev:?}");
    }

    /// ST 1402-08: PTS_DTS_flags = '10' on synchronous-metadata PES.
    #[test]
    fn sync_klv_pes_with_dts_raises_one_other_issue_per_pid() {
        let mut d = sync_klv_demuxer();
        let es = au_cell_es();
        let mut pkts = pes_to_packets(
            0x103,
            0,
            &pes_bytes(0xFC, Some(93_000), Some(90_000), true, &es, true),
        );
        pkts.extend(pes_to_packets(
            0x103,
            1,
            &pes_bytes(0xFC, Some(96_000), Some(93_000), true, &es, true),
        ));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let others = issues(&ev)
            .iter()
            .filter(|i| matches!(i, NonConformantIssue::Other(s) if s.contains("ST 1402-08")))
            .count();
        assert_eq!(others, 1, "{ev:?}");
    }

    /// A PES cut short by the per-PID cap after a signalled time-base reset
    /// was the old clock's: the next PES starts the new baseline even when it
    /// begins and ends inside one packet.
    #[test]
    fn pes_dropped_across_a_timebase_reset_does_not_leave_the_old_baseline() {
        let mut b = DemuxerConfig::builder();
        b.strict(StrictMode::TimingOnly);
        b.pes_cap_per_pid(300);
        let mut d = demuxer_with_streams(b.build(), 0x101, &[(0x0F, 0x101, &[])]);
        let pcr = |disc, v| {
            ts_packet(&PktSpec {
                pid: 0x101,
                discontinuity: disc,
                pcr: Some(v),
                ..Default::default()
            })
        };
        let mut cc = 0;
        let mut pkts = vec![pcr(false, 0)];
        pkts.extend(audio_pes(&mut cc, Some(900_000)));
        let truncated = pes_to_packets(0x101, cc, &three_packet_pes(903_000));
        pkts.push(truncated[0]);
        // H.222.0 §2.4.3.5: a new system time base on the PCR_PID, with the
        // old-clock PES in flight.
        pkts.push(pcr(true, 2_700_000_000));
        // The second packet breaches the cap: the PES is dropped.
        pkts.push(truncated[1]);
        cc = (cc + 2) & 0x0F;
        pkts.extend(audio_pes(&mut cc, Some(90_000)));
        let result = feed_packets(&mut d, &pkts);
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(result.is_ok(), "{result:?}");
        assert!(sample_index(&ev, 0x101, 90_000).is_some(), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
            "{ev:?}"
        );
    }

    /// A three-packet audio PES on 0x101 that loses its middle packet (a CC
    /// gap); `cc` advances past all three.
    fn damaged_audio_pes(cc: &mut u8, pts: Option<u64>) -> Vec<[u8; 188]> {
        let mut pkts = pes_to_packets(
            0x101,
            *cc,
            &pes_bytes(0xC0, pts, None, true, &[0xAA; 400], true),
        );
        assert_eq!(pkts.len(), 3);
        pkts.remove(1);
        *cc = cc.wrapping_add(3) & 0x0F;
        pkts
    }

    fn feed_audio(
        mode: StrictMode,
        pkts: &[[u8; 188]],
    ) -> (Result<(), DemuxError>, Vec<DemuxEvent>) {
        let mut d = audio_demuxer(mode);
        let result = feed_packets(&mut d, pkts);
        d.flush();
        (result, drain_all_events(&mut d))
    }

    /// A damaged PES a strict mode drops still carried its PTS on the wire:
    /// the spacing is measured from it, not from the PTS before it.
    #[test]
    fn dropped_damaged_pes_pts_counts_for_the_spacing_rule() {
        let mut cc = 0;
        let mut pkts = audio_pes(&mut cc, Some(0));
        pkts.extend(damaged_audio_pes(&mut cc, Some(60_000)));
        pkts.extend(audio_pes(&mut cc, None));
        pkts.extend(audio_pes(&mut cc, Some(70_000)));
        let (result, ev) = feed_audio(StrictMode::TimingOnly, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 0, "{ev:?}");
        assert!(sample_index(&ev, 0x101, 60_000).is_none(), "{ev:?}");
    }

    #[test]
    fn dropped_damaged_pts_less_pes_opens_a_pts_less_run() {
        let mut cc = 0;
        let mut pkts = audio_pes(&mut cc, Some(0));
        pkts.extend(damaged_audio_pes(&mut cc, None));
        pkts.extend(audio_pes(&mut cc, Some(63_001)));
        let (result, ev) = feed_audio(StrictMode::DescriptorsOnly, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(missing_pts_count(&ev, 0x101), 1, "{ev:?}");
    }

    #[test]
    fn dropped_damaged_pes_with_backward_pts_raises_pts_anomaly() {
        let mut cc = 0;
        let mut pkts = audio_pes(&mut cc, Some(900_000));
        pkts.extend(damaged_audio_pes(&mut cc, Some(90_000)));
        let (result, ev) = feed_audio(StrictMode::DescriptorsOnly, &pkts);
        assert!(result.is_ok(), "{result:?}");
        assert!(sample_index(&ev, 0x101, 90_000).is_none(), "{ev:?}");
        assert!(
            issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::PtsAnomaly { .. })),
            "{ev:?}"
        );
    }

    /// KLV on a `stream_type` 0x06 PID (registration "KLVA") is asynchronous
    /// metadata: neither the per-PES PTS rule (ST 1402-07) nor the DTS
    /// report (ST 1402-08) applies to it.
    #[test]
    fn async_klv_on_private_data_pid_has_no_pts_rule() {
        let klva: &[u8] = &[0x05, 0x04, b'K', b'L', b'V', b'A'];
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x104,
            &[(0x06, 0x104, klva)],
        );
        let klv = synth_klv_record(8);
        let mut pkts = pes_to_packets(
            0x104,
            0,
            &pes_bytes(0xBD, Some(93_000), Some(90_000), true, &klv, true),
        );
        pkts.extend(pes_to_packets(
            0x104,
            1,
            &pes_bytes(0xBD, None, None, true, &klv, true),
        ));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        let metadata = ev
            .iter()
            .filter(|e| matches!(e, DemuxEvent::Metadata { stream, .. } if stream.pid == 0x104))
            .count();
        assert_eq!(metadata, 2, "{ev:?}");
        assert!(
            !issues(&ev).iter().any(|i| matches!(
                i,
                NonConformantIssue::MissingRequiredPts { .. } | NonConformantIssue::Other(_)
            )),
            "{ev:?}"
        );
    }

    fn latm_demuxer() -> Demuxer {
        demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x104,
            &[(0x11, 0x104, &[])],
        )
    }

    /// One LOAS record (ISO/IEC 14496-3 §1.7.2): sync 0x2B7, 13-bit
    /// audioMuxLengthBytes, then `len` zero body bytes.
    fn loas_record(len: u16) -> Vec<u8> {
        let mut out = vec![0x56, 0xE0 | ((len >> 8) as u8 & 0x1F), (len & 0xFF) as u8];
        out.resize(3 + usize::from(len), 0);
        out
    }

    /// One unaligned (`data_alignment_indicator` = 0) LATM PES on 0x101,
    /// advancing `cc`.
    fn latm_pes(cc: &mut u8, payload: &[u8]) -> Vec<[u8; 188]> {
        let pkts = pes_to_packets(
            0x101,
            *cc,
            &pes_bytes(0xC0, Some(90_000), None, false, payload, true),
        );
        *cc = cc.wrapping_add(pkts.len() as u8) & 0x0F;
        pkts
    }

    /// LATM 0x101 with the given caps. PES 1 leaves a Body(33) carry, PES 2
    /// (400 bytes) overflows a 300-byte cap and is dropped, then `p3`. The
    /// feed's events.
    fn latm_cap_overflow_events(cap_per_pid: bool, p3: &[u8]) -> Vec<DemuxEvent> {
        let mut b = DemuxerConfig::builder();
        if cap_per_pid {
            b.pes_cap_per_pid(300);
        } else {
            b.pes_cap_total(300);
        }
        let mut d = demuxer_with_streams(b.build(), 0x101, &[(0x11, 0x101, &[])]);
        let record_b = loas_record(50);
        let mut p1 = loas_record(10);
        p1.extend_from_slice(&record_b[..20]);
        let mut cc = 0;
        let mut pkts = latm_pes(&mut cc, &p1);
        pkts.extend(latm_pes(&mut cc, &[0u8; 400]));
        pkts.extend(latm_pes(&mut cc, p3));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        drain_all_events(&mut d)
    }

    fn latm_framing_on_0x101(ev: &[DemuxEvent]) -> bool {
        issues(ev)
            .iter()
            .any(|i| matches!(i, NonConformantIssue::LatmFraming { pid: 0x101, .. }))
    }

    /// LATM 0x101 with pes_cap_per_pid 300. PES 1 leaves a Body(33) carry. PES 2
    /// (400 bytes) overflows the cap and is dropped. PES 3 starts on a record
    /// boundary; the stale carry would walk it from byte 33.
    #[test]
    fn pes_cap_overflow_forgets_the_loas_carry() {
        let ev = latm_cap_overflow_events(true, &loas_record(40));
        assert!(
            ev.iter().any(|e| matches!(
                e,
                DemuxEvent::Discontinuity {
                    kind: DiscontinuityKind::PesOversize { pid: 0x101 },
                    ..
                }
            )),
            "{ev:?}"
        );
        assert!(!latm_framing_on_0x101(&ev), "{ev:?}");
    }

    /// After the overflow a PES that opens inside a record (DAI = 0, no syncword at
    /// 0) takes the resync path, not the carry path: still no LatmFraming.
    #[test]
    fn pes_cap_overflow_then_unaligned_pes_resyncs_instead_of_walking_a_stale_carry() {
        // 7 bytes of record tail (no 0x56), then a whole record: resync
        // confirms the record at offset 7.
        let mut p3 = vec![0u8; 7];
        p3.extend(loas_record(40));
        let ev = latm_cap_overflow_events(true, &p3);
        assert!(!latm_framing_on_0x101(&ev), "{ev:?}");
    }

    /// H.222.0 §2.4.3.5: a discontinuity_indicator starts a new source, so
    /// the old source's LOAS carry (Body(33) after PES 1) must not walk the
    /// new source's first PES, whether the flag rides that PES's first
    /// packet or an adaptation-only packet before it.
    #[test]
    fn a_signalled_discontinuity_forgets_the_loas_carry() {
        let record_b = loas_record(50);
        let mut p1 = loas_record(10);
        p1.extend_from_slice(&record_b[..20]);
        let new_source = pes_bytes(0xC0, Some(90_000), None, false, &loas_record(40), true);
        for af_only in [false, true] {
            let mut d = demuxer_with_streams(
                DemuxerConfig::builder().build(),
                0x101,
                &[(0x11, 0x101, &[])],
            );
            let mut cc = 0;
            let mut pkts = latm_pes(&mut cc, &p1);
            if af_only {
                pkts.push(ts_packet(&PktSpec {
                    pid: 0x101,
                    cc: 0,
                    discontinuity: true,
                    ..Default::default()
                }));
            }
            pkts.push(ts_packet(&PktSpec {
                pid: 0x101,
                cc: 9,
                pusi: true,
                discontinuity: !af_only,
                payload: &new_source,
                ..Default::default()
            }));
            feed_packets(&mut d, &pkts).unwrap();
            d.flush();
            let ev = drain_all_events(&mut d);
            assert!(!latm_framing_on_0x101(&ev), "af_only {af_only}: {ev:?}");
        }
    }

    /// The discontinuity_indicator rides a continuation packet of an intact
    /// PES A, which completes there and is walked on the old source's
    /// framing: its carry (Body(103)) must still be forgotten before the new
    /// source's first PES, which opens on a record.
    #[test]
    fn a_discontinuity_inside_a_pes_forgets_the_carry_that_pes_leaves() {
        let mut p_a = loas_record(10);
        p_a.extend_from_slice(&loas_record(400)[..300]);
        let mut p_b = loas_record(40);
        p_b.extend(loas_record(200));
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x101,
            &[(0x11, 0x101, &[])],
        );
        let mut cc = 0;
        let mut pkts = latm_pes(&mut cc, &p_a);
        assert_eq!(pkts.len(), 2);
        // Rebuild A's second packet with the flag: its payload follows
        // `adaptation_field_length` + 1 bytes.
        let tail = pkts[1];
        let payload_start = 5 + usize::from(tail[4]);
        pkts[1] = ts_packet(&PktSpec {
            pid: 0x101,
            cc: tail[3] & 0x0F,
            discontinuity: true,
            payload: &tail[payload_start..],
            ..Default::default()
        });
        pkts.extend(latm_pes(&mut cc, &p_b));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(!latm_framing_on_0x101(&ev), "{ev:?}");
    }

    /// LATM 0x101 under `StrictMode::Full`: feed `payloads` as unaligned
    /// PES; the feed result and the events.
    fn latm_full_mode_events(payloads: &[Vec<u8>]) -> (Result<(), DemuxError>, Vec<DemuxEvent>) {
        let mut d = demuxer_with_streams(
            DemuxerConfig::builder().strict(StrictMode::Full).build(),
            0x101,
            &[(0x11, 0x101, &[])],
        );
        let mut cc = 0;
        let mut pkts = Vec::new();
        for payload in payloads {
            pkts.extend(latm_pes(&mut cc, payload));
        }
        let r = feed_packets(&mut d, &pkts);
        d.flush();
        (r, drain_all_events(&mut d))
    }

    /// A PID's first PES opens in a record's 100-byte tail holding a false
    /// syncword pair, then nine whole records: the refuted pair does not
    /// hide them, so the PES counts no unsynced bytes and is delivered.
    #[test]
    fn a_refuted_candidate_does_not_hide_the_records_after_it() {
        let mut p = vec![0x00; 100];
        p[4..7].copy_from_slice(&[0x56, 0xE0, 0x02]);
        for _ in 0..9 {
            p.extend(loas_record(1000));
        }
        let (r, ev) = latm_full_mode_events(&[p]);
        assert!(r.is_ok(), "{r:?}");
        assert!(!latm_framing_on_0x101(&ev), "{ev:?}");
        assert_eq!(sample_count(&ev, 0x101), 1, "{ev:?}");
    }

    /// A tentative candidate is refuted by the next PES, either where its
    /// record ends (a `Body` carry) or as its header completes (a `Header`
    /// carry). That PES is resynced: its own record is found, so the carry
    /// it leaves covers the PES after it, and the 8 000 candidate-free bytes
    /// counted before the false candidate do not reach the
    /// `MissingSyncword` threshold.
    #[test]
    fn a_refuted_tentative_candidate_resyncs_the_pes_that_refutes_it() {
        // 0x56 0xE5 0x00: a 1283-byte record from offset 50, past the PES.
        let mut body_false = vec![0x00; 50];
        body_false.extend_from_slice(&[0x56, 0xE5, 0x00]);
        body_false.resize(300, 0x00);
        // A lone 0x56 last: a header continued into the next PES.
        let mut header_false = vec![0x00; 300];
        header_false[299] = 0x56;
        // No syncword where the false record ends (1033) or as the header
        // completes; a record at 1040, then one that runs 303 bytes on.
        let mut refuting = vec![0x00; 1040];
        refuting.extend(loas_record(20));
        refuting.extend_from_slice(&loas_record(400)[..100]);
        for (name, false_pes) in [("body", body_false), ("header", header_false)] {
            let payloads = [
                vec![0x00; 8000],
                false_pes,
                refuting.clone(),
                vec![0x00; 300],
            ];
            let (r, ev) = latm_full_mode_events(&payloads);
            assert!(r.is_ok(), "{name}: {r:?}");
            assert!(!latm_framing_on_0x101(&ev), "{name}: {ev:?}");
            assert_eq!(sample_count(&ev, 0x101), 4, "{name}: {ev:?}");
        }
    }

    /// The aggregate `pes_cap_total` drop loses the PES the same way.
    #[test]
    fn pes_cap_total_overflow_forgets_the_loas_carry() {
        let ev = latm_cap_overflow_events(false, &loas_record(40));
        assert!(
            ev.iter().any(|e| matches!(
                e,
                DemuxEvent::Discontinuity {
                    kind: DiscontinuityKind::PesTotalOversize,
                    ..
                }
            )),
            "{ev:?}"
        );
        assert!(!latm_framing_on_0x101(&ev), "{ev:?}");
    }

    fn latm_framing_count(ev: &[DemuxEvent]) -> usize {
        issues(ev)
            .iter()
            .filter(|i| matches!(i, NonConformantIssue::LatmFraming { pid: 0x104, .. }))
            .count()
    }

    fn sample_count(ev: &[DemuxEvent], pid: u16) -> usize {
        ev.iter()
            .filter(|e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == pid))
            .count()
    }

    /// ISO/IEC 14496-3 §1.7.2: an AudioSyncStream record may continue into the
    /// next PES; data_alignment_indicator = 0 announces that.
    #[test]
    fn latm_record_split_across_two_pes_is_not_flagged() {
        let mut d = latm_demuxer();
        let record_b = loas_record(50);
        let mut es1 = loas_record(10);
        es1.extend_from_slice(&record_b[..20]);
        let mut es2 = record_b[20..].to_vec();
        es2.extend_from_slice(&loas_record(8));
        let mut pkts = pes_to_packets(
            0x104,
            0,
            &pes_bytes(0xC0, Some(90_000), None, true, &es1, true),
        );
        pkts.extend(pes_to_packets(
            0x104,
            1,
            &pes_bytes(0xC0, None, None, false, &es2, true),
        ));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(latm_framing_count(&ev), 0, "{ev:?}");
        assert_eq!(sample_count(&ev, 0x104), 2, "{ev:?}");
    }

    /// H.222.0 §2.4.3.3: after a continuity_counter gap the bytes a split
    /// record still owed are unknown, so a PES that starts mid-record with
    /// data_alignment_indicator = 0 is not walked.
    #[test]
    fn latm_continuation_after_a_cc_gap_is_not_validated() {
        let mut d = latm_demuxer();
        let mut es1 = loas_record(10);
        es1.extend_from_slice(&loas_record(50)[..20]);
        // The lost packet (CC 1) held a whole PES; PES 2 continues the
        // record it began with 43 body bytes, then one whole record.
        let mut es2 = vec![0x00; 43];
        es2.extend_from_slice(&loas_record(8));
        let mut pkts = pes_to_packets(
            0x104,
            0,
            &pes_bytes(0xC0, Some(90_000), None, true, &es1, true),
        );
        pkts.extend(pes_to_packets(
            0x104,
            2,
            &pes_bytes(0xC0, None, None, false, &es2, true),
        ));
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(latm_framing_count(&ev), 0, "{ev:?}");
        assert_eq!(sample_count(&ev, 0x104), 2, "{ev:?}");
    }

    /// A syncword candidate scanned after a gap is trusted only when a whole
    /// record follows it inside the PES: a false `56 E0` pair in a record's
    /// tail, whose bogus length runs past the PES, must not become the carry.
    #[test]
    fn false_sync_in_a_continuation_after_a_cc_gap_is_not_trusted() {
        let mut d = latm_demuxer();
        let mut es1 = loas_record(10);
        es1.extend_from_slice(&loas_record(200)[..20]);
        // PES 2 (after the lost CC 1): 30-byte tail of a record whose start
        // was lost, holding a false sync pair (length 0xFF), then a record.
        let mut es2 = vec![0x00; 30];
        es2[5..8].copy_from_slice(&[0x56, 0xE0, 0xFF]);
        es2.extend_from_slice(&loas_record(8));
        let es3 = loas_record(300);
        let es4 = loas_record(8);
        let mut pkts = pes_to_packets(
            0x104,
            0,
            &pes_bytes(0xC0, Some(90_000), None, true, &es1, true),
        );
        for (cc, es) in [(2, &es2), (3, &es3), (5, &es4)] {
            pkts.extend(pes_to_packets(
                0x104,
                cc,
                &pes_bytes(0xC0, None, None, false, es, true),
            ));
        }
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(latm_framing_count(&ev), 0, "{ev:?}");
        assert_eq!(sample_count(&ev, 0x104), 4, "{ev:?}");
    }

    /// ISO/IEC 14496-3 §1.7.2: the 3-byte LOAS header itself may be split
    /// across PES; the record it opens still owes the following PES.
    #[test]
    fn latm_header_split_across_pes_is_not_flagged() {
        let mut b = DemuxerConfig::builder();
        b.strict(StrictMode::Full);
        let mut d = demuxer_with_streams(b.build(), 0x104, &[(0x11, 0x104, &[])]);
        let record = loas_record(50);
        let mut pkts = pes_to_packets(
            0x104,
            0,
            &pes_bytes(0xC0, Some(90_000), None, true, &record[..2], true),
        );
        for (cc, es) in [(1, &record[2..23]), (2, &record[23..])] {
            pkts.extend(pes_to_packets(
                0x104,
                cc,
                &pes_bytes(0xC0, None, None, false, es, true),
            ));
        }
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(latm_framing_count(&ev), 0, "{ev:?}");
        assert_eq!(sample_count(&ev, 0x104), 3, "{ev:?}");
    }

    /// A record-boundary tail that cannot open a LOAS header is a framing
    /// error; outside `Full` it is reported and the next PES resyncs.
    #[test]
    fn latm_tail_without_sync_is_reported_and_resynced_outside_full() {
        for mode in [
            StrictMode::Off,
            StrictMode::TimingOnly,
            StrictMode::DescriptorsOnly,
        ] {
            let mut b = DemuxerConfig::builder();
            b.strict(mode);
            let mut d = demuxer_with_streams(b.build(), 0x104, &[(0x11, 0x104, &[])]);
            let mut es1 = loas_record(10);
            es1.extend_from_slice(&[0x56, 0x00]);
            let mut es2 = loas_record(8);
            es2.extend_from_slice(&loas_record(4));
            let mut pkts = pes_to_packets(
                0x104,
                0,
                &pes_bytes(0xC0, Some(90_000), None, true, &es1, true),
            );
            pkts.extend(pes_to_packets(
                0x104,
                1,
                &pes_bytes(0xC0, None, None, false, &es2, true),
            ));
            feed_packets(&mut d, &pkts).unwrap();
            d.flush();
            let ev = drain_all_events(&mut d);
            assert_eq!(latm_framing_count(&ev), 1, "{mode:?}: {ev:?}");
            assert_eq!(sample_count(&ev, 0x104), 2, "{mode:?}: {ev:?}");
        }
    }

    #[test]
    fn latm_aligned_pes_without_sync_is_still_flagged() {
        let mut d = latm_demuxer();
        let pkts = pes_to_packets(
            0x104,
            0,
            &pes_bytes(0xC0, Some(90_000), None, true, &[0x00, 0x11, 0x22], true),
        );
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert!(
            issues(&ev).iter().any(|i| matches!(
                i,
                NonConformantIssue::LatmFraming {
                    pid: 0x104,
                    kind: crate::codec::aac::latm::LatmFramingKind::MissingSyncword
                }
            )),
            "{ev:?}"
        );
    }

    fn dvb_sub_demuxer() -> Demuxer {
        // subtitling_descriptor: "eng", type 0x10, composition page 1, ancillary page 1.
        let subtitling: &[u8] = &[0x59, 0x08, b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01];
        demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x102,
            &[(0x06, 0x102, subtitling)],
        )
    }

    fn subtitle_payload(ev: &[DemuxEvent], pid: u16) -> Option<Vec<u8>> {
        ev.iter().find_map(|e| match e {
            DemuxEvent::Sample {
                stream,
                payload: SamplePayload::Subtitle { payload, .. },
                ..
            } if stream.pid == pid => Some(payload.as_slice().to_vec()),
            _ => None,
        })
    }

    fn malformed_index(ev: &[DemuxEvent], pid: u16, want: &str) -> Option<usize> {
        ev.iter().position(|e| {
            matches!(e, DemuxEvent::NonConformant {
                issue: NonConformantIssue::MalformedPes { pid: p, reason }, ..
            } if *p == pid && *reason == want)
        })
    }

    /// EN 300 743 Table 3 envelope violations: wrong data_identifier, wrong
    /// subtitle_stream_id, missing end marker.
    #[test]
    fn malformed_dvb_sub_envelopes_raise_malformed_pes_and_keep_the_raw_sample() {
        for payload in [
            &[0x40u8, 0x00, 0x0F, 0xFF][..],
            &[0x20, 0x01, 0x0F, 0xFF],
            &[0x20, 0x00, 0x0F, 0x00],
        ] {
            let mut d = dvb_sub_demuxer();
            let pkts = pes_to_packets(
                0x102,
                0,
                &pes_bytes(0xBD, Some(90_000), None, true, payload, true),
            );
            feed_packets(&mut d, &pkts).unwrap();
            d.flush();
            let ev = drain_all_events(&mut d);
            let marker = malformed_index(&ev, 0x102, DVB_SUB_ENVELOPE_MALFORMED);
            let sample = ev.iter().position(
                |e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x102),
            );
            assert!(
                matches!((marker, sample), (Some(m), Some(s)) if m < s),
                "{payload:02X?}: {ev:?}"
            );
            assert_eq!(
                subtitle_payload(&ev, 0x102).as_deref(),
                Some(payload),
                "{ev:?}"
            );
            assert!(!is_damage_reason(DVB_SUB_ENVELOPE_MALFORMED));
        }
    }

    fn teletext_demuxer() -> Demuxer {
        // teletext_descriptor: "eng", type 0x02 (subtitle page), magazine 8, page 0x88.
        let teletext: &[u8] = &[0x56, 0x05, b'e', b'n', b'g', 0x10, 0x88];
        demuxer_with_streams(
            DemuxerConfig::builder().build(),
            0x103,
            &[(0x06, 0x103, teletext)],
        )
    }

    /// A teletext PES with `header_data_length` header bytes (PTS, then 0xFF
    /// stuffing) and `data_field`, padded with 0xFF to a multiple of 184 bytes.
    fn teletext_pes(header_data_length: usize, data_field: &[u8]) -> Vec<u8> {
        let mut pes = pes_bytes(0xBD, Some(90_000), None, true, &[], true);
        pes[8] = header_data_length as u8;
        pes.resize(9 + header_data_length, 0xFF);
        pes.extend_from_slice(data_field);
        let total = pes.len().div_ceil(184) * 184;
        pes.resize(total, 0xFF);
        pes[4..6].copy_from_slice(&((total - 6) as u16).to_be_bytes());
        pes
    }

    /// data_identifier, then one data unit of `unit_id` and `unit_length`.
    fn teletext_data_field(data_identifier: u8, unit_id: u8, unit_length: u8) -> Vec<u8> {
        let mut field = vec![data_identifier, unit_id, unit_length];
        field.resize(3 + usize::from(unit_length), 0x00);
        field
    }

    fn feed_teletext(pes: &[u8]) -> Vec<DemuxEvent> {
        let mut d = teletext_demuxer();
        feed_packets(&mut d, &pes_to_packets(0x103, 0, pes)).unwrap();
        d.flush();
        drain_all_events(&mut d)
    }

    /// EN 300 472 §4.2: the PES header is 45 bytes and the PES is N × 184 bytes,
    /// so payload.len() + 45 is a multiple of 184; §4.3: data_identifier
    /// 0x10..=0x1F (or user-defined 0x80..=0xFF), data_unit_id 0x02/0x03/0xFF,
    /// data_unit_length 0x2C for 0x02/0x03.
    #[test]
    fn teletext_with_short_header_or_bad_unit_length_raises_malformed_pes() {
        for pes in [
            teletext_pes(5, &teletext_data_field(0x10, 0x03, 0x2C)),
            teletext_pes(0x24, &teletext_data_field(0x10, 0x03, 0x20)),
            teletext_pes(0x24, &teletext_data_field(0x40, 0x03, 0x2C)),
        ] {
            let ev = feed_teletext(&pes);
            assert!(
                malformed_index(&ev, 0x103, TELETEXT_DATA_FIELD_MALFORMED).is_some(),
                "{ev:?}"
            );
            assert_eq!(sample_count(&ev, 0x103), 1, "{ev:?}");
        }
        assert!(!is_damage_reason(TELETEXT_DATA_FIELD_MALFORMED));
    }

    #[test]
    fn conformant_teletext_pes_raises_nothing() {
        let ev = feed_teletext(&teletext_pes(0x24, &teletext_data_field(0x10, 0x03, 0x2C)));
        assert!(issues(&ev).is_empty(), "{ev:?}");
        assert_eq!(sample_count(&ev, 0x103), 1, "{ev:?}");
    }

    fn adaptation_only_stream(
        flag: bool,
        af_cc: u8,
        pes2_cc: u8,
    ) -> (Vec<[u8; 188]>, impl Fn(StrictMode) -> Demuxer) {
        let video = |mode| {
            demuxer_with_streams(
                DemuxerConfig::builder().strict(mode).build(),
                0x100,
                &[(0x02, 0x101, &[])],
            )
        };
        let mut pkts = pes_to_packets(
            0x101,
            0,
            &pes_bytes(0xE0, Some(90_000), None, true, &[0xAA; 600], false),
        );
        pkts.push(ts_packet(&PktSpec {
            pid: 0x101,
            cc: af_cc,
            pusi: false,
            discontinuity: flag,
            rai: false,
            pcr: None,
            scrambling: 0,
            payload: &[],
        }));
        pkts.extend(pes_to_packets(
            0x101,
            pes2_cc,
            &pes_bytes(0xE0, Some(93_000), None, true, &[0xBB; 100], false),
        ));
        (pkts, video)
    }

    fn count_flag_and_jump(ev: &[DemuxEvent]) -> (usize, usize) {
        let n = |want_flag: bool| {
            ev.iter()
                .filter(|e| match e {
                    DemuxEvent::Discontinuity { kind, .. } => match kind {
                        DiscontinuityKind::AdaptationFieldFlag => want_flag,
                        DiscontinuityKind::ContinuityJump { .. } => !want_flag,
                        _ => false,
                    },
                    _ => false,
                })
                .count()
        };
        (n(true), n(false))
    }

    /// Video 0x101 on a program whose PCR_PID is 0x100: unbounded PES at CC 0..3,
    /// an adaptation-only packet with discontinuity_indicator and CC 10, then
    /// the next PES at CC 11. H.222.0 §2.4.3.5 permits that reset. stream_type
    /// 0x02 (MPEG-2 video) is `StreamKind::Unknown(0x02)`: a video PID whose
    /// payload tst-core does not parse, so the 0xAA filler raises no codec issue
    /// and the PES surfaces as `SamplePayload::Unknown`.
    #[test]
    fn flagged_adaptation_only_packet_resets_the_continuity_baseline() {
        let (pkts, video) = adaptation_only_stream(true, 10, 11);
        let mut d = video(StrictMode::Off);
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(count_flag_and_jump(&ev), (1, 0), "{ev:?}");
        assert!(
            !issues(&ev)
                .iter()
                .any(|i| matches!(i, NonConformantIssue::MalformedPes { .. })),
            "{ev:?}"
        );
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x101))
                .count(),
            2,
            "{ev:?}"
        );
        let mut strict = video(StrictMode::Full);
        assert!(
            feed_packets(&mut strict, &pkts).is_ok(),
            "Full must not reject a permitted reset"
        );
    }

    /// Without the flag an adaptation-only packet's counter is simply not compared
    /// (§2.4.3.3: not incremented on such packets); the baseline stays CC 3.
    #[test]
    fn adaptation_only_packet_without_the_flag_leaves_the_baseline_alone() {
        let (pkts, video) = adaptation_only_stream(false, 7, 4);
        let mut d = video(StrictMode::Off);
        feed_packets(&mut d, &pkts).unwrap();
        d.flush();
        let ev = drain_all_events(&mut d);
        assert_eq!(count_flag_and_jump(&ev), (0, 0), "{ev:?}");
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x101))
                .count(),
            2,
            "{ev:?}"
        );
    }

    /// AU cells on synchronous KLV PID 0x101 (stream_type 0x15).
    mod au_in_progress {
        use super::*;
        use crate::mpegts::au_cell::{
            AuCellHeader, CellFragmentIndication, write_metadata_au_cell,
        };
        use crate::mpegts::demux::event::MultiCellAuReason;
        use CellFragmentIndication::{Complete, First, Last, Middle};

        fn klv_demuxer(mode: StrictMode) -> Demuxer {
            let mut b = DemuxerConfig::builder();
            b.strict(mode);
            let mut d = demuxer_with_streams(b.build(), 0x1FFF, &[(0x15, 0x101, &[])]);
            drain_all_events(&mut d);
            d
        }

        /// One PES (stream_id 0xFC, PTS 90 000) carrying `cells` back to back.
        fn cells_pes(cc: u8, cells: &[(CellFragmentIndication, u8, &[u8])]) -> Vec<[u8; 188]> {
            let mut body = Vec::new();
            for &(cfi, seq, inner) in cells {
                let header = AuCellHeader {
                    metadata_service_id: 0,
                    sequence_number: seq,
                    cell_fragment_indication: cfi,
                    decoder_config_flag: false,
                    random_access_indicator: true,
                };
                write_metadata_au_cell(&mut body, header, inner).unwrap();
            }
            pes_to_packets(
                0x101,
                cc,
                &pes_bytes(0xFC, Some(90_000), None, true, &body, true),
            )
        }

        fn metadata_count(ev: &[DemuxEvent]) -> usize {
            ev.iter()
                .filter(|e| matches!(e, DemuxEvent::Metadata { .. }))
                .count()
        }

        fn sequence_gaps(ev: &[DemuxEvent]) -> Vec<usize> {
            issues(ev)
                .iter()
                .filter_map(|i| match i {
                    NonConformantIssue::MultiCellAu {
                        pid: 0x101,
                        dropped_bytes,
                        reason: MultiCellAuReason::SequenceGap,
                    } => Some(*dropped_bytes),
                    _ => None,
                })
                .collect()
        }

        fn run(mode: StrictMode, pkts: &[[u8; 188]]) -> Vec<DemuxEvent> {
            let mut d = klv_demuxer(mode);
            let r = feed_packets(&mut d, pkts);
            assert!(r.is_ok(), "{mode:?}: {r:?}");
            d.flush();
            drain_all_events(&mut d)
        }

        /// PES A carries First 10 (filling its first packet) and Middle 11 in
        /// its second packet, which is lost; PES B carries Last 12. The jump
        /// is seen on B's PUSI, before A is flushed: the First that A's walk
        /// buffers must not join B's Last.
        #[test]
        fn a_lost_tail_packet_discards_the_au_in_progress() {
            // 184 payload bytes in the first packet: a 14-byte PES header
            // and a 5 + 165-byte First cell.
            let a = cells_pes(0, &[(First, 10, &[0xAA; 165]), (Middle, 11, &[0xBB; 20])]);
            assert_eq!(a.len(), 2);
            let mut pkts = vec![a[0]];
            pkts.extend(cells_pes(2, &[(Last, 12, b"CCC")]));
            for mode in [StrictMode::Off, StrictMode::TimingOnly] {
                let ev = run(mode, &pkts);
                assert_eq!(metadata_count(&ev), 0, "{mode:?}: {ev:?}");
            }
            let ev = run(StrictMode::Off, &pkts);
            assert_eq!(sequence_gaps(&ev), vec![165], "{ev:?}");
        }

        /// The same AU when PES A is closed short without a continuity gap
        /// (its PES_packet_length promised a second packet that never came):
        /// the damaged PES discards the AU in progress whether it is walked
        /// (Off) or dropped (TimingOnly).
        #[test]
        fn a_short_pes_discards_the_au_in_progress() {
            let mut pkts = cells_pes(0, &[(First, 10, b"AAA")]);
            let short = cells_pes(1, &[(Middle, 11, &[0xBB; 165]), (Middle, 12, &[0xBB; 20])]);
            pkts.push(short[0]);
            pkts.extend(cells_pes(2, &[(Last, 13, b"CCC")]));
            for mode in [StrictMode::Off, StrictMode::TimingOnly] {
                let ev = run(mode, &pkts);
                assert_eq!(metadata_count(&ev), 0, "{mode:?}: {ev:?}");
                assert!(!sequence_gaps(&ev).is_empty(), "{mode:?}: {ev:?}");
            }
        }

        /// H.222.0 §2.4.3.5: a discontinuity_indicator starts a new source on
        /// the PID, so an AU in progress from the old one is discarded and
        /// reported, whether the flag rides the new PES's first packet or an
        /// adaptation-only packet before it.
        #[test]
        fn a_signalled_discontinuity_discards_the_au_in_progress() {
            let a = cells_pes(0, &[(First, 10, b"AAA")]);
            let mut b = cells_pes(1, &[(Last, 11, b"CCC")]);
            // On B's PUSI packet: rebuild it with the flag set.
            let mut flagged = a.clone();
            // B fits one packet stuffed by its adaptation field: the payload
            // follows `adaptation_field_length` + 1 bytes.
            let payload_start = 5 + usize::from(b[0][4]);
            flagged.push(ts_packet(&PktSpec {
                pid: 0x101,
                cc: 7,
                pusi: true,
                discontinuity: true,
                payload: &b[0][payload_start..],
                ..Default::default()
            }));
            // On an adaptation-only packet between A and B.
            let mut af_only = a;
            af_only.push(ts_packet(&PktSpec {
                pid: 0x101,
                cc: 0,
                discontinuity: true,
                ..Default::default()
            }));
            b[0][3] = (b[0][3] & 0xF0) | 9;
            af_only.push(b[0]);
            for (name, pkts) in [("pusi", flagged), ("adaptation-only", af_only)] {
                let ev = run(StrictMode::Off, &pkts);
                assert_eq!(metadata_count(&ev), 0, "{name}: {ev:?}");
                assert_eq!(sequence_gaps(&ev), vec![3], "{name}: {ev:?}");
            }
        }

        /// `klv_demuxer` with a per-PID PES cap.
        fn klv_demuxer_capped(cap: usize) -> Demuxer {
            let mut b = DemuxerConfig::builder();
            b.pes_cap_per_pid(cap);
            let mut d = demuxer_with_streams(b.build(), 0x1FFF, &[(0x15, 0x101, &[])]);
            drain_all_events(&mut d);
            d
        }

        /// First 10 in P0, Last 12 in P2, and P1 lost on the way without a
        /// continuity gap: dropped by the PES cap, unparseable, zero-length
        /// on a non-video PID, or ending in a truncated cell. Each loss
        /// discards the AU in progress: no spliced Metadata, a gap report.
        #[test]
        fn a_pes_lost_without_a_continuity_gap_discards_the_au_in_progress() {
            let first = cells_pes(0, &[(First, 10, b"AAA")]);
            let one = |payload: &[u8]| {
                vec![ts_packet(&PktSpec {
                    pid: 0x101,
                    cc: 1,
                    pusi: true,
                    payload,
                    ..Default::default()
                })]
            };
            // PES_header_data_length 0xFF past the 8 bytes PES_packet_length
            // declares.
            let unparseable = one(&[
                0x00, 0x00, 0x01, 0xFC, 0x00, 0x08, 0x80, 0x80, 0xFF, 0, 0, 0, 0, 0,
            ]);
            // H.222.0 §2.4.3.7: PES_packet_length 0 is legal only for video.
            let zero_length = one(&[0x00, 0x00, 0x01, 0xFC, 0x00, 0x00, 0x80, 0x00, 0x00, 0xAB]);
            // A whole Middle 11 cell, then a Middle 12 cell whose
            // AU_cell_data_length runs past the PES (a valid first cell keeps
            // the PES on the AU-cell path).
            let mut truncated = Vec::new();
            for (seq, inner) in [(11u8, &b"BBB"[..]), (12, &[0xBB; 40][..])] {
                write_metadata_au_cell(
                    &mut truncated,
                    AuCellHeader {
                        metadata_service_id: 0,
                        sequence_number: seq,
                        cell_fragment_indication: Middle,
                        decoder_config_flag: false,
                        random_access_indicator: true,
                    },
                    inner,
                )
                .unwrap();
            }
            truncated.truncate(8 + 20);
            let truncated = pes_to_packets(
                0x101,
                1,
                &pes_bytes(0xFC, Some(90_000), None, true, &truncated, true),
            );
            let oversize = cells_pes(1, &[(Middle, 11, &[0xBB; 400])]);
            // Bytes dropped: First 10, plus Middle 11 when the walk reached it.
            for (name, middle, capped, dropped) in [
                ("pes cap", oversize, true, 3),
                ("unparseable", unparseable, false, 3),
                ("zero length", zero_length, false, 3),
                ("truncated cell", truncated, false, 6),
            ] {
                let mut d = if capped {
                    klv_demuxer_capped(300)
                } else {
                    klv_demuxer(StrictMode::Off)
                };
                // P2's counter follows P1's packets: no continuity gap.
                let last = cells_pes(1 + middle.len() as u8, &[(Last, 12, b"CCC")]);
                let mut pkts = first.clone();
                pkts.extend(middle);
                pkts.extend(last);
                let r = feed_packets(&mut d, &pkts);
                assert!(r.is_ok(), "{name}: {r:?}");
                d.flush();
                let ev = drain_all_events(&mut d);
                assert_eq!(metadata_count(&ev), 0, "{name}: {ev:?}");
                assert_eq!(sequence_gaps(&ev), vec![dropped], "{name}: {ev:?}");
            }
        }

        /// A PES on the synchronous-KLV PID that is not walked as AU cells
        /// (a bare KLV record, or bytes of neither shape) sits between First
        /// 10 and Last 12: the AU cannot be known to continue across it, so
        /// it is discarded and reported, and no spliced AU is delivered.
        #[test]
        fn a_pes_not_walked_as_cells_discards_the_au_in_progress() {
            // SMPTE 336M UL prefix + BER length 1 + one byte: async shape.
            let mut bare_klv = vec![
                0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
                0x00, 0x00,
            ];
            bare_klv.extend_from_slice(&[0x01, 0xAB]);
            let other = vec![0xFFu8; 12];
            for (name, body) in [("async", bare_klv), ("other", other)] {
                let mut pkts = cells_pes(0, &[(First, 10, b"AAA")]);
                let middle = pes_to_packets(
                    0x101,
                    1,
                    &pes_bytes(0xFC, Some(90_000), None, true, &body, true),
                );
                let next = 1 + middle.len() as u8;
                pkts.extend(middle);
                pkts.extend(cells_pes(next, &[(Last, 12, b"CCC")]));
                let ev = run(StrictMode::Off, &pkts);
                let spliced = ev.iter().any(|e| {
                    matches!(
                        e,
                        DemuxEvent::Metadata {
                            kind: MetadataKind::KlvSyncAuCell { .. },
                            ..
                        }
                    )
                });
                assert!(!spliced, "{name}: {ev:?}");
                assert_eq!(sequence_gaps(&ev), vec![3], "{name}: {ev:?}");
            }
        }

        /// PES A opens with a heavily stuffed PUSI packet carrying fewer
        /// than a PES header's bytes, and its later packets are lost. A can
        /// yield neither a Complete nor a Malformed, so the continuity jump
        /// itself discards the AU in progress.
        #[test]
        fn a_lost_pes_shorter_than_a_header_discards_the_au_in_progress() {
            let mut pkts = cells_pes(0, &[(First, 10, b"AAA")]);
            pkts.push(ts_packet(&PktSpec {
                pid: 0x101,
                cc: 1,
                pusi: true,
                payload: &[0x00, 0x00, 0x01, 0xFC],
                ..Default::default()
            }));
            // CC 2 and 3 lost.
            pkts.extend(cells_pes(4, &[(Last, 12, b"CCC")]));
            let ev = run(StrictMode::Off, &pkts);
            assert_eq!(metadata_count(&ev), 0, "{ev:?}");
            assert_eq!(sequence_gaps(&ev), vec![3], "{ev:?}");
        }

        /// A PES shorter than a PES header, with no continuity gap, sits
        /// between First 10 and Last 12: it carried no cell, so the AU
        /// cannot be known to continue across it and is discarded.
        #[test]
        fn a_pes_shorter_than_a_header_discards_the_au_in_progress() {
            let mut pkts = cells_pes(0, &[(First, 10, b"AAA")]);
            pkts.push(ts_packet(&PktSpec {
                pid: 0x101,
                cc: 1,
                pusi: true,
                payload: &[0x00, 0x00, 0x01, 0xFC],
                ..Default::default()
            }));
            pkts.extend(cells_pes(2, &[(Last, 12, b"CCC")]));
            for mode in [StrictMode::Off, StrictMode::TimingOnly] {
                let ev = run(mode, &pkts);
                assert_eq!(metadata_count(&ev), 0, "{mode:?}: {ev:?}");
                assert_eq!(sequence_gaps(&ev), vec![3], "{mode:?}: {ev:?}");
            }
        }

        /// PES A's header packet arrives, the packet with its Middle 11 is
        /// lost, and the packet with its Last 12 arrives. The loss precedes
        /// A's first surviving cell, so that Last must not complete the
        /// First 10 held from the PES before.
        #[test]
        fn a_loss_before_the_first_surviving_cell_discards_the_au_in_progress() {
            let mut body = Vec::new();
            for (cfi, seq, inner) in [(Middle, 11u8, b"BBB"), (Last, 12, b"CCC")] {
                write_metadata_au_cell(
                    &mut body,
                    AuCellHeader {
                        metadata_service_id: 0,
                        sequence_number: seq,
                        cell_fragment_indication: cfi,
                        decoder_config_flag: false,
                        random_access_indicator: true,
                    },
                    inner,
                )
                .unwrap();
            }
            let a = pes_bytes(0xFC, Some(90_000), None, true, &body, true);
            // A 14-byte PES header, then two 8-byte cells.
            assert_eq!(a.len(), 14 + 16);
            let packet = |cc: u8, pusi: bool, payload: &[u8]| {
                ts_packet(&PktSpec {
                    pid: 0x101,
                    cc,
                    pusi,
                    payload,
                    ..Default::default()
                })
            };
            let mut pkts = cells_pes(0, &[(First, 10, b"AAA")]);
            pkts.push(packet(1, true, &a[..14]));
            // CC 2, carrying Middle 11, lost.
            pkts.push(packet(3, false, &a[22..]));
            for mode in [StrictMode::Off, StrictMode::TimingOnly] {
                let ev = run(mode, &pkts);
                assert_eq!(metadata_count(&ev), 0, "{mode:?}: {ev:?}");
            }
            let ev = run(StrictMode::Off, &pkts);
            assert_eq!(sequence_gaps(&ev), vec![3], "{ev:?}");
        }

        /// A continuity gap inside PES A, after its head: the head carries
        /// the AU's Last cell intact, so the AU is delivered, marked by the
        /// PES's `MalformedPes`, and no gap is reported.
        #[test]
        fn a_lost_tail_after_the_last_cell_still_delivers_the_au() {
            let first = cells_pes(0, &[(First, 10, b"AAA")]);
            // A: Last 11 in its first packet, then a Complete cell whose
            // tail is in the lost second packet.
            let a = cells_pes(1, &[(Last, 11, b"CCC"), (Complete, 12, &[0xDD; 200])]);
            assert_eq!(a.len(), 2);
            let mut pkts = first;
            pkts.push(a[0]);
            pkts.extend(cells_pes(3, &[(Complete, 13, b"E")]));
            let ev = run(StrictMode::Off, &pkts);
            let metas: Vec<(usize, Vec<u8>)> = ev
                .iter()
                .enumerate()
                .filter_map(|(i, e)| match e {
                    DemuxEvent::Metadata { payload, .. } => Some((i, payload.to_vec())),
                    _ => None,
                })
                .collect();
            assert_eq!(metas.len(), 2, "{ev:?}");
            assert_eq!(metas[0].1, b"AAACCC", "{ev:?}");
            assert!(
                matches!(
                    &ev[metas[0].0 - 1],
                    DemuxEvent::NonConformant {
                        issue: NonConformantIssue::MalformedPes { pid: 0x101, .. },
                        ..
                    }
                ),
                "{ev:?}"
            );
            assert!(sequence_gaps(&ev).is_empty(), "{ev:?}");
        }
    }

    /// A PID signalled only by VBI_teletext_descriptor (0x46) carrying a VBI data
    /// unit (id 0xC3, EN 301 775) is delivered, not reported as malformed.
    #[test]
    fn vbi_teletext_descriptor_only_pid_is_not_held_to_the_ebu_data_unit_rule() {
        let vbi: &[u8] = &[0x46, 0x05, b'e', b'n', b'g', 0x10, 0x88];
        let run = |mode| {
            let mut d = demuxer_with_streams(
                DemuxerConfig::builder().strict(mode).build(),
                0x103,
                &[(0x06, 0x103, vbi)],
            );
            let r = feed_packets(
                &mut d,
                &pes_to_packets(
                    0x103,
                    0,
                    &teletext_pes(36, &teletext_data_field(0x99, 0xC3, 0x2C)),
                ),
            );
            d.flush();
            (r, drain_all_events(&mut d))
        };
        let (r, ev) = run(StrictMode::Off);
        assert!(r.is_ok());
        assert!(
            malformed_index(&ev, 0x103, TELETEXT_DATA_FIELD_MALFORMED).is_none(),
            "{ev:?}"
        );
        assert!(
            ev.iter()
                .any(|e| matches!(e, DemuxEvent::Sample { stream, .. } if stream.pid == 0x103)),
            "{ev:?}"
        );
        let (r, _) = run(StrictMode::Full);
        assert!(
            r.is_ok(),
            "Full must not reject VBI data on a 0x46-only PID: {r:?}"
        );
    }
}
