//! Public event types emitted by `Demuxer`.
//!
//! Independent of the demuxer's internal state — these are the types
//! consumers match on. Adding a future variant (audio codec, subtitle
//! codec, AV1 codec) is additive: add the variant to the appropriate
//! enum, no other public type changes.

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::shared::SharedBytes;

use crate::mpegts::common::{Pts90khz, StreamTypeCode};
pub use crate::mpegts::demux::ts::{AdaptationFieldKind, PcrMalformedKind};

/// Top-level event emitted by `Demuxer::next_event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemuxEvent {
    /// PSI topology — emitted on PAT/PMT discovery and on each PSI
    /// version-bump. Carries declared linkage info with provenance.
    ProgramMap(ProgramMap),

    /// Generic elementary-stream sample. Covers video today; reserves
    /// shape for audio, subtitles, and future PES-carried ES types via
    /// additive variants on `SamplePayload`.
    Sample {
        stream: StreamId,
        pts: Pts90khz,
        dts: Option<Pts90khz>,
        payload: SamplePayload,
    },

    /// Standalone metadata — KLV (sync or async), or any future
    /// metadata-stream pattern.
    Metadata {
        stream: StreamId,
        pts: Pts90khz,
        kind: MetadataKind,
        payload: Vec<u8>,
    },

    /// Continuity discontinuity on a specific PID.
    Discontinuity {
        stream: StreamId,
        kind: DiscontinuityKind,
    },

    /// Non-conformance detected; demuxer continued anyway with a
    /// best-effort interpretation. In `StrictMode::Off` (default) this
    /// surfaces as an event; in stricter modes it converts to
    /// `DemuxError::StrictRejection`.
    NonConformant {
        stream: StreamId,
        issue: NonConformantIssue,
    },

    /// Transport-level reconnect occurred between the prior event and
    /// this one. The owning shell has already dropped sync/PSI/PES
    /// state from the dead connection; all subsequent events are
    /// re-derived from the new connection's byte stream (next PAT,
    /// next PMT, next PUSI). Programs / streams seen pre-reconnect do
    /// NOT carry over; consumers must re-build any per-stream caches
    /// they hold on the next `ProgramMap` event.
    ///
    /// Emitted only by `ManagedDemuxReceiver` shells (in
    /// `tst-pipeline`) that own both the reconnect wrapper and the
    /// demuxer. Plain [`Demuxer::next_event`][crate::mpegts::demux::Demuxer::next_event]
    /// never emits this — it is inserted into the queue by the
    /// owning receive shell before the first post-reconnect event is
    /// yielded.
    ReconnectDiscontinuity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId {
    pub pid: u16,
    pub kind: StreamKind,
    /// Program-number this stream belongs to. Populated by the demuxer from
    /// PMT; provides cross-program identity for multi-program TS where two
    /// programs may reuse a PID (resolution policy is first-program-wins per
    /// CLAUDE.md project conventions).
    pub program_number: u16,
}

impl StreamId {
    /// Fallback `StreamId` for a PID whose kind is not (yet) known to the
    /// demuxer — used when `lookup_stream(pid)` returns `None` (pre-PMT
    /// context or a PSI PID not owned by any program).
    ///
    /// `pub(crate)`: keeps the tst-core public-api baseline stable; consumers
    /// that need to construct a `StreamId` should fill in the real `kind`.
    pub(crate) fn anonymous(pid: u16, program_number: u16) -> Self {
        Self {
            pid,
            kind: StreamKind::Unknown(0),
            program_number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Video(VideoCodec),
    Audio(AudioCodec),
    Subtitle(SubtitleCodec),
    KlvSync { declared_link: Option<u16> },
    KlvAsync,
    Unknown(u8),
}

/// Payload-free discriminant for [`StreamKind`]. Used as the `drop`
/// filter of
/// [`MuxerConfig::from_program_map`](crate::mpegts::mux::MuxerConfig::from_program_map);
/// mirrors the Python `tstrans.mpegts.StreamKindTag` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamKindTag {
    Video,
    Audio,
    Subtitle,
    KlvSync,
    KlvAsync,
    Unknown,
}

impl StreamKind {
    /// The payload-free discriminant of this kind.
    pub fn tag(&self) -> StreamKindTag {
        match self {
            StreamKind::Video(_) => StreamKindTag::Video,
            StreamKind::Audio(_) => StreamKindTag::Audio,
            StreamKind::Subtitle(_) => StreamKindTag::Subtitle,
            StreamKind::KlvSync { .. } => StreamKindTag::KlvSync,
            StreamKind::KlvAsync => StreamKindTag::KlvAsync,
            StreamKind::Unknown(_) => StreamKindTag::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
    /// H.266 / VVC (ITU-T H.266 V4). PMT stream_type = 0x33.
    H266,
    /// AV1 (AOM Bitstream Spec). PMT stream_type = 0x06 with
    /// `registration_descriptor` `format_identifier = "AV01"`.
    Av1,
}

impl From<VideoCodec> for crate::mpegts::mux::VideoCodec {
    /// Demux-side to mux-side codec bridge — the two enums have
    /// identical variants; this exists so receive-side callers can feed
    /// [`crate::codec::misp_time::extract`] without a hand-match.
    fn from(c: VideoCodec) -> Self {
        match c {
            VideoCodec::H264 => Self::H264,
            VideoCodec::H265 => Self::H265,
            VideoCodec::H266 => Self::H266,
            VideoCodec::Av1 => Self::Av1,
        }
    }
}

/// Audio codec carried in `SamplePayload::Audio`. Identifies the codec
/// for typed dispatch but does not parse the bitstream — `frames` holds
/// the raw PES payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    Mp2,
    Aac,
    AacLatm,
    Ac3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubtitleCodec {
    /// DVB subtitling (bitmap-shaped). Per ETSI EN 300 468 §6.2.41 +
    /// ETSI EN 300 743. Per-stream params (language, page IDs)
    /// surface on `StreamInfo::raw_descriptors`; decode lazily via
    /// `parse_subtitling_descriptor`.
    DvbSubtitling,
    /// DVB teletext. Per ETSI EN 300 468 §6.2.43 + ETSI EN 300 706.
    /// Per-stream params (language, magazine, page) surface on
    /// `StreamInfo::raw_descriptors`; decode lazily via
    /// `parse_teletext_descriptor`.
    DvbTeletext,
    /// CEA-708 caption data carried as a separate elementary stream
    /// (rather than embedded in H.264/H.265 SEI). Marked via
    /// registration_descriptor format_identifier "GA94".
    /// **Library-internal round-trip only — external-tool interop has
    /// not been empirically verified as of this writing.** See
    /// `docs/project/deferred-features.md` "CEA-708 interop" for the
    /// empirical-test-pending status.
    Cea708Standalone,
    /// WebVTT cues carried inside MPEG-TS PES. Marked via
    /// registration_descriptor format_identifier "VTTC" (not defined by
    /// any published normative spec — see the `format_identifier_vttc`
    /// rustdoc). **Library-internal round-trip only — external-tool
    /// interop has not been empirically verified as of this writing.**
    /// See `docs/project/deferred-features.md` "WebVTT-in-TS interop" for the
    /// empirical-test-pending status.
    WebVttInTs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SamplePayload {
    Video {
        codec: VideoCodec,
        /// The exact encoded access unit as demuxed (raw-first). The demuxer
        /// no longer splits the video elementary stream during demux; parsing
        /// NAL/OBU units is an opt-in call via
        /// [`split_video`](crate::mpegts::demux::split_video) (or the strict
        /// variant). This mirrors how KLV surfaces raw payload with an
        /// opt-in decode.
        raw: crate::shared::SharedBytes,
        /// True if the TS adaptation field carried `random_access_indicator`
        /// on the PES_start packet for this access unit. Source per ISO/IEC
        /// 13818-1 §2.4.3.4 flags byte bit 6 (0x40). Encoders + muxers set
        /// this on AUs that are decoder-resync points (IDR, CRA, etc.); the
        /// signal is independent of NAL-level type and reflects the
        /// stream-level RA contract.
        random_access_indicator: bool,
        /// AV1 carriage provenance. `Some(mode)` for AV1
        /// samples — the carriage the demuxer was configured for
        /// ([`crate::mpegts::demux::DemuxerConfig::av1_carriage`]); `None` for H.264/H.265/H.266
        /// (carriage is an AV1-only concept). `raw` is the exact on-wire PES
        /// payload regardless: in `Mpeg2TsBinding` mode it is
        /// `ts_open_bitstream_unit()`-framed, in `InteropRawObu` mode it is
        /// raw OBUs. To re-mux it faithfully, configure the destination
        /// muxer's carriage to this value and push `raw` via
        /// [`Muxer::push_video_wire_to`](crate::mpegts::mux::Muxer::push_video_wire_to).
        /// To parse it, pass this carriage to
        /// [`split_video`](crate::mpegts::demux::split_video).
        av1_carriage: Option<crate::mpegts::mux::Av1CarriageMode>,
    },
    Audio {
        codec: AudioCodec,
        frames: crate::shared::SharedBytes,
    },
    Subtitle {
        codec: SubtitleCodec,
        payload: crate::shared::SharedBytes,
    },
    Unknown {
        stream_type: StreamTypeCode,
        raw: crate::shared::SharedBytes,
    },
}

/// Codec-specific bitstream payload shape.
///
/// `Nals` covers Annex-B NAL-shaped codecs (H.264, H.265, H.266).
/// `Obus` covers AV1's Open Bitstream Unit format. The variant is
/// determined by [`VideoCodec`] on the parent [`SamplePayload::Video`]
/// event:
///
/// * `codec ∈ {H264, H265, H266}` ⇒ `payload = Nals(_)`
/// * `codec = Av1`                 ⇒ `payload = Obus(_)`
///
/// The demuxer enforces this invariant by construction; consumers
/// can match on `payload` after dispatching on `codec`, or vice versa,
/// and assume the mapping holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoPayload {
    /// H.264, H.265, H.266 — Annex-B NAL-shaped.
    Nals(Vec<NalUnit>),
    /// AV1 — OBU-shaped (Open Bitstream Unit).
    Obus(Vec<Obu>),
}

/// One AV1 Open Bitstream Unit. Per AV1 Bitstream Spec §5.3.2.
///
/// The header byte (`obu_forbidden_bit | obu_type | obu_extension_flag |
/// obu_has_size_field | obu_reserved_1bit`) and any extension byte are
/// parsed during split; the LEB128 `obu_size` field is consumed and
/// stripped. `payload` carries only the OBU body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obu {
    /// 4-bit `obu_type` (AV1 §5.3.2). 1=SequenceHeader, 2=TemporalDelimiter,
    /// 3=FrameHeader, 4=TileGroup, 5=Metadata, 6=Frame,
    /// 7=RedundantFrameHeader, 8=TileList, 15=Padding.
    pub obu_type: u8,
    /// Optional extension header bytes when `obu_extension_flag = 1`.
    pub extension: Option<ObuExtension>,
    /// OBU payload bytes — header + extension byte + LEB128 size field
    /// stripped. Pass-through; parsed by `codec::av1::parse_*` if the
    /// consumer wants typed fields.
    pub payload: SharedBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObuExtension {
    /// 3-bit `temporal_id` (AV1 §5.3.3).
    pub temporal_id: u8,
    /// 2-bit `spatial_id` (AV1 §5.3.3).
    pub spatial_id: u8,
}

/// One H.264 or H.265 NAL unit. Codec-tagged so wrapped languages
/// (Kotlin, Swift, Java) get idiomatic `when`/`switch` exhaustiveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NalUnit {
    H264 {
        /// 5-bit `nal_unit_type` (H.264 §7.3.1).
        nal_type: u8,
        /// 2-bit `nal_ref_idc` (H.264 §7.3.1).
        ref_idc: u8,
        /// RBSP bytes; Annex-B start codes stripped, emulation prevention
        /// bytes preserved (consumer's decoder removes them).
        payload: SharedBytes,
    },
    H265 {
        /// 6-bit `nal_unit_type` (H.265 §7.3.1.2).
        nal_type: u8,
        /// 6-bit `nuh_layer_id` (H.265 §7.3.1.2).
        layer_id: u8,
        /// 3-bit `nuh_temporal_id_plus1` (H.265 §7.3.1.2).
        temporal_id_plus1: u8,
        /// RBSP bytes; same stripping/preservation rules as `H264`.
        payload: SharedBytes,
    },
    /// One H.266 / VVC NAL unit. Header parsed per H.266 V4 §7.3.1.2.
    ///
    /// Common `nal_type` values: VPS_NUT=14, SPS_NUT=15, PPS_NUT=16,
    /// PREFIX_APS_NUT=17, SUFFIX_APS_NUT=18, PH_NUT=19, AUD_NUT=20,
    /// IDR_W_RADL=7, IDR_N_LP=8, CRA_NUT=9, GDR_NUT=10. Full table at
    /// H.266 V4 Table 5.
    H266 {
        /// 5-bit `nal_unit_type` (H.266 V4 §7.3.1.2).
        nal_type: u8,
        /// 6-bit `nuh_layer_id` (H.266 V4 §7.3.1.2).
        layer_id: u8,
        /// 3-bit `nuh_temporal_id_plus1` (H.266 V4 §7.3.1.2).
        temporal_id_plus1: u8,
        /// RBSP bytes; Annex-B start codes stripped, emulation
        /// prevention preserved (consumer's decoder removes 0x03 escapes).
        payload: SharedBytes,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataKind {
    /// H.222.0 V9 §2.12.4.2 Metadata_AU_cell — sync metadata. The demuxer
    /// has peeled the 5-byte AU cell header; `payload` (on the parent
    /// event) is the inner KLV LS ready to feed to `klv::st0601::decode`.
    /// The parent event's `pts` is the PES PTS (per H.222.0 §2.12.4.1 —
    /// the AU cell carries no embedded timestamp).
    ///
    /// Field names match the spec (Table 2-156) verbatim for FFI
    /// traceability across `tst-c` / `tst-jni` / `tst-uniffi` wrappers.
    KlvSyncAuCell {
        /// `metadata_service_id` u8. ST 1402.2 App. B Table 2: `0x00` typical.
        metadata_service_id: u8,
        /// 8-bit cell counter wrapping mod 256. Useful for loss detection on
        /// the metadata path (gaps in the sequence indicate dropped cells).
        sequence_number: u8,
        /// Cell fragmentation per H.222.0 Table 2-157. Always `Complete`
        /// on emitted samples: reassembled multi-cell AUs collapse into
        /// one event (see [`Self::KlvSyncAuCell::was_reassembled`]).
        cell_fragment_indication: crate::mpegts::au_cell::CellFragmentIndication,
        /// True if this cell carries decoder configuration data per the
        /// H.222.0 §2.12.4.2 definition. The current muxer never sets this;
        /// surfaced for receivers consuming streams from other sources.
        decoder_config_flag: bool,
        /// True if this cell is an entry point — decoding is possible
        /// without information from previous cells. Meaning is metadata-
        /// format-defined; for ST 0601 LS payloads (self-contained per
        /// record) this is typically `true` on every cell.
        random_access_indicator: bool,
        /// `true` if this event represents a multi-cell AU that the
        /// demuxer reassembled from `First` + 0..n `Middle` + `Last`
        /// cells. `false` for single-cell (Complete) AUs.
        was_reassembled: bool,
        /// Number of AU cells that contributed to this event. `1` for
        /// single-cell (Complete) AUs; `≥ 2` for reassembled AUs.
        cell_count: u32,
    },

    /// Bare KLV LS (no AU cell wrap). Async metadata, typically 1–10 Hz.
    /// `pts` on the parent event is the PES PTS.
    KlvAsync,

    /// Unrecognized metadata `stream_type`. `payload` (on the parent
    /// event) is raw PES payload. The inner [`StreamTypeCode`] preserves
    /// the raw PMT byte for forward-compat; bindings that recognize
    /// future metadata-stream types can pattern-match on
    /// `StreamTypeCode::Known(StreamType::*)`.
    Unknown(StreamTypeCode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramMap {
    pub program_number: u16,
    pub pcr_pid: u16,
    /// PID carrying this program's PMT — from the PAT entry that declared
    /// the program. Needed to reconstruct a muxer config
    /// ([`MuxerConfig::from_program_map`](crate::mpegts::mux::MuxerConfig::from_program_map));
    /// not otherwise recoverable from the emitted events.
    pub pmt_pid: u16,
    pub streams: Vec<StreamInfo>,
    pub klv_links: Vec<KlvLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub pid: u16,
    /// PMT `stream_type` byte wrapped as a typed [`StreamTypeCode`].
    /// Use [`StreamTypeCode::known`] to peel off the typed [`crate::mpegts::common::StreamType`]
    /// variant for recognized codes, or [`StreamTypeCode::as_byte`] to
    /// recover the raw PMT byte (e.g., for C ABI marshalling — `tstrans.h`
    /// exposes this as `uint8_t`).
    pub stream_type: StreamTypeCode,
    pub kind: StreamKind,
    /// Program number from the PAT entry whose PMT owns this stream.
    /// Apps filtering `Sample`/`Metadata` events by program can build a
    /// `pid → program_number` map from `ProgramMap` events.
    pub program_number: u16,
    /// Raw PMT per-stream descriptors for this PID, in PMT loop order.
    /// Empty when the PMT carried no descriptors for this stream. Use
    /// [`crate::mpegts::demux::low_level::extract_user_label`] for a quick
    /// label decode; reach into this list for vendor-specific or
    /// stack-shape (Family B) decoding.
    pub raw_descriptors: Vec<crate::mpegts::descriptors::RawDescriptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KlvLink {
    pub klv_pid: u16,
    pub video_pid: u16,
    pub source: LinkSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSource {
    /// `metadata_descriptor` in the PMT explicitly linked these PIDs.
    Declared,
    /// Demuxer inferred the link from topology (e.g., one video + one
    /// metadata PID with no descriptor). Treat as a hint, not authority.
    Inferred,
    /// Caller provided the link via `DemuxerConfigBuilder::link_klv`.
    Override,
}

/// Why a multi-cell AU reassembly attempt did not produce a `Sample`.
///
/// Surfaced via [`NonConformantIssue::MultiCellAu::reason`]. Each variant
/// names a distinct wire-format or operational failure mode the
/// per-PID `AuCellReassembler` can hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MultiCellAuReason {
    /// A continuation cell (`Middle` or `Last`) arrived without a prior
    /// `First`. Either the stream started mid-AU (e.g. seek into a recording)
    /// or a `First` cell was lost upstream.
    Orphan,
    /// A continuation cell arrived but its `sequence_number` did not equal
    /// `(first.sequence_number + cells_seen) mod 256`. Indicates a cell was
    /// lost between the buffered `First`/`Middle` and the arriving cell.
    SequenceGap,
    /// A new `First` cell arrived while the previous AU was still being
    /// buffered (i.e. its `Last` never appeared). The partial buffer is
    /// dropped before the new `First` is processed.
    ConcurrentFirst,
    /// The buffered AU's accumulated inner bytes would exceed
    /// [`crate::mpegts::demux::DemuxerConfig::au_cell_cap_per_pid`]
    /// (default 1 MiB). The partial buffer is dropped.
    Overflow,
    /// The aggregate in-flight AU-cell bytes across all PIDs would exceed
    /// [`crate::mpegts::demux::DemuxerConfig::au_cell_cap_total`]
    /// (default 16 MiB). Defends a multi-PID flood where each PID stays
    /// under its own per-PID cap but the total explodes. The offending
    /// PID's partial buffer is dropped.
    OverflowTotal,
    /// A new `First` cell would open reassembly on a PID beyond
    /// [`crate::mpegts::demux::DemuxerConfig::au_cell_max_in_flight_pids`]
    /// (default 64) concurrently in-flight PIDs. Bounds active-PID count
    /// against an adversary that opens a `First` for thousands of distinct
    /// PIDs and never sends `Last`. The new cell is rejected; existing
    /// in-flight reassemblies are left intact.
    TooManyPids,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonConformantIssue {
    /// `stream_type=0x06` carries Metadata_AU_cell payload (H.222.0 V9 §2.12.4.2,
    /// also defined in ST 1402.2 §9.4.1); treated as sync KLV.
    StreamTypeMismatchSyncOnAsyncPid,
    /// `stream_type=0x15` carries bare KLV without AU cell wrap; treated as async KLV.
    StreamTypeMismatchAsyncOnSyncPid,
    /// `metadata_descriptor` missing on a sync-KLV-shaped PID.
    MissingMetadataDescriptor,
    /// PCR jump or stream-monotonic timing inconsistency.
    ///
    /// `delta` is the signed difference in **27 MHz ticks** between the
    /// new PCR and the previously observed PCR on the same PID, computed
    /// via [`pcr_diff_27mhz`](crate::mpegts::common::pcr_diff_27mhz). A
    /// large-magnitude delta indicates a discontinuous jump (forward seek,
    /// missed packets, or non-conformant encoder). Convert to seconds by
    /// dividing by [`PCR_TICKS_PER_SECOND`](crate::mpegts::common::PCR_TICKS_PER_SECOND).
    PcrAnomaly { delta: i64 },
    /// PSI section checksum mismatch. Lenient mode falls back to the
    /// previous PSI version; strict mode converts to error.
    PsiChecksumMismatch { pid: u16 },
    /// PUSI mid-PES — a new PUSI packet arrived before the previous PES
    /// completed.
    ///
    /// **Vestigial — never emitted by [`Demuxer`](crate::mpegts::demux::Demuxer).**
    /// The PES reassembler silently discards any in-flight partial PES when a
    /// new PUSI arrives; this is the normal start-of-PES path, not a separate
    /// diagnostic event. The variant is preserved for non-exhaustive-enum
    /// binary-compatibility parity only (same zero-emit-site status as
    /// `DemuxException.Kind::UNEXPECTED_EOF` in the Python binding).
    PusiMidPes,
    /// PES header parse failed. Lenient mode: receiver continues parsing
    /// subsequent packets. Strict modes: escalates to `DemuxError`.
    MalformedPes { pid: u16, reason: &'static str },
    /// A PMT introduced a stream PID that's already bound to a different
    /// program. PID uniqueness across programs is required by ISO 13818-1;
    /// the demuxer keeps the first-program-wins binding and drops the second.
    PidReusedAcrossPrograms { pid: u16, programs: [u16; 2] },

    /// `treat_as` (or fallback) routed a PID to a subtitle codec but no
    /// recognized subtitle descriptor (subtitling/teletext/VTTC/GA94) is
    /// present on the PMT entry. Lenient mode classifies anyway; strict
    /// mode converts to `DemuxError::StrictRejection`.
    SubtitleMissingDescriptor { pid: u16 },

    /// PMT entry on `stream_type=0x06` carries more than one recognized
    /// subtitle codec marker — e.g. both `subtitling_descriptor` (0x59) and
    /// `registration_descriptor` with `format_identifier="VTTC"`. The
    /// classification cascade keeps its first-match priority order
    /// (subtitling > teletext > VTTC > GA94 > KLVA), but downstream
    /// consumers may want to know about the ambiguity for diagnostics.
    ///
    /// `tags` lists the recognized markers found on the PID, using
    /// descriptor tag bytes for tag-presence matches (`0x59`, `0x56`,
    /// `0x46`) and synthetic codepoints for `format_identifier` matches
    /// (`0xF0` = VTTC, `0xF1` = GA94, `0xF2` = KLVA).
    SubtitleDescriptorAmbiguous { pid: u16, tags: Vec<u8> },

    /// Subtitle descriptor tag was recognized but the inner length /
    /// payload bytes did not satisfy spec invariants. Per-stream params
    /// fall back to defaults (language `*b"und"`, page ids 0).
    ///
    /// **Vestigial — not currently emitted by [`Demuxer`](crate::mpegts::demux::Demuxer).**
    /// The classification cascade is tag-presence-based via
    /// `find_descriptor_tag`, so malformed descriptor bodies pass through.
    /// Typed-parser integration in the cascade — and therefore this
    /// variant's first emission — is deferred to the typed WebVTT cue /
    /// DVB-sub data-segment / teletext data-unit substrate session. Unlike
    /// [`Self::PusiMidPes`], this variant is not removable-in-spirit: it's
    /// on the C ABI (`TST_NONCONFORMANT_CODE_SUBTITLE_DESCRIPTOR_MALFORMED = 9`)
    /// and every binding's label table, so it stays reserved for
    /// cross-binding parity until the deferred typed-parser work gives it
    /// a real emission site.
    SubtitleDescriptorMalformed { pid: u16, tag: u8 },

    /// AV1 stream's PMT entry has a malformed `registration_descriptor`
    /// for `format_identifier "AV01"` — length byte mismatches payload.
    Av1RegistrationMalformed { pid: u16 },

    /// AV1 OBU encountered with `obu_has_size_field = 0`. AV1 in MPEG-2 TS
    /// binding §3.1 (linked through AV1 spec §5.2 "low overhead bitstream
    /// format") requires `=1`. Streams violating this can't be split
    /// reliably; the splitter places remaining bytes into one trailing
    /// `Obu` and stops walking.
    Av1ObuMissingSizeField { pid: u16, obu_type: u8 },

    /// Tile List OBU (`obu_type = 8`) encountered. Forbidden by AV1 in
    /// MPEG-2 TS binding §3.3. Lenient mode passes through; strict mode
    /// rejects.
    Av1TileListNotAllowed { pid: u16 },

    /// Per ISO/IEC 13818-1 §2.4.4.6 short-form sections cap at 1021 bytes;
    /// long-form private sections at 4093. ffmpeg caps the assembler at
    /// 4096 (`MAX_SECTION_SIZE`). A section that grows past this cap (either
    /// declared overlong, or accumulated past it without a closing length)
    /// triggers this issue. The partial section is discarded.
    ///
    /// `observed_len` is the byte count at the point of the cap fire — useful
    /// for telemetry, distinguishing "sender declared too long" from "CC-driven
    /// corruption with no closing length".
    ///
    /// Note: this variant deliberately consolidates two underlying error
    /// shapes (declared `section_length` exceeds cap, vs. accumulated bytes
    /// exceed cap before any length is seen). `observed_len` reflects the
    /// full overshoot in either case. Split the variant if telemetry
    /// consumers ever need to distinguish them.
    PsiOverlongSection { pid: u16, observed_len: usize },

    /// Per ISO/IEC 13818-1 §2.4.3.2, bit 0x80 of TS byte 1
    /// (`transport_error_indicator`) marks a packet as link-layer-corrupt
    /// (ATSC FEC, satellite demod, CMTS, etc.). The demuxer drops the packet
    /// (does not feed payload to PES/PSI reassembly) and emits this issue so
    /// consumers can correlate with downstream parse failures or surface to
    /// telemetry. Matches ffmpeg's `AV_PKT_FLAG_CORRUPT` in
    /// `mpegts.c:3091-3097`.
    TransportErrorPacket { pid: u16 },

    /// DVB-subtitle PES_data_field carries a `data_identifier` byte other
    /// than `0x20`. Per ETSI EN 300 743 §6.2 Table 3 the binding for
    /// DVB subtitling streams is exactly `0x20`. The broader range
    /// `0x20..=0x3F | 0x70..=0x7F` cited by EN 300 743 §7.1 covers
    /// PES_data_field carriage in general (with extensions for future use);
    /// for DVB-subtitle PIDs specifically, only `0x20` is conformant.
    ///
    /// `observed` is the byte found at offset 0 of the PES payload — useful
    /// for telemetry (e.g., distinguishing "off-by-one in caller's encoder"
    /// from "wrong subtitle binding entirely").
    ///
    /// Lenient mode (`StrictMode::Off`): the demuxer continues to strip the
    /// envelope (matching today's permissive behavior) and emits a
    /// `Sample` event alongside this `NonConformant`. Strict mode
    /// (`StrictMode::Full`): the demuxer suppresses the `Sample` event and
    /// the issue propagates as a `DemuxError::StrictRejection`.
    DvbSubDataIdentifier { observed: u8 },

    /// PTS anomaly distinct from PCR anomaly (validate-1 B4). Per ITU-T
    /// H.222.0 V9 §2.4.3.6 the PTS clock is per-PES and must be
    /// monotonically non-decreasing on a given elementary stream PID
    /// (modulo the 33-bit wrap). A backward jump means either an
    /// upstream re-mux mishap, a non-conformant encoder, or packet
    /// drops between PES boundaries.
    ///
    /// `delta` is the signed difference in **90 kHz ticks** between the
    /// new PTS and the previously observed PTS on the same PID
    /// (computed via [`pts_diff_33bit`](crate::mpegts::common::pts_diff_33bit)).
    /// Convert to seconds by dividing by 90_000.
    ///
    /// Distinct from [`NonConformantIssue::PcrAnomaly`] — the PCR
    /// anomaly uses 27 MHz ticks and lives on the program's PCR PID;
    /// this PTS anomaly uses 90 kHz ticks and lives on the elementary
    /// stream PID.
    PtsAnomaly { delta: i64 },

    /// A PES on a stream type that requires PTS (audio or video per
    /// H.222.0 V9 §2.7.4) arrived without one. The demuxer cannot
    /// timestamp the sample; lenient mode emits the sample with PTS=0
    /// and surfaces this issue. Strict mode (`StrictMode::Full`) rejects.
    MissingRequiredPts { pid: u16 },

    /// PES header structural validation failure (validate-1 B5). Catches
    /// spec violations the prior "too short for header" check missed.
    /// See [`PesHeaderMalformedKind`] for the specific violation.
    PesHeaderMalformed {
        pid: u16,
        kind: PesHeaderMalformedKind,
    },

    /// DVB subtitle or teletext PES arrived with
    /// `data_alignment_indicator = 0` (validate-1 B6). Per ETSI EN 300
    /// 743 §6.2 and EN 300 472 §4.2, subtitle streams MUST set this
    /// flag (one complete composition page / one teletext block per
    /// PES). Lenient mode emits the sample anyway; strict mode rejects.
    SubtitleAlignmentMissing { pid: u16 },

    /// PCR field decoded from the adaptation field violated ITU-T H.222.0
    /// §2.4.3.5 syntax (six reserved bits not all 1, or
    /// `program_clock_reference_extension > 299`). Surfaced separately from
    /// [`Self::PcrAnomaly`] because the latter compares values across
    /// packets while this fires on a single packet's on-wire syntax.
    ///
    /// Lenient mode (`StrictMode::Off`): the malformed PCR is dropped (the
    /// demuxer does not feed it into [`Self::PcrAnomaly`] detection or the
    /// `last_pcr_by_pid` map) so a single corrupt packet can't seed bogus
    /// timing tracking on a PID. Strict-mode timing categories
    /// (`StrictMode::TimingOnly`, `StrictMode::Full`) escalate this issue
    /// to `DemuxError::StrictRejection`.
    PcrMalformed { kind: PcrMalformedKind },

    /// NAL header constraint violation per H.264 §7.3.1 / H.265 §7.3.1.2 /
    /// H.266 V4 §7.3.1.2. The demuxer detected a NAL whose header bits
    /// violate a spec-mandated constraint (`forbidden_zero_bit`, reserved
    /// bits, `temporal_id_plus1`, layer-id range).
    ///
    /// `codec` identifies which codec's constraint table was violated.
    /// `kind` carries the specific violation; see [`NalHeaderKind`].
    ///
    /// Lenient mode (`StrictMode::Off`): the demuxer continues. For most
    /// violations the offending NAL is still surfaced on the `Sample`
    /// event. For H.266 `ReservedBit` and `LayerIdOutOfRange { id > 55 }`
    /// the H.266 spec mandates **discard**, so lenient mode drops the
    /// NAL but still emits the issue. Strict mode (`StrictMode::Full`):
    /// the issue escalates to `DemuxError::StrictRejection` and the
    /// `Sample` event is suppressed.
    NalHeader {
        codec: VideoCodec,
        kind: NalHeaderKind,
    },

    /// AV1 OBU header constraint violation per AV1 Bitstream Spec §5.3.2.
    /// `obu_forbidden_bit`, `obu_reserved_1bit`, or the 3 reserved bits
    /// on the OBU extension header are non-zero on a parsed OBU.
    ///
    /// `kind` carries the specific violation; see [`Av1ObuHeaderKind`].
    /// `pid` is patched in by [`Demuxer`](crate::mpegts::demux::Demuxer)
    /// before queue-time (the split layer uses sentinel `0`).
    ///
    /// Lenient mode (`StrictMode::Off`): the demuxer continues and the OBU
    /// surfaces on the `Sample` event. Strict mode (`StrictMode::Full`):
    /// the issue escalates to `DemuxError::StrictRejection`.
    Av1ObuHeader { pid: u16, kind: Av1ObuHeaderKind },

    /// AAC-LATM PES (stream_type `0x11`) framing violation
    /// (validate-1 C11). Per ISO/IEC 14496-3 §1.7 + H.222.0 Table 2-34,
    /// each PES on a LATM-advertising PID MUST begin with a 24-bit LOAS
    /// header (`syncword=0x2B7` + 13-bit `audioMuxLengthBytes`).
    ///
    /// `kind` carries the specific violation;
    /// see [`crate::codec::aac::latm::LatmFramingKind`].
    ///
    /// Lenient mode (`StrictMode::Off`): the demuxer continues and emits
    /// the `Sample` event alongside this issue (consumers may still want
    /// the bytes for forensic analysis). Strict mode (`StrictMode::Full`):
    /// the issue escalates to `DemuxError::StrictRejection` and the
    /// `Sample` event is suppressed.
    LatmFraming {
        pid: u16,
        kind: crate::codec::aac::latm::LatmFramingKind,
    },

    /// PSI section reassembly observed a continuity-counter jump on a
    /// continuation packet. Per ISO/IEC 13818-1 §2.4.3.3 PSI continuation
    /// packets must increment the CC; a jump means an upstream packet drop.
    /// Plan #29 strict mode (`DemuxerConfig::lenient_psi_reassembly = false`,
    /// the default) drops the partial section and emits this issue,
    /// matching ffmpeg's `mpegts.c:3118-3142` behavior. Lenient mode keeps
    /// today's behavior of feeding the bytes through; the section then
    /// either passes by luck or surfaces as `PsiChecksumMismatch`.
    PsiCcDiscontinuity {
        pid: u16,
        expected: u8,
        observed: u8,
    },

    /// A multi-cell AU reassembly attempt failed and the partial inner
    /// payload was dropped. `reason` names the specific failure mode;
    /// `dropped_bytes` is the cumulative inner-byte count discarded
    /// (useful for telemetry — quantifies what was lost).
    ///
    /// On the happy path (reassembly succeeded → `DemuxEvent::Sample`
    /// with `MetadataKind::KlvSyncAuCell { was_reassembled: true, .. }`)
    /// this event is NOT emitted.
    MultiCellAu {
        pid: u16,
        dropped_bytes: usize,
        reason: MultiCellAuReason,
    },

    /// A sync-metadata AU cell arrived with `cell_fragment_indication`
    /// set to `0b00` (Middle) or `0b01` (Last) when no prior `First`
    /// cell was buffered for the PID, AND the demuxer was configured
    /// with [`crate::mpegts::demux::DemuxerConfig::cfi_tolerance`]
    /// `= true`, AND the cell's inner payload independently validated
    /// as a single complete KLV unit (SMPTE 336M UL prefix
    /// `06 0e 2b 34` followed by a BER length describing exactly the
    /// available payload).
    ///
    /// The demuxer emitted the cell as a
    /// [`MetadataKind::KlvSyncAuCell`] event with
    /// `cell_fragment_indication = Complete` AND this diagnostic.
    /// Without the opt-in tolerance knob, the cell would have surfaced
    /// only as [`Self::MultiCellAu`] `{ reason = MultiCellAuReason::Orphan }`.
    ///
    /// Per H.222.0 V9 §2.12.4.2 Table 2-157, only `cfi_bits = 0b11`
    /// indicates a single complete cell. Producers that emit `0b00`
    /// (middle) or `0b01` (last) for a single complete payload are
    /// non-conformant; this diagnostic surfaces every tolerated cell
    /// so downstream consumers can quantify the malformation, log it,
    /// or surface it to telemetry.
    ///
    /// `pid` is the elementary stream PID. `observed_cfi` is the wire
    /// value the demuxer read. `treated_as` is the value the demuxer
    /// substituted (always [`crate::mpegts::au_cell::CellFragmentIndication::Complete`]
    /// today).
    CfiTolerated {
        pid: u16,
        observed_cfi: crate::mpegts::au_cell::CellFragmentIndication,
        treated_as: crate::mpegts::au_cell::CellFragmentIndication,
    },

    /// Per ISO/IEC 13818-1 §2.4.4.5, PSI tables may be split across
    /// multiple sections (the table's `last_section_number > 0`).
    /// Current demuxer scope reassembles single-section tables only;
    /// multi-section PAT/PMT tables are rejected and the partial
    /// section is dropped. Surfaced once per section (the assembler
    /// finalizes per declared section_length, so each rejected
    /// section emits one event).
    ///
    /// Real-world MISB-shaped ISR streams pack PAT/PMT into a single
    /// section well under the 1021-byte short-form cap. This variant
    /// fires only on high-program-count or descriptor-heavy streams
    /// (e.g. > ~250 programs in a PAT, or a PMT with many typed
    /// streams + descriptors). Full §2.4.4.5 reassembly is deferred
    /// until a real consumer needs it.
    ///
    /// `pid` is the PID the rejected section arrived on (0x0000 for
    /// PAT, the PMT PID for PMT). `table_id` is `0x00` (PAT) or
    /// `0x02` (PMT). `last_section_number` is the spec's
    /// `last_section_number` field — the count of additional
    /// sections the demuxer would need to assemble to materialize
    /// the full table.
    PsiMultiSectionUnsupported {
        pid: u16,
        table_id: u8,
        last_section_number: u8,
    },

    /// AC-3 PES on a stream type `0x81` (System A) arrived with
    /// `data_alignment_indicator = 1` but the payload does not begin
    /// with the AC-3 syncword `0x0B77` (validate-1 C12). Per ATSC
    /// A/52:2018 §A.6.3, every AC-3 PES with the alignment flag set
    /// MUST start with a syncframe; receivers gating on the flag may
    /// drop or mis-decode misaligned payloads.
    ///
    /// Lenient mode emits the sample alongside this issue; strict mode
    /// (`StrictMode::Full`) suppresses the sample so receivers can
    /// fail closed.
    ///
    /// `pid` is the stream PID.
    Ac3SyncMissing { pid: u16 },

    /// AV1-in-MPEG-2-TS binding §3.4 violation — AV1 PES arrived with
    /// `stream_id` other than `0xBD` (private_stream_1).
    ///
    /// Emitted only when the demuxer is configured for binding-conformant
    /// AV1 carriage (`DemuxerConfig::av1_carriage == Av1CarriageMode::Mpeg2TsBinding`,
    /// the default). In `InteropRawObu` mode the demuxer accepts
    /// `stream_id=0xE0` without raising this issue.
    ///
    /// `observed` is the actual PES `stream_id` byte. Lenient mode
    /// (`StrictMode::Off`): the demuxer continues to dispatch the PES.
    /// Strict mode (`StrictMode::Full`): escalates to `DemuxError::StrictRejection`.
    Av1WrongStreamId { pid: u16, observed: u8 },

    /// AV1-in-MPEG-2-TS binding §3.2 violation — AV1 PES payload did not
    /// begin with a `ts_open_bitstream_unit()` start code
    /// (`0x00 0x00 0x01`, the 3-byte `obu_start_code` = `uimsbf(24)` =
    /// `0x000001` per the binding syntax table).
    ///
    /// Raised by the opt-in
    /// [`split_video`](crate::mpegts::demux::split_video) parse (NOT by the
    /// demuxer, which is raw-first for video and never inspects the OBU
    /// framing). When `split_video` finds no binding start code it falls back
    /// to raw-OBU parsing — recovering the OBUs for interop-shaped carriage —
    /// and includes this issue in its returned issue list. There is no
    /// `StrictMode` gating: the demuxer's strict modes no longer apply to
    /// video ES content (use
    /// [`split_video_strict`](crate::mpegts::demux::split_video_strict) for a
    /// fail-fast parse). `pid` is `0` — the opt-in parse path has no PID
    /// context.
    Av1MissingTsObuFraming { pid: u16 },

    /// A PMT section's body `program_number` (H.222.0 §2.4.4.8) does not match
    /// the `program_number` the PAT (§2.4.4.4) assigned to this PMT PID. The
    /// mislabeled topology is NOT adopted. REF-PSI-01.
    PmtProgramNumberMismatch {
        /// The PMT PID on which the mislabeled section arrived.
        pid: u16,
        /// The `program_number` the PAT assigned to this PMT PID.
        pat_program: u16,
        /// The `program_number` found in the PMT body — the mislabeled value.
        pmt_program: u16,
    },

    /// Per ISO/IEC 13818-1 §2.4.3.2, `transport_scrambling_control` (bits
    /// 7-6 of TS byte 3) was non-zero — the payload is scrambled. The
    /// library does not descramble; the demuxer drops the packet (does NOT
    /// route the payload to PSI/PES reassembly, so scrambled bytes can't
    /// corrupt PSI/PES/codec/metadata state) and surfaces this issue.
    /// `StrictMode::Full` rejects. A subsequent clear (TSC=0) packet on the
    /// same PID recovers normally. REF-TS-01.
    UnsupportedScrambling {
        /// The PID the scrambled packet arrived on.
        pid: u16,
        /// The 2-bit `transport_scrambling_control` value (1, 2, or 3).
        control: u8,
    },

    /// The adaptation field's control/length combination violated H.222.0
    /// §2.4.3.2/§2.4.3.5 (reserved control 00, wrong length for the control
    /// value, or a PCR flag with too few bytes). See [`AdaptationFieldKind`].
    /// Lenient mode surfaces this and continues best-effort (control 00 routes
    /// no payload by construction); `StrictMode::Full` rejects. REF-TS-02.
    AdaptationFieldMalformed {
        pid: u16,
        kind: crate::mpegts::demux::ts::AdaptationFieldKind,
    },

    /// A PES with zero `PES_packet_length` (unbounded) arrived on a
    /// non-video stream. H.222.0 §2.4.3.7 permits zero only for a video
    /// elementary stream; on audio/KLV/subtitle/private streams an unbounded
    /// PES would buffer until the next PUSI / cap and flush a bogus sample.
    /// The demuxer drops the partial and surfaces this; `StrictMode::Full`
    /// rejects. `stream_id` is the PES stream_id byte. REF-PES-01.
    ZeroLengthPesNonVideo { pid: u16, stream_id: u8 },

    /// A PAT/PMT section violated a fixed/reserved syntax field per H.222.0
    /// §2.4.4. `section_syntax_indicator != 1` and `section_number != 0`
    /// (on a single-section table) are validated in every mode; reserved-bit
    /// violations are validated only when `StrictMode != Off` (real muxers set
    /// reserved bits inconsistently, so always-on checking would false-positive
    /// in default/lenient mode). `StrictMode::Full` rejects. `table_id` is the
    /// PSI table_id (0x00 PAT, 0x02 PMT). REF-PSI-03.
    PsiSyntax {
        pid: u16,
        table_id: u8,
        kind: PsiSyntaxKind,
    },

    /// Other.
    Other(String),
}

/// Specific PES header structural violations detected by the demuxer
/// per ITU-T H.222.0 V9 §2.4.3.6.
///
/// All PES headers carry a fixed-shape prefix with marker bits, a
/// `PTS_DTS_flags` selector, and (when PTS/DTS are present) 5-byte
/// PTS/DTS fields with 4-bit prefix + 3 marker bits. Violations of any
/// of these forms produce different repair strategies, so the variants
/// are kept distinct for telemetry.
///
/// `#[non_exhaustive]` per workspace convention — new violations
/// (e.g. ESCR marker checks if we ever decode that field) will be
/// added without breaking matchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PesHeaderMalformedKind {
    /// `PTS_DTS_flags == 0b01` — H.222.0 §2.4.3.7 marks this combination
    /// as "forbidden" (DTS-only with no PTS is not a valid shape).
    ForbiddenPtsDtsFlags,
    /// Byte 6 (`flags1`) of the PES header has high bits `!= 0b10`. The
    /// top two bits are the standard marker (`'10'b` per §2.4.3.6).
    InvalidMarkerBits,
    /// First nibble of the PTS 5-byte field does not match the expected
    /// prefix: `0b0010` (PTS-only) or `0b0011` (PTS in PTS+DTS combo).
    /// Per H.222.0 §2.4.3.7.
    InvalidPtsPrefix,
    /// First nibble of the DTS 5-byte field does not match the expected
    /// prefix `0b0001`. Per H.222.0 §2.4.3.7.
    InvalidDtsPrefix,
    /// One of the three trailing marker bits inside a 5-byte PTS or DTS
    /// field is `0`. Each must be `1` per H.222.0 §2.4.3.7.
    InvalidPtsDtsMarkerBits,
}

/// Which fixed/reserved PSI syntax field a PAT/PMT section violated.
/// Carried inside [`NonConformantIssue::PsiSyntax`]. H.222.0 §2.4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PsiSyntaxKind {
    /// `section_syntax_indicator` (bit 0x80 of section byte 1) was 0;
    /// PAT/PMT long-form sections require 1.
    SectionSyntaxIndicatorUnset,
    /// `section_number` (section byte 6) was non-zero on a single-section
    /// table (one whose `last_section_number == 0`).
    SectionNumberNonZero { observed: u8 },
    /// A reserved/fixed bit field held a non-spec value. Surfaced only when
    /// `StrictMode != Off` (gated to avoid lenient-mode false positives).
    ReservedBits,
}

/// Spec-clause that a NAL header byte violated. Carried inside
/// [`NonConformantIssue::NalHeader`].
///
/// Per H.264 §7.3.1 / H.265 §7.3.1.2 / H.266 V4 §7.3.1.2 the NAL header
/// carries a small number of fixed-value or range-constrained fields.
/// Encoders violating these constraints produce streams that strict
/// decoders MUST reject (and many lenient ones will mis-decode); the
/// demuxer surfaces the specific violation here so consumers can
/// telemetry-correlate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NalHeaderKind {
    /// `forbidden_zero_bit` (high bit of byte 0) is set. Common to all
    /// three codecs. Per H.264 §7.3.1 / H.265 §7.3.1.2 / H.266 §7.3.1.2
    /// this bit MUST be `0` (it exists to disambiguate start-code emulation
    /// in network-shaped envelopes).
    ForbiddenZeroBit,
    /// A reserved field is non-zero. H.266 V4 §7.3.1.2 defines
    /// `nuh_reserved_zero_bit` (bit 6 of byte 0); H.264/265 have no
    /// equivalent surfaced through this variant (their reserved bits are
    /// folded into other field shapes).
    ReservedBit,
    /// `nuh_temporal_id_plus1` is `0`. H.265 §7.3.1.2 + H.266 §7.3.1.2
    /// require `nuh_temporal_id_plus1 != 0` (temporal IDs are 0-based;
    /// the plus-1 encoding reserves 0 as forbidden so decoders can
    /// distinguish missing/sync). H.264 has no temporal-id field.
    ZeroTemporalIdPlus1,
    /// `nuh_layer_id` is out of the allowed range. Per H.266 V4 §7.4.2.2
    /// `nuh_layer_id` MUST be in `0..=55`; values `56..=63` are
    /// spec-reserved (currently no defined NAL types use them) and
    /// receivers MUST discard such NALs. H.264 has no layer-id; H.265's
    /// `nuh_layer_id` is unconstrained at the spec level (extension layers
    /// are valid). This variant fires only for H.266.
    LayerIdOutOfRange { id: u8 },
}

/// Spec-clause that an AV1 OBU header violated. Carried inside
/// [`NonConformantIssue::Av1ObuHeader`].
///
/// Per AV1 Bitstream Spec §5.3.2 the OBU header byte and (when present)
/// the OBU extension header byte carry forbidden / reserved bit positions
/// that conformant encoders MUST leave zero. Encoders violating these
/// constraints produce streams strict AV1 decoders may reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Av1ObuHeaderKind {
    /// `obu_forbidden_bit` (high bit of the OBU header byte) is set.
    /// Per AV1 §5.3.2 this MUST be `0`.
    ForbiddenBit,
    /// `obu_reserved_1bit` (low bit of the OBU header byte) is set.
    /// Per AV1 §5.3.2 this MUST be `0`.
    ReservedBit,
    /// One or more of the 3 reserved bits on the OBU extension header
    /// (low 3 bits, after `temporal_id` and `spatial_id`) are set.
    /// Per AV1 §5.3.3 these MUST be `0`.
    ExtensionReservedBits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscontinuityKind {
    /// Continuity counter jumped on this PID.
    ContinuityJump { expected: u8, observed: u8 },
    /// Per-PID PES reassembly buffer cap exceeded; partial PES dropped.
    PesOversize { pid: u16 },
    /// Aggregate PES-reassembly cap exceeded across all PIDs.
    PesTotalOversize,
    /// `discontinuity_indicator` set in the adaptation field.
    AdaptationFieldFlag,
}

/// Approximate "elapsed time since first event" for a stream-monotonic
/// PTS. Exposed for diagnostic / test use; production consumers usually
/// just compare `Pts90khz` values directly.
///
/// Negative ticks clamp to [`Duration::ZERO`]: a timeline produced with
/// [`DemuxerConfig::unwrap_timestamps`](crate::mpegts::demux::DemuxerConfig::unwrap_timestamps)
/// can legitimately dip below zero near its anchor (a reordered sample
/// that precedes the first observed one), while a raw wire value is
/// never negative. Values beyond what a `Duration` can hold saturate at
/// `u64::MAX` microseconds instead of wrapping.
///
/// Callers holding a raw `i64` can call [`Pts90khz::new`] to wrap; callers
/// who want the inverse can read [`Pts90khz::as_ticks`].
pub fn pts_to_duration(pts: Pts90khz) -> Duration {
    let ticks = i128::from(pts.as_ticks().max(0));
    Duration::from_micros(u64::try_from(ticks * 1_000_000 / 90_000).unwrap_or(u64::MAX))
}

impl core::fmt::Display for NonConformantIssue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid => {
                write!(f, "stream_type=0x06 carries sync KLV on async PID")
            }
            NonConformantIssue::StreamTypeMismatchAsyncOnSyncPid => {
                write!(f, "stream_type=0x15 carries async KLV on sync PID")
            }
            NonConformantIssue::MissingMetadataDescriptor => {
                write!(f, "metadata_descriptor missing on sync-KLV PID")
            }
            NonConformantIssue::PcrAnomaly { delta } => {
                write!(f, "PCR anomaly: delta={}", delta)
            }
            NonConformantIssue::PsiChecksumMismatch { pid } => {
                write!(f, "PSI checksum mismatch on PID 0x{pid:04X}")
            }
            NonConformantIssue::PusiMidPes => {
                write!(f, "PUSI packet arrived mid-PES")
            }
            NonConformantIssue::MalformedPes { pid, reason } => {
                write!(f, "malformed PES on PID 0x{pid:04X}: {reason}")
            }
            NonConformantIssue::PidReusedAcrossPrograms { pid, programs } => {
                write!(
                    f,
                    "PID 0x{pid:04X} reused across programs {} and {}",
                    programs[0], programs[1]
                )
            }
            NonConformantIssue::SubtitleMissingDescriptor { pid } => {
                write!(f, "subtitle stream on PID 0x{pid:04X} missing descriptor")
            }
            NonConformantIssue::SubtitleDescriptorAmbiguous { pid, tags } => {
                write!(
                    f,
                    "subtitle PID 0x{pid:04X} has ambiguous descriptors: {tags:?}"
                )
            }
            NonConformantIssue::SubtitleDescriptorMalformed { pid, tag } => {
                write!(
                    f,
                    "subtitle descriptor 0x{tag:02X} malformed on PID 0x{pid:04X}"
                )
            }
            NonConformantIssue::Av1RegistrationMalformed { pid } => {
                write!(
                    f,
                    "AV1 registration descriptor malformed on PID 0x{pid:04X}"
                )
            }
            NonConformantIssue::Av1ObuMissingSizeField { pid, obu_type } => {
                write!(
                    f,
                    "AV1 OBU type 0x{obu_type:02X} missing size field on PID 0x{pid:04X}"
                )
            }
            NonConformantIssue::Av1TileListNotAllowed { pid } => {
                write!(f, "AV1 Tile List OBU forbidden on PID 0x{pid:04X}")
            }
            NonConformantIssue::PsiOverlongSection { pid, observed_len } => {
                write!(
                    f,
                    "PSI section overlong on PID 0x{pid:04X}: {} bytes",
                    observed_len
                )
            }
            NonConformantIssue::TransportErrorPacket { pid } => {
                write!(f, "transport_error_indicator set on PID 0x{pid:04X}")
            }
            NonConformantIssue::PsiCcDiscontinuity {
                pid,
                expected,
                observed,
            } => {
                write!(
                    f,
                    "PSI continuity-counter jump on PID 0x{pid:04X}: expected 0x{expected:X}, observed 0x{observed:X}"
                )
            }
            NonConformantIssue::DvbSubDataIdentifier { observed } => {
                write!(
                    f,
                    "DVB-subtitle data_identifier=0x{observed:02X} \
                     (EN 300 743 §6.2 Table 3 requires 0x20)"
                )
            }
            NonConformantIssue::PtsAnomaly { delta } => {
                write!(f, "PTS anomaly: delta={delta} (90 kHz ticks)")
            }
            NonConformantIssue::MissingRequiredPts { pid } => {
                write!(
                    f,
                    "PTS required by stream type on PID 0x{pid:04X} but absent in PES header"
                )
            }
            NonConformantIssue::PesHeaderMalformed { pid, kind } => {
                let detail = match kind {
                    PesHeaderMalformedKind::ForbiddenPtsDtsFlags => {
                        "PTS_DTS_flags=0b01 forbidden by H.222.0 §2.4.3.7"
                    }
                    PesHeaderMalformedKind::InvalidMarkerBits => {
                        "flags1 byte top bits != '10' marker (H.222.0 §2.4.3.6)"
                    }
                    PesHeaderMalformedKind::InvalidPtsPrefix => {
                        "PTS 4-bit prefix mismatch (H.222.0 §2.4.3.7)"
                    }
                    PesHeaderMalformedKind::InvalidDtsPrefix => {
                        "DTS 4-bit prefix mismatch (H.222.0 §2.4.3.7)"
                    }
                    PesHeaderMalformedKind::InvalidPtsDtsMarkerBits => {
                        "PTS/DTS 5-byte marker bit != 1 (H.222.0 §2.4.3.7)"
                    }
                };
                write!(f, "PES header malformed on PID 0x{pid:04X}: {detail}")
            }
            NonConformantIssue::SubtitleAlignmentMissing { pid } => {
                write!(
                    f,
                    "subtitle PES on PID 0x{pid:04X} missing data_alignment_indicator \
                     (EN 300 743 §6.2 / EN 300 472 §4.2 require =1)"
                )
            }
            NonConformantIssue::MultiCellAu {
                pid,
                dropped_bytes,
                reason,
            } => {
                let reason_str = match reason {
                    MultiCellAuReason::Orphan => "orphan continuation (no prior First)",
                    MultiCellAuReason::SequenceGap => "sequence_number gap",
                    MultiCellAuReason::ConcurrentFirst => "new First while buffering previous AU",
                    MultiCellAuReason::Overflow => "buffer exceeded au_cell_cap_per_pid",
                    MultiCellAuReason::OverflowTotal => {
                        "aggregate buffers exceeded au_cell_cap_total"
                    }
                    MultiCellAuReason::TooManyPids => {
                        "in-flight PID count exceeded au_cell_max_in_flight_pids"
                    }
                };
                write!(
                    f,
                    "multi-cell AU reassembly failed on PID 0x{pid:04X}: {dropped_bytes} bytes dropped ({reason_str})"
                )
            }
            NonConformantIssue::CfiTolerated {
                pid,
                observed_cfi,
                treated_as,
            } => {
                write!(
                    f,
                    "malformed AU cell CFI tolerated on PID 0x{pid:04X}: \
                     observed {observed_cfi:?} (0b{observed_bits:02b}), \
                     treated as {treated_as:?} (0b{treated_bits:02b})",
                    observed_bits = *observed_cfi as u8,
                    treated_bits = *treated_as as u8,
                )
            }
            NonConformantIssue::PsiMultiSectionUnsupported {
                pid,
                table_id,
                last_section_number,
            } => {
                write!(
                    f,
                    "PSI multi-section table unsupported on PID 0x{pid:04X}: \
                     table_id=0x{table_id:02X}, last_section_number={last_section_number} \
                     (full §2.4.4.5 reassembly deferred — partial section dropped)"
                )
            }
            NonConformantIssue::NalHeader { codec, kind } => match kind {
                NalHeaderKind::ForbiddenZeroBit => write!(
                    f,
                    "{codec:?} NAL header forbidden_zero_bit set (spec mandates =0)"
                ),
                NalHeaderKind::ReservedBit => write!(
                    f,
                    "{codec:?} NAL header reserved bit set (spec mandates =0)"
                ),
                NalHeaderKind::ZeroTemporalIdPlus1 => write!(
                    f,
                    "{codec:?} NAL header nuh_temporal_id_plus1 = 0 (spec mandates !=0)"
                ),
                NalHeaderKind::LayerIdOutOfRange { id } => write!(
                    f,
                    "{codec:?} NAL header nuh_layer_id={id} out of range (spec allows 0..=55)"
                ),
            },
            NonConformantIssue::Av1ObuHeader { pid, kind } => match kind {
                Av1ObuHeaderKind::ForbiddenBit => write!(
                    f,
                    "AV1 OBU header obu_forbidden_bit set on PID 0x{pid:04X} (spec mandates =0)"
                ),
                Av1ObuHeaderKind::ReservedBit => write!(
                    f,
                    "AV1 OBU header obu_reserved_1bit set on PID 0x{pid:04X} (spec mandates =0)"
                ),
                Av1ObuHeaderKind::ExtensionReservedBits => write!(
                    f,
                    "AV1 OBU extension header reserved bits set on PID 0x{pid:04X} (spec mandates =0)"
                ),
            },
            NonConformantIssue::PcrMalformed { kind } => match kind {
                PcrMalformedKind::InvalidReservedBits => {
                    write!(f, "PCR field reserved bits not all 1 (H.222.0 §2.4.3.5)")
                }
                PcrMalformedKind::ExtensionOutOfRange => write!(
                    f,
                    "PCR field program_clock_reference_extension > 299 (H.222.0 §2.4.3.5)"
                ),
            },
            NonConformantIssue::Ac3SyncMissing { pid } => {
                write!(
                    f,
                    "AC-3 PES on PID 0x{pid:04X} missing syncword 0x0B77 \
                     despite data_alignment_indicator=1 (A/52:2018 §A.6.3)"
                )
            }
            NonConformantIssue::LatmFraming { pid, kind } => {
                use crate::codec::aac::latm::LatmFramingKind;
                let detail = match kind {
                    LatmFramingKind::MissingSyncword => {
                        "LOAS syncword (0x2B7) missing at start of PES payload"
                    }
                    LatmFramingKind::AudioMuxLengthOverrun => {
                        "audioMuxLengthBytes runs past end of PES payload"
                    }
                    LatmFramingKind::Truncated => "PES payload shorter than 3-byte LOAS header",
                };
                write!(
                    f,
                    "AAC-LATM framing violation on PID 0x{pid:04X}: {detail} \
                     (ISO/IEC 14496-3 §1.7 + H.222.0 Table 2-34 stream_type 0x11)"
                )
            }
            NonConformantIssue::Av1WrongStreamId { pid, observed } => {
                write!(
                    f,
                    "AV1 PES on PID 0x{pid:04X} carries stream_id=0x{observed:02X} \
                     (AV1-in-MPEG-2-TS binding §3.4 mandates 0xBD)"
                )
            }
            NonConformantIssue::Av1MissingTsObuFraming { pid } => {
                write!(
                    f,
                    "AV1 PES on PID 0x{pid:04X} missing ts_open_bitstream_unit \
                     start code (AV1-in-MPEG-2-TS binding §3.2 mandates 0x00 0x00 0x01 prefix)"
                )
            }
            NonConformantIssue::PmtProgramNumberMismatch {
                pid,
                pat_program,
                pmt_program,
            } => {
                write!(
                    f,
                    "PMT on PID 0x{pid:04X} body program_number={pmt_program} does not match \
                     PAT assignment program_number={pat_program} (H.222.0 §2.4.4.8 REF-PSI-01)"
                )
            }
            NonConformantIssue::UnsupportedScrambling { pid, control } => {
                write!(
                    f,
                    "unsupported transport_scrambling_control=0b{control:02b} on PID 0x{pid:04X} \
                     (H.222.0 §2.4.3.2; payload not routed)"
                )
            }
            NonConformantIssue::AdaptationFieldMalformed { pid, kind } => {
                let detail = match kind {
                    AdaptationFieldKind::ReservedControl => {
                        "adaptation_field_control=00 reserved (H.222.0 §2.4.3.2; discard)"
                    }
                    AdaptationFieldKind::BadLengthForControl => {
                        "adaptation_field_length invalid for control (H.222.0 §2.4.3.2)"
                    }
                    AdaptationFieldKind::ShortPcr => {
                        "pcr_flag set with fewer than 6 PCR bytes (H.222.0 §2.4.3.5)"
                    }
                };
                write!(f, "adaptation field malformed on PID 0x{pid:04X}: {detail}")
            }
            NonConformantIssue::ZeroLengthPesNonVideo { pid, stream_id } => {
                write!(
                    f,
                    "zero PES_packet_length on non-video PID 0x{pid:04X} \
                     (stream_id=0x{stream_id:02X}); H.222.0 §2.4.3.7 permits zero \
                     only for video — packet dropped"
                )
            }
            NonConformantIssue::PsiSyntax {
                pid,
                table_id,
                kind,
            } => {
                // Write directly to the formatter — no intermediate String
                // allocation (Display can be on the strict-rejection hot path,
                // and tst-core is no_std-capable).
                write!(
                    f,
                    "PSI syntax violation on PID 0x{pid:04X} (table_id=0x{table_id:02X}): "
                )?;
                match kind {
                    PsiSyntaxKind::SectionSyntaxIndicatorUnset => {
                        write!(f, "section_syntax_indicator != 1")?
                    }
                    PsiSyntaxKind::SectionNumberNonZero { observed } => {
                        write!(f, "section_number={observed} != 0 on single-section table")?
                    }
                    PsiSyntaxKind::ReservedBits => write!(f, "reserved bits not at spec values")?,
                }
                write!(f, " (H.222.0 §2.4.4)")
            }
            NonConformantIssue::Other(msg) => {
                write!(f, "{msg}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_unit_h264_h265_distinct() {
        let h264 = NalUnit::H264 {
            nal_type: 5,
            ref_idc: 3,
            payload: SharedBytes::from_vec(vec![]),
        };
        let h265 = NalUnit::H265 {
            nal_type: 19,
            layer_id: 0,
            temporal_id_plus1: 1,
            payload: SharedBytes::from_vec(vec![]),
        };
        assert_ne!(h264, h265);
    }

    #[test]
    fn pts_to_duration_simple() {
        // 90,000 ticks @ 90 kHz = 1 second.
        assert_eq!(
            pts_to_duration(Pts90khz::new(90_000)),
            Duration::from_secs(1)
        );
    }

    /// CORR-17: a negative unwrapped PTS used to be cast `as u64` and
    /// rendered as ~584,000 years.
    #[test]
    fn pts_to_duration_clamps_negative_ticks_to_zero() {
        assert_eq!(pts_to_duration(Pts90khz::new(-50)), Duration::ZERO);
        assert_eq!(pts_to_duration(Pts90khz::new(i64::MIN)), Duration::ZERO);
    }

    #[test]
    fn pts_to_duration_saturates_instead_of_wrapping() {
        assert_eq!(
            pts_to_duration(Pts90khz::new(i64::MAX)),
            Duration::from_micros(u64::MAX)
        );
    }

    #[test]
    fn audio_codec_real_variants_in_demux_event_surface() {
        let codecs = [
            AudioCodec::Mp2,
            AudioCodec::Aac,
            AudioCodec::AacLatm,
            AudioCodec::Ac3,
        ];
        assert_ne!(codecs[0], codecs[1]);
    }

    #[test]
    fn demux_subtitle_codec_real_variants_in_event_surface() {
        let codecs = [
            SubtitleCodec::DvbSubtitling,
            SubtitleCodec::DvbTeletext,
            SubtitleCodec::Cea708Standalone,
            SubtitleCodec::WebVttInTs,
        ];
        assert_ne!(codecs[0], codecs[1]);
        assert_ne!(codecs[2], codecs[3]);
    }

    #[test]
    fn psi_cc_discontinuity_displays_pid_and_counter_pair() {
        let issue = NonConformantIssue::PsiCcDiscontinuity {
            pid: 0x100,
            expected: 0x9,
            observed: 0xC,
        };
        let s = format!("{issue}");
        assert!(s.contains("PID 0x0100"), "Display includes PID: {s}");
        assert!(s.contains("expected 0x9"), "Display includes expected: {s}");
        assert!(s.contains("observed 0xC"), "Display includes observed: {s}");
    }

    #[test]
    fn malformed_au_cell_cfi_tolerated_displays_pid_and_cfi_bits() {
        use crate::mpegts::au_cell::CellFragmentIndication;
        let issue = NonConformantIssue::CfiTolerated {
            pid: 0x1002,
            observed_cfi: CellFragmentIndication::Middle,
            treated_as: CellFragmentIndication::Complete,
        };
        let s = format!("{issue}");
        assert!(s.contains("PID 0x1002"), "Display includes PID: {s}");
        assert!(s.contains("Middle"), "Display names observed variant: {s}");
        assert!(
            s.contains("0b00"),
            "Display includes observed CFI bits: {s}"
        );
        assert!(
            s.contains("Complete"),
            "Display names treated_as variant: {s}"
        );
        assert!(
            s.contains("0b11"),
            "Display includes treated_as CFI bits: {s}"
        );
    }

    #[test]
    fn stream_kind_tag_covers_every_variant() {
        // One representative instance per variant; `expected` is re-derived
        // through an exhaustive wildcard-free match, so a new StreamKind
        // variant fails to compile here — extend both the match and this
        // instance list when that happens.
        let kinds = [
            StreamKind::Video(VideoCodec::H264),
            StreamKind::Audio(AudioCodec::Mp2),
            StreamKind::Subtitle(SubtitleCodec::WebVttInTs),
            StreamKind::KlvSync {
                declared_link: None,
            },
            StreamKind::KlvAsync,
            StreamKind::Unknown(0x06),
        ];
        for kind in kinds {
            let expected = match kind {
                StreamKind::Video(_) => StreamKindTag::Video,
                StreamKind::Audio(_) => StreamKindTag::Audio,
                StreamKind::Subtitle(_) => StreamKindTag::Subtitle,
                StreamKind::KlvSync { .. } => StreamKindTag::KlvSync,
                StreamKind::KlvAsync => StreamKindTag::KlvAsync,
                StreamKind::Unknown(_) => StreamKindTag::Unknown,
            };
            assert_eq!(kind.tag(), expected, "tag() mismatch for {kind:?}");
        }
    }
}
