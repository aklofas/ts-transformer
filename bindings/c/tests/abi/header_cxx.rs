//! The generated header must compile as C++17 and as C11.
#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn syntax_check(compiler: &str, args: &[&str], include: &Path, extra: Option<&str>) -> Output {
    let mut cmd = Command::new(compiler);
    cmd.args(args).arg("-fsyntax-only").arg("-I").arg(include);
    if let Some(flag) = extra {
        cmd.arg(flag);
    }
    let mut child = cmd
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("invoke {compiler}: {e}"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"#include \"tstrans.h\"\nint main(void){return 0;}\n")
        .unwrap();
    child.wait_with_output().unwrap()
}

/// cbindgen emits `cpp_compat = true` linkage guards, so the header is meant
/// to be includable from C++. `_Static_assert` is C11 only; C++11 spells it
/// `static_assert`. Both must compile with the ABI asserts ON.
#[test]
fn header_compiles_as_cxx17_with_and_without_abi_asserts() {
    let include = PathBuf::from(env!("OUT_DIR")).join("include");
    for extra in [None, Some("-DTST_SKIP_ABI_ASSERTS")] {
        let out = syntax_check("g++", &["-std=c++17", "-x", "c++"], &include, extra);
        assert!(
            out.status.success(),
            "g++ {extra:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = syntax_check(
        "cc",
        &["-std=c11", "-Wall", "-Werror", "-x", "c"],
        &include,
        None,
    );
    assert!(
        out.status.success(),
        "cc: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
