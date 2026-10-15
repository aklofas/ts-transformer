//! RTCP-derived counters, exposed alongside [`tst_core::transport::SocketStats`]
//! via `RtpRecvTransport::rtcp_stats`.

use std::time::SystemTime;

use crate::rtcp::ingest::SrAnchor;

/// RTCP-specific protocol counters.
#[derive(Debug, Clone, Default)]
pub struct RtcpStats {
    pub rr_packets_received: u64,
    pub sr_packets_received: u64,
    /// Last RR ingest timestamp (system time of `recv`).
    pub last_rr_ts: Option<SystemTime>,
    /// Last SR ingest timestamp.
    pub last_sr_ts: Option<SystemTime>,
    /// Interarrival jitter in microseconds, from the most recent RR
    /// from peer.
    pub interarrival_jitter_us: u32,
    /// Q8 fixed-point fraction-lost from the most recent RR.
    pub fraction_lost_q8: u8,
    /// Cumulative packets lost reported by peer in the most recent RR
    /// report block matching our SSRC (RFC 3550 §6.4.1 24-bit field,
    /// clamped to >=0). Projected into
    /// [`tst_core::transport::SocketStats::packets_lost_send`] by
    /// [`crate::transport::RtpRecvTransport`]'s `socket_stats`.
    pub cumulative_lost_send: u32,
    /// Always 0: a round-trip time needs the peer to echo one of our own
    /// sender reports, and no transport sends RTCP. Projected into
    /// [`tst_core::transport::SocketStats::rtt_us`] by
    /// [`crate::transport::RtpRecvTransport`]'s `socket_stats`.
    pub rtt_us: u32,
    /// Anchor from the last received SR.
    pub last_sr_anchor: Option<SrAnchor>,
    /// Number of RTCP packets dropped due to parse errors.
    pub rr_parse_errors: u64,
    pub sr_parse_errors: u64,
}
