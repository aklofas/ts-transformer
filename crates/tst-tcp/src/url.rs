//! Parsing of `tcp://` and `tcps://` URLs.
//!
//! - `tcp://10.0.0.5:9000` — plain TCP caller (IP literal)
//! - `tcp://relay.example.com:9000` — plain TCP caller (hostname)
//! - `tcps://10.0.0.5:9000` — TLS caller (IP literal; certificate must have `iPAddress` SAN)
//! - `tcps://relay.example.com:9000` — TLS caller (hostname; certificate must have `dnsName` SAN)
//! - `tcp://0.0.0.0:port?listen=1` — listener (plain; IP literal required)
//! - `tcps://0.0.0.0:port?listen=1&cert=...&key=...` — listener (TLS; IP literal required)
//!
//! ## Caller URLs — IP literals and hostnames
//!
//! Caller URLs (no `?listen=1`) accept both IPv4/IPv6 literals and DNS
//! hostnames. Resolution happens at connect time, never at parse time.
//! For `tcps://`, TLS presents whatever name you dialed as the
//! SNI and verifies the server certificate against it:
//!
//! - If you dial an IP literal, the certificate must carry a matching
//!   `iPAddress` SubjectAltName (SAN).
//! - If you dial a hostname, the certificate must carry a matching `dnsName`
//!   SAN.
//!
//! ## Listener URLs — IP literals required
//!
//! Listener URLs (`?listen=1`) must use an IP literal because the OS must bind
//! a socket to a specific address. Pass `0.0.0.0` (IPv4) or `::` (IPv6) to
//! listen on all interfaces. A hostname in a listener URL is rejected at parse
//! time with [`UrlError::BadHost`].

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use tst_core::url::UrlError;
use tst_core::url::common::{parse_bool, parse_int, parse_size, parse_url};
use tst_core::url::query::{QueryKey, UnknownKeys, apply_query};

use crate::config::TcpConfig;

/// Parsed TCP URL.
#[derive(Debug, Clone)]
pub struct TcpUrl {
    /// Destination host as written in the URL — an IPv4/IPv6 literal or a
    /// DNS hostname. Caller URLs resolve it at connect time and (for
    /// `tcps://`) present it verbatim for SNI/certificate verification.
    /// Listener URLs (`?listen=1`) require an IP literal (socket bind).
    pub host: String,
    /// Port.
    pub port: u16,
    /// True for `tcps://`.
    pub tls: bool,
    /// True if the URL had `?listen=1` (listener intent).
    pub listen: bool,
    /// The socket and TLS settings the URL's query carried, over the
    /// defaults of [`TcpConfig`].
    pub config: TcpConfig,
}

type Applied = Result<(), UrlError>;

fn set_listen(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.listen = parse_bool(key, value)?;
    Ok(())
}

fn set_nodelay(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.config.nodelay = Some(parse_bool(key, value)?);
    Ok(())
}

fn set_keepalive(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.config.keepalive = Some(Duration::from_secs(parse_int(key, value)?));
    Ok(())
}

fn set_rcvbuf(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.config.rcvbuf = Some(parse_size(key, value)?);
    Ok(())
}

fn set_sndbuf(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.config.sndbuf = Some(parse_size(key, value)?);
    Ok(())
}

fn set_pkt_size(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.config.pkt_size = Some(parse_size(key, value)?);
    Ok(())
}

fn set_connect_timeout(u: &mut TcpUrl, key: &str, value: &str) -> Applied {
    u.config.connect_timeout = Some(Duration::from_secs(parse_int(key, value)?));
    Ok(())
}

fn set_cert(u: &mut TcpUrl, _key: &str, value: &str) -> Applied {
    u.config.cert = Some(PathBuf::from(value));
    Ok(())
}

fn set_key(u: &mut TcpUrl, _key: &str, value: &str) -> Applied {
    u.config.key = Some(PathBuf::from(value));
    Ok(())
}

fn set_ca(u: &mut TcpUrl, _key: &str, value: &str) -> Applied {
    u.config.ca = Some(PathBuf::from(value));
    Ok(())
}

/// The query keys a `tcp://` or `tcps://` URL accepts; the parser and the
/// generated key documentation both read this table.
#[doc(hidden)]
pub const URL_KEYS: &[QueryKey<TcpUrl>] = &[
    QueryKey::new(
        "listen",
        &[],
        "Bind and accept instead of connecting",
        "boolean",
        set_listen,
    ),
    QueryKey::new(
        "nodelay",
        &[],
        "Disable Nagle's algorithm",
        "boolean",
        set_nodelay,
    ),
    QueryKey::new(
        "keepalive",
        &[],
        "`SO_KEEPALIVE` idle time",
        "seconds",
        set_keepalive,
    ),
    QueryKey::new(
        "rcvbuf",
        &[],
        "Kernel socket buffer size",
        "bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB",
        set_rcvbuf,
    ),
    QueryKey::new(
        "sndbuf",
        &[],
        "Kernel socket buffer size",
        "bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB",
        set_sndbuf,
    ),
    QueryKey::new(
        "pkt_size",
        &[],
        "Largest payload chunk one send accepts",
        "bytes with the `K`/`M` forms",
        set_pkt_size,
    ),
    QueryKey::new(
        "connect_timeout",
        &[],
        "Connect deadline",
        "seconds",
        set_connect_timeout,
    ),
    QueryKey::new(
        "cert",
        &[],
        "Server certificate",
        "PEM file path; a `tcps` listener",
        set_cert,
    ),
    QueryKey::new(
        "key",
        &[],
        "Server private key",
        "PEM file path; a `tcps` listener",
        set_key,
    ),
    QueryKey::new(
        "ca",
        &[],
        "Custom trust anchors for a caller (the system roots otherwise)",
        "PEM file path",
        set_ca,
    ),
];

/// The refusal for a listener URL whose host is not an IP literal.
pub(crate) fn listener_host_rejection() -> UrlError {
    UrlError::BadHost("listener host must be a literal IPv4/IPv6 address".to_string())
}

impl TcpUrl {
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let parsed =
            parse_url(s).map_err(|e| UrlError::Rejected(format!("URL parse failed: {e}")))?;
        let tls = match parsed.scheme {
            "tcp" => false,
            "tcps" => true,
            other => {
                return Err(UrlError::BadScheme {
                    expected: "'tcp' or 'tcps'",
                    got: other.to_string(),
                });
            }
        };
        let port = parsed.port.ok_or(UrlError::MissingPort)?;

        // tst-core's parse_url already strips IPv6 brackets from parsed.host
        // (e.g. `[::1]` → `::1`), so no bracket-stripping is needed here.
        let mut url = Self {
            host: parsed.host.to_string(),
            port,
            tls,
            listen: false,
            config: TcpConfig::default(),
        };

        // Unknown params are silently ignored. The query is applied before
        // the host check because `listen` decides whether the host must be
        // an IP literal.
        apply_query(URL_KEYS, &mut url, &parsed.query, UnknownKeys::Ignore)?;

        // Listeners bind a socket — the host must be an IP literal. Callers
        // accept hostnames: resolution happens at connect time and TLS uses
        // the name for SNI/verification.
        if url.listen && url.host.parse::<IpAddr>().is_err() {
            return Err(listener_host_rejection());
        }

        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_caller() {
        let u = TcpUrl::parse("tcp://192.168.1.5:7001").unwrap();
        assert!(!u.tls);
        assert!(!u.listen);
        assert_eq!(u.port, 7001);
        assert_eq!(u.host, "192.168.1.5");
    }

    #[test]
    fn tls_caller() {
        let u = TcpUrl::parse("tcps://192.168.1.5:7001").unwrap();
        assert!(u.tls);
        assert!(!u.listen);
        assert_eq!(u.host, "192.168.1.5");
    }

    #[test]
    fn plain_listener() {
        let u = TcpUrl::parse("tcp://0.0.0.0:7001?listen=1").unwrap();
        assert!(!u.tls);
        assert!(u.listen);
        assert_eq!(u.host, "0.0.0.0");
    }

    #[test]
    fn tls_listener_with_cert_key() {
        let u =
            TcpUrl::parse("tcps://0.0.0.0:7001?listen=1&cert=server.crt&key=server.key").unwrap();
        assert!(u.tls);
        assert!(u.listen);
        assert_eq!(
            u.config.cert.as_deref(),
            Some(std::path::Path::new("server.crt"))
        );
        assert_eq!(
            u.config.key.as_deref(),
            Some(std::path::Path::new("server.key"))
        );
    }

    #[test]
    fn byte_size_suffix_overflow_is_rejected_not_panic() {
        // The "M" multiply must not panic in debug or wrap in release.
        let err = TcpUrl::parse("tcp://1.2.3.4:7001?rcvbuf=999999999999999999999M")
            .expect_err("overflowing rcvbuf must be rejected");
        assert!(matches!(err, UrlError::BadQueryValue { .. }));
    }

    #[test]
    fn byte_size_enormous_but_valid_is_rejected_by_bound() {
        // 999999M ≈ 1 TiB: valid digits + recognized suffix, but absurd.
        let err = TcpUrl::parse("tcp://1.2.3.4:7001?sndbuf=999999M")
            .expect_err("absurd sndbuf must exceed the byte-size ceiling");
        match err {
            UrlError::BadQueryValue { detail, .. } => {
                assert!(detail.contains("exceeds maximum"), "detail: {detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn byte_size_at_ceiling_is_accepted() {
        let u = TcpUrl::parse("tcp://1.2.3.4:7001?rcvbuf=256M").unwrap();
        assert_eq!(u.config.rcvbuf, Some(256 * 1024 * 1024));
    }

    #[test]
    fn cited_adversarial_input_returns_err() {
        // The exact adversarial fixture: `999999999999G`. Rejected (never
        // panic/wrap).
        assert!(TcpUrl::parse("tcp://1.2.3.4:7001?rcvbuf=999999999999G").is_err());
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(matches!(
            TcpUrl::parse("udp://host:7001"),
            Err(UrlError::BadScheme { .. })
        ));
    }

    #[test]
    fn rejects_missing_port() {
        assert!(matches!(
            TcpUrl::parse("tcp://1.2.3.4"),
            Err(UrlError::MissingPort)
        ));
    }

    #[test]
    fn knobs_parse() {
        let u = TcpUrl::parse(
            "tcp://1.2.3.4:7001?nodelay=1&keepalive=30&rcvbuf=8M&sndbuf=2M&connect_timeout=10",
        )
        .unwrap();
        assert_eq!(u.config.nodelay, Some(true));
        assert_eq!(u.config.keepalive, Some(Duration::from_secs(30)));
        assert_eq!(u.config.rcvbuf, Some(8 * 1024 * 1024));
        assert_eq!(u.config.sndbuf, Some(2 * 1024 * 1024));
        assert_eq!(u.config.connect_timeout, Some(Duration::from_secs(10)));
    }

    #[test]
    fn hostname_caller_accepted() {
        let u = TcpUrl::parse("tcp://relay.example.com:7001").unwrap();
        assert_eq!(u.host, "relay.example.com");
        assert!(!u.tls);
        let u = TcpUrl::parse("tcps://relay.example.com:7001").unwrap();
        assert!(u.tls);
    }

    #[test]
    fn hostname_listener_rejected() {
        let err = TcpUrl::parse("tcp://relay.example.com:7001?listen=1").unwrap_err();
        assert!(
            matches!(err, UrlError::BadHost(ref h) if h == "listener host must be a literal IPv4/IPv6 address")
        );
    }

    #[test]
    fn ipv6_literal_host_is_bracket_stripped() {
        let u = TcpUrl::parse("tcps://[::1]:7001").unwrap();
        assert_eq!(u.host, "::1");
    }

    #[test]
    fn pkt_size_is_a_byte_size() {
        let u = TcpUrl::parse("tcp://1.2.3.4:7001?pkt_size=8K").unwrap();
        assert_eq!(u.config.pkt_size, Some(8192));
        let err = TcpUrl::parse("tcp://1.2.3.4:7001?pkt_size=big").unwrap_err();
        assert!(matches!(err, UrlError::BadQueryValue { ref key, .. } if key == "pkt_size"));
    }

    #[test]
    fn nodelay_defaults_on_and_the_url_overrides_it() {
        assert_eq!(
            TcpUrl::parse("tcp://1.2.3.4:7001").unwrap().config.nodelay,
            Some(true)
        );
        assert_eq!(
            TcpUrl::parse("tcp://1.2.3.4:7001?nodelay=0")
                .unwrap()
                .config
                .nodelay,
            Some(false)
        );
    }

    #[test]
    fn tls_paths_land_in_the_config() {
        let u = TcpUrl::parse("tcps://1.2.3.4:7001?ca=/tmp/ca.pem").unwrap();
        assert_eq!(
            u.config.ca.as_deref(),
            Some(std::path::Path::new("/tmp/ca.pem"))
        );
    }
}
