//! Parsing of `rist://` URLs.
//!
//! Supported forms:
//! - `rist://host:port` — Simple Profile sender (unicast UDP)
//! - `rist://@host:port` — receiver bind (ffmpeg `@` convention)
//! - `rist://239.x.x.x:port` — multicast sender
//! - Query params: `profile`, `bandwidth`, `buffer`, `aes-type`, `secret`,
//!   `cname`, `recovery_maxbitrate`, `session_timeout`, `compression`

use std::net::IpAddr;
use std::time::Duration;

use thiserror::Error;

use crate::config::{RistProfile, RistSecret};

/// Parsed `rist://` URL.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RistUrl {
    pub addr: IpAddr,
    pub port: u16,
    /// True if the URL had `@` prefix (recv-bind intent, ffmpeg convention).
    pub is_recv_bind: bool,
    pub profile: Option<RistProfile>,
    /// kbps target throughput cap.
    pub bandwidth_kbps: Option<u32>,
    /// Recovery buffer.
    pub buffer_ms: Option<Duration>,
    /// AES key size in bits (128/192/256).
    pub aes_type: Option<u32>,
    /// AES PSK from the URL. Redacting newtype — its plaintext never appears in `Debug`.
    pub secret: Option<RistSecret>,
    /// RTCP CNAME.
    pub cname: Option<String>,
    /// Recovery retransmit bandwidth cap (kbps).
    pub recovery_maxbitrate_kbps: Option<u32>,
    /// Receiver session timeout in ms.
    pub session_timeout_ms: Option<u32>,
    /// NULL-packet deletion / compression (librist-specific).
    pub compression: Option<bool>,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RistUrlError {
    #[error("URL scheme must be 'rist', got '{0}'")]
    BadScheme(String),
    #[error("URL must include a port")]
    MissingPort,
    #[error("host '{0}' is not a literal IPv4/IPv6 address")]
    BadHost(String),
    #[error("query param '{key}' has invalid value '{value}': {detail}")]
    BadQueryValue {
        key: String,
        value: String,
        detail: String,
    },
    #[error("aes-type must be 128, 192, or 256; got {0}")]
    BadAesType(u32),
    #[error(
        "aes-type given without secret; RIST encryption needs both (secret alone defaults to AES-256)"
    )]
    AesTypeWithoutSecret,
    #[error("URL parse failed: {0}")]
    Parse(#[from] tst_core::url::common::UrlError),
}

impl RistUrl {
    pub fn parse(s: &str) -> Result<Self, RistUrlError> {
        use tst_core::url::common::parse_url;

        let parsed = parse_url(s)?;
        if parsed.scheme != "rist" {
            return Err(RistUrlError::BadScheme(parsed.scheme.to_string()));
        }
        let port = parsed.port.ok_or(RistUrlError::MissingPort)?;

        // ffmpeg convention: `@host` marks recv-bind intent. parse_url stores
        // the `@` chunk in the username slot; same trick as tst-udp / tst-tcp.
        let is_recv_bind = parsed.username.is_some();

        let host_str = parsed.host;
        let addr: IpAddr = host_str
            .parse()
            .map_err(|_| RistUrlError::BadHost(host_str.to_string()))?;

        let mut profile = None;
        let mut bandwidth_kbps = None;
        let mut buffer_ms = None;
        let mut aes_type = None;
        let mut secret = None;
        let mut cname = None;
        let mut recovery_maxbitrate_kbps = None;
        let mut session_timeout_ms = None;
        let mut compression = None;

        for (k, v) in &parsed.query {
            let key = k.as_ref();
            let value = v.as_ref();
            match key {
                "profile" => {
                    profile = Some(match value {
                        "simple" => RistProfile::Simple,
                        "main" => RistProfile::Main,
                        other => {
                            return Err(RistUrlError::BadQueryValue {
                                key: key.to_string(),
                                value: other.to_string(),
                                detail: "expected one of: simple, main".into(),
                            });
                        }
                    });
                }
                "bandwidth" => {
                    bandwidth_kbps = Some(parse_u32(key, value)?);
                }
                "buffer" => {
                    let ms: u32 = parse_u32(key, value)?;
                    buffer_ms = Some(Duration::from_millis(ms as u64));
                }
                "aes-type" => {
                    let n: u32 = parse_u32(key, value)?;
                    match n {
                        128 | 192 | 256 => aes_type = Some(n),
                        _ => return Err(RistUrlError::BadAesType(n)),
                    }
                }
                "secret" => {
                    if value.is_empty() {
                        return Err(RistUrlError::BadQueryValue {
                            key: key.to_string(),
                            value: String::new(),
                            detail: "must be non-empty".into(),
                        });
                    }
                    secret = Some(RistSecret::new(value));
                }
                "cname" => {
                    cname = Some(value.to_string());
                }
                "recovery_maxbitrate" | "recovery-maxbitrate" => {
                    recovery_maxbitrate_kbps = Some(parse_u32(key, value)?);
                }
                "session_timeout" | "session-timeout" => {
                    session_timeout_ms = Some(parse_u32(key, value)?);
                }
                "compression" => {
                    compression = Some(parse_bool(key, value)?);
                }
                _ => {}
            }
        }

        match (aes_type, &secret) {
            (Some(_), None) => return Err(RistUrlError::AesTypeWithoutSecret),
            (None, Some(_)) => aes_type = Some(256), // librist's own default for a secret without a key size
            _ => {}
        }

        Ok(Self {
            addr,
            port,
            is_recv_bind,
            profile,
            bandwidth_kbps,
            buffer_ms,
            aes_type,
            secret,
            cname,
            recovery_maxbitrate_kbps,
            session_timeout_ms,
            compression,
        })
    }
}

fn parse_u32(key: &str, value: &str) -> Result<u32, RistUrlError> {
    tst_core::url::common::parse_int_query(value).map_err(|detail| RistUrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail,
    })
}

fn parse_bool(key: &str, value: &str) -> Result<bool, RistUrlError> {
    tst_core::url::common::parse_bool_query(value).map_err(|detail| RistUrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail,
    })
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
        assert_eq!(u.profile, Some(RistProfile::Main));
        assert_eq!(u.bandwidth_kbps, Some(10000));
        assert_eq!(u.buffer_ms, Some(Duration::from_millis(200)));
        assert_eq!(u.aes_type, Some(256));
        assert_eq!(u.secret.as_ref().map(RistSecret::expose), Some("topsecret"));
        assert_eq!(u.cname.as_deref(), Some("uav-12"));
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
        let secret_dbg = format!("{:?}", u.secret);
        assert!(
            !secret_dbg.contains("topsecret"),
            "RistSecret Debug leaked the PSK: {secret_dbg}"
        );
        // The plaintext is still reachable via the explicit accessor.
        assert_eq!(u.secret.as_ref().map(RistSecret::expose), Some("topsecret"));
    }

    #[test]
    fn rejects_invalid_aes_type() {
        let err = RistUrl::parse("rist://1.2.3.4:8000?aes-type=64");
        assert!(matches!(err, Err(RistUrlError::BadAesType(64))));
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(matches!(
            RistUrl::parse("udp://host:8000"),
            Err(RistUrlError::BadScheme(_))
        ));
    }

    #[test]
    fn secret_alone_defaults_aes_type_to_256() {
        let u = RistUrl::parse("rist://127.0.0.1:9000?secret=example").unwrap();
        assert_eq!(u.aes_type, Some(256));
        assert!(u.secret.is_some());
    }

    #[test]
    fn aes_type_without_secret_is_rejected() {
        let e = RistUrl::parse("rist://127.0.0.1:9000?aes-type=256").unwrap_err();
        assert!(matches!(e, RistUrlError::AesTypeWithoutSecret), "got {e:?}");
    }

    #[test]
    fn empty_secret_is_rejected() {
        let e = RistUrl::parse("rist://127.0.0.1:9000?secret=").unwrap_err();
        assert!(
            matches!(e, RistUrlError::BadQueryValue { ref key, .. } if key == "secret"),
            "got {e:?}"
        );
    }
}
