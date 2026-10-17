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
use crate::url::RtspUrl;

/// Server-side auth scheme. Internal — consumed by
/// `RtspServer::from_builder`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ServerAuthScheme {
    Basic,
    DigestMd5,
    DigestSha256,
}

/// Server-side auth configuration carrier. Internal — consumed by
/// `RtspServer::from_builder`.
#[derive(Clone)]
pub(crate) struct ServerAuthConfig {
    pub(crate) scheme: ServerAuthScheme,
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
/// - `session_timeout`: 60 s
/// - `fanout_capacity`: 256 frames (broadcast channel size; slow peers
///   drop oldest beyond this)
/// - `graceful_shutdown_drain`: 100 ms
/// - `tls_handshake_timeout`: 30 s (`rtsps://` only)
/// - `accept_unregistered_publishers`: off
/// - No auth, no TLS — caller adds via `auth_*()` / `tls_cert()`.
pub struct RtspServerBuilder {
    pub(crate) bind_url: RtspUrl,
    pub(crate) auth: Option<ServerAuthConfig>,
    pub(crate) max_sessions: usize,
    pub(crate) session_timeout: Duration,
    pub(crate) fanout_capacity: usize,
    pub(crate) graceful_shutdown_drain: Duration,
    pub(crate) accept_unregistered_publishers: bool,
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
            accept_unregistered_publishers: false,
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
    ///
    /// A connection whose current request has not completed within the
    /// idle bound (30 s before SETUP, the session timeout plus grace after)
    /// is closed, so a trickling peer cannot hold a session slot or a
    /// publisher slot past that bound.
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
