use std::time::Duration;

use tst_srt::config::{SrtListenerConfig, SrtSocketConfig};
use tst_srt::{SrtUrl, SrtUrlError};

#[test]
fn ipv4_with_port() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000").unwrap();
    assert_eq!(u.host, "1.2.3.4");
    assert_eq!(u.port, 9000);
}

#[test]
fn dns_with_port() {
    let u = SrtUrl::parse("srt://camera.local:9000").unwrap();
    assert_eq!(u.host, "camera.local");
    assert_eq!(u.port, 9000);
}

#[test]
fn bracketed_ipv6() {
    let u = SrtUrl::parse("srt://[2001:db8::1]:9000").unwrap();
    assert_eq!(u.host, "2001:db8::1");
    assert_eq!(u.port, 9000);
}

#[test]
fn ipv6_loopback() {
    let u = SrtUrl::parse("srt://[::1]:9000").unwrap();
    assert_eq!(u.host, "::1");
    assert_eq!(u.port, 9000);
}

#[test]
fn rejects_wrong_scheme() {
    let e = SrtUrl::parse("https://1.2.3.4:9000").unwrap_err();
    assert!(matches!(e, SrtUrlError::WrongScheme { ref got } if got == "https"));
}

#[test]
fn rejects_no_scheme() {
    // Bare "host:port" doesn't have a scheme separator — the common parser rejects.
    let e = SrtUrl::parse("1.2.3.4:9000").unwrap_err();
    assert!(matches!(e, SrtUrlError::Syntax(_)) || matches!(e, SrtUrlError::WrongScheme { .. }));
}

#[test]
fn rejects_missing_port() {
    let e = SrtUrl::parse("srt://1.2.3.4").unwrap_err();
    assert!(matches!(e, SrtUrlError::MissingPort));
}

#[test]
fn rejects_missing_host() {
    let e = SrtUrl::parse("srt://:9000").unwrap_err();
    assert!(matches!(e, SrtUrlError::MissingHost));
}

#[test]
fn rejects_userinfo() {
    let e = SrtUrl::parse("srt://op:hunter2@1.2.3.4:9000").unwrap_err();
    assert!(matches!(e, SrtUrlError::UserinfoNotSupported));
}

#[test]
fn rejects_userinfo_user_only() {
    let e = SrtUrl::parse("srt://op@1.2.3.4:9000").unwrap_err();
    assert!(matches!(e, SrtUrlError::UserinfoNotSupported));
}

#[test]
fn passphrase_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?passphrase=hunter-too-long-thanks").unwrap();
    assert!(u.config.passphrase.is_some());
}

#[test]
fn passphrase_too_short_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?passphrase=short").unwrap_err();
    assert!(matches!(e, SrtUrlError::OptionValidation { .. }));
}

#[test]
fn passphrase_percent_decoded() {
    // Passphrase contains a percent-encoded space.
    let u = SrtUrl::parse("srt://1.2.3.4:9000?passphrase=hunter%20too%20long").unwrap();
    assert_eq!(
        u.config.passphrase.as_ref().unwrap().as_str(),
        "hunter too long"
    );
}

#[test]
fn streamid_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?streamid=front-camera").unwrap();
    assert_eq!(
        u.config.stream_id.as_ref().unwrap().as_str(),
        "front-camera"
    );
}

#[test]
fn streamid_with_embedded_equals() {
    // The common parser splits on the FIRST `=` per RFC 3986 form-data;
    // trailing `=` chars are part of the value.
    let u = SrtUrl::parse("srt://1.2.3.4:9000?streamid=foo=bar").unwrap();
    assert_eq!(u.config.stream_id.as_ref().unwrap().as_str(), "foo=bar");
}

#[test]
fn streamid_utf8_is_accepted_up_to_512_bytes() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?streamid=%23!::r=caf%C3%A9,m=publish").unwrap();
    assert_eq!(u.config.stream_id.unwrap().as_str(), "#!::r=café,m=publish");
    assert!(tst_srt::StreamId::new("é".repeat(256)).is_ok()); // 512 bytes
    assert!(matches!(
        tst_srt::StreamId::new(format!("{}a", "é".repeat(256))),
        Err(tst_srt::error::StreamIdError::TooLong(513))
    ));
}

#[test]
fn congestion_live() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?congestion=live").unwrap();
    assert!(matches!(
        u.config.congestion,
        Some(tst_srt::Congestion::Live)
    ));
}

#[test]
fn congestion_file() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?congestion=file").unwrap();
    assert!(matches!(
        u.config.congestion,
        Some(tst_srt::Congestion::File)
    ));
}

#[test]
fn congestion_uppercase_rejects() {
    // Strict-A: enums are lowercase-only.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?congestion=Live").unwrap_err();
    assert!(matches!(e, SrtUrlError::OptionValidation { .. }));
}

#[test]
fn congestion_unknown_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?congestion=fast").unwrap_err();
    assert!(matches!(e, SrtUrlError::OptionValidation { .. }));
}

#[test]
fn packetfilter_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?packetfilter=fec,cols:10,rows:5,arq:onreq").unwrap();
    assert!(u.config.packet_filter.is_some());
}

#[test]
fn pbkeylen_16() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=16").unwrap();
    assert!(matches!(
        u.config.key_length,
        Some(tst_srt::KeyLength::Aes128)
    ));
}

#[test]
fn pbkeylen_24() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=24").unwrap();
    assert!(matches!(
        u.config.key_length,
        Some(tst_srt::KeyLength::Aes192)
    ));
}

#[test]
fn pbkeylen_32() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=32").unwrap();
    assert!(matches!(
        u.config.key_length,
        Some(tst_srt::KeyLength::Aes256)
    ));
}

#[test]
fn pbkeylen_invalid() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?pbkeylen=15").unwrap_err();
    assert!(matches!(e, SrtUrlError::OptionValidation { .. }));
}

#[test]
fn latency_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=200").unwrap();
    assert_eq!(u.config.latency, Some(Duration::from_millis(200)));
}

#[test]
fn latency_with_suffix_rejects() {
    // Strict-A: no "ms"/"s" suffixes.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=200ms").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { ref key, .. } if key == "latency"));
}

#[test]
fn latency_negative_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=-1").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { .. }));
}

#[test]
fn latency_overflow_rejects() {
    // 2^31 = 2147483648 — outside i32 range; libsrt SRTO_LATENCY is i32.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=2147483648").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { .. }));
}

#[test]
fn rcvlatency_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?rcvlatency=120").unwrap();
    assert_eq!(u.config.recv_latency, Some(Duration::from_millis(120)));
}

#[test]
fn peerlatency_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?peerlatency=80").unwrap();
    assert_eq!(u.config.peer_latency, Some(Duration::from_millis(80)));
}

#[test]
fn mss_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?mss=1400").unwrap();
    assert_eq!(u.config.mss, Some(1400));
}

#[test]
fn payloadsize_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?payloadsize=1316").unwrap();
    assert_eq!(u.config.payload_size, Some(1316));
}

#[test]
fn maxbw_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?maxbw=10000000").unwrap();
    assert!(matches!(
        u.config.max_bandwidth,
        Some(tst_srt::MaxBandwidth::Limited(10_000_000))
    ));
}

#[test]
fn inputbw_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?inputbw=5000000").unwrap();
    assert_eq!(u.config.input_bandwidth, Some(5_000_000));
}

#[test]
fn oheadbw_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?oheadbw=25").unwrap();
    assert_eq!(u.config.overhead_bandwidth_pct, Some(25));
}

#[test]
fn oheadbw_out_of_range() {
    // Builder enforces 5..=100 in apply_socket_config; URL parser rejects
    // here at value-conversion since we know the bound.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?oheadbw=4").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { .. }));
    let e = SrtUrl::parse("srt://1.2.3.4:9000?oheadbw=101").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { .. }));
}

#[test]
fn lossmaxttl_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?lossmaxttl=20").unwrap();
    assert_eq!(u.config.loss_max_ttl, Some(20));
}

#[test]
fn fc_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?fc=8192").unwrap();
    assert_eq!(u.config.flow_window_packets, Some(8192));
}

#[test]
fn tlpktdrop_zero() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=0").unwrap();
    assert_eq!(u.config.too_late_packet_drop, Some(false));
}

#[test]
fn tlpktdrop_one() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=1").unwrap();
    assert_eq!(u.config.too_late_packet_drop, Some(true));
}

#[test]
fn tlpktdrop_true_rejects() {
    // Strict-A: BOOL is "0"/"1" only.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=true").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { ref key, .. } if key == "tlpktdrop"));
}

#[test]
fn tlpktdrop_two_rejects() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?tlpktdrop=2").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { .. }));
}

#[test]
fn x_recvtimeout_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?x-recvtimeout=5000").unwrap();
    assert_eq!(u.config.recv_timeout, Some(Duration::from_millis(5000)));
}

#[test]
fn x_sendtimeout_query() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?x-sendtimeout=2000").unwrap();
    assert_eq!(u.config.send_timeout, Some(Duration::from_millis(2000)));
}

#[test]
fn x_unknown_extension_rejects() {
    // x- prefix is reserved but not a free-for-all.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?x-foo=bar").unwrap_err();
    assert!(matches!(e, SrtUrlError::UnknownKey { ref key } if key == "x-foo"));
}

#[test]
fn mode_caller_accepted_noop() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?mode=caller&latency=100").unwrap();
    assert_eq!(u.config.latency, Some(Duration::from_millis(100)));
}

#[test]
fn mode_rendezvous_rejected() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?mode=rendezvous").unwrap_err();
    assert!(matches!(e, SrtUrlError::UnsupportedMode { ref mode } if mode == "rendezvous"));
}

#[test]
fn group1_conntimeo_honored() {
    // `conntimeo` was promoted from Group 3 (rejected) to Group 1 (honored).
    let u = SrtUrl::parse("srt://1.2.3.4:9000?conntimeo=5000").unwrap();
    assert_eq!(
        u.config.connect_timeout,
        Some(std::time::Duration::from_millis(5000))
    );
}

#[test]
fn group3_transtype_rejected() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?transtype=live").unwrap_err();
    let SrtUrlError::UnsupportedKey { key, srto } = e else {
        panic!("wrong variant");
    };
    assert_eq!(key, "transtype");
    assert_eq!(srto, "SRTO_TRANSTYPE");
}

#[test]
fn group3_rcvbuf_rejected() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?rcvbuf=1048576").unwrap_err();
    let SrtUrlError::UnsupportedKey { key, srto } = e else {
        panic!("wrong variant");
    };
    assert_eq!(key, "rcvbuf");
    assert_eq!(srto, "SRTO_RCVBUF");
}

#[test]
fn group3_sndbuf_rejected() {
    let e = SrtUrl::parse("srt://1.2.3.4:9000?sndbuf=1048576").unwrap_err();
    assert!(matches!(e, SrtUrlError::UnsupportedKey { ref key, .. } if key == "sndbuf"));
}

#[test]
fn unknown_typo_rejected() {
    // Distinct from UnsupportedKey: this name is not in the libsrt
    // vocabulary at all (typo of "latency").
    let e = SrtUrl::parse("srt://1.2.3.4:9000?lattency=100").unwrap_err();
    assert!(matches!(e, SrtUrlError::UnknownKey { ref key } if key == "lattency"));
}

/// Smoke test that all Group 3 keys reject with a non-empty `srto`.
/// Catches drift if someone forgets to fill in the SRTO_* string.
#[test]
fn all_group3_keys_reject_with_srto() {
    let group3 = [
        "bindtodevice",
        "cryptomode",
        "drifttracer",
        "enforcedencryption",
        "groupconnect",
        "groupminstabletimeo",
        "iptos",
        "ipttl",
        "ipv6only",
        "kmpreannounce",
        "kmrefreshrate",
        "maxrexmitbw",
        "messageapi",
        "mininputbw",
        "minversion",
        "nakreport",
        "peeridletimeo",
        "rcvbuf",
        "retransmitalgo",
        "sndbuf",
        "snddropdelay",
        "transtype",
        "tsbpdmode",
    ];
    for key in group3 {
        let url = format!("srt://1.2.3.4:9000?{key}=1");
        let e = SrtUrl::parse(&url).unwrap_err();
        match e {
            SrtUrlError::UnsupportedKey { key: k, srto } => {
                assert_eq!(k, key, "wrong key in error");
                assert!(!srto.is_empty(), "{key}: srto must not be empty");
                assert!(srto.starts_with("SRTO_"), "{key}: srto must be SRTO_*");
            }
            other => panic!("{key}: expected UnsupportedKey, got {other:?}"),
        }
    }
}

#[test]
fn last_occurrence_wins_on_duplicate_keys() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=100&latency=200").unwrap();
    assert_eq!(u.config.latency, Some(Duration::from_millis(200)));
}

#[test]
fn adapter_rejected_as_unsupported() {
    // "adapter" is not in libsrt's vocabulary table per the apps source,
    // and it has no SocketBuilder counterpart in this library. Reject as
    // UnknownKey.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?adapter=192.168.1.5").unwrap_err();
    assert!(matches!(e, SrtUrlError::UnknownKey { ref key } if key == "adapter"));
}

#[test]
fn multi_key_url_combines() {
    let u = SrtUrl::parse(
        "srt://camera.local:9000?streamid=front&latency=200&passphrase=hunter-too-long&pbkeylen=24",
    )
    .unwrap();
    assert_eq!(u.host, "camera.local");
    assert_eq!(u.port, 9000);
    assert_eq!(u.config.stream_id.as_ref().unwrap().as_str(), "front");
    assert_eq!(u.config.latency, Some(Duration::from_millis(200)));
    assert!(u.config.passphrase.is_some());
    assert!(matches!(
        u.config.key_length,
        Some(tst_srt::KeyLength::Aes192)
    ));
}

#[test]
fn empty_value_rejected() {
    // Strict-A: empty INT can't parse.
    let e = SrtUrl::parse("srt://1.2.3.4:9000?latency=").unwrap_err();
    assert!(matches!(e, SrtUrlError::InvalidValue { ref key, .. } if key == "latency"));
}

#[test]
fn invalid_percent_encoding_does_not_panic() {
    // The common parser is strict: a truncated percent-escape like "%2" is
    // rejected with BadPercentEncoding rather than passed through. The test
    // asserts only that no panic occurs — an Err result is fine here.
    let _ = SrtUrl::parse("srt://1.2.3.4:9000?streamid=%2");
}

#[test]
fn passphrase_with_plus_sign_is_literal_plus() {
    // The common parser does NOT decode `+` as space — only `%XX` sequences
    // are decoded. A literal `+` in an SRT URL value stays as `+`. To embed
    // a `+` in a passphrase, percent-encode it as `%2B` (which this test
    // exercises); a raw `+` would also arrive as `+` unchanged.
    let u = SrtUrl::parse("srt://1.2.3.4:9000?passphrase=hunter%2Btoo%2Blong").unwrap();
    assert_eq!(
        u.config.passphrase.as_ref().unwrap().as_str(),
        "hunter+too+long"
    );
}

#[test]
fn merge_from_writes_the_urls_keys_through() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=200&streamid=front&pbkeylen=24").unwrap();
    let mut cfg = SrtSocketConfig::default();
    cfg.merge_from(&u.config);
    assert_eq!(cfg.latency, Some(Duration::from_millis(200)));
    assert_eq!(cfg.stream_id.as_ref().unwrap().as_str(), "front");
    assert!(matches!(cfg.key_length, Some(tst_srt::KeyLength::Aes192)));
}

#[test]
fn merge_from_url_wins_over_existing() {
    // Pre-populate cfg with a builder value; the URL's key should overwrite.
    let mut cfg = SrtSocketConfig::default();
    cfg.latency = Some(Duration::from_millis(100));
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=200").unwrap();
    cfg.merge_from(&u.config);
    assert_eq!(cfg.latency, Some(Duration::from_millis(200)));
}

#[test]
fn merge_from_does_not_clear_unset_fields() {
    // The URL has only latency set; other pre-populated fields stay.
    let mut cfg = SrtSocketConfig::default();
    cfg.mss = Some(1316);
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=100").unwrap();
    cfg.merge_from(&u.config);
    assert_eq!(cfg.latency, Some(Duration::from_millis(100)));
    assert_eq!(cfg.mss, Some(1316)); // preserved
}

#[test]
fn merge_from_x_recvtimeout_lands() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?x-recvtimeout=5000").unwrap();
    let mut cfg = SrtSocketConfig::default();
    cfg.merge_from(&u.config);
    assert_eq!(cfg.recv_timeout, Some(Duration::from_millis(5000)));
}

#[test]
fn url_config_holds_only_the_urls_keys() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=100").unwrap();
    assert_eq!(u.config.latency, Some(Duration::from_millis(100)));
    assert!(u.config.connect_timeout.is_none());
    assert!(u.config.linger.is_none());
    assert!(u.config.key_length.is_none());
}

#[test]
fn listener_config_carries_the_url_as_its_socket_options() {
    let u = SrtUrl::parse("srt://1.2.3.4:9000?latency=100&passphrase=hunter-too-long").unwrap();
    let cfg: SrtListenerConfig = u.listener_config();
    assert_eq!(cfg.socket.latency, Some(Duration::from_millis(100)));
    assert!(cfg.socket.passphrase.is_some());
    assert_eq!(cfg.backlog, SrtListenerConfig::default().backlog);
    assert_eq!(cfg.reuse_addr, SrtListenerConfig::default().reuse_addr);
}
