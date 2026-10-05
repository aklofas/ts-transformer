//! Data push paths (`push_data` / `push_data_to`) + data handle accessors
//! (`data_handles` / `data_stream_handle` / `data_handles_for_program`).
//!
//! Data streams are a PES **pass-through**: the muxer applies no framing
//! (no AU cell wrap, no payload inspection, no sequence numbering) — one
//! push becomes exactly one PES packet on the configured PID. This is the
//! write-side dual of demux `StreamKind::Unknown`.
//!
//! Each public method's full rustdoc preamble stays with the method
//! body — the doc comments are part of the API contract, not the
//! source-file organization.

use crate::error::MuxError;
use crate::mpegts::common::Pts90khz;
use alloc::vec::Vec;

use super::Muxer;
use super::pes::{
    MAX_PES_HEADER_SIZE, PesFlags, PesPtsField, STREAM_ID_PRIVATE_STREAM_1, write_pes_header,
};
use super::state::ts_packets_for;
use super::ts::AdaptationField;
use super::types::{DataStreamHandle, StreamKind};

impl Muxer {
    /// Push one data payload on the muxer's single data stream.
    ///
    /// Convenience shorthand for single-data-stream configs; payload and
    /// `pts` semantics are those of [`Self::push_data_to`] (the contract
    /// holder — see its docs for the pass-through guarantees and the
    /// no-PTS-stream behavior).
    ///
    /// # C ABI
    ///
    /// `tst_muxer_push_data` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxError::NoDataStreamsConfigured`] if no data streams are
    ///   configured on this muxer.
    /// - [`MuxError::AmbiguousTarget`] when more than one data stream is
    ///   configured — call [`Self::push_data_to`] with an explicit handle.
    /// - [`MuxError::DataTooLarge`] if `data.len()` would overflow
    ///   `PES_packet_length`.
    /// - [`MuxError::BufferFull`] if the resulting TS packets would exceed
    ///   `MuxerConfig::buffer_packets`.
    pub fn push_data(&mut self, data: &[u8], pts: Pts90khz) -> Result<(), MuxError> {
        let handle = super::resolve_lone(
            &self.data_streams,
            MuxError::NoDataStreamsConfigured,
            StreamKind::Data,
            DataStreamHandle::pack,
        )?;
        self.push_data_to(handle, data, pts)
    }

    /// All `DataStreamHandle`s for this muxer, in `(program, within-program)`
    /// declaration order. One handle per `StreamSpec::Data` across all programs.
    pub fn data_handles(&self) -> Vec<DataStreamHandle> {
        super::all_handles(&self.data_streams, DataStreamHandle::pack)
    }

    /// Handle for the i-th data stream in `programs[0]`, or `None` if out of
    /// range. Convenience for single-program callers.
    pub fn data_stream_handle(&self, index: usize) -> Option<DataStreamHandle> {
        super::first_program_handle(&self.data_streams, index, DataStreamHandle::pack)
    }

    /// Data stream handles for the named program, in declaration order.
    ///
    /// Returns `Err(MuxError::ProgramNotFound)` if no program with the given
    /// number exists.
    pub fn data_handles_for_program(
        &self,
        program_number: u16,
    ) -> Result<Vec<DataStreamHandle>, MuxError> {
        super::handles_for_program(
            &self.config.programs,
            &self.data_streams,
            program_number,
            DataStreamHandle::pack,
        )
    }

    /// Push one data payload on a specific data stream.
    ///
    /// **Pass-through contract:** the muxer applies no AU-cell wrap, no
    /// framing, and no payload inspection — `data` lands verbatim as the
    /// PES payload, and one push produces exactly one PES packet. The PES
    /// uses `stream_id` `0xBD` (`private_stream_1`) with
    /// `data_alignment_indicator = 1`. Record boundaries within `data`
    /// (if any) are entirely the caller's convention.
    ///
    /// `pts` is written into the PES header only when the targeted stream
    /// was configured with `carries_pts: true` in
    /// [`crate::mpegts::mux::StreamSpec::Data`]; it is **always** used for
    /// PSI/PCR pacing decisions regardless. A DTS is not representable on
    /// this path (data PES carry at most a PTS).
    ///
    /// # No-PTS streams
    ///
    /// For `carries_pts: false` streams the PES omits the PTS field
    /// entirely. On the receive side, this library's demuxer surfaces
    /// such samples with `pts == Pts90khz::new(0)` (its no-PTS
    /// substitute) and emits no `NonConformant` — H.222.0 §2.7.4 makes
    /// PTS mandatory only for video and audio streams.
    ///
    /// # C ABI
    ///
    /// `tst_muxer_push_data_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's configured data stream count.
    /// - [`MuxError::DataTooLarge`] if `data.len()` would overflow
    ///   `PES_packet_length` (ceiling 65532 bytes without PTS, 65527
    ///   with).
    /// - [`MuxError::BufferFull`] if the resulting TS packets would exceed
    ///   `MuxerConfig::buffer_packets`.
    pub fn push_data_to(
        &mut self,
        handle: DataStreamHandle,
        data: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxError> {
        let (prog_idx, within_idx) = handle.unpack();
        if prog_idx >= self.data_streams.len() || within_idx >= self.data_streams[prog_idx].len() {
            return Err(MuxError::InvalidStreamHandle {
                kind: StreamKind::Data,
                index: handle.0 as usize,
            });
        }
        let d = &self.data_streams[prog_idx][within_idx];
        let data_pid = d.pid;
        let data_carries_pts = d.carries_pts;

        let pts_field = if data_carries_pts {
            PesPtsField::PtsOnly(pts)
        } else {
            PesPtsField::None
        };

        let pes_overhead = 3usize + if data_carries_pts { 5 } else { 0 };
        let max_data = (u16::MAX as usize) - pes_overhead;
        if data.len() > max_data {
            return Err(MuxError::DataTooLarge {
                size: data.len(),
                max: max_data,
            });
        }

        let mut header = [0u8; MAX_PES_HEADER_SIZE];
        // Data streams always use stream_id 0xBD (private_stream_1) — the
        // H.222.0 Table 2-22 carrier for PES-private payloads, matching
        // the async-KLV / private-data convention.
        // data_alignment_indicator=1: each push is one self-contained
        // caller unit starting at the PES payload's first byte.
        let header_len = write_pes_header(
            &mut header,
            STREAM_ID_PRIVATE_STREAM_1,
            pts_field,
            Some(data.len() as u16),
            PesFlags {
                data_alignment_indicator: true,
            },
        );

        self.pes_scratch.clear();
        self.pes_scratch.extend_from_slice(&header[..header_len]);
        self.pes_scratch.extend_from_slice(data);

        let data_packets = ts_packets_for(self.pes_scratch.len());
        // See push_video for the rationale. Like KLV, data
        // streams typically ride a non-PCR PID with an independent (often
        // low) push cadence, so PSI/PCR-only emissions frequently piggyback
        // on data pushes and must be budgeted in the BufferFull pre-check.
        self.reserve_preamble(prog_idx, pts, data_pid, data_packets)?;

        // No PCR-on-first-packet branch here (unlike push_klv):
        // validate() rejects PCR pinned on a data PID and the
        // effective-PCR fallback chain never lands on one, so
        // `self.pcr_pids[prog_idx] == data_pid` is unreachable.
        self.drain_pes_scratch(data_pid, AdaptationField::default());

        // Count on the Ok path only — after all early-returns above.
        // Data PIDs get per_stream items/bytes but no stream_codec_counters
        // entry (by design: they surface StreamCodecStats::Unknown).
        if let Some(s) = self.per_stream.get_mut(&data_pid) {
            s.items += 1;
            s.touch_last_seen();
            s.bytes += data.len() as u64;
        }

        Ok(())
    }
}
