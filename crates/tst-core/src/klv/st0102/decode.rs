//! ST 0102 decode entry points (`decode`, `decode_strict`) and the
//! shared private `decode_inner` driver.

use crate::error::{KlvDecodeError, KlvFieldError};
use crate::klv::pack::OwnedRawField;
use crate::klv::st0102::enums::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use crate::klv::st0102::model::{SecurityLs, decode_utf16_bom};
use crate::klv::st0102::tags::{Encoding, REQUIRED_TAGS, lookup};
use alloc::string::{String, ToString};

/// Decode a Security Metadata Local Set per MISB ST 0102.12.
///
/// Lenient: missing tags are tolerated (the typed surface is
/// `Option<T>`-shaped throughout), unknown tags are preserved verbatim
/// in [`SecurityLs::unknown`], unknown enum codepoints surface as
/// `Unknown(u8)`, a zero-length item is "unknown" (its field unset, ST
/// 0107.4-17), and Tag 13 UTF-16 decode failures, text that is not UTF-8
/// and a wrong-length enum or version are demoted to
/// [`SecurityLs::field_errors`] rather than failing the whole record.
/// Use [`decode_strict`] for spec-conformance validation.
///
/// # Example — sibling-layer decode from a parent ST 0601 record
///
/// The Security LS is typically carried as the value of ST 0601 Tag 48
/// inside a UAS Datalink LS. The parent typed parser leaves Tag 48 as
/// pass-through bytes ([`crate::klv::st0601::UasDatalinkLs::security_local_set`]);
/// consumers who want typed access call this function on those bytes.
///
/// ```
/// use tst_core::klv::{st0102, st0601};
/// use tst_core::klv::st0102::{
///     ClassifyingCountryCodingMethod, ObjectCountryCodingMethod,
///     SecurityClassification, SecurityLs,
/// };
/// use tst_core::UasDatalinkLs;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // 1. Build a typed Security LS and serialize to bytes.
/// let security = SecurityLs {
///     security_classification: Some(SecurityClassification::Unclassified),
///     classifying_country_coding_method:
///         Some(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
///     classifying_country: Some("//USA".to_string()),
///     object_country_coding_method:
///         Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
///     object_country_codes: Some("USA".to_string()),
///     version: Some(12),
///     ..Default::default()
/// };
/// let security_bytes = st0102::encode_to_vec(&security)?;
///
/// // 2. Embed those bytes in a parent ST 0601 record's Tag 48.
/// let mut parent = UasDatalinkLs::default();
/// parent.timestamp_us = Some(1_700_000_000_000_000);
/// parent.security_local_set = Some(security_bytes);
/// let parent_bytes = st0601::encode_to_vec(&parent)?;
///
/// // 3. On the receive side, decode the parent, then sibling-decode
/// //    the inner Security LS.
/// let decoded_parent = st0601::decode(&parent_bytes)?;
/// let inner = decoded_parent
///     .security_local_set
///     .as_deref()
///     .expect("Tag 48 round-trips through ST 0601");
/// let decoded_security = st0102::decode(inner)?;
///
/// assert_eq!(
///     decoded_security.security_classification,
///     Some(SecurityClassification::Unclassified),
/// );
/// assert_eq!(decoded_security.version, Some(12));
/// assert!(decoded_security.field_errors.is_empty());
/// # Ok(())
/// # }
/// ```
pub fn decode(buf: &[u8]) -> Result<SecurityLs, KlvDecodeError> {
    decode_inner(buf, /* strict = */ false)
}

/// Strict decode — rejects spec-violating input (missing required
/// tags, unknown enum codepoints, [`OmittedValueXX`] reserved
/// codepoints, malformed UTF-16, text that is not ISO/IEC 646 (ASCII),
/// a date not shaped `YYYYMMDD` (Tag 10) or `YYYY-MM-DD` (Tags 23 and
/// 24; the shape only, so `99999999` passes), duplicate defined tags,
/// wrong fixed-length values, and non-canonical BER length encodings per
/// ST 0107.5 §6.3). Unknown tags are still preserved in `unknown` per ST
/// 0107.5 §6 future-proof skip rule, a repeated one included (ST
/// 0107.3-04: unknown items are skipped; matches
/// `klv::st0601::decode_strict_compliance` posture).
///
/// [`OmittedValueXX`]: ClassifyingCountryCodingMethod::OmittedValue08
pub fn decode_strict(buf: &[u8]) -> Result<SecurityLs, KlvDecodeError> {
    decode_inner(buf, /* strict = */ true)
}

/// ST 0107.5 §6.3: reject non-canonical BER length encodings across the
/// flat ST 0102 local set, preserving buffer-relative offsets. The set's
/// tags are one byte (SMPTE ST 336 Table 5, label byte 6 = `0x03`), so
/// only the length has a canonical form. The typed decode that follows
/// reuses the permissive `Iter` — by this point the bytes have already
/// cleared the strict-BER gate.
fn strict_ber_walk(buf: &[u8]) -> Result<(), KlvDecodeError> {
    use crate::klv::length::{read_ber_strict, rebase_offset};
    let mut rest = buf;
    let mut offset = 0usize;
    // ST 0107.5 §6.3.4: each item at most once per set. Checked here, where
    // the item's buffer-relative start is known, so `offset` points at the
    // repeated tag byte. Only for defined tags: an unknown item is skipped
    // (ST 0107.3-04), so its repetition is no defect of the set.
    let mut seen: hashbrown::HashSet<u32> = hashbrown::HashSet::new();
    while !rest.is_empty() {
        let item_start = offset;
        let tag = u32::from(rest[0]);
        let (len, after_len) = read_ber_strict(&rest[1..]).map_err(|mut e| {
            rebase_offset(&mut e, item_start + 1);
            e
        })?;
        let consumed = rest.len() - after_len.len();
        if lookup(rest[0]).is_some() && !seen.insert(tag) {
            return Err(KlvDecodeError::DuplicateTag {
                tag,
                offset: item_start,
            });
        }
        if len > after_len.len() {
            // Truncated value: stop the strict pre-walk and let the permissive
            // typed decode below surface it — `Iter::local_set` returns
            // `KlvDecodeError::Truncated` for a value that overruns the buffer.
            break;
        }
        rest = &after_len[len..];
        offset = item_start + consumed + len;
    }
    Ok(())
}

fn decode_inner(buf: &[u8], strict: bool) -> Result<SecurityLs, KlvDecodeError> {
    use crate::klv::pack::Iter;

    if strict {
        strict_ber_walk(buf)?;
    }

    let mut record = SecurityLs::default();

    for r in Iter::local_set_one_byte(buf) {
        let f = r?;

        // A one-byte tag: the iterator never yields a tag above 255.
        let tag_u8 = f.tag as u8;

        let spec = match lookup(tag_u8) {
            Some(s) => s,
            None => {
                // Unknown tag — pass through per ST 0107.5 §6.
                record.unknown.push(OwnedRawField {
                    tag: f.tag,
                    value: f.value.to_vec(),
                });
                continue;
            }
        };

        // ST 0107.4-17 / ST 0107.5 §6.3.3.2: a zero-length value is
        // "unknown". It is an occurrence of its tag: a value before it is
        // overwritten as by any later duplicate.
        if f.value.is_empty() {
            unset(&mut record, tag_u8);
            continue;
        }

        match spec.encoding {
            Encoding::U8Enum => {
                if f.value.len() != 1 {
                    let e = KlvFieldError::InvalidLength {
                        tag: f.tag,
                        expected: 1,
                        got: f.value.len(),
                    };
                    if strict {
                        return Err(KlvDecodeError::FieldError(e));
                    }
                    record.field_errors.push(e);
                    continue;
                }
                let b = f.value[0];
                match tag_u8 {
                    1 => {
                        let v = SecurityClassification::from_wire(b);
                        if strict && !v.is_known_codepoint() {
                            return Err(KlvDecodeError::FieldError(
                                KlvFieldError::InvalidCodepoint {
                                    tag: f.tag,
                                    value: b,
                                },
                            ));
                        }
                        record.security_classification = Some(v);
                    }
                    2 => {
                        let v = ClassifyingCountryCodingMethod::from_wire(b);
                        if strict && !v.is_known_codepoint() {
                            return Err(KlvDecodeError::FieldError(
                                KlvFieldError::InvalidCodepoint {
                                    tag: f.tag,
                                    value: b,
                                },
                            ));
                        }
                        record.classifying_country_coding_method = Some(v);
                    }
                    12 => {
                        let v = ObjectCountryCodingMethod::from_wire(b);
                        if strict && !v.is_known_codepoint() {
                            return Err(KlvDecodeError::FieldError(
                                KlvFieldError::InvalidCodepoint {
                                    tag: f.tag,
                                    value: b,
                                },
                            ));
                        }
                        record.object_country_coding_method = Some(v);
                    }
                    _ => unreachable!("tags.rs U8Enum reserved for tags 1, 2, 12 only"),
                }
            }
            Encoding::U16Be => {
                if f.value.len() != 2 {
                    let e = KlvFieldError::InvalidLength {
                        tag: f.tag,
                        expected: 2,
                        got: f.value.len(),
                    };
                    if strict {
                        return Err(KlvDecodeError::FieldError(e));
                    }
                    record.field_errors.push(e);
                    continue;
                }
                debug_assert_eq!(tag_u8, 22);
                record.version = Some(u16::from_be_bytes([f.value[0], f.value[1]]));
            }
            Encoding::Iso646 | Encoding::FixedAscii { .. } => {
                // ST 0107.5 §6.3.3.2: a single NUL byte = empty string "".
                let s = if f.value == [0x00] {
                    String::new() // empty string signal; skip FixedAscii length check
                } else {
                    let parsed = match core::str::from_utf8(f.value) {
                        Ok(s) => s.to_string(),
                        Err(_) => {
                            let e = KlvFieldError::InvalidUtf8 { tag: f.tag };
                            if strict {
                                return Err(KlvDecodeError::FieldError(e));
                            }
                            record.field_errors.push(e);
                            continue;
                        }
                    };
                    if strict && !parsed.is_ascii() {
                        return Err(KlvDecodeError::FieldError(KlvFieldError::NonAsciiText {
                            tag: f.tag,
                        }));
                    }
                    if let Encoding::FixedAscii { expected_len } = spec.encoding
                        && strict
                    {
                        if parsed.len() != expected_len {
                            return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                                tag: f.tag,
                                expected: expected_len,
                                got: parsed.len(),
                            }));
                        }
                        if !super::encode::is_fixed_date(tag_u8, &parsed) {
                            return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidFormat {
                                tag: f.tag,
                            }));
                        }
                    }
                    parsed
                };
                match tag_u8 {
                    3 => record.classifying_country = Some(s),
                    4 => record.sci_shi_info = Some(s),
                    5 => record.caveats = Some(s),
                    6 => record.releasing_instructions = Some(s),
                    7 => record.classified_by = Some(s),
                    8 => record.derived_from = Some(s),
                    9 => record.classification_reason = Some(s),
                    10 => record.declassification_date = Some(s),
                    11 => record.classification_marking_system = Some(s),
                    14 => record.classification_comments = Some(s),
                    23 => record.classifying_country_coding_method_version_date = Some(s),
                    24 => record.object_country_coding_method_version_date = Some(s),
                    _ => unreachable!("tags.rs Iso646/FixedAscii reserved for known tags"),
                }
            }
            Encoding::Utf16Bom => {
                debug_assert_eq!(tag_u8, 13);
                match decode_utf16_bom(f.value) {
                    Ok(s) => record.object_country_codes = Some(s),
                    Err(()) => {
                        if strict {
                            return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidUtf16 {
                                tag: f.tag,
                            }));
                        } else {
                            // Lenient: signal failure via field_errors per spec §3.5.
                            // Mirrors klv::st0601's pattern. Raw bytes are NOT
                            // preserved — re-emitting malformed Tag 13 wouldn't
                            // help any consumer; the caller wanting byte-level
                            // access goes through klv::pack::Iter directly.
                            record
                                .field_errors
                                .push(KlvFieldError::InvalidUtf16 { tag: f.tag });
                        }
                    }
                }
            }
        }
    }

    if strict {
        for &t in REQUIRED_TAGS {
            let present = match t {
                1 => record.security_classification.is_some(),
                2 => record.classifying_country_coding_method.is_some(),
                3 => record.classifying_country.is_some(),
                12 => record.object_country_coding_method.is_some(),
                13 => record.object_country_codes.is_some(),
                22 => record.version.is_some(),
                _ => true,
            };
            if !present {
                return Err(KlvDecodeError::St0102MissingRequiredTag { tag: t });
            }
        }
    }

    Ok(record)
}

/// Clear the typed field of `tag` (a defined tag of the set).
fn unset(record: &mut SecurityLs, tag: u8) {
    match tag {
        1 => record.security_classification = None,
        2 => record.classifying_country_coding_method = None,
        3 => record.classifying_country = None,
        4 => record.sci_shi_info = None,
        5 => record.caveats = None,
        6 => record.releasing_instructions = None,
        7 => record.classified_by = None,
        8 => record.derived_from = None,
        9 => record.classification_reason = None,
        10 => record.declassification_date = None,
        11 => record.classification_marking_system = None,
        12 => record.object_country_coding_method = None,
        13 => record.object_country_codes = None,
        14 => record.classification_comments = None,
        22 => record.version = None,
        23 => record.classifying_country_coding_method_version_date = None,
        24 => record.object_country_coding_method_version_date = None,
        _ => unreachable!("tags.rs defines no other tag"),
    }
}
