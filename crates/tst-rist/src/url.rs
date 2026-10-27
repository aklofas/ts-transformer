//! Parsing of `rist://` URLs.
//!
//! Supported forms:
//! - `rist://host:port` — sender (unicast UDP; Main Profile unless
//!   `?profile=simple`)
//! - `rist://@host:port` — receiver bind (ffmpeg `@` convention)
//! - `rist://239.x.x.x:port` — multicast sender
//! - Query params: `profile`, `buffer`, `aes-type`, `secret`, `cname`,
//!   `recovery_maxbitrate` (alias `bandwidth`), `session_timeout`,
//!   `compression`, `pkt_size`. `bandwidth` and `recovery_maxbitrate` set the same
//!   librist field (`recovery_maxbitrate`, the cap on payload plus
//!   retransmissions in kbps); giving both with different values is a parse error.

use std::net::IpAddr;
use std::time::Duration;

use tst_core::url::UrlError;
use tst_core::url::common::{parse_bool, parse_int, parse_size, parse_url};
use tst_core::url::query::{QueryKey, UnknownKeys, apply_query};

use crate::config::{EncryptionKey, RistConfig, RistProfile, RistSecret};

/// Parsed `rist://` URL.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RistUrl {
    pub addr: IpAddr,
    pub port: u16,
    /// True if the URL had `@` prefix (recv-bind intent, ffmpeg convention).
    pub is_recv_bind: bool,
    /// The settings the URL's query carried, over the defaults of
    /// [`RistConfig`]. A `secret` sets [`RistConfig::encryption`] and
    /// promotes the profile to Main.
    pub config: RistConfig,
    /// `?aes-type=` until the end of the parse, when it sizes the key of a
    /// `?secret=` (the two keys arrive in either order).
    aes_bits: Option<u32>,
}

type Applied = Result<(), UrlError>;

fn set_profile(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    u.config.profile = match value {
        "simple" => RistProfile::Simple,
        "main" => RistProfile::Main,
        other => {
            return Err(UrlError::BadQueryValue {
                key: key.to_string(),
                value: other.to_string(),
                detail: "expected one of: simple, main".into(),
            });
        }
    };
    Ok(())
}

fn set_bandwidth(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    u.config.bandwidth_kbps = Some(parse_int(key, value)?);
    Ok(())
}

fn set_buffer(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    let ms: u32 = parse_int(key, value)?;
    u.config.buffer = Duration::from_millis(ms as u64);
    Ok(())
}

fn set_aes_type(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    let n: u32 = parse_int(key, value)?;
    match n {
        128 | 192 | 256 => {
            u.aes_bits = Some(n);
            Ok(())
        }
        _ => Err(UrlError::Rejected(format!(
            "aes-type must be 128, 192, or 256; got {n}"
        ))),
    }
}

fn set_secret(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    if value.is_empty() {
        return Err(UrlError::BadQueryValue {
            key: key.to_string(),
            value: String::new(),
            detail: "must be non-empty".into(),
        });
    }
    // librist's own default key size for a secret without `aes-type`; an
    // `aes-type` resizes it when the parse ends.
    u.config.encryption = Some(EncryptionKey {
        size_bits: 256,
        secret: RistSecret::new(value),
        rotation: 0,
    });
    Ok(())
}

fn set_cname(u: &mut RistUrl, _key: &str, value: &str) -> Applied {
    u.config.cname = Some(value.to_string());
    Ok(())
}

fn set_recovery_maxbitrate(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    u.config.recovery_maxbitrate_kbps = Some(parse_int(key, value)?);
    Ok(())
}

fn set_session_timeout(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    let ms: u32 = parse_int(key, value)?;
    u.config.session_timeout = Some(Duration::from_millis(ms as u64));
    Ok(())
}

fn set_compression(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    u.config.compression = parse_bool(key, value)?;
    Ok(())
}

fn set_pkt_size(u: &mut RistUrl, key: &str, value: &str) -> Applied {
    u.config.pkt_size = parse_size(key, value)?;
    Ok(())
}

/// The query keys a `rist://` URL accepts; the parser and the generated key
/// documentation both read this table.
#[doc(hidden)]
pub const URL_KEYS: &[QueryKey<RistUrl>] = &[
    QueryKey::new(
        "profile",
        &[],
        "RIST profile",
        "`simple` or `main`",
        set_profile,
    ),
    QueryKey::new(
        "recovery_maxbitrate",
        &["recovery-maxbitrate"],
        "Retransmit bandwidth cap",
        "kilobits per second",
        set_recovery_maxbitrate,
    ),
    QueryKey::new(
        "bandwidth",
        &[],
        "Alias of `recovery_maxbitrate`; a different value is an error",
        "kilobits per second",
        set_bandwidth,
    ),
    QueryKey::new("buffer", &[], "Recovery buffer", "milliseconds", set_buffer),
    QueryKey::new(
        "session_timeout",
        &["session-timeout"],
        "Session timeout",
        "milliseconds",
        set_session_timeout,
    ),
    QueryKey::new(
        "aes-type",
        &[],
        "Encryption key size",
        "128, 192 or 256; needs `secret`",
        set_aes_type,
    ),
    QueryKey::new(
        "secret",
        &[],
        "Encryption passphrase",
        "non-empty text; alone it means AES-256",
        set_secret,
    ),
    QueryKey::new("cname", &[], "RIST canonical name", "text", set_cname),
    QueryKey::new(
        "compression",
        &[],
        "NULL-packet compression",
        "boolean",
        set_compression,
    ),
    QueryKey::new(
        "pkt_size",
        &[],
        "Largest payload one send accepts (1..=9968)",
        "bytes with the `K`/`M` forms",
        set_pkt_size,
    ),
];

impl RistUrl {
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let parsed =
            parse_url(s).map_err(|e| UrlError::Rejected(format!("URL parse failed: {e}")))?;
        if parsed.scheme != "rist" {
            return Err(UrlError::BadScheme {
                expected: "'rist'",
                got: parsed.scheme.to_string(),
            });
        }
        let port = parsed.port.ok_or(UrlError::MissingPort)?;

        // ffmpeg convention: `@host` marks recv-bind intent. parse_url stores
        // the `@` chunk in the username slot; same trick as tst-udp / tst-tcp.
        let is_recv_bind = parsed.username.is_some();

        let host_str = parsed.host;
        let addr: IpAddr = host_str.parse().map_err(|_| {
            UrlError::BadHost(format!(
                "host '{host_str}' is not a literal IPv4/IPv6 address"
            ))
        })?;

        let mut url = Self {
            addr,
            port,
            is_recv_bind,
            config: RistConfig::default(),
            aes_bits: None,
        };
        // Unknown params are silently ignored.
        apply_query(URL_KEYS, &mut url, &parsed.query, UnknownKeys::Ignore)?;

        // `bandwidth` is an alias of `recovery_maxbitrate` (both set librist's
        // `recovery_maxbitrate`, the cap on payload plus retransmissions). Two different
        // values is a contradiction, not a precedence question.
        if let (Some(bw), Some(rm)) = (
            url.config.bandwidth_kbps,
            url.config.recovery_maxbitrate_kbps,
        ) && bw != rm
        {
            return Err(UrlError::BadQueryValue {
                key: "bandwidth".to_string(),
                value: bw.to_string(),
                detail: format!(
                    "conflicts with recovery_maxbitrate={rm}; `bandwidth` is an alias of \
                         `recovery_maxbitrate` (librist recovery_maxbitrate) — give one, or equal values"
                ),
            });
        }

        match (url.aes_bits.take(), &mut url.config.encryption) {
            (Some(_), None) => {
                return Err(UrlError::Rejected(
                    "aes-type given without secret; aes-type only selects the key size for a secret (a secret without aes-type defaults to AES-256)".into(),
                ));
            }
            (Some(bits), Some(key)) => key.size_bits = bits,
            _ => {}
        }
        // A key needs the Main profile.
        if url.config.encryption.is_some() {
            url.config.profile = RistProfile::Main;
        }

        Ok(url)
    }
}

/// Render the endpoint librist parses (`rist_parse_address2`). `SocketAddr`'s
/// `Display` brackets IPv6, which librist's `udpsocket_parse_url` requires —
/// a bare `::1:9000` splits at the first colon into host "" + port 0.
pub(crate) fn native_endpoint(url: &RistUrl, bind: bool) -> String {
    let sa = std::net::SocketAddr::new(url.addr, url.port);
    if bind {
        format!("rist://@{sa}")
    } else {
        format!("rist://{sa}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_sender() {
        let u = RistUrl::parse("rist://1.2.3.4:8000").unwrap();
        assert!(!u.is_recv_bind);
        assert_eq!(u.port, 8000);
    }

    #[test]
    fn recv_bind_via_at_prefix() {
        let u = RistUrl::parse("rist://@0.0.0.0:8000").unwrap();
        assert!(u.is_recv_bind);
    }

    #[test]
    fn full_query_chain() {
        let u = RistUrl::parse(
            "rist://1.2.3.4:8000?profile=main&bandwidth=10000&buffer=200&aes-type=256&secret=topsecret&cname=uav-12"
        ).unwrap();
        assert_eq!(u.config.profile, RistProfile::Main);
        assert_eq!(u.config.bandwidth_kbps, Some(10000));
        assert_eq!(u.config.buffer, Duration::from_millis(200));
        assert_eq!(
            u.config
                .encryption
                .as_ref()
                .map(|k| (k.size_bits, k.secret.expose())),
            Some((256, "topsecret"))
        );
        assert_eq!(u.config.cname.as_deref(), Some("uav-12"));
    }

    #[test]
    fn debug_does_not_leak_secret() {
        // The PSK is a redacting newtype, so the derived Debug of RistUrl (and
        // of the secret field itself) must NOT print the plaintext. This would
        // fail if `secret` were a plain `String` (derived Debug leaks it).
        let u = RistUrl::parse("rist://1.2.3.4:8000?aes-type=256&secret=topsecret").unwrap();
        let dbg = format!("{u:?}");
        assert!(
            !dbg.contains("topsecret"),
            "RistUrl Debug leaked the PSK: {dbg}"
        );
        let secret_dbg = format!("{:?}", u.config.encryption);
        assert!(
            !secret_dbg.contains("topsecret"),
            "RistSecret Debug leaked the PSK: {secret_dbg}"
        );
        // The plaintext is still reachable via the explicit accessor.
        assert_eq!(
            u.config.encryption.as_ref().map(|k| k.secret.expose()),
            Some("topsecret")
        );
    }

    #[test]
    fn rejects_invalid_aes_type() {
        let err = RistUrl::parse("rist://1.2.3.4:8000?aes-type=64");
        assert!(
            matches!(err, Err(UrlError::Rejected(ref m)) if m == "aes-type must be 128, 192, or 256; got 64")
        );
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(matches!(
            RistUrl::parse("udp://host:8000"),
            Err(UrlError::BadScheme { .. })
        ));
    }

    #[test]
    fn secret_alone_defaults_aes_type_to_256() {
        let u = RistUrl::parse("rist://127.0.0.1:9000?secret=example").unwrap();
        assert_eq!(u.config.encryption.as_ref().map(|k| k.size_bits), Some(256));
    }

    #[test]
    fn aes_type_without_secret_is_rejected() {
        let e = RistUrl::parse("rist://127.0.0.1:9000?aes-type=256").unwrap_err();
        assert!(
            matches!(&e, UrlError::Rejected(m) if m.starts_with("aes-type given without secret")),
            "got {e:?}"
        );
    }

    #[test]
    fn malformed_escape_never_echoes_the_secret() {
        for url in [
            "rist://127.0.0.1:9000?secret=example-secret%",
            "rist://127.0.0.1:9000?secret=example-secret%zz",
            "rist://127.0.0.1:9000?secret=example-secret%FF",
        ] {
            let e = RistUrl::parse(url).unwrap_err();
            for text in [e.to_string(), format!("{e:?}")] {
                assert!(!text.contains("example-secret"), "{text}");
                assert!(!text.contains('z'), "{text}");
                assert!(text.contains("secret"), "{text}");
            }
        }
    }

    #[test]
    fn empty_secret_is_rejected() {
        let e = RistUrl::parse("rist://127.0.0.1:9000?secret=").unwrap_err();
        assert!(
            matches!(e, UrlError::BadQueryValue { ref key, .. } if key == "secret"),
            "got {e:?}"
        );
    }

    #[test]
    fn native_endpoint_brackets_ipv6() {
        let u = RistUrl::parse("rist://[::1]:9000").unwrap();
        assert_eq!(native_endpoint(&u, false), "rist://[::1]:9000");
        assert_eq!(native_endpoint(&u, true), "rist://@[::1]:9000");
        let v4 = RistUrl::parse("rist://127.0.0.1:9000").unwrap();
        assert_eq!(native_endpoint(&v4, false), "rist://127.0.0.1:9000");
        assert_eq!(native_endpoint(&v4, true), "rist://@127.0.0.1:9000");
    }

    /// `bandwidth` and `recovery_maxbitrate` both write librist's
    /// `recovery_maxbitrate`. Different values must be a parse error, not
    /// last-writer-wins.
    #[test]
    fn conflicting_bandwidth_and_recovery_maxbitrate_rejected() {
        match RistUrl::parse("rist://1.2.3.4:5?bandwidth=1000&recovery_maxbitrate=5000") {
            Err(UrlError::BadQueryValue { key, value, detail }) => {
                assert_eq!(key, "bandwidth");
                assert_eq!(value, "1000");
                assert!(
                    detail.contains("recovery_maxbitrate=5000"),
                    "detail: {detail}"
                );
            }
            other => panic!("expected BadQueryValue for the conflict, got {other:?}"),
        }
        // Order-independent.
        assert!(matches!(
            RistUrl::parse("rist://1.2.3.4:5?recovery_maxbitrate=5000&bandwidth=1000"),
            Err(UrlError::BadQueryValue { .. })
        ));
    }

    #[test]
    fn equal_bandwidth_and_recovery_maxbitrate_accepted() {
        let u = RistUrl::parse("rist://1.2.3.4:5?bandwidth=4000&recovery-maxbitrate=4000").unwrap();
        assert_eq!(u.config.bandwidth_kbps, Some(4000));
        assert_eq!(u.config.recovery_maxbitrate_kbps, Some(4000));
    }

    #[test]
    fn aes_type_and_secret_combine_in_either_order() {
        for q in ["aes-type=128&secret=s", "secret=s&aes-type=128"] {
            let u = RistUrl::parse(&format!("rist://1.2.3.4:8000?{q}")).unwrap();
            let k = u.config.encryption.expect("a key");
            assert_eq!((k.size_bits, k.secret.expose()), (128, "s"));
            assert_eq!(u.config.profile, RistProfile::Main);
        }
    }

    #[test]
    fn secret_promotes_simple_profile_in_either_order() {
        for q in ["profile=simple&secret=x", "secret=x&profile=simple"] {
            let u = RistUrl::parse(&format!("rist://1.2.3.4:8000?{q}")).unwrap();
            assert_eq!(u.config.profile, RistProfile::Main, "{q}");
        }
    }

    #[test]
    fn pkt_size_is_a_byte_size() {
        let u = RistUrl::parse("rist://1.2.3.4:8000?pkt_size=1K").unwrap();
        assert_eq!(u.config.pkt_size, 1024);
        let e = RistUrl::parse("rist://1.2.3.4:8000?pkt_size=big").unwrap_err();
        assert!(matches!(e, UrlError::BadQueryValue { ref key, .. } if key == "pkt_size"));
    }
}
