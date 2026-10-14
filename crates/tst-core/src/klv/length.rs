//! BER short/long and BER-OID length codecs.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! BER short form: single byte `0x00..0x7F`.
//! BER long form: `0x80 | n` followed by `n` big-endian length bytes (n in 1..=8).
//! BER-OID: base-128 integer, big-endian, MSB set on every non-final byte.

use crate::error::{KlvDecodeError, KlvEncodeError, KlvFieldError};

/// Read a BER length (short or long) from `buf`. Returns `(length, &buf[consumed..])`.
pub fn read_ber(buf: &[u8]) -> Result<(usize, &[u8]), KlvDecodeError> {
    let first = buf.first().ok_or(KlvDecodeError::Truncated {
        offset: 0,
        needed: 1,
        have: 0,
    })?;
    if *first < 0x80 {
        // Short form
        Ok((*first as usize, &buf[1..]))
    } else {
        // Long form: 0x80 | n
        let n = (*first & 0x7F) as usize;
        if n == 0 {
            // Indefinite length — not allowed for KLV.
            return Err(KlvDecodeError::MalformedLength { offset: 0 });
        }
        if n > 8 {
            // Length-of-length too large to fit in usize on most platforms.
            return Err(KlvDecodeError::MalformedLength { offset: 0 });
        }
        if buf.len() < 1 + n {
            return Err(KlvDecodeError::Truncated {
                offset: 1,
                needed: n,
                have: buf.len() - 1,
            });
        }
        let mut value: u64 = 0;
        for &b in &buf[1..1 + n] {
            value = (value << 8) | b as u64;
        }
        if value > usize::MAX as u64 {
            return Err(KlvDecodeError::LengthOverflow { value });
        }
        Ok((value as usize, &buf[1 + n..]))
    }
}

/// Strict variant of [`read_ber`] that rejects non-canonical encodings per
/// MISB ST 0107.5 §6.3.2 ("encoders shall use the fewest bytes"):
///   * long-form for values that fit in short form (value < 128)
///   * long-form with a leading zero byte (e.g. `0x82 0x00 0x10` for value 16)
///
/// Use in compliance-validation paths only; default decode keeps the
/// permissive `read_ber` for legacy capture interop.
pub fn read_ber_strict(buf: &[u8]) -> Result<(usize, &[u8]), KlvDecodeError> {
    let (value, rest) = read_ber(buf)?;
    let first = buf[0]; // safe — read_ber would have errored on empty buf
    if first & 0x80 != 0 {
        // Long form. Reject if value would have fit in short form.
        if value < 0x80 {
            return Err(KlvDecodeError::NonCanonicalLength { offset: 0 });
        }
        // Reject if the long-form payload starts with a zero byte (overlong).
        let n = (first & 0x7F) as usize;
        if n > 0 && buf.len() > n && buf[1] == 0 {
            return Err(KlvDecodeError::NonCanonicalLength { offset: 1 });
        }
    }
    Ok((value, rest))
}

/// Read a BER-OID variable-length integer from `buf`. Returns `(value, &buf[consumed..])`.
pub fn read_ber_oid(buf: &[u8]) -> Result<(u32, &[u8]), KlvDecodeError> {
    let mut value: u64 = 0;
    let mut consumed = 0;
    loop {
        let b = buf.get(consumed).ok_or(KlvDecodeError::Truncated {
            offset: consumed,
            needed: 1,
            have: 0,
        })?;
        consumed += 1;
        value = (value << 7) | (*b as u64 & 0x7F);
        if value > u32::MAX as u64 {
            return Err(KlvDecodeError::LengthOverflow { value });
        }
        if b & 0x80 == 0 {
            return Ok((value as u32, &buf[consumed..]));
        }
        if consumed > 5 {
            // u32 fits in at most 5 BER-OID bytes.
            return Err(KlvDecodeError::MalformedTag { offset: 0 });
        }
    }
}

/// Strict variant of [`read_ber_oid`] that rejects non-canonical encodings
/// per MISB ST 0107.5 §6.3.1 (PDF p.4): "ASN.1 forbids the use of `0x80`
/// in the first byte of a BER-OID value." A leading `0x80` is the
/// continuation-bit-set encoding of value 0, but value 0 must always be
/// encoded as a single byte `0x00`.
pub fn read_ber_oid_strict(buf: &[u8]) -> Result<(u32, &[u8]), KlvDecodeError> {
    if buf.first() == Some(&0x80) {
        return Err(KlvDecodeError::NonCanonicalTag { offset: 0 });
    }
    read_ber_oid(buf)
}

/// Add `base` to every offset-carrying [`KlvDecodeError`] variant the
/// strict BER readers can produce. (Other offset-carrying variants exist
/// on the enum — e.g. `DuplicateTag`, `St0903InvalidVTargetPack` — but
/// neither `read_ber_oid_strict` nor `read_ber_strict` ever originates
/// them, so they're deliberately left out of the match below.)
///
/// `read_ber_oid_strict` / `read_ber_strict` report offsets relative to
/// the slice they were handed, not the caller's original buffer; a
/// strict-mode TLV walker rebases each error to the item's true position
/// before surfacing it. Shared by the ST 0601 / ST 0102 / ST 0903 strict
/// pre-walks, which each independently re-implemented this as 2-3
/// identical `if let Variant { offset } = &mut e { *offset += x }` arms
/// per read call.
pub(crate) fn rebase_offset(e: &mut KlvDecodeError, base: usize) {
    match e {
        KlvDecodeError::Truncated { offset, .. }
        | KlvDecodeError::NonCanonicalTag { offset }
        | KlvDecodeError::MalformedTag { offset }
        | KlvDecodeError::NonCanonicalLength { offset }
        | KlvDecodeError::MalformedLength { offset } => *offset += base,
        _ => {}
    }
}

/// One step of a strict-mode TLV pre-walk: a strict BER-OID tag (per
/// ST 0107.5 §6.3.1) followed by a strict BER length (per §6.3.2), with
/// every offset-carrying error rebased to `base` — the item's start
/// position in the caller's original buffer — via [`rebase_offset`].
///
/// Returns `(tag, declared_len, consumed, after_len)`. `declared_len` is
/// the length field's value, NOT yet bounds-checked against
/// `after_len.len()` — callers differ on how they handle a truncated
/// value (ST 0601 / ST 0903 hard-error immediately; ST 0102 stops the
/// strict pre-walk and defers to the later permissive pass), so that
/// check stays at the call site. `consumed` is the tag+length byte
/// count — `buf[..consumed]` is this TLV's header, and `after_len` is
/// everything past it (potentially shorter than `declared_len`).
pub(crate) fn read_strict_tlv(
    buf: &[u8],
    base: usize,
) -> Result<(u32, usize, usize, &[u8]), KlvDecodeError> {
    let (tag, after_tag) = read_ber_oid_strict(buf).map_err(|mut e| {
        rebase_offset(&mut e, base);
        e
    })?;
    let consumed_tag = buf.len() - after_tag.len();
    let (declared_len, after_len) = read_ber_strict(after_tag).map_err(|mut e| {
        rebase_offset(&mut e, base + consumed_tag);
        e
    })?;
    let consumed_len = after_tag.len() - after_len.len();
    Ok((tag, declared_len, consumed_tag + consumed_len, after_len))
}

/// Number of bytes BER would use to encode `value`.
pub fn ber_len(value: usize) -> usize {
    if value < 0x80 {
        1
    } else {
        let mut bytes = 0usize;
        let mut v = value;
        while v > 0 {
            bytes += 1;
            v >>= 8;
        }
        1 + bytes
    }
}

/// Number of bytes BER-OID would use to encode `value`.
pub fn ber_oid_len(value: u32) -> usize {
    if value == 0 {
        return 1;
    }
    let mut bytes = 0usize;
    let mut v = value as u64;
    while v > 0 {
        bytes += 1;
        v >>= 7;
    }
    bytes
}

/// Write a BER length (auto short/long) to `out`. Returns bytes written.
pub fn write_ber(value: usize, out: &mut [u8]) -> Result<usize, KlvEncodeError> {
    let needed = ber_len(value);
    if out.len() < needed {
        return Err(KlvEncodeError::BufferTooSmall {
            needed,
            got: out.len(),
        });
    }
    if value < 0x80 {
        out[0] = value as u8;
        Ok(1)
    } else {
        let n = needed - 1;
        if n > 8 {
            return Err(KlvEncodeError::RecordTooLarge);
        }
        out[0] = 0x80 | n as u8;
        for i in 0..n {
            out[1 + i] = ((value >> (8 * (n - 1 - i))) & 0xFF) as u8;
        }
        Ok(needed)
    }
}

/// `u64` sibling of [`read_ber_oid`] — for ST 0903 VMTI `targetId`
/// (§10.2.2.1 permits up to 9 BER-OID bytes ≈ 63-bit). Accumulates to `u64`
/// (up to 10 bytes / 64 bits) before [`KlvDecodeError::LengthOverflow`].
pub fn read_ber_oid_u64(buf: &[u8]) -> Result<(u64, &[u8]), KlvDecodeError> {
    let mut value: u128 = 0;
    let mut consumed = 0;
    loop {
        let b = buf.get(consumed).ok_or(KlvDecodeError::Truncated {
            offset: consumed,
            needed: 1,
            have: 0,
        })?;
        consumed += 1;
        value = (value << 7) | (*b as u128 & 0x7F);
        if value > u64::MAX as u128 {
            return Err(KlvDecodeError::LengthOverflow { value: u64::MAX }); // saturate the report
        }
        if b & 0x80 == 0 {
            return Ok((value as u64, &buf[consumed..]));
        }
        if consumed > 10 {
            // u64 fits in at most 10 BER-OID bytes.
            return Err(KlvDecodeError::MalformedTag { offset: 0 });
        }
    }
}

/// Number of bytes BER-OID would use to encode `value` as `u64`.
pub fn ber_oid_len_u64(value: u64) -> usize {
    if value == 0 {
        return 1;
    }
    let mut bytes = 0usize;
    let mut v = value;
    while v > 0 {
        bytes += 1;
        v >>= 7;
    }
    bytes
}

/// `u64` sibling of [`write_ber_oid`].
pub fn write_ber_oid_u64(value: u64, out: &mut [u8]) -> Result<usize, KlvEncodeError> {
    let needed = ber_oid_len_u64(value);
    if out.len() < needed {
        return Err(KlvEncodeError::BufferTooSmall {
            needed,
            got: out.len(),
        });
    }
    for (i, slot) in out.iter_mut().enumerate().take(needed) {
        let shift = 7 * (needed - 1 - i);
        let mut byte = ((value >> shift) & 0x7F) as u8;
        if i + 1 < needed {
            byte |= 0x80;
        }
        *slot = byte;
    }
    Ok(needed)
}

/// Write a BER-OID variable-length integer to `out`. Returns bytes written.
///
/// Delegates to [`write_ber_oid_u64`] — byte-exact for all u32 values
/// (the u64 algorithm is identical for inputs ≤ u32::MAX).
pub fn write_ber_oid(value: u32, out: &mut [u8]) -> Result<usize, KlvEncodeError> {
    write_ber_oid_u64(value as u64, out)
}

/// MISB variable-length unsigned int (length-prefixed truncatable form used
/// by ST 0601 items 110/111/123-126/131/133 and ST 0903.6's VarUint fields
/// per §9.1): plain big-endian bytes, the TLV length IS the byte count.
/// NOT BER-OID.
///
/// Hard substrate cap: at most 8 value bytes (the `u64` accumulator width) —
/// longer inputs are rejected as `InvalidLength` regardless of how permissive
/// the caller's `max_len` is (a `max_len > 8` must not re-enable silent
/// leading-byte truncation).
pub(crate) fn read_var_uint(bytes: &[u8], max_len: usize, tag: u32) -> Result<u64, KlvFieldError> {
    if bytes.is_empty() || bytes.len() > max_len || bytes.len() > 8 {
        return Err(KlvFieldError::InvalidLength {
            tag,
            expected: max_len.min(8), // report the effective ceiling
            got: bytes.len(),
        });
    }
    Ok(bytes.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64))
}

/// Signed twin (items 136/137): two's complement, sign-extended from the MSB.
///
/// Inherits the 8-byte substrate cap (the `i64` width) via [`read_var_uint`]
/// delegation — inputs over 8 bytes are rejected regardless of `max_len`,
/// which also keeps the sign-extension shift below in `0..=56`.
pub(crate) fn read_var_int(bytes: &[u8], max_len: usize, tag: u32) -> Result<i64, KlvFieldError> {
    let raw = read_var_uint(bytes, max_len, tag)?;
    let shift = 64 - 8 * bytes.len() as u32;
    Ok(((raw << shift) as i64) >> shift)
}

/// Byte count [`write_var_uint_min`] would emit for `v` (>= 1). Split out
/// so a sizing pass (e.g. st0601 `encode::each_typed_field`) can compute
/// the wire length without allocating a throwaway `Vec`.
pub(crate) fn var_uint_min_len(v: u64) -> usize {
    (8 - (v.leading_zeros() / 8) as usize).max(1)
}

/// Shortest-form big-endian emit (>= 1 byte).
pub(crate) fn write_var_uint_min(v: u64) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    write_var_uint_min_into(v, &mut out);
    out
}

/// Extend-style sibling of [`write_var_uint_min`] — appends the
/// shortest-form bytes to an existing `Vec` (e.g. an st0903 TLV-value
/// scratch buffer) instead of allocating a fresh one.
pub(crate) fn write_var_uint_min_into(v: u64, out: &mut alloc::vec::Vec<u8>) {
    let n = var_uint_min_len(v);
    out.extend_from_slice(&v.to_be_bytes()[8 - n..]);
}

/// Byte count [`write_var_int_min`] would emit for `v` (>= 1). Sizing
/// sibling of [`var_uint_min_len`].
pub(crate) fn var_int_min_len(v: i64) -> usize {
    let bytes = v.to_be_bytes();
    let mut n = 8;
    // Drop redundant leading 0x00 (positive) / 0xFF (negative) while the
    // next byte's MSB still encodes the sign.
    while n > 1 {
        let lead = bytes[8 - n];
        let next_msb = bytes[8 - n + 1] & 0x80;
        if (lead == 0x00 && next_msb == 0) || (lead == 0xFF && next_msb != 0) {
            n -= 1;
        } else {
            break;
        }
    }
    n
}

/// Minimal two's-complement emit preserving the sign bit (>= 1 byte).
pub(crate) fn write_var_int_min(v: i64) -> alloc::vec::Vec<u8> {
    let n = var_int_min_len(v);
    v.to_be_bytes()[8 - n..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- BER ----------

    #[test]
    fn ber_short_round_trip() {
        for v in [0usize, 1, 0x7E, 0x7F] {
            let mut buf = [0u8; 16];
            let n = write_ber(v, &mut buf).unwrap();
            assert_eq!(n, 1, "short form is one byte (v={v})");
            let (parsed, rest) = read_ber(&buf[..n]).unwrap();
            assert_eq!(parsed, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn ber_long_one_byte_boundary() {
        // 0x80 is the smallest long-form value
        let mut buf = [0u8; 16];
        let n = write_ber(0x80, &mut buf).unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf[0], 0x81);
        assert_eq!(buf[1], 0x80);
        let (parsed, _) = read_ber(&buf[..n]).unwrap();
        assert_eq!(parsed, 0x80);
    }

    #[test]
    fn ber_long_two_byte_boundary() {
        let mut buf = [0u8; 16];
        let n = write_ber(0xFFFF, &mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], &[0x82, 0xFF, 0xFF]);
    }

    #[test]
    fn ber_long_three_byte_boundary() {
        let mut buf = [0u8; 16];
        let n = write_ber(0x010000, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..n], &[0x83, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn ber_long_round_trip_sweep() {
        for v in [0xFFusize, 0x100, 0xFFFF, 0x10000, 0xFFFFFF, 0x1_000_000] {
            let mut buf = [0u8; 16];
            let n = write_ber(v, &mut buf).unwrap();
            let (parsed, rest) = read_ber(&buf[..n]).unwrap();
            assert_eq!(parsed, v, "round trip failed for v={v}");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn ber_buffer_too_small() {
        let mut buf = [0u8; 1];
        let err = write_ber(0x100, &mut buf).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::BufferTooSmall { needed: 3, got: 1 }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_truncated_long_form() {
        // 0x82 says "next 2 bytes are length" but we provide only 1
        let buf = [0x82, 0xFF];
        let err = read_ber(&buf).unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::Truncated { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_indefinite_form_rejected() {
        let buf = [0x80];
        let err = read_ber(&buf).unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::MalformedLength { offset: 0 }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_long_form_too_many_bytes_rejected() {
        // 0x89 = "9 length bytes", bigger than u64
        let buf = [0x89, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let err = read_ber(&buf).unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::MalformedLength { offset: 0 }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_empty_input_truncated() {
        let buf: [u8; 0] = [];
        let err = read_ber(&buf).unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::Truncated { .. }),
            "unexpected error: {err:?}"
        );
    }

    // ---------- BER-OID ----------

    #[test]
    fn ber_oid_zero() {
        let mut buf = [0u8; 8];
        let n = write_ber_oid(0, &mut buf).unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], 0x00);
        let (parsed, _) = read_ber_oid(&buf[..n]).unwrap();
        assert_eq!(parsed, 0);
    }

    #[test]
    fn ber_oid_single_byte_max() {
        let mut buf = [0u8; 8];
        let n = write_ber_oid(0x7F, &mut buf).unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], 0x7F);
    }

    #[test]
    fn ber_oid_two_byte_min() {
        let mut buf = [0u8; 8];
        let n = write_ber_oid(0x80, &mut buf).unwrap();
        assert_eq!(n, 2);
        // 0x80 = 10000000_2; split into 7-bit groups: [1, 0]; with continuation: 0x81, 0x00.
        assert_eq!(&buf[..n], &[0x81, 0x00]);
    }

    // --- KLV-2: write_ber_oid delegates to write_ber_oid_u64 (boundary) ---

    #[test]
    fn write_ber_oid_matches_u64_sibling_at_u32_boundary() {
        // write_ber_oid(v) must produce the same bytes as write_ber_oid_u64(v as u64)
        // for the two sentinel values: u32::MAX (max u32) and u32::MAX+1=2^32 (u64-only).
        for v in [0u32, 1u32, 0x7Fu32, 0x80u32, u32::MAX - 1, u32::MAX] {
            let mut buf32 = [0u8; 8];
            let mut buf64 = [0u8; 8];
            let n32 = write_ber_oid(v, &mut buf32).unwrap();
            let n64 = write_ber_oid_u64(v as u64, &mut buf64).unwrap();
            assert_eq!(n32, n64, "byte count mismatch for v={v}");
            assert_eq!(
                &buf32[..n32],
                &buf64[..n64],
                "byte content mismatch for v={v}"
            );
        }
        // Also check that 2^32 is beyond u32 range: u64 encodes it in 5 bytes.
        let mut buf = [0u8; 8];
        let n = write_ber_oid_u64(u32::MAX as u64 + 1, &mut buf).unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn ber_oid_round_trip_sweep() {
        for v in [
            0u32, 1, 0x7F, 0x80, 0xFF, 0x3FFF, 0x4000, 0xFFFF, 0xFFFFFFFF,
        ] {
            let mut buf = [0u8; 8];
            let n = write_ber_oid(v, &mut buf).unwrap();
            let (parsed, rest) = read_ber_oid(&buf[..n]).unwrap();
            assert_eq!(parsed, v, "round trip failed for v={v}");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn ber_oid_truncated_continuation() {
        // 0x80 alone has the continuation bit set but no following byte
        let buf = [0x80];
        let err = read_ber_oid(&buf).unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::Truncated { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_oid_too_long_rejected() {
        // 6 continuation bytes — overflows u32
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        let err = read_ber_oid(&buf).unwrap_err();
        assert!(
            matches!(
                err,
                KlvDecodeError::LengthOverflow { .. } | KlvDecodeError::MalformedTag { .. }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_oid_buffer_too_small() {
        let mut buf = [0u8; 1];
        let err = write_ber_oid(0x80, &mut buf).unwrap_err();
        assert!(
            matches!(err, KlvEncodeError::BufferTooSmall { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn ber_oid_remaining_returns_correct_slice() {
        let buf = [0x05, 0xAA, 0xBB];
        let (v, rest) = read_ber_oid(&buf).unwrap();
        assert_eq!(v, 5);
        assert_eq!(rest, &[0xAA, 0xBB]);
    }

    // ---------- Strict variants ----------

    #[test]
    fn ber_strict_accepts_canonical_short_form() {
        // 0x10 encodes value 16 in canonical short form.
        let buf = [0x10];
        let (v, _) = read_ber_strict(&buf).unwrap();
        assert_eq!(v, 16);
    }

    #[test]
    fn ber_strict_accepts_canonical_long_form() {
        // 0x82 0x01 0x00 encodes value 256 in canonical long form.
        let buf = [0x82, 0x01, 0x00];
        let (v, _) = read_ber_strict(&buf).unwrap();
        assert_eq!(v, 256);
    }

    #[test]
    fn ber_strict_rejects_long_form_for_short_value() {
        // 0x81 0x10 = long-form encoding of value 16 (which fits in short
        // form 0x10). Per ST 0107.5 §6.3.2 (fewest-bytes), reject.
        let buf = [0x81, 0x10];
        let err = read_ber_strict(&buf).unwrap_err();
        assert!(matches!(err, KlvDecodeError::NonCanonicalLength { .. }));
        // Permissive read_ber still accepts it for legacy interop.
        let (v, _) = read_ber(&buf).unwrap();
        assert_eq!(v, 16);
    }

    #[test]
    fn ber_strict_rejects_long_form_with_leading_zero() {
        // 0x82 0x00 0x10 = long-form value 16 with overlong encoding.
        let buf = [0x82, 0x00, 0x10];
        let err = read_ber_strict(&buf).unwrap_err();
        assert!(matches!(err, KlvDecodeError::NonCanonicalLength { .. }));
    }

    #[test]
    fn ber_oid_strict_rejects_leading_0x80() {
        // 0x80 0x00 = continuation-set encoding of value 0. Per ST 0107.5
        // §6.3.1 forbidden — value 0 must be the single byte 0x00.
        let buf = [0x80, 0x00];
        let err = read_ber_oid_strict(&buf).unwrap_err();
        assert!(matches!(err, KlvDecodeError::NonCanonicalTag { .. }));
        // Permissive read_ber_oid accepts it.
        let (v, _) = read_ber_oid(&buf).unwrap();
        assert_eq!(v, 0);
    }

    #[test]
    fn ber_oid_strict_accepts_canonical_zero() {
        let buf = [0x00];
        let (v, _) = read_ber_oid_strict(&buf).unwrap();
        assert_eq!(v, 0);
    }

    // ---------- BER-OID u64 ----------

    #[test]
    fn ber_oid_u64_round_trip_above_u32() {
        // Values that need > 5 BER-OID bytes (beyond u32 range).
        for v in [
            0u64,
            0x7F,
            0x80,
            u32::MAX as u64,
            (u32::MAX as u64) + 1,
            0x1_0000_0000_0000,
            u64::MAX >> 1, // up to 63-bit
        ] {
            let mut buf = [0u8; 10];
            let n = write_ber_oid_u64(v, &mut buf).unwrap();
            assert_eq!(n, ber_oid_len_u64(v));
            let (parsed, rest) = read_ber_oid_u64(&buf[..n]).unwrap();
            assert_eq!(parsed, v, "round trip failed for v={v}");
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn ber_oid_u64_max_round_trip() {
        let mut buf = [0u8; 10];
        let n = write_ber_oid_u64(u64::MAX, &mut buf).unwrap();
        assert_eq!(n, 10, "u64::MAX needs 10 BER-OID bytes (ceil(64/7))");
        let (parsed, _) = read_ber_oid_u64(&buf[..n]).unwrap();
        assert_eq!(parsed, u64::MAX);
    }

    // ---------- MISB variable-length truncatable ints ----------

    #[test]
    fn var_uint_reads_and_shortest_writes() {
        assert_eq!(read_var_uint(&[0x4D, 0xAF], 4, 110).unwrap(), 19887); // Tag 110 spec example
        assert_eq!(read_var_uint(&[0x00], 4, 110).unwrap(), 0);
        assert!(read_var_uint(&[], 4, 110).is_err()); // zero-length = malformed
        assert!(read_var_uint(&[0; 5], 4, 110).is_err()); // over max_len
        assert_eq!(write_var_uint_min(19887), vec![0x4D, 0xAF]);
        assert_eq!(write_var_uint_min(0), vec![0x00]); // never empty
        assert_eq!(write_var_uint_min(0x0100), vec![0x01, 0x00]);
    }

    #[test]
    fn var_int_sign_handling() {
        assert_eq!(read_var_int(&[0x1E], 4, 136).unwrap(), 30); // Tag 136 spec example
        assert_eq!(read_var_int(&[0xE2], 4, 136).unwrap(), -30); // sign-extended
        assert_eq!(
            read_var_int(&[0x01, 0x2B, 0x8D, 0xC6, 0x35], 8, 137).unwrap(),
            5_025_678_901
        ); // Tag 137
        assert_eq!(write_var_int_min(30), vec![0x1E]);
        assert_eq!(write_var_int_min(-30), vec![0xE2]);
        assert_eq!(write_var_int_min(128), vec![0x00, 0x80]); // needs a sign byte
        assert_eq!(write_var_int_min(-129), vec![0xFF, 0x7F]);
    }

    #[test]
    fn var_read_len_equal_to_max_accepted() {
        // len == max_len is inside the contract (only > rejects).
        assert_eq!(read_var_uint(&[0xAA, 0xBB], 2, 110).unwrap(), 0xAABB);
        assert_eq!(read_var_int(&[0xFF, 0x7F], 2, 136).unwrap(), -129);
    }

    #[test]
    fn var_read_rejects_over_8_bytes_even_if_max_len_allows() {
        // Substrate cap: u64/i64 hold at most 8 bytes. A permissive caller
        // max_len must not re-enable silent truncation (uint accumulate
        // shifts leading bytes out of the u64) or a shift-overflow panic
        // (int computes 64 - 8*len).
        assert!(read_var_uint(&[0x01; 9], 9, 110).is_err());
        assert!(read_var_int(&[0x01; 9], 9, 137).is_err());
    }

    #[test]
    fn var_write_full_width_and_sign_boundaries() {
        assert_eq!(write_var_uint_min(u64::MAX), vec![0xFF; 8]);
        assert_eq!(write_var_int_min(0), vec![0x00]);
        assert_eq!(write_var_int_min(-1), vec![0xFF]);
        assert_eq!(write_var_int_min(127), vec![0x7F]);
        assert_eq!(write_var_int_min(-128), vec![0x80]);
        assert_eq!(
            write_var_int_min(i64::MIN),
            vec![0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            write_var_int_min(i64::MAX),
            vec![0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }
}
