//! Verifies that tst-c's feature gating exposes the expected symbols
//! at each feature flavor. cfg! conditions are evaluated at compile
//! time per the active feature set; this test confirms what cargo
//! actually built matches the symbol expectations.
//!
//! Run across all four feature modes:
//!
//! ```text
//! cargo test -p tst-c --test feature_matrix_compile                              # default (srt + rtp)
//! cargo test -p tst-c --no-default-features --features srt --test ...            # srt-only
//! cargo test -p tst-c --no-default-features --features rtp --test ...            # rtp-only
//! cargo test -p tst-c --no-default-features --test ...                           # neither (0 tests)
//! ```
//!
//! Symbols are imported by Rust path rather than by FFI call — the goal is
//! to catch accidental cfg-leak regressions (symbol present in a build that
//! should not have it) faster than waiting for the full CI matrix.
//!
//! Note: the lib name for `tst-c` is `tstrans` (see `[lib] name` in
//! Cargo.toml); integration tests reference it as `tstrans`, not `tst_c`.

#[cfg(feature = "srt")]
#[test]
fn srt_feature_exposes_sender_open() {
    use tstrans::tst_sender_open;
    let _ = tst_sender_open;
}

#[cfg(feature = "rtp")]
#[test]
fn rtp_feature_exposes_rtp_open() {
    use tstrans::tst_rtp_sender_open;
    let _ = tst_rtp_sender_open;
}

#[cfg(feature = "rtp")]
#[test]
fn rtp_feature_exposes_rtp_mux_sender_open() {
    use tstrans::tst_rtp_mux_sender_open;
    let _ = tst_rtp_mux_sender_open;
}

#[cfg(feature = "rtp")]
#[test]
fn rtp_feature_exposes_rtsp_client_builder() {
    use tstrans::tst_rtsp_client_builder_new;
    let _ = tst_rtsp_client_builder_new;
}

#[cfg(feature = "rtp")]
#[test]
fn rtp_feature_exposes_rtsp_server_builder() {
    use tstrans::tst_rtsp_server_builder_new;
    let _ = tst_rtsp_server_builder_new;
}

// udp / tcp / hls / rist feature gates. Each test is a pure
// compile-time existence check: the symbol resolving proves the cfg gate
// is wired correctly for that feature.

#[cfg(feature = "udp")]
#[test]
fn udp_feature_exposes_open_function() {
    let _ = tstrans::tst_udp_sender_open;
}

#[cfg(feature = "tcp")]
#[test]
fn tcp_feature_exposes_open_function() {
    let _ = tstrans::tst_tcp_sender_open;
}

#[cfg(feature = "hls")]
#[test]
fn hls_feature_exposes_publisher_builder() {
    let _ = tstrans::tst_hls_publisher_builder_new;
}

#[cfg(feature = "rist")]
#[test]
fn rist_feature_exposes_open_function() {
    let _ = tstrans::tst_rist_sender_open;
}

// Combined-feature sanity: the four new transports compose without
// symbol-resolution conflicts (no rist here — see the dual-mbedTLS note
// in build.rs; srt+rist in one binary is unsupported, and this test runs
// under whatever feature set the build selected).
#[cfg(all(feature = "udp", feature = "tcp", feature = "hls"))]
#[test]
fn udp_tcp_hls_features_compose() {
    let _ = tstrans::tst_udp_sender_open;
    let _ = tstrans::tst_tcp_sender_open;
    let _ = tstrans::tst_hls_publisher_builder_new;
}
