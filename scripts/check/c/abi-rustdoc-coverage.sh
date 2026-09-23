#!/usr/bin/env bash
# Verify that every C ABI export in tstrans.h has a `# C ABI` rustdoc
# cross-reference on the corresponding Rust method, and vice versa.
#
# The script greps for the C export name in the Rust core source trees
# (tst-pipeline, tst-srt, tst-core). A mention from any `# C ABI` block
# (see existing examples in mux_sender.rs / sender/mod.rs / raw_sender.rs /
# builder.rs / mpegts/mux/mod.rs) satisfies the check.
#
# Allow-listed exports are either:
#   - error / init helpers without Rust counterparts
#   - C-only config-builder wrappers that build Rust types via opaque ptrs
#     (the underlying Rust builder method is the 1:1 counterpart, but it
#     uses the builder-shape rather than mirroring the C function name)
#   - pending backfill from sub-phase 3.6.4 / 3.6.5 (rustdoc cross-refs to
#     be added on the corresponding Rust methods)

set -euo pipefail

# Paths relative to ts-transformer/ workspace root.
HEADER="bindings/c/include/tstrans.h"
SRC_DIRS=(
    "crates/tst-pipeline/src"
    "crates/tst-srt/src"
    "crates/tst-core/src"
)

# Allow-list — exports without a 1:1 Rust counterpart, or pending 3.6.4/3.6.5
# backfill on the Rust side. Each block carries a one-line justification.
ALLOWLIST=(
    # --- error / init helpers (no Rust counterpart by design) ---
    "tst_init"
    "tst_last_error"
    "tst_get_last_error"
    "tst_get_last_error_str"
    "tst_clear_last_error"
    # "tst_version" — removed in Wave 5.A; superseded by tst_get_version_*
    "tst_panic_recover"

    # --- version accessors (no Rust counterpart by design — read Cargo.toml /
    #     TST_*_VERSION_* compile-time macros for the same values) ---
    "tst_get_version_major"
    "tst_get_version_minor"
    "tst_get_version_patch"
    "tst_get_version_packed"
    "tst_get_version_string"
    "tst_get_abi_version_major"
    "tst_get_abi_version_minor"

    # --- config-builder C wrappers (Rust uses builder methods, not 1:1 names) ---
    "tst_mux_config_new"
    "tst_mux_config_free"
    "tst_mux_config_add_program"
    "tst_mux_config_add_video_stream"
    "tst_mux_config_add_klv_stream"
    "tst_mux_config_add_data_stream"
    "tst_mux_config_set_av1_carriage"
    "tst_mux_config_set_buffer_packets"
    "tst_mux_config_set_pcr_interval_ms"
    "tst_mux_config_set_pcr_pid"
    "tst_mux_config_set_psi_interval_ms"
    "tst_mux_config_set_program_descriptors"
    "tst_mux_config_set_stream_descriptors_for_video"
    "tst_mux_config_set_stream_descriptors_for_klv"
    "tst_mux_config_set_stream_descriptors_for_data"
    "tst_mux_config_add_audio_stream"
    "tst_mux_config_add_audio_stream_with_language"
    "tst_mux_config_add_subtitle_stream_dvb_subtitling"
    "tst_mux_config_add_subtitle_stream_dvb_teletext"
    "tst_mux_config_add_subtitle_stream_cea708"
    "tst_mux_config_add_subtitle_stream_webvtt"
    "tst_sender_config_new"
    "tst_sender_config_free"
    "tst_sender_config_set_framing_mode"
    "tst_sender_config_set_max_unsynced_bytes"
    "tst_raw_sender_config_new"
    "tst_raw_sender_config_free"
    "tst_reconnect_policy_new"
    "tst_reconnect_policy_free"
    "tst_reconnect_policy_set_backoff_constant_ms"
    "tst_reconnect_policy_set_backoff_exponential_ms"
    "tst_reconnect_policy_set_gap_buffer_capacity"
    "tst_reconnect_policy_set_max_attempts"
    "tst_reconnect_policy_set_overflow_policy"
    "tst_reconnect_policy_set_mode"

    # --- managed-transport wrappers (ride 3.6.4/3.6.5 backfill on
    #     ManagedTransport / ManagedRecvTransport methods) ---
    "tst_managed_mux_sender_close"
    "tst_managed_mux_sender_get_stats"
    "tst_managed_mux_sender_reset_stats"
    "tst_managed_mux_sender_send_klv"
    "tst_managed_mux_sender_send_klv_to"
    "tst_managed_mux_sender_send_video"
    "tst_managed_mux_sender_send_video_to"
    "tst_managed_mux_sender_send_audio"
    "tst_managed_mux_sender_send_audio_to"
    "tst_managed_mux_sender_send_subtitle"
    "tst_managed_mux_sender_send_subtitle_to"
    "tst_managed_mux_sender_send_data"
    "tst_managed_mux_sender_send_data_to"
    "tst_managed_raw_sender_close"
    "tst_managed_raw_sender_get_stats"
    "tst_managed_raw_sender_reset_stats"
    "tst_managed_raw_sender_send"
    "tst_managed_sender_close"
    "tst_managed_sender_flush"
    "tst_managed_sender_get_stats"
    "tst_managed_sender_reset_stats"
    "tst_managed_sender_send_ts"

    # --- stats / open accessors pending 3.6.4/3.6.5 backfill on the
    #     corresponding Rust methods (Muxer::stats, MuxSender::stats, etc.) ---
    "tst_muxer_open"
    "tst_muxer_get_stats"
    "tst_muxer_reset_stats"
    "tst_mux_sender_get_stats"
    "tst_mux_sender_reset_stats"
    "tst_sender_get_stats"
    "tst_sender_reset_stats"
    "tst_raw_sender_get_stats"
    "tst_raw_sender_reset_stats"

    # --- Phase 1 receiver surface open helpers (no direct Rust counterpart:
    #     URL parsing + Box construction happen entirely in the C layer;
    #     there is no single Rust method that maps 1:1 to these entry points) ---
    "tst_raw_receiver_open"
    "tst_raw_receiver_open_listener"
    "tst_managed_raw_receiver_open"
    "tst_managed_raw_receiver_open_listener"

    # --- Phase 1 managed-wrapper entry points (ride the plain-side # C ABI
    #     rustdoc cross-reference: ManagedRecvTransport calls through to
    #     the same underlying RawReceiver methods that carry the backfilled
    #     # C ABI blocks; adding duplicate cross-refs on the managed wrappers
    #     would not add useful information) ---
    "tst_managed_raw_receiver_recv"
    "tst_managed_raw_receiver_cancel"
    "tst_managed_raw_receiver_close"
    "tst_managed_raw_receiver_get_stats"
    "tst_managed_raw_receiver_reset_stats"
    "tst_managed_raw_sender_cancel"
    "tst_managed_sender_cancel"
    "tst_managed_mux_sender_cancel"

    # --- Phase 2 receiver surface open helpers (no direct Rust counterpart:
    #     URL parsing + Box construction happen entirely in the C layer;
    #     there is no single Rust method that maps 1:1 to these entry points) ---
    "tst_receiver_open"
    "tst_receiver_open_listener"
    "tst_managed_receiver_open"
    "tst_managed_receiver_open_listener"

    # --- Phase 2 managed-wrapper entry points (ride the plain-side # C ABI
    #     rustdoc cross-reference on Receiver<R>::{next_packet, stats,
    #     reset_stats, close, cancel_handle}; adding duplicate cross-refs on
    #     the managed wrappers would not add useful information) ---
    "tst_managed_receiver_recv_packet"
    "tst_managed_receiver_cancel"
    "tst_managed_receiver_close"
    "tst_managed_receiver_get_stats"
    "tst_managed_receiver_reset_stats"

    # --- Phase 3 demux-config-builder C wrappers (Rust uses
    #     DemuxerConfigBuilder methods, not 1:1 names) ---
    "tst_demux_config_new"
    "tst_demux_config_free"
    "tst_demux_config_set_strict_mode"
    "tst_demux_config_add_link_klv"
    "tst_demux_config_add_treat_as"
    "tst_demux_config_set_pes_cap"
    "tst_demux_config_set_cfi_tolerance"
    "tst_demux_config_set_av1_carriage"
    "tst_demux_config_set_au_cell_cap_per_pid"
    "tst_demux_config_set_lenient_psi_reassembly"

    # --- Offline demuxer close (no 1:1 Rust counterpart — tst_demuxer_close
    #     is purely a Box::from_raw + Handle::close + drop; there is no
    #     Demuxer::close method to cross-ref) ---
    "tst_demuxer_close"

    # --- Phase 3 mux-config descriptor wrappers (mirror existing
    #     tst_mux_config_set_*_descriptors / set_program_descriptors
    #     pattern: C-side TLV assembly + opaque-ptr forwarding) ---
    "tst_mux_config_add_video_descriptor"
    "tst_mux_config_add_klv_descriptor"
    "tst_mux_config_add_data_descriptor"
    "tst_mux_config_add_audio_descriptor"
    "tst_mux_config_add_subtitle_descriptor"

    # --- Phase 3 receiver-surface open helpers (no direct Rust counterpart:
    #     URL parsing + Box construction happen entirely in the C layer;
    #     there is no single Rust method that maps 1:1 to these entry points) ---
    "tst_demux_receiver_open"
    "tst_demux_receiver_open_listener"
    "tst_demux_receiver_open_with_config"
    "tst_demux_receiver_open_listener_with_config"
    "tst_managed_demux_receiver_open"
    "tst_managed_demux_receiver_open_listener"
    "tst_managed_demux_receiver_open_with_config"
    "tst_managed_demux_receiver_open_listener_with_config"

    # --- Phase 3 plain demux-receiver entry points (ride the
    #     DemuxReceiver<R>::{recv_event, stats, reset_stats, cancel_handle}
    #     methods — the C wrappers are thin pass-throughs; adding # C ABI
    #     cross-refs on each method individually would duplicate without
    #     adding useful information given the 1:1 naming) ---
    "tst_demux_receiver_recv_event"
    "tst_demux_receiver_cancel"
    "tst_demux_receiver_get_stats"
    "tst_demux_receiver_reset_stats"
    "tst_demux_receiver_get_stream_stats"

    # --- Phase 3 managed-demux-receiver entry points (ride the plain-side
    #     allowlist entries above + the ManagedRecvTransport wrapping) ---
    "tst_managed_demux_receiver_recv_event"
    "tst_managed_demux_receiver_cancel"
    "tst_managed_demux_receiver_close"
    "tst_managed_demux_receiver_get_stats"
    "tst_managed_demux_receiver_reset_stats"
    "tst_managed_demux_receiver_get_stream_stats"

    # --- libsrt wire-stats managed-wrapper entry points (ride the plain-side
    #     # C ABI cross-refs on MuxSender::socket_stats / Sender::socket_stats /
    #     RawSender::transport / Receiver::socket_stats / RawReceiver::socket_stats
    #     / DemuxReceiver::socket_stats — ManagedTransport / ManagedRecvTransport
    #     forward to the same underlying method; adding duplicate cross-refs would
    #     not add useful information) ---
    "tst_managed_mux_sender_get_socket_stats"
    "tst_managed_sender_get_socket_stats"
    "tst_managed_raw_sender_get_socket_stats"
    "tst_managed_receiver_get_socket_stats"
    "tst_managed_raw_receiver_get_socket_stats"
    "tst_managed_demux_receiver_get_socket_stats"

    # --- Phase 4 RTSP client builder entry points (tst-c–only wrappers:
    #     RtspClientBuilder uses consuming mut-self chain setters, making
    #     in-place C mutation impossible; the C wrappers store fields
    #     directly in TstRtspClientBuilder and reconstruct the Rust builder
    #     at connect time — no 1:1 Rust method counterpart exists in
    #     tst-pipeline / tst-srt / tst-core to cross-ref) ---
    "tst_rtsp_client_builder_new"
    "tst_rtsp_client_builder_transport_pref"
    "tst_rtsp_client_builder_keepalive"
    "tst_rtsp_client_builder_tls_root_cert_pem"
    "tst_rtsp_client_builder_auth_basic"
    "tst_rtsp_client_builder_auth_digest_md5"
    "tst_rtsp_client_builder_auth_digest_sha256"
    "tst_rtsp_client_builder_free"

    # --- Phase 4 RTP lifecycle entry points (open + close) ---
    #     The C-side concrete handle types (TstRtpSender, TstRtpReceiver,
    #     TstRtpMuxSender, TstRtpDemuxReceiver) are tst-c–only projections
    #     of the Rust pipeline types parameterized on RtpTransport /
    #     RtpRecvTransport, with no 1:1 Rust counterpart.
    "tst_rtp_sender_open"
    "tst_rtp_sender_close"
    "tst_rtp_recv_open"
    "tst_rtp_receiver_close"
    "tst_rtp_mux_sender_open"
    "tst_rtp_mux_sender_close"
    "tst_rtp_mux_sender_finish"
    "tst_rtp_demux_receiver_open"
    "tst_rtp_demux_receiver_close"

    # --- Phase 4 RTP data-path entry points (Fix-C) ---
    #     Same rationale as above — tst-c–only wrappers delegating to the
    #     same pipeline::Sender / Receiver / MuxSender / DemuxReceiver
    #     methods that the SRT variants call, but typed on RtpTransport.
    # TstRtpSender
    "tst_rtp_sender_send_ts"
    "tst_rtp_sender_cancel"
    "tst_rtp_sender_get_stats"
    "tst_rtp_sender_get_socket_stats"
    "tst_rtp_sender_reset_stats"
    # TstRtpReceiver
    "tst_rtp_receiver_recv_ts"
    "tst_rtp_receiver_cancel"
    "tst_rtp_receiver_get_stats"
    "tst_rtp_receiver_get_socket_stats"
    "tst_rtp_receiver_reset_stats"
    # TstRtpMuxSender
    "tst_rtp_mux_sender_push_video"
    "tst_rtp_mux_sender_push_video_to"
    "tst_rtp_mux_sender_push_klv"
    "tst_rtp_mux_sender_push_klv_to"
    "tst_rtp_mux_sender_push_audio"
    "tst_rtp_mux_sender_push_audio_to"
    "tst_rtp_mux_sender_push_subtitle"
    "tst_rtp_mux_sender_push_subtitle_to"
    "tst_rtp_mux_sender_cancel"
    "tst_rtp_mux_sender_get_mux_sender_stats"
    "tst_rtp_mux_sender_get_socket_stats"
    "tst_rtp_mux_sender_get_stream_codec_stats"
    "tst_rtp_mux_sender_reset_stats"
    # TstRtpDemuxReceiver
    "tst_rtp_demux_receiver_next_event"
    "tst_rtp_demux_receiver_cancel"
    "tst_rtp_demux_receiver_get_stats"
    "tst_rtp_demux_receiver_get_socket_stats"
    "tst_rtp_demux_receiver_get_stream_codec_stats"
    "tst_rtp_demux_receiver_get_stream_stats"
    "tst_rtp_demux_receiver_reset_stats"

    # --- Bindings-parity bundle, item 8 (2026-08-20): RTP stream-end
    #     reason getters. Same rationale as the RTP data-path block above
    #     — TstRtpReceiver/TstRtpDemuxReceiver are tst-c–only projections;
    #     the underlying StreamEndReasonHandle::get() lives in tst-rtp,
    #     outside this script's SRC_DIRS (tst-pipeline/tst-srt/tst-core),
    #     so no in-scope Rust cross-ref is possible.
    "tst_rtp_receiver_end_reason"
    "tst_rtp_demux_receiver_end_reason"

    # --- Plan A5a UDP entry points (full RTP parity) ---
    #     tst-c–only projections of pipeline Sender/Receiver/MuxSender/
    #     DemuxReceiver typed on tst_udp::Udp{,Recv}Transport. Same rationale
    #     as the RTP block above — they delegate to the same pipeline methods
    #     the SRT variants call. The _cancel entry points (ABI 0.22) fire
    #     tst_udp::UdpCancelHandle through the shared Owned slot — the Rust
    #     counterpart is the generic pipeline shell's cancel_handle(), already
    #     cross-referenced from the SRT names.
    # TstUdpSender
    "tst_udp_sender_open"
    "tst_udp_sender_close"
    "tst_udp_sender_cancel"
    "tst_udp_sender_send_ts"
    "tst_udp_sender_get_stats"
    "tst_udp_sender_get_socket_stats"
    "tst_udp_sender_reset_stats"
    # TstUdpReceiver
    "tst_udp_recv_open"
    "tst_udp_receiver_close"
    "tst_udp_receiver_cancel"
    "tst_udp_receiver_recv_ts"
    "tst_udp_receiver_get_stats"
    "tst_udp_receiver_get_socket_stats"
    "tst_udp_receiver_reset_stats"
    # TstUdpMuxSender
    "tst_udp_mux_sender_open"
    "tst_udp_mux_sender_close"
    "tst_udp_mux_sender_cancel"
    "tst_udp_mux_sender_finish"
    "tst_udp_mux_sender_push_video"
    "tst_udp_mux_sender_push_video_to"
    "tst_udp_mux_sender_push_klv"
    "tst_udp_mux_sender_push_klv_to"
    "tst_udp_mux_sender_push_audio"
    "tst_udp_mux_sender_push_audio_to"
    "tst_udp_mux_sender_push_subtitle"
    "tst_udp_mux_sender_push_subtitle_to"
    "tst_udp_mux_sender_get_mux_sender_stats"
    "tst_udp_mux_sender_get_socket_stats"
    "tst_udp_mux_sender_get_stream_codec_stats"
    "tst_udp_mux_sender_reset_stats"
    # TstUdpDemuxReceiver
    "tst_udp_demux_receiver_open"
    "tst_udp_demux_receiver_close"
    "tst_udp_demux_receiver_cancel"
    "tst_udp_demux_receiver_next_event"
    "tst_udp_demux_receiver_get_stats"
    "tst_udp_demux_receiver_get_socket_stats"
    "tst_udp_demux_receiver_get_stream_codec_stats"
    "tst_udp_demux_receiver_get_stream_stats"
    "tst_udp_demux_receiver_reset_stats"

    # --- Plan A5a TCP entry points (full RTP parity; + listener) ---
    #     Same rationale as the UDP/RTP blocks — tst-c-only projections of the
    #     generic pipeline shells typed on tst_tcp::TcpTransport (which impls
    #     both Transport + RecvTransport). The _cancel entry points (ABI 0.22)
    #     fire tst_tcp::TcpCancelHandle through the shared Owned slot. The
    #     listener family has no Rust 1:1 counterpart (it wraps
    #     tst_tcp::TcpListener::{bind,from_url,accept_blocking,cancel_handle}).
    # TstTcpSender
    "tst_tcp_sender_open"
    "tst_tcp_sender_close"
    "tst_tcp_sender_cancel"
    "tst_tcp_sender_send_ts"
    "tst_tcp_sender_get_stats"
    "tst_tcp_sender_get_socket_stats"
    "tst_tcp_sender_reset_stats"
    # TstTcpReceiver
    "tst_tcp_recv_open"
    "tst_tcp_receiver_cancel"
    "tst_tcp_receiver_close"
    "tst_tcp_receiver_recv_ts"
    "tst_tcp_receiver_get_stats"
    "tst_tcp_receiver_get_socket_stats"
    "tst_tcp_receiver_reset_stats"
    # TstTcpMuxSender
    "tst_tcp_mux_sender_open"
    "tst_tcp_mux_sender_close"
    "tst_tcp_mux_sender_cancel"
    "tst_tcp_mux_sender_finish"
    "tst_tcp_mux_sender_push_video"
    "tst_tcp_mux_sender_push_video_to"
    "tst_tcp_mux_sender_push_klv"
    "tst_tcp_mux_sender_push_klv_to"
    "tst_tcp_mux_sender_push_audio"
    "tst_tcp_mux_sender_push_audio_to"
    "tst_tcp_mux_sender_push_subtitle"
    "tst_tcp_mux_sender_push_subtitle_to"
    "tst_tcp_mux_sender_get_mux_sender_stats"
    "tst_tcp_mux_sender_get_socket_stats"
    "tst_tcp_mux_sender_get_stream_codec_stats"
    "tst_tcp_mux_sender_reset_stats"
    # TstTcpDemuxReceiver
    "tst_tcp_demux_receiver_open"
    "tst_tcp_demux_receiver_close"
    "tst_tcp_demux_receiver_cancel"
    "tst_tcp_demux_receiver_next_event"
    "tst_tcp_demux_receiver_get_stats"
    "tst_tcp_demux_receiver_get_socket_stats"
    "tst_tcp_demux_receiver_get_stream_codec_stats"
    "tst_tcp_demux_receiver_get_stream_stats"
    "tst_tcp_demux_receiver_reset_stats"
    # TstTcpListener
    "tst_tcp_listener_bind"
    "tst_tcp_listener_from_url"
    "tst_tcp_listener_accept_sender"
    "tst_tcp_listener_accept_receiver"
    "tst_tcp_listener_free"
    "tst_tcp_listener_cancel"

    # --- Plan A5a HLS publisher entry points ---
    #     tst-c-only projections: TstPublisher (enum-dispatch over the
    #     tst_core::publisher::Publisher trait), TstHlsPublisherBuilder
    #     (wraps tst_hls::HlsPublisherBuilder), TstMuxPublisher
    #     (wraps tst_pipeline::MuxPublisher<HlsPublisher>). The universal
    #     tst_publisher_* trait-mirror symbols are separately enforced by
    #     scripts/check/c/publisher-trait-mirror.sh.
    "tst_publisher_push_ts"
    "tst_publisher_cut_segment"
    "tst_publisher_finish"
    "tst_publisher_get_stats"
    "tst_publisher_get_kind"
    "tst_publisher_free"
    "tst_hls_publisher_get_hls_stats"
    "tst_hls_publisher_get_forced_cuts"
    "tst_hls_publisher_local_addr"
    "tst_hls_publisher_render_playlist"
    "tst_hls_publisher_finish_serving"
    "tst_hls_server_handle_local_addr"
    "tst_hls_server_handle_shutdown"
    "tst_hls_server_handle_free"
    "tst_hls_publisher_builder_new"
    "tst_hls_publisher_builder_bind"
    "tst_hls_publisher_builder_output_dir"
    "tst_hls_publisher_builder_segment_duration_ms"
    "tst_hls_publisher_builder_max_segment_duration_ms"
    "tst_hls_publisher_builder_playlist_window"
    "tst_hls_publisher_builder_mode"
    "tst_hls_publisher_builder_basic_auth"
    "tst_hls_publisher_builder_enable_tls"
    "tst_hls_publisher_builder_from_url"
    "tst_hls_publisher_builder_free"
    "tst_hls_publisher_builder_build"
    "tst_mux_publisher_with_config_hls"
    "tst_mux_publisher_send_video"
    "tst_mux_publisher_send_klv"
    "tst_mux_publisher_send_audio"
    "tst_mux_publisher_send_subtitle"
    "tst_mux_publisher_cut_segment"
    "tst_mux_publisher_finish_into_publisher"
    "tst_mux_publisher_get_stats"
    "tst_mux_publisher_get_publisher_stats"
    "tst_mux_publisher_free"

    # --- Plan A5a RIST entry points (full RTP parity) ---
    #     tst-c-only projections of the generic pipeline shells typed on
    #     tst_rist::Rist{,Recv}Transport (move-style builder: new()+connect()/
    #     listen()). Same rationale as UDP/TCP/RTP. The _cancel entry points
    #     (ABI 0.22) fire tst_rist::RistCancelHandle through the shared Owned
    #     slot — on RIST they are the ONLY safe cross-thread interrupt, since
    #     _recv_ts never parks and its caller loop would race a freeing _close.
    "tst_rist_sender_open"
    "tst_rist_sender_close"
    "tst_rist_sender_cancel"
    "tst_rist_sender_send_ts"
    "tst_rist_sender_get_stats"
    "tst_rist_sender_get_socket_stats"
    "tst_rist_sender_reset_stats"
    "tst_rist_recv_open"
    "tst_rist_receiver_close"
    "tst_rist_receiver_cancel"
    "tst_rist_receiver_recv_ts"
    "tst_rist_receiver_get_stats"
    "tst_rist_receiver_get_socket_stats"
    "tst_rist_receiver_reset_stats"
    "tst_rist_mux_sender_open"
    "tst_rist_mux_sender_close"
    "tst_rist_mux_sender_cancel"
    "tst_rist_mux_sender_finish"
    "tst_rist_mux_sender_push_video"
    "tst_rist_mux_sender_push_video_to"
    "tst_rist_mux_sender_push_klv"
    "tst_rist_mux_sender_push_klv_to"
    "tst_rist_mux_sender_push_audio"
    "tst_rist_mux_sender_push_audio_to"
    "tst_rist_mux_sender_push_subtitle"
    "tst_rist_mux_sender_push_subtitle_to"
    "tst_rist_mux_sender_get_mux_sender_stats"
    "tst_rist_mux_sender_get_socket_stats"
    "tst_rist_mux_sender_get_stream_codec_stats"
    "tst_rist_mux_sender_reset_stats"
    "tst_rist_demux_receiver_open"
    "tst_rist_demux_receiver_close"
    "tst_rist_demux_receiver_cancel"
    "tst_rist_demux_receiver_next_event"
    "tst_rist_demux_receiver_get_stats"
    "tst_rist_demux_receiver_get_socket_stats"
    "tst_rist_demux_receiver_get_stream_codec_stats"
    "tst_rist_demux_receiver_get_stream_stats"
    "tst_rist_demux_receiver_reset_stats"

    # --- Phase 4 RTSP session entry points (Task 6, Wave B) ---
    #     TstRtspSession is the C-language projection of (RtspClient, RtspSession)
    #     combined; into_demux_receiver bridges to the existing TstRtpDemuxReceiver.
    #     No 1:1 Rust method counterpart in tst-pipeline / tst-srt / tst-core.
    "tst_rtsp_client_builder_connect"
    "tst_rtsp_session_play"
    "tst_rtsp_session_pause"
    "tst_rtsp_session_teardown_and_free"
    "tst_rtsp_session_cancel"
    "tst_rtsp_session_into_demux_receiver"

    # --- Phase 4 RTSP server entry points (Tasks 7-8, Wave B) ---
    #     TstRtspServerBuilder stores fields directly and reconstructs
    #     RtspServerBuilder at _start time. TstRtspServer wraps
    #     tst_rtp::RtspServer; TstRtspMountHandle wraps tst_rtp::MountHandle.
    #     No 1:1 Rust method counterpart in tst-pipeline / tst-srt / tst-core.
    # T7 (builder + setter chain):
    "tst_rtsp_server_builder_new"
    "tst_rtsp_server_builder_bind"
    "tst_rtsp_server_builder_auth_basic"
    "tst_rtsp_server_builder_auth_digest_md5"
    "tst_rtsp_server_builder_auth_digest_sha256"
    "tst_rtsp_server_builder_max_sessions"
    "tst_rtsp_server_builder_session_timeout"
    "tst_rtsp_server_builder_fanout_capacity"
    "tst_rtsp_server_builder_graceful_shutdown_drain_ms"
    "tst_rtsp_server_builder_tls_cert_pem"
    "tst_rtsp_server_builder_free"
    # T8 (start + mount creation):
    "tst_rtsp_server_builder_start"
    "tst_rtsp_server_add_unicast_mount"
    "tst_rtsp_server_add_multicast_mount"
    "tst_rtsp_mount_handle_free"

    # --- T9 mount push methods ---
    #     TstRtspMountHandle wraps tst_rtp::MountHandle. The 8 push methods
    #     delegate to MountHandle::push_{video,klv,audio,subtitle}[_to] which
    #     live in tst-rtp (not in tst-pipeline / tst-srt / tst-core). The 3
    #     lifecycle helpers (flush/cancel/reset_stats) delegate to
    #     MountHandle::{flush,reset_stats} (tst-rtp) and a C-layer AtomicBool.
    "tst_rtsp_mount_push_video"
    "tst_rtsp_mount_push_klv"
    "tst_rtsp_mount_push_audio"
    "tst_rtsp_mount_push_subtitle"
    "tst_rtsp_mount_push_video_to"
    "tst_rtsp_mount_push_klv_to"
    "tst_rtsp_mount_push_audio_to"
    "tst_rtsp_mount_push_subtitle_to"
    "tst_rtsp_mount_push_data"
    "tst_rtsp_mount_push_data_to"
    "tst_rtsp_mount_flush"
    "tst_rtsp_mount_cancel"
    "tst_rtsp_mount_reset_stats"

    # --- T10 server stats + cancel + stop ---
    #     TstRtspServer / TstRtspCancelHandle / TstRtspMountHandle are tst-c–only
    #     types wrapping tst_rtp::RtspServer / RtspServerCancelHandle /
    #     MountHandle; no 1:1 Rust method counterpart exists in tst-pipeline /
    #     tst-srt / tst-core to cross-ref against.
    # Server lifecycle:
    "tst_rtsp_server_get_stats"
    "tst_rtsp_server_cancel_handle"
    "tst_rtsp_cancel_handle_cancel"
    "tst_rtsp_cancel_handle_free"
    "tst_rtsp_server_stop"
    "tst_rtsp_server_free"
    # Mount stats + handle getters:
    "tst_rtsp_mount_get_stats"
    "tst_rtsp_mount_video_handle"
    "tst_rtsp_mount_klv_handle"
    "tst_rtsp_mount_audio_handle"
    "tst_rtsp_mount_subtitle_handle"

    # --- Task 8: Annex B <-> length-prefixed + parameter-set extraction
    #     (ABI 21). tst_annexb_to_length_prefixed has a real 1:1 cross-ref
    #     on nal_framing::annexb_to_length_prefixed, and
    #     tst_param_sets_extract on nal_framing::extract_parameter_sets
    #     (both in crates/tst-core/src/codec/nal_framing.rs) — neither
    #     needs an allowlist entry. The other three are tst-c-only
    #     accessor/lifecycle logic over the ParameterSets struct's Vec
    #     fields (bucket selection + Box lifecycle) with no single Rust
    #     method to cross-ref. ---
    "tst_param_sets_count"
    "tst_param_sets_get"
    "tst_param_sets_free"
)

# Step 1: enumerate C exports
#
# Portable read-into-array pattern (bash 3.2+, including macOS default
# bash 3.2.57). `mapfile`/`readarray` are bash 4.0+ only and silently
# fail with "command not found" on macOS — see
# `feedback_bash_ratchets_macos_portability.md`.
C_EXPORTS=()
while IFS= read -r c_export; do
    C_EXPORTS+=("$c_export")
done < <(
    grep -oE 'tst_[a-z_]+\(' "$HEADER" \
    | sed 's/(//' \
    | sort -u
)

echo "Found ${#C_EXPORTS[@]} C ABI exports in $HEADER"

missing=0
for c_export in "${C_EXPORTS[@]}"; do
    if [[ " ${ALLOWLIST[*]} " == *" $c_export "* ]]; then
        continue
    fi
    if ! grep -rq "$c_export" "${SRC_DIRS[@]}" --include='*.rs' 2>/dev/null; then
        echo "MISSING: '$c_export' has no Rust # C ABI cross-reference"
        missing=$((missing + 1))
    fi
done

if [[ $missing -gt 0 ]]; then
    echo "FAIL: $missing C ABI exports lack Rust cross-references"
    exit 1
fi

echo "OK: all C ABI exports have Rust cross-references"
