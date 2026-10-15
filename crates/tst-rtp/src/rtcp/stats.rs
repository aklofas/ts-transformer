//! RTCP-derived counters, exposed alongside [`tst_core::transport::SocketStats`]
//! via `RtpRecvTransport::rtcp_stats`.

use std::time::SystemTime;

use crate::rtcp::ingest::SrAnchor;

/// RTCP-specific protocol counters.
#[derive(Debug, Clone, Default)]
pub struct RtcpStats {
    /// Receiver reports the peer sent to this socket. A peer that only sends
    /// media (a camera, an RTSP server) sends sender reports, not receiver
    /// reports, so a receive-only client reads 0.
    pub rr_packets_received: u64,
    pub sr_packets_received: u64,
    /// Last RR ingest timestamp (system time of `recv`). Like [`Self::rr_packets_received`], filled only by receiver reports from the peer.
    pub last_rr_ts: Option<SystemTime>,
    /// Last SR ingest timestamp.
    pub last_sr_ts: Option<SystemTime>,
    /// Interarrival jitter in microseconds, from the most recent RR
    /// from peer. Like [`Self::rr_packets_received`], filled only by receiver reports from the peer.
    pub interarrival_jitter_us: u32,
    /// Q8 fixed-point fraction-lost from the most recent RR. Like [`Self::rr_packets_received`], filled only by receiver reports from the peer.
    pub fraction_lost_q8: u8,
    /// Cumulative packets lost reported by peer in the most recent RR
    /// report block matching our SSRC (RFC 3550 §6.4.1 24-bit field,
    /// clamped to >=0). Projected into
    /// [`tst_core::transport::SocketStats::packets_lost_send`] by
    /// [`crate::transport::RtpRecvTransport`]'s `socket_stats`. Like [`Self::rr_packets_received`], filled only by receiver reports from the peer.
    pub cumulative_lost_send: u32,
    /// Always 0, and written by nothing: a round-trip time needs the peer to
    /// echo one of our own sender reports, and no transport sends RTCP (see
    /// `docs/project/deferred-features.md`, RTCP statistics reporting). Projected into
    /// [`tst_core::transport::SocketStats::rtt_us`] by
    /// [`crate::transport::RtpRecvTransport`]'s `socket_stats`.
    pub rtt_us: u32,
    /// Anchor from the last received SR.
    pub last_sr_anchor: Option<SrAnchor>,
    /// Number of RTCP packets dropped due to parse errors.
    pub rr_parse_errors: u64,
    pub sr_parse_errors: u64,
}
