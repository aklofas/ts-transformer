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
use super::model::{SdccFlpField, UasDatalinkLs};
use super::packs;
use super::tags::{Encoding, FieldValue, TagSpec, lookup};

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
    // The outer length is read from `buf[16..]`, so its errors come back
    // slice-relative — rebase by the UL length to keep every offset this
    // decoder reports indexed against the caller's own buffer.
    let (declared_len, after_len) = read_ber_strict(&buf[16..]).map_err(|mut e| {
        crate::klv::length::rebase_offset(&mut e, 16);
        e
    })?;
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
        // Duplicate-tag check — only meaningful for typed tags
        // that fit in u8 AND are present in the ST 0601 table. Tags 115
        // and 102 are exempted: they are the spec's only two items with
        // "Multiples Allowed" = Yes (ST 0601.19 Table 1), so repeating
        // them is conformant, not a violation of the once-per-packet
        // rule. (Tag 102 is typed via `UasDatalinkLs::sdcc_flps` —
        // see `decode_inner`'s running tag-list capture in
        // `apply_tag`.)
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

    // Outer BER length. Read from `buf[16..]`, so its errors come back
    // slice-relative — rebase by the UL length to keep every offset this
    // decoder reports indexed against the caller's own buffer.
    let (declared_len, after_len) = read_ber(&buf[16..]).map_err(|mut e| {
        crate::klv::length::rebase_offset(&mut e, 16);
        e
    })?;
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
    // each occurrence — see `apply_multi_instance`). Tag 1
    // (checksum) is excluded below since it is handled separately and
    // never reaches `apply_tag`; Tag 102 itself is excluded inside
    // `apply_tag` (it is not one of the "sources" a later
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
        if let Err(field_err) = apply_tag(&mut record, &f, &mut tags_seen) {
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

/// Decode one raw field into `record` through its `TAGS` row. The pack
/// rows and the two multi-instance tags (102, 115) keep their own
/// handlers; every other row decodes by its encoding and stores through
/// the row's `set` accessor.
fn apply_tag(
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
    match (spec.encoding, tag) {
        (Encoding::Pack, 102 | 115) => return apply_multi_instance(record, f, tags_seen),
        (Encoding::Pack, _) => return apply_pack(record, f),
        _ => {}
    }
    let Some(value) = decode_scalar(spec, f, record)? else {
        return Ok(());
    };
    let set = spec.set.ok_or(KlvFieldError::TruncatedField { tag })?;
    set(record, value).map_err(|e| match (spec.encoding, e) {
        // A varint wider than the field's Rust type: report the row's
        // declared byte cap against the wire length.
        (
            Encoding::VarUint { max_len } | Encoding::VarInt { max_len },
            KlvFieldError::InvalidLength { tag, .. },
        ) => KlvFieldError::InvalidLength {
            tag,
            expected: max_len,
            got: f.value.len(),
        },
        (_, e) => e,
    })
}

/// Decode a scalar row's wire value by its encoding. `None` means the
/// field stays unset: an absent string, an INT_MIN sentinel (recorded in
/// `sentinel_tags`) or an IMAPB special value (recorded in
/// `imapb_specials`).
fn decode_scalar(
    spec: &TagSpec,
    f: &crate::klv::pack::RawField<'_>,
    record: &mut UasDatalinkLs,
) -> Result<Option<FieldValue>, KlvFieldError> {
    let tag = f.tag;
    let value = match spec.encoding {
        Encoding::U8 => {
            if f.value.len() != 1 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 1,
                    got: f.value.len(),
                });
            }
            // The coded enums (34, 63, 77) travel as their wire code.
            if matches!(tag, 34 | 63 | 77) {
                FieldValue::Code(f.value[0])
            } else {
                FieldValue::U64(u64::from(f.value[0]))
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
            FieldValue::I64(i64::from(f.value[0] as i8))
        }
        Encoding::U16 => {
            if f.value.len() != 2 {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: 2,
                    got: f.value.len(),
                });
            }
            FieldValue::U64(u64::from(u16::from_be_bytes([f.value[0], f.value[1]])))
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
            FieldValue::U64(u64::from_be_bytes(a))
        }
        Encoding::Utf8 { max_bytes } => {
            // ST 0107.5 §6.3.3.2: length-0 value = "Unknown" (field absent);
            // single NUL byte = empty string "". Anything else is UTF-8 text.
            if f.value.is_empty() {
                return Ok(None); // absent; leave field as None
            }
            if f.value.len() > max_bytes {
                return Err(KlvFieldError::InvalidLength {
                    tag,
                    expected: max_bytes,
                    got: f.value.len(),
                });
            }
            FieldValue::Str(if f.value == [0x00] {
                String::new() // empty string signal
            } else {
                core::str::from_utf8(f.value)
                    .map_err(|_| KlvFieldError::InvalidUtf8 { tag })?
                    .to_owned()
            })
        }
        Encoding::RawBytes => FieldValue::Bytes(f.value.to_vec()),
        Encoding::U8Range
        | Encoding::U16Range
        | Encoding::I16Range
        | Encoding::U32Range
        | Encoding::I32Range => {
            let r = spec.range.as_ref().expect("ranged tag has range");
            match decode_fixed_range(r, tag, f.value)? {
                Some(v) => FieldValue::F64(v),
                // INT_MIN sentinel: spec-defined signal, not an error.
                // Field stays None; tag is recorded for encode round-trip.
                None => {
                    record.sentinel_tags.push(tag);
                    return Ok(None);
                }
            }
        }
        Encoding::Imapb {
            min, max, max_len, ..
        } => {
            // `max_len` is a per-tag business-rule cap narrower
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
                crate::klv::imapb::DecodedImapb::Value(v) => FieldValue::F64(v),
                crate::klv::imapb::DecodedImapb::Special(s) => {
                    record.imapb_specials.push((tag, s));
                    return Ok(None);
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
            // The coded enums (125, 126) travel as their wire code. A
            // value wider than the field's Rust type is refused by the
            // row's `set`; that is unreachable by construction, since
            // `max_len` is the target type's byte width and
            // `read_var_uint` already rejected any longer wire value.
            if matches!(tag, 125 | 126) {
                FieldValue::Code(u8::try_from(raw).map_err(|_| KlvFieldError::InvalidLength {
                    tag,
                    expected: max_len,
                    got: f.value.len(),
                })?)
            } else {
                FieldValue::U64(raw)
            }
        }
        Encoding::VarInt { max_len } => FieldValue::I64(read_var_int(f.value, max_len, tag)?),
        Encoding::Pack => return Ok(None),
    };
    Ok(Some(value))
}

/// Tags 102 and 115 — MULTI-INSTANCE (ST 0601.19 Table 1 "Multiples
/// Allowed" = Yes): every occurrence appends, it never overwrites.
fn apply_multi_instance(
    record: &mut UasDatalinkLs,
    f: &crate::klv::pack::RawField<'_>,
    tags_seen: &[u32],
) -> Result<(), KlvFieldError> {
    let tag = f.tag;
    match tag {
        // Positional capture, not a full `st1010::decode_sdcc_flp`
        // parse: `peek_matrix_size` reads the Matrix Size (Element 1)
        // and the Parse Control (Element 2) and applies
        // `check_matrix_size_fits`, so `N` — how many of `tags_seen`'s
        // most-recent entries this occurrence refines — is bounded by
        // what the pack's own bytes can hold. An occurrence whose
        // matrix cannot fit is rejected as `TruncatedField` and not
        // captured. Elements 3–5 are not
        // validated: the raw bytes of an accepted pack are kept
        // verbatim, and callers decode the pack itself on demand.
        102 => {
            let n = crate::klv::st1010::peek_matrix_size(f.value)
                .ok_or(KlvFieldError::TruncatedField { tag })?;
            let start = tags_seen.len().saturating_sub(n);
            record.sdcc_flps.push(SdccFlpField {
                preceding_tags: tags_seen[start..].to_vec(),
                bytes: f.value.to_vec(),
            });
        }
        _ => record
            .control_commands
            .push(packs::parse_control_command(f.value)?),
    }
    Ok(())
}

/// The single-instance pack rows: each parses with its own function in
/// `packs.rs` and stores into its own field.
fn apply_pack(
    record: &mut UasDatalinkLs,
    f: &crate::klv::pack::RawField<'_>,
) -> Result<(), KlvFieldError> {
    match f.tag {
        81 => record.image_horizon = Some(packs::parse_image_horizon(f.value)?),
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
        143 => record.metadata_substream_id = Some(packs::parse_metadata_substream_id(f.value)?),
        _ => unreachable!(),
    }
    Ok(())
}
