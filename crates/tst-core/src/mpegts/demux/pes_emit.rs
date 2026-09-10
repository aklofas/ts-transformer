//! PES reassembly dispatch + complete-PES-to-DemuxEvent conversion.
//!
//! Hosts 2 helper methods on `Demuxer`:
//!
//! - `handle_pes_packet` — feeds a TS packet's PES bytes into the
//!   reassembler, surfaces oversize / total-cap discontinuities,
//!   converts complete PES results into events.
//! - `handle_complete_pes` — the central event-construction site.
//!   Dispatches by `StreamKind`; constructs `DemuxEvent::Sample` /
//!   `DemuxEvent::Metadata` with codec-specific payload shapes
//!   (raw AU pass-through for video — parsing into NAL/OBU units is the
//!   consumer's opt-in `split_video` call; envelope strip for DVB-sub;
//!   AU cell peel for sync KLV; raw pass-through for audio / async KLV /
//!   unknown).
//!
//! All items are `pub(super)`.

use crate::mpegts::common::{Pts90khz, StreamTypeCode, pts_diff_33bit};
use crate::mpegts::demux::event::{
    AudioCodec, DemuxEvent, DiscontinuityKind, MetadataKind, NonConformantIssue, SamplePayload,
    StreamId, StreamKind, SubtitleCodec, VideoCodec,
};
use crate::mpegts::demux::payload::{
    DvbSubStripResult, KlvShape, classify_klv, iter_au_cells, strip_dvb_sub_envelope,
};
use crate::mpegts::demux::pes::{PesPayload, ReassemblyOutcome};
use crate::shared::SharedBytes;
use alloc::string::ToString;
use alloc::vec::Vec;

/// True when H.222.0 §2.7.4 requires the PES to carry a PTS for this
/// stream kind. Audio and video have a mandatory PTS contract; subtitle,
/// KLV (sync via PTS-bearing AU cells, async without PTS), and unknown
/// stream types are optional/codec-defined.
fn stream_type_requires_pts(kind: &StreamKind) -> bool {
    matches!(kind, StreamKind::Video(_) | StreamKind::Audio(_))
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
        // REF-PES-01: zero PES_packet_length is legal only for video. Key the
        // decision on the PMT-declared stream type, not the PES stream_id — the
        // PID's kind is always known here (process_packet only routes to
        // handle_pes_packet when stream_kind_by_pid contains pkt.pid). A PID
        // tst-core recognizes as video is `StreamKind::Video(_)`; a video
        // codec it does NOT parse (MPEG-1/2/4) is `StreamKind::Unknown(t)` with
        // a video `stream_type` `t` — both are video elementary streams per
        // H.222.0 §2.4.3.7 and may carry an unbounded PES, so neither is
        // flagged. Everything else (audio/KLV/subtitle/non-video Unknown) is.
        let is_video = match self.stream_kind_by_pid.get(&pkt.pid) {
            Some(StreamKind::Video(_)) => true,
            Some(StreamKind::Unknown(stream_type)) => {
                super::pmt_classify::is_video_stream_type(*stream_type)
            }
            _ => false,
        };
        let outcomes = self.pes.push(
            pkt.pid,
            pkt.payload,
            pkt.payload_unit_start,
            pkt.random_access_indicator,
            is_video,
        )?;
        for outcome in outcomes {
            match outcome {
                ReassemblyOutcome::Complete(pes) => {
                    self.handle_complete_pes(pes);
                }
                ReassemblyOutcome::Overflow { pid } => {
                    if let Some(stream) = self.lookup_stream(pid) {
                        self.record_discontinuity(stream, DiscontinuityKind::PesOversize { pid });
                    }
                }
                ReassemblyOutcome::OverflowTotal => {
                    if let Some(stream) = self.lookup_stream(pkt.pid) {
                        self.record_discontinuity(stream, DiscontinuityKind::PesTotalOversize);
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
                }
            }
        }
        Ok(())
    }

    pub(super) fn handle_complete_pes(&mut self, pes: PesPayload) {
        let kind = match self.stream_kind_by_pid.get(&pes.pid).copied() {
            Some(k) => k,
            None => return,
        };
        let program_number = self.program_number_for_pid(pes.pid);
        let stream = StreamId {
            pid: pes.pid,
            kind,
            program_number,
        };
        // B5 — surface PES header structural issues collected during
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
        // B4 — PTS distinct from PCR. Only update `last_pts_by_pid`
        // when an actual PTS arrived; never write 0 as a fallback (the
        // prior code corrupted the monotonicity check for streams that
        // omit PTS sporadically). For stream types where H.222.0 §2.7.4
        // makes PTS mandatory (audio + video), emit
        // `MissingRequiredPts` when absent.
        let pts = pes.pts.unwrap_or(Pts90khz::new(0));
        if pes.pts.is_none() && stream_type_requires_pts(&kind) {
            self.queue_nonconformant(
                stream,
                NonConformantIssue::MissingRequiredPts { pid: pes.pid },
            );
        }
        if let Some(observed_pts) = pes.pts {
            if let Some(last) = self.last_pts_by_pid.get(&pes.pid).copied() {
                let delta = pts_diff_33bit(observed_pts.as_ticks() as u64, last as u64);
                if delta < -90_000 {
                    // PTS anomaly is its own variant (90 kHz / per-PID
                    // elementary stream), distinct from PcrAnomaly
                    // (27 MHz / per-program PCR PID).
                    self.queue_nonconformant(stream, NonConformantIssue::PtsAnomaly { delta });
                }
            }
            self.last_pts_by_pid
                .insert(pes.pid, observed_pts.as_ticks());
        }
        // Opt-in PTS/DTS unwrap (`DemuxerConfig::unwrap_timestamps`,
        // default off — the branch below is skipped entirely and `pts`/`dts`
        // stay exactly as computed above, so the default emit path is
        // byte-for-byte unchanged). `pts` and `dts` are shadowed here, once,
        // rather than at each event-construction site below — every event
        // this function pushes (`Sample` for every `StreamKind`, and every
        // `Metadata` KLV shape) reads from these same two locals, so shadowing
        // them here covers all of it. A synthesized `pts = 0` from the
        // missing-PTS branch above (`pes.pts.is_none()`) is never fed to the
        // accumulator and is left as 0.
        let (pts, dts) = if self.options.unwrap_timestamps {
            let unwrapped_pts = match pes.pts {
                Some(raw) => self.unwrap_pts(pes.pid, raw),
                None => pts,
            };
            // DTS unwraps relative to THIS PES's own raw PTS, not the
            // bare per-PID offset — the 33-bit boundary can fall between
            // one AU's DTS and PTS (DTS <= PTS always), and applying the
            // just-advanced offset directly would put a pre-wrap DTS a
            // full epoch too high. See `Self::unwrap_dts_with_pts`. The
            // DTS-with-no-PTS branch is spec-illegal but tolerated
            // defensively via the older bare-offset fallback.
            let unwrapped_dts = match (pes.pts, pes.dts) {
                (Some(pts_raw), Some(dts_raw)) => {
                    Some(Self::unwrap_dts_with_pts(unwrapped_pts, pts_raw, dts_raw))
                }
                (None, Some(dts_raw)) => Some(self.unwrap_secondary_ts(pes.pid, dts_raw)),
                (_, None) => None,
            };
            (unwrapped_pts, unwrapped_dts)
        } else {
            (pts, pes.dts)
        };
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
                    use crate::mpegts::mux::Av1CarriageMode;
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
                    crate::mpegts::stats::bump_video_counters(
                        &mut self.stream_codec_counters,
                        stream.pid,
                        0,
                        ra_count,
                    );
                }
                self.queue.push_back(DemuxEvent::Sample {
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
                });
            }
            StreamKind::KlvSync { .. } | StreamKind::KlvAsync => {
                let shape = classify_klv(&pes.payload);

                // Async-shape PIDs (bare SMPTE UL) — bypass the AU cell
                // reassembler entirely. One emit per PES, unchanged from
                // the pre-Task-4 path.
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
                    crate::mpegts::stats::bump_klv_counters(
                        &mut self.stream_codec_counters,
                        stream.pid,
                        1,
                    );
                    self.queue.push_back(DemuxEvent::Metadata {
                        stream,
                        pts,
                        kind: MetadataKind::KlvAsync,
                        payload: klv,
                    });
                    return;
                }

                // Other-shape PIDs (neither AU cell nor SMPTE UL) — pass
                // through as raw Unknown samples for forensic visibility.
                if matches!(shape, KlvShape::Other) {
                    let payload_len = pes.payload.len();
                    let raw = pes.payload;
                    self.record_item(&stream, program_number, payload_len);
                    self.queue.push_back(DemuxEvent::Sample {
                        stream,
                        pts,
                        dts,
                        payload: SamplePayload::Unknown {
                            stream_type: StreamTypeCode::from_byte(0x15),
                            raw: SharedBytes::from_slice(&raw),
                        },
                    });
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
                let cells: Vec<_> = iter_au_cells(&pes.payload).collect();
                for cell_result in cells {
                    let (header, inner) = match cell_result {
                        Ok(pair) => pair,
                        Err(_) => {
                            // Truncated / malformed trailing cell. Stop
                            // walking — earlier cells in this PES already
                            // emitted or buffered; the partial tail is
                            // dropped silently (the prior detect-only path
                            // never surfaced this either).
                            break;
                        }
                    };

                    // ConcurrentFirst loop: process_cell may report
                    // ConcurrentFirst (a new First/Complete arrived while
                    // a buffer was already open for this PID). The
                    // reassembler dropped its buffer and emitted Failure;
                    // we surface the NonConformant and re-process the
                    // triggering cell against the now-empty state. Bounded
                    // to one re-entry per cell — after the re-entry the
                    // state is empty, so the second call hits the
                    // empty-state row of the table (Buffered or Emit).
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
                                    self.au_reassembler.clear_after_emit(pes.pid);
                                }
                                let meta_len = payload_vec.len();
                                self.record_item(&stream, program_number, meta_len);
                                crate::mpegts::stats::bump_klv_counters(
                                    &mut self.stream_codec_counters,
                                    stream.pid,
                                    1,
                                );
                                self.queue.push_back(DemuxEvent::Metadata {
                                    stream,
                                    // PES PTS reused for every AU emitted
                                    // from this PES (documented limitation
                                    // — multi-AU PES has only one PTS).
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
                                // the data flows. Off by default (callers
                                // must opt in via
                                // DemuxerConfig::cfi_tolerance).
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
                                    crate::mpegts::stats::bump_klv_counters(
                                        &mut self.stream_codec_counters,
                                        stream.pid,
                                        1,
                                    );
                                    self.queue.push_back(DemuxEvent::Metadata {
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
                                    });
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
                                    // ConcurrentFirst is the only Failure
                                    // variant the spec says to re-process —
                                    // the reassembler dropped the prior
                                    // buffer and the caller must replay the
                                    // triggering cell against the now-empty
                                    // state. Other failures (Orphan /
                                    // SequenceGap / Overflow) terminate the
                                    // AU outright.
                                    if matches!(reason, MultiCellAuReason::ConcurrentFirst) {
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
                self.queue.push_back(DemuxEvent::Sample {
                    stream,
                    pts,
                    dts,
                    payload: SamplePayload::Unknown {
                        stream_type: StreamTypeCode::from_byte(stream_type),
                        raw: SharedBytes::from_slice(&pes.payload),
                    },
                });
            }
            StreamKind::Audio(codec) => {
                let payload_len = pes.payload.len();
                self.record_item(&stream, program_number, payload_len);
                // C11 — for AAC-LATM (stream_type 0x11) validate the LOAS
                // syncword at the start of the PES payload. Pre-C11 we
                // advertised LATM without any framing check, so malformed
                // streams produced opaque Sample events that downstream
                // decoders couldn't parse. Lenient mode surfaces the
                // NonConformantIssue alongside the Sample (callers may
                // want the raw bytes for forensic analysis); strict mode
                // (Full) suppresses the sample.
                let latm_rejected = if codec == AudioCodec::AacLatm {
                    match crate::codec::aac::latm::validate_latm_sync(&pes.payload) {
                        Ok(_) => false,
                        Err(kind) => {
                            let issue = NonConformantIssue::LatmFraming { pid: pes.pid, kind };
                            let reject = self.options.strict.rejects(&issue);
                            self.queue_nonconformant(stream, issue);
                            reject
                        }
                    }
                } else {
                    false
                };
                if latm_rejected {
                    return;
                }
                // Codec-specific counter bump. AAC-ADTS + MP2 have frame
                // iterators in `codec::*`; LATM + AC-3 don't (their
                // `stream_codec_stats` accessor falls back to
                // `StreamCodecStats::Unknown` via the stats_per_stream-only
                // path).
                //
                // validate-1 followup-2: use the resync variants
                // (`frames_with_resync`) so a single malformed syncframe in
                // the middle of a PES payload doesn't drop the rest of the
                // frame count — the strict `frames()` iterators terminate
                // on first parse error and undercount stats. Strict
                // `frames()` remains available for fail-fast conformance
                // callers (fuzzers, spec tests).
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
                    crate::mpegts::stats::bump_audio_counters(
                        &mut self.stream_codec_counters,
                        stream.pid,
                        frames_delta,
                    );
                }
                // C12 — AC-3 syncframe alignment enforcement.
                //
                // ATSC A/52:2018 §A.6.3 mandates `data_alignment_indicator=1`
                // for every AC-3 PES, with the implication that the PES
                // payload starts at an AC-3 syncframe (sync word 0x0B77).
                // Surface a NonConformantIssue when the alignment flag is
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
                    self.queue.push_back(DemuxEvent::Sample {
                        stream,
                        pts,
                        dts: None,
                        payload: SamplePayload::Audio {
                            codec,
                            frames: SharedBytes::from_slice(&pes.payload),
                        },
                    });
                }
            }
            StreamKind::Subtitle(codec) => {
                let payload_len = pes.payload.len();
                if self.subtitle_pids_seen.insert(stream.pid) {
                    self.subtitle_streams_seen_count += 1;
                }
                let entry = self.record_item(&stream, program_number, payload_len);
                entry.label.get_or_insert_with(|| {
                    crate::mpegts::stats::demux_subtitle_codec_label(codec).to_string()
                });
                // B6 — EN 300 743 §6.2 (DVB-sub) + EN 300 472 §4.2
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
                let surfaced_payload = if alignment_rejected {
                    // Strict-mode B6 rejection suppresses the sample so
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
                            DvbSubStripResult::Malformed => Some(raw.to_vec()),
                        },
                        _ => Some(raw.to_vec()),
                    }
                };
                if let Some(surfaced_payload) = surfaced_payload {
                    self.queue.push_back(DemuxEvent::Sample {
                        stream,
                        pts,
                        dts: None,
                        payload: SamplePayload::Subtitle {
                            codec,
                            payload: SharedBytes::from_slice(&surfaced_payload),
                        },
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
