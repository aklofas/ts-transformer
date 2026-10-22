use tst_core::url::UrlError;
use tst_webrtc::url::WhepUrl;

#[test]
fn plain_defaults() {
    let u = WhepUrl::parse("whep://0.0.0.0:8889").unwrap();
    assert_eq!(
        (
            u.port,
            u.media_port,
            u.max_viewers,
            u.tls,
            u.cors_origin.as_str()
        ),
        (8889, 8889, 10, false, "*")
    );
    assert!(u.advertise.is_none() && u.token.is_none());
    assert!(u.cert.is_none() && u.key.is_none());
}

#[test]
fn full_query() {
    let u = WhepUrl::parse(
        "wheps://[::]:8443/?media_port=50000&advertise=203.0.113.5&max_viewers=4\
         &token=s3cret&cors_origin=https://ops.example&cert=/c.pem&key=/k.pem",
    )
    .unwrap();
    assert!(u.tls);
    assert_eq!(u.addr.to_string(), "::");
    assert_eq!(u.port, 8443);
    assert_eq!(u.media_port, 50000);
    assert_eq!(u.advertise.unwrap().to_string(), "203.0.113.5");
    assert_eq!(u.max_viewers, 4);
    assert_eq!(u.token.as_deref(), Some("s3cret"));
    assert_eq!(u.cors_origin, "https://ops.example");
    assert_eq!(u.cert.as_deref(), Some("/c.pem"));
    assert_eq!(u.key.as_deref(), Some("/k.pem"));
}

#[test]
fn wheps_without_keys_is_refused() {
    for s in [
        "wheps://192.0.2.10:8443",
        "wheps://192.0.2.10:8443?cert=/c.pem",
        "wheps://192.0.2.10:8443?key=/k.pem",
    ] {
        match WhepUrl::parse(s) {
            Err(UrlError::BadQueryValue { key, .. }) => assert_eq!(key, "cert", "{s}"),
            other => panic!("{s}: expected BadQueryValue {{ key: \"cert\" }}, got {other:?}"),
        }
    }
}

#[test]
fn hostname_is_refused() {
    assert!(matches!(
        WhepUrl::parse("whep://localhost:8889"),
        Err(UrlError::BadHost(_))
    ));
}

#[test]
fn unknown_key_is_refused() {
    assert!(matches!(
        WhepUrl::parse("whep://192.0.2.10:8889?stun=x"),
        Err(UrlError::UnknownQueryKey { key }) if key == "stun"
    ));
}

#[test]
fn max_viewers_bounds() {
    for v in ["0", "65", "x"] {
        assert!(matches!(
            WhepUrl::parse(&format!("whep://192.0.2.10:1?max_viewers={v}")),
            Err(UrlError::BadQueryValue { key, .. }) if key == "max_viewers"
        ));
    }
    assert_eq!(
        WhepUrl::parse("whep://192.0.2.10:1?max_viewers=64")
            .unwrap()
            .max_viewers,
        64
    );
}

#[test]
fn a_mount_path_in_the_bind_url_is_refused() {
    assert!(matches!(
        WhepUrl::parse("whep://192.0.2.10:1/cam"),
        Err(UrlError::BadQueryValue { key, .. }) if key == "path"
    ));
    assert!(WhepUrl::parse("whep://192.0.2.10:1/").is_ok());
}

#[test]
fn debug_output_redacts_the_token() {
    let u = WhepUrl::parse("whep://0.0.0.0:8889?token=s3cret").unwrap();
    let shown = format!("{u:?}");
    assert!(!shown.contains("s3cret"), "{shown}");
    assert!(shown.contains("token: Some(\"<redacted>\")"), "{shown}");
    let none = format!("{:?}", WhepUrl::parse("whep://0.0.0.0:8889").unwrap());
    assert!(none.contains("token: None"), "{none}");
}

#[test]
fn an_empty_token_is_refused() {
    let err = WhepUrl::parse("whep://0.0.0.0:8889?token=").unwrap_err();
    assert!(
        matches!(&err, UrlError::BadQueryValue { key, value, .. } if key == "token" && value.is_empty()),
        "{err:?}"
    );
}

#[test]
fn cert_and_key_on_a_plaintext_whep_url_are_refused() {
    for s in [
        "whep://192.0.2.10:8889?cert=/c.pem",
        "whep://192.0.2.10:8889?key=/k.pem",
        "whep://192.0.2.10:8889?cert=/c.pem&key=/k.pem",
    ] {
        match WhepUrl::parse(s) {
            Err(UrlError::BadQueryValue { key, detail, .. }) => {
                assert!(key == "cert" || key == "key", "{s}: {key}");
                assert!(detail.contains("plaintext"), "{detail}");
            }
            other => panic!("{s}: expected BadQueryValue, got {other:?}"),
        }
    }
}

#[test]
fn validate_accepts_a_parsed_url_and_refuses_edited_ones() {
    let ok = WhepUrl::parse("whep://0.0.0.0:8889").unwrap();
    assert_eq!(ok.validate(), Ok(()));
    let mut edited = ok.clone();
    edited.max_viewers = 0;
    assert!(
        matches!(edited.validate(), Err(UrlError::BadQueryValue { key, .. }) if key == "max_viewers")
    );
    edited.max_viewers = 65;
    assert!(edited.validate().is_err());
    let mut edited = ok.clone();
    edited.token = Some(String::new());
    assert!(
        matches!(edited.validate(), Err(UrlError::BadQueryValue { key, .. }) if key == "token")
    );
    let mut edited = ok.clone();
    edited.tls = true;
    assert!(edited.validate().is_err());
    edited.cert = Some("c.pem".into());
    assert!(edited.validate().is_err());
    edited.key = Some("k.pem".into());
    assert_eq!(edited.validate(), Ok(()));
}
