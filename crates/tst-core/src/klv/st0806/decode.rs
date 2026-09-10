//! ST 0806.4 RVT Local Set decode — nested body form (ST 0601 Tag 73 value)
//! and standalone form (own UL + BER length + CRC-32/MPEG-2 verification).

use alloc::borrow::ToOwned;
use alloc::string::String;

use crate::error::{KlvDecodeError, KlvFieldError};
use crate::klv::length::read_ber;
use crate::klv::pack::{Iter, OwnedRawField, RawField};
use crate::klv::st0601::mapping::decode_fixed_range;
use crate::klv::st0601::tags::LinearRange;
use crate::klv::universal_label::UniversalLabel;

use super::model::{RVT_LS_UL, RvtAoi, RvtAoiType, RvtLs, RvtPoi, RvtPoiType, RvtUserData};
use super::tags::{RvtEncoding, lookup};

/// POI/AOI latitude mapping (ST 0806.4 Table 8-2 Tag 2 / Table 8-3 Tags
/// 2 & 4): signed int32, symmetric ±90°, `0x80000000` = "error" sentinel.
/// `pub(super)`: shared with `super::encode`, which re-encodes the same
/// sentinel bytes on the value-absent-but-recorded path.
pub(super) const LAT_RANGE: LinearRange = LinearRange {
    signed: true,
    byte_length: 4,
    min: -90.0,
    max: 90.0,
};
/// POI/AOI longitude mapping (Table 8-2 Tag 3 / Table 8-3 Tags 3 & 5):
/// signed int32, symmetric ±180°, same sentinel as [`LAT_RANGE`].
pub(super) const LON_RANGE: LinearRange = LinearRange {
    signed: true,
    byte_length: 4,
    min: -180.0,
    max: 180.0,
};
/// POI altitude mapping (Table 8-2 Tag 4): unsigned uint16, [-900, 19000] m
/// MSL. Unsigned ranges never produce the `None` sentinel outcome.
pub(super) const ALT_RANGE: LinearRange = LinearRange {
    signed: false,
    byte_length: 2,
    min: -900.0,
    max: 19000.0,
};

/// Decode an RVT Local Set body (ST 0806.4 Table 8-1) — the form carried
/// as the *value* of ST 0601 Tag 73, with no 16-byte UL and no outer BER
/// length prefix (the caller has already stripped both).
///
/// Lenient like the ST 0601 typed decoder: unknown tags are preserved
/// verbatim in [`RvtLs::unknown`], per-field validation failures are
/// collected in [`RvtLs::field_errors`] instead of aborting the whole
/// record, and a malformed nested Local Set (Tag 11/12/13) is skipped —
/// recorded as a [`KlvFieldError::TruncatedField`] on the *parent* tag —
/// rather than failing the enclosing decode.
///
/// Tag 1 (CRC-32/MPEG-2), if present, is captured into [`RvtLs::crc32`]
/// but **not verified**: the checksum's spec coverage is "the entire LS
/// packet including the 16-byte UL key" (ST 0806.4 §8/Appendix), which
/// this body form does not include. Use [`decode_standalone`] to verify it.
///
/// ST 0806.4-02/-04 require an *independent* RVT LS to lead with Tag 2
/// (timestamp) and end with Tag 1 (CRC); this function does not enforce
/// that ordering even in the independent case — real captures are not
/// always ordering-conformant, matching the ST 0601 typed decoder's own
/// precedent of reserving ordering checks for a dedicated strict mode.
pub fn decode(bytes: &[u8]) -> Result<RvtLs, KlvDecodeError> {
    let mut record = RvtLs::default();
    for r in Iter::local_set(bytes) {
        let f = r?;
        if let Err(e) = apply_rvt_tag(&mut record, &f) {
            record.field_errors.push(e);
        }
    }
    Ok(record)
}

/// Decode a standalone RVT Local Set: 16-byte UL ([`super::RVT_LS_UL`]) +
/// BER length + body, verifying the CRC-32/MPEG-2 checksum (Tag 1) when
/// present.
///
/// The CRC covers every byte from the start of the UL up to (but not
/// including) the CRC's own 4 value bytes — the last well-formed Tag 1
/// occurrence wins if the (non-conformant) input repeats it. Absence of
/// Tag 1 is not an error: an embedded RVT LS is not required to carry
/// one, and this function accepts standalone captures that omit it too.
/// ST 0806.4 does not explicitly state whether the covered span includes
/// the CRC's own value bytes; excluding them is the universal-practice
/// reading here, pending a real-capture check (spec ambiguity, not yet
/// resolved) — the same caveat class as the POI Label field's, see
/// [`RvtPoi::label`].
///
/// # Errors
/// - [`KlvDecodeError::Truncated`] if `bytes` is shorter than the UL, or
///   the declared BER length runs past the buffer.
/// - [`KlvDecodeError::UnexpectedUniversalLabel`] if the leading 16
///   bytes are not [`super::RVT_LS_UL`].
/// - [`KlvDecodeError::Crc32Mismatch`] if a declared Tag 1 value does
///   not match the recomputed CRC-32/MPEG-2.
/// - Any structural error surfaced while locating Tag 1, or from
///   [`decode`] once delegated to the body.
pub fn decode_standalone(bytes: &[u8]) -> Result<RvtLs, KlvDecodeError> {
    if bytes.len() < 16 {
        return Err(KlvDecodeError::Truncated {
            offset: 0,
            needed: 16,
            have: bytes.len(),
        });
    }
    let mut ul_bytes = [0u8; 16];
    ul_bytes.copy_from_slice(&bytes[..16]);
    let ul = UniversalLabel::new(ul_bytes);
    if ul != RVT_LS_UL {
        return Err(KlvDecodeError::UnexpectedUniversalLabel {
            expected: RVT_LS_UL,
            found: ul,
        });
    }

    // Read from `bytes[16..]`, so the outer length's own errors come back
    // slice-relative — rebase by the UL length to keep every offset this
    // decoder reports indexed against the caller's own buffer.
    let (declared_len, after_len) = read_ber(&bytes[16..]).map_err(|mut e| {
        crate::klv::length::rebase_offset(&mut e, 16);
        e
    })?;
    let body_offset = bytes.len() - after_len.len();
    if after_len.len() < declared_len {
        return Err(KlvDecodeError::Truncated {
            offset: body_offset,
            needed: declared_len,
            have: after_len.len(),
        });
    }
    let body = &after_len[..declared_len];

    // Locate the last well-formed Tag 1 (CRC) occurrence and verify it
    // covers every byte of `bytes` up to (not including) its own 4 value
    // bytes. A Tag 1 occurrence with the wrong length is left for the
    // body decode below to report as a field_errors entry rather than
    // aborting the standalone CRC check here.
    let mut declared_crc: Option<(u32, usize)> = None; // (value, offset of value bytes within `bytes`)
    for r in Iter::local_set(body) {
        let f = r.map_err(|mut e| {
            crate::klv::length::rebase_offset(&mut e, body_offset);
            e
        })?;
        if f.tag == 1 && f.value.len() == 4 {
            let mut a = [0u8; 4];
            a.copy_from_slice(f.value);
            // `f.value` is guaranteed by `Iter::local_set(body)` to be a
            // subslice of `body` itself, so this subtraction can't
            // underflow. `body_offset` (computed above from slice lengths,
            // no pointer math) is `body`'s own start within `bytes`, so
            // summing the two gives the value bytes' offset in `bytes`
            // without diffing pointers across the `bytes`/`after_len`/
            // `body` slicing chain.
            let offset_in_body = (f.value.as_ptr() as usize) - (body.as_ptr() as usize);
            let value_offset = body_offset + offset_in_body;
            declared_crc = Some((u32::from_be_bytes(a), value_offset));
        }
    }
    if let Some((expected, value_offset)) = declared_crc {
        // Defensive bounds check: `value_offset + 4 <= bytes.len()` always
        // holds today (the 4 CRC value bytes are themselves inside
        // `bytes`), but skip verification rather than panic on a future
        // refactor that breaks the invariant above.
        let in_bounds = value_offset
            .checked_add(4)
            .is_some_and(|end| end <= bytes.len());
        if in_bounds {
            let computed = crate::mpegts::common::crc32::crc32_mpeg2(&bytes[..value_offset]);
            if computed != expected {
                return Err(KlvDecodeError::Crc32Mismatch {
                    expected,
                    found: computed,
                });
            }
        }
    }

    decode(body)
}

/// Apply one top-level RVT LS tag (Table 8-1) to `record`. Mirrors
/// `st0601::decode::apply_typed_tag`'s shape but is local to this
/// 21-tag schema: unknown tags are preserved verbatim, per-field
/// validation failures are returned (the caller pushes them onto
/// [`RvtLs::field_errors`]), and a malformed nested Local Set under
/// Tag 11/12/13 surfaces as [`KlvFieldError::TruncatedField`] rather
/// than aborting the whole record.
fn apply_rvt_tag(record: &mut RvtLs, f: &RawField<'_>) -> Result<(), KlvFieldError> {
    let tag = f.tag;
    // Same u8-narrowing gate as st0601::apply_typed_tag: reject tags
    // outside the typed table's u8 universe before narrowing, so a
    // future multi-byte BER-OID tag can never `as u8`-collide with a
    // defined single-byte tag.
    let Ok(tag_u8) = u8::try_from(tag) else {
        record.unknown.push(OwnedRawField::from(f.clone()));
        return Ok(());
    };
    let Some(spec) = lookup(tag_u8) else {
        record.unknown.push(OwnedRawField::from(f.clone()));
        return Ok(());
    };
    match spec.encoding {
        RvtEncoding::U8 => {
            let v = read_u8(f, tag)?;
            match tag_u8 {
                5 => record.telemetry_accuracy_indicator = Some(v),
                8 => record.rvt_ls_version = Some(v),
                14 => record.aircraft_mgrs_zone = Some(v),
                18 => record.frame_center_mgrs_zone = Some(v),
                _ => unreachable!("RvtEncoding::U8 tag outside {{5,8,14,18}}"),
            }
        }
        RvtEncoding::U16 => {
            let v = read_u16(f, tag)?;
            match tag_u8 {
                3 => record.platform_true_airspeed = Some(v),
                4 => record.platform_indicated_airspeed = Some(v),
                6 => record.frag_circle_radius_m = Some(v),
                _ => unreachable!("RvtEncoding::U16 tag outside {{3,4,6}}"),
            }
        }
        RvtEncoding::U24 => {
            let v = read_u24(f, tag)?;
            match tag_u8 {
                16 => record.aircraft_mgrs_easting_m = Some(v),
                17 => record.aircraft_mgrs_northing_m = Some(v),
                20 => record.frame_center_mgrs_easting_m = Some(v),
                21 => record.frame_center_mgrs_northing_m = Some(v),
                _ => unreachable!("RvtEncoding::U24 tag outside {{16,17,20,21}}"),
            }
        }
        RvtEncoding::U32 => {
            let v = read_u32(f, tag)?;
            match tag_u8 {
                1 => record.crc32 = Some(v),
                7 => record.frame_code = Some(v),
                9 => record.video_data_rate = Some(v),
                _ => unreachable!("RvtEncoding::U32 tag outside {{1,7,9}}"),
            }
        }
        RvtEncoding::U64 => {
            if f.value.len() != 8 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 8,
                    got: f.value.len(),
                });
            }
            let mut a = [0u8; 8];
            a.copy_from_slice(f.value);
            record.timestamp_us = Some(u64::from_be_bytes(a));
        }
        RvtEncoding::Iso7 { max_bytes } => {
            let s = decode_iso7(f.value, tag, max_bytes)?;
            match tag_u8 {
                10 => record.digital_video_file_format = s,
                15 => record.aircraft_mgrs_band_grid = s,
                19 => record.frame_center_mgrs_band_grid = s,
                _ => unreachable!("RvtEncoding::Iso7 tag outside {{10,15,19}}"),
            }
        }
        RvtEncoding::Nested => match tag_u8 {
            11 => match decode_user_defined(f.value) {
                Some(ud) => record.user_defined.push(ud),
                None => return Err(KlvFieldError::TruncatedField { tag }),
            },
            12 => match decode_poi(f.value) {
                Some(poi) => record.points_of_interest.push(poi),
                None => return Err(KlvFieldError::TruncatedField { tag }),
            },
            13 => match decode_aoi(f.value) {
                Some(aoi) => record.areas_of_interest.push(aoi),
                None => return Err(KlvFieldError::TruncatedField { tag }),
            },
            _ => unreachable!("RvtEncoding::Nested tag outside {{11,12,13}}"),
        },
    }
    Ok(())
}

/// Decode a Point of Interest Local Set (ST 0806.4 Table 8-2), the value
/// of an RVT Tag 12 occurrence. Returns `None` only when the nested body
/// itself is structurally malformed (bad BER tag/length) — the caller
/// records that as a [`KlvFieldError::TruncatedField`] on Tag 12 and
/// skips the item. Individual field validation failures within a
/// well-formed body are collected in [`RvtPoi::field_errors`] instead.
fn decode_poi(bytes: &[u8]) -> Option<RvtPoi> {
    let mut poi = RvtPoi::default();
    for r in Iter::local_set(bytes) {
        let f = r.ok()?;
        if let Err(e) = apply_poi_tag(&mut poi, &f) {
            poi.field_errors.push(e);
        }
    }
    Some(poi)
}

fn apply_poi_tag(poi: &mut RvtPoi, f: &RawField<'_>) -> Result<(), KlvFieldError> {
    let tag = f.tag;
    match tag {
        1 => poi.number = Some(read_u16(f, tag)?),
        2 => match decode_fixed_range(&LAT_RANGE, tag, f.value)? {
            Some(v) => poi.lat_deg = Some(v),
            None => poi.sentinel_tags.push(tag),
        },
        3 => match decode_fixed_range(&LON_RANGE, tag, f.value)? {
            Some(v) => poi.lon_deg = Some(v),
            None => poi.sentinel_tags.push(tag),
        },
        4 => match decode_fixed_range(&ALT_RANGE, tag, f.value)? {
            Some(v) => poi.alt_m = Some(v),
            None => poi.sentinel_tags.push(tag),
        },
        5 => poi.poi_type = Some(RvtPoiType::from_wire(read_u8(f, tag)?)),
        6 => poi.text = decode_iso7(f.value, tag, 2048)?,
        7 => poi.source_icon = decode_iso7(f.value, tag, 127)?,
        8 => poi.source_id = decode_iso7(f.value, tag, 255)?,
        9 => poi.label = decode_iso7(f.value, tag, 16)?,
        10 => poi.operation_id = decode_iso7(f.value, tag, 127)?,
        _ => poi.unknown.push(OwnedRawField::from(f.clone())),
    }
    Ok(())
}

/// Decode an Area of Interest Local Set (Table 8-3), the value of an RVT
/// Tag 13 occurrence. Same skip-on-structural-malformation contract as
/// [`decode_poi`].
fn decode_aoi(bytes: &[u8]) -> Option<RvtAoi> {
    let mut aoi = RvtAoi::default();
    for r in Iter::local_set(bytes) {
        let f = r.ok()?;
        if let Err(e) = apply_aoi_tag(&mut aoi, &f) {
            aoi.field_errors.push(e);
        }
    }
    Some(aoi)
}

fn apply_aoi_tag(aoi: &mut RvtAoi, f: &RawField<'_>) -> Result<(), KlvFieldError> {
    let tag = f.tag;
    match tag {
        1 => aoi.number = Some(read_u16(f, tag)?),
        2 => match decode_fixed_range(&LAT_RANGE, tag, f.value)? {
            Some(v) => aoi.corner_lat_p1_deg = Some(v),
            None => aoi.sentinel_tags.push(tag),
        },
        3 => match decode_fixed_range(&LON_RANGE, tag, f.value)? {
            Some(v) => aoi.corner_lon_p1_deg = Some(v),
            None => aoi.sentinel_tags.push(tag),
        },
        4 => match decode_fixed_range(&LAT_RANGE, tag, f.value)? {
            Some(v) => aoi.corner_lat_p3_deg = Some(v),
            None => aoi.sentinel_tags.push(tag),
        },
        5 => match decode_fixed_range(&LON_RANGE, tag, f.value)? {
            Some(v) => aoi.corner_lon_p3_deg = Some(v),
            None => aoi.sentinel_tags.push(tag),
        },
        6 => aoi.aoi_type = Some(RvtAoiType::from_wire(read_u8(f, tag)?)),
        7 => aoi.text = decode_iso7(f.value, tag, 2048)?,
        8 => aoi.source_id = decode_iso7(f.value, tag, 255)?,
        9 => aoi.label = decode_iso7(f.value, tag, 16)?,
        10 => aoi.operation_id = decode_iso7(f.value, tag, 127)?,
        _ => aoi.unknown.push(OwnedRawField::from(f.clone())),
    }
    Ok(())
}

/// Decode a User Defined Local Set (ST 0806.4 Table 8-4), the value of an
/// RVT Tag 11 occurrence: exactly two items (Tag 1 the packed
/// data-type/numeric-id byte, Tag 2 the payload). `None` on any
/// structural malformation OR a missing mandatory item — the caller
/// records that as a [`KlvFieldError::TruncatedField`] on Tag 11 and
/// skips the item; unlike [`RvtPoi`]/[`RvtAoi`] this type carries no
/// `field_errors`/`unknown` sink of its own to partially populate.
fn decode_user_defined(bytes: &[u8]) -> Option<RvtUserData> {
    let mut numeric_id_raw = None;
    let mut data = None;
    for r in Iter::local_set(bytes) {
        let f = r.ok()?;
        match f.tag {
            1 => {
                if f.value.len() != 1 {
                    return None;
                }
                numeric_id_raw = Some(f.value[0]);
            }
            2 => data = Some(f.value.to_vec()),
            _ => {} // Table 8-4 defines exactly these two items.
        }
    }
    Some(RvtUserData {
        numeric_id_raw: numeric_id_raw?,
        data: data.unwrap_or_default(),
    })
}

fn read_u8(f: &RawField<'_>, tag: u32) -> Result<u8, KlvFieldError> {
    if f.value.len() != 1 {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: 1,
            got: f.value.len(),
        });
    }
    Ok(f.value[0])
}

fn read_u16(f: &RawField<'_>, tag: u32) -> Result<u16, KlvFieldError> {
    if f.value.len() != 2 {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: 2,
            got: f.value.len(),
        });
    }
    Ok(u16::from_be_bytes([f.value[0], f.value[1]]))
}

/// Fold a 3-byte big-endian unsigned value (MGRS easting/northing) into a `u32`.
fn read_u24(f: &RawField<'_>, tag: u32) -> Result<u32, KlvFieldError> {
    if f.value.len() != 3 {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: 3,
            got: f.value.len(),
        });
    }
    Ok((u32::from(f.value[0]) << 16) | (u32::from(f.value[1]) << 8) | u32::from(f.value[2]))
}

fn read_u32(f: &RawField<'_>, tag: u32) -> Result<u32, KlvFieldError> {
    if f.value.len() != 4 {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: 4,
            got: f.value.len(),
        });
    }
    let mut a = [0u8; 4];
    a.copy_from_slice(f.value);
    Ok(u32::from_be_bytes(a))
}

/// ASCII-lenient ISO 7-bit string decode: zero-length value means the
/// field is absent (`None`); anything longer than `max_bytes` is
/// [`KlvFieldError::InvalidLength`]; non-UTF-8 bytes are
/// [`KlvFieldError::InvalidUtf8`] (mirrors `st0601::decode`'s `Utf8`
/// arm, minus its single-NUL-byte-means-empty-string convention, which
/// ST 0806.4 does not define).
fn decode_iso7(value: &[u8], tag: u32, max_bytes: usize) -> Result<Option<String>, KlvFieldError> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > max_bytes {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: max_bytes,
            got: value.len(),
        });
    }
    let s = core::str::from_utf8(value).map_err(|_| KlvFieldError::InvalidUtf8 { tag })?;
    Ok(Some(s.to_owned()))
}
