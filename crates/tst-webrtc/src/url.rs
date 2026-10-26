//! Parsing of `whep://` and `wheps://` URLs for binding the WHEP server.
//!
//! - `whep://0.0.0.0:8889` — plain HTTP signalling on `0.0.0.0:8889`
//! - `wheps://0.0.0.0:8443?cert=server.crt&key=server.key` — HTTPS via rustls
//! - Query keys: `media_port`, `advertise`, `max_viewers`, `token`,
//!   `cors_origin`, `cert`, `key` (see [`URL_KEYS`])
//!
//! The URL names the bind address only. Mount paths are added on the server,
//! so a path other than empty or `/` is refused. Unknown query keys are
//! refused too.

use std::net::IpAddr;

use tst_core::url::UrlError;
use tst_core::url::common::{parse_int, parse_url};
use tst_core::url::query::{QueryKey, UnknownKeys, apply_query};

/// The default `max_viewers`.
const DEFAULT_MAX_VIEWERS: u64 = 10;

/// The largest `max_viewers` a URL may ask for.
const MAX_MAX_VIEWERS: u64 = 64;

/// A parsed WHEP server URL: where the server binds and how it admits viewers.
///
/// Its `Debug` output shows `token` as `<redacted>`.
#[derive(Clone, PartialEq, Eq)]
pub struct WhepUrl {
    /// The literal IP address the signalling endpoint binds on.
    pub addr: IpAddr,
    /// The signalling (HTTP or HTTPS) port.
    pub port: u16,
    /// `true` for `wheps://`: the signalling endpoint speaks HTTPS.
    pub tls: bool,
    /// The UDP port every viewer's media shares. Defaults to [`Self::port`].
    pub media_port: u16,
    /// The address offered to viewers as the media candidate, when it differs
    /// from the bind address (a host behind NAT). `None` offers the bind
    /// address.
    pub advertise: Option<IpAddr>,
    /// The most viewers connected at once (1 to 64, default 10).
    pub max_viewers: u64,
    /// A bearer token every POST and DELETE must carry; the OPTIONS
    /// preflight and a request for no route need none. `None` admits any
    /// request.
    pub token: Option<String>,
    /// The `Access-Control-Allow-Origin` value the endpoint answers with
    /// (default `*`).
    pub cors_origin: String,
    /// The server certificate's PEM file path. Required by `wheps://`.
    pub cert: Option<String>,
    /// The server private key's PEM file path. Required by `wheps://`.
    pub key: Option<String>,
}

impl std::fmt::Debug for WhepUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepUrl")
            .field("addr", &self.addr)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("media_port", &self.media_port)
            .field("advertise", &self.advertise)
            .field("max_viewers", &self.max_viewers)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("cors_origin", &self.cors_origin)
            .field("cert", &self.cert)
            .field("key", &self.key)
            .finish()
    }
}

const EMPTY_TOKEN: &str = "an empty token would refuse every request; omit token= to admit any";

type Applied = Result<(), UrlError>;

fn set_media_port(u: &mut WhepUrl, key: &str, value: &str) -> Applied {
    u.media_port = parse_int(key, value)?;
    Ok(())
}

fn set_advertise(u: &mut WhepUrl, key: &str, value: &str) -> Applied {
    let addr = value.parse().map_err(|_| UrlError::BadQueryValue {
        key: key.to_string(),
        value: value.to_string(),
        detail: "expected a literal IPv4 or IPv6 address".into(),
    })?;
    u.advertise = Some(addr);
    Ok(())
}

fn set_max_viewers(u: &mut WhepUrl, key: &str, value: &str) -> Applied {
    let n: u64 = parse_int(key, value)?;
    if !(1..=MAX_MAX_VIEWERS).contains(&n) {
        return Err(UrlError::BadQueryValue {
            key: key.to_string(),
            value: value.to_string(),
            detail: format!("expected 1..={MAX_MAX_VIEWERS}"),
        });
    }
    u.max_viewers = n;
    Ok(())
}

fn set_token(u: &mut WhepUrl, key: &str, value: &str) -> Applied {
    if value.is_empty() {
        return Err(UrlError::BadQueryValue {
            key: key.to_string(),
            value: String::new(),
            detail: EMPTY_TOKEN.into(),
        });
    }
    u.token = Some(value.to_string());
    Ok(())
}

fn set_cors_origin(u: &mut WhepUrl, _key: &str, value: &str) -> Applied {
    u.cors_origin = value.to_string();
    Ok(())
}

fn set_cert(u: &mut WhepUrl, _key: &str, value: &str) -> Applied {
    u.cert = Some(value.to_string());
    Ok(())
}

fn set_key(u: &mut WhepUrl, _key: &str, value: &str) -> Applied {
    u.key = Some(value.to_string());
    Ok(())
}

/// The query keys a `whep://` or `wheps://` URL accepts; the parser and the
/// generated key documentation both read this table.
#[doc(hidden)]
pub const URL_KEYS: &[QueryKey<WhepUrl>] = &[
    QueryKey::new(
        "media_port",
        &[],
        "UDP port shared by every viewer's media",
        "port; default the URL's port",
        set_media_port,
    ),
    QueryKey::new(
        "advertise",
        &[],
        "Address offered to viewers as the media candidate",
        "literal IP address; default the bind address",
        set_advertise,
    ),
    QueryKey::new(
        "max_viewers",
        &[],
        "Viewers connected at once",
        "count, 1 to 64; default 10",
        set_max_viewers,
    ),
    QueryKey::new(
        "token",
        &[],
        "Bearer token every POST, PATCH and DELETE must carry",
        "text",
        set_token,
    ),
    QueryKey::new(
        "cors_origin",
        &[],
        "`Access-Control-Allow-Origin` value",
        "origin or `*`; default `*`",
        set_cors_origin,
    ),
    QueryKey::new(
        "cert",
        &[],
        "Server certificate",
        "PEM file path; a `wheps` server",
        set_cert,
    ),
    QueryKey::new(
        "key",
        &[],
        "Server private key",
        "PEM file path; a `wheps` server",
        set_key,
    ),
];

impl WhepUrl {
    /// Parse a `whep://` or `wheps://` URL.
    ///
    /// ```
    /// use tst_webrtc::url::WhepUrl;
    ///
    /// let u = WhepUrl::parse(
    ///     "wheps://0.0.0.0:8443?media_port=50000&advertise=203.0.113.5\
    ///      &max_viewers=4&token=s3cret&cors_origin=https://ops.example\
    ///      &cert=server.crt&key=server.key",
    /// )?;
    /// assert!(u.tls);
    /// assert_eq!((u.port, u.media_port, u.max_viewers), (8443, 50000, 4));
    /// assert_eq!(u.advertise.map(|a| a.to_string()).as_deref(), Some("203.0.113.5"));
    /// assert_eq!(u.token.as_deref(), Some("s3cret"));
    /// assert_eq!(u.cors_origin, "https://ops.example");
    /// assert_eq!((u.cert.as_deref(), u.key.as_deref()), (Some("server.crt"), Some("server.key")));
    ///
    /// // Without a query, the media shares the signalling port.
    /// let plain = WhepUrl::parse("whep://127.0.0.1:8889")?;
    /// assert_eq!((plain.media_port, plain.max_viewers, plain.cors_origin.as_str()), (8889, 10, "*"));
    /// # Ok::<(), tst_core::url::UrlError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - [`UrlError::BadScheme`] for a scheme other than `whep` or `wheps`.
    /// - [`UrlError::MissingPort`] when the URL names no port.
    /// - [`UrlError::BadHost`] when the host is not a literal IP address.
    /// - [`UrlError::BadQueryValue`] for a value its key cannot use (an empty
    ///   `token` among them), with key
    ///   `path` for a path other than empty or `/`, and with key `cert` for a
    ///   `wheps://` URL that lacks `cert` or `key` and for a plaintext
    ///   `whep://` URL that has either.
    /// - [`UrlError::UnknownQueryKey`] for a key [`URL_KEYS`] does not list.
    /// - [`UrlError::Rejected`] when the URL's structure does not parse.
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let parsed =
            parse_url(s).map_err(|e| UrlError::Rejected(format!("URL parse failed: {e}")))?;
        let tls = match parsed.scheme {
            "whep" => false,
            "wheps" => true,
            other => {
                return Err(UrlError::BadScheme {
                    expected: "'whep' or 'wheps'",
                    got: other.to_string(),
                });
            }
        };
        let port = parsed.port.ok_or(UrlError::MissingPort)?;
        let host = parsed.host;
        let addr: IpAddr = host.parse().map_err(|_| {
            UrlError::BadHost(format!("host '{host}' is not a literal IPv4/IPv6 address"))
        })?;
        if !matches!(parsed.path, "" | "/") {
            return Err(UrlError::BadQueryValue {
                key: "path".into(),
                value: parsed.path.to_string(),
                detail: "a WHEP bind URL takes no path; mounts are added on the server".into(),
            });
        }

        let mut url = Self {
            addr,
            port,
            tls,
            media_port: port,
            advertise: None,
            max_viewers: DEFAULT_MAX_VIEWERS,
            token: None,
            cors_origin: "*".to_string(),
            cert: None,
            key: None,
        };
        apply_query(URL_KEYS, &mut url, &parsed.query, UnknownKeys::Reject)?;

        url.validate()?;
        Ok(url)
    }

    /// Checks the invariants [`parse`](Self::parse) enforces, for a value
    /// built or edited by hand: `max_viewers` in 1 to 64, a non-empty
    /// `token`, and `cert` with `key` exactly when `tls` is set.
    ///
    /// # Errors
    ///
    /// [`UrlError::BadQueryValue`] naming the offending key.
    pub fn validate(&self) -> Result<(), UrlError> {
        if !(1..=MAX_MAX_VIEWERS).contains(&self.max_viewers) {
            return Err(UrlError::BadQueryValue {
                key: "max_viewers".into(),
                value: self.max_viewers.to_string(),
                detail: format!("expected 1..={MAX_MAX_VIEWERS}"),
            });
        }
        if self.token.as_deref() == Some("") {
            return Err(UrlError::BadQueryValue {
                key: "token".into(),
                value: String::new(),
                detail: EMPTY_TOKEN.into(),
            });
        }
        if !self.tls && (self.cert.is_some() || self.key.is_some()) {
            // Refuse rather than come up unencrypted when TLS was asked for.
            let key = if self.cert.is_some() { "cert" } else { "key" };
            return Err(UrlError::BadQueryValue {
                key: key.into(),
                value: self
                    .cert
                    .clone()
                    .or_else(|| self.key.clone())
                    .unwrap_or_default(),
                detail: "cert= and key= need a wheps:// URL; this plaintext whep:// URL \
                     would serve unencrypted"
                    .into(),
            });
        }
        if self.tls && (self.cert.is_none() || self.key.is_none()) {
            let missing = match (&self.cert, &self.key) {
                (None, None) => "cert= and key= are",
                (None, Some(_)) => "cert= is",
                _ => "key= is",
            };
            return Err(UrlError::BadQueryValue {
                key: "cert".into(),
                value: self.cert.clone().unwrap_or_default(),
                detail: format!("wheps:// requires both cert= and key=; {missing} missing"),
            });
        }
        Ok(())
    }
}
