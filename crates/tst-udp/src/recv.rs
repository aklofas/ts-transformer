//! [`UdpRecvTransport`] — UDP receiver implementing `tst_core::transport::RecvTransport`.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tst_core::net::udp_socket::{
    CANCEL_POLL_INTERVAL, apply_multicast_recv_join, bind_udp_socket, bind_udp_socket_multicast,
    set_socket_buffers,
};
use tst_core::transport::{BrokenCause, RecvTransport, SocketStats, TransportError};

use crate::config::SocketConfig;
use crate::error::UdpError;
use crate::stats::UdpStats;
use crate::url::UdpUrl;

/// UDP receiver.
///
/// Construct via [`UdpRecvTransport::listen`] for the URL fast-path, or via
/// [`crate::builder::UdpRecvTransportBuilder`].
///
/// # Cross-thread cancellation
///
/// `UdpRecvTransport` does not expose a cancel handle. Both
/// [`RecvTransport::recv_bytes`] and [`RecvTransport::close`] take `&mut self`,
/// so they cannot be called concurrently — there is no race-free way to
/// interrupt a live receive from another thread. Use cooperative shutdown
/// instead: call [`UdpRecvTransport::recv_timeout`] with a finite deadline and
/// check a stop flag in the caller loop; the owning thread calls `close()` once
/// it decides to stop. If you need to fire shutdown from a thread that does not
/// own the transport, consider `tst-srt` / `tst-rtp` / `tst-tcp`, which expose
/// cloneable cancel handles.
pub struct UdpRecvTransport {
    socket: UdpSocket,
    local: SocketAddr,
    stats: UdpStats,
    alive: Arc<AtomicBool>,
    /// Cached `SO_RCVTIMEO` value. Initialized to `CANCEL_POLL_INTERVAL`
    /// (matching what the bind helper sets). Updated lazily: `recv_timeout`
    /// skips `set_read_timeout` when the socket is already at the requested
    /// value; `recv_bytes` restores to `CANCEL_POLL_INTERVAL` at entry if a
    /// prior `recv_timeout` left a different value in place.
    applied_timeout: std::time::Duration,
}

impl UdpRecvTransport {
    /// Build a `UdpRecvTransport` from a `udp://...` URL.
    ///
    /// URL semantics:
    /// - `udp://@bind_addr:port` — bind (the `@` is the ffmpeg recv convention)
    /// - `udp://bind_addr:port` — also accepted; behavior is identical
    /// - For multicast groups, the socket joins the group on bind.
    pub fn listen(url: &str) -> Result<Self, UdpError> {
        let url = UdpUrl::parse(url)?;
        if url.pkt_size.is_some() {
            return Err(UdpError::Url(crate::url::UdpUrlError::RecvPktSize));
        }
        let mut cfg = SocketConfig::default();
        cfg.merge_from_url(&url);
        Self::with_config(&url, &cfg)
    }

    /// Build from already-parsed `UdpUrl` + config.
    pub fn with_config(url: &UdpUrl, cfg: &SocketConfig) -> Result<Self, UdpError> {
        let bind_addr: SocketAddr = if url.is_multicast() {
            match url.addr {
                IpAddr::V4(_) => {
                    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), url.port)
                }
                IpAddr::V6(_) => {
                    SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), url.port)
                }
            }
        } else {
            SocketAddr::new(url.addr, url.port)
        };

        // Multicast receivers must use SO_REUSEADDR (+ SO_REUSEPORT on BSD/macOS)
        // so that more than one receiver process can bind the same group:port on
        // the same host. Unicast receivers use the plain bind to avoid sharing
        // ports unintentionally.
        let socket = if url.is_multicast() {
            bind_udp_socket_multicast(bind_addr).map_err(UdpError::Io)?
        } else {
            bind_udp_socket(bind_addr).map_err(UdpError::Io)?
        };

        if url.is_multicast() {
            apply_multicast_recv_join(&socket, url.addr, cfg.iface.as_deref())
                .map_err(UdpError::Io)?;
        }

        set_socket_buffers(&socket, cfg.rcvbuf, None).map_err(UdpError::Io)?;

        let local = socket.local_addr().map_err(UdpError::Io)?;

        Ok(Self {
            socket,
            local,
            stats: UdpStats::default(),
            alive: Arc::new(AtomicBool::new(true)),
            // The bind helper sets SO_RCVTIMEO = CANCEL_POLL_INTERVAL on
            // the socket; cache that so the first recv_bytes entry is a no-op.
            applied_timeout: CANCEL_POLL_INTERVAL,
        })
    }

    /// Local bound address (useful for tests that bind to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Snapshot of UDP stats.
    pub fn stats(&self) -> UdpStats {
        self.stats
    }

    /// Receive one datagram with a deadline. Returns `None` on timeout
    /// (no data arrived within `deadline`). Returns `Some(n)` on success.
    ///
    /// Sets `SO_RCVTIMEO` lazily: if the socket already carries `deadline`
    /// from a previous call, the setsockopt is skipped. The timeout is **not**
    /// restored after this call; `recv_bytes` restores it to the cancel-poll
    /// interval on entry when the cached value differs. Not concurrency-safe —
    /// callers must ensure no concurrent `recv_bytes` is in progress on
    /// the same transport handle (the `Mutex<Option<…>>` in the Python
    /// binding guarantees this).
    pub fn recv_timeout(
        &mut self,
        buf: &mut [u8],
        deadline: std::time::Duration,
    ) -> Result<Option<usize>, crate::error::UdpError> {
        // Only pay the setsockopt cost when the socket needs a different value.
        if self.applied_timeout != deadline {
            self.socket
                .set_read_timeout(Some(deadline))
                .map_err(crate::error::UdpError::Io)?;
            self.applied_timeout = deadline;
        }
        match self.socket.recv(buf) {
            Ok(n) => {
                self.stats.datagrams_received = self.stats.datagrams_received.saturating_add(1);
                self.stats.bytes_received = self.stats.bytes_received.saturating_add(n as u64);
                Ok(Some(n))
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(None)
            }
            Err(e) => {
                self.stats.recv_errors = self.stats.recv_errors.saturating_add(1);
                Err(crate::error::UdpError::Io(e))
            }
        }
    }
}

impl RecvTransport for UdpRecvTransport {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // Lazy restore: if recv_timeout left the socket at a non-cancel-poll
        // timeout, restore it now so the cancel-poll guarantee holds for this
        // entire recv. One setsockopt per recv_bytes entry rather than one per
        // recv_timeout call.
        if self.applied_timeout != CANCEL_POLL_INTERVAL {
            self.socket
                .set_read_timeout(Some(CANCEL_POLL_INTERVAL))
                .map_err(|e| TransportError::Broken {
                    msg: format!("failed to restore cancel-poll timeout: {e}"),
                    errno_code: e.raw_os_error(),
                    cause: BrokenCause::Unspecified,
                })?;
            self.applied_timeout = CANCEL_POLL_INTERVAL;
        }
        loop {
            if !self.alive.load(Ordering::Acquire) {
                return Err(TransportError::Closed);
            }
            match self.socket.recv(buf) {
                Ok(n) => {
                    self.stats.datagrams_received = self.stats.datagrams_received.saturating_add(1);
                    self.stats.bytes_received = self.stats.bytes_received.saturating_add(n as u64);
                    return Ok(n);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Cancel-poll tick — loop to re-check `alive`.
                    continue;
                }
                Err(e) => {
                    self.stats.recv_errors = self.stats.recv_errors.saturating_add(1);
                    return Err(TransportError::Broken {
                        msg: format!("recv error: {e}"),
                        errno_code: e.raw_os_error(),
                        cause: BrokenCause::Unspecified,
                    });
                }
            }
        }
    }

    /// Upper bound on the buffer needed to receive any UDP datagram.
    ///
    /// Always returns 65535 (the maximum UDP datagram size, headers
    /// included — actual payloads are always a few bytes smaller) so that
    /// pipeline shells and direct callers allocate a receive buffer large
    /// enough for any legal datagram without silent tail truncation.
    /// `pkt_size` is a send-side knob (see [`crate::transport::UdpTransport`]);
    /// it has no receive-side surface — the receiver always accepts any
    /// datagram up to this bound.
    fn max_payload(&self) -> usize {
        65535
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::UdpTransport;
    use std::time::{Duration, Instant};

    /// Regression test: `recv_timeout` must not permanently disable the
    /// cancel-poll `SO_RCVTIMEO` on the socket.
    ///
    /// `recv_bytes` relies on the socket waking every `CANCEL_POLL_INTERVAL`
    /// (~100 ms) to re-check the `alive` flag — that is how `close()` interrupts
    /// a blocked recv. `recv_timeout` leaves `SO_RCVTIMEO` at the deadline value;
    /// the cancel-poll interval is restored lazily at the start of `recv_bytes`
    /// when the cached timeout differs from `CANCEL_POLL_INTERVAL`.
    #[test]
    fn close_unblocks_recv_bytes_after_recv_timeout() {
        let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind recv");

        // Step 1 — call recv_timeout with a short deadline; no sender, so it
        // times out and returns Ok(None).  This is the call that, before the fix,
        // would leave SO_RCVTIMEO=None on the socket.
        let mut buf = vec![0u8; recv.max_payload()];
        let result = recv.recv_timeout(&mut buf, Duration::from_millis(50));
        assert!(
            matches!(result, Ok(None)),
            "expected timeout (Ok(None)), got {result:?}"
        );

        // Step 2 — clone the alive flag directly (we are in the same crate so
        // private fields are accessible inside this #[cfg(test)] module) and
        // spawn a closer thread that waits briefly then signals close().
        let alive = Arc::clone(&recv.alive);
        let _closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            alive.store(false, Ordering::Release);
        });

        // Step 3 — recv_bytes must return Closed within 2 s.  If the
        // SO_RCVTIMEO was cleared to None, recv_bytes blocks forever and the
        // assert on elapsed fires (or the test suite hangs).
        let start = Instant::now();
        let err = recv.recv_bytes(&mut buf);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "recv_bytes blocked for {elapsed:?}; \
             cancel-poll timeout was likely disabled by recv_timeout"
        );
        assert!(
            matches!(err, Err(TransportError::Closed)),
            "expected Err(TransportError::Closed), got {err:?}"
        );
    }

    /// DA-PERF-7: the `applied_timeout` cache must track setsockopt state correctly.
    ///
    /// After construction the cache is `CANCEL_POLL_INTERVAL`. After a `recv_timeout`
    /// call the cache holds `deadline`. After `recv_bytes` returns, the cache is back
    /// to `CANCEL_POLL_INTERVAL` because the lazy restore fires on entry.
    #[test]
    fn applied_timeout_cache_tracks_state() {
        let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind recv");

        // After construction: cache matches the cancel-poll interval.
        assert_eq!(
            recv.applied_timeout, CANCEL_POLL_INTERVAL,
            "initial applied_timeout must equal CANCEL_POLL_INTERVAL"
        );

        // After recv_timeout times out: cache updated to the deadline.
        let deadline = Duration::from_millis(20);
        let mut buf = vec![0u8; recv.max_payload()];
        let _ = recv.recv_timeout(&mut buf, deadline);
        assert_eq!(
            recv.applied_timeout, deadline,
            "applied_timeout must be updated to the deadline after recv_timeout"
        );

        // After recv_bytes (which restores lazily): cache back to CANCEL_POLL_INTERVAL.
        let alive = Arc::clone(&recv.alive);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            alive.store(false, Ordering::Release);
        });
        let _ = recv.recv_bytes(&mut buf); // returns Closed
        assert_eq!(
            recv.applied_timeout, CANCEL_POLL_INTERVAL,
            "recv_bytes must restore applied_timeout to CANCEL_POLL_INTERVAL"
        );
    }

    /// DA-NET-8: datagrams larger than the default `pkt_size` (1316 bytes)
    /// must be delivered in full. Before the fix, `max_payload()` returned
    /// `pkt_size` so pipeline shells allocated a 1316-byte buffer; the OS
    /// would silently truncate larger datagrams to 1316 bytes on `recv`.
    ///
    /// Send 2000 bytes, assert 2000 bytes arrive.
    #[test]
    fn oversize_datagram_delivered_in_full() {
        let recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind recv");
        let port = recv.local_addr().port();
        let addr = format!("127.0.0.1:{port}");

        let payload: Vec<u8> = (0u8..=255).cycle().take(2000).collect();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        sender.send_to(&payload, &addr).expect("send");

        // recv_bytes uses the provided buffer; caller must supply ≥ max_payload bytes.
        let mut buf = vec![0u8; recv.max_payload()];
        // Use recv_timeout so the test doesn't block indefinitely on CI.
        let mut recv = recv;
        let got = recv
            .recv_timeout(&mut buf, Duration::from_secs(2))
            .expect("recv_timeout")
            .expect("expected datagram, got timeout");

        assert_eq!(
            got, 2000,
            "expected 2000 bytes, got {got} (truncation may have occurred)"
        );
        assert_eq!(&buf[..got], &payload[..], "payload content mismatch");
    }

    /// `listen` must reject `?pkt_size=` the same way the builder path does.
    ///
    /// Before the fix, the public direct-listen entry parsed the URL itself and
    /// called `with_config` directly, bypassing the builder's rejection check.
    #[test]
    fn listen_rejects_pkt_size_query() {
        let result = UdpRecvTransport::listen("udp://@127.0.0.1:0?pkt_size=1316");
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("direct listen must reject ?pkt_size= like the builder path"),
        };
        assert!(
            matches!(err, UdpError::Url(crate::url::UdpUrlError::RecvPktSize)),
            "expected UdpError::Url(RecvPktSize), got: {err:?}"
        );
        assert!(
            err.to_string().contains("send-side knob"),
            "error message must teach the caller: got: {err}"
        );
    }

    /// DA-NET-7: two receivers joining the same multicast group on loopback
    /// must both bind AND both receive the same datagram.
    ///
    /// Mirrors the pattern in `crates/tst-rtp/tests/rtp/loopback_multicast.rs`:
    /// we specify `?iface=127.0.0.1` so the kernel routes multicast on the
    /// loopback interface. Some CI environments (macOS GHA) lack multicast
    /// routing on `lo` — the test degrades gracefully on `join_multicast_v4`
    /// failure so it doesn't block CI; run locally on Linux to get full cover.
    #[test]
    fn two_multicast_receivers_deliver_same_datagram() {
        // Bind and join the first receiver on loopback.
        let mut recv1 = match UdpRecvTransport::listen("udp://@239.255.42.1:0?iface=127.0.0.1") {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "skip: multicast recv1 bind/join failed ({e}); \
                     likely no loopback multicast routing — run on Linux"
                );
                return;
            }
        };
        let port = recv1.local_addr().port();

        // Second receiver joins the same group:port — requires SO_REUSEADDR.
        // recv1 succeeding proves this platform supports multicast bind+join,
        // so an AddrInUse here IS the DA-NET-7 regression: hard-fail. Other
        // errors (e.g. a second-join quirk) still degrade gracefully.
        let mut recv2 = match UdpRecvTransport::listen(&format!(
            "udp://@239.255.42.1:{port}?iface=127.0.0.1"
        )) {
            Ok(r) => r,
            Err(UdpError::Io(e)) if e.kind() == std::io::ErrorKind::AddrInUse => {
                panic!(
                    "second multicast receiver hit EADDRINUSE on a platform where \
                     the first bind+join succeeded — SO_REUSEADDR regression: {e}"
                );
            }
            Err(e) => {
                eprintln!("skip: multicast recv2 bind/join failed ({e})");
                return;
            }
        };

        let payload: Vec<u8> = (0u8..=187).collect(); // 188 bytes, recognisable
        let (tx1, rx1) = std::sync::mpsc::channel::<Vec<u8>>();
        let (tx2, rx2) = std::sync::mpsc::channel::<Vec<u8>>();

        // Each receiver blocks in its own thread with a 3-second window.
        let _t1 = std::thread::spawn(move || {
            let mut buf = vec![0u8; recv1.max_payload()];
            if let Ok(Some(n)) = recv1.recv_timeout(&mut buf, Duration::from_secs(3)) {
                let _ = tx1.send(buf[..n].to_vec());
            }
        });
        let _t2 = std::thread::spawn(move || {
            let mut buf = vec![0u8; recv2.max_payload()];
            if let Ok(Some(n)) = recv2.recv_timeout(&mut buf, Duration::from_secs(3)) {
                let _ = tx2.send(buf[..n].to_vec());
            }
        });

        // Brief pause so both receiver threads reach recv_timeout before the
        // datagram is sent (avoids a race between join and the first packet).
        std::thread::sleep(Duration::from_millis(50));

        let mut sender = match UdpTransport::connect(&format!(
            "udp://239.255.42.1:{port}?ttl=1&iface=127.0.0.1"
        )) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: multicast sender setup failed ({e})");
                return;
            }
        };
        use tst_core::transport::Transport as _;
        sender.send_bytes(&payload).expect("multicast send_bytes");

        // Collect from both receivers. A timeout here means the kernel dropped
        // the multicast — treat as a graceful platform skip.
        let got1 = match rx1.recv_timeout(Duration::from_secs(5)) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skip: recv1 timed out — kernel filtered loopback multicast");
                return;
            }
        };
        let got2 = match rx2.recv_timeout(Duration::from_secs(5)) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skip: recv2 timed out — kernel filtered loopback multicast");
                return;
            }
        };

        // Both must deliver the exact 188-byte payload.
        assert_eq!(
            got1.len(),
            payload.len(),
            "recv1: expected {} bytes, got {}",
            payload.len(),
            got1.len()
        );
        assert_eq!(
            got2.len(),
            payload.len(),
            "recv2: expected {} bytes, got {}",
            payload.len(),
            got2.len()
        );
        assert_eq!(&got1[..], &payload[..], "recv1 payload mismatch");
        assert_eq!(&got2[..], &payload[..], "recv2 payload mismatch");
    }
}
