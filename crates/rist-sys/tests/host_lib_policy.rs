#[path = "../build/host_lib.rs"]
mod host_lib;

#[test]
fn host_lib_policy() {
    let mbed = vec!["rist".to_string(), "mbedtls".into(), "mbedcrypto".into()];
    let ossl = vec!["rist".to_string(), "ssl".into(), "crypto".into()];
    assert!(host_lib::host_lib_verdict("0.2.20", (0, 2, 20), &mbed, true).is_ok());
    assert!(host_lib::host_lib_verdict("0.3.0", (0, 2, 20), &mbed, true).is_ok());
    assert!(
        host_lib::host_lib_verdict("0.2.10", (0, 2, 20), &mbed, true)
            .unwrap_err()
            .contains("0.2.20 floor")
    );
    assert!(
        host_lib::host_lib_verdict("0.2.20", (0, 2, 20), &ossl, true)
            .unwrap_err()
            .contains("mbedTLS")
    );
    assert!(host_lib::host_lib_verdict("0.2.20", (0, 2, 20), &ossl, false).is_ok());
    assert!(host_lib::host_lib_verdict("garbage", (0, 2, 20), &mbed, true).is_err());
}

#[test]
fn refusal_warning_names_the_library() {
    let reason = host_lib::host_lib_verdict("0.2.10", (0, 2, 20), &[], false).unwrap_err();
    assert!(
        host_lib::vendored_warning("librist", &reason).starts_with("using the vendored librist: ")
    );
}

use host_lib::{HostLibDecision, StaticProbe, host_lib_decision, metadata_probe_failed_warning};

#[test]
fn found_and_admitted_links() {
    let mbed = vec!["rist".to_string(), "mbedcrypto".into()];
    let d = host_lib_decision(
        "librist",
        (0, 2, 20),
        true,
        StaticProbe::Found {
            version: "0.2.20",
            libs: &mbed,
        },
    );
    assert_eq!(d, HostLibDecision::Link);
}

#[test]
fn found_but_refused_warns_with_the_reason() {
    let ossl = vec!["rist".to_string(), "ssl".into()];
    match host_lib_decision(
        "librist",
        (0, 2, 20),
        true,
        StaticProbe::Found {
            version: "0.2.20",
            libs: &ossl,
        },
    ) {
        HostLibDecision::Vendored { warning: Some(w) } => {
            assert!(w.starts_with("using the vendored librist: "), "{w}");
            assert!(w.contains("mbedTLS"), "{w}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn installed_but_not_static_resolvable_warns() {
    let d = host_lib_decision(
        "librist",
        (0, 2, 20),
        true,
        StaticProbe::Failed {
            error: "Package mbedtls was not found",
            installed_version: Some("0.2.20"),
        },
    );
    match d {
        HostLibDecision::Vendored { warning: Some(w) } => {
            assert!(
                w.contains(
                    "0.2.20 is installed but `pkg-config --static librist` could not resolve it"
                ),
                "{w}"
            );
            assert!(w.contains("Package mbedtls was not found"), "{w}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn nothing_installed_is_silent() {
    let d = host_lib_decision(
        "librist",
        (0, 2, 20),
        true,
        StaticProbe::Failed {
            error: "Package librist was not found",
            installed_version: None,
        },
    );
    assert_eq!(d, HostLibDecision::Vendored { warning: None });
}

#[test]
fn metadata_probe_failure_warning_names_the_library() {
    let w = metadata_probe_failed_warning("librist", "boom");
    assert!(w.starts_with("using the vendored librist: "), "{w}");
    assert!(w.contains("metadata probe failed: boom"), "{w}");
}
