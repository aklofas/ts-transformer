//! [`UdpTransport`] — UDP sender implementing `tst_core::transport::Transport`.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::net::udp_socket::{apply_multicast_send_knobs, bind_udp_socket, set_socket_buffers};
use tst_core::transport::{BrokenCause, SocketStats, Transport, TransportError};

use crate::config::SocketConfig;
use crate::error::UdpError;
use crate::stats::UdpStats;
use crate::url::UdpUrl;

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
    alive: Arc<AtomicBool>,
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
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(TransportError::Backpressure {
                    msg: format!("send WouldBlock: {e}"),
                    errno_code: e.raw_os_error(),
                })
            }
            Err(e) => {
                self.stats.send_errors = self.stats.send_errors.saturating_add(1);
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
        self.alive.load(Ordering::Acquire)
    }

    fn close(&mut self) {
        self.alive.store(false, Ordering::Release);
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        Some(self.stats.to_socket_stats())
    }
}
