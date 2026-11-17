//! `RtspSession` is the post-SETUP state held by `RtspClient::setup`'s
//! return value. Calling `into_recv_transport` converts it into a
//! `RtpRecvTransport` ready for `DemuxReceiver::new`.
//!
//! Two internal variants:
//!
//! - UDP-backed: holds the already-bound `UdpSocket` pair from SETUP.
//! - TCP-interleaved-backed: holds the consumer side of the media queue
//!   the client's thread feeds from SETUP on, and the RTCP statistics
//!   cell that thread counts the interleaved RTCP channel into.

use std::net::UdpSocket;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use bytes::Bytes;

use crate::h264::depacketizer::H264DepayConfig;
use crate::h264::receiver::H264Receiver;
use crate::rtcp::stats::RtcpStats;
use crate::rtsp::transport_negotiation::RtspTransportKind;
use crate::transport::RtpRecvTransport;
use tst_core::transport::EndReasonHandle;

/// State held between SETUP and PLAY / TEARDOWN.
///
/// # Send
///
/// This type is `Send`: moving it to a dedicated receive/watchdog thread
/// is a supported, documented use — a regression here is a breaking
/// change.
pub struct RtspSession {
    pub(crate) session_id: String,
    pub(crate) kind: RtspTransportKind,
    pub(crate) udp_sockets: Option<(UdpSocket, UdpSocket)>,
    /// For TcpInterleaved: consumer side of the media queue the client's
    /// thread feeds. `None` for UDP sessions.
    pub(crate) data_rx: Option<mpsc::Receiver<Bytes>>,
    /// For TcpInterleaved: the RTCP statistics cell the client's thread
    /// counts the interleaved RTCP channel into, and the local SSRC it
    /// matches report blocks against. [`Self::into_recv_transport`] hands
    /// both to the transport. `None` for UDP sessions.
    pub(crate) rtcp: Option<(Arc<Mutex<RtcpStats>>, u32)>,
    /// The `?recv_timeout=` value parsed from the `RtspUrl` at connect
    /// time (see `RtspClient::attempt_setup`). Applied to the transport
    /// / receiver built by [`Self::into_recv_transport`] /
    /// [`Self::into_h264_receiver`] via their `set_recv_timeout` setters
    /// — the session itself never blocks on I/O, so there is nothing to
    /// apply the deadline to before conversion.
    pub(crate) recv_timeout: Option<Duration>,
    /// Clone of the owning [`crate::rtsp::client::RtspClient`]'s
    /// [`EndReasonHandle`] — carried forward into
    /// [`Self::into_recv_transport`] / [`Self::into_h264_receiver`] so
    /// the resulting transport/receiver reports the SAME
    /// [`EndReason`](tst_core::transport::EndReason) the RTSP client's
    /// thread records, not a fresh (always-empty) slot.
    pub(crate) end_reason: EndReasonHandle,
}

impl RtspSession {
    pub(crate) fn new_udp(
        sid: String,
        rtp: UdpSocket,
        rtcp: UdpSocket,
        recv_timeout: Option<Duration>,
        end_reason: EndReasonHandle,
    ) -> Self {
        Self {
            session_id: sid,
            kind: RtspTransportKind::Udp,
            udp_sockets: Some((rtp, rtcp)),
            data_rx: None,
            rtcp: None,
            recv_timeout,
            end_reason,
        }
    }

    /// Construct a TCP-interleaved session from the media queue the
    /// client's thread feeds, the RTCP statistics cell it counts into and
    /// the SSRC it matches report blocks against.
    pub(crate) fn new_interleaved(
        sid: String,
        data_rx: mpsc::Receiver<Bytes>,
        rtcp_stats: Arc<Mutex<RtcpStats>>,
        ssrc: u32,
        recv_timeout: Option<Duration>,
        end_reason: EndReasonHandle,
    ) -> Self {
        Self {
            session_id: sid,
            kind: RtspTransportKind::TcpInterleaved,
            udp_sockets: None,
            data_rx: Some(data_rx),
            rtcp: Some((rtcp_stats, ssrc)),
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

    /// Consume the session and return an `RtpRecvTransport` ready for
    /// `DemuxReceiver`.
    ///
    /// For UDP: wraps the SETUP-allocated UDP socket pair into a
    /// pre-built `RtpRecvTransport` (avoiding a second `listen()` call).
    ///
    /// For TCP-interleaved: returns an `RtpRecvTransport` fed by the
    /// media queue `RtspClient`'s thread fills from SETUP on. That thread
    /// also counts the RR + SR frames arriving on the RTCP channel
    /// (RFC 7826 §14) into the transport's statistics, which
    /// `socket_stats().packets_lost_send` reports.
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
                    .expect("TcpInterleaved session has a data_rx");
                let (rtcp_stats, ssrc) = self
                    .rtcp
                    .take()
                    .expect("TcpInterleaved session has an RTCP cell");
                RtpRecvTransport::from_interleaved(data_rx, rtcp_stats, ssrc)
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
    /// For TCP-interleaved: takes the media queue. The client's thread
    /// counts the RTCP channel itself; the H.264 receiver reports no RTCP
    /// statistics.
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
                    .expect("TcpInterleaved session has a data_rx");
                H264Receiver::from_mpsc(data_rx, config)
            }
        };
        receiver.set_recv_timeout(self.recv_timeout);
        receiver.set_end_reason_slot(self.end_reason.clone());
        receiver
    }
}
