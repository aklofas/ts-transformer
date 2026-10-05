//! `repr(C)` mirrors of the application-level stats types in
//! `tst-core` / `tst-pipeline`. Layouts match the public C ABI declared in
//! `bindings/c/include/tstrans.h` and exercised by `tests/smoke.c`.
//!
//! All wrapping structs use a fixed-size
//! `[TstStreamStats; TST_STATS_MAX_STREAMS]` array +
//! `per_stream_count` + `per_stream_truncated` flag rather than a
//! heap-allocated list. That keeps the ABI allocator-free; callers
//! stack-allocate the stats struct and pass a pointer.

use crate::c_types::c_char;

/// Maximum number of per-stream entries in any stats struct exposed at
/// the C ABI. Rarely exceeded — multi-stream `mpegts::mux` caps at
/// 16 video + 16 KLV = 32, leaving 32 slots of headroom for receiver
/// PSI + observed streams.
pub const TST_STATS_MAX_STREAMS: usize = 64;

/// `repr(C)` mirror of `tst_core::mpegts::StreamStats`. Size 96 B.
///
/// Layout (offsets):
///   0: items (u64, 8 B)
///   8: bytes (u64, 8 B)
///  16: discontinuities (u64, 8 B)
///  24: pid (u16, 2 B)
///  26: stream_type (u8, 1 B)
///  27: _pad (3 B, alignment bridge)
///  30: program_number (u16, 2 B)
///  32: label ([c_char; 64], 64 B)
/// Total: 96 B — identical to the pre-program_number layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TstStreamStats {
    pub items: u64,
    pub bytes: u64,
    pub discontinuities: u64,
    pub pid: u16,
    pub stream_type: u8,
    /// Alignment padding bridging stream_type → program_number.
    pub _pad: [u8; 3],
    /// Program number from the PAT that owns this stream. 0 for PSI PIDs.
    pub program_number: u16,
    /// NUL-terminated UTF-8. `label[0]==0` means None. Truncated at 63
    /// bytes (first 63 + NUL).
    pub label: [c_char; 64],
}

const _TST_STREAM_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstStreamStats>() == 96,
    "TstStreamStats must be 96 bytes"
);

impl Default for TstStreamStats {
    fn default() -> Self {
        Self {
            items: 0,
            bytes: 0,
            discontinuities: 0,
            pid: 0,
            stream_type: 0,
            _pad: [0; 3],
            program_number: 0,
            label: [0; 64],
        }
    }
}

/// Copy `src` into the C struct, truncating label to 63 bytes + NUL.
pub fn fill_stream_stats(dst: &mut TstStreamStats, src: &tst_core::mpegts::StreamStats) {
    dst.items = src.items;
    dst.bytes = src.bytes;
    dst.discontinuities = src.discontinuities;
    dst.pid = src.pid;
    dst.stream_type = src.stream_type.as_byte();
    dst._pad = [0; 3];
    dst.program_number = src.program_number;
    dst.label = [0; 64];
    if let Some(s) = &src.label {
        let bytes = s.as_bytes();
        let n = bytes.len().min(63);
        for (i, b) in bytes[..n].iter().enumerate() {
            dst.label[i] = *b as c_char;
        }
        dst.label[n] = 0;
    }
}

/// `repr(C)` mirror of `tst_pipeline::RawSendStats`. Size 16 B.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstRawSendStats {
    pub bytes_sent: u64,
    pub packets_sent: u64,
}

const _TST_RAW_SEND_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstRawSendStats>() == 16,
    "TstRawSendStats must be 16 bytes (2 × u64)"
);

impl From<&tst_pipeline::RawSendStats> for TstRawSendStats {
    fn from(s: &tst_pipeline::RawSendStats) -> Self {
        Self {
            bytes_sent: s.bytes_sent,
            packets_sent: s.packets_sent,
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::SenderStats`. Size 32 B.
///
/// Application-level counters for the TS-aligned send shell. Caller
/// passes a pointer to a stack-allocated struct; `tst_sender_get_stats`
/// (SRT) or `tst_rtp_sender_get_stats` (RTP) fills it in.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstSenderStats {
    pub bytes_pushed: u64,
    pub bytes_skipped_for_sync: u64,
    pub resync_events: u64,
    pub packets_sent: u64,
}

const _TST_SENDER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstSenderStats>() == 32,
    "TstSenderStats must be 32 bytes (4 × u64)"
);

impl From<&tst_pipeline::SenderStats> for TstSenderStats {
    fn from(s: &tst_pipeline::SenderStats) -> Self {
        Self {
            bytes_pushed: s.bytes_pushed,
            bytes_skipped_for_sync: s.bytes_skipped_for_sync,
            resync_events: s.resync_events,
            packets_sent: s.packets_sent,
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::RawRecvStats`. Size 16 B.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstRawRecvStats {
    pub bytes_received: u64,
    pub packets_received: u64,
}

const _TST_RAW_RECV_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstRawRecvStats>() == 16,
    "TstRawRecvStats must be 16 bytes (2 × u64)"
);

#[cfg(feature = "std")]
impl From<&tst_pipeline::RawRecvStats> for TstRawRecvStats {
    fn from(s: &tst_pipeline::RawRecvStats) -> Self {
        Self {
            bytes_received: s.bytes_received,
            packets_received: s.packets_received,
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::ReceiverStats`. Size 32 B.
///
/// Application-level counters for the TS-aligned receive shell. Mirrors
/// the layout of `tst_pipeline::ReceiverStats`:
/// * `bytes_received` / `packets_received` — application-level totals
///   (one 188-byte packet per `_recv_packet` success).
/// * `bytes_skipped_for_sync` / `resync_events` — sync-recovery state
///   from the underlying `tst_pipeline::receiver::sync::Syncer`. Non-zero
///   `resync_events` indicates the syncer hit HUNT/VERIFY mid-stream
///   (either initial lock-on or re-lock after corruption).
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstReceiverStats {
    pub bytes_received: u64,
    pub bytes_skipped_for_sync: u64,
    pub resync_events: u64,
    pub packets_received: u64,
}

const _TST_RECEIVER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstReceiverStats>() == 32,
    "TstReceiverStats must be 32 bytes (4 × u64)"
);

#[cfg(feature = "std")]
impl From<&tst_pipeline::ReceiverStats> for TstReceiverStats {
    fn from(s: &tst_pipeline::ReceiverStats) -> Self {
        Self {
            bytes_received: s.bytes_received,
            bytes_skipped_for_sync: s.bytes_skipped_for_sync,
            resync_events: s.resync_events,
            packets_received: s.packets_received,
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::DemuxReceiverStats`. Size 48 B.
///
/// Application-level counters for the demux receive shell. Faithfully
/// mirrors `tst_pipeline::DemuxReceiverStats` — six u64 fields:
/// * `bytes_received` / `packets_received` — application-level totals
///   from the inner `Receiver` (one 188-byte packet per success).
/// * `program_maps_seen` / `pmt_versions_seen` — PSI topology counters.
///   `program_maps_seen` increments on every PMT emission; `pmt_versions_seen`
///   only on a `version_number` bump (PMT churn detector).
/// * `discontinuities` — sum across all PIDs of `DemuxEvent::Discontinuity`
///   emissions (continuity-counter jumps, PES oversize, etc).
/// * `nonconformant` — sum across all PIDs of `DemuxEvent::NonConformant`
///   emissions (one code per issue variant; see `tst_event_t.u.nonconformant.issue_code`).
///
/// NOTE: sync-recovery counters (`bytes_skipped_for_sync`, `resync_events`)
/// are deliberately absent — they live only on the inner `Receiver`'s
/// `ReceiverStats` (surfaced via `TstReceiverStats`). Adding them
/// here would mis-label the data source. Consumers needing them run
/// a `tst_receiver_t` instead of a `tst_demux_receiver_t`.
///
/// The per-PID `BTreeMap<u16, StreamStats>` from `DemuxReceiverStats`
/// is NOT included on this struct; it ships separately via
/// `tst_demux_receiver_get_stream_stats` returning a borrowed
/// `(*const TstStreamStats, size_t)` pair per design §4.5.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstDemuxReceiverStats {
    pub bytes_received: u64,
    pub packets_received: u64,
    pub program_maps_seen: u64,
    pub pmt_versions_seen: u64,
    pub discontinuities: u64,
    pub nonconformant: u64,
}

const _TST_DEMUX_RECEIVER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstDemuxReceiverStats>() == 48,
    "TstDemuxReceiverStats must be 48 bytes (6 × u64)"
);

#[cfg(feature = "std")]
impl From<&tst_pipeline::DemuxReceiverStats> for TstDemuxReceiverStats {
    fn from(s: &tst_pipeline::DemuxReceiverStats) -> Self {
        Self {
            bytes_received: s.bytes_received,
            packets_received: s.packets_received,
            program_maps_seen: s.program_maps_seen,
            pmt_versions_seen: s.pmt_versions_seen,
            discontinuities: s.discontinuities,
            nonconformant: s.nonconformant,
        }
    }
}

/// `repr(C)` mirror of `tst_core::transport::SocketStats`. Size 120 B.
///
/// Layout (offsets in bytes — verified by the `_TST_SOCKET_STATS_SIZE`
/// const assertion below):
///   0: rtt_us              (u32, 4 B)
///   4: send_buffer_packets (u32, 4 B)
///   8: recv_buffer_packets (u32, 4 B)
///  12: _pad                (u32, 4 B, alignment bridge to u64 below)
///  16: send_bandwidth_bps  (u64, 8 B)
///  24: recv_bandwidth_bps  (u64, 8 B)
///  32: link_bandwidth_bps  (u64, 8 B)
///  40: bytes_sent          (u64, 8 B)
///  48: packets_sent        (u64, 8 B)
///  56: bytes_received      (u64, 8 B)
///  64: packets_received    (u64, 8 B)
///  72: bytes_lost_recv     (u64, 8 B)
///  80: packets_lost_recv   (u64, 8 B)
///  88: packets_lost_send   (u64, 8 B)
///  96: packets_retransmitted (u64, 8 B)
/// 104: packets_dropped_send  (u64, 8 B)
/// 112: packets_dropped_recv  (u64, 8 B)
/// Total: 120 B.
///
/// All bandwidth fields are bits per second; RTT is microseconds;
/// buffer-depth fields are in packets. See
/// `tst_core::transport::SocketStats` rustdoc for the libsrt source
/// mappings.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstSocketStats {
    pub rtt_us: u32,
    pub send_buffer_packets: u32,
    pub recv_buffer_packets: u32,
    /// Alignment padding bridging the u32 prefix to the u64 tail.
    pub _pad: u32,
    pub send_bandwidth_bps: u64,
    pub recv_bandwidth_bps: u64,
    pub link_bandwidth_bps: u64,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub bytes_lost_recv: u64,
    pub packets_lost_recv: u64,
    pub packets_lost_send: u64,
    pub packets_retransmitted: u64,
    pub packets_dropped_send: u64,
    pub packets_dropped_recv: u64,
}

const _TST_SOCKET_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstSocketStats>() == 120,
    "TstSocketStats must be 120 bytes (3×u32 + 1×u32 pad + 13×u64)"
);

impl From<&tst_core::transport::SocketStats> for TstSocketStats {
    fn from(s: &tst_core::transport::SocketStats) -> Self {
        Self {
            rtt_us: s.rtt_us,
            send_buffer_packets: s.send_buffer_packets,
            recv_buffer_packets: s.recv_buffer_packets,
            _pad: 0,
            send_bandwidth_bps: s.send_bandwidth_bps,
            recv_bandwidth_bps: s.recv_bandwidth_bps,
            link_bandwidth_bps: s.link_bandwidth_bps,
            bytes_sent: s.bytes_sent,
            packets_sent: s.packets_sent,
            bytes_received: s.bytes_received,
            packets_received: s.packets_received,
            bytes_lost_recv: s.bytes_lost_recv,
            packets_lost_recv: s.packets_lost_recv,
            packets_lost_send: s.packets_lost_send,
            packets_retransmitted: s.packets_retransmitted,
            packets_dropped_send: s.packets_dropped_send,
            packets_dropped_recv: s.packets_dropped_recv,
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::ManagedTransportStats` — reconnect
/// telemetry for a managed (auto-reconnecting) sender. Returned by
/// `tst_managed_{sender,mux_sender,raw_sender}_get_reconnect_stats`. Size 48 B.
///
/// Layout (offsets in bytes — verified by the
/// `_TST_MANAGED_TRANSPORT_STATS_SIZE` const assertion below):
///   0: reconnect_attempts   (u64, 8 B)
///   8: reconnect_successes  (u64, 8 B)
///  16: gap_len              (u64, 8 B)
///  24: gap_messages_dropped (u64, 8 B)
///  32: gap_bytes_dropped    (u64, 8 B)
///  40: reconnecting         (bool, 1 B)
///  41: _pad                 ([u8; 7], 7 B, alignment tail)
/// Total: 48 B.
///
/// Fields:
/// - `reconnect_attempts` — total `factory()` invocations across all
///   reconnect cycles (successful + failed).
/// - `reconnect_successes` — factory calls that returned a transport
///   successfully installed as the new inner connection.
/// - `gap_len` — messages currently queued in the gap buffer, awaiting
///   drain once the inner transport reconnects.
/// - `gap_messages_dropped` / `gap_bytes_dropped` — messages/bytes evicted
///   by `DropOldest` (plus oversized-after-reconnect drops discovered
///   during drain).
/// - `reconnecting` — true while a background reconnect worker is active
///   (`ReconnectMode::Background` only; always `false` in `Blocking` mode).
///
/// All counters are mode-agnostic: `Blocking` mode's inline reconnect loop
/// bumps `reconnect_attempts`/`reconnect_successes` exactly like
/// `Background` mode's worker does.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstManagedTransportStats {
    pub reconnect_attempts: u64,
    pub reconnect_successes: u64,
    pub gap_len: u64,
    pub gap_messages_dropped: u64,
    pub gap_bytes_dropped: u64,
    pub reconnecting: bool,
    /// Alignment padding after the `bool` tail field.
    pub _pad: [u8; 7],
}

const _TST_MANAGED_TRANSPORT_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstManagedTransportStats>() == 48,
    "TstManagedTransportStats must be 48 bytes (5×u64 + bool + 7 pad)"
);

// `tst_pipeline::ManagedTransportStats` is itself `#[cfg(feature = "std")]`
// (the background-reconnect machinery it describes is threading-based) —
// this impl is the only thing in this file that touches it, so it alone
// needs the gate; the plain-integer `TstManagedTransportStats` struct
// above stays available (and its size assert stays checked) under no_std.
#[cfg(feature = "std")]
impl From<&tst_pipeline::ManagedTransportStats> for TstManagedTransportStats {
    fn from(s: &tst_pipeline::ManagedTransportStats) -> Self {
        Self {
            reconnect_attempts: s.reconnect_attempts,
            reconnect_successes: s.reconnect_successes,
            gap_len: s.gap_len,
            gap_messages_dropped: s.gap_messages_dropped,
            gap_bytes_dropped: s.gap_bytes_dropped,
            reconnecting: s.reconnecting,
            _pad: [0; 7],
        }
    }
}

/// `repr(C)` mirror of `tst_core::mpegts::mux::MuxerStats`. Size 6176 B
/// (2×u64 + 3×u32 + 4 B alignment pad + 64 × `TstStreamStats`); see the
/// `_TST_MUXER_STATS_SIZE` const assertion below.
#[repr(C)]
pub struct TstMuxerStats {
    pub ts_packets_emitted: u64,
    pub ts_bytes_emitted: u64,
    /// Number of programs (PAT entries) in this muxer's configuration.
    pub programs_configured: u32,
    pub per_stream_count: u32,
    pub per_stream_truncated: u32,
    pub per_stream: [TstStreamStats; TST_STATS_MAX_STREAMS],
}

const _TST_MUXER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstMuxerStats>() == 6176,
    "TstMuxerStats must be 6176 bytes (2×u64 + 3×u32 + 4 pad + 64 × TstStreamStats)"
);

impl Default for TstMuxerStats {
    fn default() -> Self {
        Self {
            ts_packets_emitted: 0,
            ts_bytes_emitted: 0,
            programs_configured: 0,
            per_stream_count: 0,
            per_stream_truncated: 0,
            per_stream: [TstStreamStats::default(); TST_STATS_MAX_STREAMS],
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::MuxSenderStats`. Size 6192 B
/// (4×u64 + 3×u32 + 4 B alignment pad + 64 × `TstStreamStats`); see
/// the `_TST_MUX_SENDER_STATS_SIZE` const assertion below.
#[repr(C)]
pub struct TstMuxSenderStats {
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub pending_bytes_queued: u64,
    pub pending_chunks_queued: u64,
    /// Number of programs (PAT entries) in the muxer configuration.
    pub programs_configured: u32,
    pub per_stream_count: u32,
    pub per_stream_truncated: u32,
    pub per_stream: [TstStreamStats; TST_STATS_MAX_STREAMS],
}

const _TST_MUX_SENDER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstMuxSenderStats>() == 6192,
    "TstMuxSenderStats must be 6192 bytes (4×u64 + 3×u32 + 4 pad + 64 × TstStreamStats)"
);

impl Default for TstMuxSenderStats {
    fn default() -> Self {
        Self {
            bytes_sent: 0,
            packets_sent: 0,
            pending_bytes_queued: 0,
            pending_chunks_queued: 0,
            programs_configured: 0,
            per_stream_count: 0,
            per_stream_truncated: 0,
            per_stream: [TstStreamStats::default(); TST_STATS_MAX_STREAMS],
        }
    }
}

/// Convert a Rust [`tst_pipeline::MuxSenderStats`] to its `repr(C)` mirror.
///
/// Callers own the `TstMuxSenderStats` by value; the per-stream array is
/// deep-copied from the BTreeMap by `fill_per_stream`. There is no lifetime
/// tie between the returned struct and the input.
// Gated on the union of consuming transport features (mirrors the
// transport_impls module gate): with all transports off nothing calls this
// and default-build clippy rejects the dead code.
#[cfg(any(
    feature = "srt",
    feature = "rtp",
    feature = "udp",
    feature = "tcp",
    feature = "rist"
))]
pub(crate) fn mux_sender_stats_to_c(stats: &tst_pipeline::MuxSenderStats) -> TstMuxSenderStats {
    let mut per_stream = [TstStreamStats::default(); TST_STATS_MAX_STREAMS];
    let (per_stream_count, truncated) = fill_per_stream(&mut per_stream, &stats.per_stream);
    TstMuxSenderStats {
        bytes_sent: stats.bytes_sent,
        packets_sent: stats.packets_sent,
        pending_bytes_queued: stats.pending_bytes_queued,
        pending_chunks_queued: stats.pending_chunks_queued,
        programs_configured: stats.programs_configured,
        per_stream_count,
        per_stream_truncated: if truncated { 1 } else { 0 },
        per_stream,
    }
}

/// Convert a [`tst_core::mpegts::StreamStats::last_seen`] timestamp to
/// Unix-epoch microseconds for the `_get_stream_last_seen_micros` family
/// of C ABI getters.
///
/// `None` (pid never observed) and a pre-epoch system clock (`duration_since`
/// failure — not reachable on any real deployment target) both collapse to
/// `0`, the documented "unknown" sentinel for this epoch gauge: callers
/// cannot distinguish "never seen" from "clock before 1970" and don't need
/// to — both mean "nothing to report yet".
#[cfg(feature = "std")]
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn last_seen_epoch_micros(last_seen: Option<std::time::SystemTime>) -> u64 {
    last_seen
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_micros().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Fill a fixed-size C per-stream array from a `BTreeMap<u16, StreamStats>`.
/// Returns `(count, truncated)`. Sorted by PID (BTreeMap iteration order).
pub fn fill_per_stream(
    dst: &mut [TstStreamStats; TST_STATS_MAX_STREAMS],
    src: &alloc::collections::BTreeMap<u16, tst_core::mpegts::StreamStats>,
) -> (u32, bool) {
    let total = src.len();
    let n = total.min(TST_STATS_MAX_STREAMS);
    for (i, (_pid, ss)) in src.iter().take(n).enumerate() {
        fill_stream_stats(&mut dst[i], ss);
    }
    (n as u32, total > TST_STATS_MAX_STREAMS)
}

/// Tagged-union mirror of [`tst_core::mpegts::stats::StreamCodecStats`].
/// Layout: 4 (kind) + 4 (pad) + 16 (max union arm) = 24 B.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct TstStreamCodecStats {
    /// Discriminator: 0=unknown, 1=video, 2=klv, 3=audio. Additive — new
    /// kinds get new non-zero values; consumers MUST treat unrecognized
    /// kinds as Unknown and ignore `u`.
    pub kind: u32,
    /// Alignment bridge so `u.video` (which starts with a `u64`) is
    /// 8-byte aligned.
    pub _pad: u32,
    pub u: TstStreamCodecStatsUnion,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union TstStreamCodecStatsUnion {
    pub unknown: TstCodecStatsUnknown,
    pub video: TstCodecStatsVideo,
    pub klv: TstCodecStatsKlv,
    pub audio: TstCodecStatsAudio,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct TstCodecStatsUnknown {}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct TstCodecStatsVideo {
    pub nals_or_obus: u64,
    pub random_access_aus: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct TstCodecStatsKlv {
    pub records: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct TstCodecStatsAudio {
    pub frames: u64,
}

/// Discriminator constants exported as named C constants.
pub const TST_CODEC_KIND_UNKNOWN: u32 = 0;
pub const TST_CODEC_KIND_VIDEO: u32 = 1;
pub const TST_CODEC_KIND_KLV: u32 = 2;
pub const TST_CODEC_KIND_AUDIO: u32 = 3;

/// Convert a Rust public [`tst_core::mpegts::stats::StreamCodecStats`] to
/// the C tagged union.
pub(crate) fn codec_stats_to_c(
    stats: tst_core::mpegts::stats::StreamCodecStats,
) -> TstStreamCodecStats {
    use tst_core::mpegts::stats::StreamCodecStats;
    match stats {
        StreamCodecStats::Unknown => TstStreamCodecStats {
            kind: TST_CODEC_KIND_UNKNOWN,
            _pad: 0,
            u: TstStreamCodecStatsUnion {
                unknown: TstCodecStatsUnknown {},
            },
        },
        StreamCodecStats::Video {
            nals_or_obus,
            random_access_aus,
            ..
        } => TstStreamCodecStats {
            kind: TST_CODEC_KIND_VIDEO,
            _pad: 0,
            u: TstStreamCodecStatsUnion {
                video: TstCodecStatsVideo {
                    nals_or_obus,
                    random_access_aus,
                },
            },
        },
        StreamCodecStats::Klv { records, .. } => TstStreamCodecStats {
            kind: TST_CODEC_KIND_KLV,
            _pad: 0,
            u: TstStreamCodecStatsUnion {
                klv: TstCodecStatsKlv { records },
            },
        },
        StreamCodecStats::Audio { frames, .. } => TstStreamCodecStats {
            kind: TST_CODEC_KIND_AUDIO,
            _pad: 0,
            u: TstStreamCodecStatsUnion {
                audio: TstCodecStatsAudio { frames },
            },
        },
        // #[non_exhaustive] — additive variants surface as Unknown to
        // older callers until they regenerate against a newer header.
        _ => TstStreamCodecStats {
            kind: TST_CODEC_KIND_UNKNOWN,
            _pad: 0,
            u: TstStreamCodecStatsUnion {
                unknown: TstCodecStatsUnknown {},
            },
        },
    }
}

/// `repr(C)` mirror of `tst_rtp::ServerStats` — aggregate server stats
/// snapshot returned by `tst_rtsp_server_get_stats`. Size 32 B.
///
/// Callers stack-allocate this struct and pass a pointer; the entry point
/// fills it in atomically from the server's internal counters.
///
/// Fields:
/// - `active_sessions` — live (accepted, not-yet-closed) client sessions.
///   Decrements when a session sends TEARDOWN or drops its TCP connection.
/// - `mounts` — number of registered mount paths (unicast + multicast).
///   Monotonically increases; mounts are never unregistered.
/// - `total_rtp_packets_sent` — cumulative RTP packets sent across all
///   peers and all mounts since the server started.
/// - `total_rtp_bytes_sent` — cumulative RTP payload bytes (not including
///   UDP + IP headers) sent across all peers and all mounts.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstServerStats {
    pub active_sessions: u64,
    pub mounts: u64,
    pub total_rtp_packets_sent: u64,
    pub total_rtp_bytes_sent: u64,
}

const _TST_SERVER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstServerStats>() == 32,
    "TstServerStats must be 32 bytes (4 × u64)"
);

/// Fill a `TstServerStats` from a Rust `tst_rtp::ServerStats` snapshot.
#[cfg(feature = "rtp")]
pub(crate) fn fill_server_stats(dst: &mut TstServerStats, src: &tst_rtp::ServerStats) {
    dst.active_sessions = src.active_sessions as u64;
    dst.mounts = src.mounts as u64;
    dst.total_rtp_packets_sent = src.total_rtp_packets_sent;
    dst.total_rtp_bytes_sent = src.total_rtp_bytes_sent;
}

/// `repr(C)` mirror of `tst_rtp::MountStats` — per-mount stats snapshot
/// returned by `tst_rtsp_mount_get_stats`. Size 32 B.
///
/// Callers stack-allocate this struct and pass a pointer.
///
/// Fields:
/// - `bytes_pushed` — cumulative TS bytes pushed to this mount since
///   the mount was added (includes fanout to all peers).
/// - `packets_pushed` — cumulative 1316-byte RTP-payload-sized chunks
///   broadcast since mount creation.
/// - `peer_count` — live subscriber count on the broadcast channel.
///   For unicast mounts this is the number of currently-PLAYing clients;
///   for multicast it is typically 1 (the per-mount UDP sender task).
/// - `frames_dropped_total` — cumulative frames dropped by lagging
///   subscribers that fell behind the broadcast channel head.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstMountStats {
    pub bytes_pushed: u64,
    pub packets_pushed: u64,
    pub peer_count: u64,
    pub frames_dropped_total: u64,
}

const _TST_MOUNT_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstMountStats>() == 32,
    "TstMountStats must be 32 bytes (4 × u64)"
);

// ---------------------------------------------------------------------------
// HLS publisher stats (hls feature)
// ---------------------------------------------------------------------------

/// `repr(C)` mirror of `tst_core::publisher::PublisherStats` — the
/// universal cross-publisher stats subset. Returned by
/// `tst_publisher_get_stats` and `tst_mux_publisher_get_publisher_stats`.
/// Size 32 B.
///
/// The two `Option<Duration>` source fields are flattened to whole
/// milliseconds; `None` is encoded as `-1` (a duration is never negative),
/// so C callers branch on `< 0` to detect "no segment open" / "no
/// completed segment yet".
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstPublisherStats {
    /// Total bytes pushed to the publisher's sink (for HLS: bytes written
    /// across all `.ts` segments).
    pub bytes_written: u64,
    /// Total completed segments written.
    pub segments_written: u64,
    /// Wall-clock age of the segment currently open for writes, in
    /// milliseconds. `-1` when no segment is open (between cuts / before
    /// the first push).
    pub current_segment_age_ms: i64,
    /// Wall-clock duration of the most recently completed segment, in
    /// milliseconds. `-1` when no segment has completed yet.
    pub last_segment_duration_ms: i64,
}

const _TST_PUBLISHER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstPublisherStats>() == 32,
    "TstPublisherStats must be 32 bytes (2 × u64 + 2 × i64)"
);

impl From<&tst_core::publisher::PublisherStats> for TstPublisherStats {
    fn from(s: &tst_core::publisher::PublisherStats) -> Self {
        // Option<Duration> → i64 millis; None → -1. Durations cannot be
        // negative, so -1 is an unambiguous "not present" sentinel.
        fn ms(d: Option<core::time::Duration>) -> i64 {
            d.map(|d| d.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or(-1)
        }
        Self {
            bytes_written: s.bytes_written,
            segments_written: s.segments_written,
            current_segment_age_ms: ms(s.current_segment_age),
            last_segment_duration_ms: ms(s.last_segment_duration),
        }
    }
}

/// `repr(C)` mirror of `tst_hls::HlsStats` — HLS-specific richer
/// stats. Returned by `tst_hls_publisher_get_hls_stats`. Size 24 B.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstHlsStats {
    /// Total bytes accepted by `push_ts` (sum across all segments).
    pub bytes_pushed_total: u64,
    /// Bytes in the currently-open segment (0 between cuts).
    pub open_segment_bytes: u64,
    /// Total completed segments (history + current run).
    pub segments_written: u64,
}

const _TST_HLS_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstHlsStats>() == 24,
    "TstHlsStats must be 24 bytes (3 × u64)"
);

#[cfg(feature = "hls")]
impl From<&tst_hls::HlsStats> for TstHlsStats {
    fn from(s: &tst_hls::HlsStats) -> Self {
        Self {
            bytes_pushed_total: s.bytes_pushed_total,
            open_segment_bytes: s.open_segment_bytes,
            segments_written: s.segments_written,
        }
    }
}

/// `repr(C)` mirror of `tst_pipeline::MuxPublisherStats` — cumulative
/// counters for a `MuxPublisher` shell. Returned by
/// `tst_mux_publisher_get_stats`. Size 24 B.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TstMuxPublisherStats {
    /// Total TS bytes drained from the muxer and handed to the publisher.
    pub bytes_pushed: u64,
    /// Total explicit + auto `cut_segment()` calls.
    pub cut_calls: u64,
    /// Total muxer drain calls that produced ≥1 chunk.
    pub drain_calls: u64,
}

const _TST_MUX_PUBLISHER_STATS_SIZE: () = assert!(
    core::mem::size_of::<TstMuxPublisherStats>() == 24,
    "TstMuxPublisherStats must be 24 bytes (3 × u64)"
);

#[cfg(feature = "std")]
impl From<&tst_pipeline::MuxPublisherStats> for TstMuxPublisherStats {
    fn from(s: &tst_pipeline::MuxPublisherStats) -> Self {
        Self {
            bytes_pushed: s.bytes_pushed,
            cut_calls: s.cut_calls,
            drain_calls: s.drain_calls,
        }
    }
}

/// Fill a `TstMountStats` from a Rust `tst_rtp::MountStats` snapshot.
#[cfg(feature = "rtp")]
pub(crate) fn fill_mount_stats(dst: &mut TstMountStats, src: &tst_rtp::MountStats) {
    dst.bytes_pushed = src.bytes_pushed;
    dst.packets_pushed = src.packets_pushed;
    dst.peer_count = src.peer_count as u64;
    dst.frames_dropped_total = src.frames_dropped_total;
}

#[cfg(test)]
mod codec_stats_tests {
    use super::*;
    use tst_core::mpegts::stats::StreamCodecStats;

    #[test]
    fn c_struct_size_is_24() {
        assert_eq!(core::mem::size_of::<TstStreamCodecStats>(), 24);
    }

    #[test]
    fn maps_unknown_variant() {
        let c = codec_stats_to_c(StreamCodecStats::Unknown);
        assert_eq!(c.kind, TST_CODEC_KIND_UNKNOWN);
    }

    // NOTE: per-variant unit tests for Video/Klv/Audio mappings live
    // in the integration tests that exercise the C entry points
    // against a real Muxer/Demuxer. The variants themselves are
    // `#[non_exhaustive]` (not just the enum), so they cannot be
    // constructed via struct expression from outside tst-core
    // (Rust E0639). The
    // discriminator + size invariants are covered above; the field
    // assignments in `codec_stats_to_c` are simple plumbing exercised
    // end-to-end by the integration tests.
}
