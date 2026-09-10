//! ST 0601 decode entry points — 4 modes per the typed-set's
//! lenient/strict/compliance lineage.

use crate::error::{KlvDecodeError, KlvFieldError};
use crate::klv::length::{read_ber, read_ber_strict, read_strict_tlv, read_var_int, read_var_uint};
use crate::klv::pack::{Iter, OwnedRawField};
use crate::klv::universal_label::UniversalLabel;
use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use super::mapping::decode_fixed_range;
use super::model::{
    IcingDetected, OperationalMode, PlatformStatus, SdccFlpField, SensorControlMode, SensorFovName,
    UasDatalinkLs,
};
use super::packs;
use super::tags::{Encoding, lookup};

/// Decode a UAS Datalink Local Set per MISB ST 0601.
///
/// Lenient: any 16-byte UL is accepted, the running-sum 16-bit checksum
/// (Tag 1) is verified, unknown tags are preserved verbatim in
/// [`UasDatalinkLs::unknown`], and per-tag value-validation failures are
/// collected in [`UasDatalinkLs::field_errors`] rather than failing the
/// whole record. Use [`decode_strict`] to additionally require the
/// ST 0601 family UL pattern, or [`decode_strict_compliance`] to enforce
/// the spec's mandatory tag-ordering rules.
///
/// # Example — decode a fixture and inspect typed fields
///
/// ```
/// use tst_core::klv::st0601;
/// use tst_core::UasDatalinkLs;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // Build a known-good fixture by round-tripping through the encoder.
/// let mut original = UasDatalinkLs::default();
/// original.timestamp_us = Some(1_700_000_000_000_000);
/// original.platform_heading_deg = Some(125.5);
/// original.sensor_lat_deg = Some(34.05);
/// original.sensor_lon_deg = Some(-118.25);
///
/// let bytes = st0601::encode_to_vec(&original)?;
/// let decoded = st0601::decode(&bytes)?;
///
/// // Integer tags round-trip exactly; IMAPB-encoded floats round-trip
/// // within the ST 0601 spec's bit-width quantization (~0.0055° at
/// // 16-bit heading resolution).
/// assert_eq!(decoded.timestamp_us, Some(1_700_000_000_000_000));
/// assert!((decoded.platform_heading_deg.unwrap() - 125.5).abs() < 0.01);
///
/// // No per-field decode failures on a well-formed record.
/// assert!(decoded.field_errors.is_empty());
/// // No unknown tags either — every emitted tag is in the typed table.
/// assert!(decoded.unknown.is_empty());
/// # Ok(())
/// # }
/// ```
///
/// # C ABI
///
/// `tst_st0601_decode` — see `bindings/c/include/tstrans.h`.
pub fn decode(buf: &[u8]) -> Result<UasDatalinkLs, KlvDecodeError> {
    decode_inner(
        buf, /* verify_checksum */ true, /* strict_ul */ false,
    )
}

/// Decode without verifying the running-sum 16 checksum (Tag 1) and
/// without ST 0601-family UL gating. Use only when ingesting bytes that
/// are known to be intra-sender consistent (e.g. fixture round-trips
/// in tests).
///
/// # Errors
/// Returns the same structural [`KlvDecodeError`] variants as
/// [`decode`] — [`KlvDecodeError::Truncated`],
/// [`KlvDecodeError::MalformedLength`], [`KlvDecodeError::MalformedTag`],
/// [`KlvDecodeError::LengthOverflow`] — but never
/// [`KlvDecodeError::ChecksumMismatch`] or
/// [`KlvDecodeError::UnexpectedUniversalLabel`].
pub fn decode_unchecked(buf: &[u8]) -> Result<UasDatalinkLs, KlvDecodeError> {
    decode_inner(buf, false, false)
}

/// Strict decode: verify the running-sum 16 checksum (Tag 1) AND require
/// the buffer's 16-byte UL to fall within the ST 0601-family UL pattern.
///
/// # Errors
/// In addition to the structural variants from [`decode`]:
/// - [`KlvDecodeError::ChecksumMismatch`] when the declared Tag 1 value
///   does not match the recomputed running-sum.
/// - [`KlvDecodeError::UnexpectedUniversalLabel`] when the leading
///   16-byte UL is not in the ST 0601 family.
pub fn decode_strict(buf: &[u8]) -> Result<UasDatalinkLs, KlvDecodeError> {
    decode_inner(buf, true, true)
}

/// Strict ST 0601 compliance decode. In addition to checksum
/// verification (`decode`) and ST 0601-family UL gating (same
/// restriction as `decode_strict`), this also enforces the spec's
/// mandatory structure rules:
///
/// - ST 0601.8-09: Tag 2 (Precision Time Stamp) must be the first
///   element in the Local Set body.
/// - ST 0601.8-11: Tag 1 (Checksum) must be the last element.
/// - ST 0601.8-12: Tag 65 (UAS LS Version) must be present.
/// - ST 0601.13-24: each non-multiple ST 0601 item appears at most
///   once per packet. The strict walker rejects duplicate
///   occurrences of known typed tags via
///   [`KlvDecodeError::DuplicateTag`]. Unknown tags (outside the
///   typed table) are allowed to repeat: ST 0601.13 only mandates
///   once-per-packet for defined items.
/// - ST 0107.5 §6.3.1 / §6.3.2: BER-OID tag bytes and BER length
///   bytes must use canonical (fewest-bytes) encoding both for the
///   outer total-length and for every per-item TLV inside the body.
///   The strict walker uses [`crate::klv::length::read_ber_oid_strict`] +
///   [`crate::klv::length::read_ber_strict`] so a non-canonical encoding
///   anywhere inside the body trips
///   [`KlvDecodeError::NonCanonicalTag`] / [`KlvDecodeError::NonCanonicalLength`].
///
/// Use this only when validating compliance against published
/// captures or reference test vectors. Real-world captures from the
/// corpus often violate -09/-11/-12/-24 in benign ways; prefer `decode`
/// for production parsing.
///
/// # Errors
/// In addition to all variants from [`decode_strict`]:
/// - [`KlvDecodeError::Tag2NotFirst`] if Tag 2 is missing or not the
///   first body element.
/// - [`KlvDecodeError::Tag1NotLast`] if Tag 1 is missing or not the
///   final body element.
/// - [`KlvDecodeError::MissingTag65`] if the UAS LS Version tag is
///   absent.
/// - [`KlvDecodeError::DuplicateTag`] if a known typed tag appears
///   more than once in the body.
/// - [`KlvDecodeError::NonCanonicalLength`] or
///   [`KlvDecodeError::NonCanonicalTag`] if any BER or BER-OID
///   encoding in the outer length or the body is non-canonical.
pub fn decode_strict_compliance(buf: &[u8]) -> Result<UasDatalinkLs, KlvDecodeError> {
    // Step 1: walk the LS body strictly and record tag order WITHOUT
    // ST 0601 typed-decode. The walker uses the canonical-encoding
    // strict BER + BER-OID readers and tracks duplicate occurrences
    // of known typed tags. We need raw tag positions to enforce
    // ordering (ST 0601.8-09 / -11) and once-per-packet (ST 0601.13-24).
    if buf.len() < 16 {
        return Err(KlvDecodeError::Truncated {
            offset: 0,
            needed: 16,
            have: buf.len(),
        });
    }
    let (declared_len, after_len) = read_ber_strict(&buf[16..])?;
    if after_len.len() < declared_len {
        return Err(KlvDecodeError::Truncated {
            offset: buf.len() - after_len.len(),
            needed: declared_len,
            have: after_len.len(),
        });
    }
    let body = &after_len[..declared_len];
    // Offset of `body[0]` inside the original `buf`, so DuplicateTag/
    // truncation errors report a buf-relative offset matching the
    // permissive path.
    let body_offset_in_buf = buf.len() - after_len.len();
    let tag_order = strict_body_walk(body, body_offset_in_buf)?;
    if tag_order.first() != Some(&2) {
        return Err(KlvDecodeError::Tag2NotFirst);
    }
    if tag_order.last() != Some(&1) {
        return Err(KlvDecodeError::Tag1NotLast);
    }
    if !tag_order.contains(&65) {
        return Err(KlvDecodeError::MissingTag65);
    }
    // Step 2: delegate to existing strict decode (verifies checksum + UL
    // family). All the typed dispatch happens there. The body has
    // already cleared the strict-BER + duplicate gates above, so the
    // permissive iterator inside `decode_inner` will see the same
    // bytes without surfacing a stricter error.
    decode_inner(
        buf, /* verify_checksum */ true, /* strict_ul */ true,
    )
}

/// Walk an ST 0601 LS body using the canonical-encoding strict BER
/// readers, rejecting duplicate occurrences of known typed tags.
/// Returns the in-order list of tags encountered (used by the
/// caller for Tag 2/Tag 1/Tag 65 ordering checks).
///
/// `body_offset_in_buf` is the offset of `body[0]` within the
/// original outer buffer; surfaced through `DuplicateTag.offset`
/// and `Truncated.offset` so callers can locate the violation.
///
/// Duplicate detection is gated on `lookup(tag_u8).is_some()` —
/// ST 0601.13-24's once-per-packet rule applies only to defined
/// items. Unknown tags (including 2-byte BER-OID encoded tag IDs
/// beyond the 1-byte universe) are walked but never tracked.
fn strict_body_walk(body: &[u8], body_offset_in_buf: usize) -> Result<Vec<u32>, KlvDecodeError> {
    let mut tag_order: Vec<u32> = Vec::new();
    let mut seen = [false; 256];
    let mut offset = 0usize;
    while offset < body.len() {
        let item_start = offset;
        let rest = &body[item_start..];
        let (tag, len, consumed, after_len) =
            read_strict_tlv(rest, body_offset_in_buf + item_start)?;
        if after_len.len() < len {
            return Err(KlvDecodeError::Truncated {
                offset: body_offset_in_buf + item_start + consumed,
                needed: len,
                have: after_len.len(),
            });
        }
        // Duplicate-tag check (E1) — only meaningful for typed tags
        // that fit in u8 AND are present in the ST 0601 table. Tags 115
        // and 102 are exempted: they are the spec's only two items with
        // "Multiples Allowed" = Yes (ST 0601.19 Table 1), so repeating
        // them is conformant, not a violation of the once-per-packet
        // rule. (Tag 102 is typed via `UasDatalinkLs::sdcc_flps` —
        // see `decode_inner`'s running tag-list capture in
        // `apply_typed_tag`.)
        if let Ok(tag_u8) = u8::try_from(tag) {
            if lookup(tag_u8).is_some() && tag_u8 != 115 && tag_u8 != 102 {
                if seen[tag_u8 as usize] {
                    return Err(KlvDecodeError::DuplicateTag {
                        tag,
                        offset: body_offset_in_buf + item_start,
                    });
                }
                seen[tag_u8 as usize] = true;
            }
        }
        tag_order.push(tag);
        offset = item_start + consumed + len;
    }
    Ok(tag_order)
}

fn decode_inner(
    buf: &[u8],
    verify_checksum: bool,
    strict_ul: bool,
) -> Result<UasDatalinkLs, KlvDecodeError> {
    if buf.len() < 16 {
        return Err(KlvDecodeError::Truncated {
            offset: 0,
            needed: 16,
            have: buf.len(),
        });
    }
    let mut ul_bytes = [0u8; 16];
    ul_bytes.copy_from_slice(&buf[..16]);
    let ul = UniversalLabel::new(ul_bytes);

    if strict_ul && !ul.is_st0601_family() {
        return Err(KlvDecodeError::UnexpectedUniversalLabel {
            expected: UniversalLabel::ST_0601_LS,
            found: ul,
        });
    }

    // Outer BER length
    let (declared_len, after_len) = read_ber(&buf[16..])?;
    let body_offset = buf.len() - after_len.len();
    if after_len.len() < declared_len {
        return Err(KlvDecodeError::Truncated {
            offset: body_offset,
            needed: declared_len,
            have: after_len.len(),
        });
    }
    let body = &after_len[..declared_len];

    let mut record = UasDatalinkLs {
        universal_label: ul,
        declared_version: ul.st0601_version_byte(),
        ..UasDatalinkLs::default()
    };

    let mut declared_checksum: Option<(u16, usize)> = None; // (value, offset_into_buf_of_value)

    // Wire-order item tags seen so far (Tag 102's "Refined Source List"
    // positional capture needs to know which items immediately precede
    // each occurrence — see `apply_typed_tag`'s `102` arm). Tag 1
    // (checksum) is excluded below since it is handled separately and
    // never reaches `apply_typed_tag`; Tag 102 itself is excluded inside
    // `apply_typed_tag` (it is not one of the "sources" a later
    // occurrence could refine).
    let mut tags_seen: Vec<u32> = Vec::new();

    for r in Iter::local_set(body) {
        let f = r.map_err(|mut e| {
            crate::klv::length::rebase_offset(&mut e, body_offset);
            e
        })?;
        if f.tag == 1 {
            // Checksum: capture for later verification.
            // Byte offset of f.value within buf — used both to report a
            // buffer-absolute `Truncated.offset` below and to bound the
            // checksum coverage span. `f.value` is always a subslice of
            // `body`, itself a subslice of `buf`, so the subtraction is
            // in-range even when the value is empty.
            let value_offset_in_buf =
                (f.value.as_ptr() as usize).wrapping_sub(buf.as_ptr() as usize);
            if f.value.len() != 2 {
                return Err(KlvDecodeError::Truncated {
                    offset: value_offset_in_buf,
                    needed: 2,
                    have: f.value.len(),
                });
            }
            let cksum = u16::from_be_bytes([f.value[0], f.value[1]]);
            declared_checksum = Some((cksum, value_offset_in_buf));
            continue;
        }
        if let Err(field_err) = apply_typed_tag(&mut record, &f, &mut tags_seen) {
            record.field_errors.push(field_err);
        }
    }

    if verify_checksum {
        if let Some((expected, value_offset)) = declared_checksum {
            let computed = crate::klv::checksum::checksum_running_sum_16(&buf[..value_offset]);
            if computed != expected {
                return Err(KlvDecodeError::ChecksumMismatch {
                    expected,
                    found: computed,
                });
            }
        } else {
            // ST 0601 mandates Tag 1; treat absence as a structural error in
            // verifying modes. Permissive `decode_unchecked` skips this check.
            return Err(KlvDecodeError::Truncated {
                offset: body_offset,
                needed: 3,
                have: 0,
            });
        }
    }

    Ok(record)
}

fn apply_typed_tag(
    record: &mut UasDatalinkLs,
    f: &crate::klv::pack::RawField<'_>,
    tags_seen: &mut Vec<u32>,
) -> Result<(), KlvFieldError> {
    let tag = f.tag;
    // Running tag-list capture for Tag 102's positional "Refined Source
    // List" binding (ST 0601.19 §8.102): push every item tag as
    // encountered — known or unknown, but never Tag 102 itself (it is
    // not one of the "sources" a later occurrence could refine). Pushed
    // unconditionally up front so a field that goes on to fail decode
    // below still counts as "encountered" for a later Tag 102 occurrence.
    if tag != 102 {
        tags_seen.push(tag);
    }
    // Per MISB ST 0107.3-04 the decoder shall skip unknown LS values
    // without impacting the decoding of known items. ST 0107.5 §6.3.1
    // specifies BER-OID tags so the wire-format tag space is unlimited —
    // multi-byte tags up to u32 already arrive here via
    // `Iter::local_set`. Narrow only after rejecting tags outside the
    // typed table's u8 universe; otherwise a future tag 258 (= 0x102,
    // encoded `0x82 0x02`) would `as u8`-narrow to 2 and silently
    // collide with Tag 2 (Precision Time Stamp). The sibling ST 0102
    // decoder uses the same `u8::try_from` gate.
    let Ok(tag_u8) = u8::try_from(tag) else {
        record.unknown.push(OwnedRawField::from(f.clone()));
        return Ok(());
    };
    let Some(spec) = lookup(tag_u8) else {
        // Unknown tag — pass through.
        record.unknown.push(OwnedRawField::from(f.clone()));
        return Ok(());
    };
    match spec.encoding {
        Encoding::U8 => {
            if f.value.len() != 1 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 1,
                    got: f.value.len(),
                });
            }
            let v = f.value[0];
            match tag {
                34 => record.icing_detected = Some(IcingDetected::from_wire(v)),
                47 => record.generic_flag_data = Some(v),
                61 => record.weapon_fired = Some(v),
                63 => record.sensor_fov_name = Some(SensorFovName::from_wire(v)),
                65 => record.uas_ls_version = Some(v),
                77 => record.operational_mode = Some(OperationalMode::from_wire(v)),
                _ => unreachable!(),
            }
        }
        Encoding::I8 => {
            if f.value.len() != 1 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 1,
                    got: f.value.len(),
                });
            }
            let v = f.value[0] as i8;
            match tag {
                39 => record.outside_air_temp_c = Some(v),
                _ => unreachable!(),
            }
        }
        Encoding::U16 => {
            if f.value.len() != 2 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 2,
                    got: f.value.len(),
                });
            }
            let v = u16::from_be_bytes([f.value[0], f.value[1]]);
            match tag {
                60 => record.weapon_load = Some(v),
                62 => record.laser_prf_code = Some(v),
                _ => unreachable!(),
            }
        }
        Encoding::U64 => {
            if f.value.len() != 8 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 8,
                    got: f.value.len(),
                });
            }
            let mut a = [0u8; 8];
            a.copy_from_slice(f.value);
            let v = u64::from_be_bytes(a);
            match tag {
                2 => record.timestamp_us = Some(v),
                72 => record.event_start_time_us = Some(v),
                _ => unreachable!(),
            }
        }
        Encoding::Utf8 { max_bytes } => {
            // ST 0107.5 §6.3.3.2: length-0 value = "Unknown" (field absent);
            // single NUL byte = empty string "". Anything else is UTF-8 text.
            if f.value.is_empty() {
                return Ok(()); // absent; leave field as None
            }
            if f.value.len() > max_bytes {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: max_bytes,
                    got: f.value.len(),
                });
            }
            let s = if f.value == [0x00] {
                String::new() // empty string signal
            } else {
                core::str::from_utf8(f.value)
                    .map_err(|_| KlvFieldError::InvalidUtf8 { tag })?
                    .to_owned()
            };
            match tag {
                3 => record.mission_id = Some(s),
                4 => record.platform_tail_number = Some(s),
                10 => record.platform_designation = Some(s),
                11 => record.image_source_sensor = Some(s),
                12 => record.image_coordinate_system = Some(s),
                59 => record.platform_call_sign = Some(s),
                70 => record.alternate_platform_name = Some(s),
                106 => record.stream_designator = Some(s),
                107 => record.operational_base = Some(s),
                108 => record.broadcast_source = Some(s),
                129 => record.target_id = Some(s),
                135 => record.communications_method = Some(s),
                _ => unreachable!(),
            }
        }
        Encoding::RawBytes => match tag {
            48 => record.security_local_set = Some(f.value.to_vec()),
            73 => record.rvt = Some(f.value.to_vec()),
            74 => record.vmti = Some(f.value.to_vec()),
            94 => record.miis_core_id = Some(f.value.to_vec()),
            95 => record.sar_mi_local_set = Some(f.value.to_vec()),
            97 => record.range_image_local_set = Some(f.value.to_vec()),
            98 => record.geo_registration_local_set = Some(f.value.to_vec()),
            99 => record.composite_imaging_local_set = Some(f.value.to_vec()),
            100 => record.segment_local_set = Some(f.value.to_vec()),
            101 => record.amend_local_set = Some(f.value.to_vec()),
            139 => record.active_payloads = Some(f.value.to_vec()),
            _ => unreachable!(),
        },
        Encoding::U8Range
        | Encoding::U16Range
        | Encoding::I16Range
        | Encoding::U32Range
        | Encoding::I32Range => {
            let r = spec.range.as_ref().expect("ranged tag has range");
            match decode_fixed_range(r, tag, f.value)? {
                Some(v) => assign_ranged(record, tag, v),
                // INT_MIN sentinel: spec-defined signal, not an error.
                // Field stays None; tag is recorded for encode round-trip.
                None => record.sentinel_tags.push(tag),
            }
        }
        Encoding::Imapb {
            min, max, max_len, ..
        } => {
            // `max_len` is a per-tag business-rule cap (Table B1) narrower
            // than the substrate's fixed L<=8 ceiling that `decode_imapb`
            // itself enforces — check it here so an over-long wire value
            // for a narrow-max_len tag surfaces as InvalidLength rather
            // than silently being accepted up to 8 bytes.
            if f.value.is_empty() || f.value.len() > max_len {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: max_len,
                    got: f.value.len(),
                });
            }
            let params = crate::klv::imapb::ImapbParams {
                min,
                max,
                length: f.value.len(),
            };
            match crate::klv::imapb::decode_imapb(&params, f.value)? {
                crate::klv::imapb::DecodedImapb::Value(v) => assign_ranged(record, tag, v),
                crate::klv::imapb::DecodedImapb::Special(s) => {
                    record.imapb_specials.push((tag, s));
                }
                // Both non-conformant outcomes below are producer errors
                // from this typed consumer's view (see the
                // `imapb_specials` field rustdoc) — recorded in
                // `field_errors`, not preserved as raw bytes.
                crate::klv::imapb::DecodedImapb::ReservedSpecial { .. } => {
                    return Err(KlvFieldError::OutOfRange {
                        tag,
                        value: f64::NAN,
                        min,
                        max,
                    });
                }
                crate::klv::imapb::DecodedImapb::OutOfRange { decoded } => {
                    return Err(KlvFieldError::OutOfRange {
                        tag,
                        value: decoded,
                        min,
                        max,
                    });
                }
            }
        }
        Encoding::VarUint { max_len } => {
            let raw = read_var_uint(f.value, max_len, tag)?;
            // The Err arm below is unreachable by construction for every
            // tag in this match: `max_len` is always set (in tags.rs) to
            // the target type's byte width (4 for the u32 tags, 1 for the
            // u8 tags), and `read_var_uint` already rejected any wire
            // value longer than `max_len` — so `raw` is guaranteed to fit.
            // Kept as a defensive `try_from` rather than an `as` cast in
            // case a future tag pairs a narrower Rust type with a wider
            // `max_len`.
            let narrow_u32 = |v: u64| {
                u32::try_from(v).map_err(|_| KlvFieldError::InvalidLength {
                    tag,
                    expected: max_len,
                    got: f.value.len(),
                })
            };
            let narrow_u8 = |v: u64| {
                u8::try_from(v).map_err(|_| KlvFieldError::InvalidLength {
                    tag,
                    expected: max_len,
                    got: f.value.len(),
                })
            };
            match tag {
                110 => record.time_airborne_s = Some(narrow_u32(raw)?),
                111 => record.propulsion_unit_speed_rpm = Some(narrow_u32(raw)?),
                123 => record.navsats_in_view = Some(narrow_u8(raw)?),
                124 => record.positioning_method_source = Some(narrow_u8(raw)?),
                125 => record.platform_status = Some(PlatformStatus::from_wire(narrow_u8(raw)?)),
                126 => {
                    record.sensor_control_mode = Some(SensorControlMode::from_wire(narrow_u8(raw)?))
                }
                131 => record.take_off_time_us = Some(raw),
                133 => record.mi_storage_capacity_gb = Some(narrow_u32(raw)?),
                _ => unreachable!(),
            }
        }
        Encoding::VarInt { max_len } => {
            let raw = read_var_int(f.value, max_len, tag)?;
            match tag {
                136 => {
                    record.leap_seconds =
                        Some(
                            i32::try_from(raw).map_err(|_| KlvFieldError::InvalidLength {
                                tag,
                                expected: max_len,
                                got: f.value.len(),
                            })?,
                        )
                }
                137 => record.correction_offset_us = Some(raw),
                _ => unreachable!(),
            }
        }
        Encoding::Pack => match tag {
            81 => record.image_horizon = Some(packs::parse_image_horizon(f.value)?),
            // MULTI-INSTANCE (ST 0601.19 Table 1 "Multiples Allowed" =
            // Yes) — every occurrence appends, it never overwrites.
            // Positional capture, not a full `st1010::decode_sdcc_flp`
            // parse: only the Matrix Size (Element 1) needs peeking here
            // to know how many of `tags_seen`'s most-recent entries this
            // occurrence refines; the raw bytes are kept verbatim so a
            // malformed-but-peekable pack still round-trips, and callers
            // decode the pack itself on demand.
            102 => {
                let n = crate::klv::st1010::peek_matrix_size(f.value)
                    .ok_or(KlvFieldError::TruncatedField { tag })?;
                let start = tags_seen.len().saturating_sub(n);
                record.sdcc_flps.push(SdccFlpField {
                    preceding_tags: tags_seen[start..].to_vec(),
                    bytes: f.value.to_vec(),
                });
            }
            115 => record
                .control_commands
                .push(packs::parse_control_command(f.value)?),
            116 => record.control_command_verification = Some(packs::parse_id_list(f.value, 116)?),
            121 => record.active_wavelengths = Some(packs::parse_id_list(f.value, 121)?),
            122 => record.country_codes = Some(packs::parse_country_codes(f.value)?),
            127 => record.sensor_frame_rate = Some(packs::parse_sensor_frame_rate(f.value)?),
            128 => record.wavelengths_list = Some(packs::parse_wavelengths_list(f.value)?),
            130 => record.airbase_locations = Some(packs::parse_airbase_locations(f.value)?),
            138 => record.payload_list = Some(packs::parse_payload_list(f.value)?),
            140 => record.weapons_stores = Some(packs::parse_weapons_stores(f.value)?),
            141 => record.waypoint_list = Some(packs::parse_waypoints(f.value)?),
            142 => record.view_domain = Some(packs::parse_view_domain(f.value)?),
            143 => {
                record.metadata_substream_id = Some(packs::parse_metadata_substream_id(f.value)?)
            }
            _ => unreachable!(),
        },
    }
    Ok(())
}

/// Per-tag accessor for the 83 uniform `Option<f64>` ranged fields — the
/// 69 fixed-width LinearRange fields plus the 14 ST 1201.5 IMAPB
/// extended-range items.
///
/// Drives `assign_ranged` (decode) and the ranged arms of
/// `encode_tag_value` / `each_typed_field` (encode) from one place,
/// eliminating parallel per-field dispatch tables. `fn` pointers are
/// `const`-safe and work in `no_std` context; field names remain greppable.
pub(super) struct RangedEntry {
    pub(super) id: u8,
    pub(super) get: fn(&UasDatalinkLs) -> Option<f64>,
    pub(super) set: fn(&mut UasDatalinkLs, f64),
}

/// All 83 ST 0601 ranged `Option<f64>` fields in tag-ascending order —
/// 69 fixed-width LinearRange fields (tags 5-93) plus 14 ST 1201.5 IMAPB
/// extended-range fields (tags 96-134). The encode path
/// derives value_len from `tags::TAGS[id].range.byte_length` for the
/// LinearRange rows, or `Encoding::Imapb.default_len` for the IMAPB rows;
/// the decode path calls `set`; the encode path calls `get`.
pub(super) static RANGED_FIELDS: &[RangedEntry] = &[
    RangedEntry {
        id: 5,
        get: |r| r.platform_heading_deg,
        set: |r, v| r.platform_heading_deg = Some(v),
    },
    RangedEntry {
        id: 6,
        get: |r| r.platform_pitch_deg,
        set: |r, v| r.platform_pitch_deg = Some(v),
    },
    RangedEntry {
        id: 7,
        get: |r| r.platform_roll_deg,
        set: |r, v| r.platform_roll_deg = Some(v),
    },
    RangedEntry {
        id: 8,
        get: |r| r.platform_true_airspeed,
        set: |r, v| r.platform_true_airspeed = Some(v),
    },
    RangedEntry {
        id: 9,
        get: |r| r.platform_indicated_airspeed,
        set: |r, v| r.platform_indicated_airspeed = Some(v),
    },
    RangedEntry {
        id: 13,
        get: |r| r.sensor_lat_deg,
        set: |r, v| r.sensor_lat_deg = Some(v),
    },
    RangedEntry {
        id: 14,
        get: |r| r.sensor_lon_deg,
        set: |r, v| r.sensor_lon_deg = Some(v),
    },
    RangedEntry {
        id: 15,
        get: |r| r.sensor_alt_m,
        set: |r, v| r.sensor_alt_m = Some(v),
    },
    RangedEntry {
        id: 16,
        get: |r| r.sensor_hfov_deg,
        set: |r, v| r.sensor_hfov_deg = Some(v),
    },
    RangedEntry {
        id: 17,
        get: |r| r.sensor_vfov_deg,
        set: |r, v| r.sensor_vfov_deg = Some(v),
    },
    RangedEntry {
        id: 18,
        get: |r| r.sensor_rel_az_deg,
        set: |r, v| r.sensor_rel_az_deg = Some(v),
    },
    RangedEntry {
        id: 19,
        get: |r| r.sensor_rel_el_deg,
        set: |r, v| r.sensor_rel_el_deg = Some(v),
    },
    RangedEntry {
        id: 20,
        get: |r| r.sensor_rel_roll_deg,
        set: |r, v| r.sensor_rel_roll_deg = Some(v),
    },
    RangedEntry {
        id: 21,
        get: |r| r.slant_range_m,
        set: |r, v| r.slant_range_m = Some(v),
    },
    RangedEntry {
        id: 22,
        get: |r| r.target_width_m,
        set: |r, v| r.target_width_m = Some(v),
    },
    RangedEntry {
        id: 23,
        get: |r| r.frame_center_lat_deg,
        set: |r, v| r.frame_center_lat_deg = Some(v),
    },
    RangedEntry {
        id: 24,
        get: |r| r.frame_center_lon_deg,
        set: |r, v| r.frame_center_lon_deg = Some(v),
    },
    RangedEntry {
        id: 25,
        get: |r| r.frame_center_elev_m,
        set: |r, v| r.frame_center_elev_m = Some(v),
    },
    RangedEntry {
        id: 26,
        get: |r| r.corner_lat_offset_p1_deg,
        set: |r, v| r.corner_lat_offset_p1_deg = Some(v),
    },
    RangedEntry {
        id: 27,
        get: |r| r.corner_lon_offset_p1_deg,
        set: |r, v| r.corner_lon_offset_p1_deg = Some(v),
    },
    RangedEntry {
        id: 28,
        get: |r| r.corner_lat_offset_p2_deg,
        set: |r, v| r.corner_lat_offset_p2_deg = Some(v),
    },
    RangedEntry {
        id: 29,
        get: |r| r.corner_lon_offset_p2_deg,
        set: |r, v| r.corner_lon_offset_p2_deg = Some(v),
    },
    RangedEntry {
        id: 30,
        get: |r| r.corner_lat_offset_p3_deg,
        set: |r, v| r.corner_lat_offset_p3_deg = Some(v),
    },
    RangedEntry {
        id: 31,
        get: |r| r.corner_lon_offset_p3_deg,
        set: |r, v| r.corner_lon_offset_p3_deg = Some(v),
    },
    RangedEntry {
        id: 32,
        get: |r| r.corner_lat_offset_p4_deg,
        set: |r, v| r.corner_lat_offset_p4_deg = Some(v),
    },
    RangedEntry {
        id: 33,
        get: |r| r.corner_lon_offset_p4_deg,
        set: |r, v| r.corner_lon_offset_p4_deg = Some(v),
    },
    RangedEntry {
        id: 35,
        get: |r| r.wind_direction_deg,
        set: |r, v| r.wind_direction_deg = Some(v),
    },
    RangedEntry {
        id: 36,
        get: |r| r.wind_speed,
        set: |r, v| r.wind_speed = Some(v),
    },
    RangedEntry {
        id: 37,
        get: |r| r.static_pressure_mbar,
        set: |r, v| r.static_pressure_mbar = Some(v),
    },
    RangedEntry {
        id: 38,
        get: |r| r.density_altitude_m,
        set: |r, v| r.density_altitude_m = Some(v),
    },
    RangedEntry {
        id: 40,
        get: |r| r.target_location_lat_deg,
        set: |r, v| r.target_location_lat_deg = Some(v),
    },
    RangedEntry {
        id: 41,
        get: |r| r.target_location_lon_deg,
        set: |r, v| r.target_location_lon_deg = Some(v),
    },
    RangedEntry {
        id: 42,
        get: |r| r.target_location_elev_m,
        set: |r, v| r.target_location_elev_m = Some(v),
    },
    RangedEntry {
        id: 43,
        get: |r| r.target_track_gate_width_px,
        set: |r, v| r.target_track_gate_width_px = Some(v),
    },
    RangedEntry {
        id: 44,
        get: |r| r.target_track_gate_height_px,
        set: |r, v| r.target_track_gate_height_px = Some(v),
    },
    RangedEntry {
        id: 45,
        get: |r| r.target_error_ce90_m,
        set: |r, v| r.target_error_ce90_m = Some(v),
    },
    RangedEntry {
        id: 46,
        get: |r| r.target_error_le90_m,
        set: |r, v| r.target_error_le90_m = Some(v),
    },
    RangedEntry {
        id: 49,
        get: |r| r.differential_pressure_mbar,
        set: |r, v| r.differential_pressure_mbar = Some(v),
    },
    RangedEntry {
        id: 50,
        get: |r| r.platform_angle_of_attack_deg,
        set: |r, v| r.platform_angle_of_attack_deg = Some(v),
    },
    RangedEntry {
        id: 51,
        get: |r| r.platform_vertical_speed,
        set: |r, v| r.platform_vertical_speed = Some(v),
    },
    RangedEntry {
        id: 52,
        get: |r| r.platform_sideslip_deg,
        set: |r, v| r.platform_sideslip_deg = Some(v),
    },
    RangedEntry {
        id: 53,
        get: |r| r.airfield_barometric_pressure_mbar,
        set: |r, v| r.airfield_barometric_pressure_mbar = Some(v),
    },
    RangedEntry {
        id: 54,
        get: |r| r.airfield_elevation_m,
        set: |r, v| r.airfield_elevation_m = Some(v),
    },
    RangedEntry {
        id: 55,
        get: |r| r.relative_humidity_pct,
        set: |r, v| r.relative_humidity_pct = Some(v),
    },
    RangedEntry {
        id: 56,
        get: |r| r.platform_ground_speed,
        set: |r, v| r.platform_ground_speed = Some(v),
    },
    RangedEntry {
        id: 57,
        get: |r| r.ground_range_m,
        set: |r, v| r.ground_range_m = Some(v),
    },
    RangedEntry {
        id: 58,
        get: |r| r.platform_fuel_remaining_kg,
        set: |r, v| r.platform_fuel_remaining_kg = Some(v),
    },
    RangedEntry {
        id: 64,
        get: |r| r.platform_magnetic_heading_deg,
        set: |r, v| r.platform_magnetic_heading_deg = Some(v),
    },
    RangedEntry {
        id: 67,
        get: |r| r.alternate_platform_lat_deg,
        set: |r, v| r.alternate_platform_lat_deg = Some(v),
    },
    RangedEntry {
        id: 68,
        get: |r| r.alternate_platform_lon_deg,
        set: |r, v| r.alternate_platform_lon_deg = Some(v),
    },
    RangedEntry {
        id: 69,
        get: |r| r.alternate_platform_alt_m,
        set: |r, v| r.alternate_platform_alt_m = Some(v),
    },
    RangedEntry {
        id: 71,
        get: |r| r.alternate_platform_heading_deg,
        set: |r, v| r.alternate_platform_heading_deg = Some(v),
    },
    RangedEntry {
        id: 75,
        get: |r| r.sensor_ellipsoid_height_m,
        set: |r, v| r.sensor_ellipsoid_height_m = Some(v),
    },
    RangedEntry {
        id: 76,
        get: |r| r.alternate_platform_ellipsoid_height_m,
        set: |r, v| r.alternate_platform_ellipsoid_height_m = Some(v),
    },
    RangedEntry {
        id: 78,
        get: |r| r.frame_center_ellipsoid_height_m,
        set: |r, v| r.frame_center_ellipsoid_height_m = Some(v),
    },
    RangedEntry {
        id: 79,
        get: |r| r.sensor_north_velocity,
        set: |r, v| r.sensor_north_velocity = Some(v),
    },
    RangedEntry {
        id: 80,
        get: |r| r.sensor_east_velocity,
        set: |r, v| r.sensor_east_velocity = Some(v),
    },
    RangedEntry {
        id: 82,
        get: |r| r.corner_lat_p1_deg,
        set: |r, v| r.corner_lat_p1_deg = Some(v),
    },
    RangedEntry {
        id: 83,
        get: |r| r.corner_lon_p1_deg,
        set: |r, v| r.corner_lon_p1_deg = Some(v),
    },
    RangedEntry {
        id: 84,
        get: |r| r.corner_lat_p2_deg,
        set: |r, v| r.corner_lat_p2_deg = Some(v),
    },
    RangedEntry {
        id: 85,
        get: |r| r.corner_lon_p2_deg,
        set: |r, v| r.corner_lon_p2_deg = Some(v),
    },
    RangedEntry {
        id: 86,
        get: |r| r.corner_lat_p3_deg,
        set: |r, v| r.corner_lat_p3_deg = Some(v),
    },
    RangedEntry {
        id: 87,
        get: |r| r.corner_lon_p3_deg,
        set: |r, v| r.corner_lon_p3_deg = Some(v),
    },
    RangedEntry {
        id: 88,
        get: |r| r.corner_lat_p4_deg,
        set: |r, v| r.corner_lat_p4_deg = Some(v),
    },
    RangedEntry {
        id: 89,
        get: |r| r.corner_lon_p4_deg,
        set: |r, v| r.corner_lon_p4_deg = Some(v),
    },
    RangedEntry {
        id: 90,
        get: |r| r.platform_pitch_full_deg,
        set: |r, v| r.platform_pitch_full_deg = Some(v),
    },
    RangedEntry {
        id: 91,
        get: |r| r.platform_roll_full_deg,
        set: |r, v| r.platform_roll_full_deg = Some(v),
    },
    RangedEntry {
        id: 92,
        get: |r| r.platform_angle_of_attack_full_deg,
        set: |r, v| r.platform_angle_of_attack_full_deg = Some(v),
    },
    RangedEntry {
        id: 93,
        get: |r| r.platform_sideslip_full_deg,
        set: |r, v| r.platform_sideslip_full_deg = Some(v),
    },
    // WP-B Table B1: ST 1201.5 IMAPB extended-range items. `spec.range`
    // is `None` for these (encoding dispatch happens on `Encoding::Imapb`
    // instead), but they share this same `Option<f64>` accessor table.
    RangedEntry {
        id: 96,
        get: |r| r.target_width_extended_m,
        set: |r, v| r.target_width_extended_m = Some(v),
    },
    RangedEntry {
        id: 103,
        get: |r| r.density_altitude_extended_m,
        set: |r, v| r.density_altitude_extended_m = Some(v),
    },
    RangedEntry {
        id: 104,
        get: |r| r.sensor_ellipsoid_height_extended_m,
        set: |r, v| r.sensor_ellipsoid_height_extended_m = Some(v),
    },
    RangedEntry {
        id: 105,
        get: |r| r.alternate_platform_ellipsoid_height_extended_m,
        set: |r, v| r.alternate_platform_ellipsoid_height_extended_m = Some(v),
    },
    RangedEntry {
        id: 109,
        get: |r| r.range_to_recovery_km,
        set: |r, v| r.range_to_recovery_km = Some(v),
    },
    RangedEntry {
        id: 112,
        get: |r| r.platform_course_angle_deg,
        set: |r, v| r.platform_course_angle_deg = Some(v),
    },
    RangedEntry {
        id: 113,
        get: |r| r.altitude_agl_m,
        set: |r, v| r.altitude_agl_m = Some(v),
    },
    RangedEntry {
        id: 114,
        get: |r| r.radar_altimeter_m,
        set: |r, v| r.radar_altimeter_m = Some(v),
    },
    RangedEntry {
        id: 117,
        get: |r| r.sensor_azimuth_rate_dps,
        set: |r, v| r.sensor_azimuth_rate_dps = Some(v),
    },
    RangedEntry {
        id: 118,
        get: |r| r.sensor_elevation_rate_dps,
        set: |r, v| r.sensor_elevation_rate_dps = Some(v),
    },
    RangedEntry {
        id: 119,
        get: |r| r.sensor_roll_rate_dps,
        set: |r, v| r.sensor_roll_rate_dps = Some(v),
    },
    RangedEntry {
        id: 120,
        get: |r| r.mi_storage_percent_full,
        set: |r, v| r.mi_storage_percent_full = Some(v),
    },
    RangedEntry {
        id: 132,
        get: |r| r.transmission_frequency_mhz,
        set: |r, v| r.transmission_frequency_mhz = Some(v),
    },
    RangedEntry {
        id: 134,
        get: |r| r.zoom_percentage,
        set: |r, v| r.zoom_percentage = Some(v),
    },
];

/// Look up a ranged-field entry by tag. `RANGED_FIELDS` is tag-ascending
/// (pinned by `ranged_fields_table_complete_and_injective`), so this is a
/// binary search rather than a per-call linear scan.
pub(super) fn ranged_entry(tag: u8) -> Option<&'static RangedEntry> {
    RANGED_FIELDS
        .binary_search_by_key(&tag, |e| e.id)
        .ok()
        .map(|i| &RANGED_FIELDS[i])
}

/// Write a decoded ranged-float value to the matching field in `record`.
/// Replaces a 69-arm match — the table is the single source of the tag→field mapping.
pub(super) fn assign_ranged(record: &mut UasDatalinkLs, tag: u32, v: f64) {
    if let Some(entry) = u8::try_from(tag).ok().and_then(ranged_entry) {
        (entry.set)(record, v);
    }
}
