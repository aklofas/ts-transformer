//! ST 0903.6 VTargetPack decode: `read_pack` entry point.

use super::model::{PackEncoding, VTargetPack, VTargetPackError, pack_lookup};
use crate::error::KlvFieldError;
use crate::klv::st0903::decode::special_field_error;

/// Decode a single VTargetPack from `bytes`. Returns the decoded pack
/// and the number of bytes consumed.
///
/// Wire form per ST 0903.6 §10.2 Table 10:
/// - Leading BER-OID `targetId` (no Tag, per §10.2.2.1).
/// - Then a Local Set–encoded body where each field is
///   `[one-byte tag][BER short/long length][value bytes]`: §10.2 follows
///   the embedded-LS rule of §9.2 (label byte 6 = `0x03`, one-byte tags).
///
/// A future pack tag from 128 to 255 is one byte too, and is preserved
/// as a forward-compat unknown.
///
/// Unknown / deprecated tags (e.g. 21, 102, 103) are preserved in
/// `pack.unknown` per ST 0107.5 §6 future-proof skip rule.
///
/// A zero-length item means "unknown" (ST 0107.4-17), as at the top level:
/// its field is unset, with no field error. No pack item is mandatory
/// (ST 0903.6 §10.2), so the strict reader accepts it too.
///
/// An IMAPB item (10 to 16) sent as an ST 1201 special (ST 0903.6 §9.1.1)
/// is legal: its field is unset, and this reader lists the item in
/// `pack.field_errors` (`OutOfRange` with its tag and range), because
/// [`super::write_pack`] writes only set fields and so does not write it
/// back.
pub(crate) fn read_pack(bytes: &[u8]) -> Result<(VTargetPack, usize), VTargetPackError> {
    read_pack_inner(bytes, /* strict = */ false)
}

/// Strict variant of [`read_pack`]: the first per-field value error
/// aborts with the typed [`VTargetPackError`] instead of landing in
/// `field_errors`. Framing errors abort in both modes. Strict also
/// refuses a targetId over nine bytes or padded, a pack with no item, a
/// repeated item (known or unknown), a length not in its shortest form and
/// a one-byte item outside its range.
pub(crate) fn read_pack_strict(bytes: &[u8]) -> Result<(VTargetPack, usize), VTargetPackError> {
    read_pack_inner(bytes, /* strict = */ true)
}

fn read_pack_inner(bytes: &[u8], strict: bool) -> Result<(VTargetPack, usize), VTargetPackError> {
    use crate::error::KlvDecodeError;
    use crate::klv::length::{read_ber, read_ber_oid_u64, read_ber_strict};

    // 1. Read the leading BER-OID Target ID (u64 — up to 10 BER-OID bytes).
    let (target_id, rest) =
        read_ber_oid_u64(bytes).map_err(|_| VTargetPackError::TruncatedTargetId)?;
    let header_consumed = bytes.len() - rest.len();
    if strict {
        // Table 10: the targetId is BER-OID V9, and a BER-OID has no
        // leading padding group.
        if header_consumed > 9 {
            return Err(VTargetPackError::TargetIdTooWide);
        }
        if bytes[0] == 0x80 {
            return Err(VTargetPackError::NonCanonicalLength { tag: 0 });
        }
        if rest.is_empty() {
            return Err(VTargetPackError::Empty);
        }
    }
    // Strict: every tag once per pack (ST 0903.5-98).
    let mut seen = [false; 256];

    let mut pack = VTargetPack {
        target_id,
        ..Default::default()
    };

    // 2. Walk the LS-encoded body. Each field is a one-byte tag
    //    (ST 0903.6 §10.2 → §9.2) + BER-encoded length + value.
    let mut cursor = rest;
    let mut consumed = header_consumed;
    while !cursor.is_empty() {
        let tag_byte = cursor[0];
        let tag = u32::from(tag_byte);
        cursor = &cursor[1..];
        consumed += 1;

        let (declared_len, after_len) = if strict {
            read_ber_strict(cursor).map_err(|e| match e {
                KlvDecodeError::NonCanonicalLength { .. } => {
                    VTargetPackError::NonCanonicalLength { tag }
                }
                _ => VTargetPackError::TruncatedField { tag },
            })?
        } else {
            read_ber(cursor).map_err(|_| VTargetPackError::TruncatedField { tag })?
        };
        if strict && core::mem::replace(&mut seen[usize::from(tag_byte)], true) {
            return Err(VTargetPackError::DuplicateTag { tag });
        }
        let len_consumed = cursor.len() - after_len.len();
        cursor = after_len;
        consumed += len_consumed;

        if cursor.len() < declared_len {
            return Err(VTargetPackError::LengthOverrun {
                tag,
                declared: declared_len,
                available: cursor.len(),
            });
        }
        let value = &cursor[..declared_len];
        cursor = &cursor[declared_len..];
        consumed += declared_len;

        // ST 0107.4-17: a zero-length item means "value unknown". It is an
        // occurrence of its tag like any other, so as the last one it leaves
        // the field unset. Tested here, where the length is read: in the
        // field reader the test costs the VMTI decode workload
        // (`benches/ir.rs`) six instructions an item. An unknown item of no
        // bytes goes on to `pack.unknown`.
        if declared_len == 0 && pack_lookup(tag_byte).is_some() {
            unset(&mut pack, tag_byte);
            continue;
        }

        if let Err(e) = decode_field(tag, value, &mut pack, strict) {
            if strict {
                return Err(e);
            }
            // Lenient: record and keep walking — the sibling convention
            // (`SecurityLs::field_errors`, `VmtiLs::field_errors`). The last
            // occurrence of a tag decides, so a value an earlier one gave
            // does not stay (only a defined item is refused).
            unset(&mut pack, tag_byte);
            pack.field_errors.push(field_error(e, value.len()));
        }
    }

    Ok((pack, consumed))
}

/// The first item of `bytes`, a pack [`read_pack_strict`] accepted, that
/// standalone carriage omits (ST 0903.6-116: Items 10, 11, 13 to 16), as
/// it is on the wire, whatever value it holds.
pub(crate) fn first_standalone_offset_item(bytes: &[u8]) -> Option<u32> {
    use crate::klv::length::{read_ber, read_ber_oid_u64};

    let (_, mut cursor) = read_ber_oid_u64(bytes).ok()?;
    while let Some((&tag, rest)) = cursor.split_first() {
        if matches!(tag, 10 | 11 | 13..=16) {
            return Some(u32::from(tag));
        }
        let (len, value) = read_ber(rest).ok()?;
        cursor = value.get(len..)?;
    }
    None
}

/// Project a per-field [`VTargetPackError`] onto the shared
/// [`KlvFieldError`] vocabulary `field_errors` carries. `got` is the
/// field's wire value length. IMAPB failures map to `InvalidLength`
/// against the tag's fixed IMAPB width, mirroring the top-level ST 0903
/// walker's IMAPB mapping.
fn field_error(e: VTargetPackError, got: usize) -> KlvFieldError {
    match e {
        VTargetPackError::InvalidLength { tag, expected, got } => {
            KlvFieldError::InvalidLength { tag, expected, got }
        }
        VTargetPackError::MalformedImapb { tag } => KlvFieldError::InvalidLength {
            tag,
            // Tag 12 (`targetHae`) is 2-byte IMAPB; every other IMAPB pack
            // tag is 3 bytes (§10.2.2.11–.17) — same table as `decode_field`.
            expected: if tag == 12 { 2 } else { 3 },
            got,
        },
        VTargetPackError::TruncatedField { tag } => KlvFieldError::TruncatedField { tag },
        // Framing and strict-only errors never reach here:
        // `read_pack_inner` returns them before `decode_field` runs, or in
        // strict mode only.
        VTargetPackError::TruncatedTargetId
        | VTargetPackError::LengthOverrun { .. }
        | VTargetPackError::DuplicateTag { .. }
        | VTargetPackError::NonCanonicalLength { .. }
        | VTargetPackError::Empty
        | VTargetPackError::TargetIdTooWide
        | VTargetPackError::ValueOutOfRange { .. }
        | VTargetPackError::DuplicateTargetId { .. } => KlvFieldError::TruncatedField { tag: 0 },
    }
}

/// Dispatch a single TLV field's value bytes to the matching
/// `VTargetPack` field based on the spec's encoding for that tag.
/// Unknown / deprecated tags fall through to `pack.unknown` per
/// ST 0107.5 §6.
///
/// `tag` is the field's one tag byte, widened to the `u32` the error
/// and `unknown` types carry.
fn decode_field(
    tag: u32,
    value: &[u8],
    pack: &mut VTargetPack,
    strict: bool,
) -> Result<(), VTargetPackError> {
    use crate::klv::imapb::{DecodedImapb, ImapbParams, decode_imapb};
    use crate::klv::pack::OwnedRawField;

    // The walker reads one tag byte, so the tag is never above 255.
    let tag_u8 = tag as u8;

    let Some(spec) = pack_lookup(tag_u8) else {
        // ST 0107.5 §6 skip rule — preserve unknown / deprecated tags.
        pack.unknown.push(OwnedRawField {
            tag,
            value: value.to_vec(),
        });
        return Ok(());
    };

    match spec.encoding {
        PackEncoding::U8 => {
            if value.len() != 1 {
                return Err(VTargetPackError::InvalidLength {
                    tag,
                    expected: 1,
                    got: value.len(),
                });
            }
            let v = value[0];
            if strict && !super::model::vtarget_u8_in_range(tag, v) {
                return Err(VTargetPackError::ValueOutOfRange { tag, value: v });
            }
            match tag_u8 {
                4 => pack.priority = Some(v),
                5 => pack.confidence_level = Some(v),
                7 => pack.percentage_of_target_pixels = Some(v),
                23 => pack.detection_status = Some(v),
                _ => unreachable!("U8 dispatch missing tag {tag_u8}"),
            }
        }
        PackEncoding::VarUint { max_bytes } => {
            if value.is_empty() || value.len() > max_bytes as usize {
                return Err(VTargetPackError::InvalidLength {
                    tag,
                    expected: max_bytes as usize,
                    got: value.len(),
                });
            }
            let read_var = || {
                crate::klv::length::read_var_uint(value, max_bytes as usize, tag)
                    .map_err(|_| VTargetPackError::TruncatedField { tag })
            };
            match tag_u8 {
                // Pixel fields — V6 (up to 6 bytes, u64 model).
                1 => pack.centroid_pixel = Some(read_var()?),
                2 => pack.bbox_top_left_pixel = Some(read_var()?),
                3 => pack.bbox_bottom_right_pixel = Some(read_var()?),
                19 => pack.centroid_pix_row = Some(read_var()?),
                20 => pack.centroid_pix_col = Some(read_var()?),
                // Non-pixel var fields — stay u32.
                6 => pack.history = Some(read_var()? as u16), // V2 caps at u16
                9 => pack.target_intensity = Some(read_var()? as u32),
                22 => pack.algorithm_id = Some(read_var()? as u32),
                _ => unreachable!("VarUint dispatch missing tag {tag_u8}"),
            }
        }
        PackEncoding::U24Rgb => {
            if value.len() != 3 {
                return Err(VTargetPackError::InvalidLength {
                    tag,
                    expected: 3,
                    got: value.len(),
                });
            }
            pack.target_color = Some([value[0], value[1], value[2]]);
        }
        PackEncoding::ImapbF64 { min, max } => {
            // Tag 12 (`targetHae`) uses 2-byte IMAPB; all other IMAPB
            // pack tags use 3-byte IMAPB per §10.2.2.11–.17.
            let length = if tag_u8 == 12 { 2 } else { 3 };
            let params = ImapbParams { min, max, length };
            // ST 0903.6 §9.1.1: IMAP_BELOW_MINIMUM, IMAP_ABOVE_MAXIMUM and
            // the "Invalid" user-defined special (0xC1) are sent in place
            // of the measurement, and every ST 1201 special is a legal
            // value: the item is valid and its field holds no measurement.
            // A reserved pattern or an out-of-range result is refused.
            let v = match decode_imapb(&params, value)
                .map_err(|_| VTargetPackError::MalformedImapb { tag })?
            {
                DecodedImapb::Value(v) => Some(v),
                DecodedImapb::Special(special) => {
                    // No encoder writes the special back, so the lenient
                    // reader lists the item; the strict one has nothing to
                    // refuse and fills no `field_errors`.
                    if !strict {
                        pack.field_errors
                            .push(special_field_error(tag, special, min, max));
                    }
                    None
                }
                _ => return Err(VTargetPackError::MalformedImapb { tag }),
            };
            match tag_u8 {
                10 => pack.centroid_lat_offset = v,
                11 => pack.centroid_lon_offset = v,
                12 => pack.centroid_hae = v,
                13 => pack.bbox_top_left_lat_offset = v,
                14 => pack.bbox_top_left_lon_offset = v,
                15 => pack.bbox_bottom_right_lat_offset = v,
                16 => pack.bbox_bottom_right_lon_offset = v,
                _ => unreachable!("ImapbF64 dispatch missing tag {tag_u8}"),
            }
        }
        PackEncoding::RawBytes => {
            let bytes = value.to_vec();
            match tag_u8 {
                17 => pack.target_location = Some(bytes),
                18 => pack.geospatial_contour_series = Some(bytes),
                101 => pack.vmask = Some(bytes),
                104 => pack.vtracker = Some(bytes),
                105 => pack.vchip = Some(bytes),
                106 => pack.vchip_series = Some(bytes),
                107 => pack.vobject_series = Some(bytes),
                _ => unreachable!("RawBytes dispatch missing tag {tag_u8}"),
            }
        }
    }
    Ok(())
}

/// Unset the field of the defined pack item `tag`.
fn unset(pack: &mut VTargetPack, tag: u8) {
    match tag {
        1 => pack.centroid_pixel = None,
        2 => pack.bbox_top_left_pixel = None,
        3 => pack.bbox_bottom_right_pixel = None,
        4 => pack.priority = None,
        5 => pack.confidence_level = None,
        6 => pack.history = None,
        7 => pack.percentage_of_target_pixels = None,
        8 => pack.target_color = None,
        9 => pack.target_intensity = None,
        10 => pack.centroid_lat_offset = None,
        11 => pack.centroid_lon_offset = None,
        12 => pack.centroid_hae = None,
        13 => pack.bbox_top_left_lat_offset = None,
        14 => pack.bbox_top_left_lon_offset = None,
        15 => pack.bbox_bottom_right_lat_offset = None,
        16 => pack.bbox_bottom_right_lon_offset = None,
        17 => pack.target_location = None,
        18 => pack.geospatial_contour_series = None,
        19 => pack.centroid_pix_row = None,
        20 => pack.centroid_pix_col = None,
        22 => pack.algorithm_id = None,
        23 => pack.detection_status = None,
        101 => pack.vmask = None,
        104 => pack.vtracker = None,
        105 => pack.vchip = None,
        106 => pack.vchip_series = None,
        107 => pack.vobject_series = None,
        _ => unreachable!("unset missing pack tag {tag}"),
    }
}
