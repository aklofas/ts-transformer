//! Fuzz target — ST 1201.5 IMAPB panic-freedom + round-trip for:
//!   * `encode_imapb` / `decode_imapb` (Value case within one quantisation step)
//!   * `encode_imapb_special` / `decode_imapb` (Special case exact round-trip)
//!
//! Covers the special-value path and lengths L ∈ {1, 4..8}, which the
//! ST 0903 fuzz coverage never reaches (it hits IMAPB only indirectly via
//! fixed-L table entries).
//!
//! # Input layout
//!
//! ```text
//! [0]      length selector → length = (byte % 8) + 1  ∈ 1..=8
//! [1..9]   min (little-endian f64 bits)
//! [9..17]  max (little-endian f64 bits)
//! [17..]   payload bytes
//! ```
//!
//! The driver restricts its input domain to finite `min < max`: this is the
//! fuzz target's own constraint to keep round-trip tolerance meaningful, not a
//! mirror of the library precondition. The library rejects NaN params (where
//! `partial_cmp` returns `None`) and `min >= max`, but accepts ±∞ params
//! (e.g. `−∞ < +∞` satisfies `partial_cmp`). The driver also skips the
//! round-trip probes when `sF = 2^(dPow − bPow)` overflows to `+∞` (tiny
//! span), since in that case no round-trip tolerance can be stated.

#![no_main]
use libfuzzer_sys::fuzz_target;
use tst_core::klv::imapb::{
    DecodedImapb, ImapbParams, ImapbSpecial, decode_imapb, encode_imapb, encode_imapb_special,
};

fuzz_target!(|data: &[u8]| {
    if data.len() < 18 {
        return;
    }

    let length = (data[0] % 8 + 1) as usize;
    let min = f64::from_bits(u64::from_le_bytes(data[1..9].try_into().unwrap()));
    let max = f64::from_bits(u64::from_le_bytes(data[9..17].try_into().unwrap()));

    // ST 1201.5 §6 precondition: min < max, both finite.
    if !min.is_finite() || !max.is_finite() || min >= max {
        return;
    }

    let p = ImapbParams { min, max, length };
    let payload = &data[17..];

    // == Part 1: decode_imapb panic-freedom ==
    //
    // Feed arbitrary bytes of exactly `length` bytes. Returns Ok(_) or
    // Err(InvalidLength/InvalidImapbParams) — must never panic regardless of
    // byte pattern (special-value space, out-of-range integers, all variants).
    if payload.len() >= length {
        let _ = decode_imapb(&p, &payload[..length]);
    }

    // Compute scale factor for the round-trip probes. Bail if sF is
    // non-finite or zero: a tiny span overflows sF to +∞ (the primary
    // case); the s_f <= 0.0 arm is a safety net for extreme-magnitude
    // spans where max − min itself overflows f64 arithmetic.
    // NOTE: keep this bPow/sF derivation in sync with `ImapbParams::sf()`
    // in imapb.rs; a drift here surfaces as a loud false-positive crash,
    // not a silent gap.
    let span = max - min;
    let b_pow = span.log2().ceil();
    let d_pow = (8 * length as i32 - 1) as f64;
    let s_f = 2f64.powf(d_pow - b_pow);
    if !s_f.is_finite() || s_f <= 0.0 {
        return;
    }
    // sR = 1/sF = one quantisation step (max round-trip error from floor()).
    let s_r = 1.0 / s_f;

    // == Part 2: encode_imapb → decode_imapb round-trip (Value case) ==
    //
    // Derive a finite in-range value from the first 8 payload bytes. If it
    // encodes successfully, decode must return Value(v) where
    // |v - original| ≤ sR + fp_eps (one quantisation step plus float noise).
    //
    // Property from ST 1201.5 §7.2.1 encode + §7.2.2 decode formulas:
    //   y = floor(sF·(value−min) + Zoffset)  so  value' = sR·(y−Zoffset)+min
    //   ⟹  |value' − value| < sR  (the floor introduces ≤ 1 quant error).
    if payload.len() >= 8 {
        let v_raw = f64::from_bits(u64::from_le_bytes(payload[..8].try_into().unwrap()));
        if v_raw.is_finite() && v_raw >= min && v_raw <= max {
            let mut buf = vec![0u8; length];
            if encode_imapb(&p, v_raw, &mut buf).is_ok() {
                let decoded =
                    decode_imapb(&p, &buf).expect("decode after successful encode must succeed");
                match decoded {
                    DecodedImapb::Value(v_back) => {
                        // Value must be finite — a non-finite Value violates
                        // the decode contract.
                        assert!(
                            v_back.is_finite(),
                            "decode returned Value({v_back}) which is non-finite; \
                             min={min}, max={max}, L={length}"
                        );
                        let fp_eps = span.abs() * f64::EPSILON * 8.0;
                        let tolerance = s_r + fp_eps;
                        assert!(
                            (v_back - v_raw).abs() <= tolerance,
                            "Value round-trip exceeded one quantisation step: \
                             original={v_raw}, decoded={v_back}, tolerance={tolerance}, \
                             min={min}, max={max}, L={length}"
                        );
                    }
                    // OutOfRange with a non-finite decoded value: this is the
                    // correct outcome when the decode formula overflows due to
                    // an enormous sR (degenerate range). The encode
                    // succeeded for v_raw ∈ [min, max], but the round-trip
                    // can't be guaranteed when sR is large enough that
                    // sR·Zoffset + |min| exceeds f64::MAX.
                    DecodedImapb::OutOfRange { decoded } if !decoded.is_finite() => {}
                    other => panic!(
                        "encode(value ∈ [min,max]) then decode must return Value or \
                         OutOfRange(non-finite), got {other:?}; \
                         min={min}, max={max}, L={length}"
                    ),
                }
            }
        }
    }

    // == Part 3: encode_imapb_special → decode_imapb round-trip (Special case) ==
    //
    // Construct an ImapbSpecial from the payload bytes. The NaN / user-defined
    // payload is masked to the available `8L−5` bits so encode always succeeds.
    // After a successful encode the decode must return Special(original) exactly
    // — the special-value patterns are lossless per ST 1201.5 §7.2.3.
    if payload.len() >= 9 {
        let special_sel = payload[8] % 9;
        let raw_sig = if payload.len() >= 17 {
            u64::from_le_bytes(payload[9..17].try_into().unwrap())
        } else {
            0u64
        };
        // Mask payload to (8L-5) bits so encode_imapb_special never Errs on size.
        let payload_bits = 8 * length - 5;
        let sig = if payload_bits >= 64 {
            raw_sig
        } else {
            raw_sig & ((1u64 << payload_bits) - 1)
        };
        let special = match special_sel {
            0 => ImapbSpecial::PositiveInfinity,
            1 => ImapbSpecial::NegativeInfinity,
            2 => ImapbSpecial::BelowMin,
            3 => ImapbSpecial::AboveMax,
            4 => ImapbSpecial::PositiveQuietNaN { nan_id: sig },
            5 => ImapbSpecial::NegativeQuietNaN { nan_id: sig },
            6 => ImapbSpecial::PositiveSignalingNaN { signal: sig },
            7 => ImapbSpecial::NegativeSignalingNaN { signal: sig },
            _ => ImapbSpecial::UserDefined { signal: sig },
        };
        let mut buf = vec![0u8; length];
        if encode_imapb_special(special, length, &mut buf).is_ok() {
            let decoded = decode_imapb(&p, &buf)
                .expect("decode after successful encode_imapb_special must succeed");
            assert_eq!(
                decoded,
                DecodedImapb::Special(special),
                "Special round-trip failed for {special:?} at L={length}"
            );
        }
    }
});
