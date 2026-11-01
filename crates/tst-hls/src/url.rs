//! Parsing of `hls://` URLs for binding the HLS HTTP server.
//!
//! - `hls://0.0.0.0:8080` — plain HTTP, bind on `0.0.0.0:8080`
//! - `hlss://0.0.0.0:8443?cert=server.crt&key=server.key` — HTTPS via rustls
//! - Query params: `output_dir`, `segment_duration`, `playlist_window`,
//!   `mode` (live/event/vod), `auth_user`, `auth_pass`, `cert`, `key`
//!
//! `output_dir` is optional — see [`crate::config::HlsConfig::output_dir`]
//! for the default when omitted.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use tst_core::url::UrlError;
use tst_core::url::common::{parse_int, parse_url};
use tst_core::url::query::{QueryKey, UnknownKeys, apply_query};

use crate::config::{HlsConfig, HlsMode};

/// Parsed HLS publisher URL: the publisher configuration the URL describes.
#[derive(Debug, Clone)]
pub struct HlsUrl {
    /// [`HlsConfig::default`] with the URL's bind address, its scheme's
    /// `tls` flag and its query keys applied.
    pub config: HlsConfig,
}

type Applied = Result<(), UrlError>;

fn set_output_dir(c: &mut HlsConfig, _key: &str, value: &str) -> Applied {
    c.output_dir = PathBuf::from(value);
    Ok(())
}

fn set_segment_duration(c: &mut HlsConfig, key: &str, value: &str) -> Applied {
    c.segment_duration = Duration::from_secs(parse_int(key, value)?);
    Ok(())
}

fn set_playlist_window(c: &mut HlsConfig, key: &str, value: &str) -> Applied {
    c.playlist_window = parse_int(key, value)?;
    Ok(())
}

fn set_mode(c: &mut HlsConfig, key: &str, value: &str) -> Applied {
    c.mode = match value {
        "live" => HlsMode::Live,
        "event" => HlsMode::Event,
        "vod" => HlsMode::Vod,
        other => {
            return Err(UrlError::BadQueryValue {
                key: key.to_string(),
                value: other.to_string(),
                detail: "expected one of: live, event, vod".into(),
            });
        }
    };
    Ok(())
}

// The two credentials arrive as separate keys; `parse` keeps the pair only
// when the URL named both.
fn set_auth_user(c: &mut HlsConfig, _key: &str, value: &str) -> Applied {
    let pass = c.basic_auth.take().map(|(_, p)| p).unwrap_or_default();
    c.basic_auth = Some((value.to_string(), pass));
    Ok(())
}

fn set_auth_pass(c: &mut HlsConfig, _key: &str, value: &str) -> Applied {
    let user = c.basic_auth.take().map(|(u, _)| u).unwrap_or_default();
    c.basic_auth = Some((user, value.to_string()));
    Ok(())
}

fn set_cert(c: &mut HlsConfig, _key: &str, value: &str) -> Applied {
    c.tls_cert = Some(PathBuf::from(value));
    Ok(())
}

fn set_key(c: &mut HlsConfig, _key: &str, value: &str) -> Applied {
    c.tls_key = Some(PathBuf::from(value));
    Ok(())
}

/// The query keys an `hls://` or `hlss://` URL accepts; the parser and the
/// generated key documentation both read this table.
#[doc(hidden)]
pub const URL_KEYS: &[QueryKey<HlsConfig>] = &[
    QueryKey::new(
        "output_dir",
        &[],
        "Where segments and playlists are written",
        "directory path",
        set_output_dir,
    ),
    QueryKey::new(
        "segment_duration",
        &[],
        "Target segment length",
        "seconds",
        set_segment_duration,
    ),
    QueryKey::new(
        "playlist_window",
        &[],
        "Segments kept in a live playlist",
        "count",
        set_playlist_window,
    ),
    QueryKey::new(
        "mode",
        &[],
        "Playlist mode",
        "`live`, `event`, `vod`",
        set_mode,
    ),
    QueryKey::new(
        "auth_user",
        &[],
        "HTTP Basic user name",
        "text",
        set_auth_user,
    ),
    QueryKey::new(
        "auth_pass",
        &[],
        "HTTP Basic password",
        "text",
        set_auth_pass,
    ),
    QueryKey::new(
        "cert",
        &[],
        "Server certificate",
        "PEM file path; an `hlss` publisher",
        set_cert,
    ),
    QueryKey::new(
        "key",
        &[],
        "Server private key",
        "PEM file path; an `hlss` publisher",
        set_key,
    ),
];

impl HlsUrl {
    /// Parse an `hls://` or `hlss://` URL. The host must be a literal IP
    /// address; unknown query keys are ignored.
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let parsed =
            parse_url(s).map_err(|e| UrlError::Rejected(format!("URL parse failed: {e}")))?;
        let tls = match parsed.scheme {
            "hls" => false,
            "hlss" => true,
            other => {
                return Err(UrlError::BadScheme {
                    expected: "'hls' or 'hlss'",
                    got: other.to_string(),
                });
            }
        };
        if parsed.username.is_some() || parsed.password.is_some() {
            return Err(UrlError::Rejected(
                "a bind URL takes no userinfo; set credentials with the `auth_user` and `auth_pass` query keys"
                    .into(),
            ));
        }
        let port = parsed.port.ok_or(UrlError::MissingPort)?;

        let host_str = parsed.host;
        let addr: IpAddr = host_str.parse().map_err(|_| {
            UrlError::BadHost("host is not a literal IPv4/IPv6 address".to_string())
        })?;

        let mut config = HlsConfig {
            bind: SocketAddr::new(addr, port),
            tls,
            ..HlsConfig::default()
        };
        apply_query(URL_KEYS, &mut config, &parsed.query, UnknownKeys::Ignore)?;

        // A lone `auth_user` or `auth_pass` configures nothing.
        let named = |key: &str| parsed.query.iter().any(|(k, _)| k == key);
        if !(named("auth_user") && named("auth_pass")) {
            config.basic_auth = None;
        }
        Ok(Self { config })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_host_is_never_echoed() {
        let err = HlsUrl::parse("hls://u:hunter2:8080/ss@10.0.0.5:8888/p").unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn userinfo_is_refused_without_echoing_it() {
        for url in ["hls://u:p@127.0.0.1:8080", "hlss://sekret@127.0.0.1:8080"] {
            let msg = HlsUrl::parse(url).unwrap_err().to_string();
            assert!(
                msg.contains("auth_user") && msg.contains("auth_pass"),
                "{msg}"
            );
            assert!(!msg.contains("sekret"), "{msg}");
        }
        let msg = HlsUrl::parse("hls://alice:hunter2@127.0.0.1:8080")
            .unwrap_err()
            .to_string();
        assert!(!msg.contains("alice") && !msg.contains("hunter2"), "{msg}");
        assert!(HlsUrl::parse("hls://127.0.0.1:8080?auth_user=a&auth_pass=b").is_ok());
    }

    #[test]
    fn plain_hls() {
        let u = HlsUrl::parse("hls://0.0.0.0:8080").unwrap();
        assert!(!u.config.tls);
        assert_eq!(u.config.bind.port(), 8080);
    }

    #[test]
    fn https_with_cert_key() {
        let u = HlsUrl::parse("hlss://0.0.0.0:8443?cert=server.crt&key=server.key").unwrap();
        assert!(u.config.tls);
        assert_eq!(
            u.config.tls_cert.as_deref(),
            Some(std::path::Path::new("server.crt"))
        );
    }

    #[test]
    fn mode_is_parsed() {
        let u = HlsUrl::parse("hls://0.0.0.0:8080?mode=event").unwrap();
        assert_eq!(u.config.mode, HlsMode::Event);
    }

    #[test]
    fn rejects_bad_mode() {
        assert!(matches!(
            HlsUrl::parse("hls://0.0.0.0:8080?mode=garbage"),
            Err(UrlError::BadQueryValue { .. })
        ));
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(matches!(
            HlsUrl::parse("hls2://host:8080"),
            Err(UrlError::BadScheme { .. })
        ));
    }

    #[test]
    fn a_lone_credential_configures_no_auth() {
        let u = HlsUrl::parse("hls://0.0.0.0:8080?auth_user=alice").unwrap();
        assert_eq!(u.config.basic_auth, None);
        let u = HlsUrl::parse("hls://0.0.0.0:8080?auth_user=alice&auth_pass=pw").unwrap();
        assert_eq!(u.config.basic_auth, Some(("alice".into(), "pw".into())));
    }
}
