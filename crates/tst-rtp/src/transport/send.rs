//! The sending half: [`RtpTransport`].

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;

use tst_core::cancel::CANCEL_POLL_INTERVAL;
use tst_core::net::udp_socket::{apply_multicast_send_knobs, set_socket_buffers};
use tst_core::transport::{BrokenCause, SocketStats, Transport, TransportCancel, TransportError};

use super::{RtpConnectError, random_u32, udp_err_to_connect};
use crate::cancel::RtpCancelHandle;
use crate::clock::RtpClock;
use crate::packet::{RTP_HEADER_LEN, RtpHeader};
use crate::url::{DEFAULT_PKT_SIZE, RtpUrl, RtpUrlError};

/// RTP send-side transport: writes 12-byte RTP header + TS payload to a
/// connected [`UdpSocket`].
///
/// # `SocketStats` field mapping
///
/// | [`SocketStats`] field | Source |
/// |---|---|
/// | `bytes_sent` | Local counter, ticks per successful `send_bytes` |
/// | `packets_sent` | Local counter, ticks per RTP packet |
/// | `bytes_received` / `packets_received` | 0 (this is the send half) |
/// | `rtt_us` | 0 — RTCP RR/SR ingestion is not implemented on the send side |
/// | `packets_lost_send` | 0 — same; would come from RTCP RR fraction-lost |
/// | `link_bandwidth_bps` | 0 — RTP has no link estimate |
/// | All other fields | 0 |
///
/// # `TransportError::Broken` `errno_code` mapping
///
/// For send-side `Broken`, `errno_code` carries the OS `errno` from the
/// underlying `sendto` call (`EAGAIN`=11, `EHOSTUNREACH`=113,
/// `ECONNREFUSED`=111 on Linux). `Backpressure` is not produced — UDP
/// either accepts the datagram or surfaces an error.
pub struct RtpTransport {
    pub(super) socket: Option<UdpSocket>,
    /// Max UDP datagram budget (RTP header + TS bundle), set from
    /// `RtpUrl::pkt_size` (default 1316). `send_bytes` rejects payloads
    /// where `len + RTP_HEADER_LEN` would exceed this, so the
    /// caller-visible [`Transport::max_payload`] is `pkt_size - 12`.
    max_payload: usize,
    clock: RtpClock,
    pub(super) ssrc: u32,
    next_seq: u16,
    cancel: Arc<RtpCancelHandle>,
    /// Local stats — bytes_sent / packets_sent only; the RTCP-derived
    /// fields stay zero (see the `SocketStats` table above).
    bytes_sent: u64,
    packets_sent: u64,
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
    pub fn connect(url: &str) -> Result<Self, RtpConnectError> {
        let parsed = RtpUrl::parse(url).map_err(RtpConnectError::Url)?;
        Self::connect_with(&parsed)
    }

    /// Connect using an already-parsed URL — convenient for callers that
    /// hold an `RtpUrl` (e.g., binding crates).
    pub fn connect_with(url: &RtpUrl) -> Result<Self, RtpConnectError> {
        if url.pt.is_some() {
            return Err(RtpConnectError::PayloadTypeParam);
        }
        if url.rcvbuf.is_some() {
            return Err(RtpConnectError::Url(RtpUrlError::SendRcvbuf));
        }
        if url.source.is_some() {
            return Err(RtpConnectError::Url(RtpUrlError::SendSource));
        }
        let ip: IpAddr = url.host.parse().map_err(|e: std::net::AddrParseError| {
            RtpConnectError::HostNotLiteral {
                host: "<redacted>".to_string(),
                detail: e.to_string(),
            }
        })?;
        let peer = SocketAddr::new(ip, url.port);
        let local: SocketAddr = match ip {
            IpAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
            IpAddr::V6(_) => "[::]:0".parse().unwrap(),
        };
        let socket = UdpSocket::bind(local).map_err(RtpConnectError::Io)?;
        set_socket_buffers(&socket, None, url.sndbuf).map_err(RtpConnectError::Io)?;
        socket
            .set_write_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(RtpConnectError::Io)?;
        // Multicast knobs (no-op for unicast).
        let is_multicast = match ip {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        };
        if is_multicast {
            apply_multicast_send_knobs(&socket, ip, url.ttl, url.iface.as_deref())
                .map_err(udp_err_to_connect)?;
        }
        socket.connect(peer).map_err(RtpConnectError::Io)?;
        Ok(Self::from_socket(socket, url))
    }

    /// Internal: build from an already-configured socket.
    fn from_socket(socket: UdpSocket, url: &RtpUrl) -> Self {
        let pkt_size = url.pkt_size.unwrap_or(DEFAULT_PKT_SIZE);
        let ssrc = url.ssrc.unwrap_or_else(random_u32);
        let next_seq = random_u32() as u16;
        let start_ticks = random_u32();
        Self {
            socket: Some(socket),
            max_payload: pkt_size,
            clock: RtpClock::new(start_ticks),
            ssrc,
            next_seq,
            cancel: RtpCancelHandle::new(),
            bytes_sent: 0,
            packets_sent: 0,
            send_scratch: Vec::with_capacity(RTP_HEADER_LEN + pkt_size),
        }
    }
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
        // Latches on cancel (normative table): a fired handle is a dead
        // transport whether or not an op has observed it yet.
        self.socket.is_some() && !self.cancel.is_cancelled()
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
