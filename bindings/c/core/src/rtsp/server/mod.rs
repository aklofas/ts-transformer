//! RTSP server C ABI — `tst_rtsp_server_builder_*`, `tst_rtsp_server_*`,
//! and mount handle methods.
//!
//! Wraps `tst_rtp::RtspServerBuilder`, `tst_rtp::RtspServer`, and
//! `tst_rtp::MountHandle` with a sync C ABI.
//!
//! Module layout:
//! - `builder`: `tst_rtsp_server_builder_new` + setter chain
//!   (`_bind`, `_auth_basic`, `_auth_digest_md5`, `_auth_digest_sha256`,
//!    `_max_sessions`, `_session_timeout`, `_fanout_capacity`,
//!    `_graceful_shutdown_drain_ms`, `_tls_cert_pem`,
//!    `_accept_unregistered_publishers`) + `_free`.
//! - `start`: `tst_rtsp_server_builder_start` + the opaque `TstRtspServer`
//!   scaffold.
//! - `mount` / `mount_getters`: mount creation (`_add_unicast_mount`,
//!   `_add_multicast_mount`, `_mount_handle_free`), the opaque
//!   `TstRtspMountHandle` scaffold, and push methods on it (`push_video`,
//!   `push_video_to`, `push_klv`, `push_klv_to`, `push_audio`,
//!   `push_audio_to`, `push_subtitle`, `push_subtitle_to`).
//! - `publish`: the publisher role — publish mounts
//!   (`_add_publish_mount`, `tst_rtsp_publish_mount_*`), the on-demand
//!   queue (`_next_publisher`), `_remove_mount`, and the server's publisher
//!   counters.
//! - `stop`: server-level stats, stop, cancel, free.

pub(crate) mod builder;
pub(crate) mod mount;
pub(crate) mod mount_getters;
pub(crate) mod publish;
pub(crate) mod start;
pub(crate) mod stop;
pub(crate) mod types;
