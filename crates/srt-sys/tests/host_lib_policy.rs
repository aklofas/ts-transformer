#[path = "../build/host_lib.rs"]
mod host_lib;

#[test]
fn host_lib_policy() {
    let mbed = vec!["srt".to_string(), "mbedtls".into(), "mbedcrypto".into()];
    let ossl = vec!["srt".to_string(), "ssl".into(), "crypto".into()];
    assert!(host_lib::host_lib_verdict("1.5.7", (1, 5, 7), &mbed, true).is_ok());
    assert!(host_lib::host_lib_verdict("1.5.10", (1, 5, 7), &mbed, true).is_ok());
    assert!(host_lib::host_lib_verdict("1.5.7-dev", (1, 5, 7), &mbed, true).is_ok());
    assert!(
        host_lib::host_lib_verdict("1.5.5", (1, 5, 7), &mbed, true)
            .unwrap_err()
            .contains("1.5.7 floor")
    );
    assert!(
        host_lib::host_lib_verdict("1.5.7", (1, 5, 7), &ossl, true)
            .unwrap_err()
            .contains("mbedTLS")
    );
    assert!(host_lib::host_lib_verdict("1.5.7", (1, 5, 7), &ossl, false).is_ok());
    assert!(host_lib::host_lib_verdict("garbage", (1, 5, 7), &mbed, true).is_err());
    assert!(host_lib::host_lib_verdict("1.5", (1, 5, 7), &mbed, true).is_err());
}

#[test]
fn refusal_warning_names_the_library() {
    let reason = host_lib::host_lib_verdict("1.5.5", (1, 5, 7), &[], false).unwrap_err();
    assert!(
        host_lib::vendored_warning("libsrt", &reason).starts_with("using the vendored libsrt: ")
    );
}
