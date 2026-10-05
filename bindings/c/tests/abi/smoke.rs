//! Compiles tests/smoke.c against the cdylib produced by `cargo build`
//! and runs the resulting binary.

// `smoke.c` references `tst_muxer_*` + `tst_sender_*` + `tst_raw_sender_*`
// + `tst_mux_sender_*` (all in `tstrans::sender::*`, gated behind
// `feature = "srt"`). Under `--no-default-features` the cdylib has none
// of those symbols, so `cc` link fails. CI surfaced this on macos-arm64
// because the in-place rebuild produced a stripped dylib; the linux side
// happened to find a cached default-features dylib so the test had been
// passing falsely under reduced feature sets.
//
// Gated off windows-msvc: this test GNU-links the C example with
// `cc -ltstrans`, but on the Windows runner `cc` is Strawberry's mingw gcc
// (`collect2.exe`), which cannot link against the MSVC-ABI `tstrans.dll` /
// `tstrans.dll.lib` import library. C examples are Linux-only by build
// convention; verifying MSVC C-consumption of the cdylib (compile with
// `cl`, link `tstrans.dll.lib`) is not covered here.
#![cfg(all(feature = "srt", not(target_os = "windows")))]

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// The runtime-linker env var that points the dynamic loader at extra
/// library search dirs. Differs per platform:
///   * Linux / FreeBSD / etc. — `LD_LIBRARY_PATH`
///   * macOS / iOS — `DYLD_LIBRARY_PATH`
///   * Windows — `PATH`
const DYLIB_SEARCH_ENV: &str = if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
    "DYLD_LIBRARY_PATH"
} else if cfg!(target_os = "windows") {
    "PATH"
} else {
    "LD_LIBRARY_PATH"
};

#[test]
fn smoke_c_compiles_links_runs() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(manifest_dir).join("../../target"));
    let cdylib_dir = target_dir.join("debug");
    let header = PathBuf::from(manifest_dir).join("include/tstrans.h");
    let smoke_c = PathBuf::from(manifest_dir).join("tests/smoke.c");

    // Platform-correct cdylib filename: libtstrans.so on Linux,
    // libtstrans.dylib on macOS, tstrans.dll on Windows. std::env::consts
    // gives us the prefix ("lib"/"") and suffix (".so"/".dylib"/".dll")
    // per the active target.
    let cdylib_name = format!(
        "{}tstrans{}",
        env::consts::DLL_PREFIX,
        env::consts::DLL_SUFFIX,
    );
    let cdylib_path = cdylib_dir.join(&cdylib_name);
    assert!(
        cdylib_path.exists(),
        "build tst-c first ({} missing)",
        cdylib_path.display()
    );
    assert!(header.exists(), "header missing at {}", header.display());

    let bin_path = std::env::temp_dir().join("tst_smoke");
    let status = Command::new("cc")
        .args([
            "-Wall",
            "-Wextra",
            "-Werror",
            "-I",
            header.parent().unwrap().to_str().unwrap(),
            "-L",
            cdylib_dir.to_str().unwrap(),
            smoke_c.to_str().unwrap(),
            "-ltstrans",
            "-o",
            bin_path.to_str().unwrap(),
        ])
        .status()
        .expect("invoke cc");
    assert!(status.success(), "cc failed");

    let mut cmd = Command::new(&bin_path);
    // Set the platform-correct dylib-search env var so the dynamic
    // loader finds libtstrans at runtime. On Windows, PATH must be
    // *prepended* not replaced — overwriting wipes the system path
    // and the binary fails to find basic C runtime DLLs. On Unix the
    // dylib dir alone is sufficient since the dynamic linker also
    // consults system default search paths.
    let new_search_path = if cfg!(target_os = "windows") {
        let existing = std::env::var("PATH").unwrap_or_default();
        format!("{};{}", cdylib_dir.display(), existing)
    } else {
        cdylib_dir.display().to_string()
    };
    cmd.env(DYLIB_SEARCH_ENV, &new_search_path);
    let out = cmd.output().expect("run smoke binary");
    if !out.status.success() {
        eprintln!("stdout:\n{}", String::from_utf8_lossy(&out.stdout));
        eprintln!("stderr:\n{}", String::from_utf8_lossy(&out.stderr));
        panic!("smoke binary failed: {:?}", out.status);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("smoke OK"),
        "stdout missing 'smoke OK': {stdout}"
    );
}
