//! The stats records: native Python classes and Java records of counters,
//! built from the Rust snapshots and handed to the hosts (Rust to host only).
//! They are declared in the crate that owns the counters, so the registry
//! names records of tst-core and of the transport and pipeline crates.

use crate::schema::{Direction, Entry, PyShape};

/// One stats record: a native class, to the host only.
fn stats(
    family: &'static str,
    py_module: &'static str,
    java_pkg: &'static str,
    rust_path: &'static str,
    schema: tst_core::binding_schema::RecordSchema,
) -> Entry {
    let mut e = Entry::new(
        family,
        py_module,
        java_pkg,
        Direction::ToHost,
        rust_path,
        "",
        schema,
    );
    e.shape = PyShape::Native;
    e
}

/// A record only Java exposes; Python hands the same numbers over as a dict.
fn java_only(mut e: Entry) -> Entry {
    e.python = false;
    e
}

/// A record only Python exposes; Java reads the shared socket stats.
fn python_only(mut e: Entry) -> Entry {
    e.jvm = false;
    e
}

/// The stats records, one family per Python module.
pub fn stats_records() -> Vec<Entry> {
    use tst_core::mpegts::demux::DemuxerStats;
    use tst_core::mpegts::mux::MuxerStats;
    use tst_core::transport::SocketStats;
    use tst_hls::HlsStats;
    use tst_pipeline::ext::pairing::PairerStats;
    use tst_pipeline::{ManagedTransportStats, MuxPublisherStats};
    use tst_rist::RistStats;
    use tst_rtp::{H264DepayStats, MountStats, PublishMountStats, RtpStats, ServerStats};
    use tst_srt::SrtStats;
    use tst_tcp::TcpStats;
    use tst_udp::UdpStats;
    use tst_webrtc::{MountStats as WhepMountStats, ServerStats as WhepServerStats};

    // The hand-written counters printed a few of their fields; so do these.
    let mut socket = stats(
        "stats_shells",
        "tstrans.shells",
        "org/tstrans/shells",
        "tst_core::transport::SocketStats",
        SocketStats::BINDING_SCHEMA,
    );
    socket.py_repr = Some((
        "SocketStats(bytes_sent={}, packets_sent={}, bytes_received={}, packets_received={}, rtt_us={})",
        &[
            "self.bytes_sent",
            "self.packets_sent",
            "self.bytes_received",
            "self.packets_received",
            "self.rtt_us",
        ],
    ));

    let mut muxer = stats(
        "stats_mpegts",
        "tstrans.mpegts",
        "org/tstrans/mpegts",
        "tst_core::mpegts::mux::MuxerStats",
        MuxerStats::BINDING_SCHEMA,
    );
    muxer.py_repr = Some((
        "MuxerStats(ts_packets_emitted={}, ts_bytes_emitted={}, programs_configured={}, subtitle_streams_configured={})",
        &[
            "self.ts_packets_emitted",
            "self.ts_bytes_emitted",
            "self.programs_configured",
            "self.subtitle_streams_configured",
        ],
    ));
    let demuxer = java_only(stats(
        "stats_mpegts",
        "tstrans.mpegts",
        "org/tstrans/mpegts",
        "tst_core::mpegts::demux::DemuxerStats",
        DemuxerStats::BINDING_SCHEMA,
    ));

    let mut srt = stats(
        "stats_srt",
        "tstrans.srt",
        "org/tstrans/srt",
        "tst_srt::SrtStats",
        SrtStats::BINDING_SCHEMA,
    );
    srt.host_name = "SrtStats";
    srt.py_rename = &[("rtt", "rtt_us")];
    srt.py_repr = Some((
        "SrtStats(bytes_sent={}, packets_sent={}, bytes_received={}, packets_received={}, rtt_us={}, mbps_estimated_bandwidth={:.3})",
        &[
            "self.bytes_sent",
            "self.packets_sent",
            "self.bytes_received",
            "self.packets_received",
            "self.rtt_us",
            "self.mbps_estimated_bandwidth",
        ],
    ));
    let mut managed = stats(
        "stats_srt",
        "tstrans.srt",
        "org/tstrans/srt",
        "tst_pipeline::ManagedTransportStats",
        ManagedTransportStats::BINDING_SCHEMA,
    );
    managed.py_repr = Some((
        "ManagedTransportStats(reconnect_attempts={}, reconnect_successes={}, gap_len={}, gap_messages_dropped={}, gap_bytes_dropped={}, reconnecting={})",
        &[
            "self.reconnect_attempts",
            "self.reconnect_successes",
            "self.gap_len",
            "self.gap_messages_dropped",
            "self.gap_bytes_dropped",
            "if self.reconnecting { \"True\" } else { \"False\" }",
        ],
    ));

    let pairer = java_only(stats(
        "stats_pipeline",
        "tstrans.pipeline",
        "org/tstrans/pipeline",
        "tst_pipeline::ext::pairing::PairerStats",
        PairerStats::BINDING_SCHEMA,
    ));

    let mut mux_publisher = stats(
        "stats_hls",
        "tstrans.hls",
        "org/tstrans/hls",
        "tst_pipeline::MuxPublisherStats",
        MuxPublisherStats::BINDING_SCHEMA,
    );
    mux_publisher.py_repr = Some((
        "MuxPublisherStats(bytes_pushed={}, drain_calls={}, cut_calls={})",
        &["self.bytes_pushed", "self.drain_calls", "self.cut_calls"],
    ));
    let mut hls = stats(
        "stats_hls",
        "tstrans.hls",
        "org/tstrans/hls",
        "tst_hls::HlsStats",
        HlsStats::BINDING_SCHEMA,
    );
    hls.py_ctor = true;
    hls.py_repr = Some((
        "HlsStats(segments_written={}, bytes_pushed_total={}, open_segment_bytes={}, forced_cuts={})",
        &[
            "self.segments_written",
            "self.bytes_pushed_total",
            "self.open_segment_bytes",
            "self.forced_cuts",
        ],
    ));

    let mut tcp = python_only(stats(
        "stats_tcp",
        "tstrans.tcp",
        "org/tstrans/tcp",
        "tst_tcp::TcpStats",
        TcpStats::BINDING_SCHEMA,
    ));
    tcp.host_name = "SocketStats";
    tcp.py_repr = Some((
        "SocketStats(bytes_sent={}, bytes_received={}, send_calls={}, recv_calls={})",
        &[
            "self.bytes_sent",
            "self.bytes_received",
            "self.send_calls",
            "self.recv_calls",
        ],
    ));
    let mut udp = python_only(stats(
        "stats_udp",
        "tstrans.udp",
        "org/tstrans/udp",
        "tst_udp::UdpStats",
        UdpStats::BINDING_SCHEMA,
    ));
    udp.host_name = "SocketStats";
    udp.py_repr = Some((
        "SocketStats(datagrams_sent={}, bytes_sent={}, datagrams_received={}, bytes_received={})",
        &[
            "self.datagrams_sent",
            "self.bytes_sent",
            "self.datagrams_received",
            "self.bytes_received",
        ],
    ));
    let mut rist = python_only(stats(
        "stats_rist",
        "tstrans.rist",
        "org/tstrans/rist",
        "tst_rist::RistStats",
        RistStats::BINDING_SCHEMA,
    ));
    rist.py_rename = &[("bandwidth_kbps", "current_bandwidth_kbps")];
    rist.py_repr = Some((
        "RistStats(packets_sent={}, packets_received={}, rtt_us={}, current_bandwidth_kbps={})",
        &[
            "self.packets_sent",
            "self.packets_received",
            "self.rtt_us",
            "self.current_bandwidth_kbps",
        ],
    ));

    let mut rtp = stats(
        "stats_rtp",
        "tstrans.rtp",
        "org/tstrans/rtp",
        "tst_rtp::RtpStats",
        RtpStats::BINDING_SCHEMA,
    );
    rtp.py_repr = Some((
        "RtpStats(malformed_packets={}, source_rejected={})",
        &["self.malformed_packets", "self.source_rejected"],
    ));
    let mut depay = stats(
        "stats_rtp",
        "tstrans.rtp",
        "org/tstrans/rtp",
        "tst_rtp::H264DepayStats",
        H264DepayStats::BINDING_SCHEMA,
    );
    depay.host_name = "H264DepayStats";
    depay.py_repr = Some((
        "H264DepayStats(aus_emitted={}, aus_dropped={}, seq_gaps={}, ssrc_changes={}, parameter_set_updates={})",
        &[
            "self.aus_emitted",
            "self.aus_dropped",
            "self.seq_gaps",
            "self.ssrc_changes",
            "self.parameter_set_updates",
        ],
    ));
    let mut server = stats(
        "stats_rtp",
        "tstrans.rtp",
        "org/tstrans/rtp",
        "tst_rtp::ServerStats",
        ServerStats::BINDING_SCHEMA,
    );
    server.py_repr = Some((
        "ServerStats(active_sessions={}, mounts={}, total_rtp_packets_sent={}, total_rtp_bytes_sent={}, active_publishers={}, total_rtp_packets_received={}, total_rtp_bytes_received={})",
        &[
            "self.active_sessions",
            "self.mounts",
            "self.total_rtp_packets_sent",
            "self.total_rtp_bytes_sent",
            "self.active_publishers",
            "self.total_rtp_packets_received",
            "self.total_rtp_bytes_received",
        ],
    ));
    let mut mount = stats(
        "stats_rtp",
        "tstrans.rtp",
        "org/tstrans/rtp",
        "tst_rtp::MountStats",
        MountStats::BINDING_SCHEMA,
    );
    mount.py_repr = Some((
        "MountStats(bytes_pushed={}, packets_pushed={}, peer_count={}, frames_dropped_total={})",
        &[
            "self.bytes_pushed",
            "self.packets_pushed",
            "self.peer_count",
            "self.frames_dropped_total",
        ],
    ));
    let mut publish = stats(
        "stats_rtp",
        "tstrans.rtp",
        "org/tstrans/rtp",
        "tst_rtp::PublishMountStats",
        PublishMountStats::BINDING_SCHEMA,
    );
    publish.py_repr = Some((
        "PublishMountStats(rtp_packets_received={}, bytes_received={}, malformed_packets={}, source_rejected={}, frames_emitted={}, frames_dropped_app={}, frames_dropped_readers={}, aus_emitted={}, aus_dropped={}, aus_reordered={}, klv_units_emitted={}, klv_units_dropped={}, alignment={}, alignment_steps={}, ssrc_changes={}, generation={}, peer_count={})",
        &[
            "self.rtp_packets_received",
            "self.bytes_received",
            "self.malformed_packets",
            "self.source_rejected",
            "self.frames_emitted",
            "self.frames_dropped_app",
            "self.frames_dropped_readers",
            "self.aus_emitted",
            "self.aus_dropped",
            "self.aus_reordered",
            "self.klv_units_emitted",
            "self.klv_units_dropped",
            "self.alignment.name()",
            "self.alignment_steps",
            "self.ssrc_changes",
            "self.generation",
            "self.peer_count",
        ],
    ));

    let mut whep_server = stats(
        "stats_webrtc",
        "tstrans.webrtc",
        "org/tstrans/webrtc",
        "tst_webrtc::ServerStats",
        WhepServerStats::BINDING_SCHEMA,
    );
    whep_server.py_repr = Some((
        "ServerStats(mounts={}, active_sessions={}, sessions_total={}, sessions_rejected_cap={}, sessions_rejected_codec={}, sessions_rejected_auth={}, sessions_reaped_ice_timeout={}, sessions_reaped_disconnected={}, rtp_packets_sent={}, rtp_bytes_sent={}, datagrams_unrouted={})",
        &[
            "self.mounts",
            "self.active_sessions",
            "self.sessions_total",
            "self.sessions_rejected_cap",
            "self.sessions_rejected_codec",
            "self.sessions_rejected_auth",
            "self.sessions_reaped_ice_timeout",
            "self.sessions_reaped_disconnected",
            "self.rtp_packets_sent",
            "self.rtp_bytes_sent",
            "self.datagrams_unrouted",
        ],
    ));
    let mut whep_mount = stats(
        "stats_webrtc",
        "tstrans.webrtc",
        "org/tstrans/webrtc",
        "tst_webrtc::MountStats",
        WhepMountStats::BINDING_SCHEMA,
    );
    whep_mount.py_repr = Some((
        "MountStats(frames_pushed={}, keyframes_pushed={}, keyframe_flag_mismatch={}, viewers={}, viewers_waiting_keyframe={}, klv_records_pushed={}, klv_records_dropped_unbound={}, klv_sei_inserted={}, klv_sei_dropped_oversize={}, klv_dc_messages_sent={}, klv_dc_dropped_oversize={}, klv_dc_dropped_viewer={}, audio_frames_pushed={}, audio_frames_dropped={}, plis_received={}, nacks_received={}, viewer_packets_dropped_queue_full={}, rtp_clock_rebases={}, frames_pts_reordered={}, demux_nonconformant={})",
        &[
            "self.frames_pushed",
            "self.keyframes_pushed",
            "self.keyframe_flag_mismatch",
            "self.viewers",
            "self.viewers_waiting_keyframe",
            "self.klv_records_pushed",
            "self.klv_records_dropped_unbound",
            "self.klv_sei_inserted",
            "self.klv_sei_dropped_oversize",
            "self.klv_dc_messages_sent",
            "self.klv_dc_dropped_oversize",
            "self.klv_dc_dropped_viewer",
            "self.audio_frames_pushed",
            "self.audio_frames_dropped",
            "self.plis_received",
            "self.nacks_received",
            "self.viewer_packets_dropped_queue_full",
            "self.rtp_clock_rebases",
            "self.frames_pts_reordered",
            "self.demux_nonconformant",
        ],
    ));

    vec![
        socket,
        muxer,
        demuxer,
        srt,
        managed,
        pairer,
        mux_publisher,
        hls,
        tcp,
        udp,
        rist,
        rtp,
        depay,
        server,
        mount,
        publish,
        whep_server,
        whep_mount,
    ]
}
