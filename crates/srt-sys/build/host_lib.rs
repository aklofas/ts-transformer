//! Admission policy for a host (pkg-config) native library.
//!
//! Shared verbatim by `srt-sys` and `rist-sys`: each package must build on
//! its own, so each carries its own copy of this file.

/// Parses `major.minor.patch`, ignoring any non-digit suffix on a component
/// (`1.5.7-dev` reads as 1.5.7). Fewer than three numeric parts is `None`.
///
/// A pre-release or distribution suffix on the floor itself (`0.2.20rc1`,
/// `1.5.7-dev`, `0.2.20-3`) is read as the floor and admitted: distribution
/// suffixes are patch levels and rc builds of these libraries are rare.
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

/// The outcome of `pkg-config --static <name>`, as the build script saw it.
pub enum StaticProbe<'a> {
    /// pkg-config resolved the library statically.
    Found {
        version: &'a str,
        libs: &'a [String],
    },
    /// The static probe failed. `installed_version` is `Some` when a plain
    /// (non-static) probe of the same name succeeded, meaning a host library
    /// is installed but `--static` could not resolve it (typically a
    /// `Requires.private` entry without its own `.pc`).
    Failed {
        error: &'a str,
        installed_version: Option<&'a str>,
    },
}

/// What the build does after the probe.
#[derive(Debug, PartialEq, Eq)]
pub enum HostLibDecision {
    /// Admit the host library: run the metadata probe and link it.
    Link,
    /// Build the vendored copy. `warning` is printed as `cargo:warning`
    /// when `Some`; `None` is the silent normal path (nothing installed).
    Vendored { warning: Option<String> },
}

/// Warns when a found host library is refused (floor, backend, or installed
/// but not statically resolvable), never when none is installed. `name` is the
/// display name in warnings; `pkg_config_name` is the module `pkg-config` knows.
pub fn host_lib_decision(
    name: &str,
    pkg_config_name: &str,
    floor: (u32, u32, u32),
    want_mbedtls: bool,
    probe: StaticProbe<'_>,
) -> HostLibDecision {
    match probe {
        StaticProbe::Found { version, libs } => {
            match host_lib_verdict(version, floor, libs, want_mbedtls) {
                Ok(()) => HostLibDecision::Link,
                Err(reason) => HostLibDecision::Vendored {
                    warning: Some(vendored_warning(name, &reason)),
                },
            }
        }
        StaticProbe::Failed {
            error,
            installed_version: Some(v),
        } => HostLibDecision::Vendored {
            warning: Some(vendored_warning(
                name,
                &format!(
                    "{name} {v} is installed but `pkg-config --static {pkg_config_name}` could not resolve it: {error}"
                ),
            )),
        },
        StaticProbe::Failed {
            installed_version: None,
            ..
        } => HostLibDecision::Vendored { warning: None },
    }
}

/// The warning for a library that was admitted but whose metadata probe
/// (the plain `pkg-config <name>` that emits the link lines) then failed.
pub fn metadata_probe_failed_warning(name: &str, error: &str) -> String {
    format!(
        "using the vendored {name}: admitted the host library but its pkg-config metadata probe failed: {error}"
    )
}
