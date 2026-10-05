//! ST 0903.6 VTargetPack encode: `write_pack` and `encoded_len`.

use super::model::VTargetPack;
use crate::error::KlvEncodeError;
use alloc::vec::Vec;

/// Encode a single VTargetPack into `out`. Returns bytes written.
///
/// Fields are emitted in ascending tag order (1, 2, 3, ..., 23, 101,
/// 104, 105, 106, 107), then any preserved `unknown` tags last per
/// ST 0107.5 §6.
pub(crate) fn write_pack(pack: &VTargetPack, out: &mut Vec<u8>) -> Result<usize, KlvEncodeError> {
    use crate::klv::length::write_ber_oid_u64;
    use crate::klv::pack::{emit_ber_oid_tlv, is_typed_tag};
    use crate::klv::st0903::emit::{emit_imapb_n, emit_tlv, emit_var, emit_var_u64};

    // Per-class wire-width caps. Encode must enforce these so the encoder
    // and decoder are a fixpoint — the decoder rejects the over-width
    // byte lengths these fields would otherwise produce.
    //   V3 max = 2^24−1 (Tags 9, 22)
    //   V4 max = 2^32−1 = u32::MAX (Tags 19, 20)
    //   V6 max = 2^48−1 (Tags 1, 2, 3)
    const V3_MAX: u64 = (1u64 << 24) - 1; // 16_777_215
    const V4_MAX: u64 = u32::MAX as u64; // 4_294_967_295
    const V6_MAX: u64 = (1u64 << 48) - 1; // 281_474_976_710_655

    let start = out.len();

    // 1. BER-OID Target ID — u64 needs up to 10 BER-OID bytes (ceil(64/7)).
    let mut buf = [0u8; 10];
    let n = write_ber_oid_u64(pack.target_id, &mut buf)?;
    out.extend_from_slice(&buf[..n]);

    if let Some(v) = pack.centroid_pixel {
        if v > V6_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 1,
                value: v as f64,
                min: 0.0,
                max: V6_MAX as f64,
                hint: None,
            });
        }
        emit_var_u64(out, 1, v)?;
    }
    if let Some(v) = pack.bbox_top_left_pixel {
        if v > V6_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 2,
                value: v as f64,
                min: 0.0,
                max: V6_MAX as f64,
                hint: None,
            });
        }
        emit_var_u64(out, 2, v)?;
    }
    if let Some(v) = pack.bbox_bottom_right_pixel {
        if v > V6_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 3,
                value: v as f64,
                min: 0.0,
                max: V6_MAX as f64,
                hint: None,
            });
        }
        emit_var_u64(out, 3, v)?;
    }
    if let Some(v) = pack.priority {
        emit_tlv(out, 4, &[v])?;
    }
    if let Some(v) = pack.confidence_level {
        emit_tlv(out, 5, &[v])?;
    }
    if let Some(v) = pack.history {
        emit_var(out, 6, v as u32)?;
    }
    if let Some(v) = pack.percentage_of_target_pixels {
        emit_tlv(out, 7, &[v])?;
    }
    if let Some(v) = pack.target_color {
        emit_tlv(out, 8, &v)?;
    }
    if let Some(v) = pack.target_intensity {
        if v as u64 > V3_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 9,
                value: v as f64,
                min: 0.0,
                max: V3_MAX as f64,
                hint: None,
            });
        }
        emit_var(out, 9, v)?;
    }

    // IMAPB fields. Tags 10/11/13/14/15/16 use 3-byte IMAPB per
    // §10.2.2.11/.12/.14/.15/.16/.17 over [-19.2°, 19.2°]. Tag 12
    // uses 2-byte IMAPB per §10.2.2.13 over [-900 m, 19000 m].
    if let Some(v) = pack.centroid_lat_offset {
        emit_imapb_n(out, 10, v, -19.2, 19.2, 3)?;
    }
    if let Some(v) = pack.centroid_lon_offset {
        emit_imapb_n(out, 11, v, -19.2, 19.2, 3)?;
    }
    if let Some(v) = pack.centroid_hae {
        emit_imapb_n(out, 12, v, -900.0, 19000.0, 2)?;
    }
    if let Some(v) = pack.bbox_top_left_lat_offset {
        emit_imapb_n(out, 13, v, -19.2, 19.2, 3)?;
    }
    if let Some(v) = pack.bbox_top_left_lon_offset {
        emit_imapb_n(out, 14, v, -19.2, 19.2, 3)?;
    }
    if let Some(v) = pack.bbox_bottom_right_lat_offset {
        emit_imapb_n(out, 15, v, -19.2, 19.2, 3)?;
    }
    if let Some(v) = pack.bbox_bottom_right_lon_offset {
        emit_imapb_n(out, 16, v, -19.2, 19.2, 3)?;
    }

    if let Some(ref bytes) = pack.target_location {
        emit_tlv(out, 17, bytes)?;
    }
    if let Some(ref bytes) = pack.geospatial_contour_series {
        emit_tlv(out, 18, bytes)?;
    }
    if let Some(v) = pack.centroid_pix_row {
        if v > V4_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 19,
                value: v as f64,
                min: 0.0,
                max: V4_MAX as f64,
                hint: None,
            });
        }
        emit_var_u64(out, 19, v)?;
    }
    if let Some(v) = pack.centroid_pix_col {
        if v > V4_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 20,
                value: v as f64,
                min: 0.0,
                max: V4_MAX as f64,
                hint: None,
            });
        }
        emit_var_u64(out, 20, v)?;
    }
    if let Some(v) = pack.algorithm_id {
        if v as u64 > V3_MAX {
            return Err(KlvEncodeError::OutOfRange {
                tag: 22,
                value: v as f64,
                min: 0.0,
                max: V3_MAX as f64,
                hint: None,
            });
        }
        emit_var(out, 22, v)?;
    }
    if let Some(v) = pack.detection_status {
        emit_tlv(out, 23, &[v])?;
    }
    if let Some(ref bytes) = pack.vmask {
        emit_tlv(out, 101, bytes)?;
    }
    if let Some(ref bytes) = pack.vtracker {
        emit_tlv(out, 104, bytes)?;
    }
    if let Some(ref bytes) = pack.vchip {
        emit_tlv(out, 105, bytes)?;
    }
    if let Some(ref bytes) = pack.vchip_series {
        emit_tlv(out, 106, bytes)?;
    }
    if let Some(ref bytes) = pack.vobject_series {
        emit_tlv(out, 107, bytes)?;
    }

    // Unknown tags preserved last (ST 0107.5 §6). Tag IDs use multi-
    // byte BER-OID per ST 0107.5 §6.3.1 for values ≥ 128, so a future
    // ST 0903.7+ pack tag in the unknown bucket round-trips losslessly.
    // Tags 1..=107 (the §10.2 typed universe) are all ≤ 127 and encode
    // as a single byte, byte-identical to a raw single-byte tag.
    // `encoded_len` mirrors this via `ber_oid_len(field.tag)`.
    for field in &pack.unknown {
        // Reject reserved/typed pack tags before emitting. Without this
        // guard, a caller-constructed typed tag (e.g. Tag 5 = Confidence
        // Level) in `unknown` would produce a duplicate. The `unknown`
        // vec is for forward-compat pass-through only. Mirrors
        // st0601::encode::write_unknown_fields and st0102/st0903 guards.
        if is_typed_tag(field.tag, super::model::pack_lookup) {
            return Err(KlvEncodeError::ReservedTagInUnknown { tag: field.tag });
        }
        emit_ber_oid_tlv(field.tag, &field.value, out)?;
    }

    Ok(out.len() - start)
}

/// Number of bytes `pack` would occupy when encoded. Mirrors
/// `write_pack`'s field-by-field structure.
pub(crate) fn encoded_len(pack: &VTargetPack) -> usize {
    use crate::klv::length::{ber_len, ber_oid_len, ber_oid_len_u64, var_uint_min_len};

    fn tlv_len(value_len: usize) -> usize {
        1 /* tag */ + ber_len(value_len) + value_len
    }

    let mut total = ber_oid_len_u64(pack.target_id);
    if let Some(v) = pack.centroid_pixel {
        total += tlv_len(var_uint_min_len(v));
    }
    if let Some(v) = pack.bbox_top_left_pixel {
        total += tlv_len(var_uint_min_len(v));
    }
    if let Some(v) = pack.bbox_bottom_right_pixel {
        total += tlv_len(var_uint_min_len(v));
    }
    if pack.priority.is_some() {
        total += tlv_len(1);
    }
    if pack.confidence_level.is_some() {
        total += tlv_len(1);
    }
    if let Some(v) = pack.history {
        total += tlv_len(var_uint_min_len(v as u64));
    }
    if pack.percentage_of_target_pixels.is_some() {
        total += tlv_len(1);
    }
    if pack.target_color.is_some() {
        total += tlv_len(3);
    }
    if let Some(v) = pack.target_intensity {
        total += tlv_len(var_uint_min_len(v as u64));
    }
    if pack.centroid_lat_offset.is_some() {
        total += tlv_len(3);
    }
    if pack.centroid_lon_offset.is_some() {
        total += tlv_len(3);
    }
    if pack.centroid_hae.is_some() {
        total += tlv_len(2);
    }
    if pack.bbox_top_left_lat_offset.is_some() {
        total += tlv_len(3);
    }
    if pack.bbox_top_left_lon_offset.is_some() {
        total += tlv_len(3);
    }
    if pack.bbox_bottom_right_lat_offset.is_some() {
        total += tlv_len(3);
    }
    if pack.bbox_bottom_right_lon_offset.is_some() {
        total += tlv_len(3);
    }
    if let Some(ref b) = pack.target_location {
        total += tlv_len(b.len());
    }
    if let Some(ref b) = pack.geospatial_contour_series {
        total += tlv_len(b.len());
    }
    if let Some(v) = pack.centroid_pix_row {
        total += tlv_len(var_uint_min_len(v));
    }
    if let Some(v) = pack.centroid_pix_col {
        total += tlv_len(var_uint_min_len(v));
    }
    if let Some(v) = pack.algorithm_id {
        total += tlv_len(var_uint_min_len(v as u64));
    }
    if pack.detection_status.is_some() {
        total += tlv_len(1);
    }
    if let Some(ref b) = pack.vmask {
        total += tlv_len(b.len());
    }
    if let Some(ref b) = pack.vtracker {
        total += tlv_len(b.len());
    }
    if let Some(ref b) = pack.vchip {
        total += tlv_len(b.len());
    }
    if let Some(ref b) = pack.vchip_series {
        total += tlv_len(b.len());
    }
    if let Some(ref b) = pack.vobject_series {
        total += tlv_len(b.len());
    }
    // Unknown tags use BER-OID tag + BER length + value (mirrors
    // `write_pack`). For tags ≤ 127 (the §10.2 typed universe),
    // `ber_oid_len(tag) == 1` so this collapses to the same byte
    // count as `tlv_len(value.len())`.
    for field in &pack.unknown {
        total += ber_oid_len(field.tag) + ber_len(field.value.len()) + field.value.len();
    }
    total
}
