//! Builders for MPEG-TS PMT per-stream descriptor TLVs.
//!
//! Each builder produces one complete descriptor (`descriptor_tag` +
//! `descriptor_length` + body). Two return-type families exist:
//!
//! - **Fixed-shape builders** return `Vec<u8>` infallibly — the body
//!   size is bounded by the call signature (e.g. [`stream_identifier`],
//!   [`format_identifier_av01`], [`iso_639_language`], [`metadata_klva`]).
//! - **Caller-sized builders** return `Result<Vec<u8>, DescriptorError>`
//!   because the body can overflow the 8-bit `descriptor_length` field
//!   (H.222.0 §2.6: max 255 bytes) — [`registration`], [`user_private`],
//!   [`user_private_with_tag`], [`descriptor_with_tag_unchecked`],
//!   [`component`], [`subtitling_descriptor_multi`],
//!   [`teletext_descriptor_multi`].
//!
//! Hand the result (unwrap or `?`) to
//! [`crate::mpegts::mux::MuxerProgramConfigBuilder::stream_descriptors_for_video`]
//! / `_for_klv` / `_for_audio` to splice it into the per-stream
//! descriptor loop emitted in PMT.
//!
//! Reference: ISO/IEC 13818-1 §2.6 and ETSI EN 300 468 §6.2.

use alloc::vec::Vec;
pub mod parse;
pub use parse::{
    DescriptorParseError, RawDescriptor, SubtitlingDescriptorEntry, TeletextDescriptorEntry,
    find_descriptor_tag, find_format_identifier, parse_subtitling_descriptor,
    parse_teletext_descriptor,
};

/// Errors returned by descriptor builder helpers in this module.
///
/// Four failure modes today:
///
/// - [`DescriptorError::EmptyEntries`] — empty `entries` arguments
///   produce a degenerate `tag 0x00` descriptor that the demux parser
///   rejects with [`DescriptorParseError::EmptyInput`]. The encoder
///   rejects the same shape symmetrically rather than emitting invalid
///   PSI.
/// - [`DescriptorError::TooLarge`] — the payload would overflow the
///   8-bit `descriptor_length` field (H.222.0 §2.6: max body 255 bytes).
///   A hard error, not silent truncation: truncation produces malformed
///   PSI without surfacing the bug to the caller.
/// - [`DescriptorError::InvalidComponent`] — invalid `stream_content_ext`
///   / `stream_content` combination passed to [`component`] per
///   EN 300 468 §6.2.8.
/// - [`DescriptorError::InvalidTag`] — a tag outside the user-private
///   range `0x40..=0xFF` was passed to [`user_private_with_tag`]. Use
///   [`descriptor_with_tag_unchecked`] for deliberate out-of-range tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DescriptorError {
    /// `entries` slice was empty for a multi-entry descriptor builder
    /// (subtitling 0x59 or teletext 0x56). Caller must supply at least
    /// one entry.
    #[error("descriptor tag 0x{tag:02X}: entries slice is empty (must be non-empty)")]
    EmptyEntries { tag: u8 },

    /// Descriptor body would exceed the 8-bit `descriptor_length` field
    /// (max 255 bytes per H.222.0 §2.6). `len` is the would-be body
    /// length (excluding the 2-byte tag + length header); `max` is the
    /// spec ceiling for this tag (255 for most descriptors; 249 for
    /// `component` which carries 6 bytes of metadata before the text;
    /// 251 for `registration`'s `additional_identification_info` which
    /// follows the 4-byte format_identifier).
    ///
    /// Returned by every builder whose payload is caller-supplied and
    /// not statically bounded — [`registration`], [`user_private`],
    /// [`user_private_with_tag`], [`descriptor_with_tag_unchecked`],
    /// [`component`], [`subtitling_descriptor_multi`],
    /// [`teletext_descriptor_multi`].
    #[error("descriptor tag 0x{tag:02X}: payload length {len} exceeds spec maximum of {max} bytes")]
    TooLarge { tag: u8, len: usize, max: usize },

    /// `component()` was given an invalid `stream_content_ext` /
    /// `stream_content` combination per EN 300 468 §6.2.8 (a nibble > 0xF,
    /// or `ext != 0xF` for a legacy `stream_content` in `0x1..=0x8`).
    #[error(
        "component descriptor: invalid stream_content_ext 0x{ext:X} for stream_content 0x{content:X} (EN 300 468 §6.2.8)"
    )]
    InvalidComponent { ext: u8, content: u8 },

    /// A descriptor tag outside the user-private range `0x40..=0xFF` was
    /// passed to a validated builder. Use [`descriptor_with_tag_unchecked`]
    /// for deliberate out-of-range tags.
    #[error("descriptor tag 0x{tag:02X}: outside the user-private range 0x40..=0xFF")]
    InvalidTag { tag: u8 },
}

/// Registration descriptor (tag 0x05) — H.222.0 §2.6.8.
///
/// `format_identifier` is a 4-byte ASCII tag. `additional` is the
/// optional `additional_identification_info` payload (e.g. the 4-byte
/// `FF 1B 44 3F` trailer Haivision-shaped senders put after `"HDMV"`
/// on H.264 video PIDs).
///
/// Total body must be ≤ 255 bytes per the 8-bit `descriptor_length`
/// field (H.222.0 §2.6), bounding `additional.len()` to 251 bytes
/// (4 bytes of format_identifier + 251 bytes of additional info).
///
/// # Errors
///
/// Returns [`DescriptorError::TooLarge`] when `additional.len() > 251`, so
/// malformed PSI never goes on the wire.
pub fn registration(
    format_identifier: [u8; 4],
    additional: &[u8],
) -> Result<Vec<u8>, DescriptorError> {
    // Cap on additional_identification_info: 255 (descriptor_length) -
    // 4 (format_identifier) = 251.
    const REGISTRATION_ADDITIONAL_MAX: usize = 251;
    if additional.len() > REGISTRATION_ADDITIONAL_MAX {
        return Err(DescriptorError::TooLarge {
            tag: 0x05,
            len: additional.len(),
            max: REGISTRATION_ADDITIONAL_MAX,
        });
    }
    let body_len = 4 + additional.len();
    let mut out = Vec::with_capacity(2 + body_len);
    out.push(0x05);
    out.push(body_len as u8);
    out.extend_from_slice(&format_identifier);
    out.extend_from_slice(additional);
    Ok(out)
}

/// Metadata descriptor (tag 0x26) — H.222.0 §2.6.60 — for KLV PIDs
/// carried as `stream_type=0x15` (Metadata in PES). Hard-coded to the
/// canonical KLVA shape:
///   metadata_application_format = 0x0100 (User defined)
///   metadata_format             = 0xFF (defined by format_identifier)
///   metadata_format_identifier  = 0x4B4C5641 ("KLVA")
///   metadata_service_id         = service_id (caller; 0 is universal)
///   decoder_config_flags + DSM_CC_flag = 0  (4 reserved bits = 1)
///
/// Matches the Family B (ARS) corpus byte-for-byte.
pub fn metadata_klva(service_id: u8) -> Vec<u8> {
    vec![
        0x26, 0x09, 0x01, 0x00, 0xFF, b'K', b'L', b'V', b'A', service_id, 0x0F,
    ]
}

/// Metadata STD descriptor (tag 0x27) — H.222.0 §2.6.62 — STD-buffer
/// dimensions for a Metadata-in-PES stream. All-zero values match the
/// "no rate-shaping declared" shape Family B uses.
///
/// Each rate field is 22 bits (top 2 bits reserved = `11`).
pub fn metadata_std(input_leak_rate: u32, buffer_size: u32, output_leak_rate: u32) -> Vec<u8> {
    fn pack_22(value: u32) -> [u8; 3] {
        let v = value & 0x003F_FFFF;
        [0xC0 | ((v >> 16) as u8 & 0x3F), (v >> 8) as u8, v as u8]
    }
    let i = pack_22(input_leak_rate);
    let b = pack_22(buffer_size);
    let o = pack_22(output_leak_rate);
    vec![
        0x27, 0x09, i[0], i[1], i[2], b[0], b[1], b[2], o[0], o[1], o[2],
    ]
}

/// User-private descriptor (tag 0xFF). ISO/IEC 13818-1 reserves tag
/// 0xFF, but real-world ARS-shape senders use it as the de-facto
/// per-stream label slot (e.g. `"VIDEO-ARS"`, `"KLV_SYNC"`,
/// `"JSONCMD"`).
///
/// `tst-core`'s demuxer surfaces it both via
/// [`crate::mpegts::demux::event::StreamInfo::raw_descriptors`] and (when
/// payload is valid UTF-8) via the demuxer-side stats label.
///
/// `payload` ≤ 255 bytes; not interpreted by this helper.
///
/// # Errors
///
/// Returns [`DescriptorError::TooLarge`] when `payload.len() > 255`.
pub fn user_private(payload: &[u8]) -> Result<Vec<u8>, DescriptorError> {
    user_private_with_tag(0xFF, payload)
}

/// Same as [`user_private`] but with a caller-chosen tag in the
/// user-private / reserved range `0x40..=0xFF`. Use when emitting
/// vendor-defined slots that aren't tag 0xFF.
///
/// Tags below `0x40` are rejected in all builds — pass a spec-assigned
/// tag only via [`descriptor_with_tag_unchecked`], which opts out of
/// the range guard deliberately.
///
/// # Errors
///
/// - [`DescriptorError::InvalidTag`] when `tag < 0x40`.
/// - [`DescriptorError::TooLarge`] when `payload.len() > 255`.
pub fn user_private_with_tag(tag: u8, payload: &[u8]) -> Result<Vec<u8>, DescriptorError> {
    if tag < 0x40 {
        return Err(DescriptorError::InvalidTag { tag });
    }
    descriptor_with_tag_unchecked(tag, payload)
}

/// Build a descriptor with an arbitrary caller-chosen `tag` and NO tag-range
/// validation. Use only when a descriptor must carry a tag outside the
/// user-private range `0x40..=0xFF` by deliberate design (e.g. raw
/// passthrough). Prefer [`user_private_with_tag`] for vendor-defined slots.
///
/// # Errors
///
/// Returns [`DescriptorError::TooLarge`] when `payload.len() > 255`.
pub fn descriptor_with_tag_unchecked(tag: u8, payload: &[u8]) -> Result<Vec<u8>, DescriptorError> {
    if payload.len() > 255 {
        return Err(DescriptorError::TooLarge {
            tag,
            len: payload.len(),
            max: 255,
        });
    }
    let len = payload.len();
    let mut out = Vec::with_capacity(2 + len);
    out.push(tag);
    out.push(len as u8);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Component descriptor (tag 0x50) — ETSI EN 300 468 §6.2.8.
/// Carries language-tagged free text. Not seen in real-world
/// captures; included because it's the textbook "human label" slot
/// that [`crate::mpegts::demux::low_level::extract_user_label`] reads first.
///
/// `stream_content_ext` and `stream_content` are 4-bit fields that
/// together form the first body byte: `(ext << 4) | (content & 0x0F)`.
/// Per EN 300 468 V1.19.1 §6.2.8, `ext` shall be `0xF` for the legacy
/// `stream_content` values `0x1..=0x8`; non-legacy content values may
/// carry a distinct `ext` nibble.
///
/// `text` is UTF-8; receivers conventionally treat the body as
/// language-coded per the descriptor's `iso_639_language_code` field.
///
/// # Errors
///
/// - [`DescriptorError::InvalidComponent`] when either nibble exceeds
///   `0xF`, or when `stream_content ∈ 0x1..=0x8` and `stream_content_ext
///   != 0xF` (EN 300 468 §6.2.8 requires `ext=0xF` for legacy content).
/// - [`DescriptorError::TooLarge`] when `text.len() > 249`
///   (255-byte `descriptor_length` ceiling minus 6 bytes of leading
///   fields = 249 bytes of text).
pub fn component(
    stream_content_ext: u8,
    stream_content: u8,
    component_type: u8,
    component_tag: u8,
    iso_639_language: [u8; 3],
    text: &str,
) -> Result<Vec<u8>, DescriptorError> {
    // EN 300 468 V1.19.1 §6.2.8: body byte 0 = stream_content_ext (high
    // nibble) | stream_content (low nibble); both are 4-bit fields, and
    // ext shall be 0xF only for the legacy content values 0x1..=0x8.
    if stream_content_ext > 0x0F
        || stream_content > 0x0F
        || ((0x1..=0x8).contains(&stream_content) && stream_content_ext != 0xF)
    {
        return Err(DescriptorError::InvalidComponent {
            ext: stream_content_ext,
            content: stream_content,
        });
    }
    // Cap on text: 255 (descriptor_length) - 6 (stream_content_ext/content +
    // component_type + component_tag + 3 language bytes) = 249.
    const COMPONENT_TEXT_MAX: usize = 249;
    let text_bytes = text.as_bytes();
    if text_bytes.len() > COMPONENT_TEXT_MAX {
        return Err(DescriptorError::TooLarge {
            tag: 0x50,
            len: text_bytes.len(),
            max: COMPONENT_TEXT_MAX,
        });
    }
    let body_len = 6 + text_bytes.len();
    let mut out = Vec::with_capacity(2 + body_len);
    out.push(0x50);
    out.push(body_len as u8);
    out.push((stream_content_ext << 4) | (stream_content & 0x0F)); // EN 300 468 §6.2.8
    out.push(component_type);
    out.push(component_tag);
    out.extend_from_slice(&iso_639_language);
    out.extend_from_slice(text_bytes);
    Ok(out)
}

/// Stream Identifier descriptor (tag 0x52) — ETSI EN 300 468 §6.2.39.
/// Single `component_tag` byte; pairs with a Component descriptor for
/// the actual text.
pub fn stream_identifier(component_tag: u8) -> Vec<u8> {
    vec![0x52, 0x01, component_tag]
}

/// DVB subtitling_descriptor (tag 0x59), single-entry form.
/// ETSI EN 300 468 §6.2.41.
///
/// `language` is ISO 639-2 lowercase ASCII. `subtitling_type` per
/// Table 26 (e.g. 0x10 = DVB sub, no AR signalling). `composition_page_id`
/// and `ancillary_page_id` are 16-bit values per spec.
pub fn subtitling_descriptor(
    language: [u8; 3],
    subtitling_type: u8,
    composition_page_id: u16,
    ancillary_page_id: u16,
) -> Vec<u8> {
    // Single-entry input is statically non-empty (rules out
    // EmptyEntries) and the 8-byte body is well under 255 (rules out
    // TooLarge). Multi-helper failure modes are unreachable here.
    subtitling_descriptor_multi(&[(
        language,
        subtitling_type,
        composition_page_id,
        ancillary_page_id,
    )])
    .expect("single-entry 8-byte body is statically within both DescriptorError bounds")
}

/// DVB subtitling_descriptor (tag 0x59), multi-entry form per
/// ETSI EN 300 468 §6.2.41. Each entry is `(language, subtitling_type,
/// composition_page_id, ancillary_page_id)`.
///
/// Use this for multi-language single-PID DVB subtitling services. The
/// single-entry helper [`subtitling_descriptor`] is a `len=1` shorthand.
///
/// # Errors
///
/// - [`DescriptorError::EmptyEntries`] if `entries` is empty — the
///   demux parser rejects an empty subtitling_descriptor with
///   [`DescriptorParseError::EmptyInput`], so the encoder rejects
///   symmetrically.
/// - [`DescriptorError::TooLarge`] if the total body
///   (`entries.len() * 8`) exceeds 255 bytes (i.e. more than 31
///   entries).
pub fn subtitling_descriptor_multi(
    entries: &[([u8; 3], u8, u16, u16)],
) -> Result<Vec<u8>, DescriptorError> {
    if entries.is_empty() {
        return Err(DescriptorError::EmptyEntries { tag: 0x59 });
    }
    let body_len = entries.len() * 8;
    if body_len > u8::MAX as usize {
        return Err(DescriptorError::TooLarge {
            tag: 0x59,
            len: body_len,
            max: u8::MAX as usize,
        });
    }
    let mut out = Vec::with_capacity(2 + body_len);
    out.push(0x59); // tag
    out.push(body_len as u8); // length
    for (language, subtitling_type, comp_page_id, anc_page_id) in entries {
        out.extend_from_slice(language);
        out.push(*subtitling_type);
        out.extend_from_slice(&comp_page_id.to_be_bytes());
        out.extend_from_slice(&anc_page_id.to_be_bytes());
    }
    Ok(out)
}

/// DVB teletext_descriptor (tag 0x56), single-entry form.
/// ETSI EN 300 468 §6.2.43.
///
/// `language` is ISO 639-2 lowercase ASCII. `teletext_type` is a 5-bit
/// value (e.g. 0x02 = subtitle page). `magazine_number` is 0..=7
/// (3 bits). `page_number` is BCD-encoded.
pub fn teletext_descriptor(
    language: [u8; 3],
    teletext_type: u8,
    magazine_number: u8,
    page_number: u8,
) -> Vec<u8> {
    // Single-entry input is statically non-empty (rules out
    // EmptyEntries) and the 5-byte body is well under 255 (rules out
    // TooLarge). Multi-helper failure modes are unreachable here.
    teletext_descriptor_multi(&[(language, teletext_type, magazine_number, page_number)])
        .expect("single-entry 5-byte body is statically within both DescriptorError bounds")
}

/// DVB teletext_descriptor (tag 0x56), multi-entry form per
/// ETSI EN 300 468 §6.2.43. Each entry is `(language, teletext_type,
/// magazine_number, page_number)`.
///
/// Use this for multi-language single-PID DVB teletext services. The
/// single-entry helper [`teletext_descriptor`] is a `len=1` shorthand.
///
/// # Errors
///
/// - [`DescriptorError::EmptyEntries`] if `entries` is empty — the
///   demux parser rejects an empty teletext_descriptor with
///   [`DescriptorParseError::EmptyInput`], so the encoder rejects
///   symmetrically.
/// - [`DescriptorError::TooLarge`] if the total body
///   (`entries.len() * 5`) exceeds 255 bytes (i.e. more than 51
///   entries).
pub fn teletext_descriptor_multi(
    entries: &[([u8; 3], u8, u8, u8)],
) -> Result<Vec<u8>, DescriptorError> {
    if entries.is_empty() {
        return Err(DescriptorError::EmptyEntries { tag: 0x56 });
    }
    let body_len = entries.len() * 5;
    if body_len > u8::MAX as usize {
        return Err(DescriptorError::TooLarge {
            tag: 0x56,
            len: body_len,
            max: u8::MAX as usize,
        });
    }
    let mut out = Vec::with_capacity(2 + body_len);
    out.push(0x56); // tag
    out.push(body_len as u8); // length
    for (language, teletext_type, magazine_number, page_number) in entries {
        out.extend_from_slice(language);
        out.push(((teletext_type & 0x1F) << 3) | (magazine_number & 0x07));
        out.push(*page_number);
    }
    Ok(out)
}

/// `registration_descriptor` (tag 0x05) carrying ASCII format_identifier
/// `"VTTC"` — used as a marker for WebVTT-in-MPEG-TS. Not defined by
/// RFC 8216 / draft-pantos-hls-rfc8216bis nor any published normative
/// spec; appears in ffmpeg's `mpegtsenc.c` emitter and is widely
/// observed in WebVTT-in-TS captures. **Library-internal round-trip
/// only — external-tool interop has not been empirically verified as
/// of this writing.** See `docs/project/deferred-features.md` "WebVTT-in-TS
/// interop" for the empirical-test-pending status.
pub fn format_identifier_vttc() -> Vec<u8> {
    vec![0x05, 0x04, b'V', b'T', b'T', b'C']
}

/// `registration_descriptor` (tag 0x05) carrying ASCII format_identifier
/// `"GA94"` — used as a marker for CEA-708 caption data carried as a
/// standalone elementary stream. ATSC A/53 Part 4 §6.2.3 defines
/// `"GA94"` as the `user_data_identifier` for caption data **embedded
/// in MPEG-2 video user_data**, not as a stream-level marker. **The
/// auto-emitted descriptor here is for library-internal round-trip only
/// — external-tool interop has not been empirically verified as of
/// this writing.** See `docs/project/deferred-features.md` "CEA-708 interop"
/// for the empirical-test-pending status.
pub fn format_identifier_ga94() -> Vec<u8> {
    vec![0x05, 0x04, b'G', b'A', b'9', b'4']
}

/// `registration_descriptor` (tag 0x05) carrying ASCII format_identifier
/// `"AV01"` — the AV1-in-MPEG-2-TS binding §2.1 marker. Per the binding
/// spec, this descriptor MUST be the first in the per-stream PMT
/// descriptor loop.
pub fn format_identifier_av01() -> Vec<u8> {
    vec![0x05, 0x04, b'A', b'V', b'0', b'1']
}

/// `registration_descriptor` (tag 0x05) carrying ASCII format_identifier
/// `"AC-3"` — the ATSC AC-3 marker per ATSC A/53 Part 3 §5.1. Strict
/// ATSC consumers (ffmpeg, GStreamer, TSDuck) gate AC-3 classification
/// on stream_type 0x81 + this registration; without it they may
/// fall back to probing or misclassify as MP3-on-user-private.
///
/// 6 bytes total: tag(1) + length(1) + format_identifier(4).
///
/// Note: DVB-shaped AC-3 (stream_type 0x06 + DVB AC-3 descriptor 0x6A)
/// is a distinct path and remains deferred — see `deferred-features.md`.
pub fn format_identifier_ac3() -> Vec<u8> {
    vec![0x05, 0x04, b'A', b'C', b'-', b'3']
}

/// AC-3 audio stream descriptor (tag 0x81) — ATSC A/52:2018 §A.4.3
/// Table A4.1.
///
/// Mandatory on every AC-3 elementary-stream PMT entry under System A
/// (ATSC), per §A.4.3 ("shall be constructed"). Without it, strict
/// receivers may fall back to probing the elementary stream for the
/// fields signaled here (sample_rate / bsid / bit_rate / surround /
/// service-mode / channels / language).
///
/// This builder emits the minimum-conformant 3-byte payload — fields
/// up to and including `langcod` (the first allowed termination point
/// per the table's horizontal lines). Callers needing the full
/// extension (text, language codes, asvcflags) can build them on top
/// of [`registration`] or layer additional bytes via a follow-up
/// helper.
///
/// Field encoding per Table A4.1 (3 bytes after tag+length):
///
/// | Byte | Field layout, MSB → LSB (bit widths) |
/// |---|---|
/// | 0 | sample_rate_code (3), bsid (5) |
/// | 1 | bit_rate_code (6), surround_mode (2) |
/// | 2 | bsmod (3), num_channels (4), full_svc (1) |
///
/// Field values (from a parsed `Ac3SyncInfo` — see
/// [`crate::codec::ac3::parse_syncframe`]):
///
/// - `sample_rate_code` — 3-bit per Table A4.2. Mirrors `fscod`
///   directly (0=48k, 1=44.1k, 2=32k). Values 4..=7 indicate sets of
///   rates but aren't used here (we always have exact `fscod`).
/// - `bsid` — same as AC-3 elementary stream's `bsid` field.
/// - `bit_rate_code` — 6-bit per Table A4.3. The lower 5 bits index a
///   nominal-bit-rate table; the MSB is 0 for "exact rate" or 1 for
///   "upper limit". We emit MSB=0 (exact) since `frmsizecod` gives the
///   exact rate.
/// - `surround_mode` — 2-bit per Table A4.4. We emit 0b00 ("not
///   indicated"); the AC-3 elementary stream's `dsurmod` is not surfaced
///   by the minimal parser.
/// - `bsmod` — same as AC-3 elementary stream's `bsmod` field.
/// - `num_channels` — 4-bit per Table A4.5. We mirror `acmod` in the
///   MSB-0 ("audio coding mode") encoding so receivers see the exact
///   channel layout (e.g. acmod=2 → num_channels=0b0010 = 2/0 stereo).
/// - `full_svc` — 1-bit (1 = "complete program suitable for
///   presentation"). We emit 1 — the strict-A/53 default for ISR /
///   gimbaled-platform audio (no associated-service overlay).
///
/// Field-range invariants are enforced via `debug_assert!`; release
/// builds silently mask the inputs to their bit widths. Callers source
/// the fields from a parsed [`crate::codec::ac3::Ac3SyncInfo`] which
/// already range-validates each field, so production paths never trip
/// the asserts. The 3-byte payload is statically bounded (no
/// `DescriptorError::TooLarge` path), so the return type is plain
/// `Vec<u8>` rather than `Result`.
pub fn ac3_audio_stream_descriptor(
    sample_rate_code: u8,
    bsid: u8,
    bit_rate_code: u8,
    surround_mode: u8,
    bsmod: u8,
    num_channels: u8,
    full_svc: bool,
) -> Vec<u8> {
    // All callers pre-validate the inputs to fit within their bit
    // widths (sourced from a parsed Ac3SyncInfo). debug_assert is the
    // workspace convention for caller-contract checks that production
    // code paths never violate.
    debug_assert!(sample_rate_code < 8, "sample_rate_code is 3 bits");
    debug_assert!(bsid < 32, "bsid is 5 bits");
    debug_assert!(bit_rate_code < 64, "bit_rate_code is 6 bits");
    debug_assert!(surround_mode < 4, "surround_mode is 2 bits");
    debug_assert!(bsmod < 8, "bsmod is 3 bits");
    debug_assert!(num_channels < 16, "num_channels is 4 bits");
    let byte0 = ((sample_rate_code & 0b111) << 5) | (bsid & 0b1_1111);
    let byte1 = ((bit_rate_code & 0b0011_1111) << 2) | (surround_mode & 0b11);
    let byte2 =
        ((bsmod & 0b111) << 5) | ((num_channels & 0b1111) << 1) | if full_svc { 1 } else { 0 };
    vec![0x81, 0x03, byte0, byte1, byte2]
}

/// ISO 639 Language descriptor (tag 0x0A) — H.222.0 §2.6.18.
/// 3-byte language code + 1-byte audio_type. Conventional on audio PIDs;
/// valid on any ES.
pub fn iso_639_language(language: [u8; 3], audio_type: u8) -> Vec<u8> {
    vec![
        0x0A,
        0x04,
        language[0],
        language[1],
        language[2],
        audio_type,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_klva_no_additional() {
        let bytes = registration(*b"KLVA", &[]).expect("within length cap");
        assert_eq!(bytes, vec![0x05, 0x04, b'K', b'L', b'V', b'A']);
    }

    #[test]
    fn format_identifier_ac3_canonical_bytes() {
        let bytes = format_identifier_ac3();
        assert_eq!(bytes, vec![0x05, 0x04, b'A', b'C', b'-', b'3']);
        assert_eq!(bytes.len(), 6);
        assert_eq!(bytes[0], 0x05); // registration_descriptor tag
        assert_eq!(bytes[1], 0x04); // length
        assert_eq!(&bytes[2..6], b"AC-3");
    }

    #[test]
    fn format_identifier_av01_canonical_bytes() {
        let bytes = format_identifier_av01();
        assert_eq!(bytes, vec![0x05, 0x04, b'A', b'V', b'0', b'1']);
    }

    #[test]
    fn registration_hdmv_with_trailing_bytes() {
        // Family A's video PID shape from a real-world capture.
        let bytes = registration(*b"HDMV", &[0xFF, 0x1B, 0x44, 0x3F]).expect("within length cap");
        assert_eq!(
            bytes,
            vec![0x05, 0x08, b'H', b'D', b'M', b'V', 0xFF, 0x1B, 0x44, 0x3F]
        );
    }

    #[test]
    fn registration_accepts_max_additional() {
        // additional.len() == 251 → body == 4 + 251 == 255 (descriptor_length max).
        let long = vec![0xAAu8; 251];
        let bytes = registration(*b"TEST", &long).expect("251 bytes within cap");
        assert_eq!(bytes[0], 0x05); // descriptor_tag
        assert_eq!(bytes[1], 255); // descriptor_length at the u8 ceiling
        assert_eq!(bytes.len(), 2 + 255);
        assert_eq!(&bytes[2..6], b"TEST");
        assert!(bytes[6..].iter().all(|&b| b == 0xAA));
        assert_eq!(bytes[6..].len(), 251);
    }

    #[test]
    fn registration_rejects_oversized_additional() {
        // additional.len() == 252 would overflow the u8 descriptor_length;
        // the overflow is a hard error, never a silent truncation.
        let long = vec![0xAAu8; 252];
        let err = registration(*b"TEST", &long).unwrap_err();
        assert_eq!(
            err,
            DescriptorError::TooLarge {
                tag: 0x05,
                len: 252,
                max: 251,
            }
        );
    }

    #[test]
    fn metadata_klva_canonical_shape() {
        // Family B's KLV PID 0x26 descriptor — exact bytes from the corpus.
        // app_format=0x0100 | metadata_format=0xFF | format_id=0x4B4C5641
        // (KLVA) | service_id=0x00 | flags=0x00 (decoder_config_flags=0,
        // DSM_CC_flag=0, 4 reserved bits=1).
        let bytes = metadata_klva(0x00);
        assert_eq!(
            bytes,
            vec![
                0x26, 0x09, 0x01, 0x00, 0xFF, 0x4B, 0x4C, 0x56, 0x41, 0x00, 0x0F
            ]
        );
    }

    #[test]
    fn metadata_std_all_zero() {
        // Matches Family B's 0x27 descriptor (input/buf/output rates all 0,
        // upper bits reserved = 1).
        let bytes = metadata_std(0, 0, 0);
        assert_eq!(
            bytes,
            vec![
                0x27, 0x09, 0xC0, 0x00, 0x00, 0xC0, 0x00, 0x00, 0xC0, 0x00, 0x00
            ]
        );
    }

    #[test]
    fn metadata_std_packs_22bit_rates_with_reserved_bits() {
        // input_leak_rate = 1 → low-byte = 0x01, high byte = 0xC0 | 0 = 0xC0.
        // buffer_size = 0x3F_FFFF (max 22-bit) → 0xFF 0xFF 0xFF (high byte
        // = 0xC0 | 0x3F = 0xFF).
        // output_leak_rate = 0x100 → bytes 0xC0, 0x01, 0x00.
        let bytes = metadata_std(1, 0x003F_FFFF, 0x100);
        assert_eq!(
            bytes,
            vec![
                0x27, 0x09, 0xC0, 0x00, 0x01, 0xFF, 0xFF, 0xFF, 0xC0, 0x01, 0x00
            ]
        );
    }

    #[test]
    fn user_private_default_tag_0xff() {
        let bytes = user_private(b"VIDEO-ARS").expect("within length cap");
        assert_eq!(
            bytes,
            vec![
                0xFF, 9, b'V', b'I', b'D', b'E', b'O', b'-', b'A', b'R', b'S'
            ]
        );
    }

    #[test]
    fn user_private_with_tag_lets_caller_pick_slot() {
        let bytes = user_private_with_tag(0x7E, b"VENDOR").expect("within length cap");
        assert_eq!(bytes, vec![0x7E, 6, b'V', b'E', b'N', b'D', b'O', b'R']);
    }

    #[test]
    fn user_private_rejects_oversized_payload() {
        // Payload 256 bytes overflows u8 descriptor_length.
        let payload = vec![0u8; 256];
        let err = user_private(&payload).unwrap_err();
        assert_eq!(
            err,
            DescriptorError::TooLarge {
                tag: 0xFF,
                len: 256,
                max: 255,
            }
        );
    }

    #[test]
    fn user_private_with_tag_rejects_oversized_payload() {
        let payload = vec![0u8; 300];
        let err = user_private_with_tag(0x7E, &payload).unwrap_err();
        assert_eq!(
            err,
            DescriptorError::TooLarge {
                tag: 0x7E,
                len: 300,
                max: 255,
            }
        );
    }

    #[test]
    fn user_private_with_tag_rejects_below_range_in_all_builds() {
        // 0x05 (Registration) is a spec-assigned tag, not user-private.
        let err = user_private_with_tag(0x05, b"x");
        assert!(
            matches!(err, Err(DescriptorError::InvalidTag { tag: 0x05 })),
            "got {err:?}"
        );
    }

    #[test]
    fn descriptor_with_tag_unchecked_allows_any_tag() {
        let bytes =
            descriptor_with_tag_unchecked(0x05, b"x").expect("unchecked allows arbitrary tags");
        assert_eq!(bytes, vec![0x05, 0x01, b'x']);
    }

    #[test]
    fn user_private_accepts_max_255_bytes() {
        let payload = vec![0xABu8; 255];
        let bytes = user_private(&payload).expect("255 bytes within cap");
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1], 255);
        assert_eq!(bytes.len(), 2 + 255);
    }

    #[test]
    fn component_descriptor_textbook_shape() {
        // ext=0xF, content=0x09 → first body byte 0xF9 (unchanged wire output).
        let bytes =
            component(0xF, 0x09, 0x00, 0x42, *b"eng", "EO 1080p").expect("within length cap");
        assert_eq!(
            bytes,
            vec![
                0x50, 14, 0xF9, 0x00, 0x42, b'e', b'n', b'g', b'E', b'O', b' ', b'1', b'0', b'8',
                b'0', b'p'
            ]
        );
    }

    #[test]
    fn component_descriptor_encodes_non_legacy_ext() {
        // EN 300 468 §6.2.8: ext may be non-0xF for content outside 0x1..=0x8.
        let bytes = component(0x2, 0x09, 0x00, 0x42, *b"eng", "x").expect("valid");
        assert_eq!(bytes[2], 0x29);
    }

    #[test]
    fn component_descriptor_rejects_legacy_content_with_non_f_ext() {
        // content 0x05 ∈ 0x1..=0x8 ⇒ ext must be 0xF.
        let err = component(0x3, 0x05, 0x00, 0x42, *b"eng", "x");
        assert!(
            matches!(
                err,
                Err(DescriptorError::InvalidComponent {
                    ext: 0x3,
                    content: 0x05
                })
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn component_descriptor_rejects_oversized_nibble() {
        let err = component(0x10, 0x00, 0x00, 0x42, *b"eng", "x");
        assert!(
            matches!(err, Err(DescriptorError::InvalidComponent { .. })),
            "got {err:?}"
        );
    }

    #[test]
    fn component_rejects_oversized_text() {
        // 250 bytes of text would overflow the 249-byte cap (6 fixed
        // bytes + 250 = 256 body > 255).
        let text: String = "A".repeat(250);
        let err = component(0xF, 0x09, 0x00, 0x42, *b"eng", &text).unwrap_err();
        assert_eq!(
            err,
            DescriptorError::TooLarge {
                tag: 0x50,
                len: 250,
                max: 249,
            }
        );
    }

    #[test]
    fn component_accepts_max_249_byte_text() {
        let text: String = "A".repeat(249);
        let bytes = component(0xF, 0x09, 0x00, 0x42, *b"eng", &text).expect("249 bytes within cap");
        assert_eq!(bytes[0], 0x50);
        assert_eq!(bytes[1], 255); // body length at u8 ceiling
        assert_eq!(bytes.len(), 2 + 255);
    }

    #[test]
    fn stream_identifier_one_byte_body() {
        let bytes = stream_identifier(0x42);
        assert_eq!(bytes, vec![0x52, 0x01, 0x42]);
    }

    #[test]
    fn iso_639_language_4_byte_body() {
        let bytes = iso_639_language(*b"eng", 0x00);
        assert_eq!(bytes, vec![0x0A, 0x04, b'e', b'n', b'g', 0x00]);
    }

    #[test]
    fn subtitling_descriptor_single_entry_round_trip_bytes() {
        // ETSI EN 300 468 §6.2.41 single-entry: tag(1) + length(1) +
        //   ISO_639_lang(3) + subtitling_type(1) + composition_page_id(2) +
        //   ancillary_page_id(2) = 8 payload bytes.
        let bytes = subtitling_descriptor(*b"eng", 0x10, 0x0001, 0x0001);
        assert_eq!(
            bytes,
            vec![
                0x59, // tag
                0x08, // length
                b'e', b'n', b'g', 0x10, 0x00, 0x01, 0x00, 0x01,
            ]
        );
    }

    #[test]
    fn teletext_descriptor_single_entry_round_trip_bytes() {
        // ETSI EN 300 468 §6.2.43 single-entry: tag(1) + length(1) +
        //   ISO_639_lang(3) + (teletext_type<<3 | magazine_number)(1) +
        //   page_number(1) = 5 payload bytes.
        let bytes = teletext_descriptor(*b"eng", 0x02, 1, 0x88);
        assert_eq!(
            bytes,
            vec![
                0x56, // tag
                0x05, // length
                b'e',
                b'n',
                b'g',
                (0x02 << 3) | 1,
                0x88,
            ]
        );
    }

    #[test]
    fn format_identifier_vttc_descriptor_round_trip_bytes() {
        let bytes = format_identifier_vttc();
        assert_eq!(bytes, vec![0x05, 0x04, b'V', b'T', b'T', b'C']);
    }

    #[test]
    fn format_identifier_ga94_descriptor_round_trip_bytes() {
        let bytes = format_identifier_ga94();
        assert_eq!(bytes, vec![0x05, 0x04, b'G', b'A', b'9', b'4']);
    }

    #[test]
    fn format_identifier_av01_descriptor_round_trip_bytes() {
        let bytes = format_identifier_av01();
        assert_eq!(bytes, vec![0x05, 0x04, b'A', b'V', b'0', b'1']);
    }

    #[test]
    fn subtitling_descriptor_multi_emits_two_entries() {
        let descriptor =
            subtitling_descriptor_multi(&[(*b"eng", 0x10, 1, 1), (*b"spa", 0x10, 2, 2)])
                .expect("non-empty entries");
        // tag(1) + length(1) + 2 × 8 bytes per entry = 18 bytes total.
        assert_eq!(descriptor[0], 0x59, "tag");
        assert_eq!(descriptor[1], 0x10, "length = 16 (2 × 8 byte entries)");
        // First entry: lang(3) + subtitling_type(1) + comp_page_id(2) + anc_page_id(2).
        assert_eq!(&descriptor[2..5], b"eng");
        assert_eq!(descriptor[5], 0x10);
        assert_eq!(&descriptor[6..8], &[0x00, 0x01]);
        assert_eq!(&descriptor[8..10], &[0x00, 0x01]);
        // Second entry.
        assert_eq!(&descriptor[10..13], b"spa");
        assert_eq!(descriptor[13], 0x10);
        assert_eq!(&descriptor[14..16], &[0x00, 0x02]);
        assert_eq!(&descriptor[16..18], &[0x00, 0x02]);
        assert_eq!(descriptor.len(), 18);
    }

    #[test]
    #[allow(clippy::identity_op)] // spec form (teletext_type << 3) | magazine_number is illustrative
    fn teletext_descriptor_multi_emits_two_entries() {
        let descriptor =
            teletext_descriptor_multi(&[(*b"eng", 0x02, 0, 0x88), (*b"spa", 0x02, 0, 0x77)])
                .expect("non-empty entries");
        // tag(1) + length(1) + 2 × 5 bytes per entry = 12 bytes total.
        assert_eq!(descriptor[0], 0x56, "tag");
        assert_eq!(descriptor[1], 0x0A, "length = 10 (2 × 5 byte entries)");
        assert_eq!(&descriptor[2..5], b"eng");
        assert_eq!(descriptor[5], (0x02 << 3) | 0x00); // teletext_type | magazine_number
        assert_eq!(descriptor[6], 0x88);
        assert_eq!(&descriptor[7..10], b"spa");
        assert_eq!(descriptor[10], (0x02 << 3) | 0x00);
        assert_eq!(descriptor[11], 0x77);
        assert_eq!(descriptor.len(), 12);
    }

    #[test]
    fn subtitling_descriptor_single_via_multi_matches_single_helper() {
        // The single-entry helper's output should match a 1-element multi call.
        let single = subtitling_descriptor(*b"eng", 0x10, 1, 1);
        let multi =
            subtitling_descriptor_multi(&[(*b"eng", 0x10, 1, 1)]).expect("non-empty entries");
        assert_eq!(single, multi);
    }

    #[test]
    fn teletext_descriptor_single_via_multi_matches_single_helper() {
        let single = teletext_descriptor(*b"eng", 0x02, 0, 0x88);
        let multi =
            teletext_descriptor_multi(&[(*b"eng", 0x02, 0, 0x88)]).expect("non-empty entries");
        assert_eq!(single, multi);
    }

    // ── Empty-entries rejection ──────────────────────────────────────────

    #[test]
    fn subtitling_descriptor_multi_rejects_empty_entries() {
        // Empty entries would produce a degenerate `0x59 0x00` descriptor
        // that the demux parser rejects with DescriptorParseError::EmptyInput.
        // Encoder rejects symmetrically.
        let result = subtitling_descriptor_multi(&[]);
        assert_eq!(result, Err(DescriptorError::EmptyEntries { tag: 0x59 }));
    }

    #[test]
    fn teletext_descriptor_multi_rejects_empty_entries() {
        let result = teletext_descriptor_multi(&[]);
        assert_eq!(result, Err(DescriptorError::EmptyEntries { tag: 0x56 }));
    }

    #[test]
    fn subtitling_descriptor_multi_accepts_one_entry() {
        let bytes = subtitling_descriptor_multi(&[(*b"eng", 0x10, 0x0001, 0x0002)])
            .expect("one entry should succeed");
        assert_eq!(bytes[0], 0x59); // descriptor_tag
        assert_eq!(bytes[1], 8); // length = 1 entry × 8 bytes
    }

    #[test]
    fn teletext_descriptor_multi_accepts_one_entry() {
        let bytes = teletext_descriptor_multi(&[(*b"eng", 0x02, 0, 0x88)])
            .expect("one entry should succeed");
        assert_eq!(bytes[0], 0x56); // descriptor_tag
        assert_eq!(bytes[1], 5); // length = 1 entry × 5 bytes
    }

    #[test]
    fn descriptor_error_display_unchanged() {
        assert_eq!(
            DescriptorError::EmptyEntries { tag: 0x59 }.to_string(),
            "descriptor tag 0x59: entries slice is empty (must be non-empty)"
        );
        assert_eq!(
            DescriptorError::EmptyEntries { tag: 0x56 }.to_string(),
            "descriptor tag 0x56: entries slice is empty (must be non-empty)"
        );
    }

    #[test]
    fn descriptor_error_too_large_display() {
        assert_eq!(
            DescriptorError::TooLarge {
                tag: 0x05,
                len: 252,
                max: 251,
            }
            .to_string(),
            "descriptor tag 0x05: payload length 252 exceeds spec maximum of 251 bytes"
        );
    }

    #[test]
    fn subtitling_descriptor_multi_rejects_too_many_entries() {
        // 32 entries × 8 bytes = 256 byte body — one byte over the
        // u8 descriptor_length ceiling.
        let entries: Vec<([u8; 3], u8, u16, u16)> =
            (0..32).map(|_| (*b"eng", 0x10, 0, 0)).collect();
        let err = subtitling_descriptor_multi(&entries).unwrap_err();
        assert_eq!(
            err,
            DescriptorError::TooLarge {
                tag: 0x59,
                len: 256,
                max: 255,
            }
        );
    }

    #[test]
    fn subtitling_descriptor_multi_accepts_31_entries() {
        // 31 × 8 = 248 bytes body, at the spec edge.
        let entries: Vec<([u8; 3], u8, u16, u16)> =
            (0..31).map(|_| (*b"eng", 0x10, 0, 0)).collect();
        let bytes = subtitling_descriptor_multi(&entries).expect("31 entries within cap");
        assert_eq!(bytes[0], 0x59);
        assert_eq!(bytes[1], 248);
    }

    #[test]
    fn teletext_descriptor_multi_rejects_too_many_entries() {
        // 52 entries × 5 bytes = 260 byte body, over the u8 cap.
        let entries: Vec<([u8; 3], u8, u8, u8)> = (0..52).map(|_| (*b"eng", 0x02, 0, 0)).collect();
        let err = teletext_descriptor_multi(&entries).unwrap_err();
        assert_eq!(
            err,
            DescriptorError::TooLarge {
                tag: 0x56,
                len: 260,
                max: 255,
            }
        );
    }

    #[test]
    fn teletext_descriptor_multi_accepts_51_entries() {
        // 51 × 5 = 255 bytes body, exactly at the u8 ceiling.
        let entries: Vec<([u8; 3], u8, u8, u8)> = (0..51).map(|_| (*b"eng", 0x02, 0, 0)).collect();
        let bytes = teletext_descriptor_multi(&entries).expect("51 entries within cap");
        assert_eq!(bytes[0], 0x56);
        assert_eq!(bytes[1], 255);
    }

    #[test]
    fn ac3_audio_stream_descriptor_canonical_48khz_stereo_192kbps() {
        // sample_rate_code=0 (48kHz), bsid=8, bit_rate_code=10 (192 kbps,
        // MSB=0 = exact), surround_mode=0 (not indicated), bsmod=0 (CM),
        // num_channels=0b0010 (2/0 stereo), full_svc=1.
        let bytes = ac3_audio_stream_descriptor(0, 8, 10, 0, 0, 0b0010, true);
        assert_eq!(bytes.len(), 5);
        assert_eq!(bytes[0], 0x81); // descriptor_tag (A/52 §A.4.3)
        assert_eq!(bytes[1], 0x03); // descriptor_length
        // byte0: sample_rate_code(3) << 5 | bsid(5) = (0<<5)|8 = 0x08
        assert_eq!(bytes[2], 0x08);
        // byte1: bit_rate_code(6) << 2 | surround_mode(2) = (10<<2)|0 = 0x28
        assert_eq!(bytes[3], 0x28);
        // byte2: bsmod(3) << 5 | num_channels(4) << 1 | full_svc(1)
        //      = (0<<5)|(2<<1)|1 = 0x05
        assert_eq!(bytes[4], 0x05);
    }

    #[test]
    fn ac3_audio_stream_descriptor_packs_all_max_values() {
        // Exercise upper bits of every field — confirm no overflow into
        // adjacent fields. sample_rate_code=7, bsid=31, bit_rate_code=63
        // (MSB=1 = upper limit; lower 5 bits = 31 → 640 kbps),
        // surround_mode=3 (reserved but bit-legal), bsmod=7 (VO),
        // num_channels=15 (reserved but bit-legal), full_svc=0.
        let bytes = ac3_audio_stream_descriptor(7, 31, 63, 3, 7, 15, false);
        assert_eq!(bytes.len(), 5);
        assert_eq!(bytes[0], 0x81);
        assert_eq!(bytes[1], 0x03);
        // (7<<5)|31 = 0xFF
        assert_eq!(bytes[2], 0xFF);
        // (63<<2)|3 = 0xFF
        assert_eq!(bytes[3], 0xFF);
        // (7<<5)|(15<<1)|0 = 0xFE
        assert_eq!(bytes[4], 0xFE);
    }
}
