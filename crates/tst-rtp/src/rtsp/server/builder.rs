//! Fluent builder for [`RtspServer`](crate::RtspServer), plus the internal
//! auth carrier the server reads from it. Requires the `rtsp-server`
//! feature: the server runs on a tokio runtime that doesn't exist without it.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

#[cfg(feature = "rtsp-server-tls")]
use std::path::PathBuf;
use std::time::Duration;

use secrecy::SecretString;

use crate::error::RtspServerError;
use crate::rtsp::server::mount_rules::MountRule;
use crate::url::{RtspScheme, RtspUrl};

/// Challenge scheme the server offers a client that must authenticate.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// HTTP Basic (RFC 7617).
    Basic,
    /// HTTP Digest with MD5 (RFC 7616).
    DigestMd5,
    /// HTTP Digest with SHA-256 (RFC 7616).
    DigestSha256,
}

/// Server-side auth configuration carrier. Internal — consumed by
/// `RtspServer::from_builder`.
#[derive(Clone)]
pub(crate) struct ServerAuthConfig {
    pub(crate) scheme: AuthScheme,
    pub(crate) realm: String,
    pub(crate) username: String,
    pub(crate) password: secrecy::SecretString,
}

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
/// - `max_readers`: no cap
/// - `session_timeout`: 60 s
/// - `fanout_capacity`: 256 frames (broadcast channel size; slow peers
///   drop oldest beyond this)
/// - `graceful_shutdown_drain`: 100 ms
/// - `tls_handshake_timeout`: 30 s (`rtsps://` only)
/// - `accept_unregistered_publishers`: off
/// - `end_publish_mount_on_publisher_exit`: off
/// - `publish_idle_timeout`: off
/// - `allow_udp_media`: yes on an `rtsp://` bind, no on an `rtsps://` bind
/// - `accept_digest_without_qop`: off
/// - No auth, no TLS — caller adds via `auth_*()` / `tls_cert()`.
pub struct RtspServerBuilder {
    pub(crate) bind_url: RtspUrl,
    pub(crate) auth: Option<ServerAuthConfig>,
    pub(crate) default_mount_rule: Option<MountRule>,
    pub(crate) max_sessions: usize,
    pub(crate) max_readers: Option<usize>,
    pub(crate) session_timeout: Duration,
    pub(crate) fanout_capacity: usize,
    pub(crate) graceful_shutdown_drain: Duration,
    pub(crate) accept_unregistered_publishers: bool,
    pub(crate) end_publish_mount_on_publisher_exit: bool,
    pub(crate) publish_idle_timeout: Option<Duration>,
    pub(crate) allow_udp_media: Option<bool>,
    pub(crate) accept_digest_without_qop: bool,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_cert_path: Option<PathBuf>,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_key_path: Option<PathBuf>,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_pem: Option<(Vec<u8>, Vec<u8>)>,
    #[cfg(feature = "rtsp-server-tls")]
    pub(crate) tls_handshake_timeout: Duration,
}

impl RtspServerBuilder {
    /// Start building a server bound to `url`. The URL must use scheme
    /// `rtsp://` or `rtsps://` and a host that parses as an IP literal
    /// (no DNS resolution server-side).
    ///
    /// A bind URL must not carry a user name or a password: `build()`
    /// refuses it. Configure the server's credential with `auth_basic` or
    /// `auth_digest_*`.
    ///
    /// A bind URL says where to listen and nothing else: a scheme, a host
    /// and a port. `build()` refuses one that carries a path other than `/`,
    /// or a query, an empty `?` included. A mount is added with
    /// [`RtspServer::add_mount`](crate::rtsp::server::RtspServer::add_mount)
    /// and its siblings, never named in the bind URL; and the keys an
    /// [`RtspUrl`] accepts are a client's, none of them configures a server.
    /// In particular `transport=` does not choose what media goes over:
    /// that is [`Self::allow_udp_media`]. A key no RTSP URL knows, and a
    /// known key with a value it does not take, are refused here, by the
    /// parse.
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
    ///
    /// A bind URL must not carry a user name or a password: `build()`
    /// refuses it. Configure the server's credential with `auth_basic` or
    /// `auth_digest_*`.
    ///
    /// As for [`Self::new`], the URL may say only where to listen:
    /// `build()` refuses one that was parsed with a path other than `/` or
    /// with a query. [`Self::allow_udp_media`] decides whether media may go
    /// over UDP.
    pub fn with_url(url: RtspUrl) -> Self {
        Self {
            bind_url: url,
            auth: None,
            default_mount_rule: None,
            max_sessions: 64,
            max_readers: None,
            session_timeout: Duration::from_secs(60),
            fanout_capacity: 256,
            graceful_shutdown_drain: Duration::from_millis(100),
            accept_unregistered_publishers: false,
            end_publish_mount_on_publisher_exit: false,
            publish_idle_timeout: None,
            allow_udp_media: None,
            accept_digest_without_qop: false,
            #[cfg(feature = "rtsp-server-tls")]
            tls_cert_path: None,
            #[cfg(feature = "rtsp-server-tls")]
            tls_key_path: None,
            #[cfg(feature = "rtsp-server-tls")]
            tls_pem: None,
            #[cfg(feature = "rtsp-server-tls")]
            tls_handshake_timeout: Duration::from_secs(30),
        }
    }

    /// Require Basic auth (RFC 7617). Mutually exclusive with the
    /// `auth_digest_*` methods — calling twice overwrites; calling with
    /// a different scheme at `build()` time has the final-call wins
    /// behavior (the server holds one credential).
    ///
    /// The scheme and realm are the server's first challenge;
    /// [`MountRules::set_challenge`](crate::MountRules::set_challenge)
    /// changes it for every 401, this credential's included.
    ///
    /// A user name that contains `:` is refused by `build()`: it cannot
    /// authenticate while the challenge is Basic (RFC 7617 §2), the server's
    /// own credential included.
    pub fn auth_basic(&mut self, realm: &str, username: &str, password: SecretString) -> &mut Self {
        self.auth = Some(ServerAuthConfig {
            scheme: AuthScheme::Basic,
            realm: realm.into(),
            username: username.into(),
            password,
        });
        self
    }

    /// Require Digest MD5 (RFC 7616 §3.4). The scheme and realm are the
    /// server's first challenge, as for [`Self::auth_basic`].
    pub fn auth_digest_md5(
        &mut self,
        realm: &str,
        username: &str,
        password: SecretString,
    ) -> &mut Self {
        self.auth = Some(ServerAuthConfig {
            scheme: AuthScheme::DigestMd5,
            realm: realm.into(),
            username: username.into(),
            password,
        });
        self
    }

    /// Require Digest SHA-256 (RFC 7616 §3.4). The scheme and realm are the
    /// server's first challenge, as for [`Self::auth_basic`].
    pub fn auth_digest_sha256(
        &mut self,
        realm: &str,
        username: &str,
        password: SecretString,
    ) -> &mut Self {
        self.auth = Some(ServerAuthConfig {
            scheme: AuthScheme::DigestSha256,
            realm: realm.into(),
            username: username.into(),
            password,
        });
        self
    }

    /// Cap on concurrent client connections. Excess connections beyond
    /// this are accepted then immediately dropped with a `tracing::warn!`.
    /// Defaults to 64.
    ///
    /// A connection whose current request has not completed within the
    /// idle bound (30 s before SETUP, the session timeout plus grace after)
    /// is closed, so a trickling peer cannot hold a session slot or a
    /// publisher slot past that bound.
    ///
    /// A connection that has not completed a SETUP is closed 30 s after its
    /// request loop starts (after the TLS handshake on `rtsps://`), however
    /// many OPTIONS, GET_PARAMETERs or interleaved frames it sends, however
    /// slowly it reads the replies and whatever credential it presented,
    /// so peers that never set up a session cannot keep the slots. A
    /// connection whose session ends while it stays open (a refusal on its
    /// own mount) is closed 30 s later unless it sets up again.
    pub fn max_sessions(&mut self, n: usize) -> &mut Self {
        self.max_sessions = n.max(1);
        self
    }

    /// Cap on reader sessions at once. A reader holds a slot from its SETUP
    /// to the end of its role: its TEARDOWN, a refusal on its own mount
    /// (a PLAY that finds that mount removed is one) or the end of its
    /// connection. A reader set up again on its own or
    /// another mount keeps the one slot it holds. Readers of a multicast
    /// mount count; publishers do not. A reader's SETUP over the cap is
    /// answered `453 Not Enough Bandwidth` and its connection is closed
    /// after the reply.
    ///
    /// Unset (the default): no cap besides [`Self::max_sessions`]. `0`: no
    /// readers at all, an ingest-only server (unlike `max_sessions(0)`,
    /// which counts as 1). With `n` below `max_sessions` the difference can
    /// never be held by readers, so readers cannot use up every connection
    /// of a server that also takes publishers; a cap at or above
    /// `max_sessions` protects nothing. A connection without a session
    /// still holds one of the `max_sessions` until its admission deadline
    /// (30 s, see [`Self::max_sessions`]), so a publisher's connection is
    /// not promised. On an `rtsps://` server,
    /// [`allow_udp_media(true)`](Self::allow_udp_media) is a separate
    /// choice: the cap does not change what readers are sent over.
    ///
    /// **Stability: Provisional.**
    pub fn max_readers(&mut self, n: usize) -> &mut Self {
        self.max_readers = Some(n);
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

    /// Let an ANNOUNCE on a path no mount is registered under create a
    /// publish mount there. The new mount's handle is queued for
    /// [`RtspServer::next_publisher`](crate::rtsp::server::RtspServer::next_publisher);
    /// the queue holds 64 handles the application has not taken yet, and
    /// an ANNOUNCE that would create a 65th mount answers `503 Service
    /// Unavailable` without creating it. A later ANNOUNCE on a path an
    /// earlier one created reuses that mount and queues nothing. Defaults
    /// to off: an ANNOUNCE on an unregistered path answers `404`.
    ///
    /// Security: with this on, anyone who can reach the server's port
    /// (and pass its auth, when auth is configured) can create mounts. The
    /// 64 bound counts only handles the application has not yet taken: an
    /// on-demand mount stays in the mount table after its publisher leaves
    /// and after its handle is taken or dropped, until the application
    /// calls
    /// [`RtspServer::remove_mount`](crate::rtsp::server::RtspServer::remove_mount).
    /// Removing idle names is the application's job: the table holds at
    /// most 256 on-demand mounts, taken or not, and an ANNOUNCE that would
    /// create a 257th answers `503`. Mounts registered with
    /// [`RtspServer::add_publish_mount`](crate::rtsp::server::RtspServer::add_publish_mount)
    /// are unaffected and never count. An on-demand mount whose transport
    /// has not been taken costs about 15 KB (its reader broadcast at the
    /// default `fanout_capacity`); taking the transport preallocates about
    /// 263 KB for the application queue, which can hold up to about 8.7 MB
    /// of frames if the application stops reading it.
    ///
    /// A connected publisher costs far more than its mount. An elementary
    /// (H.264, optionally with KLV) publisher can hold about 46.8 MB at
    /// worst: an 8 MiB access unit in the depacketizer, about 17 MB in the
    /// re-muxer's queue, 8 MiB of PES scratch, 4 MiB of held KLV and the
    /// 8.7 MB application queue. At the default
    /// [`max_sessions`](Self::max_sessions) of 64 that is about 3.0 GB.
    /// Configure auth, and size `max_sessions` to the memory you can give
    /// publishers, before turning this on for a reachable port.
    pub fn accept_unregistered_publishers(&mut self, yes: bool) -> &mut Self {
        self.accept_unregistered_publishers = yes;
        self
    }

    /// Server-wide default for publish mounts: when `yes`, a mount's
    /// application transport ends (reads `Closed`) as soon as a publisher
    /// that reached RECORD leaves, and the mount refuses later publishers.
    /// A publisher that was admitted but never reached RECORD does not
    /// end it. Default `false`: the mount idles for the next publisher. A
    /// mount added with
    /// [`RtspServer::add_publish_mount_with`](crate::RtspServer::add_publish_mount_with)
    /// can override this; on-demand mounts always inherit it.
    ///
    /// To accept a new publisher after the mount ended, call
    /// [`RtspServer::remove_mount`](crate::rtsp::server::RtspServer::remove_mount)
    /// and add the mount again; a plain add finds the path taken. An ended
    /// on-demand mount stays in the table, and counts toward the on-demand
    /// cap, until it is removed.
    pub fn end_publish_mount_on_publisher_exit(&mut self, yes: bool) -> &mut Self {
        self.end_publish_mount_on_publisher_exit = yes;
        self
    }

    /// End the session of a recording publisher that sends no media for
    /// `t`, timed from RECORD or from its last RTP packet. Default off: a
    /// publisher is then governed by [`Self::session_timeout`] alone. A
    /// publisher that has not reached RECORD is never ended by this knob.
    /// A value longer than the session timeout still matters: a publisher
    /// that keeps its session alive with RTSP requests while sending no
    /// media is ended by this timeout, not the session timeout. Zero
    /// leaves it unset.
    pub fn publish_idle_timeout(&mut self, t: Duration) -> &mut Self {
        self.publish_idle_timeout = (!t.is_zero()).then_some(t);
        self
    }

    /// Whether sessions may carry media over UDP: a reader's or a
    /// publisher's SETUP with a transport that is not TCP-interleaved, and
    /// multicast mounts. A server that does not allow it answers such a
    /// SETUP `461 Unsupported Transport`.
    ///
    /// Default: yes on an `rtsp://` bind, NO on an `rtsps://` bind, so that
    /// an `rtsps://` server sends and takes media only inside the TLS
    /// connection. `allow_udp_media(true)` on an `rtsps://` server restores
    /// the behaviour of 0.10.0, in which the media travels unencrypted;
    /// `allow_udp_media(false)` makes a plain server TCP-only.
    ///
    /// **Stability: Provisional.**
    pub fn allow_udp_media(&mut self, yes: bool) -> &mut Self {
        self.allow_udp_media = Some(yes);
        self
    }

    /// Accept a Digest answer that carries no `qop`: the RFC 2069 form, which
    /// GStreamer 1.24's `rtspsrc` and `rtspclientsink` send. Applies to both
    /// Digest schemes, the server's own credential and every mount rule's;
    /// the challenge still offers `qop="auth"`, and an answer WITH `qop` is
    /// checked as before, its nonce count included. Default: off, so such an
    /// answer is refused as a downgrade.
    ///
    /// What is given up, only for a client that sends no `qop` and only with
    /// this on: replay protection inside one connection. Such an answer has
    /// no nonce count, so a captured `Authorization` value for one method and
    /// URI can be sent again on the same connection while it lives. The
    /// nonce is still 128 random bits per TCP connection and is never
    /// accepted on another.
    ///
    /// GStreamer 1.24 authenticates with Basic, or with Digest MD5 under this
    /// option, never with Digest SHA-256: it answers a SHA-256 challenge
    /// with an MD5 response.
    ///
    /// **Stability: Provisional.**
    pub fn accept_digest_without_qop(&mut self, yes: bool) -> &mut Self {
        self.accept_digest_without_qop = yes;
        self
    }

    /// Whether this server allows media over UDP: the value set with
    /// [`Self::allow_udp_media`], else whether the bind is `rtsp://`.
    pub(crate) fn udp_media_allowed(&self) -> bool {
        self.allow_udp_media
            .unwrap_or(!matches!(self.bind_url.scheme(), RtspScheme::Rtsps))
    }

    /// Set the rule every mount path without its own entry in
    /// [`RtspServer::mount_rules`](crate::rtsp::server::RtspServer::mount_rules)
    /// falls back to, and the rule a path rule's `Inherit` direction takes.
    /// The default is [`MountRule::default`], which inherits the server-wide
    /// credential for both directions; the rule is not taken from `auth_*`,
    /// whose credential `Inherit` reaches and no rule edit removes. The rule
    /// is checked
    /// at [`build`](Self::build), as
    /// [`MountRules::set_default`](crate::MountRules::set_default) checks it.
    pub fn default_mount_rule(&mut self, rule: MountRule) -> &mut Self {
        self.default_mount_rule = Some(rule);
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
        self.tls_pem = None;
        self
    }

    /// [`Self::tls_cert`] with the PEM in memory instead of in files (the C
    /// binding's `tst_rtsp_server_builder_tls_cert_pem` takes bytes). The
    /// last of the two calls wins. Loaded and validated at
    /// [`RtspServer::start`](crate::rtsp::server::RtspServer::start), which
    /// also refuses a plaintext `rtsp://` bind with TLS material configured.
    #[cfg(feature = "rtsp-server-tls")]
    pub fn tls_cert_pem(&mut self, cert_chain_pem: Vec<u8>, key_pem: Vec<u8>) -> &mut Self {
        self.tls_pem = Some((cert_chain_pem, key_pem));
        self.tls_cert_path = None;
        self.tls_key_path = None;
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
    /// ready for `RtspServer::start`.
    /// Internally constructs the tokio Runtime and validates the
    /// configuration.
    ///
    /// # Errors
    ///
    /// - [`RtspServerError::Io`] on Runtime construction failure (rare)
    /// - [`RtspServerError::InvalidServerConfig`] if the
    ///   [`default_mount_rule`](Self::default_mount_rule) holds a `Require`
    ///   policy with no credential, a credential with an empty username or a
    ///   username listed twice; if an `auth_*` realm holds a control byte; if
    ///   the `auth_*` user name is empty, or contains `:` with
    ///   [`auth_basic`](Self::auth_basic); or if the bind URL carries a user
    ///   name or a password, a path other than `/`, or a query
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
    use secrecy::SecretString;

    #[test]
    fn a_zero_publish_idle_timeout_leaves_it_unset() {
        let mut b = RtspServerBuilder::new("rtsp://0.0.0.0:8554").unwrap();
        b.publish_idle_timeout(Duration::from_secs(5));
        assert_eq!(b.publish_idle_timeout, Some(Duration::from_secs(5)));
        b.publish_idle_timeout(Duration::ZERO);
        assert_eq!(b.publish_idle_timeout, None);
    }

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
        assert!(matches!(cfg.scheme, AuthScheme::Basic));
        assert_eq!(cfg.realm, "tst");
        assert_eq!(cfg.username, "admin");
    }

    #[test]
    fn build_refuses_a_realm_with_control_bytes() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.auth_basic("a\nb", "admin", SecretString::new("p".into()));
        assert!(matches!(
            b.build(),
            Err(RtspServerError::InvalidServerConfig { .. })
        ));
    }

    #[test]
    fn build_refuses_an_empty_username() {
        for scheme in 0..3 {
            let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            let p = SecretString::new("p".into());
            match scheme {
                0 => b.auth_basic("tst", "", p),
                1 => b.auth_digest_md5("tst", "", p),
                _ => b.auth_digest_sha256("tst", "", p),
            };
            assert!(matches!(
                b.build(),
                Err(RtspServerError::InvalidServerConfig { .. })
            ));
        }
    }

    #[test]
    fn rtsp_server_builder_auth_overwrites() {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        b.auth_basic("tst", "admin", SecretString::new("p".into()));
        b.auth_digest_md5("tst", "admin", SecretString::new("p".into()));
        assert!(matches!(
            b.auth.as_ref().unwrap().scheme,
            AuthScheme::DigestMd5
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
    fn rtsp_server_builder_build_succeeds_after_build() {
        // build() constructs the real Runtime-backed server without
        // starting it.
        let b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        let server = b
            .build()
            .expect("post-T7 build returns the real RtspServer");
        // local_addr is None until start() runs the listener.
        assert!(server.local_addr().is_none());
    }

    /// `tls_cert` and `tls_cert_pem` configure one slot between them: the
    /// last call wins and clears the other form.
    #[cfg(feature = "rtsp-server-tls")]
    #[test]
    fn the_last_of_tls_cert_and_tls_cert_pem_wins() {
        let mut b = RtspServerBuilder::new("rtsps://127.0.0.1:0").unwrap();
        b.tls_cert("cert.pem".into(), "key.pem".into());
        b.tls_cert_pem(b"cert".to_vec(), b"key".to_vec());
        assert!(b.tls_cert_path.is_none() && b.tls_key_path.is_none());
        assert_eq!(b.tls_pem, Some((b"cert".to_vec(), b"key".to_vec())));

        b.tls_cert("cert.pem".into(), "key.pem".into());
        assert!(b.tls_pem.is_none());
        assert_eq!(
            b.tls_cert_path.as_deref(),
            Some(std::path::Path::new("cert.pem"))
        );
        assert_eq!(
            b.tls_key_path.as_deref(),
            Some(std::path::Path::new("key.pem"))
        );
    }
}
