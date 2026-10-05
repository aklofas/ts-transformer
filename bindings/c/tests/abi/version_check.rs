//! Runtime version-accessor tests.
//!
//! Each `tst_get_*_version_*` entry MUST return the corresponding
//! `TST_*_VERSION_*` compile-time constant. The values themselves are
//! incidental — what matters is that the runtime accessor and the
//! header macro agree at this build artifact. A drift would mean
//! `examples/getting-started/version_check.c`'s cross-validation
//! would silently lie about SO/header alignment.

use std::ffi::CStr;

use tstrans::{
    TST_ABI_VERSION_MAJOR, TST_ABI_VERSION_MINOR, TST_VERSION_MAJOR, TST_VERSION_MINOR,
    TST_VERSION_PATCH,
};

// SAFETY: the tst_get_*_version_* / tst_get_abi_version_* functions are
// `unsafe extern "C"` by convention (no real unsafety; see the rustdoc
// on each). Sound under any invocation.

#[test]
fn major_matches_compile_time_const() {
    let runtime = unsafe { tstrans::tst_get_version_major() };
    assert_eq!(
        runtime, TST_VERSION_MAJOR as u32,
        "runtime version major ({runtime}) does not match compile-time const TST_VERSION_MAJOR ({})",
        TST_VERSION_MAJOR
    );
}

#[test]
fn minor_matches_compile_time_const() {
    let runtime = unsafe { tstrans::tst_get_version_minor() };
    assert_eq!(
        runtime, TST_VERSION_MINOR as u32,
        "runtime version minor ({runtime}) != TST_VERSION_MINOR ({})",
        TST_VERSION_MINOR
    );
}

#[test]
fn patch_matches_compile_time_const() {
    let runtime = unsafe { tstrans::tst_get_version_patch() };
    assert_eq!(
        runtime, TST_VERSION_PATCH as u32,
        "runtime version patch ({runtime}) != TST_VERSION_PATCH ({})",
        TST_VERSION_PATCH
    );
}

#[test]
fn packed_matches_field_composition() {
    let runtime = unsafe { tstrans::tst_get_version_packed() };
    let expected = ((TST_VERSION_MAJOR as u32) << 16)
        | ((TST_VERSION_MINOR as u32) << 8)
        | (TST_VERSION_PATCH as u32);
    assert_eq!(
        runtime, expected,
        "packed encoding (M<<16)|(m<<8)|p drifted: runtime=0x{runtime:06x} expected=0x{expected:06x}"
    );
}

#[test]
fn string_matches_dotted_form() {
    let ptr = unsafe { tstrans::tst_get_version_string() };
    assert!(!ptr.is_null(), "tst_get_version_string returned NULL");
    // SAFETY: the pointer is process-lifetime static per the function's
    // documented contract.
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .expect("tst_get_version_string returned non-UTF-8");
    let expected = format!(
        "{}.{}.{}",
        TST_VERSION_MAJOR, TST_VERSION_MINOR, TST_VERSION_PATCH
    );
    assert_eq!(s, expected);
}

#[test]
fn abi_major_matches_compile_time_const() {
    let runtime = unsafe { tstrans::tst_get_abi_version_major() };
    assert_eq!(
        runtime, TST_ABI_VERSION_MAJOR as u32,
        "runtime ABI version major ({runtime}) != TST_ABI_VERSION_MAJOR ({})",
        TST_ABI_VERSION_MAJOR
    );
}

#[test]
fn abi_minor_matches_compile_time_const() {
    let runtime = unsafe { tstrans::tst_get_abi_version_minor() };
    assert_eq!(
        runtime, TST_ABI_VERSION_MINOR as u32,
        "runtime ABI version minor ({runtime}) != TST_ABI_VERSION_MINOR ({})",
        TST_ABI_VERSION_MINOR
    );
}
