//! Audio push paths (`push_audio` / `push_audio_to`) + audio handle
//! accessors (`audio_handles` / `audio_handles_for_program`).
//!
//! Each public method's full rustdoc preamble stays with the method
//! body — the doc comments are part of the API contract, not the
//! source-file organization.

use crate::error::MuxError;
use crate::mpegts::common::Pts90khz;
use alloc::vec::Vec;

use super::Muxer;
use super::pes::{PesPtsField, write_audio_pes};
use super::state::ts_packets_for;
use super::types::{AudioStreamHandle, StreamKind};

impl Muxer {
    /// Push one audio frame buffer, single-stream shorthand.
    ///
    /// `pts` is required and becomes the PES PTS; audio has no DTS
    /// (no B-frame reorder). `frames` is one or more pre-framed audio frames
    /// concatenated by the caller.
    ///
    /// Resolves only when exactly one audio stream is configured across all
    /// programs. Otherwise rejects with [`MuxError::AmbiguousTarget`].
    ///
    /// # C ABI
    ///
    /// `tst_muxer_push_audio` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxError::NoAudioStreamsConfigured`] if no audio streams are
    ///   configured on this muxer.
    /// - [`MuxError::AmbiguousTarget`] when more than one audio stream is
    ///   configured — call [`Self::push_audio_to`] with an explicit handle.
    /// - [`MuxError::AudioTooLarge`] if `frames.len()` would overflow
    ///   `PES_packet_length`.
    /// - [`MuxError::BufferFull`] if the resulting TS packets would exceed
    ///   `MuxerConfig::buffer_packets`.
    pub fn push_audio(&mut self, frames: &[u8], pts: Pts90khz) -> Result<(), MuxError> {
        let handle = super::resolve_lone(
            &self.audio_streams,
            MuxError::NoAudioStreamsConfigured,
            StreamKind::Audio,
            AudioStreamHandle::pack,
        )?;
        self.push_audio_to(handle, pts, frames)
    }

    /// Push one audio frame buffer on a specific audio stream.
    ///
    /// Routes to the audio stream identified by `handle`. Use the bare
    /// [`push_audio`][Self::push_audio] shorthand when exactly one audio
    /// stream is configured. Handles are obtained from
    /// [`audio_handles`][Self::audio_handles] /
    /// [`audio_handles_for_program`][Self::audio_handles_for_program].
    ///
    /// # C ABI
    ///
    /// `tst_muxer_push_audio_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's configured audio stream count (across all
    ///   programs).
    /// - [`MuxError::AudioTooLarge`] if `frames.len()` would overflow
    ///   `PES_packet_length` (max ~65527 bytes after PES overhead).
    /// - [`MuxError::BufferFull`] if the resulting TS packets would exceed
    ///   `MuxerConfig::buffer_packets`.
    pub fn push_audio_to(
        &mut self,
        handle: AudioStreamHandle,
        pts: Pts90khz,
        frames: &[u8],
    ) -> Result<(), MuxError> {
        let (prog_idx, within_idx) = handle.unpack();
        if prog_idx >= self.audio_streams.len() || within_idx >= self.audio_streams[prog_idx].len()
        {
            return Err(MuxError::InvalidStreamHandle {
                kind: StreamKind::Audio,
                index: handle.0 as usize,
            });
        }
        let audio_pid = self.audio_streams[prog_idx][within_idx].pid;
        let audio_codec = self.audio_streams[prog_idx][within_idx].codec;

        // Audio always uses PTS, so PES overhead is 3 (start code) + 5 (PTS) = 8 bytes.
        // The remaining space in the u16 PES_packet_length field is for flags, header_data_length,
        // and the payload. Guard against frames that would overflow PES_packet_length.
        let pes_overhead = 3usize + 5;
        let max_audio = (u16::MAX as usize) - pes_overhead;
        if frames.len() > max_audio {
            return Err(MuxError::AudioTooLarge {
                size: frames.len(),
                max: max_audio,
            });
        }

        let pes_pts = PesPtsField::PtsOnly(pts);
        self.pes_scratch.clear();
        write_audio_pes(
            &mut self.pes_scratch,
            audio_codec,
            within_idx as u8,
            pes_pts,
            frames,
        );

        let audio_packets = ts_packets_for(self.pes_scratch.len());
        // See push_video for the rationale. Audio is
        // typically high-cadence, but a low-frame-rate stream (sparse
        // language tracks, sign-language audio) could still drift.
        self.reserve_preamble(prog_idx, pts, audio_pid, audio_packets)?;

        let first_af = self.pcr_first_af(prog_idx, pts, audio_pid);
        self.drain_pes_scratch(audio_pid, first_af);

        // Count on the Ok path only — after all early-returns above.
        if let Some(s) = self.per_stream.get_mut(&audio_pid) {
            s.items += 1;
            s.touch_last_seen();
            s.bytes += frames.len() as u64;
        }

        // Codec-counter bump. AAC + MP2 have lazy-stateless frame iterators
        // (codec::aac::frames / codec::mpegaudio::frames). LATM and AC-3
        // don't yet — those PIDs leave the codec counter unmaterialized
        // so the accessor returns Some(Unknown) via per_stream fallback.
        //
        // Count with the resync variants so a single
        // malformed syncframe inside the caller-supplied buffer doesn't
        // truncate the rest of the frame count. Strict `frames()` is still
        // available for fail-fast conformance callers.
        let frames_delta: u64 = match audio_codec {
            crate::mpegts::mux::AudioCodec::Aac => crate::codec::aac::frames_with_resync(frames)
                .filter_map(Result::ok)
                .count() as u64,
            crate::mpegts::mux::AudioCodec::Mp2 => {
                crate::codec::mpegaudio::frames_with_resync(frames)
                    .filter_map(Result::ok)
                    .count() as u64
            }
            crate::mpegts::mux::AudioCodec::AacLatm | crate::mpegts::mux::AudioCodec::Ac3 => 0,
        };
        if frames_delta > 0 {
            crate::mpegts::stats::bump_audio_counters(
                &mut self.stream_codec_counters,
                audio_pid,
                frames_delta,
            );
        }

        Ok(())
    }

    /// All `AudioStreamHandle`s for this muxer, in `(program, within-program)`
    /// declaration order. One handle per `StreamSpec::Audio` across all programs.
    pub fn audio_handles(&self) -> Vec<AudioStreamHandle> {
        super::all_handles(&self.audio_streams, AudioStreamHandle::pack)
    }

    /// Audio stream handles for the named program, in declaration order.
    ///
    /// Returns `Err(MuxError::ProgramNotFound)` if no program with the given
    /// number exists.
    pub fn audio_handles_for_program(
        &self,
        program_number: u16,
    ) -> Result<Vec<AudioStreamHandle>, MuxError> {
        super::handles_for_program(
            &self.config.programs,
            &self.audio_streams,
            program_number,
            AudioStreamHandle::pack,
        )
    }
}
