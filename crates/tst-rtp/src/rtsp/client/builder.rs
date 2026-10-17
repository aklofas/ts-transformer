//! Fluent builder for [`RtspClient`]: auth credentials, keepalive and
//! timeout overrides, TLS roots.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

use crate::error::RtspError;
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
/// The builder's [`Self::connect`] always spawns the auto-keepalive
/// background thread unless [`Self::no_auto_keepalive`] is set.
#[must_use]
pub struct RtspClientBuilder {
    url: RtspUrl,
    username: Option<String>,
    password: Option<SecretString>,
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
    /// [`Self::auth`] to override.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Url`] if the URL cannot be parsed.
    pub fn new(url: &str) -> Result<Self, RtspError> {
        let parsed = RtspUrl::parse(url)?;
        let username = parsed.username.clone();
        let password = parsed.password.clone();
        Ok(Self {
            url: parsed,
            username,
            password,
            no_auto_keepalive: false,
            keepalive_interval_override: None,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_millis(100),
            request_timeout: Some(Duration::from_secs(10)),
            user_agent: "tst-rtp/0.1".into(),
            #[cfg(feature = "tls")]
            tls_root_certs: None,
        })
    }

    /// Disable the auto-keepalive background thread. Default `false`
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
    /// the URL's userinfo component.
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

    /// TCP connect timeout. Default 10 s.
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = t;
        self
    }

    /// Per-read socket timeout: the poll interval of the interleaved
    /// (RTP-over-TCP) read pump, and therefore how quickly that pump
    /// notices a cancel or yields the connection to a request write.
    /// Default 100 ms. Must be non-zero.
    ///
    /// It does not set how long a request waits — that is
    /// [`Self::request_timeout`]. While a request method waits for its
    /// response, each read is bounded by the shortest of this value, a
    /// 100 ms cancel tick and the time left on the request deadline, so
    /// a `read_timeout` longer than `request_timeout` neither delays the
    /// `Timeout` nor a cancel. The configured value is put back on the
    /// socket when the request returns.
    pub fn read_timeout(mut self, t: Duration) -> Self {
        self.read_timeout = t;
        self
    }

    /// Per-request deadline: how long `options()` / `describe()` /
    /// `setup_*()` / `play()` / `pause()` / `get_parameter()` / `teardown()`
    /// wait for the complete response once the request write returns.
    /// Expiry returns [`RtspError::Timeout`]. `None` disables the deadline
    /// (the pre-`request_timeout` behaviour: a silent server parks the
    /// call until the cancel handle fires). Default 10 s, the same as
    /// [`Self::connect_timeout`].
    ///
    /// The deadline holds whatever [`Self::read_timeout`] is set to: no
    /// read waits past the time left, and a response that completes after
    /// the deadline is a `Timeout`, not a late success. With `None`, a
    /// cancel is still seen within ~100 ms.
    ///
    /// The clock starts just before the write, so it also covers the
    /// (normally instant) write itself — but the write is not actively
    /// bounded by this deadline: it can still block up to the
    /// connection's own internal write timeout (independent of this
    /// knob) if the peer stops draining its receive window, in which case
    /// the error is [`RtspError::Io`]`(TimedOut)`, not `Timeout`. In
    /// practice an RTSP request is a few hundred bytes and this only
    /// matters against a peer that has stopped reading entirely.
    ///
    /// After a `Timeout` treat the control connection as indeterminate and
    /// build a fresh client: the late response, if it ever arrives, is still
    /// in the socket and the non-interleaved read path does not match
    /// responses by CSeq.
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
    /// Unless [`Self::no_auto_keepalive`] was set, the background
    /// OPTIONS-pinger is spawned here with the
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

        // Bake builder-supplied credentials into the URL so the
        // auth flow (`options_describe::send_authenticated`)
        // picks them up from `client.url.username/password`. The URL
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
