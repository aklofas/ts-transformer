//! `tst_has_feature`: the Rust-side answers, and a C program's view of them.

use std::ffi::{CString, c_char};

fn has(name: &str) -> i32 {
    let c = CString::new(name).unwrap();
    // SAFETY: `c` is a valid NUL-terminated string for the call.
    unsafe { tstrans::tst_has_feature(c.as_ptr()) }
}

#[test]
fn named_features_answer_their_cfg() {
    assert_eq!(has("srt"), cfg!(feature = "srt") as i32);
    assert_eq!(has("rtp"), cfg!(feature = "rtp") as i32);
    assert_eq!(has("udp"), cfg!(feature = "udp") as i32);
    assert_eq!(has("tcp"), cfg!(feature = "tcp") as i32);
    assert_eq!(has("hls"), cfg!(feature = "hls") as i32);
    assert_eq!(has("rist"), cfg!(feature = "rist") as i32);
    assert_eq!(has("tls"), cfg!(feature = "tls") as i32);
}

#[test]
fn unknown_wrong_case_empty_and_null_answer_zero() {
    assert_eq!(has("SRT"), 0);
    assert_eq!(has("Tls"), 0);
    assert_eq!(has("quic"), 0);
    assert_eq!(has("srt "), 0);
    assert_eq!(has(""), 0);
    // SAFETY: NULL is the documented "no name" input.
    assert_eq!(
        unsafe { tstrans::tst_has_feature(std::ptr::null::<c_char>()) },
        0
    );
}

/// Compiles `tests/has_feature.c` with this binary's own feature set passed as
/// `-DEXPECT_<NAME>=0|1`, links the cdylib in `target/debug`, and runs it.
/// The cdylib must have been built with the same features
/// (`cargo build -p tst-c --features ...`); the header on disk is not consulted
/// for the expectations, so build order cannot make the check vacuous.
#[cfg(not(target_os = "windows"))]
#[test]
fn c_program_sees_the_runtime_answers() {
    use std::path::PathBuf;
    use std::process::Command;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(manifest_dir).join("../../target"));
    let debug_dir = target_dir.join("debug");
    // This build's own include dir (OUT_DIR) holds its tstrans_config.h; the
    // copy under target/debug is last-build-wins.
    let include_dir = PathBuf::from(env!("OUT_DIR")).join("include");
    let c_file = PathBuf::from(manifest_dir).join("tests/has_feature.c");
    let bin = std::env::temp_dir().join("tst_has_feature_check");

    let so = format!(
        "{}tstrans{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    assert!(
        debug_dir.join(&so).exists(),
        "build tst-c first with the same features ({} missing)",
        debug_dir.join(&so).display()
    );

    let expect = |name: &str, on: bool| format!("-DEXPECT_{name}={}", on as i32);
    let status = Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror"])
        .args([
            expect("SRT", cfg!(feature = "srt")),
            expect("RTP", cfg!(feature = "rtp")),
            expect("UDP", cfg!(feature = "udp")),
            expect("TCP", cfg!(feature = "tcp")),
            expect("HLS", cfg!(feature = "hls")),
            expect("RIST", cfg!(feature = "rist")),
            expect("TLS", cfg!(feature = "tls")),
        ])
        .arg("-I")
        .arg(&include_dir)
        .arg("-L")
        .arg(&debug_dir)
        .arg(&c_file)
        .args(["-ltstrans", "-o"])
        .arg(&bin)
        .status()
        .expect("invoke cc");
    assert!(status.success(), "cc failed");

    let env_name = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let out = Command::new(&bin)
        .env(env_name, &debug_dir)
        .output()
        .expect("run has_feature binary");
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("has_feature OK"));
}
