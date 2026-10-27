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
