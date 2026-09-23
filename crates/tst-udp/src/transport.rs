//! [`UdpTransport`] — UDP sender implementing `tst_core::transport::Transport`.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::net::udp_socket::{apply_multicast_send_knobs, bind_udp_socket, set_socket_buffers};
use tst_core::net::{SendClass, classify_send_error};
use tst_core::transport::{BrokenCause, SocketStats, Transport, TransportCancel, TransportError};

use crate::config::SocketConfig;
use crate::error::UdpError;
use crate::stats::UdpStats;
use crate::url::UdpUrl;

/// Cheap cloneable handle that unblocks a [`crate::recv::UdpRecvTransport`]
/// parked in `recv_bytes` on the 100 ms poll loop and makes the *next*
/// `send_bytes` / `recv_bytes` on the owning transport return
/// [`TransportError::ExplicitClose`], from any thread.
///
/// Obtained via [`UdpTransport::cancel_handle`] /
/// [`crate::recv::UdpRecvTransport::cancel_handle`] (inherent, non-`Option`)
/// or through `Transport::cancel_handle` / `RecvTransport::cancel_handle`
/// (the trait forms, `Some` on both UDP transports). Cancelling does **not**
/// close the socket — `close()` still does. Cancellation is cooperative,
/// with the same shape as `TcpCancelHandle` and `RtpCancelHandle`:
///
/// - a parked `recv_bytes` observes the flag at its next poll tick (≤ ~100 ms,
///   `tst_core::net::udp_socket::CANCEL_POLL_INTERVAL`) and returns
///   `ExplicitClose`;
/// - calls started after `cancel()` return `ExplicitClose` on their entry
///   check (a UDP `send_bytes` never parks — `send_to` on a datagram socket
///   returns at once — so the entry check is its only cancel point);
/// - a call already past its entry check completes its current I/O first (a
///   recv that receives a datagram in that window returns it); the *next*
///   call fails;
/// - after `cancel()` the transport's `is_alive()` reads `false`; `close()`
///   afterwards is a quiet no-op.
///
/// `close()` is a different signal: post-close calls return
/// [`TransportError::Closed`], and a handle obtained earlier does **not**
/// read `is_cancelled()` after a plain close.
///
/// `UdpCancelHandle` is `Clone + Send + Sync`; multiple holders can race
/// `cancel()` safely (the flag is an `Arc<AtomicBool>`, idempotent).
#[derive(Clone, Debug)]
pub struct UdpCancelHandle {
    /// The cancel latch proper, SEPARATE from the transport's `alive` flag.
    ///
    /// `alive` is a liveness flag — `close()` and a latched `Broken` both
    /// clear it — so `!alive` cannot answer "did the caller cancel?". The
    /// bindings relabel a caller-initiated end from exactly that answer, so
    /// reading `!alive` here would turn every broken UDP receive into a
    /// `CLOSED` outcome instead of a broken one.
    cancelled: Arc<AtomicBool>,
}

impl UdpCancelHandle {
    /// Build a handle over a shared flag — used by [`UdpTransport::cancel_handle`]
    /// and [`crate::recv::UdpRecvTransport::cancel_handle`].
    pub(crate) fn from_flag(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    /// Signal cancellation. Idempotent — repeated calls are a no-op.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// `true` once [`Self::cancel`] has been called on any clone of this handle.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl TransportCancel for UdpCancelHandle {
    fn cancel(&self) {
        UdpCancelHandle::cancel(self)
    }

    fn is_cancelled(&self) -> bool {
        UdpCancelHandle::is_cancelled(self)
    }
}

/// UDP sender.
///
/// Construct via [`UdpTransport::connect`] for the URL fast-path, or via
/// [`crate::builder::UdpTransportBuilder`] for full control over knobs.
///
/// Always sends to a single fixed peer via `send_to` on an
/// **unconnected** socket — fire-and-forget datagram semantics, matching
/// what TS-over-UDP receivers (ffmpeg, VLC, mediamtx) expect of a
/// sender. Deliberately NOT a connected socket: on Linux a connected UDP
/// socket surfaces ICMP port-unreachable as a fatal `ECONNREFUSED` on a
/// later `send`, which turns a receiver's restart/idle-rebind window
/// into a dead sender. To send to a different peer, build a new
/// transport.
pub struct UdpTransport {
    socket: UdpSocket,
    pkt_size: usize,
    peer: SocketAddr,
    stats: UdpStats,
    /// Cleared by `close()` and by a latched `Broken`; post-close/-broken
    /// sends return `Closed`.
    alive: Arc<AtomicBool>,
    /// Set by [`UdpCancelHandle::cancel`]; checked at `send_bytes` entry
    /// BEFORE `alive`, so a cancelled transport reports `ExplicitClose`
    /// rather than `Closed` (spec §3.5, one cancel outcome).
    cancelled: Arc<AtomicBool>,
}

impl UdpTransport {
    /// Build a `UdpTransport` from a `udp://...` URL.
    ///
    /// For multicast destinations, applies TTL + iface knobs from the URL.
    pub fn connect(url: &str) -> Result<Self, UdpError> {
        let url = UdpUrl::parse(url)?;
        let mut cfg = SocketConfig::default();
        cfg.merge_from_url(&url);
        Self::with_config(&url, &cfg)
    }

    /// Build a `UdpTransport` from an already-parsed `UdpUrl` + config.
    pub fn with_config(url: &UdpUrl, cfg: &SocketConfig) -> Result<Self, UdpError> {
        if url.recv_bind {
            return Err(UdpError::InvalidConfig(
                "URL has '@' prefix indicating recv-bind; use UdpRecvTransport".into(),
            ));
        }

        let local: SocketAddr = match (cfg.localaddr, url.addr) {
            (Some(a), peer) => {
                // URL-sourced localaddr already passed the parse-time family
                // check; this guards direct SocketConfig users the same way —
                // a socket bound to one family cannot send to the other, and
                // failing here beats an opaque OS error at the first send.
                if a.is_ipv4() != peer.is_ipv4() {
                    return Err(UdpError::InvalidConfig(format!(
                        "localaddr {a} and peer {peer} are different IP families"
                    )));
                }
                SocketAddr::new(a, 0)
            }
            (None, IpAddr::V4(_)) => {
                SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
            }
            (None, IpAddr::V6(_)) => {
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0)
            }
        };
        let socket = bind_udp_socket(local).map_err(UdpError::Io)?;

        if url.is_multicast() {
            apply_multicast_send_knobs(&socket, url.addr, cfg.ttl, cfg.iface.as_deref())
                .map_err(UdpError::Io)?;
        }

        apply_socket2_knobs(&socket, cfg).map_err(UdpError::Io)?;

        let peer = SocketAddr::new(url.addr, url.port);

        Ok(Self {
            socket,
            pkt_size: cfg.pkt_size_or_default(),
            peer,
            stats: UdpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Snapshot of UDP stats.
    pub fn stats(&self) -> UdpStats {
        self.stats
    }

    /// Destination address sends go to. The socket is deliberately
    /// UNCONNECTED (`send_to` per datagram) — see the send-path notes.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Cross-thread cancel handle for this sender. Cloneable; see
    /// [`UdpCancelHandle`] for the contract. The trait form
    /// [`Transport::cancel_handle`] hands back the same flag boxed as
    /// `Arc<dyn TransportCancel>`.
    pub fn cancel_handle(&self) -> UdpCancelHandle {
        UdpCancelHandle::from_flag(self.cancelled.clone())
    }
}

fn apply_socket2_knobs(socket: &UdpSocket, cfg: &SocketConfig) -> std::io::Result<()> {
    set_socket_buffers(socket, cfg.rcvbuf, cfg.sndbuf)?;
    if let Some(tos) = cfg.tos {
        // socket2 0.5 set_tos for IPv4. IPv6 traffic-class would need IPV6_TCLASS
        // via libc directly; deferred (low-priority knob).
        let _ = socket2::SockRef::from(socket).set_tos(tos as u32);
    }
    Ok(())
}

impl Transport for UdpTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        // Cancel wins over close: a caller who fired the handle sees the
        // cancel outcome even if a close() raced in afterwards.
        if self.cancelled.load(Ordering::Acquire) {
            return Err(TransportError::ExplicitClose);
        }
        if !self.alive.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        if msg.len() > self.pkt_size {
            self.stats.send_errors = self.stats.send_errors.saturating_add(1);
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.pkt_size,
            });
        }
        match self.socket.send_to(msg, self.peer) {
            Ok(_n) => {
                self.stats.datagrams_sent = self.stats.datagrams_sent.saturating_add(1);
                self.stats.bytes_sent = self.stats.bytes_sent.saturating_add(msg.len() as u64);
                Ok(())
            }
            // The send deadline ticked over (`WouldBlock` on Linux/macOS,
            // `TimedOut` on Windows — CORR-24) or a signal interrupted the
            // call: the datagram was NOT consumed, so this is retryable
            // Backpressure with the transport still alive.
            Err(e) if classify_send_error(&e) == SendClass::Transient => {
                Err(TransportError::Backpressure {
                    msg: format!("send deadline expired: {e}"),
                    errno_code: e.raw_os_error(),
                })
            }
            Err(e) => {
                // Fatal (EMSGSIZE, ENETUNREACH, EPERM, …): per the
                // `Transport` contract the state is undefined after any
                // non-Backpressure error — latch dead so `is_alive()` tells
                // the truth and later sends are `Closed`, the same as the
                // TCP/RIST fatal arms (spec §3.5, UDP `is_alive` after
                // Broken = false).
                self.stats.send_errors = self.stats.send_errors.saturating_add(1);
                self.alive.store(false, Ordering::Release);
                Err(TransportError::Broken {
                    msg: format!("send error: {e}"),
                    errno_code: e.raw_os_error(),
                    cause: BrokenCause::Unspecified,
                })
            }
        }
    }

    fn max_payload(&self) -> usize {
        self.pkt_size
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire) && !self.cancelled.load(Ordering::Acquire)
    }

    fn close(&mut self) {
        self.alive.store(false, Ordering::Release);
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(self.cancel_handle()))
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        Some(self.stats.to_socket_stats())
    }
}
