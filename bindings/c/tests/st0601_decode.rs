//! C ABI: MISB ST 0601 KLV decode surface (ABI 21).
//!
//! Exercises `tst_st0601_*` through the public `tstrans::klv_st0601`
//! re-export — the same crate-external-caller path a real C consumer
//! goes through (as opposed to `bindings/c/core/src/klv_st0601.rs`'s own
//! `#[cfg(test)]` unit tests, which run in-crate). Unconditional module
//! (no `srt`/`rtp` feature gate needed), so this file carries no
//! `#![cfg(feature = ...)]`.

use std::ffi::CStr;
use std::path::Path;

use tstrans::error::{TstError, tst_get_last_error, tst_get_last_error_str};
use tstrans::klv_st0601::{
    TST_ST0601_TAG_PLATFORM_HEADING, TST_ST0601_TAG_PRECISION_TIMESTAMP, TstSt0601FieldState,
    TstSt0601Geometry, tst_st0601_decode, tst_st0601_free, tst_st0601_geometry, tst_st0601_get_f64,
    tst_st0601_get_u64, tst_st0601_state,
};

fn load_fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/tst-core/tests/fixtures/st0601")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
}

fn last_error_msg() -> String {
    unsafe {
        CStr::from_ptr(tst_get_last_error_str())
            .to_str()
            .unwrap()
            .to_owned()
    }
}

#[test]
fn decode_full_fixture_then_geometry_reports_known_values() {
    let bytes = load_fixture("synthetic_full.klv");

    unsafe {
        let p = tst_st0601_decode(bytes.as_ptr(), bytes.len());
        assert!(!p.is_null(), "decode failed: {}", last_error_msg());

        let mut out = std::mem::MaybeUninit::<TstSt0601Geometry>::uninit();
        let rc = tst_st0601_geometry(p, out.as_mut_ptr());
        assert_eq!(rc, 0, "geometry failed: {}", last_error_msg());
        let geo = out.assume_init();

        // Known values baked into synthetic_full.klv by
        // crates/tst-core/tests/tools/gen_synthetic_fixtures.rs::full().
        assert_eq!(geo.timestamp_state, TstSt0601FieldState::Present as u8);
        assert_eq!(geo.timestamp_us, 1_700_123_456_789_000);

        assert_eq!(geo.sensor_lat_state, TstSt0601FieldState::Present as u8);
        assert!(
            (geo.sensor_lat_deg - 38.123456).abs() < 1e-3,
            "sensor_lat_deg = {}",
            geo.sensor_lat_deg
        );

        assert_eq!(
            geo.platform_heading_state,
            TstSt0601FieldState::Present as u8
        );
        assert!((geo.platform_heading_deg - 123.45).abs() < 0.01);

        // The fixture carries the full corner family (tags 82-89), so
        // geometry() must prefer it over the offset family it also
        // carries (tags 26-33).
        assert_eq!(geo.corner_lat_p1_state, TstSt0601FieldState::Present as u8);
        assert!((geo.corner_lat_p1_deg - 38.001).abs() < 1e-3);
        assert_eq!(geo.corner_lon_p4_state, TstSt0601FieldState::Present as u8);
        assert!((geo.corner_lon_p4_deg - -121.499).abs() < 1e-3);

        tst_st0601_free(p);
    }
}

#[test]
fn get_f64_on_platform_heading_matches_geometry() {
    let bytes = load_fixture("synthetic_full.klv");

    unsafe {
        let p = tst_st0601_decode(bytes.as_ptr(), bytes.len());
        assert!(!p.is_null());

        let mut heading = 0.0f64;
        let rc = tst_st0601_get_f64(p, TST_ST0601_TAG_PLATFORM_HEADING, &mut heading);
        assert_eq!(rc, 0);
        assert!((heading - 123.45).abs() < 0.01);

        tst_st0601_free(p);
    }
}

#[test]
fn get_u64_reads_precision_timestamp() {
    let bytes = load_fixture("synthetic_full.klv");

    unsafe {
        let p = tst_st0601_decode(bytes.as_ptr(), bytes.len());
        assert!(!p.is_null());

        let mut ts = 0u64;
        let rc = tst_st0601_get_u64(p, TST_ST0601_TAG_PRECISION_TIMESTAMP, &mut ts);
        assert_eq!(rc, 0);
        assert_eq!(ts, 1_700_123_456_789_000);

        tst_st0601_free(p);
    }
}

#[test]
fn get_f64_on_u64_tag_returns_wrong_type_not_a_lossy_cast() {
    let bytes = load_fixture("synthetic_full.klv");

    unsafe {
        let p = tst_st0601_decode(bytes.as_ptr(), bytes.len());
        assert!(!p.is_null());

        let mut out = 0.0f64;
        let rc = tst_st0601_get_f64(p, TST_ST0601_TAG_PRECISION_TIMESTAMP, &mut out);
        assert_eq!(rc, TstError::WrongType as i32);
        assert_eq!(tst_get_last_error(), TstError::WrongType as i32);
        // *out must be left untouched, not silently populated with a
        // lossy cast of the u64 timestamp.
        assert_eq!(out, 0.0);

        tst_st0601_free(p);
    }
}

#[test]
fn state_reports_absent_for_a_tag_outside_the_contract_table() {
    let bytes = load_fixture("synthetic_full.klv");

    unsafe {
        let p = tst_st0601_decode(bytes.as_ptr(), bytes.len());
        assert!(!p.is_null());

        // Tag 1 (checksum) is a real ST 0601 tag but is not one of the
        // curated contract fields this module maps.
        let state = tst_st0601_state(p, 1);
        assert_eq!(state, TstSt0601FieldState::Absent);

        // A populated contract tag reports Present.
        let state = tst_st0601_state(p, TST_ST0601_TAG_PLATFORM_HEADING);
        assert_eq!(state, TstSt0601FieldState::Present);

        tst_st0601_free(p);
    }
}

#[test]
fn decode_garbage_bytes_returns_null_with_klv_decode_error() {
    let garbage = [0xFFu8; 8];
    unsafe {
        let p = tst_st0601_decode(garbage.as_ptr(), garbage.len());
        assert!(p.is_null());
        assert_eq!(tst_get_last_error(), TstError::KlvDecode as i32);
    }
}
