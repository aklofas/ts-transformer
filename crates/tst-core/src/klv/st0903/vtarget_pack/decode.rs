//! ST 0903.6 VTargetPack decode: `read_pack` entry point.

use super::model::{PackEncoding, VTargetPack, VTargetPackError, pack_lookup};
use crate::error::KlvFieldError;

/// Decode a single VTargetPack from `bytes`. Returns the decoded pack
/// and the number of bytes consumed.
///
/// Wire form per ST 0903.6 §10.2 Table 10:
/// - Leading BER-OID `targetId` (no Tag, per §10.2.2.1).
/// - Then a Local Set–encoded body where each field is
///   `[BER-OID tag][BER short/long length][value bytes]` per
///   ST 0107.5 §6.3.1.
///
/// Tags 1..=107 (the §10.2 typed universe) all fit in a single BER-OID
/// byte (`write_ber_oid(N) == [N]` for `N ≤ 127`), so legacy wire bytes
/// parse byte-identical. A future ST 0903.7+ pack tag ≥ 128 would
/// encode as multi-byte BER-OID — the walker decodes it correctly and
/// preserves it as a forward-compat unknown.
///
/// Unknown / deprecated tags (e.g. 21, 102, 103) are preserved in
/// `pack.unknown` per ST 0107.5 §6 future-proof skip rule.
pub(crate) fn read_pack(bytes: &[u8]) -> Result<(VTargetPack, usize), VTargetPackError> {
    read_pack_inner(bytes, /* strict = */ false)
}

/// Strict variant of [`read_pack`]: the first per-field value error
/// aborts with the typed [`VTargetPackError`] instead of landing in
/// `field_errors`. Framing errors abort in both modes.
pub(crate) fn read_pack_strict(bytes: &[u8]) -> Result<(VTargetPack, usize), VTargetPackError> {
    read_pack_inner(bytes, /* strict = */ true)
}

fn read_pack_inner(bytes: &[u8], strict: bool) -> Result<(VTargetPack, usize), VTargetPackError> {
    use crate::klv::length::{read_ber, read_ber_oid, read_ber_oid_u64};

    // 1. Read the leading BER-OID Target ID (u64 — up to 10 BER-OID bytes).
    let (target_id, rest) =
        read_ber_oid_u64(bytes).map_err(|_| VTargetPackError::TruncatedTargetId)?;
    let header_consumed = bytes.len() - rest.len();

    let mut pack = VTargetPack {
        target_id,
        ..Default::default()
    };

    // 2. Walk the LS-encoded body. Each field is a BER-OID tag per
    //    ST 0107.5 §6.3.1 + BER-encoded length + value. Mirrors the
    //    top-level ST 0903 walker (post-E5) and the sibling ST 0102
    //    / ST 0601 walkers.
    let mut cursor = rest;
    let mut consumed = header_consumed;
    while !cursor.is_empty() {
        let (tag, after_tag) = read_ber_oid(cursor).map_err(|_| {
            // We don't know the tag yet — surface tag=0 as the closest
            // available sentinel. Production wire shouldn't trip this
            // (BER-OID needs a continuation-byte stream to fail).
            VTargetPackError::TruncatedField { tag: 0 }
        })?;
        let tag_consumed = cursor.len() - after_tag.len();
        cursor = after_tag;
        consumed += tag_consumed;

        let (declared_len, after_len) =
            read_ber(cursor).map_err(|_| VTargetPackError::TruncatedField { tag })?;
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

        if let Err(e) = decode_field(tag, value, &mut pack) {
            if strict {
                return Err(e);
            }
            // Lenient: record and keep walking — the sibling convention
            // (`SecurityLs::field_errors`, `VmtiLs::field_errors`). CORR-29e.
            pack.field_errors.push(field_error(e, value.len()));
        }
    }

    Ok((pack, consumed))
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
        VTargetPackError::MalformedUtf8 { tag } => KlvFieldError::InvalidUtf8 { tag },
        VTargetPackError::TruncatedField { tag } => KlvFieldError::TruncatedField { tag },
        // Framing errors never reach here — `read_pack_inner` returns them
        // before `decode_field` runs.
        VTargetPackError::TruncatedTargetId | VTargetPackError::LengthOverrun { .. } => {
            KlvFieldError::TruncatedField { tag: 0 }
        }
    }
}

/// Dispatch a single TLV field's value bytes to the matching
/// `VTargetPack` field based on the spec's encoding for that tag.
/// Unknown / deprecated tags fall through to `pack.unknown` per
/// ST 0107.5 §6.
///
/// `tag` arrives as BER-OID-decoded `u32`. Tags > 0xFF (future ST
/// 0903.7+ multi-byte BER-OID) are preserved in `pack.unknown` per
/// ST 0107.5 §6; the typed table only covers the u8 universe.
fn decode_field(tag: u32, value: &[u8], pack: &mut VTargetPack) -> Result<(), VTargetPackError> {
    use crate::klv::imapb::{ImapbParams, decode_imapb};
    use crate::klv::pack::OwnedRawField;

    // Forward-compat tags (≥ 128 in BER-OID) — preserve verbatim.
    // The typed table only covers tag IDs ≤ 127 (the §10.2 universe
    // tops out at 107), so a multi-byte BER-OID tag from a future spec
    // bumps directly to `unknown` without spec-table lookup.
    let Ok(tag_u8) = u8::try_from(tag) else {
        pack.unknown.push(OwnedRawField {
            tag,
            value: value.to_vec(),
        });
        return Ok(());
    };

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
            // A7: decode_imapb returns DecodedImapb (ST 1201.5 §7.2.2/.3
            // special values + bounds check). VTargetPack treats every
            // non-Value result as MalformedImapb — special-value
            // signaling at the per-target pack layer isn't a use case
            // the API surfaces today; callers needing to differentiate
            // can pattern-match the enum directly.
            let v = decode_imapb(&params, value)
                .map_err(|_| VTargetPackError::MalformedImapb { tag })?
                .value()
                .ok_or(VTargetPackError::MalformedImapb { tag })?;
            match tag_u8 {
                10 => pack.centroid_lat_offset = Some(v),
                11 => pack.centroid_lon_offset = Some(v),
                12 => pack.centroid_hae = Some(v),
                13 => pack.bbox_top_left_lat_offset = Some(v),
                14 => pack.bbox_top_left_lon_offset = Some(v),
                15 => pack.bbox_bottom_right_lat_offset = Some(v),
                16 => pack.bbox_bottom_right_lon_offset = Some(v),
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
