#![doc = include_str!("../README.md")]
//!
//! TS Transformer RTP transport — two receive shapes over RTP/RTSP:
//!
//! - **MPEG-TS-over-RTP (RFC 2250, PT=33):** an enclosing MPEG-TS stream rides
//!   a single RTP flow. Use [`RtpTransport`] / [`RtpRecvTransport`] (raw) or
//!   the `MuxSender<RtpTransport>` / `DemuxReceiver<RtpRecvTransport>` shells
//!   from `tst_pipeline`.
//!
//! - **H.264-over-RTP (RFC 6184):** a bare H.264 elementary stream rides an RTP
//!   flow with a dynamic payload type. Use [`H264Receiver`] (direct UDP) or the
//!   RTSP path [`RtspClient::setup_h264_auto`] → [`RtspSession::into_h264_receiver`].
//!
//! Both shapes share the RTSP/1.0 client (Phase 2) and server (Phase 3) for
//! negotiated unicast / multicast / TCP-interleaved sessions. RTSP/2.0 (RFC 7826)
//! requests are handled on the RFC 7826-compatible subset shared with RTSP/1.0;
//! the server does not implement RTSP/2.0-only features (e.g. pipelining,
//! REDIRECT, per-request timeouts). As of Phase 4 Stage 3, RTCP RR/SR ingest is
//! wired on the TCP-interleaved (RFC 7826 §14) client path: peer RR
//! populates `socket_stats().packets_lost_send` from the cumulative-lost
//! field and `socket_stats().rtt_us` from the RR-after-SR calculation
//! (RFC 3550 §6.4.1). UDP-side RTCP ingest is deferred.
//!
//! This crate provides the RTP-specific concrete types. The
//! [`Transport`](tst_core::transport::Transport) /
//! [`RecvTransport`](tst_core::transport::RecvTransport) traits themselves
//! live in [`tst_core`]; the transport-agnostic Sender/Receiver shells
//! live in `tst_pipeline`.
//!
//! Everything above is a sync facade — no async runtime, no tokio. The one
//! exception is `RtspServer` (only present when the `rtsp-server` feature
//! is on, which is the default), which runs an internal tokio Runtime
//! behind its own sync facade; tokio only enters this crate's dependency
//! tree through that feature. See the README's "Feature flags" section for
//! the tokio-free client-only build.

#![warn(rustdoc::broken_intra_doc_links)]

pub mod builder;
pub mod cancel;
pub mod clock;
pub mod error;
pub mod h264;
pub mod init;
pub mod packet;
pub mod rtcp;
pub mod rtsp;
pub mod sdp;
pub mod transport;
pub mod url;

mod binding_kind;

// RTP transport.
pub use builder::{RtpRecvSocketBuilder, RtpSocketBuilder};
pub use cancel::RtpCancelHandle;
pub use clock::RtpClock;
pub use packet::{Parsed, RTP_HEADER_LEN, RTP_PT_MP2T, RTP_VERSION, RtpHeader, RtpParseError};
pub use transport::{ConnectError, RtpRecvTransport, RtpStats, RtpTransport};
pub use url::{DEFAULT_PKT_SIZE, RtpUrl, UrlError as RtpUrlError};

// RTSP client.
pub use builder::RtspClientBuilder;
pub use error::RtspError;
pub use rtcp::ingest::{SrAnchor, compute_rtt_us, ingest_rr, ingest_sr, system_time_to_ntp_mid};
pub use rtcp::reporter::{RTCP_BASE_INTERVAL, RtcpReporterHandle};
pub use rtcp::stats::RtcpStats;
pub use rtcp::{ReceiverReport, ReportBlock, RtcpError, RtcpPacketType, SdesPacket, SenderReport};
pub use rtsp::auth::{AuthChallenge, DigestAlgorithm, DigestChallenge, DigestContext};
pub use rtsp::client::end_reason::{StreamEndReason, StreamEndReasonHandle};
pub use rtsp::client::options_describe::OptionsResponse;
pub use rtsp::client::play::{RtpInfo, parse_rtp_info};
pub use rtsp::client::session::RtspSession;
pub use rtsp::client::transport_negotiation::{RtspTransportKind, TransportResponse};
pub use rtsp::client::{RtspCancelHandle, RtspClient};
pub use rtsp::message::{RtspMethod, RtspRequest, RtspResponse};
pub use sdp::pick::{H264Media, pick_h264, pick_mp2t};
pub use sdp::{Sdp, SdpMedia};
/// Re-export: the credential type accepted by
/// [`RtspClientBuilder::auth`](crate::RtspClientBuilder::auth) — consumers
/// need not depend on the `secrecy` crate directly (`password.into()`
/// worked before but was non-obvious).
pub use secrecy::SecretString;
pub use url::{RtspScheme, RtspTransportPref, RtspUrl, RtspVersion};

// H.264 RTP payload format (RFC 6184).
pub use h264::{
    H264Au, H264Depacketizer, H264DepayConfig, H264DepayStats, H264FmtpParams, H264Receiver,
    ParameterSetInjection, parse_rtpmap_h264,
};

// RTSP server (requires the `rtsp-server` feature, default-on). The error
// enums stay unconditional — they have no tokio dependency, so gating them
// would only fragment the error surface for no benefit.
#[cfg(feature = "rtsp-server")]
pub use builder::RtspServerBuilder;
#[cfg(feature = "rtsp-server")]
pub use cancel::RtspServerCancelHandle;
pub use error::{MountError, RtspServerError};
#[cfg(feature = "rtsp-server")]
pub use rtsp::server::mount::{MountHandle, MountKind, MountStats};
#[cfg(feature = "rtsp-server")]
pub use rtsp::server::{RtspServer, ServerStats};
pub use url::MulticastGroup;

#[cfg(test)]
mod tests {
    /// `RtspClient` / `RtspSession` / `H264Receiver` are documented as
    /// `Send` (see each type's rustdoc) — this is the compile-time guard:
    /// a change that drops `Send` here fails to compile, so the guarantee
    /// can't regress silently.
    #[test]
    fn send_bound_is_a_public_guarantee() {
        fn assert_send<T: Send>() {}
        assert_send::<crate::RtspClient>();
        assert_send::<crate::RtspSession>();
        assert_send::<crate::H264Receiver>();
    }
}
