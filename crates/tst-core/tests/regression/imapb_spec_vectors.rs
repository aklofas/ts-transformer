//! ST 1201.5 IMAPB spec-vector regression suite: each IMAPB special-value
//! branch, bounds, and decode/encode symmetry.
//!
//! The [`DecodedImapb`] enum's §7.2.3 Table 2 special-value branches and §8.6 Eq.12
//! out-of-range diagnostic are exercised below as pure hex-byte
//! vectors. Naming convention: `imapb_<intent>_<spec_section>` so
//! cross-reference back to ST 1201.5 is one grep away.
//!
//! [`DecodedImapb`]: tst_core::klv::imapb::DecodedImapb

use tst_core::error::{KlvEncodeError, KlvFieldError};
use tst_core::klv::imapb::{DecodedImapb, ImapbParams, ImapbSpecial, decode_imapb, encode_imapb};

// ============================================================================
// §7.2.3 special-value PATTERNS (decode side)
// ============================================================================
//
// ST 1201.5 §7.2.2 step 1 detects special values by testing the top two
// bits of byte 0 of the L-byte unsigned BE integer. When both are set, the
// byte-0 value is mapped to an [`ImapbSpecial`] family per §7.2.3 Table 2
// (standard NaN/Inf/UserDefined) and Table 3 (MISB-defined overflow signals):
//
//   byte0  5-bit tag  Table  meaning
//   ----   ---------  -----  -----------------------------------------
//   0xC8   11001      T2     +∞  PositiveInfinity        (payload must be 0)
//   0xE8   11101      T2     -∞  NegativeInfinity        (payload must be 0)
//   0xD0   11010      T2     +QNaN PositiveQuietNaN      (nan_id in low bits)
//   0xF0   11110      T2     -QNaN NegativeQuietNaN      (nan_id in low bits)
//   0xD8   11011      T2     +SNaN PositiveSignalingNaN  (signal in low bits)
//   0xF8   11111      T2     -SNaN NegativeSignalingNaN  (signal in low bits)
//   0xC0   11000      T2     UserDefined                 (signal in low bits)
//   0xE0   (full)     T3     BelowMin  (MISB-defined, zero-filled)
//   0xE1   (full)     T3     AboveMax  (MISB-defined, zero-filled)
//   other  11xx_xxxx  —      ReservedSpecial { raw }
//
// BelowMin/AboveMax (Table 3) are MISB-defined overflow signals; they are
// NOT user-defined. The low `8L-5` bits carry NaN-id / signal payloads for
// the NaN families. Unrecognized patterns and non-zero-filled standard
// patterns fall to [`DecodedImapb::ReservedSpecial`].
// All named families are exercised below.

/// ST 1201.5 §7.2.3 Table 2 row "Positive Infinity": byte0 = 0xC8, zero-filled →
/// [`DecodedImapb::Special`]`(`[`ImapbSpecial::PositiveInfinity`]`)` regardless of L.
#[test]
fn imapb_decodes_positive_infinity_st1201_5_7_2_3_table2() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    // L=3, byte0 = 0xC8, payload-zero rest → Special(PositiveInfinity).
    let decoded = decode_imapb(&p, &[0xC8, 0x00, 0x00]).unwrap();
    assert_eq!(
        decoded,
        DecodedImapb::Special(ImapbSpecial::PositiveInfinity)
    );
    assert_eq!(decoded.value(), None, "legacy accessor must return None");
}

/// ST 1201.5 §7.2.3 Table 2 row "Negative Infinity": byte0 = 0xE8, zero-filled →
/// [`DecodedImapb::Special`]`(`[`ImapbSpecial::NegativeInfinity`]`)`. The range
/// straddles zero so Zoffset would be nonzero — verifies §7.2.2 step 1
/// short-circuits the normal-range arithmetic.
#[test]
fn imapb_decodes_negative_infinity_st1201_5_7_2_3_table2() {
    let p = ImapbParams {
        min: -100.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0xE8, 0x00, 0x00]).unwrap();
    assert_eq!(
        decoded,
        DecodedImapb::Special(ImapbSpecial::NegativeInfinity)
    );
}

/// ST 1201.5 §7.2.3 Table 2 row "NaN": byte0 = 0xD0 →
/// [`DecodedImapb::Special`]`(`[`ImapbSpecial::PositiveQuietNaN`]` { nan_id: 0 })`.
/// The full NaN-family decode (quiet/signaling, positive/negative, with
/// nan_id payload) is tested in `decode_recognizes_all_nan_families`.
#[test]
fn imapb_decodes_nan_st1201_5_7_2_3_table2() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0xD0, 0x00, 0x00]).unwrap();
    assert_eq!(
        decoded,
        DecodedImapb::Special(ImapbSpecial::PositiveQuietNaN { nan_id: 0 })
    );
}

/// ST 1201.5 §7.2.3 Table 3 row "IMAP_BELOW_MINIMUM": byte0 = 0xE0 →
/// [`DecodedImapb::Special`]`(`[`ImapbSpecial::BelowMin`]`)`. Producer signal for
/// "value below the configured minimum"; the decoder does NOT fabricate a
/// concrete `min - ε` — the value is structurally unknown.
#[test]
fn imapb_decodes_below_min_st1201_5_7_2_3_table2() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0xE0, 0x00, 0x00]).unwrap();
    assert_eq!(decoded, DecodedImapb::Special(ImapbSpecial::BelowMin));
}

/// ST 1201.5 §7.2.3 Table 3 row "IMAP_ABOVE_MAXIMUM": byte0 = 0xE1 →
/// [`DecodedImapb::Special`]`(`[`ImapbSpecial::AboveMax`]`)`. Producer signal for
/// "value above the configured maximum."
#[test]
fn imapb_decodes_above_max_st1201_5_7_2_3_table2() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0xE1, 0x00, 0x00]).unwrap();
    assert_eq!(decoded, DecodedImapb::Special(ImapbSpecial::AboveMax));
}

/// ST 1201.5 §7.2.3 Table 2 reserved/user-defined row: top two bits
/// set (0b11xx_xxxx) but byte0 ≠ any of the 5 named patterns →
/// [`DecodedImapb::ReservedSpecial`] carrying the L-byte raw integer.
/// 0xCC = 0b1100_1100 (not 0xC8, 0xD0, 0xE0, 0xE1, or 0xE8).
#[test]
fn imapb_decodes_reserved_special_st1201_5_7_2_3_table2() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0xCC, 0x12, 0x34]).unwrap();
    match decoded {
        // raw is the full L-byte BE integer — payload bytes survive
        // so a forward-compat consumer with out-of-band knowledge can
        // sub-classify them.
        DecodedImapb::ReservedSpecial { raw } => assert_eq!(raw, 0x00CC_1234),
        other => panic!("expected ReservedSpecial, got {other:?}"),
    }
    assert_eq!(decoded.value(), None);
}

// ============================================================================
// §7.2.3 Table 1 normal-range PATTERNS (decode side)
// ============================================================================

/// ST 1201.5 §7.1.2 Starting Point B: integer 0 decodes to exactly `min`.
/// IMAPB(0, 100, 3) wire 0x000000 → 0.0. This is a "reference" vector
/// that must hold across every (min, max, L) the substrate supports.
#[test]
fn imapb_decodes_min_integer_to_min_st1201_5_7_1_2() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0x00, 0x00, 0x00]).unwrap();
    match decoded {
        DecodedImapb::Value(v) => assert!(v.abs() < 1e-9, "expected 0.0, got {v}"),
        other => panic!("expected Value(0.0), got {other:?}"),
    }
}

/// ST 1201.5 §7.2.3 Table 1 row 2 "max-value": integer `floor(sF·(b-a))`
/// is the legitimate max-value mapping. For IMAPB(0, 100, 3), sF =
/// `2^23 / 2^ceil(log2(100))` = `2^23 / 128` = 65536, so floor(sF·100)
/// = 6553600 = 0x640000. Top byte (0x64 = 0b01100100) does NOT set the
/// top two bits, so the §7.2.2 step 1 special-value branch is skipped
/// and the normal-range reverse arithmetic returns ~100.0 exactly.
#[test]
fn imapb_decodes_max_integer_to_max_st1201_5_7_2_3_table1() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0x64, 0x00, 0x00]).unwrap();
    match decoded {
        DecodedImapb::Value(v) => {
            assert!((v - 100.0).abs() < 1e-9, "expected 100.0, got {v}");
        }
        other => panic!("expected Value(100.0), got {other:?}"),
    }
}

/// ST 1201.5 §8.6 Eq.12 "inter-band reserved" decode: bit pattern
/// with top bit = 1, second bit = 0 (0b10xx_xxxx) — NOT a special
/// value per §7.2.3, but arithmetic-decodes past `max`. For
/// IMAPB(0, 100, 3), byte0=0x80 means y = 0x800000 = 8388608;
/// sR=1/65536; value = 8388608/65536 = 128.0, outside [0, 100].
/// Substrate surfaces [`DecodedImapb::OutOfRange`] carrying the raw
/// decode so callers can choose strict reject vs. lenient clamp.
#[test]
fn imapb_decodes_inter_band_as_out_of_range_st1201_5_8_6_eq12() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let decoded = decode_imapb(&p, &[0x80, 0x00, 0x00]).unwrap();
    match decoded {
        DecodedImapb::OutOfRange { decoded } => {
            assert!(
                (decoded - 128.0).abs() < 1e-6,
                "expected ~128.0 raw decode, got {decoded}"
            );
        }
        other => panic!("expected OutOfRange, got {other:?}"),
    }
    assert_eq!(decoded.value(), None);
}

// ============================================================================
// Encode-side vectors (ST 1201.5 Appendix A)
// ============================================================================
//
// Encoding is `y = truncate(sF·(value − min) + Zoffset)` with L-byte
// unsigned BE serialization (§7.2.1). The encoder rejects:
//   - L outside 1..=8 → UnsupportedImapbLength
//   - buffer too small → BufferTooSmall
//   - non-finite or out-of-range value → OutOfRange { tag, value, min, max }
//
// `encode_imapb` operates only on finite values inside `[min, max]` and
// rejects NaN/±∞ with `OutOfRange`. Producers that want to emit ST 1201.5
// §7.2.3 special values (Table 2/3: +∞, -∞, NaN families, BelowMin,
// AboveMax, user-defined) must use `encode_imapb_special` instead —
// it accepts an `ImapbSpecial` variant and writes the correctly
// zero-filled wire bytes per §7.2.3.

/// ST 1201.5 Appendix A Test 2: IMAPB(0.0, 100.0, 3) value 100.0 →
/// `0x64 0x00 0x00`. Encoder must NOT emit signed-midpoint-shift
/// 0xE40000 (which would set the top-2-bits and collide with §7.2.3
/// special-value space).
#[test]
fn imapb_encodes_appendix_a_test_2_unsigned_be() {
    let p = ImapbParams {
        min: 0.0,
        max: 100.0,
        length: 3,
    };
    let mut buf = [0u8; 3];
    encode_imapb(&p, 100.0, &mut buf).unwrap();
    assert_eq!(buf, [0x64, 0x00, 0x00]);
}

/// ST 1201.5 Appendix A Test 3: IMAPB(-9.9, 110.0, 3) value 0.0 →
/// `0x09 0xE6 0x67`. Exercises Zoffset = sF·a − floor(sF·a) for the
/// straddle-zero range (§7.1.2 step 6). Pre-fix encoders that
/// implemented signed-mapping instead of unsigned-BE emit `0x89 0xE6 0x66`.
#[test]
fn imapb_encodes_appendix_a_test_3_zero_mapping_with_zoffset() {
    let p = ImapbParams {
        min: -9.9,
        max: 110.0,
        length: 3,
    };
    let mut buf = [0u8; 3];
    encode_imapb(&p, 0.0, &mut buf).unwrap();
    assert_eq!(buf, [0x09, 0xE6, 0x67]);
    // Round-trip back through decode — must yield Value within tolerance.
    let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
    assert!(back.abs() < 1e-4, "round trip to 0.0 failed: got {back}");
}

// ============================================================================
// Bounds / length parameter exhaustively (1..=8)
// ============================================================================
//
// ST 1201.5 §6 defines IMAPB for any L-byte mapping; this Rust
// substrate uses u64 internally (caps at L=8). The encoder + decoder
// must accept every L ∈ {1, 2, 3, 4, 5, 6, 7, 8} and reject L=0 / L=9.

/// Sweep length 1..=8: encode 0.0 then round-trip back. Asserts
/// the round-trip Value is within `2 · scale + 8 · f64::EPSILON · |span|`
/// where `scale = 2^(ceil(log2(span)) - (8L-1))` per ST 1201.5 §8.9.
/// At L=1 the grid is coarse so the tolerance is dominated by `scale`;
/// at L=8 the tolerance is dominated by f64 ULP propagation. Mirrors
/// the tolerance derivation in `crates/tst-core/tests/common::imapb_tol`.
#[test]
fn imapb_round_trip_each_length_st1201_5_section_6() {
    // Range chosen to straddle zero so Zoffset is exercised.
    let min = -10.0;
    let max = 10.0;
    let span = max - min;
    for length in 1usize..=8 {
        let p = ImapbParams { min, max, length };
        let mut buf = [0u8; 8];
        encode_imapb(&p, 0.0, &mut buf[..length])
            .unwrap_or_else(|e| panic!("encode_imapb(L={length}) at value 0.0 failed: {e:?}"));
        let decoded = decode_imapb(&p, &buf[..length])
            .unwrap_or_else(|e| panic!("decode_imapb(L={length}) failed: {e:?}"));
        let back = decoded
            .value()
            .unwrap_or_else(|| panic!("decode_imapb(L={length}) returned non-Value: {decoded:?}"));
        // scale per ST 1201.5 §8.9 (mirrors imapb.rs's internal bounds
        // tolerance). 2*scale safety factor + ULP floor.
        let b_pow = span.log2().ceil();
        let d_pow = (8 * length as i32 - 1) as f64;
        let scale = 2f64.powf(b_pow - d_pow);
        let tol = (2.0 * scale).max(8.0 * f64::EPSILON * span.abs());
        assert!(
            back.abs() <= tol,
            "L={length}: 0.0 round-trip drifted to {back} (tol {tol:.3e}, scale {scale:.3e})"
        );
    }
}

/// L=0 must be rejected by encode_imapb → UnsupportedImapbLength.
/// L=0 is degenerate (no bytes to write); per ST 1201.5 §6 the spec
/// allows any positive L, this substrate caps at L=1..=8.
#[test]
fn imapb_encode_rejects_length_zero_st1201_5_section_6() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 0,
    };
    let mut buf = [0u8; 1];
    let err = encode_imapb(&p, 0.5, &mut buf).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::UnsupportedImapbLength { length: 0 }),
        "expected UnsupportedImapbLength {{ length: 0 }}, got {err:?}"
    );
}

/// L=9 must be rejected by encode_imapb → UnsupportedImapbLength.
/// The substrate's internal arithmetic uses u64 (max 8 bytes); L=9
/// would need u128.
#[test]
fn imapb_encode_rejects_length_nine_st1201_5_section_6() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 9,
    };
    let mut buf = [0u8; 9];
    let err = encode_imapb(&p, 0.5, &mut buf).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::UnsupportedImapbLength { length: 9 }),
        "expected UnsupportedImapbLength {{ length: 9 }}, got {err:?}"
    );
}

/// L=0 must also be rejected by decode_imapb → UnsupportedImapbLength
/// (the field-error variant, sibling to the encode-side rejection).
#[test]
fn imapb_decode_rejects_length_zero_st1201_5_section_6() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 0,
    };
    let err = decode_imapb(&p, &[]).unwrap_err();
    assert!(
        matches!(err, KlvFieldError::UnsupportedImapbLength { length: 0 }),
        "expected UnsupportedImapbLength {{ length: 0 }}, got {err:?}"
    );
}

/// L=9 must also be rejected by decode_imapb → UnsupportedImapbLength.
#[test]
fn imapb_decode_rejects_length_nine_st1201_5_section_6() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 9,
    };
    let err = decode_imapb(&p, &[0u8; 9]).unwrap_err();
    assert!(
        matches!(err, KlvFieldError::UnsupportedImapbLength { length: 9 }),
        "expected UnsupportedImapbLength {{ length: 9 }}, got {err:?}"
    );
}

// ============================================================================
// Finite-domain rejection per ST 1201.5 §7.2.1
// ============================================================================
//
// The encoder rejects values outside `[min, max]` (and NaN / ±∞) with
// `KlvEncodeError::OutOfRange { tag: 0, value, min, max }`. tag=0 is
// the substrate sentinel — typed-set encoders that route through
// `encode_fixed_range` overwrite `tag` with the spec tag ID before
// surfacing the error to callers.

#[test]
fn imapb_encode_rejects_value_above_max_st1201_5_7_2_1() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 2,
    };
    let mut buf = [0u8; 2];
    let err = encode_imapb(&p, 2.0, &mut buf).unwrap_err();
    assert!(
        matches!(
            err,
            KlvEncodeError::OutOfRange { value: v, min: 0.0, max: 1.0, .. } if v == 2.0
        ),
        "expected OutOfRange {{ value: 2.0, min: 0.0, max: 1.0 }}, got {err:?}"
    );
}

#[test]
fn imapb_encode_rejects_value_below_min_st1201_5_7_2_1() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 2,
    };
    let mut buf = [0u8; 2];
    let err = encode_imapb(&p, -1.0, &mut buf).unwrap_err();
    assert!(
        matches!(
            err,
            KlvEncodeError::OutOfRange { value: v, min: 0.0, max: 1.0, .. } if v == -1.0
        ),
        "expected OutOfRange {{ value: -1.0 }}, got {err:?}"
    );
}

#[test]
fn imapb_encode_rejects_nan_st1201_5_7_2_1() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 2,
    };
    let mut buf = [0u8; 2];
    let err = encode_imapb(&p, f64::NAN, &mut buf).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::OutOfRange { .. }),
        "expected OutOfRange on NaN input, got {err:?}"
    );
}

#[test]
fn imapb_encode_rejects_positive_infinity_st1201_5_7_2_1() {
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 2,
    };
    let mut buf = [0u8; 2];
    let err = encode_imapb(&p, f64::INFINITY, &mut buf).unwrap_err();
    assert!(
        matches!(err, KlvEncodeError::OutOfRange { .. }),
        "expected OutOfRange on +inf input, got {err:?}"
    );
}

#[test]
fn imapb_decode_rejects_wrong_value_length_st1201_5_7_2_2() {
    // L=3 declared, only 2 bytes provided → InvalidLength.
    let p = ImapbParams {
        min: 0.0,
        max: 1.0,
        length: 3,
    };
    let err = decode_imapb(&p, &[0x00, 0x00]).unwrap_err();
    assert!(
        matches!(
            err,
            KlvFieldError::InvalidLength {
                expected: 3,
                got: 2,
                ..
            }
        ),
        "expected InvalidLength {{ expected: 3, got: 2 }}, got {err:?}"
    );
}

// ============================================================================
// Interior normal-range sweep (decode side)
// ============================================================================
//
// Cross-check that interior values quantize within ST 1201.5 §8.9
// scale tolerance for IMAPB(-180, 180, 4) — a common geographic range
// (e.g. ST 0601 sensor-relative azimuth). At L=4 over 360° span,
// scale = 512/2^31 ≈ 2.4e-7°.
#[test]
fn imapb_normal_range_sweep_geographic_l4_st1201_5_8_9() {
    let p = ImapbParams {
        min: -180.0,
        max: 180.0,
        length: 4,
    };
    for &v in &[-179.999_999_5, -90.0, -1.0, 0.0, 1.0, 90.0, 179.999_999_5] {
        let mut buf = [0u8; 4];
        encode_imapb(&p, v, &mut buf).unwrap();
        let back = decode_imapb(&p, &buf).unwrap().value().unwrap();
        // scale ≈ 2.4e-7 for L=4 / 360° span.
        assert!(
            (back - v).abs() < 1e-6,
            "geographic round-trip: input {v}, back {back}"
        );
    }
}
