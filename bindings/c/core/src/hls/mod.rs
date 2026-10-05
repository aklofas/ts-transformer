//! `tst_hls_*` + `tst_publisher_*` + `tst_mux_publisher_*` C ABI entry
//! points. Gated on `feature = "hls"`.
//!
//! Exposes the HLS publisher (builder + concrete `HlsPublisher` handle
//! that runs an internal tokio HTTP server) and the
//! `MuxPublisher<P>` shell projection over `tst_core::publisher::Publisher`.
//! KLV stays inside the .ts segments — no WebVTT sidecar, no
//! EXT-X-DATERANGE metadata.
//!
//! Surface map:
//! - [`builder`] — `TstHlsPublisherBuilder` + the `tst_hls_publisher_builder_*`
//!   chain that constructs a `TstPublisher`.
//! - [`publisher`] — `TstPublisher` + the universal `tst_publisher_*`
//!   trait-mirror entries (`push_ts` / `cut_segment` / `finish` /
//!   `get_stats` / `kind` / `free`) + the HLS-specific
//!   `tst_hls_publisher_*` accessors (`get_hls_stats` / `local_addr` /
//!   `render_playlist` / `get_forced_cuts`) + the `finish_serving`
//!   path with its `TstHlsServerHandle` (`local_addr` / `shutdown` /
//!   `free`).
//! - [`mux_publisher`] — `TstMuxPublisher` (a `MuxPublisher<HlsPublisher>`)
//!   + the `tst_mux_publisher_*` encoded-elementary push family.

pub mod builder;
pub mod publisher;
pub mod mux_publisher;

pub use builder::{
    TstHlsPublisherBuilder, tst_hls_publisher_builder_basic_auth, tst_hls_publisher_builder_bind,
    tst_hls_publisher_builder_build, tst_hls_publisher_builder_enable_tls,
    tst_hls_publisher_builder_free, tst_hls_publisher_builder_from_url,
    tst_hls_publisher_builder_max_segment_duration_ms, tst_hls_publisher_builder_mode,
    tst_hls_publisher_builder_new, tst_hls_publisher_builder_output_dir,
    tst_hls_publisher_builder_playlist_window, tst_hls_publisher_builder_segment_duration_ms,
};
pub use mux_publisher::{
    TstMuxPublisher, tst_mux_publisher_cut_segment, tst_mux_publisher_finish_into_publisher,
    tst_mux_publisher_free, tst_mux_publisher_get_publisher_stats, tst_mux_publisher_get_stats,
    tst_mux_publisher_send_audio, tst_mux_publisher_send_klv, tst_mux_publisher_send_subtitle,
    tst_mux_publisher_send_video, tst_mux_publisher_with_config_hls,
};
pub use publisher::{
    TstHlsServerHandle, TstPublisher, TstPublisherKind, tst_hls_publisher_finish_serving,
    tst_hls_publisher_get_forced_cuts, tst_hls_publisher_get_hls_stats,
    tst_hls_publisher_local_addr, tst_hls_publisher_render_playlist, tst_hls_server_handle_free,
    tst_hls_server_handle_local_addr, tst_hls_server_handle_shutdown, tst_publisher_cut_segment,
    tst_publisher_finish, tst_publisher_free, tst_publisher_get_kind, tst_publisher_get_stats,
    tst_publisher_push_ts,
};
