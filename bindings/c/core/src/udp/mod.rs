//! `tst_udp_*` C ABI entry points. Gated on `feature = "udp"`.
//!
//! This module exposes constructors that open UDP transports (unicast +
//! multicast, IPv4 + IPv6) and return new opaque handle types
//! (`TstUdpSender`, `TstUdpReceiver`, `TstUdpMuxSender`,
//! `TstUdpDemuxReceiver`). Once open, callers use the handle-specific
//! data-path entry points in the sub-modules below. Each handle has its
//! own `_close` entry point to free it.
//!
//! The surface mirrors `bindings/c/core/src/rtp/` module-for-module,
//! `_cancel` entry points included since ABI 0.22. The UDP transport
//! exposes a real `cancel_handle()` and every handle
//! holds it, so `_close` from any thread cancels first and
//! `tst_udp_<shell>_cancel` does the same WITHOUT freeing — see each
//! handle module's **Cancel** note for what that unblocks.
//!
//! URL grammar (ffmpeg-compatible):
//! - `udp://host:port` — unicast send (sender) / unicast bind (receiver)
//! - `udp://@group:port` (`@` prefix is ffmpeg convention) — multicast recv
//! - `udp://group:port` (group ∈ 224.0.0.0/4 or ff00::/8) — multicast send
//! - Query params: `?ttl=N`, `?iface=eth0`, `?tos=0xb8`, `?rcvbuf=8M`,
//!   `?sndbuf=2M`, `?pkt_size=1316`, `?localaddr=...`

pub mod sender;
pub mod receiver;
pub mod mux_sender;
pub mod demux_receiver;

pub use demux_receiver::{
    TstUdpDemuxReceiver, tst_udp_demux_receiver_cancel, tst_udp_demux_receiver_close,
    tst_udp_demux_receiver_get_socket_stats, tst_udp_demux_receiver_get_stats,
    tst_udp_demux_receiver_get_stream_codec_stats,
    tst_udp_demux_receiver_get_stream_last_seen_micros, tst_udp_demux_receiver_get_stream_stats,
    tst_udp_demux_receiver_next_event, tst_udp_demux_receiver_open,
    tst_udp_demux_receiver_reset_stats,
};
pub use mux_sender::{
    TstUdpMuxSender, tst_udp_mux_sender_cancel, tst_udp_mux_sender_close,
    tst_udp_mux_sender_finish, tst_udp_mux_sender_get_mux_sender_stats,
    tst_udp_mux_sender_get_socket_stats, tst_udp_mux_sender_get_stream_codec_stats,
    tst_udp_mux_sender_open, tst_udp_mux_sender_push_audio, tst_udp_mux_sender_push_audio_to,
    tst_udp_mux_sender_push_klv, tst_udp_mux_sender_push_klv_to, tst_udp_mux_sender_push_subtitle,
    tst_udp_mux_sender_push_subtitle_to, tst_udp_mux_sender_push_video,
    tst_udp_mux_sender_push_video_to, tst_udp_mux_sender_reset_stats,
};
pub use receiver::{
    TstUdpReceiver, tst_udp_receiver_cancel, tst_udp_receiver_close,
    tst_udp_receiver_get_socket_stats, tst_udp_receiver_get_stats, tst_udp_receiver_recv_ts,
    tst_udp_receiver_reset_stats, tst_udp_recv_open,
};
pub use sender::{
    TstUdpSender, tst_udp_sender_cancel, tst_udp_sender_close, tst_udp_sender_get_socket_stats,
    tst_udp_sender_get_stats, tst_udp_sender_open, tst_udp_sender_reset_stats,
    tst_udp_sender_send_ts,
};

/// `url` → a UDP send transport, recording `TST_E_UDP_CONFIG` or the build error.
pub(crate) fn connect(url: &str) -> Option<tst_udp::UdpTransport> {
    let builder = match tst_udp::UdpTransportBuilder::from_url(url) {
        Ok(b) => b,
        Err(e) => {
            crate::error::set_last_error(
                crate::error::TstError::UdpConfig,
                &format!("udp url parse: {e}"),
            );
            return None;
        }
    };
    builder
        .build()
        .map_err(|e| crate::error::record_with_context(e, "udp build"))
        .ok()
}

/// `url` → a UDP receive transport.
pub(crate) fn listen(url: &str) -> Option<tst_udp::UdpRecvTransport> {
    let builder = match tst_udp::UdpRecvTransportBuilder::from_url(url) {
        Ok(b) => b,
        Err(e) => {
            crate::error::set_last_error(
                crate::error::TstError::UdpConfig,
                &format!("udp url parse: {e}"),
            );
            return None;
        }
    };
    builder
        .build()
        .map_err(|e| crate::error::record_with_context(e, "udp recv build"))
        .ok()
}
