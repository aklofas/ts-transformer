//! [`TcpListener`] — sync TCP listener that accepts new `TcpTransport` connections.

use std::io;
use std::net::TcpListener as StdTcpListener;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::net::udp_socket::ACCEPT_POLL_INTERVAL;

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
    /// [`Self::accept_blocking`] every [`ACCEPT_POLL_INTERVAL`] (shorter than
    /// the `CANCEL_POLL_INTERVAL` the recv/send paths use — see that
    /// constant's doc for why).
    alive: Arc<AtomicBool>,
    /// Set only by [`TcpCancelHandle::cancel`] / [`Self::close`] — the cancel
    /// latch the handle reports, kept apart from `alive` (WP-C1).
    cancelled: Arc<AtomicBool>,
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
            cancelled: Arc::new(AtomicBool::new(false)),
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
    /// [`ACCEPT_POLL_INTERVAL`] poll (~5 ms); every later call returns it at
    /// the entry check. A close that lands after a connection has already
    /// been pulled off the kernel backlog but before this method returns is
    /// also caught (a re-check right after `accept()` succeeds) — the
    /// accepted socket is dropped and this still returns
    /// [`TcpError::Closed`], so a caller never receives a stream after its
    /// `close()`/`cancel()` call has returned on another thread.
    ///
    /// A cancelled listener's parked/later `accept_blocking` keeps returning
    /// [`TcpError::Closed`] — the `TcpError` layer carries no cancel variant,
    /// and a listener is not a `Transport` (WP-C2 decision), so the
    /// `ExplicitClose` the transports report has no home here. Read
    /// [`Self::cancel_handle`]`().is_cancelled()` to tell a cancel from a
    /// plain close.
    ///
    /// **Accept latency ceiling:** unlike `recv_bytes`/`send_bytes`, whose
    /// `SO_RCVTIMEO`/`SO_SNDTIMEO` wake the thread the instant data or
    /// buffer space is available (the timeout only bounds the *cancel*
    /// check, not the I/O itself), a non-blocking `accept()` loop has no
    /// such wakeup — every pending connection waits out the current sleep
    /// before the next attempt notices it. So a connection can sit for up
    /// to one [`ACCEPT_POLL_INTERVAL`] tick (~5 ms worst case) before this
    /// call returns it, even though the peer's SYN completed instantly.
    pub fn accept_blocking(&self) -> Result<TcpTransport, TcpError> {
        let (sock, peer) = loop {
            if !self.alive.load(Ordering::Acquire) {
                return Err(TcpError::Closed);
            }
            match self.inner.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Nothing pending: sleep one short poll tick, then
                    // re-check the flag. ACCEPT_POLL_INTERVAL (5 ms), not
                    // CANCEL_POLL_INTERVAL (100 ms): a parked recv/send
                    // wakes on its own via SO_RCVTIMEO/SO_SNDTIMEO the
                    // instant something arrives, but this loop's own sleep
                    // IS the only thing standing between an already-landed
                    // connection and this call noticing it, so it has to be
                    // short enough that the added latency doesn't eat into
                    // a caller's own I/O deadline on the freshly accepted
                    // connection (CORR-12 review: a TLS handshake's first
                    // write occasionally raced past its ~100 ms deadline
                    // when this used the 100 ms interval).
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => { /* EINTR: retry */ }
                Err(e) => return Err(TcpError::Io(e)),
            }
        };
        // A close()/cancel() can land in the gap between the loop's `alive`
        // check and `accept()` returning a connection that was already
        // sitting in the kernel's backlog — `accept()` never blocks on a
        // pending connection, so that gap is real, not just theoretical.
        // Re-check here so a racing close still gets what it promised
        // (a parked OR a just-unblocked accept never hands out a stream)
        // instead of silently accepting one more peer after `close()`
        // returned on another thread. `sock`'s drop closes the fd.
        if !self.alive.load(Ordering::Acquire) {
            return Err(TcpError::Closed);
        }
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
        TcpCancelHandle::from_flags(self.alive.clone(), self.cancelled.clone())
    }

    /// Close the listener from any thread: a parked `accept_blocking` returns
    /// [`TcpError::Closed`] within ~5 ms ([`ACCEPT_POLL_INTERVAL`]) and every
    /// later call returns it immediately. Idempotent. The OS socket stays
    /// bound until `self` is dropped (std cannot shut a listener down
    /// explicitly), so bind the next listener on a fresh port or drop this
    /// one first.
    pub fn close(&self) {
        // A listener has no peer-EOF path: its only terminal event IS the
        // caller asking it to stop, so close() latches the cancel flag too
        // (matching `TcpCancelHandle::cancel`, which is the same operation
        // reached from another thread).
        self.cancelled.store(true, Ordering::Release);
        self.alive.store(false, Ordering::Release);
    }
}

#[cfg(all(test, unix))]
mod accepted_stream_mode_tests {
    use super::TcpListener;
    use crate::transport::InnerStream;
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};

    /// `fcntl(F_GETFL)` returns -1 on error (with `errno` set) — and -1 has
    /// every bit set, `O_NONBLOCK` included, so a caller that only checks
    /// `flags & O_NONBLOCK` without first ruling out -1 can read an fcntl
    /// failure as "non-blocking" (a false pass) or, for an `== 0` assertion,
    /// as "not non-blocking" (a false fail) — either way the wrong signal.
    /// Fail loudly on the error case instead of trusting -1 as data.
    fn get_flags(fd: RawFd) -> libc::c_int {
        // SAFETY: F_GETFL takes no argument and only reads the descriptor's
        // status flags; the caller owns `fd` for the duration of this call.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(
            flags,
            -1,
            "fcntl(F_GETFL) failed: {}",
            io::Error::last_os_error()
        );
        flags
    }

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

        let listen_flags = get_flags(listener.inner.as_raw_fd());
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
        let flags = get_flags(stream.as_raw_fd());
        assert_eq!(
            flags & libc::O_NONBLOCK,
            0,
            "accepted stream must be blocking"
        );
    }
}
