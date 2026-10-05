//! Fuzz target — decode panic-freedom + decode → encode → decode
//! round-trip for `klv::st0903::decode`.
//!
//! `decode` may return `Err` on any input, but must not panic. For
//! inputs that decode successfully, `encode_to_vec` followed by a
//! second `decode` must yield an equal `VmtiLs` (modulo Tag 1
//! checksum, which `encode_to_vec` intentionally drops — the muxer
//! handles it externally).

#![no_main]
use libfuzzer_sys::fuzz_target;
use tst_core::klv::st0903::{decode, decode_strict, encode_strict_compliance, encode_to_vec};

fuzz_target!(|data: &[u8]| {
    // Lenient + strict decoders: panic-freedom probe (preserves
    // pre-Phase-6 coverage on decode_strict).
    let _ = decode_strict(data);
    let Ok(mut ls1) = decode(data) else {
        return;
    };

    // Strict encoder panic-freedom probe. `encode_strict_compliance`
    // exercises MissingMandatoryItem / structural guard paths that `encode_to_vec`
    // does not reach. May Err on incomplete records; must never panic.
    let _ = encode_strict_compliance(&ls1);

    // Round-trip: decode → encode → decode must yield an equal VmtiLs.
    // `field_errors` is excluded from PartialEq (manual impl) since
    // it's a decoder-side diagnostic, not part of the LS value.
    //
    // Tag 1 (checksum) is intentionally dropped by `encode_to_vec`
    // (the muxer handles it externally). Normalize
    // before comparing to avoid spurious round-trip failures on
    // inputs that happen to contain Tag 1. Minimal reproducer of
    // the un-normalized failure mode: `\x01\x02\x00\x11`.
    let bytes = match encode_to_vec(&ls1) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut ls2 = decode(&bytes).expect("decode-after-encode must succeed");
    ls1.checksum = None;
    ls2.checksum = None;
    assert_eq!(ls1, ls2, "ST 0903 round-trip mismatch");
});
