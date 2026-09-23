//! ST 0601 pack & list substrate — parse/emit for the tags whose
//! wire value is a small positional structure (a "Defined-Length Pack" /
//! "Variable-Length Pack", ST 0107 terminology) or a flat list, rather
//! than one scalar. Dispatch lives in `decode.rs`/`encode.rs` via the
//! `Encoding::Pack` marker (`tags.rs`); this module owns the per-tag wire
//! shape only.
//!
//! Tags covered here: the simple DLP packs — 81 (Image Horizon
//! Pixels), 115 (Control Command, MULTI-INSTANCE), 116 (Control Command
//! Verification List), 121 (Active Wavelength List), 127 (Sensor Frame
//! Rate Pack), 143 (Metadata Substream Id) — plus the VLP series
//! packs: 122 (Country Codes), 128 (Wavelengths List), 130 (Airbase
//! Locations), 138 (Payload List), 140 (Weapons Stores), 141 (Waypoint
//! List), 142 (View Domain). Tag 102 (SDCC-FLP, `UasDatalinkLs::sdcc_flps`)
//! is NOT here: its wire-format parser lives in `klv::st1010`
//! instead (a general-purpose MISB construct, not ST 0601-specific), and
//! its multi-instance positional-capture dispatch lives directly in
//! `decode.rs`/`encode.rs` rather than a `packs::parse_*`/`emit_*` pair.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{KlvDecodeError, KlvEncodeError, KlvFieldError};
use crate::klv::imapb::{
    DecodedImapb, ImapbParams, ImapbSpecial, decode_imapb, encode_imapb, encode_imapb_special,
};
use crate::klv::length::{
    ber_len, ber_oid_len_u64, read_ber, read_ber_oid_u64, read_var_uint, var_uint_min_len,
    write_ber, write_ber_oid_u64, write_var_uint_min,
};

use super::mapping::{decode_fixed_range, encode_fixed_range};
use super::model::OutOfRangePolicy;
use super::tags::LinearRange;

const LAT_RANGE: LinearRange = LinearRange {
    signed: true,
    byte_length: 4,
    min: -90.0,
    max: 90.0,
};
const LON_RANGE: LinearRange = LinearRange {
    signed: true,
    byte_length: 4,
    min: -180.0,
    max: 180.0,
};

/// Map a substrate [`KlvDecodeError`] (BER / BER-OID framing failure) to
/// the [`KlvFieldError`] this module's parse functions return — every
/// framing failure means the same thing at this level: the pack could
/// not be parsed. Sibling of `klv::st1010::substrate_err`.
fn truncated(tag: u32) -> impl FnOnce(KlvDecodeError) -> KlvFieldError {
    move |_| KlvFieldError::TruncatedField { tag }
}

/// Item 81: Image Horizon Pixels (ST 0601.19 §8.81) — screen-space
/// horizon line endpoints as percentages of image width/height, plus an
/// optional geodetic pair for each endpoint.
///
/// Wire shape (DLP, ST 0107 truncatable-trailing convention): 4
/// mandatory percentage bytes, then 0-4 further int32 linear-mapped
/// fields in strict order (`start_lat`, `start_lon`, `end_lat`,
/// `end_lon`) — the pack may end after any of them. A wire int32 exactly
/// at `INT_MIN` (`0x80000000`) on any of the four trailing fields is the
/// item's spec-defined "error" indicator (§8.81): that field decodes to
/// `None` just like a truncated-away field, but the byte position IS
/// consumed (parsing continues to the next field).
///
/// **Encode semantics:** a `None` field that lies BEFORE the last `Some`
/// field (in wire order) is re-encoded as the `INT_MIN` sentinel — the
/// spec's own error indicator, which decode maps straight back to `None`
/// — so a later `Some` field is never silently dropped just because an
/// earlier optional field is absent. Only a `None` AFTER the last `Some`
/// (or all four `None`) truncates the pack there, matching decode's own
/// truncation semantics: there is no wire difference between "never
/// sent" and "sentinel would be redundant here" once the pack has ended.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ImageHorizonPixels {
    pub x0_pct: u8,
    pub y0_pct: u8,
    pub x1_pct: u8,
    pub y1_pct: u8,
    pub start_lat_deg: Option<f64>,
    pub start_lon_deg: Option<f64>,
    pub end_lat_deg: Option<f64>,
    pub end_lon_deg: Option<f64>,
}

/// The four optional trailing fields of [`ImageHorizonPixels`], in wire
/// order, paired with the `LinearRange` each one decodes/encodes with.
/// Shared between [`parse_image_horizon`], [`emit_image_horizon`], and
/// [`image_horizon_len`] so the truncation-stop order can't drift between
/// them.
const IMAGE_HORIZON_OPTIONAL_RANGES: [LinearRange; 4] =
    [LAT_RANGE, LON_RANGE, LAT_RANGE, LON_RANGE];

fn image_horizon_optional_values(h: &ImageHorizonPixels) -> [Option<f64>; 4] {
    [
        h.start_lat_deg,
        h.start_lon_deg,
        h.end_lat_deg,
        h.end_lon_deg,
    ]
}

fn image_horizon_optional_setters() -> [fn(&mut ImageHorizonPixels, Option<f64>); 4] {
    [
        |h, v| h.start_lat_deg = v,
        |h, v| h.start_lon_deg = v,
        |h, v| h.end_lat_deg = v,
        |h, v| h.end_lon_deg = v,
    ]
}

pub(crate) fn parse_image_horizon(bytes: &[u8]) -> Result<ImageHorizonPixels, KlvFieldError> {
    if bytes.len() < 4 {
        return Err(KlvFieldError::TruncatedField { tag: 81 });
    }
    let mut h = ImageHorizonPixels {
        x0_pct: bytes[0],
        y0_pct: bytes[1],
        x1_pct: bytes[2],
        y1_pct: bytes[3],
        ..ImageHorizonPixels::default()
    };
    let rest = &bytes[4..];
    let setters = image_horizon_optional_setters();
    let mut offset = 0usize;
    for (range, setter) in IMAGE_HORIZON_OPTIONAL_RANGES.into_iter().zip(setters) {
        if offset + 4 > rest.len() {
            break; // clean truncation — no more optional fields on the wire
        }
        let v = decode_fixed_range(&range, 81, &rest[offset..offset + 4])?;
        setter(&mut h, v);
        offset += 4;
    }
    if offset != rest.len() {
        // Leftover bytes that don't align to a full 4-byte optional
        // field — not a clean truncation, matches the malformed-scalar
        // policy for over-long values.
        return Err(KlvFieldError::InvalidLength {
            tag: 81,
            expected: 4 + offset,
            got: bytes.len(),
        });
    }
    Ok(h)
}

/// Index of the last `Some` among the four optional trailing fields, or
/// `None` if all four are absent. Every optional field up to and
/// including this index gets a wire slot (sentinel-filled where the
/// field itself is `None`); shared by [`image_horizon_len`] and
/// [`emit_image_horizon`] so their stopping point can't drift apart.
fn image_horizon_last_some(h: &ImageHorizonPixels) -> Option<usize> {
    image_horizon_optional_values(h)
        .iter()
        .rposition(|v| v.is_some())
}

pub(crate) fn image_horizon_len(h: &ImageHorizonPixels) -> usize {
    match image_horizon_last_some(h) {
        Some(last) => 4 + 4 * (last + 1),
        None => 4,
    }
}

pub(crate) fn emit_image_horizon(
    h: &ImageHorizonPixels,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    out.push(h.x0_pct);
    out.push(h.y0_pct);
    out.push(h.x1_pct);
    out.push(h.y1_pct);
    let Some(last) = image_horizon_last_some(h) else {
        return Ok(()); // no optional fields at all
    };
    for (i, (value, range)) in image_horizon_optional_values(h)
        .into_iter()
        .zip(IMAGE_HORIZON_OPTIONAL_RANGES)
        .enumerate()
    {
        if i > last {
            break; // trailing None(s) after the last Some — clean truncation
        }
        let mut buf = [0u8; 4];
        match value {
            Some(v) => encode_fixed_range(&range, 81, v, &mut buf, OutOfRangePolicy::Error)?,
            // Interior None (before the last Some): fill with the
            // spec's own INT_MIN "error" indicator (§8.81) rather than
            // dropping this slot — decode maps it straight back to
            // `None`, so the round trip is lossless for the Some
            // fields that follow.
            None => buf = i32::MIN.to_be_bytes(),
        }
        out.extend_from_slice(&buf);
    }
    Ok(())
}

/// Item 115: Control Command (ST 0601.19 §8.115) — one command sent to
/// the platform. MULTI-INSTANCE per ST 0601.19 Table 1 ("Multiples
/// Allowed" = Yes): every wire occurrence of Tag 115 appends one
/// `ControlCommand` to `UasDatalinkLs::control_commands` rather than
/// overwriting a single field.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlCommand {
    /// BER-OID command id.
    pub id: u64,
    /// Command text, UTF-8, at most 127 bytes.
    pub command: String,
    /// Time the command was issued/executed, microseconds — MISB
    /// var-length truncatable trailing field (present iff the wire pack
    /// has trailing bytes after `command`).
    pub time_us: Option<u64>,
}

/// Wire shape: `id (BER-OID, M)` + `string_len (BER) + command (UTF-8,
/// M, <=127 bytes)` + `[time_us (MISB var-length truncatable, 1..=8
/// bytes)]`.
pub(crate) fn parse_control_command(bytes: &[u8]) -> Result<ControlCommand, KlvFieldError> {
    let (id, rest) = read_ber_oid_u64(bytes).map_err(truncated(115))?;
    let (str_len, rest) = read_ber(rest).map_err(truncated(115))?;
    if str_len > 127 {
        return Err(KlvFieldError::InvalidLength {
            tag: 115,
            expected: 127,
            got: str_len,
        });
    }
    if rest.len() < str_len {
        return Err(KlvFieldError::TruncatedField { tag: 115 });
    }
    let (str_bytes, rest) = (&rest[..str_len], &rest[str_len..]);
    let command = core::str::from_utf8(str_bytes)
        .map_err(|_| KlvFieldError::InvalidUtf8 { tag: 115 })?
        .to_owned();
    let time_us = match rest.len() {
        0 => None,
        1..=8 => Some(read_var_uint(rest, 8, 115)?),
        got => {
            return Err(KlvFieldError::InvalidLength {
                tag: 115,
                expected: 8,
                got,
            });
        }
    };
    Ok(ControlCommand {
        id,
        command,
        time_us,
    })
}

/// Wire byte length [`emit_control_command`] would write for `cmd`,
/// without allocating — the sizing sibling used by the Tag 115
/// multi-instance special-case in `encode.rs`.
pub(crate) fn control_command_len(cmd: &ControlCommand) -> usize {
    ber_oid_len_u64(cmd.id)
        + ber_len(cmd.command.len())
        + cmd.command.len()
        + cmd.time_us.map(var_uint_min_len).unwrap_or(0)
}

pub(crate) fn emit_control_command(
    cmd: &ControlCommand,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    if cmd.command.len() > 127 {
        return Err(KlvEncodeError::StringTooLong { tag: 115, max: 127 });
    }
    let mut id_buf = [0u8; 10];
    let n = write_ber_oid_u64(cmd.id, &mut id_buf)?;
    out.extend_from_slice(&id_buf[..n]);
    let mut len_buf = [0u8; 9];
    let m = write_ber(cmd.command.len(), &mut len_buf)?;
    out.extend_from_slice(&len_buf[..m]);
    out.extend_from_slice(cmd.command.as_bytes());
    if let Some(t) = cmd.time_us {
        out.extend_from_slice(&write_var_uint_min(t));
    }
    Ok(())
}

/// Parse a Tag 116/121 value: a flat sequence of BER-OID ids, consumed
/// until the value bytes are exhausted (no per-item length prefix).
pub(crate) fn parse_id_list(bytes: &[u8], tag: u32) -> Result<Vec<u64>, KlvFieldError> {
    let mut ids = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (id, r) = read_ber_oid_u64(rest).map_err(truncated(tag))?;
        ids.push(id);
        rest = r;
    }
    Ok(ids)
}

pub(crate) fn id_list_len(ids: &[u64]) -> usize {
    ids.iter().map(|&id| ber_oid_len_u64(id)).sum()
}

pub(crate) fn emit_id_list(ids: &[u64], out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    for &id in ids {
        let n = write_ber_oid_u64(id, &mut buf)?;
        out.extend_from_slice(&buf[..n]);
    }
    Ok(())
}

/// Item 127: Sensor Frame Rate Pack (ST 0601.19 §8.127) — frame rate as a
/// numerator/denominator ratio; `denominator` defaults to 1 when the wire
/// pack ends after the numerator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorFrameRate {
    /// BER-OID numerator.
    pub numerator: u64,
    /// BER-OID denominator; wire-absent means 1 (whole-number fps).
    pub denominator: u64,
}

impl SensorFrameRate {
    /// Frames per second as `numerator / denominator`.
    pub fn fps(&self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }
}

pub(crate) fn parse_sensor_frame_rate(bytes: &[u8]) -> Result<SensorFrameRate, KlvFieldError> {
    let (numerator, rest) = read_ber_oid_u64(bytes).map_err(truncated(127))?;
    let denominator = if rest.is_empty() {
        1
    } else {
        let (d, rest2) = read_ber_oid_u64(rest).map_err(truncated(127))?;
        if !rest2.is_empty() {
            return Err(KlvFieldError::InvalidLength {
                tag: 127,
                expected: bytes.len() - rest2.len(),
                got: bytes.len(),
            });
        }
        d
    };
    Ok(SensorFrameRate {
        numerator,
        denominator,
    })
}

/// Canonical wire length: the denominator is only emitted when it isn't
/// the default (1) — matches [`emit_sensor_frame_rate`].
pub(crate) fn sensor_frame_rate_len(fr: &SensorFrameRate) -> usize {
    ber_oid_len_u64(fr.numerator)
        + if fr.denominator == 1 {
            0
        } else {
            ber_oid_len_u64(fr.denominator)
        }
}

pub(crate) fn emit_sensor_frame_rate(
    fr: &SensorFrameRate,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(fr.numerator, &mut buf)?;
    out.extend_from_slice(&buf[..n]);
    if fr.denominator != 1 {
        let n2 = write_ber_oid_u64(fr.denominator, &mut buf)?;
        out.extend_from_slice(&buf[..n2]);
    }
    Ok(())
}

/// Item 143: Metadata Substream Id (ST 0601.19 §8.143) — identifies
/// which metadata substream this Local Set belongs to. Per §8.143, `uuid`
/// is REQUIRED when `local_id == 0` and OMITTED when `local_id > 0`; this
/// decoder is lenient and stores whatever combination the wire carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataSubstreamId {
    /// BER-OID local substream id.
    pub local_id: u64,
    /// RFC 4122 UUID bytes, present per the §8.143 rule above.
    pub uuid: Option<[u8; 16]>,
}

pub(crate) fn parse_metadata_substream_id(
    bytes: &[u8],
) -> Result<MetadataSubstreamId, KlvFieldError> {
    let (local_id, rest) = read_ber_oid_u64(bytes).map_err(truncated(143))?;
    let uuid = match rest.len() {
        0 => None,
        16 => {
            let mut u = [0u8; 16];
            u.copy_from_slice(rest);
            Some(u)
        }
        got => {
            return Err(KlvFieldError::InvalidLength {
                tag: 143,
                expected: 16,
                got,
            });
        }
    };
    Ok(MetadataSubstreamId { local_id, uuid })
}

pub(crate) fn metadata_substream_id_len(ms: &MetadataSubstreamId) -> usize {
    ber_oid_len_u64(ms.local_id) + if ms.uuid.is_some() { 16 } else { 0 }
}

pub(crate) fn emit_metadata_substream_id(
    ms: &MetadataSubstreamId,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(ms.local_id, &mut buf)?;
    out.extend_from_slice(&buf[..n]);
    if let Some(uuid) = ms.uuid {
        out.extend_from_slice(&uuid);
    }
    Ok(())
}

// ============================================================================
// Task C3 shared substrate: `[BER length][value bytes]` fields
// ============================================================================

/// Read one `[BER length][value bytes]` field, returning `(value_bytes,
/// rest)`. Shared substrate for every VLP that precedes each sub-value
/// (or sub-record) with its own BER length: Country Codes (§8.122),
/// Wavelengths List / Payload List / Weapons Stores / Waypoint List
/// record framing (§8.128/.138/.140/.141), and Airbase Locations'
/// per-site framing (§8.130).
fn read_len_prefixed(bytes: &[u8], tag: u32) -> Result<(&[u8], &[u8]), KlvFieldError> {
    let (len, rest) = read_ber(bytes).map_err(truncated(tag))?;
    if rest.len() < len {
        return Err(KlvFieldError::TruncatedField { tag });
    }
    Ok((&rest[..len], &rest[len..]))
}

/// Accumulate a byte slice as a big-endian unsigned integer (empty ⇒ 0).
fn be_uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b))
}

/// Interpret a `[BER length][value bytes]` UTF-8 field: length 0 means
/// "unknown" (ST 0107.5 §6.3.3.2's absent-value convention, reused
/// per-field here rather than per-tag).
fn len_prefixed_utf8(bytes: &[u8], tag: u32) -> Result<Option<String>, KlvFieldError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        core::str::from_utf8(bytes)
            .map_err(|_| KlvFieldError::InvalidUtf8 { tag })?
            .to_owned(),
    ))
}

/// Decode a MANDATORY IMAPB field, mapping any non-[`DecodedImapb::Value`]
/// outcome (special / reserved-special / out-of-range) to an error.
/// Sibling of [`decode_imapb_optional`] for fields that have no side
/// channel to carry a producer-signaled special.
fn decode_imapb_required(p: &ImapbParams, bytes: &[u8], tag: u32) -> Result<f64, KlvFieldError> {
    match decode_imapb(p, bytes)? {
        DecodedImapb::Value(v) => Ok(v),
        DecodedImapb::OutOfRange { decoded } => Err(KlvFieldError::OutOfRange {
            tag,
            value: decoded,
            min: p.min,
            max: p.max,
        }),
        DecodedImapb::Special(_) | DecodedImapb::ReservedSpecial { .. } => {
            Err(KlvFieldError::OutOfRange {
                tag,
                value: f64::NAN,
                min: p.min,
                max: p.max,
            })
        }
    }
}

/// Decode an OPTIONAL IMAPB field, mapping any non-`Value` outcome
/// (special / reserved-special / out-of-range) to `None` rather than an
/// error. Used by [`parse_location`], which repurposes the IMAPB
/// special-value space as a filler for an "interior absent" field a
/// later field's presence forces onto the wire (see that function's
/// rustdoc) — there is no separate side channel to carry which special
/// was signaled, so any of the three non-`Value` outcomes collapses to
/// the same `None`.
fn decode_imapb_optional(p: &ImapbParams, bytes: &[u8]) -> Result<Option<f64>, KlvFieldError> {
    Ok(match decode_imapb(p, bytes)? {
        DecodedImapb::Value(v) => Some(v),
        DecodedImapb::Special(_)
        | DecodedImapb::ReservedSpecial { .. }
        | DecodedImapb::OutOfRange { .. } => None,
    })
}

/// Item 122: Country Codes (ST 0601.19 §8.122) — country-code metadata
/// about the platform's operation and manufacture. VLP of four
/// `[BER length][value]` fields in strict order: `coding_method`
/// (mandatory uint — an enumeration from MISB ST 0102 Table 2 Item 12),
/// `overflight` (mandatory utf8 — though its own length may be 0,
/// meaning "unknown"), `operator` (optional utf8), `manufacture`
/// (optional utf8).
///
/// Per §8.122.1: "if one of the country values is unknown, set the
/// length for the country code to zero (0) and do not include the
/// country code string" — length-0 always means "unknown", never a
/// distinct empty string. Truncation removes a length-value pair
/// ENTIRELY from the end ("When truncating a value, the length-value
/// pair are both removed") — distinct from the length-0 marker, which
/// still writes a (zero) length byte. Re-encode canonicalizes a
/// length-0 `manufacture` immediately followed by nothing else down to
/// plain truncation (no `operator`/`manufacture` bytes at all) when
/// both are absent; an absent `operator` with a present `manufacture`
/// still gets its own length-0 marker so `manufacture` is never
/// silently swallowed by truncation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CountryCodes {
    pub coding_method: u64,
    pub overflight: Option<String>,
    pub operator: Option<String>,
    pub manufacture: Option<String>,
}

/// Index of the last `Some` among `[operator, manufacture]`, or `None`
/// if both are absent. Shared by [`country_codes_len`] and
/// [`emit_country_codes`] so their stopping point can't drift apart —
/// same pattern as `image_horizon_last_some`.
fn country_codes_last_some(cc: &CountryCodes) -> Option<usize> {
    [cc.operator.is_some(), cc.manufacture.is_some()]
        .iter()
        .rposition(|&b| b)
}

pub(crate) fn parse_country_codes(bytes: &[u8]) -> Result<CountryCodes, KlvFieldError> {
    let (coding_bytes, rest) = read_len_prefixed(bytes, 122)?;
    let coding_method = be_uint(coding_bytes);
    let (overflight_bytes, rest) = read_len_prefixed(rest, 122)?;
    let overflight = len_prefixed_utf8(overflight_bytes, 122)?;
    let mut cc = CountryCodes {
        coding_method,
        overflight,
        operator: None,
        manufacture: None,
    };
    if rest.is_empty() {
        return Ok(cc);
    }
    let (operator_bytes, rest) = read_len_prefixed(rest, 122)?;
    cc.operator = len_prefixed_utf8(operator_bytes, 122)?;
    if rest.is_empty() {
        return Ok(cc);
    }
    let (manufacture_bytes, rest) = read_len_prefixed(rest, 122)?;
    cc.manufacture = len_prefixed_utf8(manufacture_bytes, 122)?;
    if !rest.is_empty() {
        return Err(KlvFieldError::InvalidLength {
            tag: 122,
            expected: bytes.len() - rest.len(),
            got: bytes.len(),
        });
    }
    Ok(cc)
}

pub(crate) fn country_codes_len(cc: &CountryCodes) -> usize {
    let coding_bytes_len = var_uint_min_len(cc.coding_method);
    let mut n = ber_len(coding_bytes_len)
        + coding_bytes_len
        + ber_len(str_len_opt(&cc.overflight))
        + str_len_opt(&cc.overflight);
    if let Some(last) = country_codes_last_some(cc) {
        let opts = [&cc.operator, &cc.manufacture];
        for opt in &opts[..=last] {
            let l = str_len_opt(opt);
            n += ber_len(l) + l;
        }
    }
    n
}

fn str_len_opt(opt: &Option<String>) -> usize {
    opt.as_ref().map(String::len).unwrap_or(0)
}

pub(crate) fn emit_country_codes(
    cc: &CountryCodes,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    emit_len_prefixed(&write_var_uint_min(cc.coding_method), out)?;
    emit_len_prefixed(cc.overflight.as_deref().unwrap_or("").as_bytes(), out)?;
    let Some(last) = country_codes_last_some(cc) else {
        return Ok(());
    };
    let opts = [&cc.operator, &cc.manufacture];
    for opt in &opts[..=last] {
        emit_len_prefixed(opt.as_deref().unwrap_or("").as_bytes(), out)?;
    }
    Ok(())
}

fn emit_len_prefixed(value: &[u8], out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut len_buf = [0u8; 9];
    let n = write_ber(value.len(), &mut len_buf)?;
    out.extend_from_slice(&len_buf[..n]);
    out.extend_from_slice(value);
    Ok(())
}

// ============================================================================
// `Location` — shared by Tag 130 (Airbase Locations) and Tag 141
// (Waypoint List)
// ============================================================================

/// A WGS84 geodetic point: latitude, longitude, and Height Above
/// Ellipsoid (HAE), each ST 1201.5 IMAPB-encoded. Shared substrate for
/// Item 130 (Airbase Locations, ST 0601.19 §8.130) and the per-waypoint
/// location in Item 141 (Waypoint List, §8.141).
///
/// Wire shape (truncatable DLP, per §8.130.1 bullet 4): `lat`
/// IMAPB(-90,90,4) + `lon` IMAPB(-180,180,4) + `hae` IMAPB(-900,9000,3)
/// (NB 9000 max, not the 40000 used by the ST 0601 items with their own
/// tag — this is the ST 0601.19 Table 16 range, distinct from e.g. Item
/// 113's Altitude AGL). `lat`/`lon` are mandatory once any bytes are
/// present; only the trailing `hae` is truncatable.
///
/// **Encode semantics for an interior-absent field** (e.g. `lat: None`
/// while `hae: Some(..)`): IMAPB has no INT_MIN-style sentinel of its
/// own, so this repurposes the ST 1201.5 special-value space —
/// specifically `ImapbSpecial::UserDefined { signal: 0 }` — as an
/// "absent" filler at that field's wire position, matching decode's
/// `decode_imapb_optional`, which maps ANY special/reserved/out-of-range
/// pattern back to `None`. This is spec-legal (§7.2.3's special-value
/// space is defined at the IMAPB layer, independent of the ST 0601 item
/// using it) even though it is not itself a case ST 0601.19 §8.130
/// describes — the spec only ever truncates `hae` from the end, never
/// leaves `lat`/`lon` absent with `hae` present. Only a trailing `None`
/// (or all three `None`) truncates the pack.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Location {
    pub lat_deg: Option<f64>,
    pub lon_deg: Option<f64>,
    pub hae_m: Option<f64>,
}

const LOCATION_PARAMS: [ImapbParams; 3] = [
    ImapbParams {
        min: -90.0,
        max: 90.0,
        length: 4,
    },
    ImapbParams {
        min: -180.0,
        max: 180.0,
        length: 4,
    },
    ImapbParams {
        min: -900.0,
        max: 9000.0,
        length: 3,
    },
];

/// Wire byte length that would be occupied by `loc`'s BOTH-OR-NEITHER
/// `lat`/`lon` pair plus, if present, the trailing `hae` — one of `0`
/// (fully absent), `8` (pair only), or `11` (pair + HAE). ST 0601.19
/// Table 16 marks Latitude and Longitude Mandatory and HAE Optional:
/// only HAE truncates independently, the pair does not. Shared by
/// [`location_len`] and [`emit_location`] so their stopping point can't
/// drift apart, and by [`parse_location`]'s valid-length check.
fn location_wire_len(loc: &Location) -> usize {
    if loc.hae_m.is_some() {
        11
    } else if loc.lat_deg.is_some() || loc.lon_deg.is_some() {
        8
    } else {
        0
    }
}

/// Whether `n` is one of the three valid [`Location`] wire lengths —
/// `0`, `8`, or `11` (see [`location_wire_len`]). Callers combining a
/// `Location` with a preceding self-delimiting field (e.g. [`Waypoint`]'s
/// `info`) can check this BEFORE attempting to consume that field: no
/// two members of `{0, 8, 11}` differ by exactly 1 byte, so a 1-byte
/// `info` value can never be mistaken for part of a `Location`.
fn is_valid_location_len(n: usize) -> bool {
    matches!(n, 0 | 8 | 11)
}

fn emit_location_field(
    value: Option<f64>,
    params: &ImapbParams,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 4];
    match value {
        Some(v) => encode_imapb(params, v, &mut buf[..params.length])?,
        // Interior-absent filler — see the struct rustdoc. Only `lat`/
        // `lon` can hit this arm (the pair is transmitted as a unit
        // once either is `Some`); `hae` is only ever passed `Some` here.
        None => encode_imapb_special(
            ImapbSpecial::UserDefined { signal: 0 },
            params.length,
            &mut buf[..params.length],
        )?,
    }
    out.extend_from_slice(&buf[..params.length]);
    Ok(())
}

pub(crate) fn parse_location(bytes: &[u8], tag: u32) -> Result<Location, KlvFieldError> {
    if !is_valid_location_len(bytes.len()) {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: 11,
            got: bytes.len(),
        });
    }
    if bytes.is_empty() {
        return Ok(Location::default());
    }
    let lat_deg = decode_imapb_optional(&LOCATION_PARAMS[0], &bytes[0..4])?;
    let lon_deg = decode_imapb_optional(&LOCATION_PARAMS[1], &bytes[4..8])?;
    let hae_m = if bytes.len() == 11 {
        decode_imapb_optional(&LOCATION_PARAMS[2], &bytes[8..11])?
    } else {
        None
    };
    Ok(Location {
        lat_deg,
        lon_deg,
        hae_m,
    })
}

pub(crate) fn location_len(loc: &Location) -> usize {
    location_wire_len(loc)
}

pub(crate) fn emit_location(loc: &Location, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    if location_wire_len(loc) == 0 {
        return Ok(());
    }
    emit_location_field(loc.lat_deg, &LOCATION_PARAMS[0], out)?;
    emit_location_field(loc.lon_deg, &LOCATION_PARAMS[1], out)?;
    if let Some(hae) = loc.hae_m {
        emit_location_field(Some(hae), &LOCATION_PARAMS[2], out)?;
    }
    Ok(())
}

/// Item 130: Airbase Locations (ST 0601.19 §8.130) — the take-off and
/// recovery site locations. VLP: `[BER length][Location]` × 2 (take-off,
/// then recovery).
///
/// Per §8.130.1's bandwidth optimizations: a [`Location`] slot whose own
/// length is 0 decodes to `None` ("unknown"); recovery ABSENT from the
/// wire ENTIRELY (no second length-value pair at all, not even a
/// length-0 one) decodes to `Some(take_off)` — "if the Recovery Location
/// is absent then the Recovery Location is set equal to the Take-Off
/// location". Encode mirrors this: when `recovery == take_off` the
/// second pair is omitted entirely (canonicalizing bullet-1's
/// optimization); a `recovery` that differs from `take_off` — including
/// `None` while `take_off` is `Some` (deliberately unknown, not
/// "same-as-take-off") — always gets its own explicit pair, so the two
/// cases stay distinguishable through a round trip.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AirbaseLocations {
    pub take_off: Option<Location>,
    pub recovery: Option<Location>,
}

pub(crate) fn parse_airbase_locations(bytes: &[u8]) -> Result<AirbaseLocations, KlvFieldError> {
    let (loc1_bytes, rest) = read_len_prefixed(bytes, 130)?;
    let take_off = if loc1_bytes.is_empty() {
        None
    } else {
        Some(parse_location(loc1_bytes, 130)?)
    };
    if rest.is_empty() {
        // Recovery entirely absent -> same as take-off (§8.130.1 bullet 1).
        return Ok(AirbaseLocations {
            take_off,
            recovery: take_off,
        });
    }
    let (loc2_bytes, rest) = read_len_prefixed(rest, 130)?;
    if !rest.is_empty() {
        return Err(KlvFieldError::InvalidLength {
            tag: 130,
            expected: bytes.len() - rest.len(),
            got: bytes.len(),
        });
    }
    let recovery = if loc2_bytes.is_empty() {
        None
    } else {
        Some(parse_location(loc2_bytes, 130)?)
    };
    Ok(AirbaseLocations { take_off, recovery })
}

pub(crate) fn airbase_locations_len(al: &AirbaseLocations) -> usize {
    let take_off_len = al.take_off.map(|loc| location_len(&loc)).unwrap_or(0);
    let mut n = ber_len(take_off_len) + take_off_len;
    if al.recovery != al.take_off {
        let recovery_len = al.recovery.map(|loc| location_len(&loc)).unwrap_or(0);
        n += ber_len(recovery_len) + recovery_len;
    }
    n
}

pub(crate) fn emit_airbase_locations(
    al: &AirbaseLocations,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let take_off_len = al.take_off.map(|loc| location_len(&loc)).unwrap_or(0);
    let mut len_buf = [0u8; 9];
    let n = write_ber(take_off_len, &mut len_buf)?;
    out.extend_from_slice(&len_buf[..n]);
    if let Some(loc) = al.take_off {
        emit_location(&loc, out)?;
    }
    if al.recovery != al.take_off {
        let recovery_len = al.recovery.map(|loc| location_len(&loc)).unwrap_or(0);
        let n2 = write_ber(recovery_len, &mut len_buf)?;
        out.extend_from_slice(&len_buf[..n2]);
        if let Some(loc) = al.recovery {
            emit_location(&loc, out)?;
        }
    }
    Ok(())
}

// ============================================================================
// Item 128: Wavelengths List
// ============================================================================

/// One record of Item 128, Wavelengths List (ST 0601.19 §8.128) — a
/// sensor wavelength band definition. `min_nm`/`max_nm` are ST 1201.5
/// IMAPB(0,1e9,4)-encoded, giving ~½ nm precision across the full
/// X-ray-to-VHF spectrum span the spec cites.
#[derive(Debug, Clone, PartialEq)]
pub struct WavelengthRecord {
    pub id: u64,
    pub min_nm: f64,
    pub max_nm: f64,
    pub name: String,
}

const WAVELENGTH_IMAPB: ImapbParams = ImapbParams {
    min: 0.0,
    max: 1e9,
    length: 4,
};

fn parse_wavelength_record(bytes: &[u8]) -> Result<WavelengthRecord, KlvFieldError> {
    let (id, rest) = read_ber_oid_u64(bytes).map_err(truncated(128))?;
    if rest.len() < 8 {
        return Err(KlvFieldError::TruncatedField { tag: 128 });
    }
    let min_nm = decode_imapb_required(&WAVELENGTH_IMAPB, &rest[0..4], 128)?;
    let max_nm = decode_imapb_required(&WAVELENGTH_IMAPB, &rest[4..8], 128)?;
    let name = core::str::from_utf8(&rest[8..])
        .map_err(|_| KlvFieldError::InvalidUtf8 { tag: 128 })?
        .to_owned();
    Ok(WavelengthRecord {
        id,
        min_nm,
        max_nm,
        name,
    })
}

fn wavelength_record_len(w: &WavelengthRecord) -> usize {
    ber_oid_len_u64(w.id) + 4 + 4 + w.name.len()
}

fn emit_wavelength_record(w: &WavelengthRecord, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(w.id, &mut buf)?;
    out.extend_from_slice(&buf[..n]);
    let mut fbuf = [0u8; 4];
    encode_imapb(&WAVELENGTH_IMAPB, w.min_nm, &mut fbuf)?;
    out.extend_from_slice(&fbuf);
    encode_imapb(&WAVELENGTH_IMAPB, w.max_nm, &mut fbuf)?;
    out.extend_from_slice(&fbuf);
    out.extend_from_slice(w.name.as_bytes());
    Ok(())
}

/// Item 128 value: `[BER length][WavelengthRecord]` repeated until the
/// value bytes are exhausted — no leading count field (unlike Item 138).
pub(crate) fn parse_wavelengths_list(bytes: &[u8]) -> Result<Vec<WavelengthRecord>, KlvFieldError> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (rec_bytes, r) = read_len_prefixed(rest, 128)?;
        out.push(parse_wavelength_record(rec_bytes)?);
        rest = r;
    }
    Ok(out)
}

pub(crate) fn wavelengths_list_len(list: &[WavelengthRecord]) -> usize {
    list.iter()
        .map(|w| {
            let l = wavelength_record_len(w);
            ber_len(l) + l
        })
        .sum()
}

pub(crate) fn emit_wavelengths_list(
    list: &[WavelengthRecord],
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    for w in list {
        let l = wavelength_record_len(w);
        let mut len_buf = [0u8; 9];
        let n = write_ber(l, &mut len_buf)?;
        out.extend_from_slice(&len_buf[..n]);
        emit_wavelength_record(w, out)?;
    }
    Ok(())
}

// ============================================================================
// Item 138: Payload List
// ============================================================================

/// Item 138 §Table 17 Payload Type enumeration.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadType {
    ElectroOptical,
    Lidar,
    Radar,
    Sigint,
    Sar,
    /// Wire-unknown codepoint; round-trips byte-exact through encode.
    Other(u64),
}

impl PayloadType {
    /// Every variant, unit variants in declaration order plus one
    /// representative `Other`. Binding crates iterate this at test time to
    /// prove their mirrors cover every variant (they cannot `match`
    /// exhaustively across the crate boundary — `#[non_exhaustive]`).
    /// Completeness is pinned by the wildcard-free `match` in this
    /// module's `variant_inventory` tests.
    ///
    /// The `Other` payload here is an ARBITRARY representative, chosen only
    /// to be outside the mapped range so it round-trips; it carries no
    /// meaning and callers must not treat it as a sentinel value.
    pub const ALL: &'static [Self] = &[
        Self::ElectroOptical,
        Self::Lidar,
        Self::Radar,
        Self::Sigint,
        Self::Sar,
        Self::Other(u64::MAX),
    ];

    /// ST 0601.19 §8.138 Table 17 wire codepoint → variant (`Other(code)`
    /// for unknown codes).
    pub fn from_wire(code: u64) -> Self {
        match code {
            0 => Self::ElectroOptical,
            1 => Self::Lidar,
            2 => Self::Radar,
            3 => Self::Sigint,
            4 => Self::Sar,
            other => Self::Other(other),
        }
    }

    /// Variant → ST 0601.19 §8.138 Table 17 wire codepoint (inverse of
    /// [`Self::from_wire`]).
    pub fn to_wire(self) -> u64 {
        match self {
            Self::ElectroOptical => 0,
            Self::Lidar => 1,
            Self::Radar => 2,
            Self::Sigint => 3,
            Self::Sar => 4,
            Self::Other(code) => code,
        }
    }
}

/// One record of Item 138, Payload List (ST 0601.19 §8.138).
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadRecord {
    pub id: u64,
    pub payload_type: PayloadType,
    pub name: String,
}

fn parse_payload_record(bytes: &[u8]) -> Result<PayloadRecord, KlvFieldError> {
    let (id, rest) = read_ber_oid_u64(bytes).map_err(truncated(138))?;
    let (type_code, rest) = read_ber_oid_u64(rest).map_err(truncated(138))?;
    let (name_bytes, rest) = read_len_prefixed(rest, 138)?;
    if !rest.is_empty() {
        return Err(KlvFieldError::InvalidLength {
            tag: 138,
            expected: bytes.len() - rest.len(),
            got: bytes.len(),
        });
    }
    let name = core::str::from_utf8(name_bytes)
        .map_err(|_| KlvFieldError::InvalidUtf8 { tag: 138 })?
        .to_owned();
    Ok(PayloadRecord {
        id,
        payload_type: PayloadType::from_wire(type_code),
        name,
    })
}

fn payload_record_len(r: &PayloadRecord) -> usize {
    ber_oid_len_u64(r.id)
        + ber_oid_len_u64(r.payload_type.to_wire())
        + ber_len(r.name.len())
        + r.name.len()
}

fn emit_payload_record(r: &PayloadRecord, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(r.id, &mut buf)?;
    out.extend_from_slice(&buf[..n]);
    let n2 = write_ber_oid_u64(r.payload_type.to_wire(), &mut buf)?;
    out.extend_from_slice(&buf[..n2]);
    emit_len_prefixed(r.name.as_bytes(), out)?;
    Ok(())
}

/// Item 138 value (ST 0601.19 §8.138): `count` (BER-OID Payload Count)
/// followed by a VLP of `[BER length][PayloadRecord]` entries.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadList {
    pub count: u64,
    pub records: Vec<PayloadRecord>,
}

pub(crate) fn parse_payload_list(bytes: &[u8]) -> Result<PayloadList, KlvFieldError> {
    let (count, mut rest) = read_ber_oid_u64(bytes).map_err(truncated(138))?;
    let mut records = Vec::new();
    while !rest.is_empty() {
        let (rec_bytes, r) = read_len_prefixed(rest, 138)?;
        records.push(parse_payload_record(rec_bytes)?);
        rest = r;
    }
    Ok(PayloadList { count, records })
}

pub(crate) fn payload_list_len(pl: &PayloadList) -> usize {
    ber_oid_len_u64(pl.count)
        + pl.records
            .iter()
            .map(|r| {
                let l = payload_record_len(r);
                ber_len(l) + l
            })
            .sum::<usize>()
}

pub(crate) fn emit_payload_list(pl: &PayloadList, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(pl.count, &mut buf)?;
    out.extend_from_slice(&buf[..n]);
    for r in &pl.records {
        let l = payload_record_len(r);
        let mut len_buf = [0u8; 9];
        let ln = write_ber(l, &mut len_buf)?;
        out.extend_from_slice(&len_buf[..ln]);
        emit_payload_record(r, out)?;
    }
    Ok(())
}

// ============================================================================
// Item 140: Weapons Stores
// ============================================================================

/// One record of Item 140, Weapons Stores (ST 0601.19 §8.140) — a single
/// weapon's physical address, status, and type. `status_raw` packs the
/// spec's 14-bit Status BER-OID value verbatim: the low 8 bits are the
/// §Table 21 General Status enumeration, the next 4 bits are the
/// §Table 22 Engagement Status flags, and any remaining high bits are
/// spec-reserved (preserved verbatim, not masked away, in case a future
/// revision widens the field).
#[derive(Debug, Clone, PartialEq)]
pub struct WeaponsStore {
    pub station_id: u64,
    pub hardpoint_id: u64,
    pub carriage_id: u64,
    pub store_id: u64,
    pub status_raw: u64,
    pub weapon_type: String,
}

impl WeaponsStore {
    /// §Table 21 General Status code (low 8 bits of `status_raw`).
    pub fn general_status(&self) -> u8 {
        (self.status_raw & 0xFF) as u8
    }
    /// §Table 22 bit position 1.
    pub fn fuze_enabled(&self) -> bool {
        self.status_raw & 0x100 != 0
    }
    /// §Table 22 bit position 2.
    pub fn laser_enabled(&self) -> bool {
        self.status_raw & 0x200 != 0
    }
    /// §Table 22 bit position 3.
    pub fn target_enabled(&self) -> bool {
        self.status_raw & 0x400 != 0
    }
    /// §Table 22 bit position 4.
    pub fn weapon_armed(&self) -> bool {
        self.status_raw & 0x800 != 0
    }
}

fn parse_weapons_store(bytes: &[u8]) -> Result<WeaponsStore, KlvFieldError> {
    let (station_id, rest) = read_ber_oid_u64(bytes).map_err(truncated(140))?;
    let (hardpoint_id, rest) = read_ber_oid_u64(rest).map_err(truncated(140))?;
    let (carriage_id, rest) = read_ber_oid_u64(rest).map_err(truncated(140))?;
    let (store_id, rest) = read_ber_oid_u64(rest).map_err(truncated(140))?;
    let (status_raw, rest) = read_ber_oid_u64(rest).map_err(truncated(140))?;
    let (type_bytes, rest) = read_len_prefixed(rest, 140)?;
    if !rest.is_empty() {
        return Err(KlvFieldError::InvalidLength {
            tag: 140,
            expected: bytes.len() - rest.len(),
            got: bytes.len(),
        });
    }
    let weapon_type = core::str::from_utf8(type_bytes)
        .map_err(|_| KlvFieldError::InvalidUtf8 { tag: 140 })?
        .to_owned();
    Ok(WeaponsStore {
        station_id,
        hardpoint_id,
        carriage_id,
        store_id,
        status_raw,
        weapon_type,
    })
}

fn weapons_store_len(ws: &WeaponsStore) -> usize {
    ber_oid_len_u64(ws.station_id)
        + ber_oid_len_u64(ws.hardpoint_id)
        + ber_oid_len_u64(ws.carriage_id)
        + ber_oid_len_u64(ws.store_id)
        + ber_oid_len_u64(ws.status_raw)
        + ber_len(ws.weapon_type.len())
        + ws.weapon_type.len()
}

fn emit_weapons_store(ws: &WeaponsStore, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    for id in [
        ws.station_id,
        ws.hardpoint_id,
        ws.carriage_id,
        ws.store_id,
        ws.status_raw,
    ] {
        let n = write_ber_oid_u64(id, &mut buf)?;
        out.extend_from_slice(&buf[..n]);
    }
    emit_len_prefixed(ws.weapon_type.as_bytes(), out)?;
    Ok(())
}

/// Item 140 value: `[BER length][WeaponsStore]` repeated until the value
/// bytes are exhausted.
pub(crate) fn parse_weapons_stores(bytes: &[u8]) -> Result<Vec<WeaponsStore>, KlvFieldError> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (rec_bytes, r) = read_len_prefixed(rest, 140)?;
        out.push(parse_weapons_store(rec_bytes)?);
        rest = r;
    }
    Ok(out)
}

pub(crate) fn weapons_stores_len(list: &[WeaponsStore]) -> usize {
    list.iter()
        .map(|ws| {
            let l = weapons_store_len(ws);
            ber_len(l) + l
        })
        .sum()
}

pub(crate) fn emit_weapons_stores(
    list: &[WeaponsStore],
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    for ws in list {
        let l = weapons_store_len(ws);
        let mut len_buf = [0u8; 9];
        let n = write_ber(l, &mut len_buf)?;
        out.extend_from_slice(&len_buf[..n]);
        emit_weapons_store(ws, out)?;
    }
    Ok(())
}

// ============================================================================
// Item 141: Waypoint List
// ============================================================================

/// One record of Item 141, Waypoint List (ST 0601.19 §8.141).
///
/// `info` (the Mode/Source bitfield) and `location` are both optional
/// trailing fields, but only `location` is self-delimiting by its own
/// wire length the way [`Location`]'s internal truncation is — `info`
/// is a self-delimiting BER-OID (so its presence never needs an
/// external marker), and this decoder distinguishes "info present" from
/// "info absent, straight to location" by checking whether the
/// remaining byte count already matches a valid [`Location`] length
/// (`0`, `8`, or `11`, per [`Location`]'s truncation rule — the pair of
/// Latitude/Longitude is mandatory both-or-neither, only HAE truncates
/// independently) BEFORE attempting to consume an `info` BER-OID. This
/// is unambiguous only because no two members of `{0, 8, 11}` differ by
/// exactly the 1 byte a conformant `info` value occupies (values 0-3
/// per §8.141's 2-bit Mode/Source field) — a future revision that
/// widened `info` enough to need e.g. 8 BER-OID bytes could collide
/// with a location-only remainder and misparse; not a concern for the
/// field as currently defined.
#[derive(Debug, Clone, PartialEq)]
pub struct Waypoint {
    pub id: u64,
    pub prosecution_order: i16,
    pub info: Option<u64>,
    pub location: Option<Location>,
}

fn parse_waypoint(bytes: &[u8]) -> Result<Waypoint, KlvFieldError> {
    let (id, rest) = read_ber_oid_u64(bytes).map_err(truncated(141))?;
    if rest.len() < 2 {
        return Err(KlvFieldError::TruncatedField { tag: 141 });
    }
    let prosecution_order = i16::from_be_bytes([rest[0], rest[1]]);
    let rest = &rest[2..];
    let (info, rest) = if is_valid_location_len(rest.len()) {
        (None, rest)
    } else {
        let (v, r) = read_ber_oid_u64(rest).map_err(truncated(141))?;
        (Some(v), r)
    };
    if !is_valid_location_len(rest.len()) {
        // Malformed: after accounting for `info`, what's left doesn't
        // match any valid Location length (0/8/11).
        return Err(KlvFieldError::InvalidLength {
            tag: 141,
            expected: 11,
            got: rest.len(),
        });
    }
    let location = if rest.is_empty() {
        None
    } else {
        Some(parse_location(rest, 141)?)
    };
    Ok(Waypoint {
        id,
        prosecution_order,
        info,
        location,
    })
}

fn waypoint_len(wp: &Waypoint) -> usize {
    ber_oid_len_u64(wp.id)
        + 2
        + wp.info.map(ber_oid_len_u64).unwrap_or(0)
        + wp.location.map(|loc| location_len(&loc)).unwrap_or(0)
}

fn emit_waypoint(wp: &Waypoint, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(wp.id, &mut buf)?;
    out.extend_from_slice(&buf[..n]);
    out.extend_from_slice(&wp.prosecution_order.to_be_bytes());
    if let Some(info) = wp.info {
        let n2 = write_ber_oid_u64(info, &mut buf)?;
        out.extend_from_slice(&buf[..n2]);
    }
    if let Some(loc) = wp.location {
        emit_location(&loc, out)?;
    }
    Ok(())
}

/// Item 141 value: `[BER length][Waypoint]` repeated until the value
/// bytes are exhausted.
pub(crate) fn parse_waypoints(bytes: &[u8]) -> Result<Vec<Waypoint>, KlvFieldError> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (rec_bytes, r) = read_len_prefixed(rest, 141)?;
        out.push(parse_waypoint(rec_bytes)?);
        rest = r;
    }
    Ok(out)
}

pub(crate) fn waypoints_len(list: &[Waypoint]) -> usize {
    list.iter()
        .map(|wp| {
            let l = waypoint_len(wp);
            ber_len(l) + l
        })
        .sum()
}

pub(crate) fn emit_waypoints(list: &[Waypoint], out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    for wp in list {
        let l = waypoint_len(wp);
        let mut len_buf = [0u8; 9];
        let n = write_ber(l, &mut len_buf)?;
        out.extend_from_slice(&len_buf[..n]);
        emit_waypoint(wp, out)?;
    }
    Ok(())
}

// ============================================================================
// Item 142: View Domain
// ============================================================================

/// One `(start, range)` pair of Item 142, View Domain (ST 0601.19
/// §8.142). `start` uses the axis-specific IMAPB range (see
/// [`ViewDomain`]'s azimuth/elevation/roll fields); `range` always uses
/// IMAPB(0,360) — "the
/// angular range specifies the limit from the starting point to the
/// sensor's maximum value; numerically the angular range is always a
/// positive value". Both fields of a pair always share the same IMAPB
/// byte length (whatever the producer chose; this crate encodes at 3
/// bytes).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewDomainPair {
    pub start_deg: f64,
    pub range_deg: f64,
}

/// Item 142 value (ST 0601.19 §8.142): up to three [`ViewDomainPair`]s
/// — azimuth, elevation, roll, in that fixed order — each preceded by a
/// BER pair-length. A pair-length of 0 means "unknown" (the pair is
/// absent, but a byte was still spent saying so); the pack is also a
/// truncation pack, so trailing pairs may be dropped from the wire
/// entirely (no pair-length byte at all).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewDomain {
    pub azimuth: Option<ViewDomainPair>,
    pub elevation: Option<ViewDomainPair>,
    pub roll: Option<ViewDomainPair>,
}

/// Byte width this crate uses to encode each field of a
/// [`ViewDomainPair`] (start and range always share one width). ST
/// 0601.19 §8.142.1: "the IMAPB length is determined at runtime to
/// adjust to the data producer's desired precision" — decode accepts
/// any even pair-length; this is only the encode-side default.
const VIEW_DOMAIN_PAIR_ENCODE_LEN: usize = 3;

/// `(start_min, start_max)` for each axis, in wire order — azimuth and
/// roll share Item 18/20's `[0, 360]`, elevation uses Item 19's
/// `[-180, 180]` (§8.142.1 Table 25).
const VIEW_DOMAIN_AXES: [(f64, f64); 3] = [(0.0, 360.0), (-180.0, 180.0), (0.0, 360.0)];

fn view_domain_setters() -> [fn(&mut ViewDomain, Option<ViewDomainPair>); 3] {
    [
        |v, p| v.azimuth = p,
        |v, p| v.elevation = p,
        |v, p| v.roll = p,
    ]
}

fn view_domain_values(vd: &ViewDomain) -> [Option<ViewDomainPair>; 3] {
    [vd.azimuth, vd.elevation, vd.roll]
}

fn view_domain_last_some(vd: &ViewDomain) -> Option<usize> {
    view_domain_values(vd).iter().rposition(|p| p.is_some())
}

fn parse_view_domain_pair(
    bytes: &[u8],
    start_min: f64,
    start_max: f64,
    tag: u32,
) -> Result<ViewDomainPair, KlvFieldError> {
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: bytes.len() + 1,
            got: bytes.len(),
        });
    }
    let half = bytes.len() / 2;
    let start_params = ImapbParams {
        min: start_min,
        max: start_max,
        length: half,
    };
    let range_params = ImapbParams {
        min: 0.0,
        max: 360.0,
        length: half,
    };
    let start_deg = decode_imapb_required(&start_params, &bytes[..half], tag)?;
    let range_deg = decode_imapb_required(&range_params, &bytes[half..], tag)?;
    Ok(ViewDomainPair {
        start_deg,
        range_deg,
    })
}

fn emit_view_domain_pair(
    p: &ViewDomainPair,
    start_min: f64,
    start_max: f64,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let start_params = ImapbParams {
        min: start_min,
        max: start_max,
        length: VIEW_DOMAIN_PAIR_ENCODE_LEN,
    };
    let range_params = ImapbParams {
        min: 0.0,
        max: 360.0,
        length: VIEW_DOMAIN_PAIR_ENCODE_LEN,
    };
    let mut buf = [0u8; VIEW_DOMAIN_PAIR_ENCODE_LEN];
    encode_imapb(&start_params, p.start_deg, &mut buf)?;
    out.extend_from_slice(&buf);
    encode_imapb(&range_params, p.range_deg, &mut buf)?;
    out.extend_from_slice(&buf);
    Ok(())
}

pub(crate) fn parse_view_domain(bytes: &[u8]) -> Result<ViewDomain, KlvFieldError> {
    let mut vd = ViewDomain::default();
    let setters = view_domain_setters();
    let mut rest = bytes;
    for ((start_min, start_max), setter) in VIEW_DOMAIN_AXES.into_iter().zip(setters) {
        if rest.is_empty() {
            break; // trailing truncation
        }
        let (pair_len, r) = read_ber(rest).map_err(truncated(142))?;
        rest = r;
        if pair_len > 0 {
            if rest.len() < pair_len {
                return Err(KlvFieldError::TruncatedField { tag: 142 });
            }
            let pair = parse_view_domain_pair(&rest[..pair_len], start_min, start_max, 142)?;
            setter(&mut vd, Some(pair));
            rest = &rest[pair_len..];
        }
        // pair_len == 0: "unknown" marker -- axis stays None, continue.
    }
    if !rest.is_empty() {
        return Err(KlvFieldError::InvalidLength {
            tag: 142,
            expected: bytes.len() - rest.len(),
            got: bytes.len(),
        });
    }
    Ok(vd)
}

pub(crate) fn view_domain_len(vd: &ViewDomain) -> usize {
    match view_domain_last_some(vd) {
        None => 0,
        Some(last) => {
            let pair_bytes = 2 * VIEW_DOMAIN_PAIR_ENCODE_LEN;
            view_domain_values(vd)[..=last]
                .iter()
                .map(|p| {
                    let plen = if p.is_some() { pair_bytes } else { 0 };
                    ber_len(plen) + plen
                })
                .sum()
        }
    }
}

pub(crate) fn emit_view_domain(vd: &ViewDomain, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let Some(last) = view_domain_last_some(vd) else {
        return Ok(());
    };
    let mut len_buf = [0u8; 9];
    for (i, ((start_min, start_max), pair)) in VIEW_DOMAIN_AXES
        .into_iter()
        .zip(view_domain_values(vd))
        .enumerate()
    {
        if i > last {
            break;
        }
        match pair {
            Some(p) => {
                let plen = 2 * VIEW_DOMAIN_PAIR_ENCODE_LEN;
                let n = write_ber(plen, &mut len_buf)?;
                out.extend_from_slice(&len_buf[..n]);
                emit_view_domain_pair(&p, start_min, start_max, out)?;
            }
            // Interior "unknown" marker (pair-len 0) — see the struct rustdoc.
            None => out.push(0x00),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_horizon_truncated_optional_bytes_error() {
        // 4 mandatory + 2 stray bytes: not a clean truncation (doesn't
        // align to a 4-byte optional field boundary).
        let err = parse_image_horizon(&[0, 0, 0, 0, 0xAA, 0xBB]).unwrap_err();
        assert!(matches!(err, KlvFieldError::InvalidLength { tag: 81, .. }));
    }

    #[test]
    fn image_horizon_too_short_for_mandatory_fields_error() {
        let err = parse_image_horizon(&[0, 0, 0]).unwrap_err();
        assert!(matches!(err, KlvFieldError::TruncatedField { tag: 81 }));
    }

    #[test]
    fn image_horizon_full_pack_round_trips() {
        let h = ImageHorizonPixels {
            x0_pct: 10,
            y0_pct: 20,
            x1_pct: 30,
            y1_pct: 40,
            start_lat_deg: Some(10.0),
            start_lon_deg: Some(-20.0),
            end_lat_deg: Some(30.0),
            end_lon_deg: Some(-40.0),
        };
        assert_eq!(image_horizon_len(&h), 20);
        let mut buf = Vec::new();
        emit_image_horizon(&h, &mut buf).unwrap();
        assert_eq!(buf.len(), 20);
        let back = parse_image_horizon(&buf).unwrap();
        assert_eq!(back.x0_pct, 10);
        assert!((back.start_lat_deg.unwrap() - 10.0).abs() < 1e-3);
        assert!((back.end_lon_deg.unwrap() - (-40.0)).abs() < 1e-3);
    }

    /// A spec-legal wire can carry the INT_MIN "error" indicator
    /// (§8.81) for an EARLIER optional field while carrying valid data
    /// for a LATER one — decode must not lose the later fields, and
    /// re-encode must reproduce the exact same wire bytes (sentinel
    /// fill, not truncation).
    #[test]
    fn image_horizon_sentinel_then_valid_data_round_trips_byte_identical() {
        let mut wire = vec![10u8, 20, 30, 40];
        wire.extend_from_slice(&i32::MIN.to_be_bytes()); // start_lat: sentinel
        let mut buf = [0u8; 4];
        encode_fixed_range(&LON_RANGE, 81, -20.0, &mut buf, OutOfRangePolicy::Error).unwrap();
        wire.extend_from_slice(&buf); // start_lon: valid
        encode_fixed_range(&LAT_RANGE, 81, 30.0, &mut buf, OutOfRangePolicy::Error).unwrap();
        wire.extend_from_slice(&buf); // end_lat: valid
        encode_fixed_range(&LON_RANGE, 81, -40.0, &mut buf, OutOfRangePolicy::Error).unwrap();
        wire.extend_from_slice(&buf); // end_lon: valid

        let h = parse_image_horizon(&wire).unwrap();
        assert_eq!((h.x0_pct, h.y0_pct, h.x1_pct, h.y1_pct), (10, 20, 30, 40));
        assert_eq!(h.start_lat_deg, None, "INT_MIN sentinel decodes to None");
        assert!((h.start_lon_deg.unwrap() - (-20.0)).abs() < 1e-6);
        assert!((h.end_lat_deg.unwrap() - 30.0).abs() < 1e-6);
        assert!((h.end_lon_deg.unwrap() - (-40.0)).abs() < 1e-6);

        let mut re_encoded = Vec::new();
        emit_image_horizon(&h, &mut re_encoded).unwrap();
        assert_eq!(
            re_encoded, wire,
            "sentinel-then-data pack must re-encode byte-identical, not truncated"
        );
    }

    /// In-memory record with interior `None` gaps (not derived from a
    /// decode) must still round-trip its `Some` fields intact through
    /// encode -> decode.
    #[test]
    fn image_horizon_interior_none_round_trips_via_encode_decode() {
        let h = ImageHorizonPixels {
            x0_pct: 1,
            y0_pct: 2,
            x1_pct: 3,
            y1_pct: 4,
            start_lat_deg: None,
            start_lon_deg: Some(45.0),
            end_lat_deg: None,
            end_lon_deg: Some(-90.0),
        };
        let mut buf = Vec::new();
        emit_image_horizon(&h, &mut buf).unwrap();
        assert_eq!(buf.len(), image_horizon_len(&h));
        assert_eq!(
            buf.len(),
            20,
            "all 4 optional slots present up to the last Some"
        );
        let back = parse_image_horizon(&buf).unwrap();
        assert_eq!(back.start_lat_deg, None);
        assert!((back.start_lon_deg.unwrap() - 45.0).abs() < 1e-6);
        assert_eq!(back.end_lat_deg, None);
        assert!((back.end_lon_deg.unwrap() - (-90.0)).abs() < 1e-6);
    }

    #[test]
    fn control_command_over_length_string_rejected() {
        let cmd = ControlCommand {
            id: 1,
            command: "x".repeat(128),
            time_us: None,
        };
        let mut buf = Vec::new();
        let err = emit_control_command(&cmd, &mut buf).unwrap_err();
        assert!(matches!(
            err,
            KlvEncodeError::StringTooLong { tag: 115, max: 127 }
        ));
    }

    #[test]
    fn control_command_with_time_us_round_trips() {
        let cmd = ControlCommand {
            id: 200,
            command: "abc".into(),
            time_us: Some(1_700_000_000_000_000),
        };
        let mut buf = Vec::new();
        emit_control_command(&cmd, &mut buf).unwrap();
        assert_eq!(buf.len(), control_command_len(&cmd));
        let back = parse_control_command(&buf).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn id_list_empty_and_round_trip() {
        assert_eq!(parse_id_list(&[], 116).unwrap(), Vec::<u64>::new());
        let ids = vec![0u64, 3, 300];
        let mut buf = Vec::new();
        emit_id_list(&ids, &mut buf).unwrap();
        assert_eq!(buf.len(), id_list_len(&ids));
        assert_eq!(parse_id_list(&buf, 116).unwrap(), ids);
    }

    #[test]
    fn sensor_frame_rate_fps_computes_ratio() {
        let fr = SensorFrameRate {
            numerator: 60000,
            denominator: 1001,
        };
        assert!((fr.fps() - 59.940_059_940_06).abs() < 1e-9);
    }

    #[test]
    fn sensor_frame_rate_explicit_denominator_one_canonicalizes_away() {
        // Wire explicitly encodes denominator=1; re-encode canonicalizes
        // to the shorter numerator-only form (documented behavior).
        let bytes = [0x1E, 0x01]; // 30, 1
        let fr = parse_sensor_frame_rate(&bytes).unwrap();
        assert_eq!((fr.numerator, fr.denominator), (30, 1));
        let mut buf = Vec::new();
        emit_sensor_frame_rate(&fr, &mut buf).unwrap();
        assert_eq!(buf, vec![0x1E]);
    }

    #[test]
    fn metadata_substream_id_bad_uuid_length_rejected() {
        let err = parse_metadata_substream_id(&[0x00, 0xAA, 0xBB]).unwrap_err();
        assert!(matches!(
            err,
            KlvFieldError::InvalidLength {
                tag: 143,
                expected: 16,
                got: 2,
            }
        ));
    }

    #[test]
    fn metadata_substream_id_local_id_only_round_trips() {
        let ms = MetadataSubstreamId {
            local_id: 5,
            uuid: None,
        };
        let mut buf = Vec::new();
        emit_metadata_substream_id(&ms, &mut buf).unwrap();
        assert_eq!(buf, vec![0x05]);
        assert_eq!(parse_metadata_substream_id(&buf).unwrap(), ms);
    }
}

#[cfg(test)]
mod variant_inventory {
    //! Exhaustiveness for this module's wire-code enum (Arc 2 R2). See
    //! [`crate::klv::inventory_test`].
    use super::*;
    use crate::klv::inventory_test;

    inventory_test!(
        payload_type_all_is_complete_and_round_trips,
        PayloadType,
        [
            PayloadType::ElectroOptical,
            PayloadType::Lidar,
            PayloadType::Radar,
            PayloadType::Sigint,
            PayloadType::Sar,
            PayloadType::Other(_),
        ]
    );
}
