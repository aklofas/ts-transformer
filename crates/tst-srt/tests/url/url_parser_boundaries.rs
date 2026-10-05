//! Boundary-value tests for the URL parser.

use tst_srt::{SrtUrl, UrlError};

// ============================================================================
// INT (i32) boundaries — using `latency` as the representative key.
// ============================================================================

#[test]
fn int_zero() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=0").unwrap();
    assert!(u.overlay.latency.is_some());
}

#[test]
fn int_i32_max() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=2147483647").unwrap();
    assert!(u.overlay.latency.is_some());
}

#[test]
fn int_overflow_above_i32() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=2147483648").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

#[test]
fn int_negative() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=-1").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

#[test]
fn int_empty_value() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

// ============================================================================
// INT64 (u64) boundaries — using `maxbw` as the representative key.
// ============================================================================

#[test]
fn int64_zero() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?maxbw=0").unwrap();
    assert!(u.overlay.max_bandwidth.is_some());
}

#[test]
fn int64_u64_max() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?maxbw=18446744073709551615").unwrap();
    assert!(u.overlay.max_bandwidth.is_some());
}

#[test]
fn int64_overflow_above_u64() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?maxbw=18446744073709551616").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

// ============================================================================
// BOOL boundaries — using `tlpktdrop`.
// ============================================================================

#[test]
fn bool_zero() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=0").unwrap();
    assert_eq!(u.overlay.too_late_packet_drop, Some(false));
}

#[test]
fn bool_one() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=1").unwrap();
    assert_eq!(u.overlay.too_late_packet_drop, Some(true));
}

#[test]
fn bool_two_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=2").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

#[test]
fn bool_true_word_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=true").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

#[test]
fn bool_empty_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=").unwrap_err();
    assert!(matches!(e, UrlError::InvalidValue { .. }));
}

// ============================================================================
// ENUM boundaries — using `congestion`.
// ============================================================================

#[test]
fn enum_lowercase_live() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?congestion=live").unwrap();
    assert!(matches!(
        u.overlay.congestion,
        Some(tst_srt::Congestion::Live)
    ));
}

#[test]
fn enum_uppercase_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?congestion=Live").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

#[test]
fn enum_unknown_value_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?congestion=fast").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

// ============================================================================
// STRING boundaries — passphrase and streamid.
// ============================================================================

#[test]
fn passphrase_min_length_10() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?passphrase=ten-chars!").unwrap();
    assert!(u.overlay.passphrase.is_some());
}

#[test]
fn passphrase_max_length_79() {
    let p79 = "a".repeat(79);
    let url = format!("srt://1.2.3.4:9000?passphrase={p79}");
    let u = SrtUrl::parse(&url).unwrap();
    assert!(u.overlay.passphrase.is_some());
}

#[test]
fn passphrase_too_long_80() {
    let p80 = "a".repeat(80);
    let url = format!("srt://1.2.3.4:9000?passphrase={p80}");
    let e = SrtUrl::parse(&url).unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

#[test]
fn passphrase_empty_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?passphrase=").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

#[test]
fn streamid_empty_succeeds() {
    // Empty streamid is technically valid (zero-length is OK per
    // libsrt); but our typed StreamId::new accepts empty.
    let u = SrtUrl::parse("srt://1.2.3.4:9000?streamid=").unwrap();
    assert_eq!(u.overlay.stream_id.as_ref().unwrap().as_str(), "");
}

#[test]
fn streamid_at_512_byte_limit() {
    let s = "a".repeat(512);
    let url = format!("srt://1.2.3.4:9000?streamid={s}");
    let u = SrtUrl::parse(&url).unwrap();
    assert_eq!(u.overlay.stream_id.as_ref().unwrap().as_str().len(), 512);
}

#[test]
fn streamid_above_512_byte_limit() {
    let s = "a".repeat(513);
    let url = format!("srt://1.2.3.4:9000?streamid={s}");
    let e = SrtUrl::parse(&url).unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

// ============================================================================
// pbkeylen — only 16/24/32 are valid.
// ============================================================================

#[test]
fn pbkeylen_zero_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=0").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

#[test]
fn pbkeylen_15_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=15").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

#[test]
fn pbkeylen_17_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=17").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}

#[test]
fn pbkeylen_25_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=25").unwrap_err();
    assert!(matches!(e, UrlError::OptionValidation { .. }));
}
