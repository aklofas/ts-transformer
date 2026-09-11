//! `RtpTransport` (send) + `RtpRecvTransport` (recv) — sync UDP socket
//! wrappers behind the [`tst_core::transport`] traits.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Phase 1 ships only the UDP data plane; RTSP control plane (Phase 2)
//! is what makes negotiated transports work. For now, sender + receiver
//! agree on a fixed `host:port` and use it directly.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tst_core::net::udp_socket::{
    CANCEL_POLL_INTERVAL, apply_multicast_recv_join, apply_multicast_send_knobs,
    bind_udp_socket_multicast,
};
use tst_core::transport::{
    BrokenCause, RecvTransport, SocketStats, Transport, TransportCancel, TransportError,
};

use crate::cancel::RtpCancelHandle;
use crate::clock::RtpClock;
use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T, RtpHeader};
use crate::rtcp::ingest::{ingest_rr, ingest_sr};
use crate::rtcp::reporter::RtcpReporterHandle;
use crate::rtcp::stats::RtcpStats;
use crate::rtcp::{ReceiverReport, SdesPacket, SenderReport};
use crate::rtsp::client::end_reason::{EndReasonSlot, StreamEndReason, StreamEndReasonHandle};
use crate::url::{DEFAULT_PKT_SIZE, RtpUrl, UrlError as RtpUrlError};

/// Convert an `io::Error` from the shared UDP socket helpers into a
/// `ConnectError`. `io::ErrorKind::Unsupported` (produced by
/// `apply_multicast_iface` / `set_multicast_hops_v6` / `set_multicast_if_v4`)
/// maps to `ConnectError::IfaceUnsupported`; everything else maps to
/// `ConnectError::Io`.
fn udp_err_to_connect(e: io::Error) -> ConnectError {
    if e.kind() == io::ErrorKind::Unsupported {
        ConnectError::IfaceUnsupported {
            iface: String::new(),
            detail: e.to_string(),
        }
    } else {
        ConnectError::Io(e)
    }
}

/// RTP send-side transport: writes 12-byte RTP header + TS payload to a
/// connected [`UdpSocket`].
///
/// # `SocketStats` field mapping
///
/// | [`SocketStats`] field | Phase 1 source |
/// |---|---|
/// | `bytes_sent` | Local counter, ticks per successful `send_bytes` |
/// | `packets_sent` | Local counter, ticks per RTP packet |
/// | `bytes_received` / `packets_received` | 0 (this is the send half) |
/// | `rtt_us` | 0 — RTCP RR/SR ingestion is deferred past Phase 1 |
/// | `packets_lost_send` | 0 — same; would come from RTCP RR fraction-lost |
/// | `link_bandwidth_bps` | 0 — RTP has no link estimate |
/// | All other fields | 0 |
///
/// # `TransportError::Broken` `errno_code` mapping
///
/// For send-side `Broken`, `errno_code` carries the OS `errno` from the
/// underlying `sendto` call (`EAGAIN`=11, `EHOSTUNREACH`=113,
/// `ECONNREFUSED`=111 on Linux). `Backpressure` is not produced in
/// Phase 1 — UDP either accepts the datagram or surfaces an error.
pub struct RtpTransport {
    socket: Option<UdpSocket>,
    /// Max UDP datagram budget (RTP header + TS bundle), set from
    /// `RtpUrl::pkt_size` (default 1316). `send_bytes` rejects payloads
    /// where `len + RTP_HEADER_LEN` would exceed this, so the
    /// caller-visible [`Transport::max_payload`] is `pkt_size - 12`.
    max_payload: usize,
    clock: RtpClock,
    ssrc: u32,
    next_seq: u16,
    cancel: Arc<RtpCancelHandle>,
    /// Local stats — bytes_sent / packets_sent only in Phase 1; the
    /// RTCP-derived fields stay zero per the master spec's SocketStats
    /// table.
    bytes_sent: u64,
    packets_sent: u64,
    /// Companion RTCP socket bound on `port + 1` per RFC 3550 §11.
    /// `None` when the caller opted out via `RtpSocketBuilder::rtcp(false)`.
    /// Retained on the transport so it stays alive for the reporter
    /// thread's lifetime — the reporter holds its own `try_clone`'d FD.
    #[allow(dead_code)]
    rtcp_socket: Option<UdpSocket>,
    /// RTCP-derived counters, shared with the reporter thread (which
    /// ticks `sr_packets_sent` on each SR emission) and any Task-8
    /// ingest path.
    rtcp_stats: Arc<Mutex<RtcpStats>>,
    /// Background SR-emitter handle. Dropping this cancels + joins
    /// the reporter thread. Held only for its `Drop` side effect.
    #[allow(dead_code)]
    rtcp_reporter: Option<RtcpReporterHandle>,
    /// Reusable scratch buffer for building RTP datagrams. Avoids a heap
    /// allocation per `send_bytes` call — cleared and refilled each time,
    /// but the underlying allocation is retained across frames.
    send_scratch: Vec<u8>,
}

impl RtpTransport {
    /// Connect (just sets `SocketAddr::connect`-style default) and
    /// return a ready-to-send transport.
    ///
    /// `url` must have scheme `rtp://` and an explicit port.
    ///
    /// The outgoing RTCP SR reporter is **off by default** (see
    /// [`Self::connect_with_rtcp`] for the experimental opt-in and its
    /// limitations). RTCP *reception* is unaffected.
    pub fn connect(url: &str) -> Result<Self, ConnectError> {
        let parsed = RtpUrl::parse(url).map_err(ConnectError::Url)?;
        Self::connect_with_rtcp(&parsed, false)
    }

    /// Connect using an already-parsed URL — convenient for callers that
    /// hold an `RtpUrl` (e.g., binding crates). The outgoing RTCP SR
    /// reporter is **off by default** (see [`Self::connect_with_rtcp`]).
    pub fn connect_with(url: &RtpUrl) -> Result<Self, ConnectError> {
        Self::connect_with_rtcp(url, false)
    }

    /// Connect using an already-parsed URL with an explicit RTCP toggle.
    ///
    /// `rtcp_enabled = true` binds the RTCP companion socket on `port + 1`
    /// and spawns the periodic SR-emitter thread. `rtcp_enabled = false`
    /// (the default for [`Self::connect`] / [`Self::connect_with`]) skips
    /// both.
    ///
    /// # Experimental: the SR reporter emits placeholder statistics
    ///
    /// The periodic SR reporter is **experimental and off by default**. It
    /// currently emits **placeholder (zero) statistics** — there are no
    /// live sender counters wired into the SR (`sender_packet_count`,
    /// `sender_octet_count`, `rtp_timestamp`, and `ntp_timestamp` are all
    /// zero). As such it is **NOT RFC 3550-conformant** and must not be
    /// relied on by peers for sender-side reception statistics. Enabling it
    /// is only useful for exercising the RTCP socket-pair plumbing. RTCP
    /// *reception* (ingesting peer SR/RR into [`Self::rtcp_stats`] and the
    /// projected [`SocketStats`] fields) is a separate, working path and is
    /// not affected by this toggle.
    pub fn connect_with_rtcp(url: &RtpUrl, rtcp_enabled: bool) -> Result<Self, ConnectError> {
        if url.pt.is_some() {
            return Err(ConnectError::PayloadTypeParam);
        }
        let ip: IpAddr = url.host.parse().map_err(|e: std::net::AddrParseError| {
            ConnectError::HostNotLiteral {
                host: url.host.clone(),
                detail: e.to_string(),
            }
        })?;
        let peer = SocketAddr::new(ip, url.port);
        let local: SocketAddr = match ip {
            IpAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
            IpAddr::V6(_) => "[::]:0".parse().unwrap(),
        };
        let socket = UdpSocket::bind(local).map_err(ConnectError::Io)?;
        socket
            .set_write_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(ConnectError::Io)?;
        // Multicast knobs (no-op for unicast).
        let is_multicast = match ip {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        };
        if is_multicast {
            apply_multicast_send_knobs(&socket, ip, url.ttl, url.iface.as_deref())
                .map_err(udp_err_to_connect)?;
        }
        socket.connect(peer).map_err(ConnectError::Io)?;
        // RTCP companion socket bound on `port + 1` per RFC 3550 §11.
        // Sender binds an ephemeral local port + sends SR to peer's
        // port+1 (the symmetric pair).
        let rtcp_socket = if rtcp_enabled {
            let rtcp_local: SocketAddr = match ip {
                IpAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
                IpAddr::V6(_) => "[::]:0".parse().unwrap(),
            };
            Some(UdpSocket::bind(rtcp_local).map_err(ConnectError::Io)?)
        } else {
            None
        };
        Ok(Self::from_socket(socket, url, rtcp_socket, peer))
    }

    /// Internal: build from an already-configured socket.
    fn from_socket(
        socket: UdpSocket,
        url: &RtpUrl,
        rtcp_socket: Option<UdpSocket>,
        peer: SocketAddr,
    ) -> Self {
        let pkt_size = url.pkt_size.unwrap_or(DEFAULT_PKT_SIZE);
        let ssrc = url.ssrc.unwrap_or_else(random_u32);
        let next_seq = random_u32() as u16;
        let start_ticks = random_u32();
        let rtcp_stats = Arc::new(Mutex::new(RtcpStats::default()));
        // Spawn the SR-emitter thread when RTCP is enabled. The closure
        // grabs its own clone of the rtcp socket FD + the stats handle;
        // both live for the reporter thread's lifetime via Arc/`try_clone`.
        let rtcp_reporter = rtcp_socket.as_ref().and_then(|sock| {
            // try_clone gives the reporter its own FD ref; close
            // semantics on the original FD stay intact.
            let sock_clone = match sock.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "rtcp socket try_clone failed; skipping reporter");
                    return None;
                }
            };
            let stats_clone = rtcp_stats.clone();
            // Guard: port 65535 has no valid RTCP companion port
            // (65536 overflows u16). Skip the reporter in that case,
            // mirroring the guard in `bind_server_udp_pair` in rtsp/server/handlers.rs.
            let Some(rtcp_companion_port) = peer.port().checked_add(1) else {
                tracing::warn!("peer port 65535 has no RTCP companion; skipping SR reporter");
                return None;
            };
            let rtcp_target = SocketAddr::new(peer.ip(), rtcp_companion_port);
            Some(RtcpReporterHandle::spawn(move || {
                // Phase 2 v1: SR carries running totals snapshot.
                // Real bytes_sent / packets_sent live on the
                // transport — for the v1 reporter we emit a
                // minimal SR (counters zero) + SDES CNAME. Full
                // SR counter wire-up happens in Phase 2 Task 14
                // (integration). The reporter thread + socket
                // pair are what Task 10 retrofits — the SR's
                // counters are a follow-up.
                let sr = crate::rtcp::SenderReport {
                    ssrc,
                    ntp_timestamp: 0,
                    rtp_timestamp: 0,
                    sender_packet_count: 0,
                    sender_octet_count: 0,
                    report_blocks: Vec::new(),
                };
                let cname = format!("tst-rtp-{ssrc:08x}");
                let sdes = SdesPacket { ssrc, cname };
                // These are locally-built, well-formed packets (no report
                // blocks, short CNAME) so encode never fails — but the
                // encoders are fallible now; skip the send on the
                // (unreachable) validation error rather than unwrapping.
                let (Ok(mut compound), Ok(sdes_bytes)) = (sr.encode(), sdes.encode()) else {
                    tracing::error!(
                        "internal: locally-built RTCP SR/SDES failed to encode; skipping send"
                    );
                    debug_assert!(false, "locally-built RTCP SR/SDES must always encode");
                    return;
                };
                compound.extend_from_slice(&sdes_bytes);
                let _ = sock_clone.send_to(&compound, rtcp_target);
                if let Ok(mut g) = stats_clone.lock() {
                    g.sr_packets_sent = g.sr_packets_sent.saturating_add(1);
                }
            }))
        });
        Self {
            socket: Some(socket),
            max_payload: pkt_size,
            clock: RtpClock::new(start_ticks),
            ssrc,
            next_seq,
            cancel: RtpCancelHandle::new(),
            bytes_sent: 0,
            packets_sent: 0,
            rtcp_socket,
            rtcp_stats,
            rtcp_reporter,
            send_scratch: Vec::with_capacity(RTP_HEADER_LEN + pkt_size),
        }
    }

    /// Snapshot of the RTCP-derived counters. Returns a clone of the
    /// internal `RtcpStats` (cheap — counters are plain integers).
    pub fn rtcp_stats(&self) -> RtcpStats {
        self.rtcp_stats
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

/// Failure shape for [`RtpTransport::connect`] / [`RtpTransport::connect_with`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    #[error("URL has ?pt= (elementary RTP): use H264Receiver, not the MP2T transport")]
    PayloadTypeParam,
    #[error("H264Receiver requires ?pt=<dynamic PT> on the URL")]
    MissingPayloadTypeParam,
    #[error("URL parse failed: {0}")]
    Url(#[from] RtpUrlError),
    /// `RtpUrl::host` couldn't be parsed as a literal IP. Phase 1
    /// doesn't do DNS resolution — callers can pre-resolve and pass the
    /// literal.
    #[error("host '{host}' is not a literal IPv4/IPv6 address: {detail}")]
    HostNotLiteral { host: String, detail: String },
    /// OS-level socket failure (bind, connect, setsockopt).
    #[error("UDP socket error: {0}")]
    Io(#[from] io::Error),
    /// `?iface=` couldn't be applied — typically because the platform
    /// requires a different form (e.g., IPv6 needs scope-id integer).
    #[error("multicast iface '{iface}' unsupported: {detail}")]
    IfaceUnsupported { iface: String, detail: String },
}

impl Transport for RtpTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if msg.len() + RTP_HEADER_LEN > self.max_payload {
            return Err(TransportError::TooLarge {
                len: msg.len() + RTP_HEADER_LEN,
                max: self.max_payload,
            });
        }
        let socket = self.socket.as_ref().ok_or(TransportError::Closed)?;
        // Build datagram: RTP header (12 B) + TS payload. Reuse the
        // per-transport scratch buffer to avoid a heap allocation per frame.
        self.send_scratch.clear();
        self.send_scratch.resize(RTP_HEADER_LEN, 0);
        RtpHeader::new(self.next_seq, self.clock.now_ticks(), self.ssrc)
            .encode_into(&mut self.send_scratch);
        self.send_scratch.extend_from_slice(msg);
        loop {
            if self.cancel.is_cancelled() {
                return Err(TransportError::ExplicitClose);
            }
            match socket.send(&self.send_scratch) {
                Ok(n) => {
                    self.next_seq = self.next_seq.wrapping_add(1);
                    self.bytes_sent += n as u64;
                    self.packets_sent += 1;
                    return Ok(());
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    // Timeout — re-check cancel and retry.
                    continue;
                }
                Err(e) => {
                    self.socket = None;
                    return Err(TransportError::Broken {
                        msg: format!("UDP send failed: {e}"),
                        errno_code: e.raw_os_error(),
                        cause: BrokenCause::Unspecified,
                    });
                }
            }
        }
    }

    fn max_payload(&self) -> usize {
        self.max_payload.saturating_sub(RTP_HEADER_LEN)
    }

    fn is_alive(&self) -> bool {
        self.socket.is_some()
    }

    fn close(&mut self) {
        self.socket = None;
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(self.cancel.clone() as Arc<dyn TransportCancel + Send + Sync>)
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        self.socket.as_ref()?;
        // `SocketStats` is `#[non_exhaustive]` in tst-core, so neither
        // a field-init expression nor `..Default::default()` works at a
        // distance — mirror tst-srt's `map_stats` and default-then-assign.
        #[allow(clippy::field_reassign_with_default)]
        let mut s = SocketStats::default();
        s.bytes_sent = self.bytes_sent;
        s.packets_sent = self.packets_sent;
        Some(s)
    }
}

impl Drop for RtpTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// Helper: 4 random bytes from `getrandom`.
fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    // `getrandom` Result type cannot fail on a healthy system; if it
    // somehow does, fall back to a process-stable default (0). Logging
    // a tracing event preserves the diagnostic.
    if let Err(e) = getrandom::getrandom(&mut buf) {
        tracing::warn!(error = %e, "getrandom failed; using zero for RTP randomness field");
    }
    u32::from_be_bytes(buf)
}

/// Size of the receive scratch buffer: one whole RTP packet, at the
/// largest size any source can legally deliver.
///
/// - **UDP:** the UDP length field is 16 bits, so 65535 is the ceiling on
///   any legal datagram — `UdpSocket::recv` into a buffer this large can
///   never truncate. Sizing from `pkt_size` instead would truncate
///   full-MTU datagrams from conformant peers (a 7×188 TS bundle is a
///   1328-byte packet, and CSRC/extension headers push it further —
///   CC=15 alone adds 60 bytes) because the OS silently discards the
///   excess bytes on a short `recv`.
/// - **Mpsc (TCP-interleaved):** RFC 7826 §14 interleaved frames carry a
///   u16 length prefix, so the same 65535 ceiling applies.
pub(crate) const RECV_SCRATCH_LEN: usize = 65535;

/// Sentinel `TransportError::Broken.msg` emitted by [`Source::recv_raw`]'s
/// mpsc arm when the interleaved pump thread drops its `Sender` (clean
/// RTSP teardown). Consumers that treat this disconnect as EOS — e.g.
/// `H264Receiver::recv_au` — match against this const, NOT a string
/// literal, so a wording change can't silently break the EOS contract.
pub(crate) const MPSC_PUMP_DISCONNECTED: &str = "interleaved pump bridge disconnected";

/// Inner data source for [`RtpRecvTransport`].
///
/// `Udp` — the Phase 1 default: read UDP datagrams off a bound socket
/// and strip the RTP header in `recv_bytes`.
///
/// `Mpsc` — the TCP-interleaved bridge introduced in Phase 2 Task 17:
/// the interleaved pump background thread parses `$<ch><len><data>`
/// frames off the RTSP control TCP and pushes **whole RTP packets**
/// (header intact) through the mpsc channel. `recv_bytes` decodes the
/// RTP header, enforces PT=33, strips CSRC/extension/padding, and
/// validates the MP2T payload shape — mirroring the UDP arm end-to-end.
pub(crate) enum Source {
    Udp(UdpSocket),
    Mpsc(std::sync::mpsc::Receiver<bytes::Bytes>),
}

impl Source {
    /// Block (polling `cancel` between timeouts) until one raw packet is
    /// available; copy it into `scratch` and return its byte length.
    ///
    /// # Contract
    ///
    /// - **Blocking with cancel polling:** for the UDP arm the socket's
    ///   `SO_RCVTIMEO` is set to `CANCEL_POLL_INTERVAL` at construction;
    ///   `WouldBlock`/`TimedOut` wakes the loop which re-checks `cancel`
    ///   before blocking again. The mpsc arm uses `recv_timeout` with the
    ///   same interval.
    ///
    /// - **Returns `Ok(n)`** when `scratch[..n]` holds the raw RTP packet
    ///   (header + payload, as received from the underlying source). The
    ///   caller is responsible for decoding the RTP header, applying PT
    ///   policy, stripping CSRC/extension/padding, and validating MP2T
    ///   shape before copying the TS payload into the caller's buffer.
    ///
    /// - **Returns `Err(TransportError::ExplicitClose)`** when `cancel` is
    ///   set.
    ///
    /// - **Returns `Err(TransportError::Backpressure)`** when `deadline` is
    ///   `Some` and has passed. Checked at the top of each poll iteration
    ///   (after the cancel check), so the granularity is one
    ///   [`CANCEL_POLL_INTERVAL`] (~100 ms) — a deadline can expire up to
    ///   that long after its instant before the caller observes it. The
    ///   source is left usable; callers may call `recv_raw` again.
    ///
    /// - **Returns `Err(TransportError::Broken)`** when the underlying
    ///   source reports a hard error:
    ///   - UDP: any `io::Error` that is not `WouldBlock`/`TimedOut` —
    ///     `errno_code` carries the OS errno.
    ///   - Mpsc: `RecvTimeoutError::Disconnected` (the pump's `Sender`
    ///     was dropped) — `errno_code` is `None`.
    ///   - Mpsc: a frame larger than `scratch` — `errno_code` is `None`.
    ///     Unreachable when `scratch` is [`RECV_SCRATCH_LEN`] bytes (the
    ///     interleaved u16 frame cap); kept as defence.
    ///
    /// # Scratch sizing
    ///
    /// `scratch` must be at least [`RECV_SCRATCH_LEN`] bytes. The UDP arm
    /// has **no oversize signal** — `UdpSocket::recv` silently truncates a
    /// datagram larger than the buffer — so only a buffer at the 16-bit
    /// datagram ceiling guarantees no legal packet (full-MTU 7×188 bundle,
    /// CSRC list, header extension) is corrupted.
    ///
    /// # Side effects
    ///
    /// `recv_raw` takes `&self` and performs no side effects on the source
    /// or any counters. Callers are responsible for:
    /// - Ticking `bytes_received` / `packets_received` after `Ok`.
    /// - Clearing `self.source = None` after a `Broken` return.
    pub(crate) fn recv_raw(
        &self,
        scratch: &mut [u8],
        cancel: &RtpCancelHandle,
        deadline: Option<Instant>,
    ) -> Result<usize, TransportError> {
        match self {
            Source::Udp(socket) => loop {
                if cancel.is_cancelled() {
                    return Err(TransportError::ExplicitClose);
                }
                if let Some(d) = deadline {
                    if Instant::now() >= d {
                        return Err(TransportError::Backpressure {
                            msg: "recv deadline elapsed".to_string(),
                            errno_code: None,
                        });
                    }
                }
                match socket.recv(scratch) {
                    Ok(0) => continue, // Zero-byte recv is meaningless on UDP; loop.
                    Ok(n) => return Ok(n),
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        return Err(TransportError::Broken {
                            msg: format!("UDP recv failed: {e}"),
                            errno_code: e.raw_os_error(),
                            cause: BrokenCause::Unspecified,
                        });
                    }
                }
            },
            Source::Mpsc(rx) => loop {
                if cancel.is_cancelled() {
                    return Err(TransportError::ExplicitClose);
                }
                if let Some(d) = deadline {
                    if Instant::now() >= d {
                        return Err(TransportError::Backpressure {
                            msg: "recv deadline elapsed".to_string(),
                            errno_code: None,
                        });
                    }
                }
                // Same cancel-poll cadence as the UDP path. recv_timeout
                // wakes on either a value arriving or the timeout
                // elapsing — the latter just loops to re-check cancel.
                match rx.recv_timeout(CANCEL_POLL_INTERVAL) {
                    Ok(packet) => {
                        if packet.len() > scratch.len() {
                            // Unreachable with a RECV_SCRATCH_LEN-sized
                            // scratch (interleaved frames are capped by
                            // their u16 length prefix) — kept as defence.
                            // Treat as broken so the recv shell stops the
                            // demux loop.
                            return Err(TransportError::Broken {
                                msg: format!(
                                    "interleaved frame ({} B) exceeds scratch buffer ({} B)",
                                    packet.len(),
                                    scratch.len()
                                ),
                                errno_code: None,
                                cause: BrokenCause::Unspecified,
                            });
                        }
                        let n = packet.len();
                        scratch[..n].copy_from_slice(&packet);
                        return Ok(n);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        // The pump thread dropped its Sender — surface as a
                        // broken transport so the recv shell can stop the
                        // demux loop.
                        return Err(TransportError::Broken {
                            msg: MPSC_PUMP_DISCONNECTED.to_string(),
                            errno_code: None,
                            cause: BrokenCause::Unspecified,
                        });
                    }
                }
            },
        }
    }
}

/// RTP receive-side transport: reads UDP datagrams, strips the 12-byte
/// RTP header, returns the TS payload bytes to callers.
///
/// Malformed packets (wrong version, wrong payload type, truncated,
/// CSRC list overflowing the datagram) are silently dropped — the
/// counter on [`Self::rtp_stats`] ticks for diagnosis. RFC 3550 §5.1
/// expects receivers to ignore unparseable packets.
///
/// # `SocketStats` field mapping
///
/// | [`SocketStats`] field | Source |
/// |---|---|
/// | `bytes_received` / `packets_received` | Local counters; incremented on every received datagram/chunk before RTP-header or MP2T-shape validation. Malformed-but-received packets are counted here; their drops are separately tracked in [`RtpStats::malformed_packets`]. |
/// | `rtt_us` | Always 0. RTT computation is deferred; see `docs/project/deferred-features.md` (RTCP statistics reporting). |
/// | `packets_lost_send` | RTCP RR cumulative-lost field. Populates on the same paths as `rtt_us`; `0` otherwise |
/// | `bytes_sent` / `packets_sent` | 0 (this is the receive half) |
/// | All other fields | 0 |
pub struct RtpRecvTransport {
    /// Underlying byte source — UDP socket or mpsc-fed
    /// TCP-interleaved bridge. `None` after [`Self::close`].
    source: Option<Source>,
    cancel: Arc<RtpCancelHandle>,
    bytes_received: u64,
    packets_received: u64,
    /// Counter for RTP packets that failed the header check.
    malformed_packets: u64,
    /// Per-recv scratch, sized to [`RECV_SCRATCH_LEN`] — heap allocated
    /// once; holds one whole RTP packet (header + payload) per recv.
    scratch: Vec<u8>,
    /// Companion RTCP socket bound on `port + 1` per RFC 3550 §11.
    /// `None` when the caller opted out via `RtpRecvSocketBuilder::rtcp(false)`.
    /// Retained on the transport so it stays alive for the reporter
    /// thread's lifetime — the reporter holds its own `try_clone`'d FD.
    #[allow(dead_code)]
    rtcp_socket: Option<UdpSocket>,
    /// RTCP-derived counters, shared with the reporter thread (which
    /// ticks `rr_packets_sent` on each RR emission) and any Task-8
    /// ingest path.
    rtcp_stats: Arc<Mutex<RtcpStats>>,
    /// Background RR-emitter handle. Dropping this cancels + joins
    /// the reporter thread. Held only for its `Drop` side effect.
    #[allow(dead_code)]
    rtcp_reporter: Option<RtcpReporterHandle>,
    /// Local SSRC used in the RR sender field — defaults to a random
    /// value generated at listen time; matches the RTP send-side
    /// pattern. Captured by the reporter closure and read by
    /// [`Self::from_mpsc_with_rtcp`] to seed the RTCP-ingest thread.
    ssrc: u32,
    /// Persistent deadline applied by the blocking
    /// [`RecvTransport::recv_bytes`] path. `None` (the default) blocks
    /// indefinitely. See [`Self::set_recv_timeout`].
    recv_timeout: Option<Duration>,
    /// Structured record of why the session ended. For an RTSP-backed
    /// transport (built via [`crate::rtsp::client::session::RtspSession::into_recv_transport`])
    /// this is a clone of the owning `RtspClient`'s slot — populated by
    /// the interleaved pump / keepalive threads. For a plain `rtp://`
    /// transport (`listen*` / `from_udp_socket`) this is a fresh slot
    /// written only by [`Self::close`] / an explicit
    /// [`Self::cancel_handle`] fire — see [`Self::set_end_reason_slot`].
    end_reason: EndReasonSlot,
}

/// RTP-protocol-level stats separate from [`SocketStats`].
///
/// Currently exposes only the malformed-packet counter. Future fields
/// (out-of-order delta, gap counter, etc.) can be added under
/// `#[non_exhaustive]` without breaking consumers.
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RtpStats {
    /// Number of UDP datagrams received whose RTP header was invalid
    /// (wrong V, wrong PT, truncated, CSRC overflow). Cumulative since
    /// `listen()`.
    /// Also counts datagrams with a valid RTP header (PT=33) whose
    /// MP2T payload fails RFC 2250 shape checks (not 188-byte aligned,
    /// missing `0x47` sync byte, or empty).
    pub malformed_packets: u64,
}

impl RtpRecvTransport {
    /// Bind to `url`'s host:port and (for multicast) join the group.
    ///
    /// The outgoing RTCP RR reporter is **off by default** (see
    /// [`Self::listen_with_rtcp`] for the experimental opt-in and its
    /// limitations). RTCP *reception* is unaffected.
    pub fn listen(url: &str) -> Result<Self, ConnectError> {
        let parsed = RtpUrl::parse(url).map_err(ConnectError::Url)?;
        Self::listen_with_rtcp(&parsed, false)
    }

    /// Bind using an already-parsed URL. The outgoing RTCP RR reporter is
    /// **off by default** (see [`Self::listen_with_rtcp`]).
    pub fn listen_with(url: &RtpUrl) -> Result<Self, ConnectError> {
        Self::listen_with_rtcp(url, false)
    }

    /// Bind using an already-parsed URL with an explicit RTCP toggle.
    ///
    /// `rtcp_enabled = true` binds the RTCP companion socket on `port + 1`
    /// and spawns the periodic RR-emitter thread. `rtcp_enabled = false`
    /// (the default for [`Self::listen`] / [`Self::listen_with`]) skips
    /// both.
    ///
    /// # Experimental: the RR reporter emits placeholder statistics
    ///
    /// The periodic RR reporter is **experimental and off by default**. It
    /// currently emits **placeholder (zero) statistics** — there are no
    /// live receiver counters wired into the RR (it carries an empty report
    /// block list, so no fraction-lost / cumulative-lost / jitter / last-SR
    /// values reach the peer). As such it is **NOT RFC 3550-conformant**
    /// and must not be relied on by senders for receiver-side reception
    /// quality. Enabling it is only useful for exercising the RTCP
    /// socket-pair plumbing. RTCP *reception* (ingesting peer SR/RR into
    /// [`Self::rtcp_stats`] and the projected [`SocketStats`] fields, e.g.
    /// the TCP-interleaved RTSP `rtt_us` / `packets_lost_send` path) is a
    /// separate, working path and is not affected by this toggle.
    pub fn listen_with_rtcp(url: &RtpUrl, rtcp_enabled: bool) -> Result<Self, ConnectError> {
        if url.pkt_size.is_some() {
            return Err(ConnectError::Url(RtpUrlError::RecvPktSize));
        }
        if url.pt.is_some() {
            return Err(ConnectError::PayloadTypeParam);
        }
        let ip: IpAddr = url.host.parse().map_err(|e: std::net::AddrParseError| {
            ConnectError::HostNotLiteral {
                host: url.host.clone(),
                detail: e.to_string(),
            }
        })?;
        // For multicast recv, bind to ANY:port and JoinMulticast on the
        // group; for unicast recv, bind to the literal host:port. This
        // matches GStreamer's `udpsrc address=...` behavior and tcpdump.
        let is_multicast = match ip {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        };
        let local: SocketAddr = if is_multicast {
            match ip {
                IpAddr::V4(_) => SocketAddr::new("0.0.0.0".parse().unwrap(), url.port),
                IpAddr::V6(_) => SocketAddr::new("::".parse().unwrap(), url.port),
            }
        } else {
            SocketAddr::new(ip, url.port)
        };
        let socket = if is_multicast {
            // SO_REUSEADDR (+ SO_REUSEPORT on BSD/macOS) allows a second
            // receiver to bind the same group:port. The helper also sets
            // SO_SNDTIMEO to CANCEL_POLL_INTERVAL; the RTP recv socket
            // never sends, so the write timeout is inert.
            bind_udp_socket_multicast(local).map_err(ConnectError::Io)?
        } else {
            UdpSocket::bind(local).map_err(ConnectError::Io)?
        };
        // For the multicast path this re-sets SO_RCVTIMEO to the same
        // value the helper already applied — redundant but harmless.
        socket
            .set_read_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(ConnectError::Io)?;
        if is_multicast {
            apply_multicast_recv_join(&socket, ip, url.iface.as_deref())
                .map_err(udp_err_to_connect)?;
        }
        // RTCP companion socket bound on `port + 1` per RFC 3550 §11.
        // Use the RTP socket's actual local port (kernel-assigned when
        // url.port == 0) — `url.port + 1` would resolve to port 1 in
        // that case, which is privileged.
        let rtcp_socket = if rtcp_enabled {
            let actual_rtp_port = socket.local_addr().map_err(ConnectError::Io)?.port();
            // Guard: if the kernel handed us port 65535 (or the caller
            // requested it explicitly), there is no valid RTCP companion
            // port. Skip the RTCP socket rather than wrapping to 0.
            // Mirrors the guard in `bind_server_udp_pair` in rtsp/server/handlers.rs.
            if let Some(rtcp_port) = actual_rtp_port.checked_add(1) {
                let rtcp_local: SocketAddr = if is_multicast {
                    match ip {
                        IpAddr::V4(_) => SocketAddr::new("0.0.0.0".parse().unwrap(), rtcp_port),
                        IpAddr::V6(_) => SocketAddr::new("::".parse().unwrap(), rtcp_port),
                    }
                } else {
                    SocketAddr::new(ip, rtcp_port)
                };
                Some(if is_multicast {
                    // SO_REUSEADDR (+ SO_REUSEPORT on BSD/macOS) allows a
                    // second receiver to bind the same RTCP companion port
                    // (group:port+1). The helper's SO_RCVTIMEO is inert —
                    // rtcp_socket is never read from on the recv transport
                    // path. SO_SNDTIMEO limits the RR-emitter's send_to
                    // but that result is discarded (`let _ = ...`).
                    bind_udp_socket_multicast(rtcp_local).map_err(ConnectError::Io)?
                } else {
                    UdpSocket::bind(rtcp_local).map_err(ConnectError::Io)?
                })
            } else {
                tracing::warn!(
                    "RTP port 65535 has no valid RTCP companion; \
                     RTCP companion socket skipped"
                );
                None
            }
        } else {
            None
        };
        let ssrc = url.ssrc.unwrap_or_else(random_u32);
        let rtcp_stats = Arc::new(Mutex::new(RtcpStats::default()));
        // Spawn the RR-emitter thread when RTCP is enabled. v1: target
        // is symmetric — RTP-port + 1 of the peer we last received
        // from. With no peer seen yet, target the URL's host:port+1
        // (the symmetric assumption for a known-destination receiver).
        let rtcp_reporter = rtcp_socket.as_ref().and_then(|sock| {
            let sock_clone = match sock.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "rtcp socket try_clone failed; skipping reporter");
                    return None;
                }
            };
            let stats_clone = rtcp_stats.clone();
            // For non-multicast, the URL host is the address we
            // bound to — so the symmetric RTCP target is host:port+1.
            // For multicast, there's no per-peer notion here; v1
            // doesn't emit RR until a peer is observed (Task 8's
            // ingest path lands that wiring). For now we still
            // spawn the thread so the rr_packets_sent counter
            // ticks deterministically against the URL host.
            //
            // Guard: url.port 65535 has no valid companion port.
            // The RTCP socket is already None in that case (guarded
            // above), so this arm is unreachable at 65535 — the
            // checked_add is a belt-and-suspenders defence.
            let Some(rtcp_companion_port) = url.port.checked_add(1) else {
                tracing::warn!("url port 65535 has no RTCP companion; skipping RR reporter");
                return None;
            };
            let rtcp_target = SocketAddr::new(ip, rtcp_companion_port);
            Some(RtcpReporterHandle::spawn(move || {
                let rr = ReceiverReport {
                    ssrc,
                    report_blocks: Vec::new(),
                };
                let cname = format!("tst-rtp-{ssrc:08x}");
                let sdes = SdesPacket { ssrc, cname };
                // Locally-built, well-formed packets (no report blocks,
                // short CNAME) — encode is fallible now but never fails
                // here; skip the send on the (unreachable) error.
                let (Ok(mut compound), Ok(sdes_bytes)) = (rr.encode(), sdes.encode()) else {
                    tracing::error!(
                        "internal: locally-built RTCP RR/SDES failed to encode; skipping send"
                    );
                    debug_assert!(false, "locally-built RTCP RR/SDES must always encode");
                    return;
                };
                compound.extend_from_slice(&sdes_bytes);
                let _ = sock_clone.send_to(&compound, rtcp_target);
                if let Ok(mut g) = stats_clone.lock() {
                    g.rr_packets_sent = g.rr_packets_sent.saturating_add(1);
                }
            }))
        });
        Ok(Self {
            source: Some(Source::Udp(socket)),
            cancel: RtpCancelHandle::new(),
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            rtcp_socket,
            rtcp_stats,
            rtcp_reporter,
            ssrc,
            recv_timeout: url.recv_timeout,
            end_reason: EndReasonSlot::default(),
        })
    }

    /// Build an `RtpRecvTransport` from an already-bound UDP socket.
    ///
    /// Used by [`crate::rtsp::client::session::RtspSession::into_recv_transport`]
    /// to wrap the UDP socket pair the RTSP SETUP exchange allocated,
    /// without re-binding via [`Self::listen`]. RTCP is not started here
    /// — the RTSP control plane drives RR/SR via a different path in the
    /// SETUP-direct flow.
    ///
    /// The caller is responsible for any platform-specific setup
    /// (multicast joins, TTL, etc.) before passing the socket in. This
    /// constructor only sets the cancel-poll read timeout to match the
    /// rest of the recv-side machinery.
    pub(crate) fn from_udp_socket(socket: UdpSocket) -> Result<Self, ConnectError> {
        socket
            .set_read_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(ConnectError::Io)?;
        let ssrc = random_u32();
        Ok(Self {
            source: Some(Source::Udp(socket)),
            cancel: RtpCancelHandle::new(),
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            rtcp_socket: None,
            rtcp_stats: Arc::new(Mutex::new(RtcpStats::default())),
            rtcp_reporter: None,
            ssrc,
            recv_timeout: None,
            end_reason: EndReasonSlot::default(),
        })
    }

    /// Construct an `RtpRecvTransport` whose source is an mpsc channel
    /// fed by the RTSP client's interleaved pump background thread.
    ///
    /// Used by
    /// [`crate::rtsp::client::session::RtspSession::into_recv_transport`]
    /// when SETUP negotiated TCP-interleaved transport. The producer
    /// (the pump thread inside the RtspClient) parses `$<ch><len><data>`
    /// frames off the RTSP control TCP, validates the RTP header, and
    /// pushes the **whole RTP packet** (header intact) into `rx`'s paired
    /// sender. `recv_bytes` on the resulting transport decodes the header,
    /// enforces PT=33, strips CSRC/extension/padding, applies MP2T shape
    /// checks, and copies the TS payload into the caller's buffer —
    /// mirroring the UDP arm end-to-end.
    ///
    /// `rx` is the consumer side of the bridge; the producer side
    /// (`Sender<Bytes>`) is held by the pump thread.
    pub(crate) fn from_mpsc_placeholder(rx: std::sync::mpsc::Receiver<bytes::Bytes>) -> Self {
        let ssrc = random_u32();
        Self {
            source: Some(Source::Mpsc(rx)),
            cancel: RtpCancelHandle::new(),
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            // Scratch holds one whole RTP packet (header + TS payload).
            // recv_raw (Source::Mpsc arm) copies the incoming Bytes into
            // scratch before recv_bytes decodes the header and strips the
            // payload — same buffer + ceiling as the UDP arm.
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            rtcp_socket: None,
            rtcp_stats: Arc::new(Mutex::new(RtcpStats::default())),
            rtcp_reporter: None,
            ssrc,
            recv_timeout: None,
            end_reason: EndReasonSlot::default(),
        }
    }

    /// Variant of [`Self::from_mpsc_placeholder`] that also spawns a
    /// background `rtsp-rtcp-ingest` thread to drain `rtcp_rx` (the
    /// RTCP channel demuxed by the RTSP client's interleaved pump) and
    /// feed each packet into the shared [`RtcpStats`] via [`ingest_rr`]
    /// or [`ingest_sr`]. Unknown PTs (SDES/BYE/APP/etc.) are counted
    /// as ignored and skipped.
    ///
    /// The `data_rx` channel carries **whole RTP packets** (header
    /// intact), as pushed by the pump — `recv_bytes` performs header
    /// decode, PT enforcement, and stripping on each dequeued packet.
    ///
    /// Used by
    /// [`crate::rtsp::client::session::RtspSession::into_recv_transport`]
    /// on the TCP-interleaved path. Without this constructor the RTCP
    /// channel is silently consumed and `socket_stats().rtt_us` /
    /// `packets_lost_send` stay at 0. The thread exits when the pump's
    /// `Sender<Bytes>` is dropped (which produces `mpsc::RecvError`).
    pub(crate) fn from_mpsc_with_rtcp(
        data_rx: std::sync::mpsc::Receiver<bytes::Bytes>,
        rtcp_rx: std::sync::mpsc::Receiver<bytes::Bytes>,
    ) -> Self {
        let t = Self::from_mpsc_placeholder(data_rx);
        spawn_rtcp_ingest(rtcp_rx, t.rtcp_stats.clone(), t.ssrc);
        t
    }

    /// RTP-protocol-level stats — separate from [`SocketStats`].
    pub fn rtp_stats(&self) -> RtpStats {
        RtpStats {
            malformed_packets: self.malformed_packets,
        }
    }

    /// Snapshot of the RTCP-derived counters. Returns a clone of the
    /// internal `RtcpStats` (cheap — counters are plain integers).
    pub fn rtcp_stats(&self) -> RtcpStats {
        self.rtcp_stats
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// The recorded [`StreamEndReason`], or `None` if the session hasn't
    /// ended yet (or ended through a path this arc doesn't instrument —
    /// see the field doc on `end_reason` for the plain-`rtp://` case).
    pub fn end_reason(&self) -> Option<StreamEndReason> {
        self.end_reason.get()
    }

    /// A cloneable, cross-thread-safe handle onto this transport's end
    /// reason — for a watchdog thread that wants to poll
    /// [`StreamEndReasonHandle::get`] without holding a reference to the
    /// transport itself (which a single-owner `recv_bytes` loop is
    /// using).
    pub fn end_reason_handle(&self) -> StreamEndReasonHandle {
        StreamEndReasonHandle::new(self.end_reason.clone())
    }

    /// Replace this transport's end-reason slot with `slot`.
    ///
    /// Used by [`crate::rtsp::client::session::RtspSession::into_recv_transport`]
    /// to swap in the owning `RtspClient`'s shared slot — so
    /// [`Self::end_reason`] reports the SAME reason the RTSP client's
    /// interleaved pump / keepalive threads recorded, rather than the
    /// fresh (always-empty) slot every constructor starts with. A plain
    /// `rtp://` transport (built via [`Self::listen`] et al., with no
    /// owning `RtspClient`) never has this called — its slot stays the
    /// fresh one from construction, written only by [`Self::close`] or
    /// an explicit [`Self::cancel_handle`] fire.
    pub(crate) fn set_end_reason_slot(&mut self, slot: EndReasonSlot) {
        self.end_reason = slot;
    }

    /// Configure a persistent receive deadline for the blocking
    /// [`RecvTransport::recv_bytes`] path — and therefore for any shell
    /// wrapping this transport (`DemuxReceiver`, `Receiver`,
    /// `RawReceiver`).
    ///
    /// With `Some(timeout)`, a `recv_bytes` call that sees no valid MP2T
    /// bundle within `timeout` returns
    /// [`TransportError::Backpressure`]; the transport stays alive and
    /// the next call starts a fresh deadline. This is the
    /// configured-knob mirror of [`Self::recv_timeout`]'s one-shot
    /// deadline, aligned with the SRT transport's builder-configured
    /// receive timeout: shells surface the expiry as their
    /// `Backpressure`-kind error (see `DemuxReceiverError`'s
    /// reachable-kinds table in `tst-pipeline`) rather than a terminal
    /// one. That makes a deadline-driven stall watchdog possible with no
    /// cancel thread — a stalled-but-healthy session (peer stops
    /// sending; no error, no EOS) hands control back to the caller every
    /// `timeout`. `ManagedRecvTransport` propagates the `Backpressure`
    /// unchanged (a recv timeout is not a reconnect trigger).
    ///
    /// `None` (the default) restores the indefinite-block behavior.
    ///
    /// Deadline granularity is the internal cancel-poll interval
    /// (~100 ms). A `timeout` too large to represent as a deadline
    /// (e.g. `Duration::MAX`) saturates to "no deadline". The one-shot
    /// [`Self::recv_timeout`] method ignores this setting — its explicit
    /// argument always wins for that call.
    ///
    /// The setting belongs to THIS transport instance. A
    /// `ManagedRecvTransport` factory that rebuilds an
    /// `RtpRecvTransport` after a genuine `Closed`/`Broken` error must
    /// call `set_recv_timeout` on the newly constructed transport —
    /// configuration does not automatically cross a factory
    /// reconstruction.
    pub fn set_recv_timeout(&mut self, timeout: Option<Duration>) {
        self.recv_timeout = timeout;
    }

    /// [`RecvTransport::recv_bytes`] with a deadline. Returns `Ok(None)` if
    /// no valid MP2T bundle arrives within `timeout` — the transport
    /// stays alive and callers may call this again to keep waiting.
    /// Mirrors `UdpRecvTransport::recv_timeout`'s `Ok(None)` shape
    /// (`crates/tst-udp/src/recv.rs`); `recv_bytes` still blocks
    /// indefinitely unless a persistent deadline was configured via
    /// [`Self::set_recv_timeout`] (which this one-shot method ignores —
    /// the explicit `timeout` argument always wins for this call).
    ///
    /// Deadline granularity is the internal cancel-poll interval
    /// (~100 ms): RTP-header-decode failures and PT mismatches keep
    /// retrying inside the receive loop, but each retry re-checks the
    /// same absolute deadline rather than extending it.
    pub fn recv_timeout(
        &mut self,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<Option<usize>, TransportError> {
        // checked_add: a timeout too large to represent as an `Instant`
        // (e.g. `Duration::MAX`) saturates to "no deadline" rather than
        // panicking on a public input; `Duration::ZERO` expires at the
        // first poll.
        match self.recv_bytes_inner(buf, Instant::now().checked_add(timeout)) {
            Ok(n) => Ok(Some(n)),
            Err(TransportError::Backpressure { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Shared body for [`RecvTransport::recv_bytes`] and
    /// [`Self::recv_timeout`]. `deadline` is `None` for an unbounded
    /// wait (the trait `recv_bytes` with no
    /// [`Self::set_recv_timeout`] value configured); `Some` — from the
    /// one-shot `recv_timeout` argument or the configured persistent
    /// timeout — threads an absolute instant down to
    /// [`Source::recv_raw`] so a stalled source (no packets, no
    /// disconnect) can surface `Backpressure` instead of blocking
    /// forever.
    fn recv_bytes_inner(
        &mut self,
        buf: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, TransportError> {
        if self.source.is_none() {
            return Err(TransportError::Closed);
        }
        loop {
            // Borrow source for the duration of recv_raw only; we re-check
            // self.source at the top of each iteration via the outer guard
            // so the mutable `self.source = None` on Broken is unambiguous.
            let raw_result = self
                .source
                .as_ref()
                .expect("source checked above; cannot be None here")
                .recv_raw(&mut self.scratch, &self.cancel, deadline);
            let n = match raw_result {
                Ok(n) => n,
                Err(TransportError::Broken { ref msg, .. })
                    if msg == MPSC_PUMP_DISCONNECTED
                        && matches!(
                            self.end_reason.get(),
                            Some(StreamEndReason::CleanTeardown)
                        ) =>
                {
                    // The pump's `Sender` drop (Disconnected) looks
                    // identical whether the peer TEARDOWN'd cleanly or the
                    // wire broke — the recorded end reason (set by the SAME
                    // pump thread strictly before it returns and drops the
                    // Sender, see interleaved_pump.rs's `Ok(0)` arm) tells
                    // them apart. Only a recorded CleanTeardown remaps this
                    // disconnect to `Closed` (→ `EndOfStream` at the
                    // Receiver/DemuxReceiver shells); every other reason —
                    // or a still-empty slot, e.g. a plain `rtp://` source
                    // with no owning `RtspClient` — falls through to the
                    // generic `Broken` arm below unchanged.
                    self.source = None;
                    return Err(TransportError::Closed);
                }
                Err(e @ TransportError::Broken { .. }) => {
                    // Hard error from the underlying source — mark transport
                    // dead (same as both pre-refactor arms did) then propagate.
                    self.source = None;
                    return Err(e);
                }
                Err(e @ TransportError::ExplicitClose) => {
                    // `cancel` was observed set by the underlying source —
                    // either `Self::close` already ran (which also records
                    // this directly, since a closed transport with no recv
                    // in flight never reaches here) or a bare
                    // `cancel_handle().cancel()` fired while this call was
                    // blocked. Either way the session ended because the
                    // caller asked it to, not a wire failure.
                    self.end_reason.record(StreamEndReason::Cancelled);
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            // Count at wire-level, before validation — consistent with the
            // pre-refactor UDP path (incremented on Ok(n) before RTP-header or
            // MP2T-shape checks). Malformed-but-received packets are counted
            // here; drops are tracked in `malformed_packets`.
            self.bytes_received = self.bytes_received.saturating_add(n as u64);
            self.packets_received = self.packets_received.saturating_add(1);
            // Decode the RTP header from scratch — applies to both UDP and
            // mpsc paths after recv_raw copied the whole packet into scratch.
            let parsed = match RtpHeader::decode(&self.scratch[..n]) {
                Ok(p) => p,
                Err(parse_err) => {
                    self.malformed_packets = self.malformed_packets.saturating_add(1);
                    tracing::debug!(
                        error = ?parse_err,
                        "RTP packet rejected at recv; counter ticked",
                    );
                    continue;
                }
            };
            if parsed.header.payload_type != RTP_PT_MP2T {
                self.malformed_packets = self.malformed_packets.saturating_add(1);
                tracing::debug!(
                    pt = parsed.header.payload_type,
                    "non-MP2T payload type at MP2T receiver; packet dropped",
                );
                continue;
            }
            // Use payload_end (not n) to exclude any RFC 3550 padding bytes
            // and to reflect extension skipping.
            let payload = &self.scratch[parsed.payload_offset..parsed.payload_end];
            if payload.len() > buf.len() {
                // Caller buf too small. Treat as broken, since the recv shell
                // is misconfigured (it should have sized buf to at least
                // max_payload()).
                return Err(TransportError::Broken {
                    msg: format!("recv buf too small: {} < {}", buf.len(), payload.len()),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            }
            // DA-RTP-5: RFC 2250 shape guard — payload must be non-empty,
            // 188-byte aligned, and begin with 0x47. RTP-header validation
            // above already pinned PT=33 (MP2T); this catches a corrupt or
            // misaligned bundle.
            if !is_valid_mp2t_payload(payload) {
                self.malformed_packets = self.malformed_packets.saturating_add(1);
                tracing::debug!(
                    payload_len = payload.len(),
                    first_byte = payload.first().copied().unwrap_or(0),
                    "MP2T payload shape invalid (len%188≠0 or no 0x47 sync byte); packet dropped",
                );
                continue;
            }
            buf[..payload.len()].copy_from_slice(payload);
            return Ok(payload.len());
        }
    }
}

impl RecvTransport for RtpRecvTransport {
    /// Receive the next valid MP2T bundle from the RTP stream.
    ///
    /// # MP2T shape enforcement (DA-RTP-5)
    ///
    /// After stripping and validating the RTP header (V=2, PT=33), the payload
    /// is checked against RFC 2250 shape requirements before being returned:
    ///
    /// - Non-empty
    /// - `len % 188 == 0` — integral number of 188-byte TS packets
    /// - First byte is `0x47` — the TS sync byte of the leading packet
    ///
    /// Payloads that fail any of these checks are **silently dropped**
    /// (the `malformed_packets` counter in [`RtpStats`] is incremented, and the
    /// recv loop continues to the next datagram). The same check applies to the
    /// TCP-interleaved mpsc path.
    ///
    /// The demuxer's own resync logic remains defense-in-depth and is
    /// unchanged.
    ///
    /// # Blocking and the configured deadline
    ///
    /// Blocks indefinitely by default. With a persistent deadline
    /// configured via [`RtpRecvTransport::set_recv_timeout`], returns
    /// [`TransportError::Backpressure`] when no valid MP2T bundle
    /// arrives within the configured window — the transport stays
    /// alive and the next call starts a fresh deadline (the receive
    /// shells surface this as their `Backpressure`-kind error).
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        // checked_add mirrors `recv_timeout`'s saturation: a configured
        // timeout too large to represent as an `Instant` (e.g.
        // `Duration::MAX`) means "no deadline" rather than a panic on a
        // public input.
        let deadline = self
            .recv_timeout
            .and_then(|t| Instant::now().checked_add(t));
        self.recv_bytes_inner(buf, deadline)
    }

    fn max_payload(&self) -> usize {
        // Recv-side deliverable ceiling (see RecvTransport::max_payload
        // in tst-core): one whole RTP payload at the largest size any
        // legal source can deliver — RECV_SCRATCH_LEN (the 16-bit
        // UDP-datagram / interleaved-frame limit) minus the fixed RTP
        // header. Deliberately NOT derived from the URL's pkt_size:
        // that is the send-side budget, and conformant foreign senders
        // (gst/ffmpeg full-MTU 7×188 bundles) exceed it.
        RECV_SCRATCH_LEN - RTP_HEADER_LEN
    }

    fn is_alive(&self) -> bool {
        self.source.is_some()
    }

    fn close(&mut self) {
        self.source = None;
        self.end_reason.record(StreamEndReason::Cancelled);
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(self.cancel.clone() as Arc<dyn TransportCancel + Send + Sync>)
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        self.source.as_ref()?;
        #[allow(clippy::field_reassign_with_default)]
        // SocketStats is #[non_exhaustive] in tst-core, so the
        // default-and-assign pattern is the only way to construct one
        // from outside that crate.
        let mut s = SocketStats::default();
        s.bytes_received = self.bytes_received;
        s.packets_received = self.packets_received;
        // Project RTCP-derived fields when ingest has populated them.
        // Paths without an ingest thread (UDP today, mpsc-placeholder
        // pre-T28) leave these at 0, matching prior behavior.
        if let Ok(rtcp) = self.rtcp_stats.lock() {
            s.rtt_us = rtcp.rtt_us;
            s.packets_lost_send = rtcp.cumulative_lost_send as u64;
        }
        Some(s)
    }
}

impl Drop for RtpRecvTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// Returns `true` iff `payload` is a well-formed RFC 2250 MP2T bundle:
/// non-empty, an integral number of 188-byte TS packets, and the first
/// byte is the TS sync byte `0x47`.
///
/// Used by [`RtpRecvTransport::recv_bytes`] to gate payloads before they
/// reach the demuxer. A failed check ticks `malformed_packets`.
#[inline]
fn is_valid_mp2t_payload(payload: &[u8]) -> bool {
    !payload.is_empty() && payload.len() % 188 == 0 && payload[0] == 0x47
}

/// Spawn the `rtsp-rtcp-ingest` background thread.
///
/// Drains `rtcp_rx` (fed by the RTSP client's interleaved pump on the
/// RFC 7826 §14 RTCP channel), parses each frame, and dispatches to
/// [`ingest_sr`] (PT=200) or [`ingest_rr`] (PT=201). Unknown PTs
/// (SDES/BYE/APP) are ignored — they don't affect the projected
/// `SocketStats` fields. Parse errors increment
/// `rtcp_stats.sr_parse_errors` / `rr_parse_errors`.
///
/// The thread exits cleanly when the pump's `Sender<Bytes>` drops
/// (`rtcp_rx.recv()` returns `Err(mpsc::RecvError)`). It is detached
/// (no join) — the transport's lifecycle holds the consumer side, and
/// once the transport drops + the pump dies the channel closes.
fn spawn_rtcp_ingest(
    rtcp_rx: std::sync::mpsc::Receiver<bytes::Bytes>,
    stats: Arc<Mutex<RtcpStats>>,
    our_ssrc: u32,
) {
    let spawn_result = std::thread::Builder::new()
        .name("rtsp-rtcp-ingest".to_string())
        .spawn(move || {
            while let Ok(bytes) = rtcp_rx.recv() {
                // RFC 3550 §6.4 — PT byte lives at offset 1 of every RTCP packet.
                // Packets shorter than 2 bytes are non-conformant; drop silently.
                if bytes.len() < 2 {
                    continue;
                }
                let pt = bytes[1];
                match pt {
                    200 => match SenderReport::decode(&bytes) {
                        Ok((sr, _)) => {
                            if let Ok(mut g) = stats.lock() {
                                ingest_sr(&mut g, &sr);
                            }
                        }
                        Err(_) => {
                            if let Ok(mut g) = stats.lock() {
                                g.sr_parse_errors = g.sr_parse_errors.saturating_add(1);
                            }
                        }
                    },
                    201 => match ReceiverReport::decode(&bytes) {
                        Ok((rr, _)) => {
                            if let Ok(mut g) = stats.lock() {
                                ingest_rr(&mut g, our_ssrc, &rr);
                            }
                        }
                        Err(_) => {
                            if let Ok(mut g) = stats.lock() {
                                g.rr_parse_errors = g.rr_parse_errors.saturating_add(1);
                            }
                        }
                    },
                    _ => continue, // SDES/BYE/APP/etc. — ignored for stats.
                }
            }
        });
    // Degrade gracefully instead of panicking: a thread-spawn failure (OS
    // resource exhaustion) must NOT panic — this runs on the RTSP connect path,
    // and the JVM/C bindings do not catch unwinds across the FFI boundary, so a
    // panic here would abort the host process. If ingest can't start, RTCP stats
    // simply stay unpopulated (reporting is best-effort / experimental anyway).
    if let Err(e) = spawn_result {
        tracing::warn!(
            target: "tst_rtp",
            error = %e,
            "failed to spawn rtsp-rtcp-ingest thread; RTCP stats will not be collected"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::RtpRecvSocketBuilder;

    /// Build a minimal valid RTP packet (V=2, P=0, X=0, CC=0, PT=33)
    /// wrapping `payload`. Used by mpsc-path tests that feed whole RTP
    /// packets into `from_mpsc_placeholder`, matching the pump's new
    /// contract (the pump no longer strips the header before enqueuing).
    fn make_rtp_packet(payload: &[u8]) -> bytes::Bytes {
        use crate::packet::RtpHeader;
        let mut pkt = vec![0u8; RTP_HEADER_LEN];
        RtpHeader::new(0, 0, 0).encode_into(&mut pkt);
        pkt.extend_from_slice(payload);
        bytes::Bytes::from(pkt)
    }

    /// Verify socket_stats() now returns Some(_) once Task 9 wires up
    /// the local counters. bytes_sent / packets_sent advance through
    /// the integration test in Task 14; here we just check the shape.
    #[test]
    fn socket_stats_returns_some_when_alive() {
        let url = RtpUrl::parse("rtp://127.0.0.1:1").unwrap();
        let t = RtpTransport::connect_with(&url).unwrap();
        let stats = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(stats.bytes_sent, 0);
        assert_eq!(stats.packets_sent, 0);
        // RTCP-derived fields should stay zero in Phase 1.
        assert_eq!(stats.rtt_us, 0);
        assert_eq!(stats.packets_lost_send, 0);
    }

    /// A quiet socket (no sender) must not block `recv_timeout` past its
    /// deadline — the RTP-side counterpart of the field report's stall
    /// case. Mirrors `tst-udp`'s `close_unblocks_recv_bytes_after_recv_timeout`
    /// shape: `Ok(None)` on expiry, transport still alive.
    #[test]
    fn rtp_recv_timeout_returns_none_on_quiet_socket() {
        let mut t = RtpRecvSocketBuilder::new("127.0.0.1", 0).build().unwrap();
        let mut buf = vec![0u8; 2048];
        let start = std::time::Instant::now();
        let res = t
            .recv_timeout(&mut buf, std::time::Duration::from_millis(300))
            .unwrap();
        assert!(res.is_none());
        let dt = start.elapsed();
        assert!(
            dt >= Duration::from_millis(250) && dt < Duration::from_secs(5),
            "elapsed {dt:?}"
        );
        assert!(t.is_alive(), "transport must stay usable after a timeout");
        // A second call after an expiry must behave identically — pins
        // that a timeout leaves no poisoned state behind (deadline
        // bookkeeping, socket read-timeout restoration).
        let res2 = t
            .recv_timeout(&mut buf, std::time::Duration::from_millis(150))
            .unwrap();
        assert!(res2.is_none());
        assert!(t.is_alive(), "transport must survive repeated timeouts");
    }

    /// Extreme timeout inputs are specified, not panics: ZERO expires at
    /// the first poll (`Ok(None)`); MAX saturates to "no deadline" via
    /// `checked_add` (the old unchecked add panicked) — proven by a
    /// delayed packet actually being received under a MAX timeout.
    #[test]
    fn rtp_recv_timeout_extreme_durations_never_panic() {
        // Discover a free base/base+1 pair first (RTP transports expose no
        // local-addr accessor; RTCP auto-binds port+1) — the same pattern
        // as the loopback integration tests.
        let port = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let base = s.local_addr().unwrap().port();
            assert!(base < u16::MAX, "ephemeral port at u16::MAX");
            drop(std::net::UdpSocket::bind(("127.0.0.1", base + 1)).unwrap());
            base
        };
        let mut t = RtpRecvSocketBuilder::new("127.0.0.1", port)
            .build()
            .unwrap();
        let mut buf = vec![0u8; 2048];
        let res = t.recv_timeout(&mut buf, std::time::Duration::ZERO).unwrap();
        assert!(res.is_none(), "ZERO must expire promptly");
        // MAX: a peer sends one minimal RTP packet after a short delay;
        // the saturated no-deadline wait must deliver it (not panic, not
        // return instantly empty).
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            // RTP header (V=2, PT=33) + one valid MP2T packet as payload —
            // the receive path discards degenerate payloads (must be a
            // non-empty 188-multiple starting with the 0x47 sync byte),
            // so a header-only packet would be dropped and the wait would
            // continue.
            let mut pkt = vec![0u8; 12 + 188];
            pkt[0] = 0x80;
            pkt[1] = 33;
            pkt[12] = 0x47;
            let _ = s.send_to(&pkt, ("127.0.0.1", port));
        });
        let res = t.recv_timeout(&mut buf, std::time::Duration::MAX).unwrap();
        assert!(res.is_some(), "MAX wait must deliver the delayed packet");
        sender.join().unwrap();
    }

    /// Configured-timeout knob (`set_recv_timeout`): the blocking TRAIT
    /// `recv_bytes` path must honor a persistent deadline, surfacing
    /// `TransportError::Backpressure` on expiry with the transport left
    /// alive — the RTP mirror of `tst-srt`'s `SocketBuilder::recv_timeout`.
    /// Runs on the mpsc source: both sources share `recv_bytes_inner`'s
    /// deadline plumbing (per-source expiry mechanics are pinned by the
    /// one-shot `recv_timeout` tests above), and mpsc gives a
    /// deterministic unblock lever (drop the sender) so a regression to
    /// infinite-block fails loudly instead of wedging the suite.
    #[test]
    fn configured_recv_timeout_bounds_trait_recv_bytes() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(rx);
        t.set_recv_timeout(Some(Duration::from_millis(300)));

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let start = std::time::Instant::now();
            let r = t.recv_bytes(&mut buf);
            (r, start.elapsed(), t)
        });
        // Hang-proof join: if recv_bytes ignores the configured deadline
        // (the pre-knob behavior), dropping the sender unblocks the
        // parked recv via the mpsc-disconnect path so this test FAILS
        // instead of hanging the binary.
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            drop(tx);
            let _ = worker.join();
            panic!("recv_bytes ignored the configured timeout (blocked >= 5 s)");
        }
        let (result, elapsed, mut t) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?}"
        );
        assert!(t.is_alive(), "transport must stay alive after expiry");

        // The session stays usable: a packet queued after the expiry is
        // delivered by the very next call.
        let payload = {
            let mut p = [0u8; 188];
            p[0] = 0x47;
            p
        };
        tx.send(make_rtp_packet(&payload)).unwrap();
        let mut buf = vec![0u8; 2048];
        let n = t
            .recv_bytes(&mut buf)
            .expect("post-expiry recv must deliver");
        assert_eq!(n, 188);
        assert_eq!(buf[0], 0x47);

        // And a second quiet interval expires the same way — pins that a
        // timeout leaves no poisoned deadline bookkeeping behind.
        match t.recv_bytes(&mut buf) {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on second expiry, got {other:?}"),
        }
    }

    /// The headline consumer contract: `DemuxReceiver` over an
    /// `RtpRecvTransport` with a configured timeout surfaces
    /// `ShellErrorKind::Backpressure` on a stalled-but-healthy session
    /// (peer stops sending; no error, no EOS) — the shell's documented
    /// Backpressure-on-recv-timeout path, previously reachable only on
    /// SRT — and the same receiver keeps demuxing once bytes flow again.
    /// A deadline-driven stall watchdog with no cancel thread.
    #[test]
    fn demux_receiver_surfaces_backpressure_then_keeps_demuxing() {
        use tst_core::mpegts::common::Pts90khz;
        use tst_core::mpegts::demux::DemuxEvent;
        use tst_core::mpegts::mux::{Muxer, MuxerConfig};
        use tst_pipeline::{DemuxReceiver, ShellErrorKind};

        // Real muxed TS bytes (PAT + PMT + video PES) so the demuxer has
        // something to emit once the stall ends. Ten AUs, not one: the
        // receive shell's TS syncer needs several consecutive aligned
        // packets to declare lock before it emits the FIRST packet (see
        // `Receiver::next_packet`'s doc example), so a 3-packet burst
        // would never reach the demuxer. Minimal Annex-B AUs: the muxer
        // doesn't parse past the start-code shape.
        let mut mux = Muxer::new(MuxerConfig::default()).expect("valid default config");
        let au = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
        for i in 0..10i64 {
            mux.push_video(&au, Pts90khz::new(i * 3000), i == 0)
                .expect("push_video");
        }
        let mut ts = Vec::new();
        let mut pkt = [0u8; 188];
        loop {
            let n = mux.pull(&mut pkt);
            if n == 0 {
                break;
            }
            ts.extend_from_slice(&pkt[..n]);
        }
        assert!(
            !ts.is_empty() && ts.len() % 188 == 0,
            "muxer output shape (got {} bytes)",
            ts.len()
        );

        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(rx);
        t.set_recv_timeout(Some(Duration::from_millis(300)));
        let mut shell = DemuxReceiver::new(t);

        let worker = std::thread::spawn(move || {
            let r = shell.recv_event();
            (r, shell)
        });
        // Same hang-proofing as the trait-level test above.
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            drop(tx);
            let _ = worker.join();
            panic!("recv_event ignored the configured timeout (blocked >= 5 s)");
        }
        let (result, mut shell) = worker.join().unwrap();
        let err = match result {
            Err(e) => e,
            other => panic!("expected a Backpressure-kind error on stall, got {other:?}"),
        };
        assert_eq!(
            err.kind,
            ShellErrorKind::Backpressure,
            "stall must surface as Backpressure, got {err:?}"
        );

        // Stall ends: the muxed bytes arrive as one RTP packet and the
        // SAME receiver emits the PSI map — no rebuild, no cancel thread.
        tx.send(make_rtp_packet(&ts)).unwrap();
        let ev = shell
            .recv_event()
            .expect("post-stall recv_event must succeed")
            .expect("post-stall recv_event must yield an event");
        assert!(
            matches!(ev, DemuxEvent::ProgramMap(_)),
            "first event should be the PSI map"
        );
    }

    /// `Duration::MAX` as a configured timeout must saturate to "no
    /// deadline" (checked_add), not panic — the configured-path mirror
    /// of the one-shot extreme-duration pin above.
    #[test]
    fn configured_recv_timeout_duration_max_saturates() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(rx);
        t.set_recv_timeout(Some(Duration::MAX));
        let payload = {
            let mut p = [0u8; 188];
            p[0] = 0x47;
            p
        };
        tx.send(make_rtp_packet(&payload)).unwrap();
        let mut buf = vec![0u8; 2048];
        let n = t
            .recv_bytes(&mut buf)
            .expect("MAX-saturated recv must deliver");
        assert_eq!(n, 188);
    }

    /// `set_recv_timeout(None)` restores the indefinite-block contract:
    /// after clearing a previously configured 250 ms timeout, a quiet
    /// recv must NOT surface Backpressure at the old deadline. The
    /// timing assertion points in the safe direction — an early worker
    /// return can only mean the cleared timeout still fired.
    #[test]
    fn set_recv_timeout_none_restores_infinite_block() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(rx);
        t.set_recv_timeout(Some(Duration::from_millis(250)));
        t.set_recv_timeout(None);

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            t.recv_bytes(&mut buf)
        });
        // Watch well past the cleared 250 ms deadline (plus the ~100 ms
        // poll granularity): the worker must still be parked throughout.
        let check_deadline = std::time::Instant::now() + Duration::from_millis(900);
        while std::time::Instant::now() < check_deadline {
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !worker.is_finished(),
                "recv_bytes returned early — the cleared timeout still fired"
            );
        }
        // Unblock via channel disconnect and confirm the exit is the
        // disconnect error, not a stale Backpressure.
        drop(tx);
        match worker.join().unwrap() {
            Err(TransportError::Backpressure { .. }) => {
                panic!("cleared timeout must not produce Backpressure")
            }
            Err(_) => {}
            Ok(n) => panic!("unexpected data on an empty channel: {n}"),
        }
    }

    /// The one-shot `recv_timeout` ignores the configured persistent
    /// value — its explicit argument wins. With a 60 s persistent
    /// timeout configured, an explicit 250 ms one-shot must still
    /// expire at ~250 ms (`Ok(None)`), not inherit the 60 s value.
    #[test]
    fn one_shot_recv_timeout_wins_over_configured() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(rx);
        t.set_recv_timeout(Some(Duration::from_secs(60)));

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let start = std::time::Instant::now();
            let r = t.recv_timeout(&mut buf, Duration::from_millis(250));
            (r, start.elapsed())
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            drop(tx);
            let _ = worker.join();
            panic!("one-shot recv_timeout inherited the 60 s configured value");
        }
        let (result, elapsed) = worker.join().unwrap();
        assert!(
            result.expect("one-shot expiry is Ok(None)").is_none(),
            "expected expiry, got data"
        );
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?}"
        );
    }

    /// The configured knob on the REAL UDP source — the persistent-path
    /// twin of the mpsc trait-level test above: a quiet socket with a
    /// 300 ms configured timeout must surface `Backpressure` through the
    /// trait `recv_bytes`, transport still alive. The port must be known
    /// up front (the transport exposes no local-addr getter and the
    /// wake-packet unblock lever below needs a destination), so the test
    /// discovers-then-releases an ephemeral port and retries the build
    /// on the rare cross-process steal. RTCP stays at the builder
    /// default (off) — no companion port is involved.
    #[test]
    fn configured_recv_timeout_bounds_udp_trait_recv_bytes() {
        let (mut t, port) = {
            let mut out = None;
            for _ in 0..50 {
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let candidate = s.local_addr().unwrap().port();
                drop(s);
                if let Ok(built) = RtpRecvSocketBuilder::new("127.0.0.1", candidate).build() {
                    out = Some((built, candidate));
                    break;
                }
            }
            out.expect("could not allocate a loopback UDP port in 50 attempts")
        };
        t.set_recv_timeout(Some(Duration::from_millis(300)));

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let start = std::time::Instant::now();
            let r = t.recv_bytes(&mut buf);
            (r, start.elapsed(), t)
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            // Unblock lever: a valid RTP+MP2T packet makes a hung recv
            // return data so the test FAILS instead of wedging.
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut pkt = vec![0u8; 12 + 188];
            pkt[0] = 0x80;
            pkt[1] = 33;
            pkt[12] = 0x47;
            let _ = s.send_to(&pkt, ("127.0.0.1", port));
            let _ = worker.join();
            panic!("UDP trait recv_bytes ignored the configured timeout");
        }
        let (result, elapsed, t) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?}"
        );
        assert!(t.is_alive(), "transport must stay alive after expiry");
    }

    /// Task A2: the `?recv_timeout=<ms>` URL knob (parsed by A1) must arm
    /// the transport with NO explicit `set_recv_timeout` call —
    /// `RtpRecvTransport::listen`'s URL path is the sibling of the
    /// builder/trait-level knob tests above, proving the query key alone
    /// is enough. Same ephemeral-port-discovery + unblock-lever pattern as
    /// `configured_recv_timeout_bounds_udp_trait_recv_bytes` (the transport
    /// exposes no local-addr getter). Asserts the error kind, not timing —
    /// the elapsed-duration pin already lives on the setter-driven test
    /// above.
    #[test]
    fn url_recv_timeout_arms_transport_without_explicit_setter() {
        let (mut t, port) = {
            let mut out = None;
            for _ in 0..50 {
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let candidate = s.local_addr().unwrap().port();
                drop(s);
                let url = format!("rtp://127.0.0.1:{candidate}?recv_timeout=200");
                if let Ok(built) = RtpRecvTransport::listen(&url) {
                    out = Some((built, candidate));
                    break;
                }
            }
            out.expect("could not allocate a loopback UDP port in 50 attempts")
        };

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let r = t.recv_bytes(&mut buf);
            (r, t)
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            // Unblock lever: a valid RTP+MP2T packet makes a hung recv
            // return data so the test FAILS instead of wedging.
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut pkt = vec![0u8; 12 + 188];
            pkt[0] = 0x80;
            pkt[1] = 33;
            pkt[12] = 0x47;
            let _ = s.send_to(&pkt, ("127.0.0.1", port));
            let _ = worker.join();
            panic!("?recv_timeout= URL knob did not arm the transport (blocked >= 5 s)");
        }
        let (result, t) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
        assert!(t.is_alive(), "transport must stay alive after expiry");
    }

    /// Compile-time check: RtpTransport satisfies Transport (so
    /// MuxSender<RtpTransport> works) and RtpRecvTransport satisfies
    /// RecvTransport (so DemuxReceiver<RtpRecvTransport> works).
    /// Catches signature drift if the pipeline shell trait bounds tighten.
    #[test]
    fn satisfies_pipeline_trait_bounds() {
        fn accept_send<T: Transport>(_: T) {}
        fn accept_recv<T: RecvTransport>(_: T) {}
        // Use port 0 / 1 to avoid binding conflicts in CI.
        let send_url = RtpUrl::parse("rtp://127.0.0.1:1").unwrap();
        let recv_url = RtpUrl::parse("rtp://127.0.0.1:0").unwrap();
        let send = RtpTransport::connect_with(&send_url).unwrap();
        let recv = RtpRecvTransport::listen_with(&recv_url).unwrap();
        accept_send(send);
        accept_recv(recv);
    }

    /// T30 — verify that an RR pushed onto the rtcp_rx channel of an
    /// `RtpRecvTransport` built via `from_mpsc_with_rtcp` reaches the
    /// ingest thread, populates `RtcpStats.cumulative_lost_send`, and
    /// is projected into `socket_stats().packets_lost_send`. Closes
    /// the Stage 3 RR-on-interleaved-channel deliverable.
    #[test]
    fn rr_on_rtcp_rx_populates_packets_lost_send() {
        use crate::rtcp::{ReceiverReport, ReportBlock};

        let (_data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let (rtcp_tx, rtcp_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let t = RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx);
        let our_ssrc = t.ssrc;

        // Build an RR whose first report block references our SSRC and
        // reports 2024 cumulative losses.
        let rb = ReportBlock {
            ssrc: our_ssrc,
            fraction_lost: 0x40, // 25% q8
            cumulative_lost: 2024,
            extended_highest_seq: 9000,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0xDEAD_BEEF,
            report_blocks: vec![rb],
        };
        let bytes = bytes::Bytes::from(rr.encode().unwrap());
        rtcp_tx.send(bytes).expect("send RR onto rtcp_rx");

        // Spin briefly for the ingest thread to wake + process.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut surfaced = 0u64;
        while std::time::Instant::now() < deadline {
            let s = t.socket_stats().expect("alive transport reports stats");
            if s.packets_lost_send > 0 {
                surfaced = s.packets_lost_send;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            surfaced, 2024,
            "expected RR-derived loss 2024 surfaced as packets_lost_send"
        );

        // RTT stays 0 because no SR anchor preceded the RR.
        let s = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(s.rtt_us, 0);
    }

    /// T30 — verify that SR + RR leave `rtt_us` at 0 after the ingest
    /// thread processes both packets. RTT computation is deferred (see
    /// `docs/project/deferred-features.md`); the previously-asserted
    /// "non-zero rtt_us after SR→RR" was wrong because the anchor NTP
    /// came from the PEER's SR (wrong clock domain for the formula).
    #[test]
    fn sr_then_rr_leaves_rtt_us_zero() {
        use crate::rtcp::{ReceiverReport, ReportBlock, SenderReport};

        let (_data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let (rtcp_tx, rtcp_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let t = RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx);
        let our_ssrc = t.ssrc;

        // Peer SR — establishes the anchor.
        let sr_ntp_full: u64 = 0xDEAD_BEEF_0000_0000u64; // arbitrary
        let sr = SenderReport {
            ssrc: 0xCAFEBABE,
            ntp_timestamp: sr_ntp_full,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        };
        rtcp_tx
            .send(bytes::Bytes::from(sr.encode().unwrap()))
            .expect("send SR");

        // Deterministic wait: the SR must actually be ingested before we
        // proceed — otherwise the final rtt_us == 0 assertion would be
        // vacuously true (0 is also the never-processed default).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while t.rtcp_stats().sr_packets_received == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for SR ingest"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Peer RR — RB's `last_sr` mirrors the anchor's mid-32 NTP.
        let last_sr_mid = ((sr_ntp_full >> 16) & 0xFFFF_FFFF) as u32;
        let rb = ReportBlock {
            ssrc: our_ssrc,
            fraction_lost: 0,
            cumulative_lost: 0,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: last_sr_mid,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0xCAFEBABE,
            report_blocks: vec![rb],
        };
        rtcp_tx
            .send(bytes::Bytes::from(rr.encode().unwrap()))
            .expect("send RR");

        // Same deterministic wait for the RR: only after it is provably
        // ingested does the rtt_us == 0 assertion pin the new behavior.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while t.rtcp_stats().rr_packets_received == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for RR ingest"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let s = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(
            s.rtt_us, 0,
            "rtt_us must stay 0 after SR+RR (RTT computation is deferred); got {}",
            s.rtt_us
        );
    }

    /// T30 — verify that a malformed RTCP packet (PT=201 but truncated)
    /// increments `rtcp_stats().rr_parse_errors` and does NOT crash
    /// or corrupt the other stats.
    #[test]
    fn malformed_rr_increments_parse_error_counter() {
        let (_data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let (rtcp_tx, rtcp_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let t = RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx);

        // 2-byte truncated "RR" — PT byte is 201 so the dispatcher
        // attempts to decode but ReceiverReport::decode rejects on
        // length check.
        let bytes = bytes::Bytes::from(vec![0x80u8, 201]);
        rtcp_tx.send(bytes).expect("send truncated RR");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut parse_errs = 0u64;
        while std::time::Instant::now() < deadline {
            parse_errs = t.rtcp_stats().rr_parse_errors;
            if parse_errs > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(parse_errs, 1);
        // Stats unaffected.
        assert_eq!(t.rtcp_stats().rr_packets_received, 0);
    }

    /// Guard: RtpTransport::connect with port 65535 must not panic (no u16
    /// overflow in RTCP companion port arithmetic). Ok or Err both accepted.
    #[test]
    fn rtp_connect_port_65535_does_not_panic() {
        let _ = RtpTransport::connect("rtp://127.0.0.1:65535");
    }

    /// Guard: RtpRecvTransport::listen with port 65535 must not panic.
    /// The RTCP companion would need port 65536, which is invalid — the
    /// bind must be skipped, not overflowed.
    #[test]
    fn rtp_listen_port_65535_does_not_panic() {
        let _ = RtpRecvTransport::listen("rtp://127.0.0.1:65535");
    }

    // ── ?pt= rejection guards ─────────────────────────────────────────────────

    /// A `?pt=` URL must be rejected by the MP2T `RtpTransport::connect*` path.
    #[test]
    fn mp2t_connect_rejects_pt_param() {
        let url = RtpUrl::parse("rtp://127.0.0.1:5004?pt=96").unwrap();
        let err = RtpTransport::connect_with(&url)
            .map(|_| ())
            .expect_err("connect_with ?pt= must fail");
        assert!(
            matches!(err, ConnectError::PayloadTypeParam),
            "expected PayloadTypeParam, got {err:?}"
        );
    }

    /// A `?pt=` URL must be rejected by the MP2T `RtpRecvTransport::listen*` path.
    #[test]
    fn mp2t_listen_rejects_pt_param() {
        let url = RtpUrl::parse("rtp://127.0.0.1:0?pt=96").unwrap();
        let err = RtpRecvTransport::listen_with(&url)
            .map(|_| ())
            .expect_err("listen_with ?pt= must fail");
        assert!(
            matches!(err, ConnectError::PayloadTypeParam),
            "expected PayloadTypeParam, got {err:?}"
        );
    }

    // --- DA-RTP-5: MP2T payload shape validation ---
    //
    // These tests drive the recv path via the mpsc seam (from_mpsc_placeholder).
    // Since the pump now pushes whole RTP packets, tests feed whole packets
    // (built with make_rtp_packet) rather than raw payload bytes directly.
    // The mpsc path is representative because the UDP path shares
    // is_valid_mp2t_payload and the same RTP header decode logic.

    /// A 100-byte payload that doesn't start with 0x47 must be dropped and
    /// must tick malformed_packets.
    #[test]
    fn mp2t_shape_invalid_non_aligned_drops_and_ticks_counter() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        // Push a whole RTP packet wrapping a 100-byte non-0x47 TS "payload"
        // (not 188-aligned, wrong sync byte) — shape check rejects it.
        let malformed_ts = vec![0xAAu8; 100];
        data_tx
            .send(make_rtp_packet(&malformed_ts))
            .expect("send malformed payload");

        // Push a valid 188-byte TS packet (0x47 sync) so recv_bytes has
        // something to return after discarding the malformed one.
        let mut valid_pkt = vec![0x00u8; 188];
        valid_pkt[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&valid_pkt))
            .expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        // recv_bytes must skip the malformed payload and return the valid one.
        let n = t.recv_bytes(&mut buf).expect("expected valid packet");
        assert_eq!(n, 188, "valid 188-byte packet must be returned");
        assert_eq!(buf[0], 0x47, "returned packet must start with TS sync byte");

        // Counter must be 1 — exactly the malformed payload that was dropped.
        assert_eq!(
            t.rtp_stats().malformed_packets,
            1,
            "malformed_packets must be 1 after one shape-invalid payload"
        );
    }

    /// A valid 188-byte TS payload (starts with 0x47) must pass through
    /// unchanged and must NOT tick malformed_packets.
    #[test]
    fn mp2t_shape_valid_single_packet_passes_through() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        let mut pkt = vec![0xABu8; 188];
        pkt[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&pkt))
            .expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("valid packet must not error");
        assert_eq!(n, 188);
        assert_eq!(&buf[..188], pkt.as_slice());
        assert_eq!(
            t.rtp_stats().malformed_packets,
            0,
            "valid payload must not tick malformed_packets"
        );
    }

    /// A multi-packet 188*7=1316 byte bundle (all starting with 0x47 at
    /// offset 0) must pass through — RFC 2250 allows up to 7 TS packets
    /// per RTP datagram.
    #[test]
    fn mp2t_shape_valid_bundle_passes_through() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        // 7-packet bundle: only the FIRST byte must be 0x47 for the shape check.
        let mut bundle = vec![0x00u8; 188 * 7];
        bundle[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&bundle))
            .expect("send valid bundle");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("valid bundle must not error");
        assert_eq!(n, 188 * 7);
        assert_eq!(t.rtp_stats().malformed_packets, 0);
    }

    /// An empty TS payload (inside a valid RTP header) must be dropped and
    /// tick malformed_packets.
    #[test]
    fn mp2t_shape_empty_payload_drops_and_ticks_counter() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        // Empty TS payload wrapped in a valid RTP header.
        data_tx
            .send(make_rtp_packet(&[]))
            .expect("send empty payload");

        // Follow with a valid packet so recv_bytes can return.
        let mut valid_pkt = vec![0u8; 188];
        valid_pkt[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&valid_pkt))
            .expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("must return valid packet");
        assert_eq!(n, 188);
        assert_eq!(
            t.rtp_stats().malformed_packets,
            1,
            "empty payload must tick malformed_packets"
        );
    }

    /// Wire-level counter semantics on the mpsc path: `bytes_received` and
    /// `packets_received` must increment before MP2T-shape validation, so a
    /// malformed payload still advances the counters (same as the UDP path,
    /// which increments on `Ok(n)` before any header or shape check).
    ///
    /// We send one malformed (100-byte, wrong sync, wrapped in RTP) + one
    /// valid (188-byte, 0x47 sync, wrapped in RTP). After recv_bytes returns
    /// the valid one we expect packets_received == 2 and bytes_received to
    /// sum both whole RTP packets.
    #[test]
    fn mpsc_bytes_packets_received_counted_at_wire_level() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        // Malformed TS payload: not 188-aligned, wrong sync byte.
        let malformed_ts = vec![0xAAu8; 100];
        let malformed_pkt = make_rtp_packet(&malformed_ts);
        let malformed_len = malformed_pkt.len(); // RTP_HEADER_LEN + 100

        // Valid: exactly one 188-byte TS packet.
        let mut valid_ts = vec![0x00u8; 188];
        valid_ts[0] = 0x47;
        let valid_pkt = make_rtp_packet(&valid_ts);
        let valid_len = valid_pkt.len(); // RTP_HEADER_LEN + 188

        data_tx.send(malformed_pkt).expect("send malformed payload");
        data_tx.send(valid_pkt).expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("valid packet must not error");
        assert_eq!(n, 188);

        let s = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(
            s.packets_received, 2,
            "both malformed and valid packets must be counted at wire-level"
        );
        assert_eq!(
            s.bytes_received,
            (malformed_len + valid_len) as u64,
            "bytes_received must sum both whole RTP packets"
        );
        assert_eq!(
            t.rtp_stats().malformed_packets,
            1,
            "malformed_packets must be 1 for the one dropped payload"
        );
    }

    /// Regression: full-MTU RTP truncation (pre-existing bug shipped in
    /// v0.2.0). A conformant peer sending a full 7×188 MP2T bundle emits a
    /// 1328-byte datagram (12-byte RTP header + 1316-byte payload) — larger
    /// than the old `pkt_size`-sized (1316 B) recv scratch, so
    /// `UdpSocket::recv` silently truncated it: corrupt delivery in v0.2.0,
    /// silent drop once the DA-RTP-5 shape guard landed. The scratch is now
    /// sized to `RECV_SCRATCH_LEN` (the UDP datagram ceiling), so the whole
    /// payload must arrive intact with `malformed_packets == 0`.
    ///
    /// Pre-fix this test HANGS rather than failing cleanly: the truncated
    /// payload fails the shape guard, is dropped, and `recv_bytes` blocks
    /// waiting for a next packet that never comes (harness timeout catches
    /// a regression).
    #[test]
    fn udp_full_mtu_datagram_delivered_whole() {
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        let recv_addr = recv_sock.local_addr().expect("recv local addr");
        let mut t = RtpRecvTransport::from_udp_socket(recv_sock).expect("wrap recv socket");

        // 7×188 = 1316-byte TS bundle; every packet 0x47-led.
        let mut bundle = vec![0u8; 188 * 7];
        for chunk in bundle.chunks_mut(188) {
            chunk[0] = 0x47;
        }
        let pkt = make_rtp_packet(&bundle);
        assert_eq!(pkt.len(), 1328, "whole datagram must be 1328 bytes");

        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.send_to(&pkt, recv_addr).expect("send datagram");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("full-MTU datagram must be delivered");
        assert_eq!(n, 188 * 7, "whole 1316-byte payload must be returned");
        assert_eq!(&buf[..n], bundle.as_slice(), "payload must be untruncated");
        assert_eq!(
            t.rtp_stats().malformed_packets,
            0,
            "conformant full-MTU datagram must not be counted malformed"
        );
    }

    /// Regression twin with a CSRC-bearing header: CC=15 adds 60 bytes of
    /// CSRC list, so the whole datagram is 12 + 60 + 1316 = 1388 bytes —
    /// larger than even `pkt_size + RTP_HEADER_LEN`. Conformant per
    /// RFC 3550 §5.1; must be delivered whole.
    ///
    /// Pre-fix this test HANGS rather than failing cleanly (truncated →
    /// shape-guard drop → recv blocks on a packet that never comes);
    /// the harness timeout catches a regression.
    #[test]
    fn udp_csrc_bearing_oversize_datagram_delivered_whole() {
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        let recv_addr = recv_sock.local_addr().expect("recv local addr");
        let mut t = RtpRecvTransport::from_udp_socket(recv_sock).expect("wrap recv socket");

        let mut bundle = vec![0u8; 188 * 7];
        for chunk in bundle.chunks_mut(188) {
            chunk[0] = 0x47;
        }

        // V=2 | P=0 | X=0 | CC=15 → 0x8F; M=0 | PT=33.
        let mut pkt = vec![0x8Fu8, RTP_PT_MP2T];
        pkt.extend_from_slice(&[0u8; 10]); // seq/ts/ssrc
        pkt.extend_from_slice(&[0u8; 60]); // 15 CSRC entries × 4 bytes
        pkt.extend_from_slice(&bundle);
        assert_eq!(pkt.len(), 1388, "whole datagram must be 1388 bytes");

        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.send_to(&pkt, recv_addr).expect("send datagram");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("CSRC-bearing datagram must be delivered");
        assert_eq!(n, 188 * 7, "payload after CSRC strip must be 1316 bytes");
        assert_eq!(&buf[..n], bundle.as_slice(), "payload must be untruncated");
        assert_eq!(t.rtp_stats().malformed_packets, 0);
    }

    /// Mpsc twin of the full-MTU pin: a whole 1328-byte interleaved frame
    /// (12-byte header + 7×188 payload) must round-trip through recv_raw's
    /// scratch copy without truncation or drop.
    #[test]
    fn mpsc_full_frame_1328_delivered_whole() {
        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        let mut bundle = vec![0u8; 188 * 7];
        for chunk in bundle.chunks_mut(188) {
            chunk[0] = 0x47;
        }
        let pkt = make_rtp_packet(&bundle);
        assert_eq!(pkt.len(), 1328, "whole frame must be 1328 bytes");
        data_tx.send(pkt).expect("send frame");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("1328-byte frame must be delivered");
        assert_eq!(n, 188 * 7);
        assert_eq!(&buf[..n], bundle.as_slice());
        assert_eq!(t.rtp_stats().malformed_packets, 0);
    }

    /// B4 / T1-RTSP-RTP (transport level) — a whole RTP packet with CSRC list
    /// (CC>0), header extension (X=1), and trailing padding (P=1) arriving on
    /// the mpsc path must have ONLY the true TS payload reach the caller —
    /// CSRC words and extension skipped, padding trimmed. The pump delivers
    /// the whole packet; stripping is the transport's responsibility.
    #[test]
    fn interleaved_rtp_csrc_extension_padding_stripped_at_recv_site() {
        use crate::packet::RTP_PT_MP2T;
        use tst_core::transport::RecvTransport;

        // Build a whole RTP packet with CC=1, X=1, P=1, PT=33.
        //   Octet 0: V=2 | P=1 | X=1 | CC=1 = 0b10_1_1_0001 = 0xB1
        //   Octet 1: M=0 | PT=33
        //   Octets 2..12: seq/ts/ssrc (all zero)
        //   Octets 12..16: 1 CSRC entry (4 bytes)
        //   Octets 16..20: extension header (profile=0xBEDE, len=1 word)
        //   Octets 20..24: 1 word (4 bytes) of extension data
        //   Octets 24..212: 188-byte TS payload (0x47 sync + fill)
        //   Octets 212..214: 2 padding bytes (last byte = pad count = 2)
        let mut ts_payload = vec![0xABu8; 188];
        ts_payload[0] = 0x47;

        let mut rtp = vec![0xB1u8, RTP_PT_MP2T];
        rtp.extend_from_slice(&[0u8; 10]); // seq/ts/ssrc
        rtp.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // CSRC[0]
        rtp.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]); // ext header, len=1 word
        rtp.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // ext data word
        rtp.extend_from_slice(&ts_payload); // 188-byte TS payload
        rtp.extend_from_slice(&[0x00, 0x02]); // 2 padding bytes (count=2)

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_mpsc_placeholder(data_rx);

        data_tx
            .send(bytes::Bytes::from(rtp))
            .expect("send whole RTP packet");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("valid MP2T payload must not error");
        assert_eq!(
            n, 188,
            "only the 188-byte TS payload must be returned (CSRC+ext skipped, padding trimmed)"
        );
        assert_eq!(
            &buf[..188],
            ts_payload.as_slice(),
            "returned bytes must be the exact TS payload"
        );
        assert_eq!(
            t.rtp_stats().malformed_packets,
            0,
            "well-formed packet must not tick malformed_packets"
        );
    }

    /// is_valid_mp2t_payload covers the helper directly.
    #[test]
    fn is_valid_mp2t_payload_covers_edge_cases() {
        // Empty — invalid.
        assert!(!is_valid_mp2t_payload(&[]));
        // 100 bytes, 0x47 start — not 188-aligned, invalid.
        let mut v = vec![0x47u8; 100];
        assert!(!is_valid_mp2t_payload(&v));
        // 188 bytes, wrong sync — invalid.
        v = vec![0xAAu8; 188];
        assert!(!is_valid_mp2t_payload(&v));
        // 188 bytes, correct sync — valid.
        v = vec![0u8; 188];
        v[0] = 0x47;
        assert!(is_valid_mp2t_payload(&v));
        // 376 bytes (2 packets), correct sync — valid.
        v = vec![0u8; 376];
        v[0] = 0x47;
        assert!(is_valid_mp2t_payload(&v));
        // 189 bytes — not 188-aligned, invalid.
        v = vec![0x47u8; 189];
        assert!(!is_valid_mp2t_payload(&v));
    }

    // ── StreamEndReason: plain rtp:// transport (no owning RtspClient) ──

    /// A fresh, never-touched transport reports `None` — the session
    /// hasn't ended.
    #[test]
    fn end_reason_none_on_fresh_transport() {
        let t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        assert!(t.end_reason().is_none());
        assert!(t.end_reason_handle().get().is_none());
    }

    /// `RecvTransport::close()` is the ONLY writer for a plain `rtp://`
    /// transport (no pump, no keepalive) — it must record `Cancelled`.
    #[test]
    fn close_records_cancelled_on_plain_udp_transport() {
        let mut t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        RecvTransport::close(&mut t);
        assert!(
            matches!(t.end_reason(), Some(StreamEndReason::Cancelled)),
            "close() must record Cancelled, got {:?}",
            t.end_reason()
        );
    }

    /// Firing the transport's own cancel handle (without calling
    /// `close()`) must be observed on the NEXT `recv_bytes` call — it
    /// records `Cancelled` and returns `ExplicitClose`, and the recorded
    /// reason is visible through a handle obtained BEFORE the fire (the
    /// cross-thread-watchdog shape).
    #[test]
    fn cancel_handle_fire_records_cancelled_on_next_recv() {
        let mut t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        let handle = t.end_reason_handle();
        assert!(handle.get().is_none());

        let cancel = t
            .cancel_handle()
            .expect("recv transport exposes a cancel handle");
        cancel.cancel();

        let mut buf = vec![0u8; 2048];
        let result = RecvTransport::recv_bytes(&mut t, &mut buf);
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "expected ExplicitClose, got {result:?}"
        );
        assert!(
            matches!(t.end_reason(), Some(StreamEndReason::Cancelled)),
            "a cancel-handle fire observed at recv must record Cancelled, got {:?}",
            t.end_reason()
        );
        assert!(
            matches!(handle.get(), Some(StreamEndReason::Cancelled)),
            "a handle obtained before the fire must observe the same recording"
        );
    }
}
