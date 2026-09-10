//! `tst_event_t` tagged union + subordinate `repr(C)` structs +
//! per-handle `EventArena` for the demux receiver hot path.
//!
//! Lifetime contract (design §4.5): all pointer fields on `TstEvent`
//! borrow from the `EventArena` owned by the `TstDemuxReceiver`
//! handle. Valid until the next `_recv_event` / `_close` call on the
//! same handle. Callers wanting longer lifetime memcpy out.

use crate::c_types::{c_char, c_int};
use alloc::vec::Vec;

// ------------------------------------------------------------------
// Top-level event kind discriminator (6 variants)
// ------------------------------------------------------------------

/// `repr(i32)` discriminator for `TstEvent::kind`. cbindgen emits
/// `#define TST_EVENT_*` blocks.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstEventKind {
    ProgramMap = 1,
    Sample = 2,
    Metadata = 3,
    Discontinuity = 4,
    NonConformant = 5,
    /// Boundary marker emitted by `tst_managed_demux_receiver_*` after
    /// the underlying transport reconnects and the demuxer's sync /
    /// PSI / PES state was reset (validate-1 Sprint 4 F2 + followup-1).
    ///
    /// **Carries no body** — the `u` union is zero-initialized for this
    /// kind. Consumers should drop any per-stream caches and wait for
    /// the next [`ProgramMap`](Self::ProgramMap) event on the fresh
    /// connection.
    ///
    /// The plain `tst_demux_receiver_*` family never emits this kind.
    ReconnectDiscontinuity = 6,
}

// ------------------------------------------------------------------
// Stream-kind discriminator (6 variants matching tst_core StreamKind)
// ------------------------------------------------------------------

/// `repr(i32)` mirror of `tst_core::mpegts::demux::StreamKind`'s
/// discriminator. Variant payloads (codec, declared_link, unknown
/// stream_type) live on the per-event union fields, not on this enum.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstStreamKindTag {
    Video = 0,
    Audio = 1,
    Subtitle = 2,
    KlvSync = 3,
    KlvAsync = 4,
    Unknown = 5,
}

// ------------------------------------------------------------------
// LinkSource (3 variants)
// ------------------------------------------------------------------

/// `repr(i32)` mirror of `tst_core::mpegts::demux::LinkSource`.
/// On `tst_klv_link_t.source` and on synthetic KLV-link inference.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstLinkSource {
    Declared = 0,
    Inferred = 1,
    Override = 2,
}

// ------------------------------------------------------------------
// Metadata-kind discriminator (3 variants)
// ------------------------------------------------------------------

/// `repr(i32)` mirror of `tst_core::mpegts::demux::MetadataKind`'s
/// discriminator. Variant payload (the KlvSyncAuCell 5 fields) lives
/// on `tst_event_t.u.metadata` fields, zero/false for other kinds.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstMetadataKindTag {
    KlvSyncAuCell = 0,
    KlvAsync = 1,
    Unknown = 2,
}

// ------------------------------------------------------------------
// Discontinuity-kind discriminator (4 variants)
// ------------------------------------------------------------------

/// `repr(i32)` mirror of `tst_core::mpegts::demux::DiscontinuityKind`'s
/// discriminator. ContinuityJump's `expected`/`observed` and
/// PesOversize's `pid` live on the union fields.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstDiscontinuityKindTag {
    ContinuityJump = 0,
    PesOversize = 1,
    PesTotalOversize = 2,
    AdaptationFieldFlag = 3,
}

// ------------------------------------------------------------------
// Non-conformant-issue codes (one code per `NonConformantIssue` variant)
// ------------------------------------------------------------------

/// `repr(i32)` mirror of `tst_core::mpegts::demux::NonConformantIssue`'s
/// discriminator. Issue-specific fields live on `tst_event_t.u.nonconformant`
/// (zero/null for variants that don't carry them).
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstNonConformantCode {
    StreamTypeMismatchSyncOnAsyncPid = 0,
    StreamTypeMismatchAsyncOnSyncPid = 1,
    MissingMetadataDescriptor = 2,
    PcrAnomaly = 3,
    PsiChecksumMismatch = 4,
    PusiMidPes = 5,
    PidReusedAcrossPrograms = 6,
    SubtitleMissingDescriptor = 7,
    SubtitleDescriptorAmbiguous = 8,
    SubtitleDescriptorMalformed = 9,
    Av1RegistrationMalformed = 10,
    Av1ObuMissingSizeField = 11,
    Av1TileListNotAllowed = 12,
    PsiOverlongSection = 13,
    TransportErrorPacket = 14,
    PsiCcDiscontinuity = 15,
    MultiCellAu = 16,
    PsiMultiSectionUnsupported = 17,
    Other = 18,
    MalformedPes = 19,
    /// EN 300 743 §6.2 Table 3 binds DVB-subtitle `data_identifier` to
    /// exactly `0x20`. Reuses `table_id` field as the observed byte carrier
    /// (mirroring `SubtitleDescriptorMalformed`'s reuse).
    DvbSubDataIdentifier = 20,
    /// PTS backward jump on an elementary stream PID (validate-1 B4).
    /// `pcr_delta` field carries the 90 kHz tick delta (re-used from
    /// PcrAnomaly; PTS and PCR anomalies never co-occur for a single
    /// event, so the storage is shared without ambiguity).
    PtsAnomaly = 21,
    /// PES on a PTS-required stream type (audio / video) arrived without
    /// one (validate-1 B4). `pid` carries the stream PID.
    MissingRequiredPts = 22,
    /// PES header structural violation (validate-1 B5). Re-uses the
    /// `table_id` field as the [`tst_core::mpegts::demux::PesHeaderMalformedKind`]
    /// discriminator (0=ForbiddenPtsDtsFlags, 1=InvalidMarkerBits,
    /// 2=InvalidPtsPrefix, 3=InvalidDtsPrefix, 4=InvalidPtsDtsMarkerBits).
    PesHeaderMalformed = 23,
    /// DVB subtitle / teletext PES arrived with
    /// `data_alignment_indicator = 0` (validate-1 B6). `pid` carries the
    /// stream PID.
    SubtitleAlignmentMissing = 24,
    /// H.222.0 §2.4.3.5 PCR field syntax violation (reserved bits not all 1,
    /// or `program_clock_reference_extension > 299`). Reuses the `table_id`
    /// field to carry a `TstPcrMalformedKind` discriminator.
    PcrMalformed = 25,
    /// H.264 / H.265 / H.266 NAL header constraint violation
    /// (forbidden_zero_bit / reserved bit / temporal_id_plus1 / layer_id).
    /// `codec` byte surfaces on `table_id` (reusing the existing carrier;
    /// values match `TstVideoCodec` discriminants — H264=0, H265=1, H266=2,
    /// Av1=3); `nal_header_kind` byte (`obu_type` carrier) encodes which
    /// constraint variant (0=ForbiddenZeroBit, 1=ReservedBit,
    /// 2=ZeroTemporalIdPlus1, 3=LayerIdOutOfRange).
    /// `LayerIdOutOfRange.id` surfaces on `cc_observed` (the offending
    /// `nuh_layer_id` byte).
    NalHeader = 26,
    /// AV1 OBU header constraint violation (obu_forbidden_bit /
    /// obu_reserved_1bit / OBU extension reserved bits).
    /// `obu_header_kind` byte (`obu_type` carrier) encodes which constraint
    /// variant (0=ForbiddenBit, 1=ReservedBit, 2=ExtensionReservedBits).
    Av1ObuHeader = 27,
    /// AC-3 PES with `data_alignment_indicator=1` did not start with
    /// the syncword `0x0B77` (validate-1 C12; ATSC A/52:2018 §A.6.3).
    /// `pid` carries the stream PID.
    Ac3SyncMissing = 28,
    /// AAC-LATM (stream_type 0x11) PES framing violation (validate-1 C11).
    /// `pid` carries the stream PID; `latm_framing_kind` byte
    /// (`obu_type` carrier) encodes which violation variant
    /// (0=MissingSyncword, 1=AudioMuxLengthOverrun, 2=Truncated).
    LatmFraming = 29,
    /// AV1-in-MPEG-2-TS binding §3.4 violation — PES `stream_id` other than
    /// `0xBD`. `pid` carries the AV1 stream PID; `table_id` carrier
    /// surfaces the observed `stream_id` byte (reused field, same shape as
    /// `DvbSubDataIdentifier` and `SubtitleDescriptorMalformed`).
    Av1WrongStreamId = 30,
    /// AV1-in-MPEG-2-TS binding §3.2 violation — PES payload did not begin
    /// with a `ts_open_bitstream_unit()` start code (`0x00 0x00 0x01`, the
    /// 3-byte `obu_start_code` = `uimsbf(24)` = `0x000001` per the binding
    /// syntax table). `pid` carries the AV1 stream PID.
    Av1MissingTsObuFraming = 31,
    /// Orphan sync-metadata AU cell with malformed
    /// `cell_fragment_indication` was tolerated under
    /// `tst_demux_config_set_cfi_tolerance(_, true)`.
    /// `pid` is the elementary stream PID. `cc_expected` carries the
    /// observed CFI bits (`TstCellFragmentIndication` mirror); `cc_observed`
    /// carries the substituted CFI bits (today always `Complete = 3`).
    /// The KLV metadata payload was also emitted as a separate
    /// `TST_EVENT_KIND_METADATA` event with `cell_fragment_indication = Complete`.
    CfiTolerated = 32,
    /// PMT body `program_number` (H.222.0 §2.4.4.8) does not match the
    /// `program_number` the PAT (§2.4.4.4) assigned to this PMT PID.
    /// The mislabeled topology is NOT adopted (REF-PSI-01).
    /// `pid` carries the PMT PID. `programs[0]` = `pat_program`;
    /// `programs[1]` = `pmt_program` (reuses the two-element `programs_buf`
    /// carrier, same layout as `PidReusedAcrossPrograms`).
    PmtProgramNumberMismatch = 33,
    /// REF-TS-01. transport_scrambling_control != 0; payload not routed.
    /// `pid` = the scrambled PID; `table_id` carries the 2-bit control value.
    UnsupportedScrambling = 34,
    /// REF-TS-02. Adaptation-field control/length violation. `pid` = the PID;
    /// `table_id` carries the AdaptationFieldKind discriminator
    /// (0=ReservedControl, 1=BadLengthForControl, 2=ShortPcr, 0xFF=unknown).
    AdaptationFieldMalformed = 35,
    /// REF-PES-01. Zero PES_packet_length on a non-video stream; partial
    /// dropped. `pid` = the PID; `table_id` carries the PES stream_id byte.
    ZeroLengthPesNonVideo = 36,
    /// REF-PSI-03. PAT/PMT fixed/reserved syntax violation. `pid` = the PID;
    /// `table_id` = the PSI table_id (0x00 PAT, 0x02 PMT); `obu_type` carries
    /// the PsiSyntaxKind discriminator (0=SectionSyntaxIndicatorUnset,
    /// 1=SectionNumberNonZero, 2=ReservedBits, 0xFF=unknown); for
    /// SectionNumberNonZero, `cc_observed` carries the observed section_number.
    PsiSyntax = 37,
}

/// `repr(i32)` mirror of `tst_core::mpegts::au_cell::CellFragmentIndication`.
/// Surfaced on `tst_event_t.u.nonconformant.cc_expected` (observed) and
/// `tst_event_t.u.nonconformant.cc_observed` (substituted) when
/// `issue_code == TST_NONCONFORMANT_CODE_CFI_TOLERATED`.
///
/// Discriminants match the H.222.0 V9 Table 2-157 wire bits exactly:
/// `Middle = 0` (0b00), `Last = 1` (0b01), `First = 2` (0b10),
/// `Complete = 3` (0b11).
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstCellFragmentIndication {
    Middle = 0,
    Last = 1,
    First = 2,
    Complete = 3,
}

/// `repr(i32)` mirror of `tst_core::mpegts::demux::PcrMalformedKind`.
/// Surfaced on `tst_event_t.u.nonconformant.table_id` when
/// `issue_code == TST_NONCONFORMANT_CODE_PCR_MALFORMED`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstPcrMalformedKind {
    /// Six reserved bits of PCR byte 4 (mask `0x7E`) were not all 1.
    InvalidReservedBits = 0,
    /// `program_clock_reference_extension` decoded to a value > 299.
    ExtensionOutOfRange = 1,
}

/// `repr(i32)` mirror of `tst_core::mpegts::demux::MultiCellAuReason`.
/// Surfaced on `tst_event_t.u.nonconformant.multi_cell_au_reason` when
/// `issue_code == TST_NONCONFORMANT_CODE_MULTI_CELL_AU`.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TstMultiCellAuReason {
    /// A continuation cell (`Middle` or `Last`) arrived without a prior
    /// `First`. Stream started mid-AU or a `First` was lost upstream.
    Orphan = 0,
    /// A continuation cell's `sequence_number` did not match the expected
    /// `(first.sequence_number + cells_seen) mod 256`. Cell loss between
    /// the buffered prefix and the arriving cell.
    SequenceGap = 1,
    /// A new `First` cell arrived while the previous AU was still being
    /// buffered (its `Last` never appeared). The partial buffer is
    /// dropped before the new `First` is processed.
    ConcurrentFirst = 2,
    /// The buffered AU's accumulated inner bytes would exceed
    /// [`tst_core::mpegts::demux::DemuxerConfig::au_cell_cap_per_pid`]
    /// (default 1 MiB). The partial buffer is dropped.
    Overflow = 3,
    /// The aggregate in-flight AU-cell bytes across all PIDs would exceed
    /// [`tst_core::mpegts::demux::DemuxerConfig::au_cell_cap_total`]
    /// (default 16 MiB). Defends a multi-PID flood where each PID stays
    /// under its own per-PID cap but the total explodes. The offending
    /// PID's partial buffer is dropped.
    OverflowTotal = 4,
    /// A new `First` cell would open reassembly on a PID beyond
    /// [`tst_core::mpegts::demux::DemuxerConfig::au_cell_max_in_flight_pids`]
    /// (default 64) concurrently in-flight PIDs. Bounds active-PID count
    /// against an adversary that opens a `First` for thousands of distinct
    /// PIDs and never sends `Last`. The new cell is rejected; existing
    /// in-flight reassemblies are left intact.
    TooManyPids = 5,
}

// ------------------------------------------------------------------
// Subordinate list-element structs
// ------------------------------------------------------------------

/// `repr(C)` mirror of `tst_core::mpegts::demux::NalUnit`.
///
/// Three Rust variants (H264 / H265 / H266) collapsed into one C
/// struct; field semantics keyed by the parent `tst_event_t.u.sample.codec`.
/// * H.264: `nal_type` 5-bit; `ref_idc_or_layer_id` is `ref_idc`;
///   `temporal_id_plus1` is `0` (H.264 has no temporal_id).
/// * H.265 / H.266: `nal_type` 6-bit (H.265) or 5-bit (H.266);
///   `ref_idc_or_layer_id` is `nuh_layer_id`; `temporal_id_plus1`
///   is the temporal-id field +1 (per spec).
///
/// `payload` is arena-owned — for H.26x it points into the raw-AU copy
/// exposed by the parent sample's `payload` field; valid until the next
/// `_recv_event` / `_close` call on this handle.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstNal {
    pub nal_type: u8,
    pub ref_idc_or_layer_id: u8,
    pub temporal_id_plus1: u8,
    pub _reserved: u8,
    pub payload: *const u8,
    pub payload_len: usize,
}

#[cfg(target_pointer_width = "64")]
const _TST_NAL_SIZE: () = assert!(
    core::mem::size_of::<TstNal>() == 24,
    "TstNal must be 24 bytes (4 bytes header + 8 bytes pointer + 8 bytes len)"
);
#[cfg(target_pointer_width = "32")]
const _TST_NAL_SIZE: () = assert!(
    core::mem::size_of::<TstNal>() == 12,
    "TstNal must be 12 bytes on 32-bit (4 bytes header + 4 bytes pointer + 4 bytes len)"
);

/// `repr(C)` mirror of `tst_core::mpegts::demux::Obu`.
///
/// `has_extension` is 0 or 1; `temporal_id` and `spatial_id` are
/// valid only when `has_extension == 1`. `payload` is the OBU body
/// (header byte + extension byte + LEB128 size already stripped).
/// `payload` is arena-owned; valid until the next `_recv_event` / `_close`
/// call on this handle.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstObu {
    pub obu_type: u8,
    pub has_extension: u8,
    pub temporal_id: u8,
    pub spatial_id: u8,
    pub payload: *const u8,
    pub payload_len: usize,
}

#[cfg(target_pointer_width = "64")]
const _TST_OBU_SIZE: () = assert!(
    core::mem::size_of::<TstObu>() == 24,
    "TstObu must be 24 bytes"
);
#[cfg(target_pointer_width = "32")]
const _TST_OBU_SIZE: () = assert!(
    core::mem::size_of::<TstObu>() == 12,
    "TstObu must be 12 bytes on 32-bit"
);

/// `repr(C)` mirror of `tst_core::mpegts::descriptors::RawDescriptor`.
///
/// `data` borrows from the demuxer's per-PMT descriptor list; valid
/// until the next `_recv_event` / `_close` call on this handle. The
/// length byte from the wire is stripped — `data_len` is the body length.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstDescriptor {
    pub tag: u8,
    pub _reserved: [u8; 7],
    pub data: *const u8,
    pub data_len: usize,
}

#[cfg(target_pointer_width = "64")]
const _TST_DESCRIPTOR_SIZE: () = assert!(
    core::mem::size_of::<TstDescriptor>() == 24,
    "TstDescriptor must be 24 bytes"
);
#[cfg(target_pointer_width = "32")]
const _TST_DESCRIPTOR_SIZE: () = assert!(
    core::mem::size_of::<TstDescriptor>() == 16,
    "TstDescriptor must be 16 bytes on 32-bit"
);

/// `repr(C)` mirror of `tst_core::mpegts::demux::StreamInfo`.
///
/// `stream_kind` is `TST_STREAM_KIND_*` (see `TstStreamKindTag`);
/// `codec` is `TST_VIDEO_CODEC_*` / `TST_AUDIO_CODEC_*` /
/// `TST_SUBTITLE_CODEC_*` keyed by `stream_kind`, or `-1` when
/// `stream_kind` is KlvSync / KlvAsync / Unknown.
///
/// `raw_descriptors` borrows from the demuxer's per-PMT descriptor
/// list; valid until the next `_recv_event` / `_close` call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstStreamInfo {
    pub pid: u16,
    pub stream_type: u8,
    pub _pad: u8,
    pub stream_kind: c_int,
    pub codec: c_int,
    pub program_number: u16,
    pub _pad2: [u8; 6],
    pub raw_descriptors: *const TstDescriptor,
    pub descriptor_count: usize,
}

#[cfg(target_pointer_width = "64")]
const _TST_STREAM_INFO_SIZE: () = assert!(
    core::mem::size_of::<TstStreamInfo>() == 40,
    "TstStreamInfo must be 40 bytes"
);
#[cfg(target_pointer_width = "32")]
const _TST_STREAM_INFO_SIZE: () = assert!(
    core::mem::size_of::<TstStreamInfo>() == 28,
    "TstStreamInfo must be 28 bytes on 32-bit"
);

/// `repr(C)` mirror of `tst_core::mpegts::demux::KlvLink`.
/// `source` is `TST_LINK_SOURCE_*` (see `TstLinkSource`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstKlvLink {
    pub klv_pid: u16,
    pub video_pid: u16,
    pub source: c_int,
}

const _TST_KLV_LINK_SIZE: () = assert!(
    core::mem::size_of::<TstKlvLink>() == 8,
    "TstKlvLink must be 8 bytes"
);

// ------------------------------------------------------------------
// TstEvent tagged union
// ------------------------------------------------------------------

/// Per-event-kind union body for `TstEvent`. cbindgen emits this as
/// `union { ... } u` on the C side; each kind's fields are a flat
/// nested struct. Fields not relevant to the active `kind` are zero
/// or null after `convert()`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union TstEventBody {
    pub program_map: TstEventProgramMap,
    pub sample: TstEventSample,
    pub metadata: TstEventMetadata,
    pub discontinuity: TstEventDiscontinuity,
    pub nonconformant: TstEventNonConformant,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstEventProgramMap {
    pub program_number: u16,
    pub pcr_pid: u16,
    /// PID carrying this program's PMT — the raw 13-bit PID from the PAT
    /// entry that declared the program (never 0 for a demuxed program;
    /// program 0 is the NIT and is not emitted as a ProgramMap event).
    /// Pass it as the `pmt_pid` argument to `tst_mux_config_add_program`
    /// when re-muxing the same program.
    pub pmt_pid: u16,
    pub _pad: [u8; 2],
    pub streams: *const TstStreamInfo,
    pub stream_count: usize,
    pub klv_links: *const TstKlvLink,
    pub klv_link_count: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstEventSample {
    pub pid: u16,
    pub program_number: u16,
    pub stream_kind: c_int,
    pub pts: i64,
    pub dts: i64, // INT64_MIN if absent
    pub codec: c_int,
    /// (Video only) `1` if the TS adaptation field carried
    /// `random_access_indicator` (ISO/IEC 13818-1 §2.4.3.4 bit 0x40) on
    /// the PES_start packet of this access unit. Zero for non-video samples
    /// and when RAI was not set.
    pub random_access_indicator: u8,
    /// (Unknown samples only) Raw PMT `stream_type` byte for the source
    /// stream. Allows C callers to discriminate unknown stream types
    /// without correlating back to the most recent ProgramMap event. Zero
    /// for known stream types (use `codec` field instead).
    pub stream_type: u8,
    /// (Video only) AV1 carriage provenance: `0` =
    /// `TST_AV1_CARRIAGE_MODE_MPEG2_TS_BINDING`, `1` =
    /// `TST_AV1_CARRIAGE_MODE_INTEROP_RAW_OBU`, `0xFF` = not applicable
    /// (non-AV1 sample or AV1 with unknown carriage). Mirrors
    /// `TstAv1CarriageMode`. For byte-faithful transmux, set the
    /// destination muxer's carriage to this value via
    /// `tst_mux_config_set_av1_carriage`, then push `payload` through
    /// `tst_muxer_push_video_wire` — NOT `tst_muxer_push_video`, which
    /// would re-wrap binding-framed OBUs and corrupt the stream.
    pub av1_carriage: u8,
    pub _pad: [u8; 1],
    /// (Video, NAL-shaped codecs) Parsed NAL-unit views of the access unit
    /// in `payload`. Null for non-video samples and AV1 (see `obus`).
    /// (`nal_count` may be 0.)
    pub nals: *const TstNal,
    pub nal_count: usize,
    /// (Video, AV1) Parsed OBU views. Null for non-video samples and
    /// NAL-shaped codecs (see `nals`).
    pub obus: *const TstObu,
    pub obu_count: usize,
    /// Raw payload bytes, arena-owned; valid until the next `_recv_event`
    /// / `_close` call on this handle.
    /// * Video (since v0.2.0; null before): the exact encoded access unit
    ///   — Annex-B byte stream for H.264/H.265/H.266, on-wire PES payload
    ///   for AV1. For H.264/H.265/H.266: feed it back to
    ///   `tst_muxer_push_video` for byte-faithful transmux. For AV1: use
    ///   `tst_muxer_push_video_wire` (with the muxer configured via
    ///   `tst_mux_config_set_av1_carriage` to match `av1_carriage`) —
    ///   `tst_muxer_push_video` would re-wrap binding-framed OBUs and
    ///   corrupt the stream. The parsed view of the same AU is in
    ///   `nals` / `obus`.
    /// * Audio: the raw frame bytes (e.g. ADTS for AAC).
    /// * Subtitle / Unknown: the raw PES payload bytes.
    pub payload: *const u8,
    pub payload_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstEventMetadata {
    pub pid: u16,
    pub program_number: u16,
    pub _pad: [u8; 4],
    pub pts: i64,
    pub metadata_kind: c_int,
    pub _pad2: [u8; 4],
    pub payload: *const u8,
    pub payload_len: usize,
    // KlvSyncAuCell-only fields (zero/false for other kinds)
    pub metadata_service_id: u8,
    pub sequence_number: u8,
    pub cell_fragment_indication: u8, // 0=Middle, 1=Last, 2=First, 3=Complete
    pub decoder_config_flag: u8,
    pub random_access_indicator: u8,
    pub _pad3: [u8; 3],
    /// Multi-cell AU reassembly outcome (KlvSyncAuCell only). `true` if
    /// `payload` is the concatenated inner bytes of 2+ cells whose
    /// `cell_fragment_indication` chain (First → Middle\* → Last) was
    /// validated and joined; `false` if `payload` is a single complete
    /// cell or the metadata kind is not KlvSyncAuCell.
    pub was_reassembled: bool,
    pub _pad4: [u8; 3],
    /// Number of source cells contributing to `payload` (KlvSyncAuCell
    /// only). `1` for single-cell AUs and non-KlvSyncAuCell kinds; `>= 2`
    /// when `was_reassembled == true`.
    pub cell_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstEventDiscontinuity {
    /// Stream PID — always the parent `StreamId.pid` for the stream this
    /// discontinuity is associated with. Stable across discontinuity_kind
    /// variants.
    pub pid: u16,

    /// Variant-specific PID, populated for discontinuity kinds that carry
    /// their own PID in the Rust enum (currently: `PesOversize { pid }`).
    /// Zero for variants that don't carry a variant-specific PID
    /// (`ContinuityJump`, `PesTotalOversize`, `AdaptationFieldFlag`). For
    /// `PesOversize`, `variant_pid` usually equals `pid` but the variant
    /// preserves it independently for the rare divergence case.
    pub variant_pid: u16,

    pub discontinuity_kind: c_int,
    pub cc_expected: u8,
    pub cc_observed: u8,
    pub _pad2: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstEventNonConformant {
    pub pid: u16,
    pub _pad: [u8; 2],
    pub issue_code: c_int,
    pub pcr_delta: i64,
    pub table_id: u8,
    pub last_section_number: u8,
    pub cc_expected: u8,
    pub cc_observed: u8,
    pub _pad2: [u8; 4],
    pub observed_len: usize,
    pub obu_type: u8,
    pub _pad3: [u8; 3],
    /// `repr(i32)` mirror of `tst_core::mpegts::demux::MultiCellAuReason`.
    /// Valid only when `issue_code == TST_NONCONFORMANT_CODE_MULTI_CELL_AU`;
    /// values match `TstMultiCellAuReason` discriminants
    /// (Orphan=0, SequenceGap=1, ConcurrentFirst=2, Overflow=3,
    /// OverflowTotal=4, TooManyPids=5).
    /// Zero (Orphan) for unrelated issue codes — gate on `issue_code`
    /// before reading. The accompanying `observed_len` field carries
    /// the cumulative inner-byte count discarded.
    pub multi_cell_au_reason: c_int,
    pub programs: *const u16, // PidReusedAcrossPrograms / PmtProgramNumberMismatch (len 2; [0]=pat_program, [1]=pmt_program for the latter)
    pub tags: *const u8,      // SubtitleDescriptorAmbiguous
    pub tag_count: usize,
    pub detail: *const c_char, // Other(String); also human-readable summary
}

/// Top-level event struct. Caller stack-allocates and passes
/// `&mut TstEvent` to `tst_demux_receiver_recv_event`. After a
/// success return, `kind` selects which `u.*` body is populated;
/// pointer fields on the active body borrow from the receiver's
/// `EventArena` until the next `_recv_event` / `_close` call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstEvent {
    pub kind: c_int,
    pub _pad: [u8; 4],
    pub u: TstEventBody,
}

const _TST_EVENT_SIZE_REASONABLE: () = assert!(
    core::mem::size_of::<TstEvent>() <= 256,
    "TstEvent should fit in 256 bytes for stack-allocation comfort"
);

impl Default for TstEvent {
    fn default() -> Self {
        // SAFETY: zeroed union is valid for all our variants (every
        // payload struct is plain old data + nullable pointers).
        unsafe { core::mem::zeroed() }
    }
}

// ------------------------------------------------------------------
// EventArena — reusable per-handle backing storage
// ------------------------------------------------------------------

/// Per-handle arena owning the backing buffers pointed to by `TstEvent`
/// pointer fields. Reused across `_recv_event` calls — zero-alloc in
/// steady state once the Vecs reach their working size.
///
/// All Vec fields are cleared (not dropped) at the start of each
/// `convert()` call; capacity is retained.
///
/// `payload_buf` owns the byte ranges that pointer fields on `TstEvent`,
/// `TstNal`, `TstObu`, and `TstDescriptor` reference. Without this the
/// fill_* helpers used to write `payload.as_ptr()` into the C structs
/// while `payload` was borrowed from the input `DemuxEvent`, which is
/// dropped at the end of the `recv_event` closure — leaving C callers
/// with dangling pointers (validate-1 A2 / Codex CABI-02).
pub(crate) struct EventArena {
    pub(crate) nals: Vec<TstNal>,
    pub(crate) obus: Vec<TstObu>,
    pub(crate) descriptors: Vec<TstDescriptor>,
    pub(crate) stream_infos: Vec<TstStreamInfo>,
    pub(crate) klv_links: Vec<TstKlvLink>,
    /// CString buffer for NonConformant::Other(String).detail; one
    /// per convert() call.
    pub(crate) detail_buf: Vec<u8>,
    /// Programs array for PidReusedAcrossPrograms and PmtProgramNumberMismatch
    /// (always len 2 when used; for the latter [0]=pat_program, [1]=pmt_program).
    pub(crate) programs_buf: [u16; 2],
    /// Tags array for SubtitleDescriptorAmbiguous.
    pub(crate) tags_buf: Vec<u8>,
    /// Owned byte storage for every C pointer field that previously
    /// aliased input `DemuxEvent` storage. After all `convert()` extends
    /// complete, `payload_buf.as_ptr()` is the stable base for `base +
    /// offset` pointer resolution.
    ///
    /// For H.26x video samples the AU copy is skipped; `last_payload`
    /// retains the Arc instead and NAL/payload pointers point directly
    /// into its backing (DA-PERF-11).
    pub(crate) payload_buf: Vec<u8>,
    /// Retained `SharedBytes` Arc for the H.26x zero-copy AU path.
    /// Cleared (Arc dropped) at the start of each `convert()` call so
    /// NAL pointers from the previous event are no longer valid.
    pub(crate) last_payload: Option<tst_core::shared::SharedBytes>,
}

impl EventArena {
    pub(crate) fn new() -> Self {
        Self {
            nals: Vec::new(),
            obus: Vec::new(),
            descriptors: Vec::new(),
            stream_infos: Vec::new(),
            klv_links: Vec::new(),
            detail_buf: Vec::new(),
            programs_buf: [0; 2],
            tags_buf: Vec::new(),
            payload_buf: Vec::new(),
            last_payload: None,
        }
    }

    fn clear(&mut self) {
        self.nals.clear();
        self.obus.clear();
        self.descriptors.clear();
        self.stream_infos.clear();
        self.klv_links.clear();
        self.detail_buf.clear();
        self.programs_buf = [0; 2];
        self.tags_buf.clear();
        self.payload_buf.clear();
        // Drop any retained Arc so H.26x NAL pointers from the previous
        // event are immediately invalidated.
        self.last_payload = None;
    }
}

// ------------------------------------------------------------------
// convert() — DemuxEvent → TstEvent
// ------------------------------------------------------------------

/// Convert a `tst_core` `DemuxEvent` into a populated `TstEvent`,
/// using `arena` as backing storage for pointer-valued fields.
///
/// On return, `out` is fully populated:
/// * `out.kind` set to the corresponding `TstEventKind`.
/// * The active `u.*` body's fields filled; inactive bodies' contents
///   are unspecified (they share storage via the union).
/// * Pointer fields point into the arena's Vec buffers; valid until
///   the next `convert()` call clears the arena.
pub(crate) fn convert(
    arena: &mut EventArena,
    ev: &tst_core::mpegts::demux::DemuxEvent,
    out: &mut TstEvent,
) {
    use tst_core::mpegts::demux::DemuxEvent;
    arena.clear();
    *out = TstEvent::default();
    match ev {
        DemuxEvent::ProgramMap(pm) => fill_program_map(arena, pm, out),
        DemuxEvent::Sample {
            stream,
            pts,
            dts,
            payload,
        } => {
            // Pts90khz typed at the public Rust boundary; the C ABI keeps
            // DTS as `int64_t` ticks per
            // `reference_typed_pts_pcr_public_boundary.md`.
            fill_sample(
                arena,
                stream,
                pts.as_ticks(),
                dts.map(|d| d.as_ticks()),
                payload,
                out,
            );
        }
        DemuxEvent::Metadata {
            stream,
            pts,
            kind,
            payload,
        } => {
            fill_metadata(arena, stream, pts.as_ticks(), kind, payload, out);
        }
        DemuxEvent::Discontinuity { stream, kind } => {
            fill_discontinuity(stream, kind, out);
        }
        DemuxEvent::NonConformant { stream, issue } => {
            fill_nonconformant(arena, stream, issue, out);
        }
        DemuxEvent::ReconnectDiscontinuity => {
            // No-payload event surfaced only by the managed demux
            // receiver after the underlying transport reconnects and
            // sync / demux state was reset (followup-1). The union
            // body is left zero-initialized via the `*out =
            // TstEvent::default()` above.
            out.kind = TstEventKind::ReconnectDiscontinuity as c_int;
        }
    }
}

fn fill_program_map(
    arena: &mut EventArena,
    pm: &tst_core::mpegts::demux::ProgramMap,
    out: &mut TstEvent,
) {
    // Two-pass: first collect each descriptor's byte range as
    // (payload_buf offset, len) so the arena owns the bytes, then
    // resolve to base+offset pointers after all extends are done
    // (payload_buf.as_ptr() is only stable after the final extend).
    let mut per_stream_desc_ranges: Vec<(usize, usize)> = Vec::with_capacity(pm.streams.len());
    let mut desc_byte_records: Vec<(usize, usize)> = Vec::new();
    for si in &pm.streams {
        let start = arena.descriptors.len();
        for d in &si.raw_descriptors {
            let offset = arena.payload_buf.len();
            arena.payload_buf.extend_from_slice(&d.data);
            desc_byte_records.push((offset, d.data.len()));
            arena.descriptors.push(TstDescriptor {
                tag: d.tag,
                _reserved: [0; 7],
                // Pointer placeholder; resolved after payload_buf stops growing.
                data: core::ptr::null(),
                data_len: d.data.len(),
            });
        }
        per_stream_desc_ranges.push((start, arena.descriptors.len() - start));
    }
    // Resolve descriptor pointers now that payload_buf is fully populated
    // for this convert() call (PMTs don't share storage with NAL/OBU/
    // sample payloads; convert() processes one event at a time).
    let payload_base = arena.payload_buf.as_ptr();
    for (desc_slot, (offset, _len)) in arena.descriptors.iter_mut().zip(desc_byte_records.iter()) {
        // SAFETY: `offset` was returned by `payload_buf.len()` BEFORE the
        // extend that contributed `_len` bytes, so `base + offset` points
        // into the buf and `[base+offset .. base+offset+len]` is in-bounds.
        desc_slot.data = unsafe { payload_base.add(*offset) };
    }
    let descriptors_base = arena.descriptors.as_ptr();
    for (si, (start, count)) in pm.streams.iter().zip(per_stream_desc_ranges.iter()) {
        let (kind_tag, codec_int) = stream_kind_to_c(&si.kind);
        arena.stream_infos.push(TstStreamInfo {
            pid: si.pid,
            stream_type: si.stream_type.as_byte(),
            _pad: 0,
            stream_kind: kind_tag,
            codec: codec_int,
            program_number: si.program_number,
            _pad2: [0; 6],
            // SAFETY: descriptors_base offset is in bounds; we built
            // both arrays in this function and the base pointer is
            // stable until arena.clear() in the next convert() call.
            raw_descriptors: unsafe { descriptors_base.add(*start) },
            descriptor_count: *count,
        });
    }
    for link in &pm.klv_links {
        arena.klv_links.push(TstKlvLink {
            klv_pid: link.klv_pid,
            video_pid: link.video_pid,
            source: link_source_to_c(link.source),
        });
    }
    out.kind = TstEventKind::ProgramMap as c_int;
    out.u.program_map = TstEventProgramMap {
        program_number: pm.program_number,
        pcr_pid: pm.pcr_pid,
        pmt_pid: pm.pmt_pid,
        _pad: [0; 2],
        streams: arena.stream_infos.as_ptr(),
        stream_count: arena.stream_infos.len(),
        klv_links: arena.klv_links.as_ptr(),
        klv_link_count: arena.klv_links.len(),
    };
}

/// `TstEventSample::av1_carriage` "not applicable" sentinel: the sample is
/// non-AV1, or AV1 with an unknown/future carriage mode. Distinct from the
/// `0`/`1` carriage discriminants (`MPEG2_TS_BINDING` / `INTEROP_RAW_OBU`).
pub(crate) const TST_AV1_CARRIAGE_NA: u8 = 0xFF;

fn fill_sample(
    arena: &mut EventArena,
    stream: &tst_core::mpegts::demux::StreamId,
    pts: i64,
    dts: Option<i64>,
    payload: &tst_core::mpegts::demux::SamplePayload,
    out: &mut TstEvent,
) {
    use tst_core::mpegts::demux::{SamplePayload, VideoPayload, split_video};
    let (kind_tag, _codec_int) = stream_kind_to_c(&stream.kind);
    let mut codec = -1i32;
    let mut nals_ptr: *const TstNal = core::ptr::null();
    let mut nal_count: usize = 0;
    let mut obus_ptr: *const TstObu = core::ptr::null();
    let mut obu_count: usize = 0;
    let payload_ptr: *const u8;
    let payload_len: usize;
    let mut random_access_indicator: u8 = 0;
    let mut stream_type: u8 = 0;
    // N/A sentinel for non-AV1 samples; overwritten to 0 or 1 in the Video arm.
    let mut av1_carriage_byte: u8 = TST_AV1_CARRIAGE_NA;
    match payload {
        SamplePayload::Video {
            codec: vc,
            raw,
            random_access_indicator: rai,
            av1_carriage,
            ..
        } => {
            codec = crate::config::TstVideoCodec::from_core(*vc) as i32;
            random_access_indicator = u8::from(*rai);
            // 0=binding, 1=interop, 0xFF=N/A (non-AV1 or unknown).
            // `Av1CarriageMode` is `#[non_exhaustive]`; the wildcard catches
            // any future variants and falls back to the N/A sentinel.
            av1_carriage_byte = match av1_carriage {
                Some(tst_core::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding) => 0,
                Some(tst_core::mpegts::mux::Av1CarriageMode::InteropRawObu) => 1,
                Some(_) | None => TST_AV1_CARRIAGE_NA,
            };
            // Split into NAL/OBU units (the opt-in parse) so the
            // TstNal[]/TstObu[] surface keeps working; ES-conformance
            // issues are not surfaced over this C ABI.
            //
            // H.26x (Nals arm): zero-copy path — retain the SharedBytes
            // Arc in `arena.last_payload` and point payload_ptr / each NAL
            // slot directly into its backing. No copy into payload_buf.
            //
            // AV1 (Obus arm): keep the arena copy. Binding-mode OBUs are
            // unwrapped into a fresh buffer (not subslices of the AU), so
            // `unit_arena_offset` needs a stable arena base to fall back to.
            let raw_base = raw.as_ptr() as usize;
            let raw_len = raw.len();
            let (vp, _issues) = split_video(raw, *vc, av1_carriage.unwrap_or_default());
            match &vp {
                VideoPayload::Nals(nals) => {
                    // Retain Arc so NAL pointers remain valid until the next
                    // convert() call drops last_payload via clear().
                    arena.last_payload = Some(raw.clone());
                    for n in nals {
                        let bytes = nal_payload_bytes(n);
                        let mut c = nal_to_c(n);
                        // SAFETY: bytes is a subslice of the SharedBytes
                        // retained in arena.last_payload; valid until the
                        // next arena.clear() call.
                        c.payload = bytes.as_ptr();
                        arena.nals.push(c);
                    }
                    nals_ptr = arena.nals.as_ptr();
                    nal_count = arena.nals.len();
                    // SAFETY: raw is retained via arena.last_payload above;
                    // the pointer is valid until the next convert() call.
                    payload_ptr = raw.as_ptr();
                    payload_len = raw_len;
                }
                VideoPayload::Obus(obus) => {
                    // Copy the AU once; OBU slots point into the copy.
                    let raw_off = arena.payload_buf.len();
                    arena.payload_buf.extend_from_slice(raw);
                    let mut records: Vec<usize> = Vec::with_capacity(obus.len());
                    for o in obus {
                        records.push(unit_arena_offset(
                            arena, raw_off, raw_base, raw_len, &o.payload,
                        ));
                        arena.obus.push(obu_to_c(o));
                    }
                    let base = arena.payload_buf.as_ptr();
                    for (slot, offset) in arena.obus.iter_mut().zip(records.iter()) {
                        // SAFETY: offset is within payload_buf (see unit_arena_offset).
                        slot.payload = unsafe { base.add(*offset) };
                    }
                    obus_ptr = arena.obus.as_ptr();
                    obu_count = arena.obus.len();
                    // Resolve the AU pointer only after every extend is done
                    // (Vec base pointer is stable from here until the next
                    // convert()). SAFETY: raw_len bytes appended at raw_off.
                    payload_ptr = unsafe { arena.payload_buf.as_ptr().add(raw_off) };
                    payload_len = raw_len;
                }
            }
        }
        SamplePayload::Audio { codec: ac, frames } => {
            codec = crate::config::TstAudioCodec::from_core(*ac) as i32;
            arena.payload_buf.extend_from_slice(frames);
            payload_ptr = arena.payload_buf.as_ptr();
            payload_len = arena.payload_buf.len();
        }
        SamplePayload::Subtitle {
            codec: sc,
            payload: pl,
        } => {
            codec = crate::config::TstSubtitleCodec::from_core(*sc) as i32;
            arena.payload_buf.extend_from_slice(pl);
            payload_ptr = arena.payload_buf.as_ptr();
            payload_len = arena.payload_buf.len();
        }
        SamplePayload::Unknown {
            stream_type: st,
            raw,
        } => {
            // codec stays -1; surface the raw PMT stream_type byte so C
            // callers can discriminate without correlating back to the
            // most recent ProgramMap event. `st` is StreamTypeCode (typed
            // wrapper from plan #75); .as_byte() preserves the existing
            // uint8_t C ABI for TstEventSample.stream_type per plan #71.
            stream_type = st.as_byte();
            arena.payload_buf.extend_from_slice(raw);
            payload_ptr = arena.payload_buf.as_ptr();
            payload_len = arena.payload_buf.len();
        }
    }
    out.kind = TstEventKind::Sample as c_int;
    out.u.sample = TstEventSample {
        pid: stream.pid,
        program_number: stream.program_number,
        stream_kind: kind_tag,
        pts,
        dts: dts.unwrap_or(i64::MIN),
        codec,
        random_access_indicator,
        stream_type,
        av1_carriage: av1_carriage_byte,
        _pad: [0; 1],
        nals: nals_ptr,
        nal_count,
        obus: obus_ptr,
        obu_count,
        payload: payload_ptr,
        payload_len,
    };
}

fn fill_metadata(
    arena: &mut EventArena,
    stream: &tst_core::mpegts::demux::StreamId,
    pts: i64,
    kind: &tst_core::mpegts::demux::MetadataKind,
    payload: &[u8],
    out: &mut TstEvent,
) {
    use tst_core::mpegts::demux::MetadataKind;
    let mut metadata_service_id = 0u8;
    let mut sequence_number = 0u8;
    let mut cell_fragment_indication = 0u8;
    let mut decoder_config_flag = 0u8;
    let mut random_access_indicator = 0u8;
    let mut was_reassembled = false;
    let mut cell_count: u32 = 0;
    let md_kind = match kind {
        MetadataKind::KlvSyncAuCell {
            metadata_service_id: sid,
            sequence_number: seq,
            cell_fragment_indication: cfi,
            decoder_config_flag: dcf,
            random_access_indicator: rai,
            was_reassembled: wr,
            cell_count: cc,
        } => {
            metadata_service_id = *sid;
            sequence_number = *seq;
            cell_fragment_indication = *cfi as u8;
            decoder_config_flag = *dcf as u8;
            random_access_indicator = *rai as u8;
            was_reassembled = *wr;
            cell_count = *cc;
            TstMetadataKindTag::KlvSyncAuCell as c_int
        }
        MetadataKind::KlvAsync => TstMetadataKindTag::KlvAsync as c_int,
        MetadataKind::Unknown(_) => TstMetadataKindTag::Unknown as c_int,
    };
    arena.payload_buf.extend_from_slice(payload);
    out.kind = TstEventKind::Metadata as c_int;
    out.u.metadata = TstEventMetadata {
        pid: stream.pid,
        program_number: stream.program_number,
        _pad: [0; 4],
        pts,
        metadata_kind: md_kind,
        _pad2: [0; 4],
        payload: arena.payload_buf.as_ptr(),
        payload_len: arena.payload_buf.len(),
        metadata_service_id,
        sequence_number,
        cell_fragment_indication,
        decoder_config_flag,
        random_access_indicator,
        _pad3: [0; 3],
        was_reassembled,
        _pad4: [0; 3],
        cell_count,
    };
}

fn fill_discontinuity(
    stream: &tst_core::mpegts::demux::StreamId,
    kind: &tst_core::mpegts::demux::DiscontinuityKind,
    out: &mut TstEvent,
) {
    use tst_core::mpegts::demux::DiscontinuityKind;
    // `variant_pid` preserves the variant-specific PID for kinds that carry
    // their own (currently only `PesOversize { pid }`); 0 for the others.
    // Codex review pass-1 flagged the previous `pid: _` discard as
    // identity-loss — the variant's PID usually matches `stream.pid` but
    // the variant carries it independently for the rare divergence case.
    let (tag, cc_expected, cc_observed, variant_pid) = match kind {
        DiscontinuityKind::ContinuityJump { expected, observed } => (
            TstDiscontinuityKindTag::ContinuityJump as c_int,
            *expected,
            *observed,
            0,
        ),
        DiscontinuityKind::PesOversize { pid } => {
            (TstDiscontinuityKindTag::PesOversize as c_int, 0, 0, *pid)
        }
        DiscontinuityKind::PesTotalOversize => {
            (TstDiscontinuityKindTag::PesTotalOversize as c_int, 0, 0, 0)
        }
        DiscontinuityKind::AdaptationFieldFlag => (
            TstDiscontinuityKindTag::AdaptationFieldFlag as c_int,
            0,
            0,
            0,
        ),
    };
    out.kind = TstEventKind::Discontinuity as c_int;
    out.u.discontinuity = TstEventDiscontinuity {
        pid: stream.pid,
        variant_pid,
        discontinuity_kind: tag,
        cc_expected,
        cc_observed,
        _pad2: [0; 6],
    };
}

fn fill_nonconformant(
    arena: &mut EventArena,
    stream: &tst_core::mpegts::demux::StreamId,
    issue: &tst_core::mpegts::demux::NonConformantIssue,
    out: &mut TstEvent,
) {
    use tst_core::mpegts::demux::NonConformantIssue;
    let mut body = TstEventNonConformant {
        pid: stream.pid,
        _pad: [0; 2],
        issue_code: TstNonConformantCode::Other as c_int,
        pcr_delta: 0,
        table_id: 0,
        last_section_number: 0,
        cc_expected: 0,
        cc_observed: 0,
        _pad2: [0; 4],
        observed_len: 0,
        obu_type: 0,
        _pad3: [0; 3],
        multi_cell_au_reason: 0,
        programs: core::ptr::null(),
        tags: core::ptr::null(),
        tag_count: 0,
        detail: core::ptr::null(),
    };
    match issue {
        NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid => {
            body.issue_code = TstNonConformantCode::StreamTypeMismatchSyncOnAsyncPid as c_int;
        }
        NonConformantIssue::StreamTypeMismatchAsyncOnSyncPid => {
            body.issue_code = TstNonConformantCode::StreamTypeMismatchAsyncOnSyncPid as c_int;
        }
        NonConformantIssue::MissingMetadataDescriptor => {
            body.issue_code = TstNonConformantCode::MissingMetadataDescriptor as c_int;
        }
        NonConformantIssue::PcrAnomaly { delta } => {
            body.issue_code = TstNonConformantCode::PcrAnomaly as c_int;
            body.pcr_delta = *delta;
        }
        NonConformantIssue::PsiChecksumMismatch { pid } => {
            body.issue_code = TstNonConformantCode::PsiChecksumMismatch as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::PusiMidPes => {
            body.issue_code = TstNonConformantCode::PusiMidPes as c_int;
        }
        NonConformantIssue::PidReusedAcrossPrograms { pid, programs } => {
            body.issue_code = TstNonConformantCode::PidReusedAcrossPrograms as c_int;
            body.pid = *pid;
            arena.programs_buf = *programs;
            body.programs = arena.programs_buf.as_ptr();
        }
        NonConformantIssue::SubtitleMissingDescriptor { pid } => {
            body.issue_code = TstNonConformantCode::SubtitleMissingDescriptor as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::SubtitleDescriptorAmbiguous { pid, tags } => {
            body.issue_code = TstNonConformantCode::SubtitleDescriptorAmbiguous as c_int;
            body.pid = *pid;
            arena.tags_buf.extend_from_slice(tags);
            body.tags = arena.tags_buf.as_ptr();
            body.tag_count = arena.tags_buf.len();
        }
        NonConformantIssue::SubtitleDescriptorMalformed { pid, tag } => {
            body.issue_code = TstNonConformantCode::SubtitleDescriptorMalformed as c_int;
            body.pid = *pid;
            body.table_id = *tag; // reuse table_id field as the descriptor tag carrier
        }
        NonConformantIssue::Av1RegistrationMalformed { pid } => {
            body.issue_code = TstNonConformantCode::Av1RegistrationMalformed as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::Av1ObuMissingSizeField { pid, obu_type } => {
            body.issue_code = TstNonConformantCode::Av1ObuMissingSizeField as c_int;
            body.pid = *pid;
            body.obu_type = *obu_type;
        }
        NonConformantIssue::Av1TileListNotAllowed { pid } => {
            body.issue_code = TstNonConformantCode::Av1TileListNotAllowed as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::PsiOverlongSection { pid, observed_len } => {
            body.issue_code = TstNonConformantCode::PsiOverlongSection as c_int;
            body.pid = *pid;
            body.observed_len = *observed_len;
        }
        NonConformantIssue::TransportErrorPacket { pid } => {
            body.issue_code = TstNonConformantCode::TransportErrorPacket as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::PsiCcDiscontinuity {
            pid,
            expected,
            observed,
        } => {
            body.issue_code = TstNonConformantCode::PsiCcDiscontinuity as c_int;
            body.pid = *pid;
            body.cc_expected = *expected;
            body.cc_observed = *observed;
        }
        NonConformantIssue::MultiCellAu {
            pid,
            dropped_bytes,
            reason,
        } => {
            use tst_core::mpegts::demux::event::MultiCellAuReason;
            body.issue_code = TstNonConformantCode::MultiCellAu as c_int;
            body.pid = *pid;
            body.observed_len = *dropped_bytes;
            // `MultiCellAuReason` is `#[non_exhaustive]`; future variants
            // fall through to `Orphan` (0) so the consumer-visible code
            // remains stable. Add a new `TstMultiCellAuReason` variant
            // when a future Rust variant warrants distinct C-side handling.
            body.multi_cell_au_reason = match reason {
                MultiCellAuReason::Orphan => TstMultiCellAuReason::Orphan as c_int,
                MultiCellAuReason::SequenceGap => TstMultiCellAuReason::SequenceGap as c_int,
                MultiCellAuReason::ConcurrentFirst => {
                    TstMultiCellAuReason::ConcurrentFirst as c_int
                }
                MultiCellAuReason::Overflow => TstMultiCellAuReason::Overflow as c_int,
                MultiCellAuReason::OverflowTotal => TstMultiCellAuReason::OverflowTotal as c_int,
                MultiCellAuReason::TooManyPids => TstMultiCellAuReason::TooManyPids as c_int,
                _ => TstMultiCellAuReason::Orphan as c_int,
            };
        }
        NonConformantIssue::PsiMultiSectionUnsupported {
            pid,
            table_id,
            last_section_number,
        } => {
            body.issue_code = TstNonConformantCode::PsiMultiSectionUnsupported as c_int;
            body.pid = *pid;
            body.table_id = *table_id;
            body.last_section_number = *last_section_number;
        }
        NonConformantIssue::CfiTolerated {
            pid,
            observed_cfi,
            treated_as,
        } => {
            use tst_core::mpegts::au_cell::CellFragmentIndication;
            // Map CellFragmentIndication discriminants to the mirror;
            // the discriminant values agree with the wire bits, so this
            // is a one-to-one cast.
            let to_byte = |c: CellFragmentIndication| match c {
                CellFragmentIndication::Middle => TstCellFragmentIndication::Middle as u8,
                CellFragmentIndication::Last => TstCellFragmentIndication::Last as u8,
                CellFragmentIndication::First => TstCellFragmentIndication::First as u8,
                CellFragmentIndication::Complete => TstCellFragmentIndication::Complete as u8,
            };
            body.issue_code = TstNonConformantCode::CfiTolerated as c_int;
            body.pid = *pid;
            // Reuse the existing single-byte carriers: cc_expected for
            // the observed CFI, cc_observed for the substituted value
            // (mirrors PsiCcDiscontinuity's expected/observed shape).
            // Avoids growing TstEventNonConformant on this ABI bump.
            body.cc_expected = to_byte(*observed_cfi);
            body.cc_observed = to_byte(*treated_as);
        }
        NonConformantIssue::Other(s) => {
            body.issue_code = TstNonConformantCode::Other as c_int;
            arena.detail_buf.extend_from_slice(s.as_bytes());
            arena.detail_buf.push(0); // NUL terminator
            body.detail = arena.detail_buf.as_ptr() as *const c_char;
        }
        NonConformantIssue::DvbSubDataIdentifier { observed } => {
            body.issue_code = TstNonConformantCode::DvbSubDataIdentifier as c_int;
            body.table_id = *observed; // reuse table_id field as the observed-byte carrier
        }
        NonConformantIssue::MalformedPes { pid, reason } => {
            body.issue_code = TstNonConformantCode::MalformedPes as c_int;
            body.pid = *pid;
            // Route the &'static str reason through detail_buf (NUL-terminated)
            // so C callers read it via body.detail, mirroring the Other arm.
            arena.detail_buf.extend_from_slice(reason.as_bytes());
            arena.detail_buf.push(0); // NUL terminator
            body.detail = arena.detail_buf.as_ptr() as *const c_char;
        }
        NonConformantIssue::PtsAnomaly { delta } => {
            // B4 — PTS delta in 90 kHz ticks. Re-uses `pcr_delta` field;
            // the issue_code disambiguates the unit (27 MHz for PcrAnomaly
            // vs 90 kHz for PtsAnomaly).
            body.issue_code = TstNonConformantCode::PtsAnomaly as c_int;
            body.pcr_delta = *delta;
        }
        NonConformantIssue::MissingRequiredPts { pid } => {
            body.issue_code = TstNonConformantCode::MissingRequiredPts as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::PesHeaderMalformed { pid, kind } => {
            use tst_core::mpegts::demux::PesHeaderMalformedKind;
            body.issue_code = TstNonConformantCode::PesHeaderMalformed as c_int;
            body.pid = *pid;
            // Re-use `table_id` field as the kind discriminator. Match
            // values per the docstring on TstNonConformantCode::PesHeaderMalformed.
            body.table_id = match kind {
                PesHeaderMalformedKind::ForbiddenPtsDtsFlags => 0,
                PesHeaderMalformedKind::InvalidMarkerBits => 1,
                PesHeaderMalformedKind::InvalidPtsPrefix => 2,
                PesHeaderMalformedKind::InvalidDtsPrefix => 3,
                PesHeaderMalformedKind::InvalidPtsDtsMarkerBits => 4,
                // `PesHeaderMalformedKind` is #[non_exhaustive]. Future
                // variants surface as 0xFF until the C mapping is widened.
                _ => 0xFF,
            };
        }
        NonConformantIssue::SubtitleAlignmentMissing { pid } => {
            body.issue_code = TstNonConformantCode::SubtitleAlignmentMissing as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::PcrMalformed { kind } => {
            use tst_core::mpegts::demux::PcrMalformedKind;
            body.issue_code = TstNonConformantCode::PcrMalformed as c_int;
            // Reuse the table_id field as a `TstPcrMalformedKind` discriminator
            // (u8 wide; PcrMalformedKind has 2 variants today, comfortably
            // under 256). Mirrors the SubtitleDescriptorMalformed reuse of
            // the same field.
            body.table_id = match kind {
                PcrMalformedKind::InvalidReservedBits => {
                    TstPcrMalformedKind::InvalidReservedBits as u8
                }
                PcrMalformedKind::ExtensionOutOfRange => {
                    TstPcrMalformedKind::ExtensionOutOfRange as u8
                }
                // PcrMalformedKind is #[non_exhaustive]; future variants
                // fall back to InvalidReservedBits until the C surface
                // gains a discriminator entry. The bash ratchet
                // scripts/check/c/raw-mapper-coverage.sh covers MuxError /
                // TransportError but not PcrMalformedKind; rely on this
                // wildcard plus the explicit arms above.
                _ => 0xFF,
            };
        }
        NonConformantIssue::NalHeader { codec, kind } => {
            use tst_core::mpegts::demux::{NalHeaderKind, VideoCodec};
            body.issue_code = TstNonConformantCode::NalHeader as c_int;
            // Encode codec byte on table_id (reuses existing u8 carrier;
            // values match TstVideoCodec discriminants: H264=0, H265=1,
            // H266=2, Av1=3).
            body.table_id = match codec {
                VideoCodec::H264 => 0,
                VideoCodec::H265 => 1,
                VideoCodec::H266 => 2,
                VideoCodec::Av1 => 3,
            };
            // Encode the NalHeaderKind discriminator on the obu_type carrier
            // (a free u8 on TstEventNonConformant). LayerIdOutOfRange's `id`
            // additionally lands on cc_observed.
            // `NalHeaderKind` is `#[non_exhaustive]` (future-extension
            // contract); the wildcard arm encodes future variants as a
            // sentinel `0xFF` so C callers see "unknown kind" rather than
            // a silent miscategorization. Update with each new variant.
            let (kind_code, layer_id) = match kind {
                NalHeaderKind::ForbiddenZeroBit => (0u8, 0u8),
                NalHeaderKind::ReservedBit => (1u8, 0u8),
                NalHeaderKind::ZeroTemporalIdPlus1 => (2u8, 0u8),
                NalHeaderKind::LayerIdOutOfRange { id } => (3u8, *id),
                _ => (0xFFu8, 0u8),
            };
            body.obu_type = kind_code;
            body.cc_observed = layer_id;
        }
        NonConformantIssue::Av1ObuHeader { pid, kind } => {
            use tst_core::mpegts::demux::Av1ObuHeaderKind;
            body.issue_code = TstNonConformantCode::Av1ObuHeader as c_int;
            body.pid = *pid;
            // `Av1ObuHeaderKind` is `#[non_exhaustive]`; wildcard arm
            // encodes future variants as sentinel `0xFF`.
            body.obu_type = match kind {
                Av1ObuHeaderKind::ForbiddenBit => 0,
                Av1ObuHeaderKind::ReservedBit => 1,
                Av1ObuHeaderKind::ExtensionReservedBits => 2,
                _ => 0xFF,
            };
        }
        NonConformantIssue::Ac3SyncMissing { pid } => {
            body.issue_code = TstNonConformantCode::Ac3SyncMissing as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::LatmFraming { pid, kind } => {
            use tst_core::codec::aac::latm::LatmFramingKind;
            body.issue_code = TstNonConformantCode::LatmFraming as c_int;
            body.pid = *pid;
            // `LatmFramingKind` is `#[non_exhaustive]`; wildcard arm
            // encodes future variants as sentinel `0xFF`. Discriminator
            // lands on the `obu_type` carrier (free u8 byte; reuses the
            // pattern from NalHeader / Av1ObuHeader).
            body.obu_type = match kind {
                LatmFramingKind::MissingSyncword => 0,
                LatmFramingKind::AudioMuxLengthOverrun => 1,
                LatmFramingKind::Truncated => 2,
                _ => 0xFF,
            };
        }
        NonConformantIssue::Av1WrongStreamId { pid, observed } => {
            // AV1-in-MPEG-2-TS binding §3.4 violation. `table_id` carrier
            // surfaces the observed stream_id byte (re-uses the same field
            // as DvbSubDataIdentifier / SubtitleDescriptorMalformed —
            // C ABI conserves struct width by routing per-variant bytes
            // through shared carriers; the issue_code disambiguates).
            body.issue_code = TstNonConformantCode::Av1WrongStreamId as c_int;
            body.pid = *pid;
            body.table_id = *observed;
        }
        NonConformantIssue::Av1MissingTsObuFraming { pid } => {
            // AV1-in-MPEG-2-TS binding §3.2 violation — no per-variant
            // payload beyond the PID.
            body.issue_code = TstNonConformantCode::Av1MissingTsObuFraming as c_int;
            body.pid = *pid;
        }
        NonConformantIssue::PmtProgramNumberMismatch {
            pid,
            pat_program,
            pmt_program,
        } => {
            // REF-PSI-01. Reuse the `programs_buf` carrier (same two-element
            // layout as PidReusedAcrossPrograms): programs[0]=pat_program,
            // programs[1]=pmt_program. No struct growth needed.
            body.issue_code = TstNonConformantCode::PmtProgramNumberMismatch as c_int;
            body.pid = *pid;
            arena.programs_buf = [*pat_program, *pmt_program];
            body.programs = arena.programs_buf.as_ptr();
        }
        NonConformantIssue::UnsupportedScrambling { pid, control } => {
            body.issue_code = TstNonConformantCode::UnsupportedScrambling as c_int;
            body.pid = *pid;
            body.table_id = *control; // 1, 2, or 3
        }
        NonConformantIssue::AdaptationFieldMalformed { pid, kind } => {
            use tst_core::mpegts::demux::AdaptationFieldKind;
            body.issue_code = TstNonConformantCode::AdaptationFieldMalformed as c_int;
            body.pid = *pid;
            body.table_id = match kind {
                AdaptationFieldKind::ReservedControl => 0,
                AdaptationFieldKind::BadLengthForControl => 1,
                AdaptationFieldKind::ShortPcr => 2,
                _ => 0xFF,
            };
        }
        NonConformantIssue::ZeroLengthPesNonVideo { pid, stream_id } => {
            body.issue_code = TstNonConformantCode::ZeroLengthPesNonVideo as c_int;
            body.pid = *pid;
            body.table_id = *stream_id; // PES stream_id byte (reuses table_id carrier)
        }
        NonConformantIssue::PsiSyntax {
            pid,
            table_id,
            kind,
        } => {
            use tst_core::mpegts::demux::PsiSyntaxKind;
            body.issue_code = TstNonConformantCode::PsiSyntax as c_int;
            body.pid = *pid;
            body.table_id = *table_id;
            let (kind_code, observed) = match kind {
                PsiSyntaxKind::SectionSyntaxIndicatorUnset => (0u8, 0u8),
                PsiSyntaxKind::SectionNumberNonZero { observed } => (1u8, *observed),
                PsiSyntaxKind::ReservedBits => (2u8, 0u8),
                _ => (0xFFu8, 0u8),
            };
            body.obu_type = kind_code;
            body.cc_observed = observed;
        }
    }
    out.kind = TstEventKind::NonConformant as c_int;
    out.u.nonconformant = body;
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

fn stream_kind_to_c(kind: &tst_core::mpegts::demux::StreamKind) -> (c_int, c_int) {
    use tst_core::mpegts::demux::StreamKind;
    match kind {
        StreamKind::Video(c) => (
            TstStreamKindTag::Video as c_int,
            crate::config::TstVideoCodec::from_core(*c) as c_int,
        ),
        StreamKind::Audio(c) => (
            TstStreamKindTag::Audio as c_int,
            crate::config::TstAudioCodec::from_core(*c) as c_int,
        ),
        StreamKind::Subtitle(c) => (
            TstStreamKindTag::Subtitle as c_int,
            crate::config::TstSubtitleCodec::from_core(*c) as c_int,
        ),
        StreamKind::KlvSync { .. } => (TstStreamKindTag::KlvSync as c_int, -1),
        StreamKind::KlvAsync => (TstStreamKindTag::KlvAsync as c_int, -1),
        StreamKind::Unknown(_) => (TstStreamKindTag::Unknown as c_int, -1),
    }
}

fn link_source_to_c(s: tst_core::mpegts::demux::LinkSource) -> c_int {
    use tst_core::mpegts::demux::LinkSource;
    match s {
        LinkSource::Declared => TstLinkSource::Declared as c_int,
        LinkSource::Inferred => TstLinkSource::Inferred as c_int,
        LinkSource::Override => TstLinkSource::Override as c_int,
    }
}

/// Extract the payload byte slice from a [`NalUnit`] without copying.
/// Single-source-of-truth for the codec-variant → bytes mapping; used
/// during `fill_sample`'s two-pass arena-extend/resolve.
fn nal_payload_bytes(n: &tst_core::mpegts::demux::NalUnit) -> &[u8] {
    use tst_core::mpegts::demux::NalUnit;
    match n {
        NalUnit::H264 { payload, .. }
        | NalUnit::H265 { payload, .. }
        | NalUnit::H266 { payload, .. } => payload,
    }
}

/// Arena offset for one split-unit's bytes during `fill_sample`'s Video
/// arm. `split_video` yields zero-copy subslices of the raw AU for H.26x
/// (and AV1 raw-OBU carriage) — reuse the single AU copy already in
/// `payload_buf` at the matching offset. AV1 *binding-mode* unwraps into a
/// fresh buffer (units are NOT subslices of the AU) — append a per-unit
/// copy instead. `raw_off` is the AU copy's offset in `payload_buf`;
/// `raw_base`/`raw_len` describe the live input AU backing.
fn unit_arena_offset(
    arena: &mut EventArena,
    raw_off: usize,
    raw_base: usize,
    raw_len: usize,
    bytes: &[u8],
) -> usize {
    let p = bytes.as_ptr() as usize;
    // Empty slices are excluded — an empty slice's dangling as_ptr() must
    // not be offset-mapped into the AU copy. The end-address sums below
    // cannot overflow: `bytes` and the AU backing are live slices, and Rust
    // guarantees a live allocation is <= isize::MAX bytes and does not wrap
    // the address space, so `ptr + len` is a representable one-past-end
    // address for both.
    if !bytes.is_empty() && p >= raw_base && p + bytes.len() <= raw_base + raw_len {
        raw_off + (p - raw_base)
    } else {
        let off = arena.payload_buf.len();
        arena.payload_buf.extend_from_slice(bytes);
        off
    }
}

/// Build a `TstNal` with metadata fields populated and `payload` set to
/// null + `payload_len` carrying the actual size. The caller (`fill_sample`)
/// resolves `payload` to an arena-owned pointer after `payload_buf` stops
/// growing in the current `convert()` call.
fn nal_to_c(n: &tst_core::mpegts::demux::NalUnit) -> TstNal {
    use tst_core::mpegts::demux::NalUnit;
    match n {
        NalUnit::H264 {
            nal_type,
            ref_idc,
            payload,
        } => TstNal {
            nal_type: *nal_type,
            ref_idc_or_layer_id: *ref_idc,
            temporal_id_plus1: 0,
            _reserved: 0,
            payload: core::ptr::null(),
            payload_len: payload.len(),
        },
        NalUnit::H265 {
            nal_type,
            layer_id,
            temporal_id_plus1,
            payload,
        }
        | NalUnit::H266 {
            nal_type,
            layer_id,
            temporal_id_plus1,
            payload,
        } => TstNal {
            nal_type: *nal_type,
            ref_idc_or_layer_id: *layer_id,
            temporal_id_plus1: *temporal_id_plus1,
            _reserved: 0,
            payload: core::ptr::null(),
            payload_len: payload.len(),
        },
    }
}

/// Build a `TstObu` with metadata fields populated and `payload` set to
/// null. Caller (`fill_sample`) resolves the pointer after extends finish.
fn obu_to_c(o: &tst_core::mpegts::demux::Obu) -> TstObu {
    match &o.extension {
        Some(ext) => TstObu {
            obu_type: o.obu_type,
            has_extension: 1,
            temporal_id: ext.temporal_id,
            spatial_id: ext.spatial_id,
            payload: core::ptr::null(),
            payload_len: o.payload.len(),
        },
        None => TstObu {
            obu_type: o.obu_type,
            has_extension: 0,
            temporal_id: 0,
            spatial_id: 0,
            payload: core::ptr::null(),
            payload_len: o.payload.len(),
        },
    }
}

// =========================================================================
// Tests — arena ownership of payload bytes (validate-1 A2 / CABI-02)
// =========================================================================
//
// The docstring at the top of this file promises that all pointer fields
// on TstEvent borrow from the EventArena, valid until the next recv_event
// or close on the same handle. Pre-A2, that contract was violated for
// every byte-payload pointer field — they aliased the input DemuxEvent
// storage, which is dropped at the end of the recv_event closure. C
// callers would dereference dangling pointers.
//
// These tests assert that after convert(), each C pointer field is NOT
// the input Vec's data pointer (i.e., the arena holds an owned copy).
// Vec moves are pointer-stable, so capturing the source `.as_ptr()`
// BEFORE moving the Vec into the DemuxEvent gives us a deterministic
// reference for the "owns its own bytes" check.

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use tst_core::mpegts::common::{Pts90khz, StreamTypeCode};
    use tst_core::mpegts::demux::{
        AudioCodec, DemuxEvent, MetadataKind, SamplePayload, StreamId, StreamKind, SubtitleCodec,
        VideoCodec,
    };
    use tst_core::shared::SharedBytes;

    fn stream_id(pid: u16, kind: StreamKind) -> StreamId {
        StreamId {
            pid,
            kind,
            program_number: 1,
        }
    }

    #[test]
    fn audio_sample_payload_is_arena_owned() {
        let frames = SharedBytes::from_vec(vec![0xAAu8, 0xBB, 0xCC, 0xDD]);
        let input_ptr = frames.as_ptr();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x100, StreamKind::Audio(AudioCodec::Aac)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Audio {
                codec: AudioCodec::Aac,
                frames,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let out_ptr = unsafe { out.u.sample.payload };
        assert_ne!(
            out_ptr, input_ptr,
            "audio payload pointer must point into the arena, not alias the input buffer"
        );
        let payload_len = unsafe { out.u.sample.payload_len };
        let out_bytes = unsafe { core::slice::from_raw_parts(out_ptr, payload_len) };
        assert_eq!(out_bytes, &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn subtitle_sample_payload_is_arena_owned() {
        let pl = SharedBytes::from_vec(vec![0x20u8, 0x00, 0xDE, 0xAD, 0xFF]);
        let input_ptr = pl.as_ptr();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x200, StreamKind::Subtitle(SubtitleCodec::DvbSubtitling)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Subtitle {
                codec: SubtitleCodec::DvbSubtitling,
                payload: pl,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let out_ptr = unsafe { out.u.sample.payload };
        assert_ne!(
            out_ptr, input_ptr,
            "subtitle payload pointer must point into the arena, not alias the input buffer"
        );
        let payload_len = unsafe { out.u.sample.payload_len };
        let out_bytes = unsafe { core::slice::from_raw_parts(out_ptr, payload_len) };
        assert_eq!(out_bytes, &[0x20u8, 0x00, 0xDE, 0xAD, 0xFF]);
    }

    #[test]
    fn unknown_sample_raw_is_arena_owned() {
        let raw = SharedBytes::from_vec(vec![0x01u8, 0x02, 0x03]);
        let input_ptr = raw.as_ptr();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x300, StreamKind::Unknown(0xFE)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Unknown {
                stream_type: StreamTypeCode::Unknown(0xFE),
                raw,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let out_ptr = unsafe { out.u.sample.payload };
        assert_ne!(
            out_ptr, input_ptr,
            "unknown sample raw pointer must point into the arena, not alias the input buffer"
        );
        let payload_len = unsafe { out.u.sample.payload_len };
        let out_bytes = unsafe { core::slice::from_raw_parts(out_ptr, payload_len) };
        assert_eq!(out_bytes, &[0x01u8, 0x02, 0x03]);
    }

    #[test]
    fn metadata_payload_is_arena_owned() {
        let payload = vec![0x06u8, 0x0Eu8, 0x2Bu8, 0x34u8];
        let payload_ptr_before = payload.as_ptr();
        let ev = DemuxEvent::Metadata {
            stream: stream_id(0x400, StreamKind::KlvAsync),
            pts: Pts90khz::new(0),
            kind: MetadataKind::KlvAsync,
            payload,
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let out_ptr = unsafe { out.u.metadata.payload };
        assert_ne!(
            out_ptr, payload_ptr_before,
            "metadata payload pointer must NOT alias the input Vec"
        );
    }

    #[test]
    fn h264_nal_payload_points_into_sharedbytes_backing() {
        // DA-PERF-11 zero-copy path: the demuxer emits the encoded AU;
        // `convert` `split_video`s it and retains the SharedBytes Arc in
        // `arena.last_payload`. NAL payloads are subslices of that Arc —
        // their pointers MUST fall within the live SharedBytes backing range,
        // not alias a separate arena copy.
        let shared = SharedBytes::from_vec(vec![0x00u8, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E]);
        let backing = shared.as_ptr() as usize;
        let backing_len = shared.len();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x500, StreamKind::Video(VideoCodec::H264)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: shared,
                random_access_indicator: true,
                av1_carriage: None,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        assert_eq!(arena.nals.len(), 1);
        let nal_ptr = arena.nals[0].payload as usize;
        let nal_len = arena.nals[0].payload_len;
        // Zero-copy proof: NAL payload MUST be within the SharedBytes backing.
        assert!(
            nal_ptr >= backing && nal_ptr + nal_len <= backing + backing_len,
            "H.264 NAL payload range must be within the retained SharedBytes backing"
        );
        // Content check: NAL body bytes (1-byte header 0x67 stripped).
        let nal_bytes = unsafe { core::slice::from_raw_parts(arena.nals[0].payload, nal_len) };
        assert_eq!(nal_bytes, &[0x42, 0x00, 0x1E]);
    }

    #[test]
    fn av1_obu_payload_is_arena_owned() {
        // Raw-first: build a single raw OBU (interop carriage, no binding
        // framing). Header = (obu_type=1 << 3) | has_size(0x02) = 0x0A;
        // LEB128 size = 0x03; body = 0x0A 0x0B 0x0C. `split_video(_, Av1)`
        // falls back to raw-OBU parsing; the arena copy must not alias the
        // SharedBytes backing. Same live-backing range check as the H.264
        // test above (a pre-`from_vec` Vec pointer is freed by `from_vec`
        // and can be reused by the arena — the windows-msvc flake).
        let shared = SharedBytes::from_vec(vec![0x0Au8, 0x03, 0x0A, 0x0B, 0x0C]);
        let backing = shared.as_ptr() as usize;
        let backing_len = shared.len();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x600, StreamKind::Video(VideoCodec::Av1)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::Av1,
                raw: shared,
                random_access_indicator: true,
                // av1_carriage: None → fill_sample maps to 0xFF (N/A sentinel).
                av1_carriage: None,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        assert_eq!(arena.obus.len(), 1);
        let obu_ptr = arena.obus[0].payload as usize;
        let obu_len = arena.obus[0].payload_len;
        assert!(
            obu_ptr + obu_len <= backing || obu_ptr >= backing + backing_len,
            "AV1 OBU payload range must NOT overlap the live input AU backing"
        );
        // Deep-copy proof: the arena copy carries the OBU body bytes.
        let obu_bytes = unsafe { core::slice::from_raw_parts(arena.obus[0].payload, obu_len) };
        assert_eq!(obu_bytes, &[0x0A, 0x0B, 0x0C]);
    }

    #[test]
    fn h264_video_sample_payload_is_raw_au() {
        // v0.2.0 Wave 5: video samples expose the raw encoded AU via
        // `payload`/`payload_len` (NULL for video before). Two-NAL Annex-B
        // AU (SPS + PPS, 4-byte start codes) so the zero-copy path is
        // exercised across multiple units (DA-PERF-11).
        let au = vec![
            0x00u8, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80, // PPS
        ];
        let shared = SharedBytes::from_vec(au.clone());
        let backing = shared.as_ptr() as usize;
        let backing_len = shared.len();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x500, StreamKind::Video(VideoCodec::H264)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::H264,
                raw: shared,
                random_access_indicator: true,
                av1_carriage: None,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let payload_ptr = unsafe { out.u.sample.payload };
        let payload_len = unsafe { out.u.sample.payload_len };
        assert!(
            !payload_ptr.is_null(),
            "video payload must carry the raw encoded AU since v0.2.0"
        );
        let raw_bytes = unsafe { core::slice::from_raw_parts(payload_ptr, payload_len) };
        assert_eq!(raw_bytes, &au[..], "payload must be the exact encoded AU");
        // DA-PERF-11: payload_ptr must point INTO the live SharedBytes
        // backing (zero-copy — no separate arena copy for H.26x).
        let p = payload_ptr as usize;
        assert!(
            p >= backing && p + payload_len <= backing + backing_len,
            "video payload must point into the retained SharedBytes backing"
        );
        // Each NAL slot must also be within the SharedBytes backing range
        // (they are subslices of the same Arc allocation).
        assert_eq!(arena.nals.len(), 2);
        for slot in &arena.nals {
            let sp = slot.payload as usize;
            assert!(
                sp >= backing && sp + slot.payload_len <= backing + backing_len,
                "NAL slot must point into the retained SharedBytes backing"
            );
        }
        let nal0 = unsafe {
            core::slice::from_raw_parts(arena.nals[0].payload, arena.nals[0].payload_len)
        };
        assert_eq!(nal0, &[0x42, 0x00, 0x1E], "SPS body (header 0x67 stripped)");
        let nal1 = unsafe {
            core::slice::from_raw_parts(arena.nals[1].payload, arena.nals[1].payload_len)
        };
        assert_eq!(nal1, &[0xCE, 0x38, 0x80], "PPS body (header 0x68 stripped)");
    }

    #[test]
    fn av1_video_sample_payload_is_raw_wire_bytes() {
        // AV1 `payload` = the on-wire PES payload exactly (raw-OBU
        // carriage here). OBU views into the AU → slots reuse the AU copy.
        let au = vec![0x0Au8, 0x03, 0x0A, 0x0B, 0x0C];
        let shared = SharedBytes::from_vec(au.clone());
        let backing = shared.as_ptr() as usize;
        let backing_len = shared.len();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x600, StreamKind::Video(VideoCodec::Av1)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::Av1,
                raw: shared,
                random_access_indicator: true,
                av1_carriage: None,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let payload_ptr = unsafe { out.u.sample.payload };
        let payload_len = unsafe { out.u.sample.payload_len };
        assert!(!payload_ptr.is_null());
        let raw_bytes = unsafe { core::slice::from_raw_parts(payload_ptr, payload_len) };
        assert_eq!(raw_bytes, &au[..], "AV1 payload must mirror the wire bytes");
        let p = payload_ptr as usize;
        assert!(
            p + payload_len <= backing || p >= backing + backing_len,
            "raw AU copy must not overlap the live input backing"
        );
        assert_eq!(arena.obus.len(), 1);
        let obu_bytes = unsafe {
            core::slice::from_raw_parts(arena.obus[0].payload, arena.obus[0].payload_len)
        };
        assert_eq!(obu_bytes, &[0x0A, 0x0B, 0x0C]);
    }

    #[test]
    fn av1_binding_mode_payload_is_wire_bytes_with_per_unit_fallback() {
        // AV1 *binding-mode* AU (§3.2 start-code framing): `payload` still
        // mirrors the wire bytes (transmux reproduces input — the
        // documented AV1 exception), while `split_video` unwraps into a
        // FRESH buffer, so OBU slots take the per-unit-copy fallback. The
        // contract assertions are content-based + live-range-based only
        // (which copy strategy was used is an internal detail).
        let au = vec![0x00u8, 0x00, 0x01, 0x0A, 0x03, 0x0A, 0x0B, 0x0C];
        let shared = SharedBytes::from_vec(au.clone());
        let backing = shared.as_ptr() as usize;
        let backing_len = shared.len();
        let ev = DemuxEvent::Sample {
            stream: stream_id(0x601, StreamKind::Video(VideoCodec::Av1)),
            pts: Pts90khz::new(0),
            dts: None,
            payload: SamplePayload::Video {
                codec: VideoCodec::Av1,
                raw: shared,
                random_access_indicator: false,
                av1_carriage: None,
            },
        };
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        let payload_ptr = unsafe { out.u.sample.payload };
        let payload_len = unsafe { out.u.sample.payload_len };
        let raw_bytes = unsafe { core::slice::from_raw_parts(payload_ptr, payload_len) };
        assert_eq!(
            raw_bytes,
            &au[..],
            "binding-mode AV1 payload must mirror the wire bytes (framing intact)"
        );
        let p = payload_ptr as usize;
        assert!(
            p + payload_len <= backing || p >= backing + backing_len,
            "raw AU copy must not overlap the live input backing"
        );
        assert_eq!(
            arena.obus.len(),
            1,
            "binding unwrap must still split the OBU"
        );
        let obu_ptr = arena.obus[0].payload as usize;
        let obu_len = arena.obus[0].payload_len;
        assert!(
            obu_ptr + obu_len <= backing || obu_ptr >= backing + backing_len,
            "fallback OBU copy must not overlap the live input backing"
        );
        let obu_bytes = unsafe { core::slice::from_raw_parts(arena.obus[0].payload, obu_len) };
        assert_eq!(obu_bytes, &[0x0A, 0x0B, 0x0C]);
    }

    #[test]
    fn reconnect_discontinuity_maps_to_event_kind_6() {
        // Sprint 4-5 review followup-1: the managed demux receiver now
        // wires `DemuxEvent::ReconnectDiscontinuity` through to a
        // dedicated `TstEventKind::ReconnectDiscontinuity` (= 6) with a
        // zeroed union body. Verify the mapping here so the C ABI
        // contract is locked at unit-test resolution; an end-to-end
        // reconnect-driven test is out of scope (would require either
        // SRT loopback or replumbing the C wrapper for a generic
        // transport).
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &DemuxEvent::ReconnectDiscontinuity, &mut out);
        assert_eq!(out.kind, TstEventKind::ReconnectDiscontinuity as c_int);
        // No body assertions — the variant intentionally carries no payload.
    }

    #[test]
    fn program_map_descriptor_data_is_arena_owned() {
        use tst_core::mpegts::demux::{ProgramMap, StreamInfo};
        use tst_core::mpegts::descriptors::RawDescriptor;

        let desc_data = vec![b'K', b'L', b'V', b'A'];
        let desc_data_ptr_before = desc_data.as_ptr();
        let pm = ProgramMap {
            program_number: 1,
            pcr_pid: 0x100,
            pmt_pid: 0x30,
            streams: vec![StreamInfo {
                pid: 0x100,
                stream_type: StreamTypeCode::Unknown(0x06),
                kind: StreamKind::KlvAsync,
                program_number: 1,
                raw_descriptors: vec![RawDescriptor {
                    tag: 0x05, // registration_descriptor
                    data: desc_data,
                }],
            }],
            klv_links: vec![],
        };
        let ev = DemuxEvent::ProgramMap(pm);
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        assert_eq!(arena.descriptors.len(), 1);
        let desc_out_ptr = arena.descriptors[0].data;
        assert_ne!(
            desc_out_ptr, desc_data_ptr_before,
            "ProgramMap descriptor data pointer must NOT alias the input Vec"
        );
    }

    #[test]
    fn program_map_pmt_pid_is_forwarded() {
        use tst_core::mpegts::demux::{ProgramMap, StreamInfo};

        // pmt_pid distinct from pcr_pid and the stream pid so a crossed
        // wire in fill_program_map can't pass by coincidence.
        let pm = ProgramMap {
            program_number: 1,
            pcr_pid: 0x100,
            pmt_pid: 0x30,
            streams: vec![StreamInfo {
                pid: 0x100,
                stream_type: StreamTypeCode::Unknown(0x06),
                kind: StreamKind::KlvAsync,
                program_number: 1,
                raw_descriptors: vec![],
            }],
            klv_links: vec![],
        };
        let ev = DemuxEvent::ProgramMap(pm);
        let mut arena = EventArena::new();
        let mut out = TstEvent::default();
        convert(&mut arena, &ev, &mut out);
        assert_eq!(out.kind, TstEventKind::ProgramMap as c_int);
        assert_eq!(
            unsafe { out.u.program_map.pmt_pid },
            0x30,
            "pmt_pid must be forwarded from the core ProgramMap to the C event"
        );
    }
}
