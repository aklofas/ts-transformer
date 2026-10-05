//! KLV push paths (`push_klv` / `push_klv_to`) + KLV handle accessors
//! (`klv_handles` / `klv_stream_handle` / `klv_handles_for_program`).
//!
//! Each public method's full rustdoc preamble stays with the method
//! body — the doc comments are part of the API contract, not the
//! source-file organization.

use crate::error::MuxError;
use crate::mpegts::common::Pts90khz;
use alloc::vec::Vec;

use super::Muxer;
use super::pes::{
    MAX_PES_HEADER_SIZE, PesFlags, PesPtsField, STREAM_ID_KLV, STREAM_ID_PRIVATE_STREAM_1,
    write_pes_header,
};
use super::state::ts_packets_for;
use super::types::{KlvStreamHandle, KlvStreamType, StreamKind};

impl Muxer {
    /// Push one KLV metadata blob.
    ///
    /// `pts` becomes the PES PTS when the KLV stream was configured with
    /// `carries_pts: true` in [`crate::mpegts::mux::StreamSpec::Klv`]; ignored otherwise.
    ///
    /// `metadata_service_id` is written into the AU cell header per
    /// ITU-T H.222.0 V9 §2.12.4.2 / ST 1402.2 App. B Table 2 **only** when
    /// the stream is configured as [`KlvStreamType::SynchronousMetadata`]
    /// (stream_type 0x15). For [`KlvStreamType::PrivateData`] (0x06) streams
    /// the payload passes through verbatim and this parameter is ignored.
    ///
    /// The spec default is `0x00`. Pass `0x00` unless you have a specific
    /// reason to use a non-zero service_id (e.g. to mirror the `service_id`
    /// byte of a `metadata_klva` PMT descriptor you supplied at config time).
    ///
    /// # C ABI
    ///
    /// `tst_muxer_push_klv` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxError::NoKlvStreamsConfigured`] if no KLV streams are
    ///   configured on this muxer.
    /// - [`MuxError::AmbiguousTarget`] when more than one KLV stream is
    ///   configured — call [`Self::push_klv_to`] with an explicit handle.
    /// - [`MuxError::KlvTooLarge`] if `klv.len()` would overflow
    ///   `PES_packet_length` (with a 5-byte AU cell header reservation
    ///   for `SynchronousMetadata` streams).
    /// - [`MuxError::BufferFull`] if the resulting TS packets would exceed
    ///   `MuxerConfig::buffer_packets`.
    pub fn push_klv(
        &mut self,
        klv: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), MuxError> {
        let handle = super::resolve_lone(
            &self.klv_streams,
            MuxError::NoKlvStreamsConfigured,
            StreamKind::Klv,
            KlvStreamHandle::pack,
        )?;
        self.push_klv_to(handle, klv, pts, metadata_service_id)
    }

    /// All `KlvStreamHandle`s for this muxer, in `(program, within-program)`
    /// declaration order. One handle per `StreamSpec::Klv` across all programs.
    pub fn klv_handles(&self) -> Vec<KlvStreamHandle> {
        super::all_handles(&self.klv_streams, KlvStreamHandle::pack)
    }

    /// Handle for the i-th KLV stream in `programs[0]`, or `None` if out of
    /// range. Convenience for single-program callers.
    pub fn klv_stream_handle(&self, index: usize) -> Option<KlvStreamHandle> {
        super::first_program_handle(&self.klv_streams, index, KlvStreamHandle::pack)
    }

    /// KLV stream handles for the named program, in declaration order.
    ///
    /// Returns `Err(MuxError::ProgramNotFound)` if no program with the given
    /// number exists.
    pub fn klv_handles_for_program(
        &self,
        program_number: u16,
    ) -> Result<Vec<KlvStreamHandle>, MuxError> {
        super::handles_for_program(
            &self.config.programs,
            &self.klv_streams,
            program_number,
            KlvStreamHandle::pack,
        )
    }

    /// Push one KLV metadata blob on a specific KLV stream.
    ///
    /// `pts` carries the same semantics as [`Self::push_klv`] —
    /// used as the PES PTS only when the targeted KLV stream was
    /// configured with `carries_pts: true`; ignored otherwise.
    ///
    /// For [`KlvStreamType::SynchronousMetadata`] streams, the muxer
    /// auto-prepends a 5-byte `Metadata_AU_cell` header per ITU-T
    /// H.222.0 V9 §2.12.4.2 Tables 2-155+2-156 (see
    /// [`crate::mpegts::au_cell`]). Pass raw KLV LS bytes; do not
    /// pre-wrap. PTS lives in the PES header (per §2.12.4.1).
    /// [`KlvStreamType::PrivateData`] streams pass payload through
    /// unchanged, and `metadata_service_id` is silently ignored on
    /// that path.
    ///
    /// `metadata_service_id` lands in the AU cell header per
    /// ITU-T H.222.0 V9 §2.12.4.2 / ST 1402.2 App. B Table 2.
    /// The spec default is `0x00`. Pass `0x00` unless you have a
    /// specific reason to use a non-zero service_id (e.g. to mirror
    /// the `service_id` byte of a `metadata_klva` PMT descriptor you
    /// supplied at config time).
    ///
    /// # C ABI
    ///
    /// `tst_muxer_push_klv_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's configured KLV stream count.
    /// - [`MuxError::KlvTooLarge`] if `klv.len()` would overflow
    ///   `PES_packet_length` (with a 5-byte AU cell header reservation
    ///   for `SynchronousMetadata` streams).
    /// - [`MuxError::BufferFull`] if the resulting TS packets would exceed
    ///   `MuxerConfig::buffer_packets`.
    pub fn push_klv_to(
        &mut self,
        handle: KlvStreamHandle,
        klv: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), MuxError> {
        let (prog_idx, within_idx) = handle.unpack();
        if prog_idx >= self.klv_streams.len() || within_idx >= self.klv_streams[prog_idx].len() {
            return Err(MuxError::InvalidStreamHandle {
                kind: StreamKind::Klv,
                index: handle.0 as usize,
            });
        }
        let k = &self.klv_streams[prog_idx][within_idx];
        let klv_pid = k.pid;
        let klv_carries_pts = k.carries_pts;
        let is_sync = k.stream_type == KlvStreamType::SynchronousMetadata;
        let seq_num = k.au_cell_sequence_number;

        // Auto-wrap sync KLV in an H.222.0 §2.12.4.2 Metadata_AU_cell header.
        // PrivateData streams pass payload through as-is (caller controls shape).
        let wrapped_storage: Option<Vec<u8>> = if is_sync {
            let header = crate::mpegts::au_cell::AuCellHeader {
                metadata_service_id, // caller-supplied; see push_klv_to doc comment.
                sequence_number: seq_num,
                cell_fragment_indication: crate::mpegts::au_cell::CellFragmentIndication::Complete,
                decoder_config_flag: false,
                random_access_indicator: true, // ST 0601 records are self-contained.
            };
            let mut buf = Vec::with_capacity(5 + klv.len());
            crate::mpegts::au_cell::write_metadata_au_cell(&mut buf, header, klv).map_err(|e| {
                match e {
                    crate::mpegts::au_cell::AuCellError::PayloadTooLarge { size, .. } => {
                        MuxError::KlvTooLarge {
                            size,
                            max: crate::mpegts::au_cell::MAX_AU_CELL_PAYLOAD,
                        }
                    }
                }
            })?;
            Some(buf)
        } else {
            None
        };
        let effective_klv: &[u8] = wrapped_storage.as_deref().unwrap_or(klv);

        let pts_field = if klv_carries_pts {
            PesPtsField::PtsOnly(pts)
        } else {
            PesPtsField::None
        };

        let pes_overhead = 3usize + if klv_carries_pts { 5 } else { 0 };
        let max_klv = (u16::MAX as usize) - pes_overhead;
        if effective_klv.len() > max_klv {
            // Report the inner caller payload size in the error, since that's
            // what they control. Subtract 5-byte AU cell header overhead from
            // the cap when sync.
            let report_size = klv.len();
            let report_max = if is_sync { max_klv - 5 } else { max_klv };
            return Err(MuxError::KlvTooLarge {
                size: report_size,
                max: report_max,
            });
        }

        let mut header = [0u8; MAX_PES_HEADER_SIZE];
        // Sync KLV (stream_type 0x15 SynchronousMetadata): stream_id 0xFC per
        // H.222.0 V9 Table 2-22 (reserved for metadata streams).
        // Async KLV (stream_type 0x06 PrivateData): stream_id 0xBD per ffmpeg +
        // GStreamer convention — H.222.0 Table 2-22 reserves 0xFC for metadata
        // streams (stream_type 0x15) only.
        // data_alignment_indicator=1 on both paths: H.222.0 V9 §2.12.4.1
        // mandates it for sync KLV; also conventional for async KLV AU delivery.
        let pes_stream_id = if is_sync {
            STREAM_ID_KLV // 0xFC — H.222.0 metadata stream_id, sync KLV (stream_type 0x15).
        } else {
            STREAM_ID_PRIVATE_STREAM_1 // 0xBD — async KLV (stream_type 0x06).
        };
        let header_len = write_pes_header(
            &mut header,
            pes_stream_id,
            pts_field,
            Some(effective_klv.len() as u16),
            PesFlags {
                data_alignment_indicator: true,
            },
        );

        self.pes_scratch.clear();
        self.pes_scratch.extend_from_slice(&header[..header_len]);
        self.pes_scratch.extend_from_slice(effective_klv);

        let klv_packets = ts_packets_for(self.pes_scratch.len());
        // See push_video for the rationale. KLV is the
        // most-likely-affected push path because KLV streams are frequently
        // configured on a non-PCR PID with low push cadence relative to
        // the PCR PID's own (zero, here) push cadence.
        self.reserve_preamble(prog_idx, pts, klv_pid, klv_packets)?;

        let first_af = self.pcr_first_af(prog_idx, pts, klv_pid);
        self.drain_pes_scratch(klv_pid, first_af);

        // Count on the Ok path only — after all early-returns above. Stats
        // count caller's payload bytes, not auto-wrapped bytes.
        if let Some(s) = self.per_stream.get_mut(&klv_pid) {
            s.items += 1;
            s.touch_last_seen();
            s.bytes += klv.len() as u64;
        }
        // One push = one KLV record (muxer contract: caller passes a single
        // KLV LS per call). Wire-format records-per-PES > 1 are not
        // possible through this API.
        crate::mpegts::stats::bump_klv_counters(&mut self.stream_codec_counters, klv_pid, 1);
        if is_sync {
            self.klv_streams[prog_idx][within_idx].au_cell_sequence_number =
                seq_num.wrapping_add(1);
        }

        Ok(())
    }
}
