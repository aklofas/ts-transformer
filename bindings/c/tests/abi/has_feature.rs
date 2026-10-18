//! `tst_has_feature`: the Rust-side answers, and the agreement between the
//! generated header's `TST_HAS_*` defines and the runtime answer.

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

/// Compiles `tests/has_feature.c` against the header the build generated for
/// the active features and the cdylib next to it. The cdylib must have been
/// built with the same features (`cargo build -p tst-c --features …`).
#[cfg(not(target_os = "windows"))]
#[test]
fn header_defines_agree_with_the_runtime_answer() {
    use std::path::PathBuf;
    use std::process::Command;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(manifest_dir).join("../../target"));
    let debug_dir = target_dir.join("debug");
    let include_dir = debug_dir.join("include");
    let c_file = PathBuf::from(manifest_dir).join("tests/has_feature.c");
    let bin = std::env::temp_dir().join("tst_has_feature_check");

    let status = Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror", "-I"])
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
