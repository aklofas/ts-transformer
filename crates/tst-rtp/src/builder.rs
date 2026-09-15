//! Fluent builders over [`RtpUrl`] for callers who'd rather construct
//! transports field-by-field than parse a string.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Mirrors the `SocketBuilder` / `ListenerBuilder` setter shape from
//! `tst-srt` — `&mut self -> &mut Self` setters, with a terminal `build`
//! that takes `self` by value.
//!
//! `build` delegates to [`RtpTransport::connect_with_rtcp`] /
//! [`RtpRecvTransport::listen_with_rtcp`]; the builder is sugar, not a
//! parallel implementation.

#[cfg(feature = "rtsp-server-tls")]
use std::path::PathBuf;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};

use crate::error::RtspError;
#[cfg(feature = "rtsp-server")]
use crate::error::RtspServerError;
use crate::rtsp::client::{ConnectParams, RtspClient};
use crate::rtsp::message::validate_header_field;
use crate::transport::{ConnectError, RtpRecvTransport, RtpTransport};
use crate::url::{RtpUrl, RtspTransportPref, RtspUrl, UrlError as RtpUrlError};

/// Fluent builder for send-side [`RtpTransport`].
#[must_use]
#[derive(Debug, Clone)]
pub struct RtpSocketBuilder {
    url: RtpUrl,
    /// Whether to auto-bind the RTCP companion socket on `port + 1`
    /// per RFC 3550 §11 and spawn the periodic SR reporter. Default
    /// `false` — the reporter is experimental and emits placeholder
    /// (zero) statistics (see [`Self::rtcp`]).
    rtcp: bool,
}

impl RtpSocketBuilder {
    /// New builder targeting `host:port`. `host` must be a literal IP
    /// (multicast group or unicast destination).
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            url: RtpUrl {
                host: host.into(),
                port,
                ttl: None,
                iface: None,
                pkt_size: None,
                ssrc: None,
                pt: None,
                recv_timeout: None,
            },
            rtcp: false,
        }
    }

    /// Parse an `rtp://host:port[?...]` URL into a builder.
    pub fn from_url(url: &str) -> Result<Self, RtpUrlError> {
        let parsed = RtpUrl::parse(url)?;
        Ok(Self {
            url: parsed,
            rtcp: false,
        })
    }

    /// Override TTL / hop-limit for multicast send (`IP_MULTICAST_TTL` /
    /// `IPV6_MULTICAST_HOPS`). Unicast: ignored.
    pub fn ttl(&mut self, n: u8) -> &mut Self {
        self.url.ttl = Some(n);
        self
    }

    /// Override the multicast interface (`IP_MULTICAST_IF`). IPv4: takes
    /// a literal IPv4 address (`"192.168.1.50"`).
    pub fn iface(&mut self, name_or_ip: impl Into<String>) -> &mut Self {
        self.url.iface = Some(name_or_ip.into());
        self
    }

    /// UDP payload size (188-multiple). Default 1316.
    pub fn pkt_size(&mut self, n: usize) -> &mut Self {
        self.url.pkt_size = Some(n);
        self
    }

    /// Override the RTP SSRC. Default: random.
    pub fn ssrc(&mut self, n: u32) -> &mut Self {
        self.url.ssrc = Some(n);
        self
    }

    /// Enable or disable the auto-bound RTCP companion socket on
    /// `port + 1` and the periodic SR reporter thread. Default `false`.
    ///
    /// The SR reporter is **experimental**: when enabled it emits
    /// **placeholder (zero) statistics** (no live sender counters wired
    /// in) and is therefore **NOT RFC 3550-conformant**. Pass `true`
    /// only to exercise the RTCP socket-pair plumbing. RTCP *reception*
    /// is unaffected by this toggle.
    pub fn rtcp(mut self, enabled: bool) -> Self {
        self.rtcp = enabled;
        self
    }

    /// Build the transport.
    pub fn build(self) -> Result<RtpTransport, ConnectError> {
        RtpTransport::connect_with_rtcp(&self.url, self.rtcp)
    }
}

/// Fluent builder for recv-side [`RtpRecvTransport`].
#[must_use]
#[derive(Debug, Clone)]
pub struct RtpRecvSocketBuilder {
    url: RtpUrl,
    /// Whether to auto-bind the RTCP companion socket on `port + 1`
    /// per RFC 3550 §11 and spawn the periodic RR reporter. Default
    /// `false` — the reporter is experimental and emits placeholder
    /// (zero) statistics (see [`Self::rtcp`]).
    rtcp: bool,
}

impl RtpRecvSocketBuilder {
    /// New builder bound to `host:port` for listening. For multicast,
    /// `host` is the group address (the socket binds to ANY:port and
    /// joins `group`).
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            url: RtpUrl {
                host: host.into(),
                port,
                ttl: None,
                iface: None,
                pkt_size: None,
                ssrc: None,
                pt: None,
                recv_timeout: None,
            },
            rtcp: false,
        }
    }

    /// Parse an `rtp://host:port[?...]` URL into a builder.
    pub fn from_url(url: &str) -> Result<Self, RtpUrlError> {
        let parsed = RtpUrl::parse(url)?;
        Ok(Self {
            url: parsed,
            rtcp: false,
        })
    }

    /// Override the multicast-recv interface.
    pub fn iface(&mut self, name_or_ip: impl Into<String>) -> &mut Self {
        self.url.iface = Some(name_or_ip.into());
        self
    }

    /// Enable or disable the auto-bound RTCP companion socket on
    /// `port + 1` and the periodic RR reporter thread. Default `false`.
    ///
    /// The RR reporter is **experimental**: when enabled it emits
    /// **placeholder (zero) statistics** (empty report blocks, no live
    /// receiver counters) and is therefore **NOT RFC 3550-conformant**.
    /// Pass `true` only to exercise the RTCP socket-pair plumbing. RTCP
    /// *reception* is unaffected by this toggle.
    pub fn rtcp(mut self, enabled: bool) -> Self {
        self.rtcp = enabled;
        self
    }

    /// Build the recv transport.
    pub fn build(self) -> Result<RtpRecvTransport, ConnectError> {
        RtpRecvTransport::listen_with_rtcp(&self.url, self.rtcp)
    }
}

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

    /// Per-read socket timeout (poll interval for cancel + interleaved
    /// frame reads). Default 100 ms.
    pub fn read_timeout(mut self, t: Duration) -> Self {
        self.read_timeout = t;
        self
    }

    /// Per-request response deadline: how long `options()` / `describe()` /
    /// `setup_*()` / `play()` / `pause()` / `get_parameter()` / `teardown()`
    /// wait for the server's reply once the request is written. Expiry
    /// returns [`RtspError::Timeout`]. `None` disables the deadline (the
    /// pre-`request_timeout` behaviour: a silent server parks the call
    /// until the cancel handle fires). Default 10 s, the same as
    /// [`Self::connect_timeout`].
    ///
    /// After a `Timeout` treat the control connection as indeterminate and
    /// build a fresh client: the late response, if it ever arrives, is still
    /// in the socket and the non-interleaved read path does not match
    /// responses by CSeq.
    pub fn request_timeout(mut self, t: Option<Duration>) -> Self {
        self.request_timeout = t;
        self
    }

    /// Override the rustls root certificate store for `rtsps://`
    /// connections. Default: webpki-roots / system roots per the `tls`
    /// module's policy.
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
        // smuggle a second header or whole request onto the wire (T1-UA-CRLF).
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

// ----------------------------------------------------------------------
// Phase 3 — server-side builder. Requires the `rtsp-server` feature: it
// builds a `crate::rtsp::server::RtspServer`, which doesn't exist without
// the feature's tokio Runtime.
// ----------------------------------------------------------------------

/// Server-side auth scheme. Internal — consumed by Task 7's
/// `RtspServer::from_builder`.
#[cfg(feature = "rtsp-server")]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ServerAuthScheme {
    Basic,
    DigestMd5,
    DigestSha256,
}

/// Server-side auth configuration carrier. Internal — consumed by
/// Task 7's `RtspServer::from_builder`.
#[cfg(feature = "rtsp-server")]
#[derive(Clone)]
pub(crate) struct ServerAuthConfig {
    pub(crate) scheme: ServerAuthScheme,
    pub(crate) realm: String,
    pub(crate) username: String,
    pub(crate) password: secrecy::SecretString,
}

#[cfg(feature = "rtsp-server")]
impl std::fmt::Debug for ServerAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerAuthConfig")
            .field("scheme", &self.scheme)
            .field("realm", &self.realm)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Builder for an [`crate::rtsp::server::RtspServer`].
///
/// Chainable `&mut self -> &mut Self` shape per workspace FFI-readiness
/// convention. Build with [`Self::build`].
///
/// # Defaults
///
/// - `max_sessions`: 64
/// - `session_timeout`: 60 s
/// - `fanout_capacity`: 256 frames (broadcast channel size; slow peers
///   drop oldest beyond this)
/// - `graceful_shutdown_drain`: 100 ms
/// - `tls_handshake_timeout`: 30 s (`rtsps://` only)
/// - No auth, no TLS — caller adds via `auth_*()` / `tls_cert()`.
#[cfg(feature = "rtsp-server")]
pub struct RtspServerBuilder {
    pub(crate) bind_url: RtspUrl,
    pub(crate) auth: Option<ServerAuthConfig>,
    pub(crate) max_sessions: usize,
    pub(crate) session_timeout: Duration,
    pub(crate) fanout_capacity: usize,
    pub(crate) graceful_shutdown_drain: Duration,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_cert_path: Option<PathBuf>,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_key_path: Option<PathBuf>,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_handshake_timeout: Duration,
}

#[cfg(feature = "rtsp-server")]
impl RtspServerBuilder {
    /// Start building a server bound to `url`. The URL must use scheme
    /// `rtsp://` or `rtsps://` and a host that parses as an IP literal
    /// (no DNS resolution server-side).
    ///
    /// # Errors
    ///
    /// - [`RtspServerError::UrlParse`] if `url` cannot be parsed as RTSP
    ///   or its host is not an IP literal.
    pub fn new(url: &str) -> Result<Self, RtspServerError> {
        let parsed = RtspUrl::parse(url).map_err(RtspServerError::UrlParse)?;
        parsed
            .validate_for_server_bind()
            .map_err(RtspServerError::UrlParse)?;
        Ok(Self::with_url(parsed))
    }

    /// Start building from an already-parsed [`RtspUrl`]. Skips the
    /// `validate_for_server_bind` check — caller is responsible.
    pub fn with_url(url: RtspUrl) -> Self {
        Self {
            bind_url: url,
            auth: None,
            max_sessions: 64,
            session_timeout: Duration::from_secs(60),
            fanout_capacity: 256,
            graceful_shutdown_drain: Duration::from_millis(100),
            #[cfg(feature = "rtsp-server-tls")]
            tls_cert_path: None,
            #[cfg(feature = "rtsp-server-tls")]
            tls_key_path: None,
            #[cfg(feature = "rtsp-server-tls")]
            tls_handshake_timeout: Duration::from_secs(30),
        }
    }

    /// Require Basic auth (RFC 7617). Mutually exclusive with the
    /// `auth_digest_*` methods — calling twice overwrites; calling with
    /// a different scheme at `build()` time has the final-call wins
    /// behavior (this builder is single-user-only in v1).
    pub fn auth_basic(&mut self, realm: &str, username: &str, password: SecretString) -> &mut Self {
        self.auth = Some(ServerAuthConfig {
            scheme: ServerAuthScheme::Basic,
            realm: realm.into(),
            username: username.into(),
            password,
        });
        self
    }

    /// Require Digest MD5 (RFC 7616 §3.4).
    pub fn auth_digest_md5(
        &mut self,
        realm: &str,
        username: &str,
        password: SecretString,
    ) -> &mut Self {
        self.auth = Some(ServerAuthConfig {
            scheme: ServerAuthScheme::DigestMd5,
            realm: realm.into(),
            username: username.into(),
            password,
        });
        self
    }

    /// Require Digest SHA-256 (RFC 7616 §3.4).
    pub fn auth_digest_sha256(
        &mut self,
        realm: &str,
        username: &str,
        password: SecretString,
    ) -> &mut Self {
        self.auth = Some(ServerAuthConfig {
            scheme: ServerAuthScheme::DigestSha256,
            realm: realm.into(),
            username: username.into(),
            password,
        });
        self
    }

    /// Cap on concurrent client connections. Excess connections beyond
    /// this are accepted then immediately dropped with a `tracing::warn!`.
    /// Defaults to 64.
    pub fn max_sessions(&mut self, n: usize) -> &mut Self {
        self.max_sessions = n.max(1);
        self
    }

    /// Advertise this session timeout to clients via the `Session:
    /// <id>;timeout=N` response header. Clients are expected to send
    /// keepalive pings at timeout/2. Defaults to 60 s.
    pub fn session_timeout(&mut self, t: Duration) -> &mut Self {
        self.session_timeout = t;
        self
    }

    /// Broadcast channel capacity (per mount). When a peer's per-session
    /// task can't keep up, broadcast drops the oldest frames for that
    /// peer; the per-peer dropped-frame counter ticks but the muxer is
    /// not back-pressured. Defaults to 256.
    pub fn fanout_capacity(&mut self, frames: usize) -> &mut Self {
        self.fanout_capacity = frames.max(1);
        self
    }

    /// Maximum drain window after `stop()` is called and the
    /// session-end Notice has been emitted. Defaults to 100 ms.
    pub fn graceful_shutdown_drain(&mut self, t: Duration) -> &mut Self {
        self.graceful_shutdown_drain = t;
        self
    }

    /// Configure TLS cert chain + private key paths (PEM format) for an
    /// `rtsps://` bind. The cert + key are loaded and validated at
    /// [`RtspServer::start`](crate::rtsp::server::RtspServer::start) time;
    /// missing or malformed files surface as [`RtspServerError::Tls`]
    /// from `start()` (never a silently-dead listener).
    ///
    /// A plaintext `rtsp://` bind with TLS paths configured is likewise
    /// refused by `start()`: configured-but-ignored TLS material would
    /// silently serve unencrypted.
    #[cfg(feature = "rtsp-server-tls")]
    pub fn tls_cert(&mut self, cert_chain_pem: PathBuf, key_pem: PathBuf) -> &mut Self {
        self.tls_cert_path = Some(cert_chain_pem);
        self.tls_key_path = Some(key_pem);
        self
    }

    /// Deadline for the TLS handshake on an `rtsps://` bind. An accepted TCP
    /// connection that has not completed its handshake within `t` is dropped
    /// and its `max_sessions` slot released. Without a deadline a silent
    /// connect (no ClientHello, ever) held its slot until the peer went away:
    /// `max_sessions` such connects — no credentials needed — wedged the
    /// server at its cap for good. Defaults to 30 s, the same bound the
    /// request loop applies to an idle read.
    #[cfg(feature = "rtsp-server-tls")]
    pub fn tls_handshake_timeout(&mut self, t: Duration) -> &mut Self {
        self.tls_handshake_timeout = t;
        self
    }

    /// Consume the builder and produce an [`crate::rtsp::server::RtspServer`]
    /// ready for `RtspServer::start` (introduced in Phase 3 Task 7).
    /// Internally constructs the tokio Runtime and validates the
    /// configuration.
    ///
    /// # Errors
    ///
    /// - [`RtspServerError::Io`] on Runtime construction failure (rare)
    ///
    /// TLS cert/key paths are NOT validated here — they are loaded at
    /// [`RtspServer::start`](crate::rtsp::server::RtspServer::start), which
    /// returns [`RtspServerError::Tls`] on any load/parse failure.
    pub fn build(self) -> Result<crate::rtsp::server::RtspServer, RtspServerError> {
        crate::rtsp::server::RtspServer::from_builder(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_builder_round_trip_via_build() {
        // Use port 1 — likely refused on bind→connect→sendto, but the
        // builder fields should propagate through connect_with_rtcp cleanly.
        let mut b = RtpSocketBuilder::new("127.0.0.1", 1);
        b.pkt_size(376).ssrc(0xABCDEF01);
        let t = b
            .build()
            .expect("connect to 127.0.0.1:1 should bind locally");
        // The Transport trait's max_payload() returns the application-
        // visible cap (pkt_size minus the 12-byte RTP header).
        use tst_core::transport::Transport;
        assert_eq!(t.max_payload(), 376 - 12);
    }

    #[test]
    fn recv_builder_binds_unicast() {
        let b = RtpRecvSocketBuilder::new("127.0.0.1", 0);
        // Port 0 -> OS-assigned; doesn't conflict.
        let _ = b.build().expect("bind 127.0.0.1:0 should succeed");
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

    // --- B6: User-Agent / credential header injection rejected at connect,
    // before the socket opens (T1-UA-CRLF). ---

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

#[cfg(all(test, feature = "rtsp-server"))]
mod phase3_server_builder_tests {
    use super::*;
    use secrecy::SecretString;

    #[test]
    fn rtsp_server_builder_new_parses_url() {
        let b = RtspServerBuilder::new("rtsp://0.0.0.0:8554").unwrap();
        assert_eq!(b.bind_url.host, "0.0.0.0");
        assert_eq!(b.bind_url.port, 8554);
        assert!(b.auth.is_none());
        assert_eq!(b.max_sessions, 64);
    }

    #[test]
    fn rtsp_server_builder_loopback_port_zero() {
        let b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        assert_eq!(b.bind_url.port, 0);
    }

    #[test]
    fn rtsp_server_builder_chainable() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.max_sessions(100).fanout_capacity(512);
        assert_eq!(b.max_sessions, 100);
        assert_eq!(b.fanout_capacity, 512);
    }

    #[test]
    fn rtsp_server_builder_auth_basic() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.auth_basic("tst", "admin", SecretString::new("p".into()));
        let cfg = b.auth.as_ref().unwrap();
        assert!(matches!(cfg.scheme, ServerAuthScheme::Basic));
        assert_eq!(cfg.realm, "tst");
        assert_eq!(cfg.username, "admin");
    }

    #[test]
    fn rtsp_server_builder_auth_overwrites() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.auth_basic("tst", "admin", SecretString::new("p".into()));
        b.auth_digest_md5("tst", "admin", SecretString::new("p".into()));
        assert!(matches!(
            b.auth.as_ref().unwrap().scheme,
            ServerAuthScheme::DigestMd5
        ));
    }

    #[test]
    fn rtsp_server_builder_min_caps() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.max_sessions(0); // floor to 1
        assert_eq!(b.max_sessions, 1);
        b.fanout_capacity(0); // floor to 1
        assert_eq!(b.fanout_capacity, 1);
    }

    #[test]
    fn rtsp_server_bind_rejects_dns_name() {
        let res = RtspServerBuilder::new("rtsp://example.com:8554");
        assert!(res.is_err());
    }

    #[test]
    fn rtsp_server_builder_build_succeeds_post_t7() {
        // Originally this test asserted Err(NotStarted) because
        // RtspServer::from_builder was a stub. The real Runtime-building impl
        // replaced the stub, so build() now returns Ok.
        // Test renamed + retargeted accordingly.
        let b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        let server = b
            .build()
            .expect("post-T7 build returns the real RtspServer");
        // local_addr is None until start() runs the listener.
        assert!(server.local_addr().is_none());
    }
}
