//! Host-capability skips that CI can turn into failures.
//!
//! A test that needs something the host may lack (multicast on loopback, an
//! external tool such as ffprobe) returns early through
//! [`skip_unless_required`]. Where CI knows the capability is present it sets
//! `TST_REQUIRE_<CAP>=1`, and the skip becomes a failure. Elsewhere the skip is
//! printed and, when `TST_CAPABILITY_LOG` names a file, appended to it so the
//! job can list every skip it took.

use std::fmt::Display;
use std::io::Write;

/// `TST_REQUIRE_<cap>` is set to `1`.
pub fn required(cap: &str) -> bool {
    std::env::var(format!("TST_REQUIRE_{cap}")).is_ok_and(|v| v == "1")
}

/// Records that the calling test skips for want of `cap`; panics instead when
/// [`required`] says this host must have it.
pub fn skip_unless_required(cap: &str, why: impl Display) {
    if required(cap) {
        panic!("capability {cap} is required on this host and is unavailable: {why}");
    }
    eprintln!("SKIP(capability {cap}): {why}");
    if let Some(path) = std::env::var_os("TST_CAPABILITY_LOG") {
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("<unnamed>");
        // One write per line: nextest runs each test in its own process, and
        // `writeln!`'s several writes would interleave with another
        // process's line. A lost log line must not fail the skipping test.
        let line = format!("{cap}\t{test}\t{why}\n");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_required_capability_panics_and_an_optional_one_logs() {
        let log = std::env::temp_dir().join(format!("cap-log-{}", std::process::id()));
        // SAFETY: the test owns these two variable names; no other test reads them.
        unsafe {
            std::env::set_var("TST_CAPABILITY_LOG", &log);
            std::env::remove_var("TST_REQUIRE_SELFTESTCAP");
        }
        skip_unless_required("SELFTESTCAP", "no such thing");
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .starts_with("SELFTESTCAP\t")
        );
        unsafe {
            std::env::set_var("TST_REQUIRE_SELFTESTCAP", "1");
        }
        assert!(std::panic::catch_unwind(|| skip_unless_required("SELFTESTCAP", "x")).is_err());
        let _ = std::fs::remove_file(&log);
    }
}
