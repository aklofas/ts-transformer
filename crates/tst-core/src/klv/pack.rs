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
/// BER/length-prefix substrate. Local-set form only (a BER-OID tag, or a
/// one-byte tag when `ONE_BYTE`, then a BER length); a future ST 0905
/// universal-set decoder would need a separate iterator.
pub(crate) struct Iter<'a, const ONE_BYTE: bool = false> {
    buf: &'a [u8],
    offset: usize,
    finished: bool,
}

impl<'a> Iter<'a> {
    /// Iterate a local-set body: BER-OID tag + BER length, repeating.
    /// Caller is responsible for stripping the outer UL + total length first.
    pub(crate) fn local_set(buf: &'a [u8]) -> Self {
        Self {
            buf,
            offset: 0,
            finished: false,
        }
    }
}

impl<'a> Iter<'a, true> {
    /// Iterate a local-set body whose label declares one-byte tags (SMPTE
    /// ST 336 Table 5, registry designator `0x03`: the Security Metadata LS
    /// and the VTarget Pack): every tag byte `0x00` to `0xFF` is the tag.
    pub(crate) fn local_set_one_byte(buf: &'a [u8]) -> Self {
        Self {
            buf,
            offset: 0,
            finished: false,
        }
    }
}

impl<'a, const ONE_BYTE: bool> Iterator for Iter<'a, ONE_BYTE> {
    type Item = Result<RawField<'a>, KlvDecodeError>;

    // One out-of-line step per item, with the item's reading inlined into
    // it: the instruction counts of the KLV decode workloads
    // (`benches/ir.rs`) are lowest with this split.
    #[inline(never)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.offset >= self.buf.len() {
            return None;
        }
        self.next_local_set()
    }
}

impl<'a, const ONE_BYTE: bool> Iter<'a, ONE_BYTE> {
    #[inline(always)]
    fn next_local_set(&mut self) -> Option<Result<RawField<'a>, KlvDecodeError>> {
        let start = self.offset;
        let rest = &self.buf[start..];
        let (tag, after_tag) = if ONE_BYTE {
            // `next` only calls here with at least one byte left.
            (u32::from(rest[0]), &rest[1..])
        } else {
            match read_ber_oid(rest) {
                Ok(v) => v,
                // A tag wider than 32 bits is unknown to every set, and
                // ST 0107.3-04 has a decoder skip an unknown item.
                Err(KlvDecodeError::LengthOverflow { .. }) => {
                    return self.next_after_wide_items(start);
                }
                Err(mut e) => {
                    crate::klv::length::rebase_offset(&mut e, start);
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        };
        self.read_value(start, tag, after_tag)
    }

    /// The rest of the item at `start` once its tag is read: the BER
    /// length and the value.
    #[inline(always)]
    fn read_value(
        &mut self,
        start: usize,
        tag: u32,
        after_tag: &'a [u8],
    ) -> Option<Result<RawField<'a>, KlvDecodeError>> {
        let consumed_tag = self.buf.len() - start - after_tag.len();
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

    /// Step over the wide-tag item at `start` and every one right after it,
    /// then read the item that follows them. Kept off the inner loop.
    #[cold]
    fn next_after_wide_items(
        &mut self,
        mut start: usize,
    ) -> Option<Result<RawField<'a>, KlvDecodeError>> {
        loop {
            if let Some(e) = self.skip_wide_item(start) {
                return Some(Err(e));
            }
            start = self.offset;
            if start >= self.buf.len() {
                return None;
            }
            match read_ber_oid(&self.buf[start..]) {
                Ok((tag, after_tag)) => return self.read_value(start, tag, after_tag),
                Err(KlvDecodeError::LengthOverflow { .. }) => {}
                Err(mut e) => {
                    crate::klv::length::rebase_offset(&mut e, start);
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        }
    }

    /// Step over the item at `start`, whose BER-OID tag is wider than 32
    /// bits: the item is dropped (nothing can carry its tag). Returns the
    /// framing error, buffer-relative, when the item runs past the buffer.
    #[cold]
    fn skip_wide_item(&mut self, start: usize) -> Option<KlvDecodeError> {
        use crate::klv::length::{rebase_offset, skip_ber_oid};
        let rest = &self.buf[start..];
        let framed = skip_ber_oid(rest).and_then(|after_tag| {
            let tag_len = rest.len() - after_tag.len();
            let (len, after_len) = read_ber(after_tag).map_err(|mut e| {
                rebase_offset(&mut e, tag_len);
                e
            })?;
            let header_len = rest.len() - after_len.len();
            if after_len.len() < len {
                return Err(KlvDecodeError::Truncated {
                    offset: header_len,
                    needed: len,
                    have: after_len.len(),
                });
            }
            Ok(header_len + len)
        });
        match framed {
            Ok(item_len) => {
                self.offset = start + item_len;
                None
            }
            Err(mut e) => {
                rebase_offset(&mut e, start);
                self.finished = true;
                Some(e)
            }
        }
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

    /// A tag of 2^32 (`90 80 80 80 00`) and an 11-byte tag: both are
    /// well-formed BER-OID values wider than the `u32` a tag is read into.
    const WIDE_TAGS: [&[u8]; 2] = [
        &[0x90, 0x80, 0x80, 0x80, 0x00],
        &[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F,
        ],
    ];

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// An ST 0601 packet: the UL, a short-form length, `items`, and a
    /// checksum over every byte before its value (ST 0601 §6.5).
    fn st0601_packet(items: &[&[u8]]) -> Vec<u8> {
        let ul = crate::klv::universal_label::UniversalLabel::ST_0601_LS.0;
        let mut body = items.concat();
        body.extend_from_slice(&[0x01, 0x02]);
        let mut out = ul.to_vec();
        out.push((body.len() + 2) as u8);
        out.extend_from_slice(&body);
        let sum = crate::klv::checksum::checksum_running_sum_16(&out);
        out.extend_from_slice(&sum.to_be_bytes());
        out
    }

    #[test]
    fn lenient_decoders_skip_an_unknown_tag_wider_than_32_bits() {
        // ST 0107.3-04: a decoder skips an unknown item; a tag that does
        // not fit 32 bits is unknown to every set and is dropped.
        for wide in WIDE_TAGS {
            let item = concat(&[wide, &[0x01, 0x42]]);

            let body = concat(&[&item, &[0x08, 0x01, 0x04]]);
            let fields: Vec<RawField> = Iter::local_set(&body).collect::<Result<_, _>>().unwrap();
            assert_eq!(
                fields,
                vec![RawField {
                    tag: 8,
                    value: &[0x04]
                }],
                "{wide:02X?}"
            );

            let rvt = crate::klv::st0806::decode(&body).unwrap();
            assert_eq!(rvt.rvt_ls_version, Some(4), "{wide:02X?}");

            let vmti = crate::klv::st0903::decode(&concat(&[&item, &[0x04, 0x01, 0x05]])).unwrap();
            assert_eq!(vmti.version_number, Some(5), "{wide:02X?}");
            assert!(vmti.field_errors.is_empty(), "{wide:02X?}: {vmti:?}");

            let packet = st0601_packet(&[
                &[0x02, 0x08, 0, 0, 0, 0, 0, 0, 0, 0x01],
                &item,
                &[0x41, 0x01, 0x13],
            ]);
            for (mode, decoded) in [
                ("decode", crate::klv::st0601::decode(&packet)),
                ("decode_strict", crate::klv::st0601::decode_strict(&packet)),
            ] {
                let r = decoded.unwrap_or_else(|e| panic!("{mode} {wide:02X?}: {e:?}"));
                assert_eq!(r.timestamp_us, Some(1), "{mode}");
                assert_eq!(r.uas_ls_version, Some(19), "{mode}");
                assert!(r.unknown.is_empty(), "{mode}: {:?}", r.unknown);
            }
        }
    }

    #[test]
    fn wide_tag_items_in_a_row_are_all_skipped() {
        // Three wide-tag items (each empty) and the items of both widths
        // after them: every wide one is stepped over, in one call.
        let wide_empty = concat(&[WIDE_TAGS[0], &[0x00]]);
        let wide_long = concat(&[WIDE_TAGS[1], &[0x01, 0x42]]);
        let body = concat(&[&wide_empty, &wide_long, &wide_empty, &[0x08, 0x01, 0x04]]);
        let mut it = Iter::local_set(&body);
        assert_eq!(
            it.next().unwrap().unwrap(),
            RawField {
                tag: 8,
                value: &[0x04]
            }
        );
        assert!(it.next().is_none());
        assert_eq!(
            crate::klv::st0806::decode(&body).unwrap().rvt_ls_version,
            Some(4)
        );
        // A set that ends in wide-tag items ends with them.
        let tail = concat(&[&[0x08, 0x01, 0x04], &wide_empty, &wide_long]);
        let fields: Vec<RawField> = Iter::local_set(&tail).collect::<Result<_, _>>().unwrap();
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn a_truncated_wide_tag_is_still_truncated() {
        for body in [
            &[0x90, 0x80][..],
            &[0x90, 0x80, 0x80, 0x80, 0x80],
            &[0x90, 0x80, 0x80, 0x80, 0x00, 0x05, 0x42],
        ] {
            let mut it = Iter::local_set(body);
            let err = it.next().unwrap().unwrap_err();
            assert!(
                matches!(err, KlvDecodeError::Truncated { .. }),
                "{body:02X?}: {err:?}"
            );
            assert!(it.next().is_none(), "{body:02X?}: finished after error");
        }
    }

    #[test]
    fn patch_copies_a_wide_unknown_tag_verbatim() {
        // Timestamp 1, the item `90 80 80 80 00 01 42`, version 19, checksum.
        let input = [
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x18, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x90,
            0x80, 0x80, 0x80, 0x00, 0x01, 0x42, 0x41, 0x01, 0x13, 0x01, 0x02, 0xBF, 0xA6,
        ];
        let out = crate::klv::st0601::patch(&input, &crate::klv::st0601::UasDatalinkLs::default())
            .unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn compliance_decode_still_refuses_a_wide_tag() {
        let item = concat(&[WIDE_TAGS[0], &[0x01, 0x42]]);
        let packet = st0601_packet(&[
            &[0x02, 0x08, 0, 0, 0, 0, 0, 0, 0, 0x01],
            &item,
            &[0x41, 0x01, 0x13],
        ]);
        let err = crate::klv::st0601::decode_strict_compliance(&packet).unwrap_err();
        assert!(
            matches!(
                err,
                KlvDecodeError::LengthOverflow {
                    value: 4_294_967_296
                }
            ),
            "{err:?}"
        );
        let err =
            crate::klv::st0903::decode_strict(&concat(&[&item, &[0x04, 0x01, 0x05]])).unwrap_err();
        assert!(
            matches!(
                err,
                KlvDecodeError::LengthOverflow {
                    value: 4_294_967_296
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn the_largest_32_bit_tag_is_still_kept() {
        let body = [0x8F, 0xFF, 0xFF, 0xFF, 0x7F, 0x01, 0x42, 0x08, 0x01, 0x04];
        let fields: Vec<RawField> = Iter::local_set(&body).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            fields,
            vec![
                RawField {
                    tag: u32::MAX,
                    value: &[0x42],
                },
                RawField {
                    tag: 8,
                    value: &[0x04],
                },
            ]
        );
        let packet = st0601_packet(&[&[0x02, 0x08, 0, 0, 0, 0, 0, 0, 0, 0x01], &body[..7]]);
        let r = crate::klv::st0601::decode(&packet).unwrap();
        assert_eq!(
            r.unknown,
            vec![OwnedRawField {
                tag: u32::MAX,
                value: vec![0x42],
            }]
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
