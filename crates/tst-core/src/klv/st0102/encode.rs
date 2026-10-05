//! ST 0102 encode entry points (`encode`, `encode_to_vec`, `encode_strict_compliance`, `encoded_len`).

use crate::error::KlvEncodeError;
use crate::klv::pack::is_typed_tag;
use crate::klv::st0102::model::{SecurityLs, encode_utf16_bom};
use alloc::vec::Vec;

/// Encode into a caller-provided buffer. Returns the number of bytes
/// written.
///
/// # Errors
/// - [`KlvEncodeError::BufferTooSmall`] if `out.len()` is less than
///   the required encoded length (call [`encoded_len`] first).
/// - [`KlvEncodeError::RecordTooLarge`] if any individual TLV's
///   declared length would overflow BER encoding (in practice,
///   a value > 2^64 bytes — guards against pathological input).
pub fn encode(record: &SecurityLs, out: &mut [u8]) -> Result<usize, KlvEncodeError> {
    use crate::klv::length::{write_ber, write_ber_oid};

    let needed = encoded_len(record);
    if out.len() < needed {
        return Err(KlvEncodeError::BufferTooSmall {
            needed,
            got: out.len(),
        });
    }

    let mut pos = 0usize;
    // Defined ST 0102 LS tags are all ≤ 127 (single-byte BER-OID); a
    // dedicated single-byte writer keeps the call sites concise. The
    // multi-byte BER-OID path is reserved for the `unknown` loop
    // below — see comment there for why round-tripping multi-byte
    // tags matters for forward-compat.
    let emit =
        |out: &mut [u8], pos: &mut usize, tag: u8, value: &[u8]| -> Result<(), KlvEncodeError> {
            out[*pos] = tag;
            *pos += 1;
            let n = write_ber(value.len(), &mut out[*pos..])
                .map_err(|_| KlvEncodeError::RecordTooLarge)?;
            *pos += n;
            out[*pos..*pos + value.len()].copy_from_slice(value);
            *pos += value.len();
            Ok(())
        };

    // Emit tags in numeric order for determinism. The wire spec
    // doesn't mandate ordering for ST 0102 LS — unlike ST 0601's
    // "tag 2 first, tag 1 last" rules — but emitting numeric is
    // friendlier to byte-diff tooling.
    if let Some(v) = record.security_classification {
        emit(out, &mut pos, 1, &[v.to_u8()])?;
    }
    if let Some(v) = record.classifying_country_coding_method {
        emit(out, &mut pos, 2, &[v.to_u8()])?;
    }
    // ST 0107.5 §6.3.3.2: empty string → single NUL byte on the wire.
    // Non-empty strings encode as raw UTF-8 bytes.
    if let Some(s) = record.classifying_country.as_ref() {
        emit(out, &mut pos, 3, str_wire(s))?;
    }
    if let Some(s) = record.sci_shi_info.as_ref() {
        emit(out, &mut pos, 4, str_wire(s))?;
    }
    if let Some(s) = record.caveats.as_ref() {
        emit(out, &mut pos, 5, str_wire(s))?;
    }
    if let Some(s) = record.releasing_instructions.as_ref() {
        emit(out, &mut pos, 6, str_wire(s))?;
    }
    if let Some(s) = record.classified_by.as_ref() {
        emit(out, &mut pos, 7, str_wire(s))?;
    }
    if let Some(s) = record.derived_from.as_ref() {
        emit(out, &mut pos, 8, str_wire(s))?;
    }
    if let Some(s) = record.classification_reason.as_ref() {
        emit(out, &mut pos, 9, str_wire(s))?;
    }
    if let Some(s) = record.declassification_date.as_ref() {
        emit(out, &mut pos, 10, str_wire(s))?;
    }
    if let Some(s) = record.classification_marking_system.as_ref() {
        emit(out, &mut pos, 11, str_wire(s))?;
    }
    if let Some(v) = record.object_country_coding_method {
        emit(out, &mut pos, 12, &[v.to_u8()])?;
    }
    if let Some(s) = record.object_country_codes.as_ref() {
        let utf16 = encode_utf16_bom(s);
        emit(out, &mut pos, 13, &utf16)?;
    }
    if let Some(s) = record.classification_comments.as_ref() {
        emit(out, &mut pos, 14, str_wire(s))?;
    }
    if let Some(v) = record.version {
        emit(out, &mut pos, 22, &v.to_be_bytes())?;
    }
    if let Some(s) = record
        .classifying_country_coding_method_version_date
        .as_ref()
    {
        emit(out, &mut pos, 23, str_wire(s))?;
    }
    if let Some(s) = record.object_country_coding_method_version_date.as_ref() {
        emit(out, &mut pos, 24, str_wire(s))?;
    }

    // Emit unknown tags last to preserve forward-compat. Tags above
    // 127 use multi-byte BER-OID encoding per ST 0107 §6.3.1 (also
    // SMPTE ST 336 §6) — write them via `write_ber_oid` so future
    // ST 0102 security-marking extensions (which the decoder already
    // preserves in `unknown`) survive an encode/decode round-trip.
    // `encoded_len` sizes via `ber_oid_len(tag)` to keep the
    // buffer-size precheck consistent.
    for u in record.unknown.iter() {
        // Reject reserved/typed tags before emitting. Without this guard,
        // a caller-constructed typed tag (e.g. Tag 3 = Classifying Country)
        // in `unknown` would produce a duplicate that ST 0102 decode_strict
        // rejects as DuplicateTag. The `unknown` vec is for forward-compat
        // pass-through only. Mirrors st0601::encode::write_unknown_fields.
        if is_typed_tag(u.tag, super::tags::lookup) {
            return Err(KlvEncodeError::ReservedTagInUnknown { tag: u.tag });
        }
        let n =
            write_ber_oid(u.tag, &mut out[pos..]).map_err(|_| KlvEncodeError::RecordTooLarge)?;
        pos += n;
        let n = write_ber(u.value.len(), &mut out[pos..])
            .map_err(|_| KlvEncodeError::RecordTooLarge)?;
        pos += n;
        out[pos..pos + u.value.len()].copy_from_slice(&u.value);
        pos += u.value.len();
    }

    Ok(pos)
}

/// Encode into a fresh `Vec<u8>`. Convenience over [`encode`] when
/// the caller has no pre-sized buffer.
///
/// # Errors
/// Returns the same [`KlvEncodeError`] variants as [`encode`] minus
/// [`KlvEncodeError::BufferTooSmall`] (the buffer is pre-sized via
/// [`encoded_len`]). [`KlvEncodeError::RecordTooLarge`] can still
/// fire for pathological per-TLV lengths.
pub fn encode_to_vec(record: &SecurityLs) -> Result<Vec<u8>, KlvEncodeError> {
    let mut buf = vec![0u8; encoded_len(record)];
    let n = encode(record, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Encode with required-field validation per ST 0102.12 §6.7 Table 2.
///
/// Mirrors [`crate::klv::st0601::encode_strict_compliance`] in shape.
/// Rejects any record missing one of the six required tags
/// (1, 2, 3, 12, 13, 22) before delegating to [`encode_to_vec`].
/// The default [`encode`] stays lenient — ST 0102.12 §6.4 permits
/// valid partial Security records; strict-mode is opt-in.
///
/// Required-tag order matches `REQUIRED_TAGS` (`[1,2,3,12,13,22]`);
/// the first absent tag wins so callers get a deterministic error.
///
/// Strict-encoded output always passes [`crate::klv::st0102::decode_strict`]
/// (symmetric contract: required field present on encode ↔ not rejected
/// by decode).
///
/// # Errors
///
/// - [`KlvEncodeError::MissingMandatoryItem`] with `tag` and `name` for
///   the first required tag whose `Option` field is `None`.
/// - All [`KlvEncodeError`] variants that [`encode_to_vec`] can return,
///   surfaced verbatim once the strict precondition gate passes.
pub fn encode_strict_compliance(record: &SecurityLs) -> Result<Vec<u8>, KlvEncodeError> {
    use super::tags::REQUIRED_TAGS;

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
            let name: &'static str = match t {
                1 => "Security Classification",
                2 => "Classifying Country Coding Method",
                3 => "Classifying Country",
                12 => "Object Country Coding Method",
                13 => "Object Country Codes",
                22 => "Version",
                _ => "Unknown",
            };
            return Err(KlvEncodeError::MissingMandatoryItem {
                tag: u16::from(t),
                name,
            });
        }
    }
    // ST 0107.5 §6.3.3: sanitize all string fields before encoding — remove
    // banned control chars everywhere (ST 0107.3-13) and trim leading/trailing
    // null/tab/LF/CR/space (ST 0107.3-12). Sanitization runs before the
    // ST 0107.5 §6.3.3.2 empty-string mapping, so a field that sanitizes to
    // "" encodes as [0x00].
    let mut r = record.clone();
    sanitize_strings_st0102(&mut r);
    encode_to_vec(&r)
}

/// Pre-compute the encoded length for a given record.
pub fn encoded_len(record: &SecurityLs) -> usize {
    use crate::klv::length::{ber_len, ber_oid_len};

    let mut total = 0usize;
    let mut add = |value_len: usize| {
        total += 1 /* tag byte */ + ber_len(value_len) + value_len;
    };

    if record.security_classification.is_some() {
        add(1);
    }
    if record.classifying_country_coding_method.is_some() {
        add(1);
    }
    if let Some(s) = record.classifying_country.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.sci_shi_info.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.caveats.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.releasing_instructions.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.classified_by.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.derived_from.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.classification_reason.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.declassification_date.as_ref() {
        add(str_wire_len(s));
    }
    if let Some(s) = record.classification_marking_system.as_ref() {
        add(str_wire_len(s));
    }
    if record.object_country_coding_method.is_some() {
        add(1);
    }
    if let Some(s) = record.object_country_codes.as_ref() {
        // 2 bytes BOM + 2 bytes per UTF-16 code unit
        let utf16_bytes = 2 + s.encode_utf16().count() * 2;
        add(utf16_bytes);
    }
    if let Some(s) = record.classification_comments.as_ref() {
        add(str_wire_len(s));
    }
    if record.version.is_some() {
        add(2);
    }
    if let Some(s) = record
        .classifying_country_coding_method_version_date
        .as_ref()
    {
        add(str_wire_len(s));
    }
    if let Some(s) = record.object_country_coding_method_version_date.as_ref() {
        add(str_wire_len(s));
    }

    for u in record.unknown.iter() {
        // Re-emit unknown tags verbatim with multi-byte BER-OID support
        // per ST 0107 §6.3.1. The decoder already preserves any
        // forward-compat tag in `unknown`; sizing via `ber_oid_len`
        // ensures encode round-trip stays lossless for tags > 127.
        total += ber_oid_len(u.tag) + ber_len(u.value.len()) + u.value.len();
    }

    total
}

/// Wire byte count for a string field: empty → 1 (NUL signal), else `s.len()`.
fn str_wire_len(s: &str) -> usize {
    if s.is_empty() { 1 } else { s.len() }
}

/// Encode a string field per ST 0107.5 §6.3.3.2:
/// empty → single NUL byte `b"\x00"`, non-empty → raw UTF-8 bytes.
fn str_wire(s: &str) -> &[u8] {
    if s.is_empty() { b"\x00" } else { s.as_bytes() }
}

/// Sanitize all Iso646/FixedAscii string fields of a SecurityLs per
/// ST 0107.5 §6.3.3 (control-char removal + leading/trailing-whitespace
/// trim — see `sanitize_st0107_string`) before strict-compliance encode.
fn sanitize_strings_st0102(r: &mut super::model::SecurityLs) {
    use crate::klv::st0601::encode::sanitize_st0107_string;
    let strip = |s: &mut Option<alloc::string::String>| {
        if let Some(s) = s {
            *s = sanitize_st0107_string(s);
        }
    };
    strip(&mut r.classifying_country);
    strip(&mut r.sci_shi_info);
    strip(&mut r.caveats);
    strip(&mut r.releasing_instructions);
    strip(&mut r.classified_by);
    strip(&mut r.derived_from);
    strip(&mut r.classification_reason);
    strip(&mut r.declassification_date);
    strip(&mut r.classification_marking_system);
    strip(&mut r.classification_comments);
    strip(&mut r.classifying_country_coding_method_version_date);
    strip(&mut r.object_country_coding_method_version_date);
    // Tag 13 (object_country_codes) is UTF-16 BOM — control char stripping
    // is for single-byte Iso646/FixedAscii encodings; leave it unchanged.
}
