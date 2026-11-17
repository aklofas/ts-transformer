//! Fluent builder for [`RtspClient`]: auth credentials, keepalive and
//! timeout overrides, TLS roots.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

use crate::error::RtspError;
use crate::rtsp::auth::AuthSchemes;
use crate::rtsp::client::{ConnectParams, RtspClient};
use crate::rtsp::message::validate_header_field;
use crate::url::{RtspTransportPref, RtspUrl, RtspVersion};

/// Fluent builder for [`RtspClient`] with extended options (auth
/// credentials, keepalive overrides, TLS roots, timeouts).
///
/// Unlike [`RtspClient::connect`] / [`RtspClient::connect_with`] which
/// take just a URL, this builder lets callers pass credentials and
/// keepalive policy without re-encoding them into the URL query string.
///
/// The builder's [`Self::connect`] starts the keepalive pings on the
/// client's thread unless [`Self::no_auto_keepalive`] is set.
#[must_use]
pub struct RtspClientBuilder {
    url: RtspUrl,
    username: Option<String>,
    password: Option<SecretString>,
    auth_schemes: AuthSchemes,
    no_auto_keepalive: bool,
    keepalive_interval_override: Option<Duration>,
    connect_timeout: Duration,
    read_timeout: Duration,
    request_timeout: Option<Duration>,
    user_agent: String,
    #[cfg(feature = "tls")]
    tls_root_certs: Option<rustls::RootCertStore>,
}

impl RtspClientBuilder {
    /// New builder for `url`. URL-embedded credentials
    /// (`rtsp://user:pass@host/...`) are picked up as defaults; call
    /// [`Self::auth`] to override. A user name without a password
    /// (`rtsp://user@host/...`) answers a challenge with an empty password,
    /// as `rtsp://user:@host/...` does; an empty user name sends no
    /// credentials.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Url`] if the URL cannot be parsed.
    pub fn new(url: &str) -> Result<Self, RtspError> {
        Ok(Self::from_url(RtspUrl::parse(url)?))
    }

    /// Builder for an already parsed URL; URL userinfo becomes the default
    /// credentials, which [`Self::auth`] overrides (a user name without a
    /// password answers with an empty one, see [`Self::new`]). Every query
    /// option the URL carries (`?transport=`, `?rtsp_version=`,
    /// `?tcp_keepalive=`, `?recv_timeout=`) is kept, so a caller holding an
    /// [`RtspUrl`] need not render it back to a string first.
    pub fn from_url(url: RtspUrl) -> Self {
        let username = url.username.clone();
        let password = url.password.clone();
        Self {
            url,
            username,
            password,
            auth_schemes: AuthSchemes::default(),
            no_auto_keepalive: false,
            keepalive_interval_override: None,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_millis(100),
            request_timeout: Some(Duration::from_secs(10)),
            user_agent: "tst-rtp/0.1".into(),
            #[cfg(feature = "tls")]
            tls_root_certs: None,
        }
    }

    /// Disable the automatic keepalive pings. Default `false`
    /// (i.e., auto-keepalive is on).
    pub fn no_auto_keepalive(mut self, disabled: bool) -> Self {
        self.no_auto_keepalive = disabled;
        self
    }

    /// Override the keepalive interval. Default: `session_timeout / 2`
    /// as derived from the server's `Session: ...;timeout=N` header
    /// (starting from the 60 s RFC default, retuned when SETUP learns
    /// the advertised value). An explicit override pins the cadence —
    /// the SETUP retune never clobbers it.
    pub fn keepalive_interval(mut self, t: Duration) -> Self {
        self.keepalive_interval_override = Some(t);
        self
    }

    /// Enable OS-level TCP `SO_KEEPALIVE` on the control socket with the
    /// given idle time. The kernel then probes an idle connection, so a
    /// peer that died without FIN/RST (e.g. a hard-power-cycled camera)
    /// eventually errors the socket instead of leaving it silently open.
    /// Probe cadence and count after the idle time are the OS defaults.
    ///
    /// Distinct from [`Self::keepalive_interval`], which paces the
    /// RTSP-level OPTIONS pinger. Equivalent to the `?tcp_keepalive=N`
    /// URL query key (seconds). Default: disabled (OS default).
    pub fn tcp_keepalive(mut self, idle: Duration) -> Self {
        self.url.tcp_keepalive = Some(idle);
        self
    }

    /// Provide explicit credentials. Overrides anything parsed from
    /// the URL's userinfo component. The password may be empty; an empty
    /// `username` sends no credentials. A URL user name without a password
    /// already answers with an empty one, so this call is not needed for it.
    ///
    /// # Example
    /// ```
    /// use tst_rtp::{RtspClientBuilder, SecretString};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let builder = RtspClientBuilder::new("rtsp://127.0.0.1:8554/live")?
    ///     .auth("admin", SecretString::new("hunter2".into()));
    /// # let _ = builder;
    /// # Ok(())
    /// # }
    /// ```
    pub fn auth(mut self, username: impl Into<String>, password: SecretString) -> Self {
        self.username = Some(username.into());
        self.password = Some(password);
        self
    }

    /// Restrict which `WWW-Authenticate` schemes the client answers.
    /// Default [`AuthSchemes::Automatic`] (Digest when offered, else Basic).
    /// [`AuthSchemes::DigestOnly`] never sends the password in Basic form:
    /// a challenge without Digest fails with
    /// [`RtspError::AuthUnsupported`], so a
    /// plaintext `rtsp://` endpoint or an on-path party cannot downgrade the
    /// client to Basic.
    pub fn auth_schemes(mut self, schemes: AuthSchemes) -> Self {
        self.auth_schemes = schemes;
        self
    }

    /// Set the SETUP transport preference, overriding the URL's
    /// `?transport=` query when both are present. Equivalent to
    /// `?transport=tcp|udp` but self-documenting — no URL string editing
    /// required. This isn't only a "force": passing
    /// [`RtspTransportPref::PreferUdp`] here overrides a URL query back to
    /// the default try-UDP-then-fall-back-to-TCP behavior too.
    pub fn transport_preference(mut self, pref: RtspTransportPref) -> Self {
        self.url.transport_preference = pref;
        self
    }

    /// Set the RTSP version written on request lines, overriding the URL's
    /// `?rtsp_version=` query when both are present. Default
    /// [`RtspVersion::V1_0`].
    pub fn rtsp_version(mut self, version: RtspVersion) -> Self {
        self.url.rtsp_version = version;
        self
    }

    /// Override the `User-Agent` header. Default: `tst-rtp/0.1`.
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Connect budget, default 10 s: bounds the TCP connect and, for
    /// `rtsps://`, the TLS handshake together, each socket read and write
    /// of the handshake included. Name resolution runs before the budget
    /// starts, and so does loading the OS trust store (only for `rtsps://`
    /// without `tls_root_certs`; the first load that yields a certificate
    /// is kept for the process, so later connects skip it).
    /// Expiry is `RtspError::Io(TimedOut)`.
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = t;
        self
    }

    /// Per-read socket timeout of the `rtsps://` TLS handshake: each
    /// socket read of the handshake waits at most this long, or the time
    /// left on the [`Self::connect_timeout`] budget when that is shorter.
    /// Default 100 ms. Must be non-zero: zero fails every connect,
    /// `rtsp://` included, with [`RtspError::Io`].
    ///
    /// Apart from that check it has no effect on a plain `rtsp://`
    /// connect, nor on a connected client: the client's thread reads the
    /// control connection only when it has bytes. How long a request waits is
    /// [`Self::request_timeout`].
    pub fn read_timeout(mut self, t: Duration) -> Self {
        self.read_timeout = t;
        self
    }

    /// Per-request deadline: how long `options()` / `describe()` /
    /// `setup_*()` / `play()` / `pause()` / `teardown()`
    /// wait for the request to be written and its complete response.
    /// Expiry returns [`RtspError::Timeout`]. `None` disables the deadline
    /// (a silent server parks the call until the cancel handle fires).
    /// Default 10 s, the same as [`Self::connect_timeout`].
    ///
    /// The clock starts when the call hands the request to the client's
    /// thread, so it covers the write too: a request the peer has not
    /// taken in full by the deadline is a `Timeout`, and its unwritten
    /// bytes stay queued and may still go out. A response that completes
    /// after the deadline is a `Timeout`, not a late success. Once the
    /// request is written, a cancel is seen within ~100 ms, with or
    /// without a deadline.
    ///
    /// Separately, queued bytes the peer takes none of for 5 s end the
    /// control connection: a request still waiting then fails with
    /// [`RtspError::Io`]`(TimedOut)`, and so does every later request.
    ///
    /// Replies are matched to requests by `CSeq`: a reply carrying the
    /// `CSeq` of a request that already timed out is dropped, so the next
    /// request is not answered with it.
    ///
    /// A duration too large to form an `Instant` deadline (e.g.
    /// [`Duration::MAX`]) behaves as `None` (no deadline) rather than
    /// panicking.
    pub fn request_timeout(mut self, t: Option<Duration>) -> Self {
        self.request_timeout = t;
        self
    }

    /// Override the rustls root certificate store for `rtsps://`
    /// connections. Default: the operating system's trust store.
    #[cfg(feature = "tls")]
    pub fn tls_root_certs(mut self, certs: rustls::RootCertStore) -> Self {
        self.tls_root_certs = Some(certs);
        self
    }

    /// Connect, returning the live client.
    ///
    /// Unless [`Self::no_auto_keepalive`] was set, the OPTIONS keepalive
    /// pings are started here, on the client's thread, with the
    /// [`Self::keepalive_interval`] override when one was supplied
    /// (otherwise the cadence derives from the session timeout — the
    /// 60 s default now, retuned when SETUP learns the
    /// server-advertised value).
    ///
    /// # Errors
    ///
    /// See [`RtspClient::connect_with`].
    pub fn connect(self) -> Result<RtspClient, RtspError> {
        // Fail fast on header injection: a User-Agent or credential
        // (username/password) carrying CR/LF/NUL/control bytes would be
        // serialized verbatim into the User-Agent / Authorization header and
        // smuggle a second header or whole request onto the wire.
        // Reject before opening the socket so a malicious value never reaches
        // the network. The encode path also re-validates as a backstop, but
        // catching it here gives a clean error and never touches the wire.
        validate_header_field(&self.user_agent, "User-Agent contains a control byte")?;
        if let Some(user) = &self.username {
            validate_header_field(user, "auth username contains a control byte")?;
        }
        if let Some(pass) = &self.password {
            validate_header_field(
                pass.expose_secret(),
                "auth password contains a control byte",
            )?;
        }

        // Bake builder-supplied credentials into the URL so the connect
        // path hands them to the client's engine from
        // `url.username/password`. The URL
        // already carries credentials when the caller passed
        // `rtsp://user:pass@host/...`; explicit builder `.auth()` calls
        // override.
        let mut url = self.url.clone();
        if self.username.is_some() {
            url.username = self.username.clone();
        }
        if self.password.is_some() {
            url.password = self.password.clone();
        }
        let params = ConnectParams {
            connect_timeout: self.connect_timeout,
            read_timeout: self.read_timeout,
            request_timeout: self.request_timeout,
            user_agent: self.user_agent,
            auth_schemes: self.auth_schemes,
            #[cfg(test)]
            socket_probe: None,
            #[cfg(test)]
            exit_gate: None,
        };
        #[cfg(feature = "tls")]
        let mut client =
            RtspClient::connect_with_params(&url, self.tls_root_certs.clone(), params)?;
        #[cfg(not(feature = "tls"))]
        let mut client = RtspClient::connect_with_params(&url, None, params)?;
        if !self.no_auto_keepalive {
            client.spawn_keepalive_if_needed(self.keepalive_interval_override)?;
        }
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every URL field survives the parsed-URL constructor.
    #[test]
    fn from_url_keeps_every_url_field() {
        let url = RtspUrl::parse(
            "rtsp://u:p@h:1/x?transport=tcp&rtsp_version=2.0&tcp_keepalive=7&recv_timeout=250",
        )
        .unwrap();
        let b = RtspClientBuilder::from_url(url);
        assert_eq!(b.url.transport_preference, RtspTransportPref::ForceTcp);
        assert_eq!(b.url.rtsp_version, RtspVersion::V2_0);
        assert_eq!(b.url.tcp_keepalive, Some(Duration::from_secs(7)));
        assert_eq!(b.url.recv_timeout, Some(Duration::from_millis(250)));
        assert_eq!(b.username.as_deref(), Some("u"));
        assert_eq!(
            b.password
                .as_ref()
                .map(|p| p.expose_secret().to_owned())
                .as_deref(),
            Some("p")
        );
    }

    /// Both knob homes land on the same `RtspUrl` field: the builder
    /// setter and the `?tcp_keepalive=` URL query (already baked in by
    /// `new`), so `connect` needs no separate plumbing for either.
    #[test]
    fn tcp_keepalive_builder_and_url_paths_set_knob() {
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:8554/live")
            .unwrap()
            .tcp_keepalive(Duration::from_secs(15));
        assert_eq!(b.url.tcp_keepalive, Some(Duration::from_secs(15)));

        let b = RtspClientBuilder::new("rtsp://127.0.0.1:8554/live?tcp_keepalive=9").unwrap();
        assert_eq!(b.url.tcp_keepalive, Some(Duration::from_secs(9)));
    }

    // --- User-Agent / credential header injection rejected at connect,
    // before the socket opens. ---

    #[test]
    fn connect_rejects_crlf_user_agent_before_socket() {
        // A CRLF in the User-Agent would inject an Authorization header onto
        // the wire. `connect` must reject it with InvalidHeader *before*
        // touching the network — so this fails fast even though the host is
        // unroutable. (If validation were skipped we'd instead get an Io/
        // timeout error from the connect attempt.)
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:9/live")
            .unwrap()
            .user_agent("evil\r\nAuthorization: Basic injected");
        let e = b.connect().unwrap_err();
        assert!(
            matches!(e, RtspError::InvalidHeader { .. }),
            "expected InvalidHeader, got {e:?}"
        );
    }

    #[test]
    fn connect_rejects_nul_user_agent() {
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:9/live")
            .unwrap()
            .user_agent("evil\0nul");
        assert!(matches!(
            b.connect().unwrap_err(),
            RtspError::InvalidHeader { .. }
        ));
    }

    #[test]
    fn connect_rejects_crlf_in_credentials() {
        // Credentials flow into the Authorization header; a CRLF in the
        // username is the same injection.
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:9/live")
            .unwrap()
            .auth("user\r\nX-Evil: 1", SecretString::new("pw".into()));
        assert!(matches!(
            b.connect().unwrap_err(),
            RtspError::InvalidHeader { .. }
        ));

        let b = RtspClientBuilder::new("rtsp://127.0.0.1:9/live")
            .unwrap()
            .auth("user", SecretString::new("pw\r\nX-Evil: 1".into()));
        assert!(matches!(
            b.connect().unwrap_err(),
            RtspError::InvalidHeader { .. }
        ));
    }

    #[test]
    fn transport_preference_overrides_url_query() {
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:8554/s?transport=udp")
            .unwrap()
            .transport_preference(RtspTransportPref::ForceTcp);
        assert_eq!(b.url.transport_preference, RtspTransportPref::ForceTcp);
    }

    /// The version setter overrides the URL's `?rtsp_version=` query.
    #[test]
    fn rtsp_version_setter_overrides_the_url_query() {
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:8554/x?rtsp_version=1.0")
            .unwrap()
            .rtsp_version(RtspVersion::V2_0);
        assert_eq!(b.url.rtsp_version, RtspVersion::V2_0);
    }

    #[test]
    fn rtsp_client_builder_request_timeout_defaults_to_ten_seconds() {
        let b = RtspClientBuilder::new("rtsp://127.0.0.1:1/x").unwrap();
        assert_eq!(b.request_timeout, Some(Duration::from_secs(10)));
        assert_eq!(
            ConnectParams::default().request_timeout,
            Some(Duration::from_secs(10)),
            "the bare connect() entry points must carry the same default"
        );
        let b = b.request_timeout(None);
        assert_eq!(b.request_timeout, None);
    }
}
