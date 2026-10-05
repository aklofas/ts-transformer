//! Property tests for KLV substrate: BER, BER-OID, IMAPB.
//!
//! These exercise the round-trip identity: encode a valid value, decode
//! the bytes, assert identity (within precision for IMAPB). Pure
//! property tests — they don't probe parser panic-freedom (that's the
//! fuzz targets' job) — they catch encode/decode asymmetries on the
//! happy path.

use crate::common::imapb_tol;
use proptest::prelude::*;
use tst_core::klv::imapb::{ImapbParams, decode_imapb, encode_imapb};
use tst_core::klv::length::{
    ber_len, ber_oid_len, read_ber, read_ber_oid, write_ber, write_ber_oid,
};

proptest! {
    /// BER length encoding: `write_ber(value)` -> `read_ber` is identity.
    /// Domain bounded at 2^31 — BER can encode larger but TS sections are
    /// 4 KiB capped; 2^31 is well over any real-world section length.
    #[test]
    fn ber_roundtrip(value in 0usize..(1usize << 31)) {
        let mut buf = vec![0u8; ber_len(value)];
        let written = write_ber(value, &mut buf).unwrap();
        prop_assert_eq!(written, buf.len());
        let (parsed, rest) = read_ber(&buf).unwrap();
        prop_assert_eq!(parsed, value);
        prop_assert!(rest.is_empty(), "read_ber should consume all bytes");
    }

    /// BER-OID length encoding: `write_ber_oid(value)` -> `read_ber_oid` is identity.
    /// Domain bounded at 2^28 — KLV tag values are u32 in practice.
    #[test]
    fn ber_oid_roundtrip(value in 0u32..(1u32 << 28)) {
        let mut buf = vec![0u8; ber_oid_len(value)];
        let written = write_ber_oid(value, &mut buf).unwrap();
        prop_assert_eq!(written, buf.len());
        let (parsed, rest) = read_ber_oid(&buf).unwrap();
        prop_assert_eq!(parsed, value);
        prop_assert!(rest.is_empty(), "read_ber_oid should consume all bytes");
    }

    /// IMAPB round-trip: encode a value in [min, max] with `length`-byte
    /// representation, decode the bytes, assert the result is within
    /// one IMAPB scale-factor of the original. Scale factor is the
    /// quantization step; rounding is to nearest grid point so worst-
    /// case error is scale_factor/2, but we allow full scale_factor
    /// tolerance for floating-point safety margin.
    ///
    /// `length` is sampled across 1..=8 so the domain exercises the
    /// L=8 cap; `encode_imapb` returns `UnsupportedImapbLength` for
    /// L=8 and `prop_assume!` skips those samples. The round-trip
    /// property only applies when encode succeeds.
    #[test]
    fn imapb_roundtrip(
        // Bounded but realistic IMAPB params. ST 0601 lat/lon use
        // length=4; altitude uses length=2; pitch/yaw use length=2/4.
        // Sample length=8 too so the cap is exercised (skipped via prop_assume).
        length in 1usize..=8usize,
        min in -180.0f64..0.0,
        max in 0.0f64..180.0,
        // `t` is a [0,1] interpolation factor; we lerp into [min, max].
        // Generating `value` directly in [-180, 180] would force a
        // prop_assume rejection any time `value` fell outside [min, max],
        // which dominates the global-rejects budget at higher case counts.
        t in 0.0f64..=1.0,
    ) {
        prop_assume!(min < max);
        let value = min + t * (max - min);
        let params = ImapbParams { min, max, length };
        let mut buf = vec![0u8; length];
        // encode_imapb errors on length >= 8 (UnsupportedImapbLength) —
        // prop_assume skips those samples; the round-trip property
        // only applies when encode succeeds.
        prop_assume!(encode_imapb(&params, value, &mut buf).is_ok());
        // decode_imapb returns DecodedImapb (ST 1201.5 §7.2.2/.3
        // special values + bounds check). The round-trip property only
        // applies when encode produced normal-range output, so chain
        // `.value()` to extract the f64; if the legitimate encoded
        // integer arithmetic-decodes to a top-2-bits-set pattern
        // (impossible by construction here since encode rejects values
        // outside [min, max]), the prop_assume above already filtered.
        let decoded = decode_imapb(&params, &buf).unwrap().value().unwrap();
        // Tolerance is the max of two sources of round-trip error:
        // (1) IMAPB quantization step `scale = 2^ceil(log2(span)) / 2^(8L-1)`.
        //     Encode rounds to nearest grid point so the integer-rounding
        //     error is at most `scale/2`; we allow full `scale` for safety.
        // (2) f64 ULP propagated through the encode/decode arithmetic. At
        //     L=6/7 with degree-scale spans, `scale` drops below f64's
        //     relative precision and representation error dominates. The
        //     intermediate `(value - min)` and `sf * (i + offset) + min`
        //     terms have magnitudes near `span` and `max(|min|, |max|)`,
        //     so the bound is f64::EPSILON * max(span, |min|, |max|) with
        //     a small safety factor. Without this term the property
        //     spuriously fails at L=7 (caught: `decoded - value` ~1.1e-14
        //     versus a `scale` of ~7.1e-15 when value is small but span
        //     is degree-scale).
        //
        // Formula extracted to `tests/common::imapb_tol` so the typed-
        // set proptests share the same derivation.
        let bound = imapb_tol(min, max, length);
        prop_assert!(
            (decoded - value).abs() <= bound.tol,
            "IMAPB round-trip: decoded {} too far from input {} (tol={}, scale={}, fp_eps={}, length={}, min={}, max={})",
            decoded, value, bound.tol, bound.scale, bound.fp_eps, length, min, max
        );
    }
}
