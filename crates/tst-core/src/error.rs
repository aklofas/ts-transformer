//! Non-transport error types: KLV, MPEG-TS mux/demux.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use alloc::string::String;
use thiserror::Error;

use crate::mpegts::mux::{StreamKind, TeletextField};

// ============================================================================
// KLV errors
// ============================================================================

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum KlvDecodeError {
    #[error("buffer truncated at offset {offset}: needed {needed} bytes, have {have}")]
    Truncated {
        offset: usize,
        needed: usize,
        have: usize,
    },

    #[error("malformed BER length at offset {offset}")]
    MalformedLength { offset: usize },

    #[error("BER length {value} exceeds maximum supported size")]
    LengthOverflow { value: u64 },

    #[error("malformed BER-OID tag at offset {offset}")]
    MalformedTag { offset: usize },

    /// Non-canonical BER long-form length encoding (per MISB ST 0107.5
    /// §6.3.2: encoders shall use the fewest bytes). Returned by
    /// `read_ber_strict`; the permissive `read_ber` accepts non-canonical
    /// for legacy capture interop.
    #[error("non-canonical BER length encoding at offset {offset}")]
    NonCanonicalLength { offset: usize },

    /// Non-canonical BER-OID encoding (per MISB ST 0107.5 §6.3.1:
    /// leading `0x80` byte forbidden). Returned by `read_ber_oid_strict`;
    /// the permissive `read_ber_oid` accepts non-canonical for legacy.
    #[error("non-canonical BER-OID tag at offset {offset}")]
    NonCanonicalTag { offset: usize },

    #[error("unexpected universal label: expected {expected}, got {found}")]
    UnexpectedUniversalLabel {
        expected: crate::klv::UniversalLabel,
        found: crate::klv::UniversalLabel,
    },

    #[error("checksum mismatch: declared {expected:#06x}, computed {found:#06x}")]
    ChecksumMismatch { expected: u16, found: u16 },

    /// ST 0806.4 RVT Local Set standalone-form checksum (Tag 1) — CRC-32
    /// with the MPEG-2 (ISO/IEC 13818-1) parameters, distinct from the
    /// ST 0601 16-bit running-sum above. See `klv::st0806::decode_standalone`.
    #[error("CRC-32 mismatch: declared {expected:#010x}, computed {found:#010x}")]
    Crc32Mismatch { expected: u32, found: u32 },

    #[error("duplicate tag {tag} at offset {offset}")]
    DuplicateTag { tag: u32, offset: usize },

    #[error("trailing bytes after declared length: {len} extra")]
    TrailingBytes { len: usize },

    #[error("Precision Time Stamp Pack body must be 9 bytes, got {got}")]
    BadTimeStampPackLength { got: usize },

    /// Not produced by `klv::st0605::decode` (which is permissive about
    /// reserved bits per its doc); call `time_status.reserved_bits_valid()`
    /// on the decoded pack and raise this if a stricter caller wants it.
    #[error("Time Status reserved bits 4-0 must be 0b11111, got {got:#04x}")]
    ReservedBitsInvalid { got: u8 },

    #[error("Tag 2 (timestamp) must be the first element per ST 0601.8-09")]
    Tag2NotFirst,

    #[error("Tag 1 (checksum) must be the last element per ST 0601.8-11")]
    Tag1NotLast,

    #[error("Tag 65 (UAS LS Version) is required per ST 0601.8-12")]
    MissingTag65,

    /// Strict-mode `klv::st0102::decode_strict` only: spec-mandatory tag
    /// (1, 2, 3, 12, 13, or 22) absent from the record per ST 0102.12
    /// §6.7 Table 2.
    #[error("ST 0102 record missing required tag {tag} per ST 0102.12 §6.7")]
    St0102MissingRequiredTag { tag: u8 },

    /// Strict-mode `klv::st0903::decode_strict` only: spec-mandatory tag
    /// absent from the record per ST 0903.6 §6 Table 1.
    #[error("ST 0903 record missing required tag {tag} per ST 0903.6 §6")]
    St0903MissingRequiredTag { tag: u8 },

    /// Pack-internal malformation surfaced from VTargetSeries (Tag 101)
    /// decode. `offset` is the byte offset within the VTargetSeries
    /// payload (not the outer LS).
    #[error("ST 0903 VTargetPack at offset {offset}: {reason}")]
    St0903InvalidVTargetPack {
        offset: usize,
        reason: crate::klv::st0903::vtarget_pack::VTargetPackError,
    },

    /// A field-level validation error promoted to a fatal decode error.
    /// `klv::st0102::decode` raises this for InvalidLength on tag 1/2/12/22
    /// and InvalidUtf8 on ASCII string tags even in lenient mode (the
    /// graceful-fallback path is reserved for Tag 13 UTF-16 only —
    /// see the module-level rationale). `decode_strict` raises it for
    /// all field validation failures including unknown enum codepoints
    /// and Tag 13 UTF-16 decode failures.
    #[error("field validation failed: {0}")]
    FieldError(#[from] KlvFieldError),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum KlvEncodeError {
    #[error("output buffer too small: needed {needed} bytes, got {got}")]
    BufferTooSmall { needed: usize, got: usize },

    #[error("record exceeds maximum BER-encodable length")]
    RecordTooLarge,

    #[error("value out of range for tag {tag}: {value} not in [{min}, {max}]{}", .hint.map(|h| alloc::format!("; {h}")).unwrap_or_default())]
    OutOfRange {
        tag: u32,
        value: f64,
        min: f64,
        max: f64,
        hint: Option<&'static str>,
    },

    #[error("string field for tag {tag} exceeds {max} bytes")]
    StringTooLong { tag: u32, max: usize },

    /// `ImapbParams::length` outside the supported range `1..=8`.
    /// ST 1201.5 §6 defines IMAPB for any L-byte mapping; this
    /// implementation uses `u64` arithmetic internally, which holds up
    /// to 8 bytes. L=0 is degenerate; L > 8 would need `u128`. In-tree
    /// consumers use L ∈ {1,2,3,4,5,6}; if a future consumer needs
    /// L > 8, swap the internal arithmetic to `u128`.
    #[error("IMAPB length {length} not supported (must be 1..=8)")]
    UnsupportedImapbLength { length: usize },

    /// `ImapbParams` violates the ST 1201.5 §6 `min < max` precondition.
    /// Surfaced by [`crate::klv::imapb::encode_imapb`] (and any sibling
    /// that derives a scale factor via `sf()`) before any wire bytes are
    /// written — the §8.9 `bPow = ceil(log2(max − min))` derivation is
    /// undefined when `max <= min` (`log2(0) = −∞`, `log2` of a negative
    /// is NaN). The `length` field is included for diagnostic
    /// completeness; pure-L failures (length outside `1..=8`) keep
    /// surfacing as the narrower `UnsupportedImapbLength` so existing
    /// diagnostics don't regress.
    #[error(
        "IMAPB params violate ST 1201.5 §6 preconditions: min={min}, max={max}, length={length} (require min < max and length in 1..=8)"
    )]
    InvalidImapbParams { min: f64, max: f64, length: u8 },

    /// A mandatory KLV item is missing from a record passed to a
    /// strict-compliance encoder. `tag` is the numeric item code and
    /// `name` is the human-readable item label for diagnostics.
    ///
    /// Used by `st0601::encode_strict_compliance` (Tag 2 Precision
    /// Time Stamp), `st0102::encode_strict_compliance`, and
    /// `st0903` strict encoders. Tags that are auto-emitted by the
    /// encoder (e.g. Checksum, LS Version Number) are never flagged here.
    #[error("KLV mandatory item missing: tag {tag} ({name})")]
    MissingMandatoryItem { tag: u16, name: &'static str },

    /// A VTarget Pack in an ST 0903 vTargetSeries has no TLV items
    /// after the targetId (ST 0903.4-10 requires at least one additional
    /// item). `target_id` is the offending pack's identifier.
    #[error(
        "ST 0903 VTarget Pack {target_id} has no TLV items after targetId (ST 0903.4-10 requires at least one)"
    )]
    VTargetPackEmpty { target_id: u64 },

    /// An ST 0903 vTargetSeries contains more than one VTarget Pack
    /// with the same targetId (ST 0903.6-126 requires unique targetIds).
    #[error(
        "ST 0903 vTargetSeries contains duplicate targetId {target_id} (ST 0903.6-126 requires unique targetIds)"
    )]
    DuplicateTargetId { target_id: u64 },

    /// A standalone ST 0903 VMTI Local Set contains a VTarget Pack
    /// with a parent-relative offset tag, which is forbidden in the
    /// standalone form (ST 0903.6-116). `tag` is the offending tag number.
    #[error(
        "ST 0903 standalone VMTI VTarget Pack must omit parent-relative offset tag {tag} (ST 0903.6-116)"
    )]
    ForbiddenStandaloneOffset { tag: u32 },

    /// Caller placed a reserved or typed tag in `UasDatalinkLs.unknown`.
    ///
    /// `unknown` is for forward-compat pass-through of tags the encoder
    /// does not model — emitting a typed tag or a reserved structural
    /// tag from `unknown` would produce a non-conformant Local Set. Per
    /// MISB ST 0601.13 §6 (and ST 0601.24 §6) the reserved structural
    /// tags are Tag 1 (Checksum, always last and computed by the
    /// encoder), Tag 2 (Precision Time Stamp, encoded from
    /// `timestamp_us`), and Tag 65 (UAS LS Version Number, encoded from
    /// `uas_ls_version` / auto-emitted). Tags listed in the encoder's
    /// typed table (`tags::TAGS`) would produce duplicate entries.
    ///
    /// Surfaced by [`crate::klv::st0601::encode`] before any bytes are
    /// written; remove the offending entry from `unknown` or set the
    /// corresponding typed field.
    #[error("tag {tag} is reserved or modeled by the typed encoder and cannot appear in `unknown`")]
    ReservedTagInUnknown { tag: u32 },
}

/// Error from [`crate::klv::st0805`]: a KLV tag required by MISB ST 0805.1's
/// field mapping for the requested CoT event is missing from the input
/// record.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CotError {
    /// `tag` is the ST 0601 tag number; `name` is its human-readable item
    /// label for diagnostics. See `klv::st0805` for which tags are
    /// mandatory per event type.
    #[error("KLV tag {tag} ({name}) required for CoT conversion is missing")]
    MissingField { tag: u32, name: &'static str },
}

/// Error from [`crate::klv::st0601::patch`]: either the input local set
/// is malformed (decode side) or an edited value cannot be encoded
/// (encode side).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum KlvPatchError {
    /// The input local set could not be walked (truncated, malformed
    /// tag/length, ...).
    #[error("malformed input local set: {0}")]
    Decode(#[from] KlvDecodeError),
    /// An edited field failed to encode (out of range, string too
    /// long, reserved tag in `unknown`, ...).
    #[error("failed to encode edited tag: {0}")]
    Encode(#[from] KlvEncodeError),
}

#[derive(Debug, Clone, Error, PartialEq)]
#[non_exhaustive]
pub enum KlvFieldError {
    #[error("tag {tag}: value {value} out of declared range [{min}, {max}]")]
    OutOfRange {
        tag: u32,
        value: f64,
        min: f64,
        max: f64,
    },

    #[error("tag {tag}: invalid UTF-8 in string field")]
    InvalidUtf8 { tag: u32 },

    #[error("tag {tag}: expected {expected} value bytes, got {got}")]
    InvalidLength {
        tag: u32,
        expected: usize,
        got: usize,
    },

    /// Tag value declared as RFC 2781 UTF-16 contains malformed code
    /// units (lone surrogate, odd-length buffer). Reusable for any
    /// future UTF-16 / UCS-2 fields beyond ST 0102 Tag 13.
    #[error("tag {tag}: invalid UTF-16 in string field")]
    InvalidUtf16 { tag: u32 },

    /// Tag value's first byte is a typed-enum codepoint outside the
    /// spec's enumerated range. Strict-mode only; lenient decode
    /// surfaces an `Unknown(u8)` enum arm instead.
    #[error("tag {tag}: codepoint {value:#04x} not in spec-defined range")]
    InvalidCodepoint { tag: u32, value: u8 },

    /// Tag value's BER-declared length exceeded the available buffer,
    /// or its inner codec ran out of bytes. Surfaced by the lenient
    /// `klv::st0903::decode` walker when an LS body is truncated
    /// mid-field — the walker stops at the boundary, records this
    /// error, and returns the partially-populated record.
    #[error("tag {tag}: truncated value bytes")]
    TruncatedField { tag: u32 },

    /// `ImapbParams::length` outside the supported range `1..=8`.
    /// See `KlvEncodeError::UnsupportedImapbLength` for the rationale —
    /// the substrate caps at L=8 because its `u64` internal arithmetic
    /// holds at most 8 bytes.
    #[error("IMAPB length {length} not supported (must be 1..=8)")]
    UnsupportedImapbLength { length: usize },

    /// `ImapbParams` violates the ST 1201.5 §6 `min < max` precondition.
    /// Surfaced by [`crate::klv::imapb::decode_imapb`] after the
    /// existing `UnsupportedImapbLength` check (which keeps its narrow
    /// diagnostic for pure-L failures) but before the wire bytes are
    /// interpreted — the §8.9 `bPow = ceil(log2(max − min))` derivation
    /// is undefined when `max <= min`. The `length` field is included
    /// for diagnostic completeness.
    #[error(
        "IMAPB params violate ST 1201.5 §6 preconditions: min={min}, max={max}, length={length} (require min < max and length in 1..=8)"
    )]
    InvalidImapbParams { min: f64, max: f64, length: u8 },
}

// ============================================================================
// MPEG-TS mux errors
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MuxError {
    #[error("muxer configuration is invalid: {0}")]
    InvalidConfig(&'static str),

    /// Like [`MuxError::InvalidConfig`] but carries a `String` reason
    /// so the diagnostic can format-print structural context
    /// (program numbers, actual vs expected lengths). Use this variant
    /// when the reason needs runtime context beyond a static string.
    ///
    /// Introduced in plan #72 (Wave 2.3) for the
    /// `MuxerProgramConfig.stream_descriptors` length invariant; future
    /// `validate()` checks that need formatted reasons should also
    /// route through this variant.
    #[error("muxer configuration is invalid: {reason}")]
    ConfigInvalid { reason: String },

    #[error("video input is not Annex-B framed (no start code prefix)")]
    InvalidNal,

    /// AV1 OBU input could not be framed for binding-mode carriage — the
    /// bytes are not a well-formed elementary OBU stream (e.g. already-
    /// carried on-wire bytes from a demuxer, or a truncated/malformed OBU
    /// sequence). Use [`Muxer::push_video_wire_to`](crate::mpegts::mux::Muxer::push_video_wire_to)
    /// for already-carried wire bytes; pass raw elementary OBUs to
    /// `push_video_to`.
    #[error(
        "AV1 OBU input is not a well-formed elementary OBU stream; use push_video_wire_to for already-carried wire bytes"
    )]
    InvalidAv1Obu,

    /// A MISP-timestamp push (`push_video_misp_to*`) could not build or
    /// splice the ST 0604 SEI: nano-precision on H.264, an AV1/H.266
    /// stream, or an access unit with no VCL NAL.
    #[error("MISP timestamp SEI: {0}")]
    MispTime(#[from] crate::codec::misp_time::MispTimeError),

    #[error("muxer packet buffer is full ({capacity_packets} packets); drain via pull and retry")]
    BufferFull { capacity_packets: u64 },

    /// KLV blob exceeds the 16-bit `PES_packet_length` ceiling.
    ///
    /// PES_packet_length is at most 65535 and must cover flags1, flags2,
    /// header_data_length, the PTS field (if present), and the ES payload —
    /// so the KLV payload itself is bounded to 65532 bytes (no PTS) or
    /// 65527 bytes (with PTS). MISB ST 0601 packs are typically <2 KB so
    /// this is a sanity check, not a regular failure mode.
    #[error("KLV blob is {size} bytes, exceeds PES_packet_length ceiling of {max} bytes")]
    KlvTooLarge { size: usize, max: usize },

    /// Audio frames exceed the 16-bit `PES_packet_length` ceiling.
    ///
    /// PES_packet_length is at most 65535 and must cover flags1, flags2,
    /// header_data_length, the PTS field (always present for audio), and
    /// the ES payload — so the audio frames themselves are bounded to 65527 bytes.
    /// In practice audio frames are far smaller than this limit (KB scale), but
    /// the guard prevents silent wraparound on pathologically large inputs.
    #[error("audio frames too large: {size} bytes, max {max}")]
    AudioTooLarge { size: usize, max: usize },

    /// Caller passed a `VideoStreamHandle` / `KlvStreamHandle` that doesn't
    /// match a configured stream on this `Muxer`. Handles are obtained from
    /// `Muxer::video_handles()` / `klv_handles()` and are tied to the
    /// muxer that produced them — passing one from a different muxer is
    /// also rejected here.
    #[error("invalid {kind} stream handle (index {index}) — not a configured stream")]
    InvalidStreamHandle { kind: StreamKind, index: usize },

    /// Caller invoked the no-suffix `push_video` / `push_klv` (or the
    /// `MuxSender::send_video` / `send_klv` wrappers) on a muxer that has more
    /// than one stream of that kind. The single-target API can only resolve
    /// to a single handle when exactly one stream of that kind is configured.
    #[error(
        "ambiguous push: {count} {kind} streams configured — call push_{kind}_to(handle, ...) instead"
    )]
    AmbiguousTarget { kind: StreamKind, count: usize },

    /// `Muxer::push_klv` shorthand called when no KLV streams configured.
    /// Use `push_klv_to(handle, ...)` with a KLV handle from `klv_handles()`,
    /// or add a KLV stream to the config.
    #[error("no KLV streams configured; use push_klv_to with a handle from klv_handles()")]
    NoKlvStreamsConfigured,

    /// `Muxer::push_audio` shorthand called when no audio streams configured.
    /// Use `push_audio_to(handle, ...)` with an audio handle from `audio_handles()`,
    /// or add an audio stream to the config.
    #[error("no audio streams configured; use push_audio_to with a handle from audio_handles()")]
    NoAudioStreamsConfigured,

    /// `Muxer::push_subtitle` shorthand called when no subtitle streams configured.
    /// Use `push_subtitle_to(handle, ...)` with a subtitle handle from `subtitle_handles()`,
    /// or add a subtitle stream to the config.
    #[error(
        "no subtitle streams configured; use push_subtitle_to with a handle from subtitle_handles()"
    )]
    NoSubtitleStreamsConfigured,

    /// `MuxerConfig::validate` rejects more than 16 video streams.
    /// Trivially lifted if a consumer asks; 16 is well above realistic
    /// gimbaled-platform topologies (EO + IR + maybe IR-narrow + a depth
    /// channel = 4 in the wild today).
    #[error("too many video streams: {count} configured, cap is {cap}")]
    TooManyVideoStreams { count: usize, cap: usize },

    /// `MuxerConfig::validate` rejects more than 16 KLV streams.
    #[error("too many klv streams: {count} configured, cap is {cap}")]
    TooManyKlvStreams { count: usize, cap: usize },

    /// `MuxerConfig::validate` rejects more than 16 audio streams in any program.
    #[error("too many audio streams: {count} configured, cap is {cap}")]
    TooManyAudioStreams { count: usize, cap: usize },

    /// `MuxerConfig::validate` rejects more than 16 subtitle streams in any program.
    #[error("too many subtitle streams: {count} configured, cap is {cap}")]
    TooManySubtitleStreams { count: usize, cap: usize },

    /// `push_subtitle` payload exceeds the PES packet length budget. PES
    /// packet length is at most 65535 and must cover flags + PTS field +
    /// payload, bounding subtitle payloads to 65527 bytes.
    #[error("subtitle PES payload too large: {size} bytes (max {max})")]
    SubtitleTooLarge { size: usize, max: usize },

    /// Caller pinned a subtitle PID as the PCR PID. Subtitles are sparse
    /// and event-driven; using one for PCR pacing produces poor PCR
    /// spacing. Move PCR to a video / audio / KLV PID.
    #[error(
        "subtitle PID 0x{pid:04X} cannot be used as the PCR PID; subtitles are too sparse for PCR pacing"
    )]
    SubtitlePidUsedAsPcrPid { pid: u16 },

    /// PCR-PID resolved (caller-pinned or via fallback chain) to a KLV stream
    /// PID. KLV pushes are typically sparse (1-10 Hz from sensors) and would
    /// produce PCR at the same cadence, failing ETSI TR 101 290 §5.6.1's
    /// 100 ms ceiling. The right fix is to add a video stream to the program
    /// (PCR follows video naturally) or to caller-pin `pcr_pid` to a stream
    /// that pushes at ≥10 Hz. Today's deterministic-output muxer cannot emit
    /// standalone PCR-only TS packets between push events.
    #[error(
        "PCR PID 0x{pid:04X} resolves to a KLV stream — KLV push cadence is too sparse for PCR (ETSI TR 101 290 §5.6.1 requires ≤100 ms between PCRs); add a video stream or pin pcr_pid to a faster-cadence stream"
    )]
    KlvPidUsedAsPcrPid { pid: u16 },

    /// `MuxerConfig::validate` rejects ISO 639-2 language codes that aren't
    /// 3 lowercase ASCII bytes.
    #[error("invalid ISO 639-2 language code: {code:02x?} (must be 3 lowercase ASCII bytes)")]
    InvalidLanguageCode { code: [u8; 3] },

    /// `MuxerConfig::validate` rejects DVB teletext field values that exceed
    /// their bit-width budget.
    #[error("invalid DVB teletext {field}: {value} (max {max})")]
    InvalidTeletextField {
        field: TeletextField,
        value: u8,
        max: u8,
    },

    /// `MuxerConfig::validate` rejected a configuration whose total PMT
    /// section length wouldn't fit in a single TS packet. `used_bytes`
    /// is the estimated full PMT section size (header + program-level
    /// descriptors + per-stream entries with their auto-emit + caller-
    /// supplied descriptor bytes + CRC); `max_bytes` is the single-TS-
    /// packet payload cap (`MAX_PMT_SECTION_BYTES = 183`). Multi-section
    /// PMT support is out of scope; if you hit this, drop one or more
    /// user-supplied descriptors or shorten their payloads.
    #[error(
        "PMT too large: {used_bytes} bytes used, {max_bytes} max (single-section PMT must fit in one TS packet)"
    )]
    PmtTooLarge { used_bytes: usize, max_bytes: usize },

    /// Caller-supplied descriptor TLV bytes are not well-formed.
    /// Length byte must equal `data.len() - 2`.
    #[error(
        "malformed descriptor for stream {stream_index} descriptor {descriptor_index}: {reason}"
    )]
    MalformedDescriptor {
        stream_index: usize,
        descriptor_index: usize,
        reason: &'static str,
    },

    /// Configured `programs.len()` exceeded `MAX_PROGRAMS`.
    #[error("too many programs: {count} configured, cap is {cap}")]
    TooManyPrograms { count: usize, cap: usize },

    /// A program in `MuxerConfig::programs` had zero streams. Programs must
    /// carry at least one elementary stream.
    #[error("program {program_number} has no streams configured")]
    EmptyProgram { program_number: u16 },

    /// Two programs in `MuxerConfig::programs` share the same `program_number`.
    #[error("duplicate program_number {program_number} across programs")]
    DuplicateProgramNumber { program_number: u16 },

    /// Two programs in `MuxerConfig::programs` share the same `pmt_pid`.
    #[error("pmt_pid 0x{pid:04X} reused by programs {programs:?}")]
    DuplicatePmtPid { pid: u16, programs: [u16; 2] },

    /// A stream PID appears in two different programs. PID uniqueness across
    /// programs is required (repacking workflows control renumbering).
    #[error("stream PID 0x{pid:04X} used by programs {programs:?}")]
    DuplicatePidAcrossPrograms { pid: u16, programs: [u16; 2] },

    /// Caller referenced a program_number that doesn't exist in `MuxerConfig::programs`.
    #[error("program {program_number} not found")]
    ProgramNotFound { program_number: u16 },

    /// A program's `pmt_pid` collides with one of its own (or another program's)
    /// stream PIDs.
    #[error("pmt_pid 0x{pmt_pid:04X} of program {program_number} conflicts with a stream PID")]
    PmtPidConflictsWithStream { pmt_pid: u16, program_number: u16 },

    /// `MuxerConfig::validate` rejected a program that contains no
    /// PCR-eligible stream. Only video and audio streams may carry PCR:
    /// KLV cadence is too sparse for ETSI TR 101 290 §5.6.1's 100 ms
    /// ceiling, subtitles must NOT carry PCR per ETSI EN 300 472 §4.0 +
    /// EN 300 743 §6.1, and data streams have no cadence guarantee. Add
    /// at least one video or audio stream to the program.
    #[error(
        "program {program_number} has no PCR-eligible stream — needs at least one video or audio stream (KLV, data, and subtitle streams cannot carry PCR)"
    )]
    NoPcrEligibleStream { program_number: u16 },

    /// `Muxer::push_data` shorthand called when no data streams configured.
    /// Use `push_data_to(handle, ...)` with a data handle from `data_handles()`,
    /// or add a data stream to the config.
    #[error("no data streams configured; use push_data_to with a handle from data_handles()")]
    NoDataStreamsConfigured,

    /// `MuxerConfig::validate` rejects more than 16 data streams in any program.
    #[error("too many data streams: {count} configured, cap is {cap}")]
    TooManyDataStreams { count: usize, cap: usize },

    /// `push_data` payload exceeds the PES packet length budget.
    ///
    /// PES_packet_length is at most 65535 and must cover flags1, flags2,
    /// header_data_length, the PTS field (present when `carries_pts` is
    /// set), and the ES payload — so the data payload itself is bounded
    /// to 65532 bytes (no PTS) or 65527 bytes (with PTS).
    #[error("data payload is {size} bytes, exceeds PES_packet_length ceiling of {max} bytes")]
    DataTooLarge { size: usize, max: usize },

    /// Caller pinned a data-stream PID as the PCR PID. Data pushes are
    /// caller-paced with no cadence guarantee, so a data PID cannot
    /// promise ETSI TR 101 290 §5.6.1's 100 ms PCR ceiling.
    #[error(
        "pcr_pid 0x{pid:04X} is a data stream; data cadence is not \
         guaranteed to meet the 100 ms PCR ceiling — pin PCR to a video \
         or audio PID"
    )]
    DataPidUsedAsPcrPid { pid: u16 },

    /// A kind-relative descriptor index (as passed to
    /// `stream_descriptors_for_video` / `_for_klv` / `_for_audio` /
    /// `_for_subtitle`) is out of range for the corresponding stream list
    /// in the given program. Ensure the stream was added via `add_{kind}`
    /// before calling `stream_descriptors_for_{kind}`.
    #[error(
        "descriptor index {index} out of range for {kind} streams in program {program_number} \
         (call after the corresponding add_{kind})"
    )]
    DescriptorIndexOutOfRange {
        kind: StreamKind,
        index: u32,
        program_number: u16,
    },

    /// An absolute stream index (as passed to `stream_descriptors_for_stream`)
    /// is out of range for the given program's total stream count. `len` is
    /// the number of streams currently configured on the program.
    #[error("abs_idx {abs_idx} out of range for program {program_number} (has {len} streams)")]
    AbsIndexOutOfRange {
        abs_idx: u32,
        len: u32,
        program_number: u16,
    },
}

/// Categorical reason for a [`MuxError`], suitable for action-discriminating
/// dispatch in binding-author code.
///
/// The 5-variant set is the **inner-tier** coarse classification of muxer
/// failures: a JNI/UniFFI/pure-C binding author pattern-matches on this enum
/// to map muxer failures into a language-native exception hierarchy without
/// enumerating the 36 underlying [`MuxError`] variants. For spec-aware
/// diagnostic code (KLV-handling, DVB-subtitling, descriptor validation),
/// match on the full [`MuxError`] variant set directly via the
/// [`crate::mpegts::mux::_detail::MuxError`] re-export.
///
/// This is distinct from the **outer-tier** `tst_pipeline::ShellErrorKind`
/// (6 variants, shell-agnostic). The two tiers complement each other:
/// `ShellErrorKind` is the binding-canonical action category at the shell
/// boundary (`MuxSender`, `Sender`, `RawSender`, `DemuxReceiver`,
/// `Receiver`, `RawReceiver`); `MuxErrorKind` is the muxer-specific
/// inner category exposing the runtime-API-misuse-vs-construction-rejection
/// distinction that `ShellErrorKind::ConfigInvalid` collapses.
///
/// **Stability:** the variant set is `#[non_exhaustive]` and may grow.
/// New variants will be added without a major version bump; bindings that
/// pattern-match on this enum should include a wildcard arm routing to a
/// generic "muxer-side failure" exception.
///
/// # Example
///
/// ```
/// use tst_core::error::{MuxError, MuxErrorKind};
///
/// let err = MuxError::InvalidNal;
/// match err.kind() {
///     MuxErrorKind::InputMalformed => {
///         eprintln!("caller pushed malformed input: {err}");
///     }
///     MuxErrorKind::ConfigInvalid => {
///         eprintln!("muxer config is invalid: {err}");
///     }
///     MuxErrorKind::InvalidUsage => {
///         eprintln!("caller is using the muxer API incorrectly: {err}");
///     }
///     MuxErrorKind::Backpressure => {
///         eprintln!("muxer queue full; back off and retry");
///     }
///     MuxErrorKind::Internal => {
///         eprintln!("muxer hit a bug-path invariant: {err}");
///     }
///     _ => {
///         eprintln!("muxer-side failure (new category): {err}");
///     }
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MuxErrorKind {
    /// Caller pushed input bytes that don't conform to the expected
    /// shape. Includes non-Annex-B NAL units
    /// ([`MuxError::InvalidNal`]), non-well-formed AV1 OBU streams
    /// ([`MuxError::InvalidAv1Obu`]), MISP SEI build/splice failures
    /// ([`MuxError::MispTime`]), KLV blobs over the
    /// `PES_packet_length` ceiling ([`MuxError::KlvTooLarge`]), and
    /// audio / subtitle / data PES payloads over the PES cap
    /// ([`MuxError::AudioTooLarge`], [`MuxError::SubtitleTooLarge`],
    /// [`MuxError::DataTooLarge`]).
    ///
    /// **Action:** the push input was invalid; the muxer state is
    /// unchanged. Surface a "bad input" diagnostic to the caller and
    /// do not retry with the same input.
    InputMalformed,

    /// `MuxerConfig::validate()` rejected the construction-time config
    /// (duplicate PIDs, too many streams, malformed descriptor TLV,
    /// PCR-PID conflicts, ISO 639 / DVB teletext field violations,
    /// PMT over-budget, etc.). The muxer was not constructed; no
    /// pushes have happened.
    ///
    /// **Action:** fix the config and retry construction. The 21
    /// `MuxError` variants in this category collectively cover every
    /// `MuxerConfig::validate()` rejection path.
    ConfigInvalid,

    /// Caller is using the muxer API incorrectly on a successfully-built
    /// muxer. Includes passing a stream handle from a different muxer
    /// ([`MuxError::InvalidStreamHandle`]), invoking the single-target
    /// shorthand on a multi-stream muxer
    /// ([`MuxError::AmbiguousTarget`]), invoking shorthand with no
    /// streams of that kind ([`MuxError::NoKlvStreamsConfigured`] etc.),
    /// referencing an unknown program ([`MuxError::ProgramNotFound`]),
    /// or passing an out-of-range descriptor / absolute stream index
    /// ([`MuxError::DescriptorIndexOutOfRange`],
    /// [`MuxError::AbsIndexOutOfRange`]).
    ///
    /// **Action:** the muxer state is unchanged. Fix the API call site
    /// and retry; the muxer remains usable. Distinct from
    /// `ConfigInvalid` (which requires reconstructing the muxer from a
    /// new config).
    InvalidUsage,

    /// Muxer outbound queue is at capacity ([`MuxError::BufferFull`]).
    /// The push input is valid and the muxer state is consistent; the
    /// downstream transport has not yet drained the queued packets.
    ///
    /// **Action:** pause pushes, drain the muxer via `pull` (or wait
    /// for the bundled `tst_pipeline::MuxSender` to send queued
    /// packets), then retry the same push.
    Backpressure,

    /// Reserved for BUG-path variants (Mutex poison surfaces,
    /// arithmetic-overflow guards firing, internal-invariant
    /// violations). No current [`MuxError`] variant maps here; future
    /// internal-failure variants would route here rather than expanding
    /// the user-facing categories.
    ///
    /// **Action:** treat as an unrecoverable internal error. The muxer
    /// is in an indeterminate state; reconstruct from a fresh config.
    Internal,
}

impl MuxError {
    /// Categorize this error for binding-author pattern matching.
    ///
    /// Returns the coarse-tier [`MuxErrorKind`] (5 stable
    /// categories) corresponding to this variant. Use this when
    /// writing a generic binding that maps muxer failures to a
    /// language-native exception hierarchy without enumerating the
    /// full inner variant set.
    ///
    /// For spec-aware diagnostic code (KLV-handling, DVB-subtitling,
    /// descriptor validation), match on the full [`MuxError`] variant
    /// set directly via the [`crate::mpegts::mux::_detail::MuxError`]
    /// re-export (the same enum; the re-export signals intent at the
    /// import site).
    ///
    /// Per-variant routing is enforced by the compiler: the match below
    /// lives in [`MuxError`]'s own crate and carries no wildcard, so a new
    /// variant without an arm here is a build error (Arc 2 R2 retired the
    /// awk ratchet that used to check this).
    ///
    /// See [`MuxErrorKind`] for the per-variant rationale.
    ///
    /// # Example
    ///
    /// ```
    /// use tst_core::error::{MuxError, MuxErrorKind};
    ///
    /// let err = MuxError::InvalidNal;
    /// assert_eq!(err.kind(), MuxErrorKind::InputMalformed);
    /// ```
    #[must_use]
    pub fn kind(&self) -> MuxErrorKind {
        use MuxErrorKind::*;
        // No wildcard on purpose: this match is inside the defining crate,
        // so the compiler enforces exhaustiveness (Arc 2 R2 replaced the
        // awk rail that used to check this).
        match self {
            // === InputMalformed (7 variants) ===
            // Caller pushed bytes that don't conform to the expected shape.
            MuxError::InvalidNal => InputMalformed,
            MuxError::InvalidAv1Obu => InputMalformed,
            MuxError::MispTime(_) => InputMalformed,
            MuxError::KlvTooLarge { .. } => InputMalformed,
            MuxError::AudioTooLarge { .. } => InputMalformed,
            MuxError::SubtitleTooLarge { .. } => InputMalformed,
            MuxError::DataTooLarge { .. } => InputMalformed,

            // === Backpressure (1 variant) ===
            // Muxer queue at capacity; retry after drain.
            MuxError::BufferFull { .. } => Backpressure,

            // === ConfigInvalid (21 variants) ===
            // MuxerConfig::validate() rejected the construction-time config.
            MuxError::InvalidConfig(_) => ConfigInvalid,
            MuxError::ConfigInvalid { .. } => ConfigInvalid,
            MuxError::InvalidLanguageCode { .. } => ConfigInvalid,
            MuxError::InvalidTeletextField { .. } => ConfigInvalid,
            MuxError::TooManyVideoStreams { .. } => ConfigInvalid,
            MuxError::TooManyKlvStreams { .. } => ConfigInvalid,
            MuxError::TooManyAudioStreams { .. } => ConfigInvalid,
            MuxError::TooManySubtitleStreams { .. } => ConfigInvalid,
            MuxError::TooManyDataStreams { .. } => ConfigInvalid,
            MuxError::TooManyPrograms { .. } => ConfigInvalid,
            MuxError::EmptyProgram { .. } => ConfigInvalid,
            MuxError::DuplicateProgramNumber { .. } => ConfigInvalid,
            MuxError::DuplicatePmtPid { .. } => ConfigInvalid,
            MuxError::DuplicatePidAcrossPrograms { .. } => ConfigInvalid,
            MuxError::PmtPidConflictsWithStream { .. } => ConfigInvalid,
            MuxError::SubtitlePidUsedAsPcrPid { .. } => ConfigInvalid,
            MuxError::KlvPidUsedAsPcrPid { .. } => ConfigInvalid,
            MuxError::DataPidUsedAsPcrPid { .. } => ConfigInvalid,
            MuxError::NoPcrEligibleStream { .. } => ConfigInvalid,
            MuxError::MalformedDescriptor { .. } => ConfigInvalid,
            MuxError::PmtTooLarge { .. } => ConfigInvalid,

            // === InvalidUsage (9 variants) ===
            // Caller is using the muxer API incorrectly on a working muxer.
            MuxError::InvalidStreamHandle { .. } => InvalidUsage,
            MuxError::AmbiguousTarget { .. } => InvalidUsage,
            MuxError::NoKlvStreamsConfigured => InvalidUsage,
            MuxError::NoAudioStreamsConfigured => InvalidUsage,
            MuxError::NoSubtitleStreamsConfigured => InvalidUsage,
            MuxError::NoDataStreamsConfigured => InvalidUsage,
            MuxError::ProgramNotFound { .. } => InvalidUsage,
            MuxError::DescriptorIndexOutOfRange { .. } => InvalidUsage,
            MuxError::AbsIndexOutOfRange { .. } => InvalidUsage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_index_out_of_range_displays() {
        let err = MuxError::DescriptorIndexOutOfRange {
            kind: StreamKind::Video,
            index: 5,
            program_number: 1,
        };
        let s = err.to_string();
        assert!(s.contains("video"));
        assert!(s.contains("5"));
        assert!(s.contains("program 1"));
    }

    #[test]
    fn abs_index_out_of_range_displays() {
        let err = MuxError::AbsIndexOutOfRange {
            abs_idx: 99,
            len: 3,
            program_number: 1,
        };
        let s = err.to_string();
        assert!(s.contains("99"));
        assert!(s.contains("3 streams"));
        assert!(s.contains("program 1"));
    }
}

// ============================================================================
// MPEG-TS demux errors
// ============================================================================

/// Errors emitted by `mpegts::demux`.
///
/// Lenient-mode demuxing typically does NOT return errors — non-conformance
/// surfaces as `DemuxEvent::NonConformant { issue }` so the receive loop
/// keeps running. The error variants below fire when something is genuinely
/// fatal (the byte stream is unrecoverable, or strict mode converts a
/// `NonConformantIssue` into a hard failure).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DemuxError {
    /// One sync-search window held no packet boundary: `Demuxer::feed`
    /// scanned more than 32 × 188 = 6,016 bytes without a confirmed
    /// `0x47`. The verdict covers that window only — the scanned bytes
    /// are discarded, the counter restarts at zero and the next `feed`
    /// starts a fresh search (no `reset_sync` needed), except that a
    /// candidate sync byte the scan stopped on is retained and
    /// re-validated (N-of-M) on the next `feed` rather than accepted
    /// outright. `feed_aligned` returns it with `after_bytes: 0` for a
    /// packet whose first byte is not `0x47`. PSI checksum failures
    /// never produce it — they surface as `NonConformant` events.
    #[error("demuxer cannot recover sync after {after_bytes} bytes")]
    Unrecoverable { after_bytes: usize },

    /// Strict mode rejected a `NonConformantIssue`. Lenient mode would have
    /// emitted a `NonConformant` event instead and continued.
    #[error("strict-mode rejection: {0}")]
    StrictRejection(String),

    /// PSI section claimed a length that doesn't fit a valid PAT/PMT.
    /// Distinct from a checksum mismatch (which is `NonConformant` in
    /// lenient mode); this is structurally impossible.
    #[error("malformed PSI section at PID 0x{pid:04X}: {reason}")]
    MalformedPsi { pid: u16, reason: &'static str },

    /// PES header at PID 0x{pid:04X} declared a length that's too short to
    /// contain its own claimed flags. Unlike PSI checksum failures (which
    /// surface as `NonConformant` in lenient mode), this prevents the
    /// reassembler from making any forward progress.
    #[error("malformed PES header at PID 0x{pid:04X}: {reason}")]
    MalformedPes { pid: u16, reason: &'static str },

    /// The demuxer's pre-sync buffer (`Demuxer::sync_buf`) exceeded its
    /// configured ceiling. Fired when the projected buffer size after a
    /// `feed` call would exceed the cap (default 4 MiB, configurable via
    /// [`crate::mpegts::demux::DemuxerConfig::sync_buf_cap`]). The bytes
    /// in the call are never copied in — the check fires before
    /// `extend_from_slice`. On this error the demuxer clears `sync_buf`
    /// and resets sync state; the caller's only sane response is to
    /// teardown the demuxer or feed in smaller chunks going forward.
    #[error(
        "demuxer sync buffer exhausted: {observed} bytes exceeds the {max} byte ceiling; \
             feed in smaller chunks and drain events between feeds, or raise \
             DemuxerConfig::sync_buf_cap (this ceiling is NOT pes_cap_per_pid/pes_cap_total)"
    )]
    SyncBufExhausted { observed: usize, max: usize },
}
