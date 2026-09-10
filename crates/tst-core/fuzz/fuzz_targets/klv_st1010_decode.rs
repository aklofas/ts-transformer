//! Fuzz target — panic/abort-freedom for `klv::st1010::decode_sdcc_flp`,
//! plus an `encode_sdcc_flp_mode2` → `decode_sdcc_flp` round-trip.
//!
//! `decode_sdcc_flp` may return `Err` on any input, but must never panic
//! *or abort the process* — the latter is the failure mode a hostile
//! Element-1 Matrix Size (BER-OID, attacker-controlled up to `u32::MAX`)
//! used to trigger: `corr_slots(N) = N(N-1)/2` sized a `Vec` before any
//! correlation byte was read, so a ~7-byte input could demand a
//! multi-exabyte allocation. See `check_matrix_size_fits` in `st1010.rs`.

#![no_main]
use libfuzzer_sys::fuzz_target;
use tst_core::klv::st1010::{decode_sdcc_flp, encode_sdcc_flp_mode2};

fuzz_target!(|data: &[u8]| {
    let Ok(pack) = decode_sdcc_flp(data) else {
        return;
    };

    // Round-trip: re-encode at a fixed Clen and decode again. Only
    // well-defined when the decoded correlations are all within IMAPB's
    // [-1.0, 1.0] range (encode_sdcc_flp_mode2 rejects out-of-range
    // values; a foreign producer's IEEE correlations can legally exceed
    // that band, which is out of round-trip scope here).
    if pack
        .correlations
        .iter()
        .any(|&r| !(-1.0..=1.0).contains(&r))
    {
        return;
    }
    let Ok(bytes) = encode_sdcc_flp_mode2(&pack.std_devs, &pack.correlations, 2) else {
        return;
    };
    let _ = decode_sdcc_flp(&bytes);
});
