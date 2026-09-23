//! `tst_tcp_*` C ABI entry points. Gated on `feature = "tcp"`.
//!
//! This module exposes constructors for the dual-trait `TcpTransport`
//! (implements both `Transport` + `RecvTransport`; role determined by which
//! pipeline shell consumes it) plus the `TcpListener` accept loop.
//!
//! Unlike UDP — which has separate `UdpTransport` (send) and
//! `UdpRecvTransport` (recv) types — TCP has a single `TcpTransport` for both
//! roles. All four handle families (sender / receiver / mux sender / demux
//! receiver) are constructed via `TcpTransportBuilder::from_url`; the role is
//! determined by which pipeline shell you wrap it in.
//!
//! A fifth handle type, `TstTcpListener`, binds a server-side socket and
//! accepts incoming connections as sender or receiver handles.
//!
//! URL schemes:
//! - `tcp://host:port` — plain TCP caller (connect)
//! - `tcps://host:port` — TLS caller (requires `tls` sub-feature; returns
//!   `TST_E_TCP_TLS` if disabled at build time)
//! - `tcp://addr:port?listen=1` — listener (via `tst_tcp_listener_from_url`)
//!
//! Common query params for caller-side handles: `?nodelay=1`, `?rcvbuf=N`,
//! `?sndbuf=N`, `?pkt_size=N`, `?connect_timeout=Ns`.
//!
//! **No C-side cancel yet:** `TcpTransport` does expose a cross-thread
//! `cancel_handle()` (`TcpCancelHandle`), but this module has no
//! `tst_tcp_*_cancel` entry points to reach it (additive ABI work, not yet
//! requested). To unblock a thread parked in a data-path call, close the
//! handle from the same thread (or rely on the socket's read/write
//! behavior) — the same contract the UDP module has today.

pub mod sender;
pub mod receiver;
pub mod mux_sender;
pub mod demux_receiver;
pub mod listener;

pub use demux_receiver::{
    TstTcpDemuxReceiver, tst_tcp_demux_receiver_close, tst_tcp_demux_receiver_get_socket_stats,
    tst_tcp_demux_receiver_get_stats, tst_tcp_demux_receiver_get_stream_codec_stats,
    tst_tcp_demux_receiver_get_stream_last_seen_micros, tst_tcp_demux_receiver_get_stream_stats,
    tst_tcp_demux_receiver_next_event, tst_tcp_demux_receiver_open,
    tst_tcp_demux_receiver_reset_stats,
};
pub use listener::{
    TstTcpListener, tst_tcp_listener_accept_receiver, tst_tcp_listener_accept_sender,
    tst_tcp_listener_bind, tst_tcp_listener_free, tst_tcp_listener_from_url,
};
pub use mux_sender::{
    TstTcpMuxSender, tst_tcp_mux_sender_close, tst_tcp_mux_sender_finish,
    tst_tcp_mux_sender_get_mux_sender_stats, tst_tcp_mux_sender_get_socket_stats,
    tst_tcp_mux_sender_get_stream_codec_stats, tst_tcp_mux_sender_open,
    tst_tcp_mux_sender_push_audio, tst_tcp_mux_sender_push_audio_to, tst_tcp_mux_sender_push_klv,
    tst_tcp_mux_sender_push_klv_to, tst_tcp_mux_sender_push_subtitle,
    tst_tcp_mux_sender_push_subtitle_to, tst_tcp_mux_sender_push_video,
    tst_tcp_mux_sender_push_video_to, tst_tcp_mux_sender_reset_stats,
};
pub use receiver::{
    TstTcpReceiver, tst_tcp_receiver_close, tst_tcp_receiver_get_socket_stats,
    tst_tcp_receiver_get_stats, tst_tcp_receiver_recv_ts, tst_tcp_receiver_reset_stats,
    tst_tcp_recv_open,
};
pub use sender::{
    TstTcpSender, tst_tcp_sender_close, tst_tcp_sender_get_socket_stats, tst_tcp_sender_get_stats,
    tst_tcp_sender_open, tst_tcp_sender_reset_stats, tst_tcp_sender_send_ts,
};
