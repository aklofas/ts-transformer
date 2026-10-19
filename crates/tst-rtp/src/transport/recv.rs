//! The receiving half: [`RtpRecvTransport`] and the byte source behind it.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tst_core::cancel::CANCEL_POLL_INTERVAL;
use tst_core::net::udp_socket::{
    apply_multicast_recv_join, bind_udp_socket_multicast, set_socket_buffers,
};
use tst_core::transport::{
    BrokenCause, EndReason, EndReasonHandle, RecvTransport, SocketStats, TransportCancel,
    TransportError,
};

use super::rtcp_ingest::spawn_rtcp_ingest;
use super::{RtpConnectError, random_u32, udp_err_to_connect};
use crate::cancel::RtpCancelHandle;
use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T, RtpHeader};
use crate::rtcp::stats::RtcpStats;
use crate::url::{RtpUrl, RtpUrlError};

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
/// `Udp` — the default: read UDP datagrams off a bound socket
/// and strip the RTP header in `recv_bytes`.
///
/// `Mpsc` — the TCP-interleaved bridge:
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
    /// `source_pin` is `Some((ip, rejected))` when the receiver was built
    /// with `?source=`: the UDP arm then reads with `recv_from`, drops any
    /// datagram whose canonical peer IP differs from `ip` (the port is not
    /// compared), ticks `*rejected`, and keeps looping with the same cancel
    /// and deadline checks. `None` keeps the UDP arm on plain `recv`. The
    /// mpsc arm ignores it (interleaved frames have no per-datagram peer).
    ///
    /// Apart from `*rejected`, `recv_raw` takes `&self` and performs no
    /// side effects on the source or any counters. Callers are responsible for:
    /// - Ticking `bytes_received` / `packets_received` after `Ok`.
    /// - Clearing `self.source = None` after a `Broken` return.
    pub(crate) fn recv_raw(
        &self,
        scratch: &mut [u8],
        cancel: &RtpCancelHandle,
        deadline: Option<Instant>,
        mut source_pin: Option<(IpAddr, &mut u64)>,
    ) -> Result<usize, TransportError> {
        match self {
            Source::Udp(socket) => loop {
                if cancel.is_cancelled() {
                    return Err(TransportError::ExplicitClose);
                }
                if let Some(d) = deadline
                    && Instant::now() >= d
                {
                    return Err(TransportError::Backpressure {
                        msg: "recv deadline elapsed".to_string(),
                        errno_code: None,
                    });
                }
                let received = match source_pin.as_mut() {
                    None => socket.recv(scratch),
                    Some((pin, rejected)) => match socket.recv_from(scratch) {
                        Ok((_, peer)) if peer.ip().to_canonical() != *pin => {
                            **rejected = rejected.saturating_add(1);
                            continue;
                        }
                        Ok((n, _)) => Ok(n),
                        Err(e) => Err(e),
                    },
                };
                match received {
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
                if let Some(d) = deadline
                    && Instant::now() >= d
                {
                    return Err(TransportError::Backpressure {
                        msg: "recv deadline elapsed".to_string(),
                        errno_code: None,
                    });
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
    pub(super) source: Option<Source>,
    cancel: Arc<RtpCancelHandle>,
    bytes_received: u64,
    packets_received: u64,
    /// Counter for RTP packets that failed the header check.
    malformed_packets: u64,
    /// `?source=` pin, canonicalised (an IPv4-mapped IPv6 address compares
    /// as its IPv4 form). `None` keeps the receive loop on plain `recv`.
    source_pin: Option<IpAddr>,
    /// Datagrams dropped because their peer IP did not match `source_pin`.
    source_rejected: u64,
    /// Per-recv scratch, sized to [`RECV_SCRATCH_LEN`] — heap allocated
    /// once; holds one whole RTP packet (header + payload) per recv.
    scratch: Vec<u8>,
    /// RTCP-derived counters. Filled by the interleaved-RTSP ingest thread
    /// ([`Self::from_mpsc_with_rtcp`]); stay at their defaults on a plain
    /// `rtp://` receiver, which ingests no RTCP.
    rtcp_stats: Arc<Mutex<RtcpStats>>,
    /// Local SSRC: random at bind time unless the URL sets `?ssrc=`. The
    /// interleaved-RTSP ingest thread matches a peer's report blocks
    /// against it ([`Self::from_mpsc_with_rtcp`]).
    pub(super) ssrc: u32,
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
    end_reason: EndReasonHandle,
}

tst_core::binding_record! {
    /// RTP protocol-level statistics snapshot, separate from the socket stats.
    ///
    /// Exposes the malformed-packet and source-rejected counters.
    ///
    /// <!-- rust-only -->
    /// Future fields (out-of-order delta, gap counter, etc.) can be added under
    /// `#[non_exhaustive]` without breaking consumers.
    #[must_use]
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct RtpStats {
        /// Number of UDP datagrams received whose RTP header was invalid
        /// (wrong V, wrong PT, truncated, CSRC overflow). Cumulative since
        /// the receiver started listening.
        /// Also counts datagrams with a valid RTP header (PT=33) whose
        /// MP2T payload fails RFC 2250 shape checks (not 188-byte aligned,
        /// missing `0x47` sync byte, or empty).
        pub malformed_packets: u64,
        /// Datagrams dropped because their peer IP did not match the
        /// `?source=` pin; 0 when no pin is set. Rejected datagrams are not
        /// counted in the received counters of the socket stats.
        pub source_rejected: u64,
    }
}

impl RtpRecvTransport {
    /// Bind to `url`'s host:port and (for multicast) join the group.
    pub fn listen(url: &str) -> Result<Self, RtpConnectError> {
        let parsed = RtpUrl::parse(url).map_err(RtpConnectError::Url)?;
        Self::listen_with(&parsed)
    }

    /// Bind using an already-parsed URL — convenient for callers that hold
    /// an `RtpUrl` (e.g., binding crates).
    pub fn listen_with(url: &RtpUrl) -> Result<Self, RtpConnectError> {
        if url.pkt_size.is_some() {
            return Err(RtpConnectError::Url(RtpUrlError::RecvPktSize));
        }
        if url.sndbuf.is_some() {
            return Err(RtpConnectError::Url(RtpUrlError::RecvSndbuf));
        }
        if url.pt.is_some() {
            return Err(RtpConnectError::PayloadTypeParam);
        }
        let ip: IpAddr = url.host.parse().map_err(|e: std::net::AddrParseError| {
            RtpConnectError::HostNotLiteral {
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
            bind_udp_socket_multicast(local).map_err(RtpConnectError::Io)?
        } else {
            UdpSocket::bind(local).map_err(RtpConnectError::Io)?
        };
        // `?rcvbuf=` sizes the receive socket.
        set_socket_buffers(&socket, url.rcvbuf, None).map_err(RtpConnectError::Io)?;
        // For the multicast path this re-sets SO_RCVTIMEO to the same
        // value the helper already applied — redundant but harmless.
        socket
            .set_read_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(RtpConnectError::Io)?;
        if is_multicast {
            apply_multicast_recv_join(&socket, ip, url.iface.as_deref())
                .map_err(udp_err_to_connect)?;
        }
        let ssrc = url.ssrc.unwrap_or_else(random_u32);
        Ok(Self {
            source: Some(Source::Udp(socket)),
            cancel: RtpCancelHandle::new(),
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            source_pin: url.source.map(|ip| ip.to_canonical()),
            source_rejected: 0,
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            rtcp_stats: Arc::new(Mutex::new(RtcpStats::default())),
            ssrc,
            recv_timeout: url.recv_timeout,
            end_reason: EndReasonHandle::default(),
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
    pub(crate) fn from_udp_socket(socket: UdpSocket) -> Result<Self, RtpConnectError> {
        socket
            .set_read_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(RtpConnectError::Io)?;
        let ssrc = random_u32();
        Ok(Self {
            source: Some(Source::Udp(socket)),
            cancel: RtpCancelHandle::new(),
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            source_pin: None,
            source_rejected: 0,
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            rtcp_stats: Arc::new(Mutex::new(RtcpStats::default())),
            ssrc,
            recv_timeout: None,
            end_reason: EndReasonHandle::default(),
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
    pub(crate) fn from_packet_channel(rx: std::sync::mpsc::Receiver<bytes::Bytes>) -> Self {
        let ssrc = random_u32();
        Self {
            source: Some(Source::Mpsc(rx)),
            cancel: RtpCancelHandle::new(),
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            source_pin: None,
            source_rejected: 0,
            // Scratch holds one whole RTP packet (header + TS payload).
            // recv_raw (Source::Mpsc arm) copies the incoming Bytes into
            // scratch before recv_bytes decodes the header and strips the
            // payload — same buffer + ceiling as the UDP arm.
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            rtcp_stats: Arc::new(Mutex::new(RtcpStats::default())),
            ssrc,
            recv_timeout: None,
            end_reason: EndReasonHandle::default(),
        }
    }

    /// Replace the cancel handle (publisher mounts share one handle between
    /// the `PublishMountHandle` and the transport it hands out). Only
    /// consumed under `rtsp-server` (the only caller is
    /// `PublishMountHandle::into_recv_transport`); gated to match so a
    /// `--no-default-features` build has no orphaned method.
    #[cfg(feature = "rtsp-server")]
    pub(crate) fn with_cancel_handle(mut self, cancel: Arc<RtpCancelHandle>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Variant of [`Self::from_packet_channel`] that also spawns a
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
        let t = Self::from_packet_channel(data_rx);
        spawn_rtcp_ingest(rtcp_rx, t.rtcp_stats.clone(), t.ssrc);
        t
    }

    /// RTP-protocol-level stats — separate from [`SocketStats`].
    pub fn rtp_stats(&self) -> RtpStats {
        RtpStats {
            malformed_packets: self.malformed_packets,
            source_rejected: self.source_rejected,
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

    /// The recorded [`EndReason`], or `None` if the session hasn't
    /// ended yet (or ended through a path that records no reason —
    /// see the field doc on `end_reason` for the plain-`rtp://` case).
    pub fn end_reason(&self) -> Option<EndReason> {
        self.end_reason.get()
    }

    /// A cloneable, cross-thread-safe handle onto this transport's end
    /// reason — for a watchdog thread that wants to poll
    /// [`EndReasonHandle::get`] without holding a reference to the
    /// transport itself (which a single-owner `recv_bytes` loop is
    /// using).
    pub fn end_reason_handle(&self) -> EndReasonHandle {
        self.end_reason.clone()
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
    pub(crate) fn set_end_reason_slot(&mut self, slot: EndReasonHandle) {
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
    ///
    /// An empty `buf` returns `Ok(Some(0))` immediately.
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
        // An empty destination is a no-op — return before the
        // liveness check and before touching the source, so it can neither
        // park nor be reported as "buf too small" (which latches dead).
        if buf.is_empty() {
            return Ok(0);
        }
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
                .recv_raw(
                    &mut self.scratch,
                    &self.cancel,
                    deadline,
                    self.source_pin.map(|ip| (ip, &mut self.source_rejected)),
                );
            let n = match raw_result {
                Ok(n) => n,
                Err(TransportError::Broken { ref msg, .. })
                    if msg == MPSC_PUMP_DISCONNECTED
                        && matches!(self.end_reason.get(), Some(EndReason::CleanTeardown)) =>
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
                    // dead then propagate.
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
                    record_end(&self.end_reason, EndReason::Cancelled);
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            // Count at wire-level, before the RTP-header and MP2T-shape
            // checks. Malformed-but-received packets are counted
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
            // RFC 2250 shape guard — payload must be non-empty,
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

/// The reasons an RTP or RTSP receive stream can end with (see
/// `tst_core::transport::EndReason`'s table). Every recording site in this
/// crate goes through [`record_end`].
pub(crate) fn rtp_receiver_can_end_with(r: &EndReason) -> bool {
    matches!(
        r,
        EndReason::CleanTeardown
            | EndReason::SessionExpired
            | EndReason::KeepaliveFailed { .. }
            | EndReason::TransportFailed { .. }
            | EndReason::ProtocolError { .. }
            | EndReason::Cancelled
    )
}

/// Record why an RTP/RTSP receive stream ended (first writer wins).
///
/// A plain `rtp://` receiver reaches only `Cancelled` of the permitted
/// set, because its only recording site is the cancel path; the other
/// reasons come from the RTSP client sharing its handle with the receiver.
pub(crate) fn record_end(h: &EndReasonHandle, r: EndReason) {
    debug_assert!(
        rtp_receiver_can_end_with(&r),
        "an RTP receiver cannot end with {r:?}"
    );
    h.record(r);
}

impl RecvTransport for RtpRecvTransport {
    /// Receive the next valid MP2T bundle from the RTP stream.
    ///
    /// # MP2T shape enforcement
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
        // Latches on cancel — see `RtpTransport::is_alive`.
        self.source.is_some() && !self.cancel.is_cancelled()
    }

    fn close(&mut self) {
        self.source = None;
        record_end(&self.end_reason, EndReason::Cancelled);
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
        // Paths without an ingest thread (UDP, `from_packet_channel`)
        // leave these at 0.
        if let Ok(rtcp) = self.rtcp_stats.lock() {
            s.rtt_us = rtcp.rtt_us;
            s.packets_lost_send = rtcp.cumulative_lost_send as u64;
        }
        Some(s)
    }

    fn end_reason_handle(&self) -> Option<EndReasonHandle> {
        Some(RtpRecvTransport::end_reason_handle(self))
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
pub(crate) fn is_valid_mp2t_payload(payload: &[u8]) -> bool {
    !payload.is_empty() && payload.len().is_multiple_of(188) && payload[0] == 0x47
}
