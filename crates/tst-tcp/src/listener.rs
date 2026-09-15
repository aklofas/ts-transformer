//! [`TcpListener`] — sync TCP listener that accepts new `TcpTransport` connections.

use std::io;
use std::net::TcpListener as StdTcpListener;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::net::udp_socket::CANCEL_POLL_INTERVAL;

use crate::config::SocketConfig;
use crate::error::TcpError;
use crate::transport::{TcpCancelHandle, TcpTransport};

/// Sync TCP listener.
///
/// Construct via [`TcpListener::bind`], then call [`TcpListener::accept_blocking`] to
/// receive a fresh [`TcpTransport`] per inbound connection.
///
/// Unlike the connectionless transports (UDP, RTP, RIST), which expose a single
/// `listen(url)` one-shot factory, TCP is connection-oriented: one `TcpListener`
/// instance serves multiple peers, each accepted call returning its own
/// `TcpTransport`. `TcpListener::from_url` is the URL-style alternative to
/// `TcpListener::bind`; pass a `tcp://host:port?listen=1` URL to construct the
/// listener without a raw `SocketAddr`. See the receive-side entry-points
/// table in `docs/reference/compatibility.md` for a side-by-side comparison
/// of all transport receive-entry patterns.
pub struct TcpListener {
    inner: StdTcpListener,
    config: SocketConfig,
    /// Dropped by [`Self::close`] / [`TcpCancelHandle::cancel`]; polled by
    /// [`Self::accept_blocking`] every [`CANCEL_POLL_INTERVAL`].
    alive: Arc<AtomicBool>,
    #[cfg(feature = "tls")]
    tls_config: Option<Arc<rustls::ServerConfig>>,
}

impl TcpListener {
    /// Bind a TCP listener on `addr`. Returns a listener ready for `accept_blocking`.
    pub fn bind(addr: SocketAddr) -> Result<Self, TcpError> {
        let inner = StdTcpListener::bind(addr).map_err(TcpError::Io)?;
        // Non-blocking so `accept_blocking` can poll the `alive` flag between
        // `WouldBlock`s (std has no accept timeout and no way to wake a
        // parked accept(2) from another thread). Accepted streams are put
        // back into blocking mode before they are handed out.
        inner.set_nonblocking(true).map_err(TcpError::Io)?;
        Ok(Self {
            inner,
            config: SocketConfig::default(),
            alive: Arc::new(AtomicBool::new(true)),
            #[cfg(feature = "tls")]
            tls_config: None,
        })
    }

    /// Bind from a `tcp://0.0.0.0:port?listen=1` URL.
    pub fn from_url(url: &str) -> Result<Self, TcpError> {
        let parsed = crate::url::TcpUrl::parse(url)?;
        Self::from_parsed(&parsed)
    }

    /// Bind from an already-parsed `TcpUrl`. Skips the format-then-reparse
    /// round trip `TcpListenerBuilder::build` used to go through — a source
    /// of the class of bug where a field the formatter forgets to re-emit
    /// silently vanishes on `build()`.
    pub(crate) fn from_parsed(parsed: &crate::url::TcpUrl) -> Result<Self, TcpError> {
        if !parsed.listen {
            return Err(TcpError::InvalidConfig(
                "URL does not have ?listen=1 — use TcpTransport::connect for caller-side".into(),
            ));
        }

        // TcpUrl::parse already rejected a non-literal host for a
        // ?listen=1 URL (TcpUrlError::BadHost), so this can't-happen path
        // guards a hand-built TcpUrl that skipped that check (TcpUrl's
        // fields are public and the struct isn't marked non-exhaustive)
        // instead of panicking if `from_parsed` is ever promoted to a
        // public API.
        let ip: IpAddr = parsed
            .host
            .parse()
            .map_err(|_| TcpError::Url(crate::url::TcpUrlError::BadHost(parsed.host.clone())))?;
        let bind_addr = SocketAddr::new(ip, parsed.port);
        let mut listener = Self::bind(bind_addr)?;
        listener.config.merge_from_url(parsed);

        #[cfg(feature = "tls")]
        if parsed.tls {
            let cert = parsed.cert.as_deref().ok_or_else(|| {
                TcpError::InvalidConfig("tcps:// listener requires ?cert=path".into())
            })?;
            let key = parsed.key.as_deref().ok_or_else(|| {
                TcpError::InvalidConfig("tcps:// listener requires ?key=path".into())
            })?;
            let server_cfg = crate::tls::load_server_config(cert, key)?;
            listener.tls_config = Some(Arc::new(server_cfg));
        }
        #[cfg(not(feature = "tls"))]
        if parsed.tls {
            return Err(TcpError::TlsDisabled);
        }

        Ok(listener)
    }

    /// Local address the listener bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Set the SocketConfig that will be applied to accepted connections.
    pub fn config_mut(&mut self) -> &mut SocketConfig {
        &mut self.config
    }

    /// Block until a connection arrives. Returns a fully-configured TcpTransport.
    ///
    /// Cancellable: [`Self::cancel_handle`] / [`Self::close`] drop the alive
    /// flag and a parked accept returns [`TcpError::Closed`] at its next
    /// ~100 ms poll; every later call returns it at the entry check.
    pub fn accept_blocking(&self) -> Result<TcpTransport, TcpError> {
        let (sock, peer) = loop {
            if !self.alive.load(Ordering::Acquire) {
                return Err(TcpError::Closed);
            }
            match self.inner.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Nothing pending: sleep one poll tick, then re-check
                    // the flag — the same cadence the transports' socket
                    // timeouts give a parked recv/send.
                    std::thread::sleep(CANCEL_POLL_INTERVAL);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => { /* EINTR: retry */ }
                Err(e) => return Err(TcpError::Io(e)),
            }
        };
        // The listening socket is non-blocking only so the loop above can
        // observe `alive`. The accepted stream must NOT be: `apply_knobs`'
        // read/write timeouts are meaningless on a non-blocking socket and
        // every recv/send would spin on WouldBlock. Linux does not inherit
        // O_NONBLOCK across accept(2); BSD/macOS and Windows do — so set it
        // explicitly rather than rely on the platform.
        sock.set_nonblocking(false).map_err(TcpError::Io)?;

        #[cfg(feature = "tls")]
        if let Some(tls_cfg) = &self.tls_config {
            return crate::tls::accept_tls(sock, peer, &self.config, tls_cfg.clone());
        }

        TcpTransport::from_accepted_plain(sock, peer, &self.config)
    }

    /// Cloneable handle that unblocks a parked [`Self::accept_blocking`] from
    /// another thread. Obtain it BEFORE moving the listener into the accept
    /// thread. See [`TcpCancelHandle`] for the cooperative-cancel contract.
    pub fn cancel_handle(&self) -> TcpCancelHandle {
        TcpCancelHandle::from_flag(self.alive.clone())
    }

    /// Close the listener from any thread: a parked `accept_blocking` returns
    /// [`TcpError::Closed`] within ~100 ms and every later call returns it
    /// immediately. Idempotent. The OS socket stays bound until `self` is
    /// dropped (std cannot shut a listener down explicitly), so bind the
    /// next listener on a fresh port or drop this one first.
    pub fn close(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

#[cfg(all(test, unix))]
mod accepted_stream_mode_tests {
    use super::TcpListener;
    use crate::transport::InnerStream;
    use std::os::fd::AsRawFd;

    /// The listening socket is non-blocking (the cancel poll needs it); the
    /// accepted stream must be blocking or the transports' 100 ms socket
    /// timeouts stop meaning anything. Linux happens not to inherit
    /// O_NONBLOCK across accept(2) — this pins the explicit reset so the
    /// BSD/macOS/Windows inheritance cannot regress silently.
    #[test]
    fn accepted_stream_is_blocking_even_though_listener_is_not() {
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = listener.local_addr().unwrap().port();
        let _peer = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let accepted = listener.accept_blocking().unwrap();

        // SAFETY: F_GETFL takes no argument and only reads the descriptor's
        // status flags; the fd is owned by `listener` for the whole call.
        let listen_flags = unsafe { libc::fcntl(listener.inner.as_raw_fd(), libc::F_GETFL) };
        assert!(
            listen_flags & libc::O_NONBLOCK != 0,
            "listener must poll non-blocking"
        );

        // Irrefutable (and clippy says so) when the `tls` feature is off —
        // `InnerStream` has only the `Plain` variant in that build.
        #[allow(irrefutable_let_patterns)]
        let InnerStream::Plain(stream) = &accepted.inner else {
            panic!("plain listener must hand back a plain stream");
        };
        // SAFETY: as above — read-only query on a descriptor `accepted` owns.
        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(
            flags & libc::O_NONBLOCK,
            0,
            "accepted stream must be blocking"
        );
    }
}
