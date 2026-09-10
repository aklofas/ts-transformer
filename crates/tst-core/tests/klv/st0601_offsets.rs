//! Regression tests: enveloped decoders (ST 0601 `decode`/`decode_strict_compliance`,
//! ST 0806 `decode_standalone`) rebase every iterator error to the offset
//! of the malformed byte within the *whole buffer* (UL + outer BER
//! length + body), not just within the local-set body. Report repro:
//! 16-byte ST 0601 UL + `05 05 01 41 06 80` — strict reported
//! `MalformedLength { offset: 21 }`, permissive reported `offset: 0`.

use tst_core::error::KlvDecodeError;
use tst_core::klv::st0601;
use tst_core::klv::st0806;
use tst_core::klv::universal_label::UniversalLabel;

/// Wrap a local-set `body` in a 16-byte UL + BER short-form length,
/// matching the enveloped wire form both `st0601::decode` and
/// `st0601::decode_strict_compliance` accept.
fn ul_plus(ul: UniversalLabel, body: &[u8]) -> Vec<u8> {
    let mut v = ul.0.to_vec();
    assert!(
        body.len() < 0x80,
        "test body must fit BER short-form length"
    );
    v.push(body.len() as u8); // BER short-form length
    v.extend_from_slice(body);
    v
}

fn st0601_plus(body: &[u8]) -> Vec<u8> {
    ul_plus(UniversalLabel::ST_0601_LS, body)
}

#[test]
fn permissive_and_strict_report_the_same_absolute_offset() {
    // Field 1: tag=5, len=1, value=[0x41] (3 bytes, body offset 0..3).
    // Field 2: tag=6, then a length byte 0x80 — long-form flag with a
    // zero length-of-length ("indefinite length"), rejected as
    // MalformedLength. That length byte sits at body offset 4, i.e.
    // absolute offset 17 (UL 16 + length byte 1) + 4 = 21.
    let buf = st0601_plus(&[0x05, 0x01, 0x41, 0x06, 0x80]);
    let p = st0601::decode(&buf).unwrap_err();
    let s = st0601::decode_strict_compliance(&buf).unwrap_err();
    assert!(
        matches!(s, KlvDecodeError::MalformedLength { offset: 21 }),
        "strict {s:?}"
    );
    assert!(
        matches!(p, KlvDecodeError::MalformedLength { offset: 21 }),
        "permissive {p:?}"
    );
}

#[test]
fn permissive_reports_absolute_offset_for_malformed_tag() {
    // Field 1: tag=5, len=1, value=[0x41] (3 bytes, body offset 0..3).
    // Field 2 starts at body offset 3 with six BER-OID continuation
    // bytes (0x80 with the high bit set) — more than the 5-byte cap for
    // a u32 tag, rejected as MalformedTag at the tag's own start.
    // Absolute offset = 17 (UL + length byte) + 3 = 20.
    let buf = st0601_plus(&[0x05, 0x01, 0x41, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80]);
    let p = st0601::decode(&buf).unwrap_err();
    assert!(
        matches!(p, KlvDecodeError::MalformedTag { offset: 20 }),
        "permissive {p:?}"
    );
}

#[test]
fn permissive_reports_absolute_offset_for_truncated_value() {
    // Field 1: tag=5, len=1, value=[0x41] (3 bytes, body offset 0..3).
    // Field 2: tag=6, declared len=5, but only 4 value bytes follow.
    // The length field ends at body offset 5, so
    // absolute offset = 17 (UL + length byte) + 5 = 22.
    let buf = st0601_plus(&[0x05, 0x01, 0x41, 0x06, 0x05, 0xAA, 0xBB, 0xCC, 0xDD]);
    let p = st0601::decode(&buf).unwrap_err();
    assert!(
        matches!(
            p,
            KlvDecodeError::Truncated {
                offset: 22,
                needed: 5,
                have: 4
            }
        ),
        "permissive {p:?}"
    );
}

#[test]
fn tag1_length_mismatch_reports_absolute_value_offset() {
    // Tag 1 (checksum) must carry exactly 2 value bytes. Here it declares
    // len=1, so the mismatch is reported against the value's own start:
    // body offset 2 (tag byte, length byte, then value), i.e.
    // absolute offset = 17 (UL + outer length byte) + 2 = 19.
    // Checked on both the verifying and the permissive entry points —
    // the Tag 1 arm runs before any checksum verification.
    let buf = st0601_plus(&[0x01, 0x01, 0xAA]);
    for (name, e) in [
        ("decode", st0601::decode(&buf).unwrap_err()),
        (
            "decode_unchecked",
            st0601::decode_unchecked(&buf).unwrap_err(),
        ),
    ] {
        assert!(
            matches!(
                e,
                KlvDecodeError::Truncated {
                    offset: 19,
                    needed: 2,
                    have: 1
                }
            ),
            "{name} {e:?}"
        );
    }
}

#[test]
fn st0806_decode_standalone_reports_absolute_offset() {
    // Same MalformedLength shape as the ST 0601 repro, wrapped in the
    // RVT LS's own UL instead: field 1 tag=2 len=1 value=[0x41] (3
    // bytes), field 2 tag=3 then a 0x80 "indefinite length" byte at
    // body offset 4. Absolute offset = 17 (UL + length byte) + 4 = 21.
    let buf = ul_plus(st0806::RVT_LS_UL, &[0x02, 0x01, 0x41, 0x03, 0x80]);
    let p = st0806::decode_standalone(&buf).unwrap_err();
    assert!(
        matches!(p, KlvDecodeError::MalformedLength { offset: 21 }),
        "permissive {p:?}"
    );
}
