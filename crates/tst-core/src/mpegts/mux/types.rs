//! Public type vocabulary for `mpegts::mux` — codec / stream-class enums,
//! per-stream config tags, and opaque handle types.
//!
//! Production code lives in `mod.rs` (`Muxer` impl, configuration, PSI
//! / PES sites). Splitting these decl-heavy sections out keeps `mod.rs`
//! navigable and gives the type vocabulary its own LSP-friendly home.

/// Video codec carried by the muxer's video PID.
///
/// Drives the PMT `stream_type` byte: 0x1B for H.264 / AVC,
/// 0x24 for H.265 / HEVC, 0x33 for H.266 / VVC. AV1 sits on
/// `stream_type 0x06` with an auto-emitted AV01 `registration_descriptor`.
/// Mid-stream codec change is out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
    /// H.266 / VVC. Drives PMT stream_type 0x33.
    H266,
    /// AV1. Drives PMT stream_type 0x06 with auto-emitted AV01
    /// `registration_descriptor`.
    Av1,
}

/// AV1 carriage mode in PES — controls the AV1-in-MPEG-2-TS binding
/// conformance level.
///
/// The AV1-in-MPEG-2-TS binding (`av1-mpeg2-ts-binding.html`) mandates
/// two non-default behaviors for AV1 PES carriage:
/// - **§3.4**: PES `stream_id` MUST be `0xBD` (private_stream_1),
///   distinct from the typical `0xE0` for video.
/// - **§3.2**: OBUs MUST be wrapped in `ts_open_bitstream_unit()` framing
///   — each OBU prefixed with a 3-byte `obu_start_code` (`uimsbf(24)`
///   with value `0x000001`, i.e. the byte sequence `0x00 0x00 0x01`) plus
///   emulation-prevention escapes inside the payload (any byte sequence
///   `0x00 0x00 0x0X` with `X ∈ {0x00, 0x01, 0x02, 0x03}` has a `0x03`
///   emulation prevention byte inserted after the second `0x00`).
///
/// Default is [`Av1CarriageMode::Mpeg2TsBinding`] for spec conformance.
/// Use [`Av1CarriageMode::InteropRawObu`] when interoperating with
/// ffmpeg / libaom / hls.js / mediamtx — those tools today carry AV1
/// PES with `stream_id=0xE0` and raw OBU payload (no `ts_open_bitstream_unit`
/// framing). The interop mode preserves loopback with that ecosystem.
///
/// Symmetric setting exists on the demuxer
/// (`DemuxerConfig::av1_carriage`); the two MUST match for a successful
/// round-trip.
///
/// Strict-mode receivers running in `Mpeg2TsBinding` mode surface
/// [`crate::mpegts::demux::NonConformantIssue::Av1WrongStreamId`] when
/// an incoming AV1 PES uses `stream_id != 0xBD`, and
/// [`crate::mpegts::demux::NonConformantIssue::Av1MissingTsObuFraming`]
/// when the PES payload does not start with a `ts_open_bitstream_unit`
/// start code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Av1CarriageMode {
    /// AV1-in-MPEG-2-TS binding conformant carriage. PES `stream_id=0xBD`
    /// (private_stream_1) per §3.4; OBUs wrapped in `ts_open_bitstream_unit()`
    /// framing per §3.2 (3-byte start code `0x00 0x00 0x01` + emulation
    /// prevention bytes). This is the default.
    #[default]
    Mpeg2TsBinding,
    /// Interoperability mode for the ffmpeg / libaom / hls.js / mediamtx
    /// AV1-in-TS toolchain — PES `stream_id=0xE0` (video) and raw OBU
    /// payload (no `ts_open_bitstream_unit` framing). Non-conformant per
    /// the binding spec, but matches the de facto carriage used by these
    /// tools today.
    InteropRawObu,
}

/// Transport-stream type for the KLV PID.
///
/// `PrivateData` (PMT stream_type 0x06) is the broadly-recognized form;
/// `SynchronousMetadata` (0x15) is strict ST 1402 sync.
///
/// Whether the KLV PES carries a PTS is controlled separately via the
/// `carries_pts` field in `StreamSpec::Klv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KlvStreamType {
    PrivateData,
    SynchronousMetadata,
}

/// Audio codec carried by an audio elementary stream.
///
/// Drives the PMT `stream_type` byte:
/// - `Mp2` → 0x03 (ISO/IEC 11172-3 Audio — covers MPEG-1 Layer I/II/III)
/// - `Aac` → 0x0F (ISO/IEC 13818-7 ADTS Audio)
/// - `AacLatm` → 0x11 (ISO/IEC 14496-3 LATM Audio)
/// - `Ac3` → 0x81 (ATSC AC-3)
///
/// E-AC-3, DVB-shaped AC-3 (`stream_type 0x06` + AC-3 registration),
/// MP3 on user-private stream_types: not classified automatically;
/// callers route via `DemuxerConfig::treat_as` on the demux side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    Mp2,
    Aac,
    AacLatm,
    Ac3,
}

/// Subtitle / caption codec carried by a subtitle elementary stream.
///
/// All four variants emit PMT `stream_type = 0x06` (PES private data);
/// disambiguation happens via the auto-emitted PMT descriptor at PSI
/// generation time. See `mpegts::descriptors` for the descriptor
/// encoders this enum drives.
///
/// `Clone` but not `Copy` — a deliberate asymmetry vs. `VideoCodec` /
/// `AudioCodec` (which are both `Copy`). Subtitle codec parameters are
/// structurally part of the codec value here (vs. siblings, where the
/// enum is purely a tag), so forward-compatible variants that may carry
/// non-`Copy` payloads (e.g. variable-length DVB ancillary blobs) won't
/// require a breaking change to drop `Copy` later.
///
/// CEA-608/708 in SEI (the typical "captions in H.264/H.265") is NOT
/// in scope for this enum — that's the deferred SEI parsing plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleCodec {
    /// DVB subtitling (bitmap-shaped). Per ETSI EN 300 468 §6.2.41 +
    /// ETSI EN 300 743.
    DvbSubtitling {
        /// ISO 639-2 language code, lowercase ASCII (e.g. *b"eng").
        language: [u8; 3],
        /// ETSI EN 300 468 Table 26. Common values: 0x10 (DVB sub,
        /// no AR signalling), 0x14 (DVB sub for 4:3 aspect-ratio).
        subtitling_type: u8,
        /// Composition page identifier (0..=0xFFFF).
        composition_page_id: u16,
        /// Ancillary page identifier (0..=0xFFFF).
        ancillary_page_id: u16,
    },
    /// DVB teletext. Per ETSI EN 300 468 §6.2.43 + ETSI EN 300 706.
    DvbTeletext {
        /// ISO 639-2 language code, lowercase ASCII.
        language: [u8; 3],
        /// 5-bit teletext_type. Common values: 0x01 (initial page),
        /// 0x02 (subtitle page), 0x04 (programme schedule),
        /// 0x05 (hearing-impaired subtitle).
        teletext_type: u8,
        /// Magazine number, 0..=7. (3-bit field.)
        magazine_number: u8,
        /// BCD-encoded page number, 0x00..=0x99. The convention for
        /// subtitles is magazine 8 page 88 (= magazine_number=0,
        /// page_number=0x88 in this representation since
        /// magazine "8" wraps to 0 in the 3-bit field).
        page_number: u8,
    },
    /// CEA-708 caption data carried as a separate elementary stream
    /// (rather than embedded in H.264 / H.265 SEI). **Informal industry
    /// convention** — ATSC A/53 Part 4 §6.2.3 defines `"GA94"` as the
    /// `user_data_identifier` for caption data **embedded in MPEG-2
    /// video user_data**, not as a stream-level marker for a standalone
    /// CEA-708 elementary stream. No published spec defines this
    /// *raw-`cc_data`* carriage form; the auto-emitted
    /// `registration_descriptor` with `format_identifier = "GA94"` is
    /// interop-with-ATSC-ecosystem-tooling best-effort. (The
    /// standards-aligned standalone form — a SMPTE ST 334-2 Caption
    /// Distribution Packet per EG 43 §6.7 — is not implemented; see
    /// `docs/project/deferred-features.md` "CEA-708 interop".)
    Cea708Standalone,
    /// WebVTT cues carried inside MPEG-TS PES. **Informal industry
    /// convention** — neither RFC 8216 nor draft-pantos-hls-rfc8216bis
    /// nor any published spec defines WebVTT-in-MPEG-TS PES carriage.
    /// The `"VTTC"` `format_identifier` is a ffmpeg `mpegtsenc.c`
    /// convention recognized by hls.js v1.7+ and mediamtx, not a
    /// normatively-defined codepoint. Auto-emits `registration_descriptor`
    /// with `format_identifier = "VTTC"`.
    WebVttInTs,
}

/// Classifier for the five supported stream classes carried in an MPEG-TS
/// program. Used by [`MuxError`](crate::error::MuxError) variants whose semantics are
/// stream-kind-specific (e.g., [`MuxError::AmbiguousTarget`](crate::error::MuxError::AmbiguousTarget),
/// [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle), [`MuxError::DescriptorIndexOutOfRange`](crate::error::MuxError::DescriptorIndexOutOfRange)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamKind {
    Video,
    Audio,
    Klv,
    Subtitle,
    Data,
}

impl core::fmt::Display for StreamKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            StreamKind::Video => "video",
            StreamKind::Audio => "audio",
            StreamKind::Klv => "klv",
            StreamKind::Subtitle => "subtitle",
            StreamKind::Data => "data",
        })
    }
}

/// Field-name discriminator inside a teletext-stream configuration block;
/// used by [`MuxError::InvalidTeletextField`](crate::error::MuxError::InvalidTeletextField) in place of `&'static str`
/// tagging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TeletextField {
    MagazineNumber,
    TeletextType,
}

impl core::fmt::Display for TeletextField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            TeletextField::MagazineNumber => "magazine_number",
            TeletextField::TeletextType => "teletext_type",
        })
    }
}

/// One elementary stream in the muxer's output TS.
///
/// [`MuxerConfig::validate`](crate::mpegts::mux::MuxerConfig::validate) caps at 16 video + 16 KLV streams, with at least
/// one of either kind required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamSpec {
    Video {
        /// PID for the video PES stream. Must be in `0x0010..=0x1FFE`.
        pid: u16,
        /// Video codec — drives PMT stream_type (0x1B for H.264, 0x24 for H.265).
        codec: VideoCodec,
    },
    Klv {
        /// PID for the KLV metadata stream. Must be in `0x0010..=0x1FFE`,
        /// distinct from any video PID.
        pid: u16,
        /// Transport-stream type — drives the PMT stream_type byte
        /// (0x06 PrivateData / 0x15 SynchronousMetadata).
        stream_type: KlvStreamType,
        /// Whether the KLV PES carries a PTS in its header.
        /// `false` = ST 1402 async (no PTS).
        /// `true`  = sync KLV (PTS aligns with video).
        /// `SynchronousMetadata` + `false` is invalid.
        carries_pts: bool,
    },
    Audio {
        /// PID for the audio PES stream. Must be in `0x0010..=0x1FFE`.
        pid: u16,
        /// Audio codec — drives PMT stream_type (0x03 MP2, 0x0F AAC, 0x11 LATM, 0x81 AC-3).
        codec: AudioCodec,
        /// Optional ISO 639-2 language code (3 lowercase ASCII bytes, e.g. `*b"eng"`).
        ///
        /// When `Some`, the muxer auto-emits an `iso_639_language_descriptor`
        /// (tag `0x0A`, ISO/IEC 13818-1 §2.6.18-19) with `audio_type=0x00`
        /// (undefined / clean main). When `None`, no descriptor is emitted.
        ///
        /// Suppressed when the caller has already supplied a tag-`0x0A`
        /// descriptor via `stream_descriptors_for_audio` — same posture
        /// as KLVA / AV01 / AC-3 registration auto-emit. The auto-emit
        /// itself is wired in the PMT descriptor writer; this field exists,
        /// defaults to `None`, and is plumbed through from the builder helpers.
        language: Option<[u8; 3]>,
    },
    Subtitle {
        /// PID for the subtitle PES stream. Must be in `0x0010..=0x1FFE`.
        pid: u16,
        /// Subtitle codec — all variants emit PMT `stream_type = 0x06`;
        /// the auto-emitted PMT descriptor disambiguates.
        codec: SubtitleCodec,
    },
    /// One arbitrary private/application data elementary stream — PES
    /// pass-through with a caller-chosen PMT stream_type byte. The
    /// write-side dual of demux `StreamKind::Unknown(stream_type)`.
    ///
    /// Caller PMT descriptors (e.g. private tag-0xFF name descriptors)
    /// ride the program's `stream_descriptors` array via
    /// [`MuxerProgramConfigBuilder::stream_descriptors_for_data`](crate::mpegts::mux::MuxerProgramConfigBuilder::stream_descriptors_for_data);
    /// the muxer never auto-emits a descriptor on a Data stream.
    ///
    /// `validate()` rejects `(stream_type, descriptors)` pairs the demux
    /// classifier would map to a typed kind ("must classify Unknown"):
    /// typed bytes (0x1B/0x24/0x33/0x15/0x03/0x04/0x0F/0x11/0x81) and
    /// 0x06 with classifying markers (subtitling/teletext tags or
    /// AV01/VTTC/GA94/KLVA registrations). Use the typed `StreamSpec`
    /// variants for those.
    Data {
        /// PID. Must be in `0x0010..=0x1FFE`, distinct from other stream PIDs.
        pid: u16,
        /// Raw PMT stream_type byte (e.g. 0xF0/0xF1 user-private, bare 0x06).
        stream_type: u8,
        /// Whether the PES header carries a PTS (PES-level property the
        /// PMT cannot declare). The PES stream_id is always 0xBD
        /// (private_stream_1); DTS is not representable.
        carries_pts: bool,
    },
}

impl StreamSpec {
    pub(crate) fn pid(&self) -> u16 {
        match self {
            StreamSpec::Video { pid, .. } => *pid,
            StreamSpec::Klv { pid, .. } => *pid,
            StreamSpec::Audio { pid, .. } => *pid,
            StreamSpec::Subtitle { pid, .. } => *pid,
            StreamSpec::Data { pid, .. } => *pid,
        }
    }
}

// ── StreamHandle macro ────────────────────────────────────────────────────
//
// All five *StreamHandle types share byte-identical Debug + impl blocks;
// only the type name (for constructor / debug label) and the StreamKind
// variant (for the try_from_raw error) differ. This macro emits those
// two blocks, keeping the per-type struct definition and its public
// rustdoc outside so each type reads independently in docs.
macro_rules! impl_stream_handle {
    ($ty:ident, $kind:expr, $label:literal) => {
        impl core::fmt::Debug for $ty {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                let (prog, within) = self.unpack();
                write!(f, concat!($label, "(prog={}, stream={})"), prog, within)
            }
        }

        impl $ty {
            // Load-bearing for `tst-c` C ABI: `tst-c` converts handles
            // to/from `uint32_t` at the FFI boundary. All methods are
            // `#[doc(hidden)]` — obtain handles via the `Muxer::*_handles`
            // entry points.

            /// Pack `(program_index, within_program_index)` into an opaque u32.
            #[doc(hidden)]
            pub fn pack(program_index: usize, within_index: usize) -> Self {
                Self(crate::mpegts::common::handle_pack::pack(
                    program_index,
                    within_index,
                ))
            }

            /// Unpack the opaque u32 into `(program_index, within_program_index)`.
            #[doc(hidden)]
            pub fn unpack(self) -> (usize, usize) {
                crate::mpegts::common::handle_pack::unpack(self.0)
            }

            /// Return the packed `u32` representation (for FFI use).
            #[doc(hidden)]
            pub fn raw(self) -> u32 {
                self.0
            }

            /// Wrap a raw packed `u32` for trusted in-process round-trips.
            /// Use [`Self::try_from_raw`] at every external trust boundary.
            #[doc(hidden)]
            pub fn from_raw(raw: u32) -> Self {
                Self(raw)
            }

            /// Validating sibling of [`Self::from_raw`]. Rejects any
            /// `raw` value with bits set outside the 4-bit program + 4-bit
            /// within-program layout. Use at every FFI / PyO3 / IPC boundary.
            ///
            /// # Errors
            ///
            /// Returns `MuxError::InvalidStreamHandle` if `raw` contains
            /// any high bits outside the packed 8-bit layout.
            #[doc(hidden)]
            pub fn try_from_raw(raw: u32) -> Result<Self, crate::error::MuxError> {
                if crate::mpegts::common::handle_pack::try_unpack(raw).is_none() {
                    return Err(crate::error::MuxError::InvalidStreamHandle {
                        kind: $kind,
                        index: raw as usize,
                    });
                }
                Ok(Self(raw))
            }
        }
    };
}

/// Opaque handle to a configured video stream on a `Muxer`.
///
/// Obtained from [`Muxer::video_handles`](crate::mpegts::mux::Muxer::video_handles) / [`Muxer::video_stream_handle`](crate::mpegts::mux::Muxer::video_stream_handle) /
/// [`Muxer::video_handles_for_program`](crate::mpegts::mux::Muxer::video_handles_for_program).
/// Handles are valid only on the muxer that produced them; passing a handle
/// to a different muxer is rejected with [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle).
///
/// The internal representation encodes `(program_index, within_program_index)`
/// in a packed `u32`. Callers treat this as an opaque token.
///
/// # Lifecycle
///
/// - **Bound to producer.** Each handle is bound to the `Muxer` (or
///   `MuxSender` wrapping that `Muxer`) that produced it. Using a handle
///   with a different `Muxer` / `MuxSender` instance is rejected with
///   [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle)
///   (accessible as `err.source == MuxSenderErrorSource::Mux(MuxError::InvalidStreamHandle { .. })`
///   when the call goes through `MuxSender`).
/// - **Parent close invalidates.** A handle remains valid for the lifetime
///   of its parent muxer. After the parent is dropped or closed (e.g.,
///   `MuxSender::close()`), the handle becomes inert — any further use
///   through the now-dropped parent path is moot. Storing a handle past
///   parent drop is safe (the handle is `Copy + 'static`), but cannot be
///   "rebound" to a new parent muxer.
/// - **Clone semantics.** `VideoStreamHandle: Copy`. Cloning produces an
///   identical handle that refers to the same configured stream within
///   the same parent muxer. There is no per-handle identity beyond
///   `(program_index, within_program_index)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoStreamHandle(pub(super) u32);

impl_stream_handle!(VideoStreamHandle, StreamKind::Video, "VideoStreamHandle");

/// Opaque handle to a configured KLV stream on a `Muxer`.
///
/// Obtained from [`Muxer::klv_handles`](crate::mpegts::mux::Muxer::klv_handles) /
/// [`Muxer::klv_handles_for_program`](crate::mpegts::mux::Muxer::klv_handles_for_program).
/// Handles are valid only on the muxer that produced them; passing a handle
/// to a different muxer is rejected with [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle).
///
/// The internal representation encodes `(program_index, within_program_index)`
/// in a packed `u32`. Callers treat this as an opaque token.
///
/// # Lifecycle
///
/// Same rules as [`VideoStreamHandle`]: handles are bound to the producing
/// `Muxer` / `MuxSender`; parent close invalidates the handle's usefulness
/// (the handle remains `Copy + 'static` but cannot be rebound); cloning
/// produces an identical handle referring to the same stream within the
/// same parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KlvStreamHandle(pub(super) u32);

impl_stream_handle!(KlvStreamHandle, StreamKind::Klv, "KlvStreamHandle");

/// Opaque handle to a configured audio stream on a `Muxer`.
///
/// Obtained from [`Muxer::audio_handles`](crate::mpegts::mux::Muxer::audio_handles) / [`Muxer::audio_handles_for_program`](crate::mpegts::mux::Muxer::audio_handles_for_program).
/// Handles are valid only on the muxer that produced them; passing a handle
/// to a different muxer is rejected with [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle).
///
/// The internal representation encodes `(program_index, within_program_index)`
/// in a packed `u32`. Callers treat this as an opaque token.
///
/// # Lifecycle
///
/// Same rules as [`VideoStreamHandle`]: handles are bound to the producing
/// `Muxer` / `MuxSender`; parent close invalidates the handle's usefulness
/// (the handle remains `Copy + 'static` but cannot be rebound); cloning
/// produces an identical handle referring to the same stream within the
/// same parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioStreamHandle(pub(super) u32);

impl_stream_handle!(AudioStreamHandle, StreamKind::Audio, "AudioStreamHandle");

/// Per-program upper bound on subtitle streams. Total program-stream
/// cap with all kinds saturated: ≤16 video + ≤16 KLV + ≤16 audio +
/// ≤16 subtitle + ≤16 data = ≤80; the PMT single-section size budget
/// (`MAX_PMT_SECTION_BYTES`) binds long before the kind caps do.
pub const MAX_SUBTITLE_STREAMS_PER_PROGRAM: usize = 16;

/// Opaque handle to a configured subtitle stream on a `Muxer`.
///
/// Obtained from [`Muxer::subtitle_handles`](crate::mpegts::mux::Muxer::subtitle_handles) /
/// [`Muxer::subtitle_handles_for_program`](crate::mpegts::mux::Muxer::subtitle_handles_for_program). Handles are valid only on
/// the muxer that produced them; passing a handle to a different
/// muxer is rejected with [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle).
///
/// The internal representation encodes `(program_index,
/// within_program_index)` in a packed `u32`. Callers treat this as an
/// opaque token.
///
/// # Lifecycle
///
/// Same rules as [`VideoStreamHandle`]: handles are bound to the producing
/// `Muxer` / `MuxSender`; parent close invalidates the handle's usefulness
/// (the handle remains `Copy + 'static` but cannot be rebound); cloning
/// produces an identical handle referring to the same stream within the
/// same parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubtitleStreamHandle(pub(super) u32);

impl_stream_handle!(
    SubtitleStreamHandle,
    StreamKind::Subtitle,
    "SubtitleStreamHandle"
);

/// Opaque handle to a configured data stream on a `Muxer`.
///
/// Obtained from [`Muxer::data_handles`](crate::mpegts::mux::Muxer::data_handles) /
/// [`Muxer::data_handles_for_program`](crate::mpegts::mux::Muxer::data_handles_for_program).
/// Handles are valid only on the muxer that produced them; passing a handle
/// to a different muxer is rejected with [`MuxError::InvalidStreamHandle`](crate::error::MuxError::InvalidStreamHandle).
///
/// The internal representation encodes `(program_index, within_program_index)`
/// in a packed `u32`. Callers treat this as an opaque token.
///
/// # Lifecycle
///
/// Same rules as [`VideoStreamHandle`]: handles are bound to the producing
/// `Muxer` / `MuxSender`; parent close invalidates the handle's usefulness
/// (the handle remains `Copy + 'static` but cannot be rebound); cloning
/// produces an identical handle referring to the same stream within the
/// same parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataStreamHandle(pub(super) u32);

impl_stream_handle!(DataStreamHandle, StreamKind::Data, "DataStreamHandle");

/// Maximum number of programs in one transport stream multiplex.
/// Mirrors the per-program 16-video + 16-KLV stream caps; far above any
/// realistic gimbaled-platform aggregation use case.
pub const MAX_PROGRAMS: usize = 16;

#[cfg(test)]
mod try_from_raw_tests {
    //! Regression tests for the trust-boundary handle-validation path.
    //! Each `try_from_raw` must reject any
    //! raw value with high bits set outside the 4-bit program + 4-bit
    //! within layout, even if the low byte aliases a valid handle.
    //!
    //! Without `try_from_raw`, the legacy `from_raw` + `unpack` path
    //! silently masks high bits — `valid.raw() | 0x100` would route
    //! the payload to the same elementary stream as `valid.raw()`.
    use super::*;
    use crate::error::MuxError;

    #[test]
    fn video_try_from_raw_rejects_forged_high_bit() {
        let valid = VideoStreamHandle::pack(0, 0);
        let forged = valid.raw() | 0x100;
        match VideoStreamHandle::try_from_raw(forged) {
            Err(MuxError::InvalidStreamHandle { kind, index }) => {
                assert_eq!(kind, StreamKind::Video);
                assert_eq!(index, forged as usize);
            }
            other => panic!("expected InvalidStreamHandle, got {other:?}"),
        }
    }

    #[test]
    fn video_try_from_raw_accepts_canonical_handle() {
        let valid = VideoStreamHandle::pack(0, 0);
        let parsed =
            VideoStreamHandle::try_from_raw(valid.raw()).expect("canonical handle must round-trip");
        assert_eq!(parsed.raw(), valid.raw());
    }

    #[test]
    fn klv_try_from_raw_rejects_forged_high_bit() {
        let valid = KlvStreamHandle::pack(0, 0);
        let forged = valid.raw() | 0x100;
        match KlvStreamHandle::try_from_raw(forged) {
            Err(MuxError::InvalidStreamHandle { kind, index }) => {
                assert_eq!(kind, StreamKind::Klv);
                assert_eq!(index, forged as usize);
            }
            other => panic!("expected InvalidStreamHandle, got {other:?}"),
        }
    }

    #[test]
    fn audio_try_from_raw_rejects_forged_high_bit() {
        let valid = AudioStreamHandle::pack(0, 0);
        let forged = valid.raw() | 0x100;
        match AudioStreamHandle::try_from_raw(forged) {
            Err(MuxError::InvalidStreamHandle { kind, index }) => {
                assert_eq!(kind, StreamKind::Audio);
                assert_eq!(index, forged as usize);
            }
            other => panic!("expected InvalidStreamHandle, got {other:?}"),
        }
    }

    #[test]
    fn subtitle_try_from_raw_rejects_forged_high_bit() {
        let valid = SubtitleStreamHandle::pack(0, 0);
        let forged = valid.raw() | 0x100;
        match SubtitleStreamHandle::try_from_raw(forged) {
            Err(MuxError::InvalidStreamHandle { kind, index }) => {
                assert_eq!(kind, StreamKind::Subtitle);
                assert_eq!(index, forged as usize);
            }
            other => panic!("expected InvalidStreamHandle, got {other:?}"),
        }
    }

    #[test]
    fn data_try_from_raw_rejects_forged_high_bit() {
        let valid = DataStreamHandle::pack(0, 0);
        let forged = valid.raw() | 0x100;
        match DataStreamHandle::try_from_raw(forged) {
            Err(MuxError::InvalidStreamHandle { kind, index }) => {
                assert_eq!(kind, StreamKind::Data);
                assert_eq!(index, forged as usize);
            }
            other => panic!("expected InvalidStreamHandle, got {other:?}"),
        }
    }

    #[test]
    fn try_from_raw_rejects_far_upper_bit() {
        // A 1-bit set in the upper word also rejects — defends against any
        // future encoding shift that pushes the canonical region wider.
        assert!(VideoStreamHandle::try_from_raw(0x0001_0000).is_err());
        assert!(VideoStreamHandle::try_from_raw(u32::MAX).is_err());
    }

    #[test]
    fn try_from_raw_accepts_full_canonical_layout() {
        // (program=0xF, within=0xF) — bit 7 set, bit 8 clear: still
        // within the 8-bit canonical region. Push-time range checks
        // will reject on actual muxer state; validation only filters
        // the layout-violating subset here.
        assert!(VideoStreamHandle::try_from_raw(0xFF).is_ok());
        assert!(KlvStreamHandle::try_from_raw(0xFF).is_ok());
        assert!(AudioStreamHandle::try_from_raw(0xFF).is_ok());
        assert!(SubtitleStreamHandle::try_from_raw(0xFF).is_ok());
        assert!(DataStreamHandle::try_from_raw(0xFF).is_ok());
    }
}
