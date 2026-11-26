//! Generic KLV pack and unpack — `RawField`, `OwnedRawField`, `Iter`.
//! Knows nothing about ST 0601 specifically.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! `RawField<'a>` borrows from the input buffer (zero-alloc iteration).
//! `OwnedRawField` is the owned counterpart used by parsed records that
//! cross the FFI boundary — keeping the parsed record `'static`.

use crate::error::{KlvDecodeError, KlvEncodeError};
use crate::klv::length::{read_ber, read_ber_oid, write_ber, write_ber_oid};
use alloc::vec::Vec;

/// A KLV field that borrows its value bytes from a parent buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawField<'a> {
    pub tag: u32,
    pub value: &'a [u8],
}

/// A KLV field that owns its value bytes. Used by parsed records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnedRawField {
    pub tag: u32,
    pub value: Vec<u8>,
}

impl<'a> From<&RawField<'a>> for OwnedRawField {
    fn from(r: &RawField<'a>) -> Self {
        Self {
            tag: r.tag,
            value: r.value.to_vec(),
        }
    }
}

impl<'a> From<RawField<'a>> for OwnedRawField {
    fn from(r: RawField<'a>) -> Self {
        Self {
            tag: r.tag,
            value: r.value.to_vec(),
        }
    }
}

impl OwnedRawField {
    pub fn as_ref(&self) -> RawField<'_> {
        RawField {
            tag: self.tag,
            value: &self.value,
        }
    }
}

/// Iterator over a KLV body (post-UL, post-outer-length). Crate-internal:
/// the typed decoders (`klv::st0601`, `klv::st0102`, `klv::st0605`,
/// `klv::st0806`, `klv::st0903`) are the consumer-facing API; this is
/// BER/length-prefix substrate. Local-set form only (a BER-OID or a
/// one-byte tag, then a BER length); a future ST 0905 universal-set decoder
/// would need a separate iterator.
pub(crate) struct Iter<'a> {
    buf: &'a [u8],
    offset: usize,
    finished: bool,
    one_byte: bool,
}

impl<'a> Iter<'a> {
    /// Iterate a local-set body: BER-OID tag + BER length, repeating.
    /// Caller is responsible for stripping the outer UL + total length first.
    pub(crate) fn local_set(buf: &'a [u8]) -> Self {
        Self {
            buf,
            offset: 0,
            finished: false,
            one_byte: false,
        }
    }

    /// Iterate a local-set body whose label declares one-byte tags (SMPTE
    /// ST 336 Table 5, registry designator `0x03`: the Security Metadata LS
    /// and the VTarget Pack): every tag byte `0x00` to `0xFF` is the tag.
    pub(crate) fn local_set_one_byte(buf: &'a [u8]) -> Self {
        Self {
            one_byte: true,
            ..Self::local_set(buf)
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = Result<RawField<'a>, KlvDecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.offset >= self.buf.len() {
            return None;
        }
        self.next_local_set()
    }
}

impl<'a> Iter<'a> {
    fn next_local_set(&mut self) -> Option<Result<RawField<'a>, KlvDecodeError>> {
        let start = self.offset;
        let rest = &self.buf[start..];
        let (tag, after_tag) = if self.one_byte {
            // `next` only calls here with at least one byte left.
            (u32::from(rest[0]), &rest[1..])
        } else {
            match read_ber_oid(rest) {
                Ok(v) => v,
                Err(mut e) => {
                    crate::klv::length::rebase_offset(&mut e, start);
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        };
        let consumed_tag = rest.len() - after_tag.len();
        let (len, after_len) = match read_ber(after_tag) {
            Ok(v) => v,
            Err(mut e) => {
                crate::klv::length::rebase_offset(&mut e, start + consumed_tag);
                self.finished = true;
                return Some(Err(e));
            }
        };
        let consumed_len = after_tag.len() - after_len.len();
        if after_len.len() < len {
            self.finished = true;
            return Some(Err(KlvDecodeError::Truncated {
                offset: start + consumed_tag + consumed_len,
                needed: len,
                have: after_len.len(),
            }));
        }
        let value = &after_len[..len];
        self.offset = start + consumed_tag + consumed_len + len;
        Some(Ok(RawField { tag, value }))
    }
}

/// Return `true` if `tag` is a known typed item in `lookup`'s table.
///
/// `lookup` is a per-set function `fn(u8) -> Option<T>` (e.g.
/// `st0601::tags::lookup`, `st0903::tags::lookup`, `st0102::tags::lookup`,
/// `st0806::tags::lookup`, `vtarget_pack::model::pack_lookup`). Collapses
/// the 5 identical `is_reserved_or_typed_tag` function bodies across the
/// KLV encode modules.
///
/// Any tag value > 255 is by definition not in a u8-keyed typed table and
/// returns `false` without calling `lookup`.
pub(crate) fn is_typed_tag<T>(tag: u32, lookup: fn(u8) -> Option<T>) -> bool {
    match u8::try_from(tag) {
        Ok(t) => lookup(t).is_some(),
        Err(_) => false,
    }
}

/// Emit a single BER-OID-tagged TLV into a `Vec<u8>`.
///
/// Writes: `BER-OID(tag)` + `BER-length(value.len())` + `value`. This is
/// the repeated 6-line emit pattern shared by every unknown-tag loop and
/// typed-field emit in the ST 0601 / ST 0806 / ST 0903 / VTargetPack encoders.
///
/// Returns `Err` only if `write_ber_oid` rejects the tag (i.e. the tag
/// value is too large to encode as BER-OID, which is exceedingly unlikely
/// for any realistic ST 0601/0806/0903 tag number).
pub(crate) fn emit_ber_oid_tlv(
    tag: u32,
    value: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    let mut tag_buf = [0u8; 8]; // u32 fits in at most 5 BER-OID bytes; 8 ≥ 5
    let n = write_ber_oid(tag, &mut tag_buf)?;
    out.extend_from_slice(&tag_buf[..n]);
    let mut len_buf = [0u8; 9]; // BER length: 1 flag byte + up to 8 value bytes
    let m = write_ber(value.len(), &mut len_buf)?;
    out.extend_from_slice(&len_buf[..m]);
    out.extend_from_slice(value);
    Ok(())
}

/// The tag byte of a set whose label declares one-byte tags (SMPTE ST 336
/// Table 5, registry designator `0x03`); a tag above 255 has no such byte.
pub(crate) fn one_byte_tag(tag: u32) -> Result<u8, KlvEncodeError> {
    u8::try_from(tag).map_err(|_| KlvEncodeError::TagNotOneByte { tag })
}

/// Emit a single one-byte-tagged TLV into a `Vec<u8>`: the tag byte,
/// `BER-length(value.len())`, `value`. Refuses a tag above 255 with
/// [`KlvEncodeError::TagNotOneByte`].
pub(crate) fn emit_one_byte_tlv(
    tag: u32,
    value: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    out.push(one_byte_tag(tag)?);
    let mut len_buf = [0u8; 9]; // BER length: 1 flag byte + up to 8 value bytes
    let m = write_ber(value.len(), &mut len_buf)?;
    out.extend_from_slice(&len_buf[..m]);
    out.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_field_into_owned() {
        let raw = RawField {
            tag: 13,
            value: &[1, 2, 3],
        };
        let owned: OwnedRawField = (&raw).into();
        assert_eq!(owned.tag, 13);
        assert_eq!(owned.value, vec![1, 2, 3]);
    }

    #[test]
    fn owned_as_ref() {
        let owned = OwnedRawField {
            tag: 5,
            value: vec![0xAA, 0xBB],
        };
        let r = owned.as_ref();
        assert_eq!(r.tag, 5);
        assert_eq!(r.value, &[0xAA, 0xBB]);
    }

    #[test]
    fn iter_local_set_three_fields() {
        // Tag 1, len 2, [0xAA, 0xBB]
        // Tag 5, len 1, [0x42]
        // Tag 13, len 4, [0x01, 0x02, 0x03, 0x04]
        let buf = [
            0x01, 0x02, 0xAA, 0xBB, // tag=1, len=2
            0x05, 0x01, 0x42, // tag=5, len=1
            0x0D, 0x04, 0x01, 0x02, 0x03, 0x04, // tag=13, len=4
        ];
        let mut it = Iter::local_set(&buf);
        let f1 = it.next().unwrap().unwrap();
        assert_eq!(f1.tag, 1);
        assert_eq!(f1.value, &[0xAA, 0xBB]);
        let f2 = it.next().unwrap().unwrap();
        assert_eq!(f2.tag, 5);
        assert_eq!(f2.value, &[0x42]);
        let f3 = it.next().unwrap().unwrap();
        assert_eq!(f3.tag, 13);
        assert_eq!(f3.value, &[0x01, 0x02, 0x03, 0x04]);
        assert!(it.next().is_none());
    }

    #[test]
    fn iter_local_set_truncated_value() {
        // Tag 1, len 4, but only 2 bytes follow
        let buf = [0x01, 0x04, 0xAA, 0xBB];
        let mut it = Iter::local_set(&buf);
        let err = it.next().unwrap().unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::Truncated { .. }),
            "unexpected error: {err:?}"
        );
        assert!(it.next().is_none(), "iterator finished after error");
    }

    #[test]
    fn iter_local_set_truncated_length() {
        // Tag 1 followed by long-form length declaration that's truncated
        let buf = [0x01, 0x82, 0xFF];
        let mut it = Iter::local_set(&buf);
        let err = it.next().unwrap().unwrap_err();
        assert!(
            matches!(err, KlvDecodeError::Truncated { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn iter_local_set_empty() {
        let buf: [u8; 0] = [];
        let mut it = Iter::local_set(&buf);
        assert!(it.next().is_none());
    }

    #[test]
    fn iter_local_set_two_byte_tag() {
        // BER-OID tag 0x80 (= 0x81 0x00), len 1, [0x42]
        let buf = [0x81, 0x00, 0x01, 0x42];
        let mut it = Iter::local_set(&buf);
        let f = it.next().unwrap().unwrap();
        assert_eq!(f.tag, 0x80);
        assert_eq!(f.value, &[0x42]);
    }

    #[test]
    fn malformed_length_offset_is_body_absolute() {
        // item 1: tag 5, len 1, 'A'   → 3 bytes;  item 2: tag 6, len byte 0x80 (indefinite)
        let body = [0x05, 0x01, 0x41, 0x06, 0x80];
        let mut it = Iter::local_set(&body);
        assert!(it.next().unwrap().is_ok());
        let e = it.next().unwrap().unwrap_err();
        assert!(
            matches!(e, KlvDecodeError::MalformedLength { offset: 4 }),
            "got {e:?}"
        );
    }

    #[test]
    fn malformed_tag_offset_is_body_absolute() {
        // item 1: tag 5, len 1, 'A' → 3 bytes; item 2: 6 continuation-set BER-OID
        // bytes with no terminator (read_ber_oid's u32 path rejects >5 bytes) —
        // same continuation-bit shape as `ber_oid_truncated_continuation` in
        // length.rs, just long enough to hit the length cap instead of running
        // out of buffer.
        let body = [0x05, 0x01, 0x41, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80];
        let mut it = Iter::local_set(&body);
        assert!(it.next().unwrap().is_ok());
        let e = it.next().unwrap().unwrap_err();
        assert!(
            matches!(e, KlvDecodeError::MalformedTag { offset: 3 }),
            "got {e:?}"
        );
    }
}
