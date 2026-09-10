//! `tst_rist_*` C ABI entry points. Gated on `feature = "rist"`.
//!
//! This module exposes constructors that open RIST transports (Simple +
//! Main profiles, librist v0.2.x, AES encryption via mbedTLS) and
//! return new opaque handle types (`TstRistSender`, `TstRistReceiver`,
//! `TstRistMuxSender`, `TstRistDemuxReceiver`). Once open, callers use
//! the handle-specific data-path entry points in the sub-modules below.
//! Each handle has its own `_close` entry point to free it.
//!
//! The surface mirrors `bindings/c/core/src/udp/` module-for-module **minus
//! cancel**: the RIST transport does not expose a `cancel_handle()`, so
//! there are no `tst_rist_*_cancel` entry points. To unblock a thread
//! parked in a data-path call, close the handle from the same thread (or
//! rely on the socket's read/write behavior).
//!
//! **Construction pattern (differs from UDP):**
//! RIST uses a move-style builder:
//! - Sender / MuxSender: `RistTransportBuilder::new(url)?.connect()?`
//! - Receiver / DemuxReceiver: `RistRecvTransportBuilder::new(url)?.listen()?`
//!
//! URL grammar:
//! - `rist://host:port` — unicast send (sender / mux_sender)
//! - `rist://@host:port` (`@` prefix, ffmpeg convention) — bind/listen (receiver / demux_receiver)
//! - `rist://group:port` (group ∈ 224.0.0.0/4) — multicast send
//! - Query params: `?profile=simple|main`, `?buffer=N` (recovery buffer ms),
//!   `?bandwidth=N` (kbps), `?cname=...` (RTCP CNAME)
//! - Encryption (Main Profile only, requires `mbedtls` feature):
//!   `?aes-type=128|192|256&secret=<psk>` — forces Main Profile; returns
//!   `TST_E_RIST_ENCRYPTION_DISABLED (-41)` when mbedtls is disabled.
//!   `?secret=` alone also forces Main Profile and selects AES-256
//!   (librist's own default); `?aes-type=` alone is rejected at parse,
//!   since a cipher without a PSK would configure a plaintext link.

pub mod sender;
pub mod receiver;
pub mod mux_sender;
pub mod demux_receiver;

pub use demux_receiver::{
    TstRistDemuxReceiver, tst_rist_demux_receiver_close, tst_rist_demux_receiver_get_socket_stats,
    tst_rist_demux_receiver_get_stats, tst_rist_demux_receiver_get_stream_codec_stats,
    tst_rist_demux_receiver_get_stream_last_seen_micros, tst_rist_demux_receiver_get_stream_stats,
    tst_rist_demux_receiver_next_event, tst_rist_demux_receiver_open,
    tst_rist_demux_receiver_reset_stats,
};
pub use mux_sender::{
    TstRistMuxSender, tst_rist_mux_sender_close, tst_rist_mux_sender_get_mux_sender_stats,
    tst_rist_mux_sender_get_socket_stats, tst_rist_mux_sender_get_stream_codec_stats,
    tst_rist_mux_sender_open, tst_rist_mux_sender_push_audio, tst_rist_mux_sender_push_audio_to,
    tst_rist_mux_sender_push_klv, tst_rist_mux_sender_push_klv_to,
    tst_rist_mux_sender_push_subtitle, tst_rist_mux_sender_push_subtitle_to,
    tst_rist_mux_sender_push_video, tst_rist_mux_sender_push_video_to,
    tst_rist_mux_sender_reset_stats,
};
pub use receiver::{
    TstRistReceiver, tst_rist_receiver_close, tst_rist_receiver_get_socket_stats,
    tst_rist_receiver_get_stats, tst_rist_receiver_recv_ts, tst_rist_receiver_reset_stats,
    tst_rist_recv_open,
};
pub use sender::{
    TstRistSender, tst_rist_sender_close, tst_rist_sender_get_socket_stats,
    tst_rist_sender_get_stats, tst_rist_sender_open, tst_rist_sender_reset_stats,
    tst_rist_sender_send_ts,
};
