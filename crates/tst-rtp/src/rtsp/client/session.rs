//! `RtspSession` is the post-SETUP state held by `RtspClient::setup`'s
//! return value. Calling `into_recv_transport` converts it into a
//! `RtpRecvTransport` ready for `DemuxReceiver::new`.
//!
//! Two internal variants:
//!
//! - UDP-backed: holds the already-bound `UdpSocket` pair from SETUP.
//! - TCP-interleaved-backed: holds the consumer side of the mpsc
//!   channel fed by `RtspClient`'s pump thread (spawned in SETUP via
//!   the crate-private `RtspClient::activate_interleaved_pump`).

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

use bytes::Bytes;

use crate::h264::depacketizer::H264DepayConfig;
use crate::h264::receiver::H264Receiver;
use crate::rtsp::client::end_reason::EndReasonSlot;
use crate::rtsp::client::transport_negotiation::{RtspTransportKind, TransportResponse};
use crate::transport::RtpRecvTransport;

/// State held between SETUP and PLAY / TEARDOWN.
///
/// # Send
///
/// This type is `Send`: moving it to a dedicated receive/watchdog thread
/// is a supported, documented use — a regression here is a breaking
/// change.
pub struct RtspSession {
    pub(crate) session_id: String,
    pub(crate) transport: TransportResponse,
    pub(crate) kind: RtspTransportKind,
    pub(crate) udp_sockets: Option<(UdpSocket, UdpSocket)>,
    pub(crate) peer_addr: Option<SocketAddr>,
    /// For TcpInterleaved: consumer side of the pump's data channel.
    /// `None` for UDP sessions.
    pub(crate) data_rx: Option<mpsc::Receiver<Bytes>>,
    /// For TcpInterleaved: consumer side of the pump's RTCP channel.
    /// `None` for UDP sessions (UDP carries RTCP on its own dedicated
    /// socket pair). The pump's RTCP demux feeds this
    /// `mpsc::Receiver<Bytes>`; [`Self::into_recv_transport`] consumes
    /// it via `RtpRecvTransport::from_mpsc_with_rtcp`, which spawns the
    /// ingest thread that feeds each frame into `RtcpStats` for
    /// receiver-side stats.
    pub(crate) rtcp_rx: Option<mpsc::Receiver<Bytes>>,
    /// The `?recv_timeout=` value parsed from the `RtspUrl` at connect
    /// time (see `RtspClient::attempt_setup`). Applied to the transport
    /// / receiver built by [`Self::into_recv_transport`] /
    /// [`Self::into_h264_receiver`] via their `set_recv_timeout` setters
    /// — the session itself never blocks on I/O, so there is nothing to
    /// apply the deadline to before conversion.
    pub(crate) recv_timeout: Option<Duration>,
    /// Clone of the owning [`crate::rtsp::client::RtspClient`]'s
    /// [`EndReasonSlot`] — carried forward into
    /// [`Self::into_recv_transport`] / [`Self::into_h264_receiver`] so
    /// the resulting transport/receiver reports the SAME
    /// [`crate::StreamEndReason`] the RTSP client's pump / keepalive
    /// threads record, not a fresh (always-empty) slot.
    pub(crate) end_reason: EndReasonSlot,
}

impl RtspSession {
    pub(crate) fn new_udp(
        sid: String,
        rtp: UdpSocket,
        rtcp: UdpSocket,
        transport: TransportResponse,
        peer: SocketAddr,
        recv_timeout: Option<Duration>,
        end_reason: EndReasonSlot,
    ) -> Self {
        Self {
            session_id: sid,
            kind: RtspTransportKind::Udp,
            transport,
            udp_sockets: Some((rtp, rtcp)),
            peer_addr: Some(peer),
            data_rx: None,
            rtcp_rx: None,
            recv_timeout,
            end_reason,
        }
    }

    /// Construct a TCP-interleaved session, carrying both the pump's
    /// data `mpsc::Receiver<Bytes>` and the pump's RTCP
    /// `mpsc::Receiver<Bytes>`; [`Self::into_recv_transport`] hands both
    /// to `RtpRecvTransport::from_mpsc_with_rtcp` for the consumer side.
    pub(crate) fn new_interleaved_with_data_rx(
        sid: String,
        transport: TransportResponse,
        data_rx: mpsc::Receiver<Bytes>,
        rtcp_rx: mpsc::Receiver<Bytes>,
        recv_timeout: Option<Duration>,
        end_reason: EndReasonSlot,
    ) -> Self {
        Self {
            session_id: sid,
            kind: RtspTransportKind::TcpInterleaved,
            transport,
            udp_sockets: None,
            peer_addr: None,
            data_rx: Some(data_rx),
            rtcp_rx: Some(rtcp_rx),
            recv_timeout,
            end_reason,
        }
    }

    /// Session ID returned by the server at SETUP. Echoed back in PLAY /
    /// PAUSE / TEARDOWN under the `Session:` header.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Which transport flavor the server picked at SETUP.
    pub fn transport_kind(&self) -> RtspTransportKind {
        self.kind
    }

    /// Peer's RTCP endpoint (`peer.ip():server_rtcp_port`) for UDP
    /// sessions, `None` for TCP-interleaved (the RTCP carrier is the
    /// control TCP itself in that case).
    pub fn rtcp_endpoint(&self) -> Option<SocketAddr> {
        let peer = self.peer_addr?;
        let port = self.transport.server_port.map(|(_, hi)| hi)?;
        Some(SocketAddr::new(peer.ip(), port))
    }

    /// Consume the session and return an `RtpRecvTransport` ready for
    /// `DemuxReceiver`.
    ///
    /// For UDP: wraps the SETUP-allocated UDP socket pair into a
    /// pre-built `RtpRecvTransport` (avoiding a second `listen()` call).
    ///
    /// For TCP-interleaved: returns an `RtpRecvTransport` fed by the
    /// `mpsc::Receiver<Bytes>` populated by `RtspClient`'s
    /// interleaved-pump thread (spawned at SETUP time by the
    /// crate-private `RtspClient::activate_interleaved_pump`). The
    /// pump's RTCP `mpsc::Receiver<Bytes>` is also handed in and drives
    /// the new `rtsp-rtcp-ingest` thread inside the transport — that
    /// thread populates `socket_stats().rtt_us` +
    /// `socket_stats().packets_lost_send` from RR + SR frames arriving
    /// on RFC 7826 §14 channel 1.
    pub fn into_recv_transport(mut self) -> RtpRecvTransport {
        let mut transport = match self.kind {
            RtspTransportKind::Udp => {
                let (rtp, _rtcp) = self.udp_sockets.expect("UDP session has sockets");
                RtpRecvTransport::from_udp_socket(rtp).expect("from_udp_socket")
            }
            RtspTransportKind::TcpInterleaved => {
                let data_rx = self
                    .data_rx
                    .take()
                    .expect("TcpInterleaved session has a pump data_rx");
                let rtcp_rx = self
                    .rtcp_rx
                    .take()
                    .expect("TcpInterleaved session has a pump rtcp_rx");
                RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx)
            }
        };
        transport.set_recv_timeout(self.recv_timeout);
        transport.set_end_reason_slot(self.end_reason.clone());
        transport
    }

    /// Consume the session and return an [`H264Receiver`] wired to the
    /// transport negotiated at SETUP time.
    ///
    /// For UDP: takes the RTP socket from the SETUP-allocated UDP pair;
    /// the companion RTCP socket is dropped (RTCP is not implemented on
    /// the H.264 path — a recorded deferral; see `docs/project/deferred-features.md`).
    ///
    /// For TCP-interleaved: takes both the pump's data channel and its RTCP
    /// channel. The RTCP channel is kept alive inside the receiver and drained
    /// (discard-only) on each `recv_au` iteration so the pump's
    /// `rtcp_tx.try_send()` never sees `Disconnected` — which would otherwise
    /// kill the session at the first server RTCP Sender Report.
    ///
    /// Use the `config` returned by
    /// [`RtspClient::setup_h264_auto`](crate::rtsp::client::RtspClient::setup_h264_auto)
    /// to carry the negotiated payload type and out-of-band SPS/PPS NALUs
    /// into the receiver.
    ///
    /// # Call order
    ///
    /// The full sequence is `connect` → `describe` → `setup_h264_auto` →
    /// [`RtspClient::play`](crate::rtsp::client::RtspClient::play) →
    /// `into_h264_receiver`. Issue PLAY (on the still-live `RtspClient`)
    /// before converting the session: the server does not start pushing
    /// RTP data until it sees PLAY, so calling this first just leaves the
    /// returned [`H264Receiver::recv_au`] blocked waiting for AUs that
    /// never arrive until PLAY runs elsewhere. `RtspClient` and
    /// `RtspSession` are independent values — nothing here enforces the
    /// order at compile time or runtime, so this is a documented usage
    /// contract, not a checked one.
    pub fn into_h264_receiver(mut self, config: H264DepayConfig) -> H264Receiver {
        let mut receiver = match self.kind {
            RtspTransportKind::Udp => {
                let (rtp, _rtcp) = self.udp_sockets.expect("UDP session has sockets");
                H264Receiver::from_udp_socket_with(rtp, config).expect("from_udp_socket_with")
            }
            RtspTransportKind::TcpInterleaved => {
                let data_rx = self
                    .data_rx
                    .take()
                    .expect("TcpInterleaved session has a pump data_rx");
                let rtcp_rx = self.rtcp_rx.take();
                // RTCP frames are drained-and-discarded inside H264Receiver
                // (no RTCP processing on the H.264 path — v1 decision; see
                // `docs/project/deferred-features.md`). We pass `rtcp_rx`
                // so the receiver holds the consumer end: if we dropped it
                // here the pump's `rtcp_tx.try_send()` would return
                // `Disconnected` at the first server RTCP Sender Report,
                // causing the pump to exit and triggering a false clean-EOS.
                H264Receiver::from_mpsc_with_rtcp_drain(data_rx, rtcp_rx, config)
            }
        };
        receiver.set_recv_timeout(self.recv_timeout);
        receiver.set_end_reason_slot(self.end_reason.clone());
        receiver
    }
}
