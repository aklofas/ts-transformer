//! Admission policy for a host (pkg-config) native library.
//!
//! Shared verbatim by `srt-sys` and `rist-sys`: each package must build on
//! its own, so each carries its own copy of this file.

/// Parses `major.minor.patch`, ignoring any non-digit suffix on a component
/// (`1.5.7-dev` reads as 1.5.7). Fewer than three numeric parts is `None`.
fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.trim().split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Decides whether a host library may be linked instead of the vendored one.
///
/// It must be at least `floor`, and when the build wants mbedTLS the host
/// library's static link set must contain `mbedcrypto`. A host library built
/// against another crypto backend would link a second TLS stack beside the
/// vendored mbedTLS of the sibling sys crate.
pub fn host_lib_verdict(
    version: &str,
    floor: (u32, u32, u32),
    static_libs: &[String],
    want_mbedtls: bool,
) -> Result<(), String> {
    let (fa, fb, fc) = floor;
    match parse_version(version) {
        None => {
            return Err(format!(
                "version {version:?} is not a major.minor.patch triple"
            ));
        }
        Some(v) if v < floor => {
            return Err(format!(
                "version {version} is below the {fa}.{fb}.{fc} floor"
            ));
        }
        Some(_) => {}
    }
    if want_mbedtls && !static_libs.iter().any(|l| l == "mbedcrypto") {
        return Err(format!(
            "{version} is not built against mbedTLS (no mbedcrypto in its static link set)"
        ));
    }
    Ok(())
}

/// The build-script warning for a refused host library; names the library.
pub fn vendored_warning(name: &str, reason: &str) -> String {
    format!("using the vendored {name}: {reason}")
}
