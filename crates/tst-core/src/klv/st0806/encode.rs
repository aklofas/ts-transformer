//! ST 0806.4 RVT Local Set encode — body form (the value carried by ST 0601
//! Tag 73) and standalone form (own UL + BER length + CRC-32/MPEG-2
//! trailer). Mirror of [`super::decode`]'s two entry points.

use alloc::vec::Vec;

use crate::error::KlvEncodeError;
use crate::klv::length::{ber_len, write_ber};
use crate::klv::pack::{emit_ber_oid_tlv, is_typed_tag};
use crate::klv::st0601::OutOfRangePolicy;
use crate::klv::st0601::mapping::encode_fixed_range;
use crate::klv::st0601::tags::LinearRange;
use crate::mpegts::common::crc32::crc32_mpeg2;

use super::decode::{ALT_RANGE, LAT_RANGE, LON_RANGE};
use super::model::{RVT_LS_UL, RvtAoi, RvtLs, RvtPoi, RvtUserData};

/// Encode an RVT Local Set body (ST 0806.4 Table 8-1) — the form carried
/// as the *value* of ST 0601 Tag 73: no 16-byte UL, no outer BER length
/// wrapper (the caller prepends both), and no CRC (an embedded RVT LS
/// is not required to carry Tag 1 — see [`RvtLs::crc32`]).
///
/// Tag 2 (timestamp) is emitted first when present — matching the
/// independent-LS ordering rule even though it is not required for the
/// embedded case — then the remaining scalar tags ascending, then the
/// repeatable nested sets (Tag 11 User Defined / 12 POI / 13 AOI) in
/// `Vec` push order, then `record.unknown` verbatim.
///
/// # Errors
/// - [`KlvEncodeError::MissingMandatoryItem`] if an [`RvtPoi`] omits
///   Number/Latitude/Longitude, or an [`RvtAoi`] omits Number/either
///   corner pair/Type (ST 0806.4-08..-10 / -13..-18). A sentinel
///   recorded in `sentinel_tags` counts as present for Latitude/Longitude
///   (value wins over the sentinel when both are given).
/// - [`KlvEncodeError::StringTooLong`] if an ISO-7 string field exceeds
///   its Table 8-1/8-2/8-3 byte cap.
/// - [`KlvEncodeError::InvalidFieldValue`] if an ISO-7 string field holds a
///   character that is not ASCII.
/// - [`KlvEncodeError::OutOfRange`] if a MGRS easting/northing (uint24)
///   exceeds 99,999, an MGRS zone is outside 1 to 60, or a POI
///   lat/lon/altitude value cannot be mapped into its declared range.
/// - [`KlvEncodeError::InvalidFieldValue`] if an MGRS latitude band and
///   grid square is not three letters A to Z without I and O (ST 0806.4
///   Table 8-1, Items 15 and 19).
/// - [`KlvEncodeError::ReservedTagInUnknown`] if `ls.unknown` (or a
///   nested [`RvtPoi::unknown`] / [`RvtAoi::unknown`]) carries a tag
///   already covered by a typed field — emitting it would produce a
///   non-conformant duplicate.
pub fn encode_to_vec(ls: &RvtLs) -> Result<Vec<u8>, KlvEncodeError> {
    let mut body = Vec::with_capacity(64);
    write_body(ls, &mut body)?;
    Ok(body)
}

/// Encode a standalone RVT Local Set: [`RVT_LS_UL`] + BER length + body,
/// with Tag 2 (timestamp) FIRST per ST 0806.4-02 and Tag 1
/// (CRC-32/MPEG-2) LAST per ST 0806.4-04. The CRC covers every byte from
/// the start of the UL through the CRC's own tag+length bytes (not its 4
/// value bytes) — mirrors the span [`super::decode::decode_standalone`]
/// recomputes over.
///
/// # Errors
/// - [`KlvEncodeError::MissingMandatoryItem`] with `tag: 2` if
///   `ls.timestamp_us` is `None` (ST 0806.4-01).
/// - Any other [`KlvEncodeError`] variant [`encode_to_vec`] can return,
///   from the same body composition.
pub fn encode_to_vec_standalone(ls: &RvtLs) -> Result<Vec<u8>, KlvEncodeError> {
    if ls.timestamp_us.is_none() {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 2,
            name: "User Defined Time Stamp",
        });
    }
    let mut body = Vec::with_capacity(64);
    write_body(ls, &mut body)?;

    // Tag 1 (CRC) reserves 6 bytes: 1-byte tag + 1-byte length + 4-byte value.
    let body_len_with_crc = body.len() + 6;
    let mut out = Vec::with_capacity(16 + ber_len(body_len_with_crc) + body_len_with_crc);
    out.extend_from_slice(&RVT_LS_UL.0);
    let mut len_buf = [0u8; 9]; // BER: 1 flag byte + up to 8 length bytes
    let n = write_ber(body_len_with_crc, &mut len_buf)?;
    out.extend_from_slice(&len_buf[..n]);
    out.extend_from_slice(&body);
    out.push(0x01); // Tag 1
    out.push(0x04); // length 4
    let crc = crc32_mpeg2(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    Ok(out)
}

/// Shared body composition for both [`encode_to_vec`] and
/// [`encode_to_vec_standalone`] — Tag 2 first, tags 3-10/14-21 ascending,
/// nested sets 11/12/13 in `Vec` order, then `unknown`.
fn write_body(ls: &RvtLs, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    if let Some(ts) = ls.timestamp_us {
        emit_ber_oid_tlv(2, &ts.to_be_bytes(), out)?;
    }
    emit_u16(3, ls.platform_true_airspeed, out)?;
    emit_u16(4, ls.platform_indicated_airspeed, out)?;
    emit_u8(5, ls.telemetry_accuracy_indicator, out)?;
    emit_u16(6, ls.frag_circle_radius_m, out)?;
    emit_u32(7, ls.frame_code, out)?;
    emit_u8(8, ls.rvt_ls_version, out)?;
    emit_u32(9, ls.video_data_rate, out)?;
    emit_iso7(10, ls.digital_video_file_format.as_deref(), 127, out)?;
    check_mgrs(
        14,
        ls.aircraft_mgrs_zone,
        ls.aircraft_mgrs_band_grid.as_deref(),
    )?;
    check_mgrs(
        18,
        ls.frame_center_mgrs_zone,
        ls.frame_center_mgrs_band_grid.as_deref(),
    )?;
    emit_u8(14, ls.aircraft_mgrs_zone, out)?;
    emit_iso7(15, ls.aircraft_mgrs_band_grid.as_deref(), 3, out)?;
    emit_u24(16, ls.aircraft_mgrs_easting_m, out)?;
    emit_u24(17, ls.aircraft_mgrs_northing_m, out)?;
    emit_u8(18, ls.frame_center_mgrs_zone, out)?;
    emit_iso7(19, ls.frame_center_mgrs_band_grid.as_deref(), 3, out)?;
    emit_u24(20, ls.frame_center_mgrs_easting_m, out)?;
    emit_u24(21, ls.frame_center_mgrs_northing_m, out)?;

    for ud in &ls.user_defined {
        let body = encode_user_defined(ud)?;
        emit_ber_oid_tlv(11, &body, out)?;
    }
    for poi in &ls.points_of_interest {
        let body = encode_poi(poi)?;
        emit_ber_oid_tlv(12, &body, out)?;
    }
    for aoi in &ls.areas_of_interest {
        let body = encode_aoi(aoi)?;
        emit_ber_oid_tlv(13, &body, out)?;
    }
    for f in &ls.unknown {
        // Reject a typed tag before emitting it: without this guard, a
        // caller-constructed entry at e.g. Tag 2 in `unknown` would
        // produce a duplicate timestamp that silently overwrites the
        // typed field on decode (mirrors st0601::encode's
        // `write_unknown_fields` guard).
        if is_typed_tag(f.tag, super::tags::lookup) {
            return Err(KlvEncodeError::ReservedTagInUnknown { tag: f.tag });
        }
        emit_ber_oid_tlv(f.tag, &f.value, out)?;
    }
    Ok(())
}

/// Encode a User Defined Local Set (Table 8-4), the value of an RVT Tag
/// 11 occurrence. Both items are total by construction
/// (`numeric_id_raw: u8`, `data: Vec<u8>` default empty) — no mandatory-
/// item check is possible or needed (ST 0806.4-21..-24).
fn encode_user_defined(ud: &RvtUserData) -> Result<Vec<u8>, KlvEncodeError> {
    let mut body = Vec::new();
    emit_ber_oid_tlv(1, &[ud.numeric_id_raw], &mut body)?;
    emit_ber_oid_tlv(2, &ud.data, &mut body)?;
    Ok(body)
}

/// Encode a Point of Interest Local Set (Table 8-2), the value of an RVT
/// Tag 12 occurrence. Number/Latitude/Longitude are mandatory on every
/// encode (ST 0806.4-08..-10) — a recorded sentinel counts as present
/// for Latitude/Longitude (see [`emit_signed_range`]).
fn encode_poi(poi: &RvtPoi) -> Result<Vec<u8>, KlvEncodeError> {
    let Some(number) = poi.number else {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 1,
            name: "POI Number",
        });
    };
    if poi.lat_deg.is_none() && !poi.sentinel_tags.contains(&2) {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 2,
            name: "POI Latitude",
        });
    }
    if poi.lon_deg.is_none() && !poi.sentinel_tags.contains(&3) {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 3,
            name: "POI Longitude",
        });
    }

    let mut body = Vec::new();
    emit_ber_oid_tlv(1, &number.to_be_bytes(), &mut body)?;
    emit_signed_range(2, poi.lat_deg, &poi.sentinel_tags, &LAT_RANGE, &mut body)?;
    emit_signed_range(3, poi.lon_deg, &poi.sentinel_tags, &LON_RANGE, &mut body)?;
    if let Some(alt) = poi.alt_m {
        let mut buf = [0u8; 2];
        encode_fixed_range(&ALT_RANGE, 4, alt, &mut buf, OutOfRangePolicy::Error)?;
        emit_ber_oid_tlv(4, &buf, &mut body)?;
    }
    if let Some(t) = poi.poi_type {
        emit_ber_oid_tlv(5, &[t.to_wire()], &mut body)?;
    }
    emit_iso7(6, poi.text.as_deref(), 2048, &mut body)?;
    emit_iso7(7, poi.source_icon.as_deref(), 127, &mut body)?;
    emit_iso7(8, poi.source_id.as_deref(), 255, &mut body)?;
    emit_iso7(9, poi.label.as_deref(), 16, &mut body)?;
    emit_iso7(10, poi.operation_id.as_deref(), 127, &mut body)?;
    for f in &poi.unknown {
        // POI has no `tags.rs` table to run `is_typed_tag` against, so
        // the guard is the same 1..=10 range Table 8-2 defines (see
        // `apply_poi_tag`'s match arms) — same rationale as `write_body`'s
        // guard above.
        if (1..=10).contains(&f.tag) {
            return Err(KlvEncodeError::ReservedTagInUnknown { tag: f.tag });
        }
        emit_ber_oid_tlv(f.tag, &f.value, &mut body)?;
    }
    Ok(body)
}

/// Encode an Area of Interest Local Set (Table 8-3), the value of an RVT
/// Tag 13 occurrence. Number/both corner pairs/Type are mandatory on
/// every encode (ST 0806.4-13..-18) — same sentinel-counts-as-present
/// rule as [`encode_poi`].
fn encode_aoi(aoi: &RvtAoi) -> Result<Vec<u8>, KlvEncodeError> {
    let Some(number) = aoi.number else {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 1,
            name: "AOI Number",
        });
    };
    if aoi.corner_lat_p1_deg.is_none() && !aoi.sentinel_tags.contains(&2) {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 2,
            name: "AOI Point 1 (NW) Latitude",
        });
    }
    if aoi.corner_lon_p1_deg.is_none() && !aoi.sentinel_tags.contains(&3) {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 3,
            name: "AOI Point 1 (NW) Longitude",
        });
    }
    if aoi.corner_lat_p3_deg.is_none() && !aoi.sentinel_tags.contains(&4) {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 4,
            name: "AOI Point 3 (SE) Latitude",
        });
    }
    if aoi.corner_lon_p3_deg.is_none() && !aoi.sentinel_tags.contains(&5) {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 5,
            name: "AOI Point 3 (SE) Longitude",
        });
    }
    let Some(aoi_type) = aoi.aoi_type else {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 6,
            name: "AOI Type",
        });
    };

    let mut body = Vec::new();
    emit_ber_oid_tlv(1, &number.to_be_bytes(), &mut body)?;
    emit_signed_range(
        2,
        aoi.corner_lat_p1_deg,
        &aoi.sentinel_tags,
        &LAT_RANGE,
        &mut body,
    )?;
    emit_signed_range(
        3,
        aoi.corner_lon_p1_deg,
        &aoi.sentinel_tags,
        &LON_RANGE,
        &mut body,
    )?;
    emit_signed_range(
        4,
        aoi.corner_lat_p3_deg,
        &aoi.sentinel_tags,
        &LAT_RANGE,
        &mut body,
    )?;
    emit_signed_range(
        5,
        aoi.corner_lon_p3_deg,
        &aoi.sentinel_tags,
        &LON_RANGE,
        &mut body,
    )?;
    emit_ber_oid_tlv(6, &[aoi_type.to_wire()], &mut body)?;
    emit_iso7(7, aoi.text.as_deref(), 2048, &mut body)?;
    emit_iso7(8, aoi.source_id.as_deref(), 255, &mut body)?;
    emit_iso7(9, aoi.label.as_deref(), 16, &mut body)?;
    emit_iso7(10, aoi.operation_id.as_deref(), 127, &mut body)?;
    for f in &aoi.unknown {
        // AOI has no `tags.rs` table either; same 1..=10 range guard as
        // `encode_poi` (Table 8-3 defines the same tag-id universe).
        if (1..=10).contains(&f.tag) {
            return Err(KlvEncodeError::ReservedTagInUnknown { tag: f.tag });
        }
        emit_ber_oid_tlv(f.tag, &f.value, &mut body)?;
    }
    Ok(body)
}

/// Emit a signed lat/lon-style ranged tag: a populated value wins over a
/// recorded sentinel (mirrors `st0601::encode`'s `write_sentinel_tags`
/// value-wins convention). If the value is `None` but `tag` is recorded
/// in `sentinel_tags`, re-emit the spec's INT_MIN "error" indicator
/// instead of dropping the item; if neither, the tag is omitted (the
/// caller enforces mandatory presence before calling this for POI/AOI
/// lat/lon tags).
fn emit_signed_range(
    tag: u32,
    value: Option<f64>,
    sentinel_tags: &[u32],
    range: &LinearRange,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    if let Some(v) = value {
        let mut buf = [0u8; 4];
        encode_fixed_range(
            range,
            tag,
            v,
            &mut buf[..range.byte_length],
            OutOfRangePolicy::Error,
        )?;
        return emit_ber_oid_tlv(tag, &buf[..range.byte_length], out);
    }
    if sentinel_tags.contains(&tag) {
        let int_min_value: i64 = match range.byte_length {
            2 => i64::from(i16::MIN),
            4 => i64::from(i32::MIN),
            _ => return Ok(()),
        };
        let all = int_min_value.to_be_bytes();
        emit_ber_oid_tlv(tag, &all[8 - range.byte_length..], out)?;
    }
    Ok(())
}

fn emit_u8(tag: u32, v: Option<u8>, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    match v {
        Some(v) => emit_ber_oid_tlv(tag, &[v], out),
        None => Ok(()),
    }
}

fn emit_u16(tag: u32, v: Option<u16>, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    match v {
        Some(v) => emit_ber_oid_tlv(tag, &v.to_be_bytes(), out),
        None => Ok(()),
    }
}

fn emit_u32(tag: u32, v: Option<u32>, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    match v {
        Some(v) => emit_ber_oid_tlv(tag, &v.to_be_bytes(), out),
        None => Ok(()),
    }
}

/// Emit a 3-byte big-endian unsigned value (MGRS easting/northing).
/// `KlvEncodeError::OutOfRange` if `v` exceeds the Table 8-1 MGRS cap of
/// 99,999 m.
fn emit_u24(tag: u32, v: Option<u32>, out: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let Some(v) = v else { return Ok(()) };
    if v > 99_999 {
        return Err(KlvEncodeError::OutOfRange {
            tag,
            value: f64::from(v),
            min: 0.0,
            max: 99_999.0,
            hint: None,
        });
    }
    emit_ber_oid_tlv(tag, &v.to_be_bytes()[1..], out)
}

/// Refuse an MGRS zone (Item `zone_tag`) outside 1 to 60 and a band and
/// grid square (Item `zone_tag + 1`) that is not three letters A to Z
/// without I and O (ST 0806.4 Table 8-1). A polar position has no zone.
fn check_mgrs(
    zone_tag: u32,
    zone: Option<u8>,
    band_grid: Option<&str>,
) -> Result<(), KlvEncodeError> {
    use super::model::{is_mgrs_band_grid, is_mgrs_zone};
    if let Some(z) = zone
        && !is_mgrs_zone(z)
    {
        return Err(KlvEncodeError::OutOfRange {
            tag: zone_tag,
            value: f64::from(z),
            min: 1.0,
            max: 60.0,
            hint: Some("an MGRS zone is a UTM zone, 01 to 60"),
        });
    }
    if let Some(s) = band_grid
        && !is_mgrs_band_grid(s)
    {
        return Err(KlvEncodeError::InvalidFieldValue {
            tag: zone_tag + 1,
            reason: "an MGRS latitude band and grid square are three letters A to Z without I and O",
        });
    }
    Ok(())
}

/// Emit an ISO-7 (ASCII) string tag. `KlvEncodeError::StringTooLong` if
/// `v` exceeds `max_bytes`, `KlvEncodeError::InvalidFieldValue` if it is
/// not ASCII (ISO-7 is ISO/IEC 646).
fn emit_iso7(
    tag: u32,
    v: Option<&str>,
    max_bytes: usize,
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let Some(s) = v else { return Ok(()) };
    if s.len() > max_bytes {
        return Err(KlvEncodeError::StringTooLong {
            tag,
            max: max_bytes,
        });
    }
    if !s.is_ascii() {
        return Err(KlvEncodeError::InvalidFieldValue {
            tag,
            reason: "ISO-7 text must be ASCII",
        });
    }
    emit_ber_oid_tlv(tag, s.as_bytes(), out)
}
