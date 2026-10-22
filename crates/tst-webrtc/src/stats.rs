//! Statistics for the server, its mounts and their viewer sessions.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

tst_core::binding_record! {
    /// Counters of one mount.
    ///
    /// The push calls write the frame, KLV, audio and RTP-clock counters. The
    /// viewer counts and the PLI, NACK, data-channel and per-viewer queue
    /// counters are written by the server's media thread, and the demuxer
    /// counter by an MP2T mount's demuxer; a mount without those writers
    /// reports them as 0.
    ///
    /// <!-- rust-only -->
    /// Read with [`MountHandle::stats`](crate::MountHandle::stats).
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct MountStats {
        /// Access units accepted by `push_video`.
        pub frames_pushed: u64,
        /// Accepted access units forwarded as keyframes: an IDR/CRA/BLA (IRAP)
        /// picture with parameter sets in the access unit or cached from an
        /// earlier one.
        pub keyframes_pushed: u64,
        /// Access units whose `key_frame` flag disagreed with the presence of
        /// an IDR/CRA/BLA (IRAP) NAL unit. The NAL units decide; the flag is
        /// advisory.
        pub keyframe_flag_mismatch: u64,
        /// Viewer sessions currently attached to the mount.
        pub viewers: u64,
        /// Attached viewers that have not received a keyframe yet.
        pub viewers_waiting_keyframe: u64,
        /// KLV records accepted by `push_klv`.
        pub klv_records_pushed: u64,
        /// KLV records dropped before any frame carried them, because the
        /// queue of records waiting for a frame was full.
        pub klv_records_dropped_unbound: u64,
        /// KLV records carried in an SEI message.
        pub klv_sei_inserted: u64,
        /// KLV records left out of the SEI because they did not fit the
        /// access unit's SEI budget.
        pub klv_sei_dropped_oversize: u64,
        /// KLV messages sent on viewers' data channels. A viewer that went
        /// away without a DELETE keeps its session, and these sends, until
        /// the disconnect grace closes it
        /// ([`ServerStats::sessions_reaped_disconnected`]).
        pub klv_dc_messages_sent: u64,
        /// KLV records refused because they exceed the 256 KiB data-channel
        /// message limit.
        ///
        /// <!-- rust-only -->
        /// The limit is [`KLV_DC_LIMIT`](crate::KLV_DC_LIMIT).
        pub klv_dc_dropped_oversize: u64,
        /// KLV data-channel messages dropped on the way to viewers: once per
        /// message when the server's media queue was full or the frame it is
        /// bound to was dropped waiting for a keyframe, and once per viewer
        /// when that viewer's queue was full or its data channel refused or
        /// failed the write (no room in the send buffer, a message above the
        /// browser's `max-message-size`, a closed channel). The video behind
        /// such a message is sent regardless.
        pub klv_dc_dropped_viewer: u64,
        /// Audio packets accepted by `push_audio` for the viewers.
        pub audio_frames_pushed: u64,
        /// Audio pushes discarded: `push_audio` calls under `AudioPolicy::Drop`
        /// and demuxed TS audio events (each may hold several frames), plus,
        /// once per viewer, an Opus packet for a viewer that has no audio send
        /// stream to carry it.
        pub audio_frames_dropped: u64,
        /// Picture loss indications received from viewers.
        pub plis_received: u64,
        /// NACKs received from viewers.
        pub nacks_received: u64,
        /// Video RTP payloads and Opus packets dropped because a queue was
        /// full: every payload of a frame the server's media queue did not
        /// take, and of each later frame the mount then drops until the next
        /// keyframe (counted once), every Opus packet the media queue did not
        /// take (counted once), and every payload or Opus packet a viewer's
        /// queue did not take (counted per viewer). KLV messages dropped the
        /// same way are counted in `klv_dc_dropped_viewer`.
        pub viewer_packets_dropped_queue_full: u64,
        /// Times the RTP clock was rebased after a PTS jump.
        pub rtp_clock_rebases: u64,
        /// Video frames whose PTS was below the previous frame's, within the
        /// rebase window (a jump of more than 10 s back is a rebase instead).
        /// WebRTC carries frames in decode order with the RTP timestamp as
        /// the presentation time, so a source with B-frames is counted here
        /// and plays badly; feed I/P-only streams. Audio is not counted.
        pub frames_pts_reordered: u64,
        /// Non-conformances reported by an MP2T mount's demuxer, and the units
        /// it skips: damaged ones, those of another video codec, and the
        /// access units and KLV records of streams the mount does not play.
        pub demux_nonconformant: u64,
    }
}

tst_core::binding_record! {
    /// Counters of the whole server.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct ServerStats {
        /// Mounts currently registered.
        pub mounts: u64,
        /// Viewer sessions currently open.
        pub active_sessions: u64,
        /// Viewer sessions opened since the server started.
        pub sessions_total: u64,
        /// Session offers refused because a viewer cap was reached.
        pub sessions_rejected_cap: u64,
        /// Session offers refused because no offered codec matched the mount.
        pub sessions_rejected_codec: u64,
        /// Requests refused by authentication: offers and session DELETEs
        /// without the server's bearer token.
        pub sessions_rejected_auth: u64,
        /// Sessions closed because ICE did not connect in time.
        pub sessions_reaped_ice_timeout: u64,
        /// Sessions closed after the viewer disconnected.
        pub sessions_reaped_disconnected: u64,
        /// RTP packets sent to all viewers.
        pub rtp_packets_sent: u64,
        /// RTP bytes sent to all viewers.
        pub rtp_bytes_sent: u64,
        /// Datagrams that matched no session, came from an address other
        /// than the one a session connected from, or came from a source
        /// beyond a session's pre-connect limit.
        pub datagrams_unrouted: u64,
    }
}

/// Statistics of one viewer session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionStats {
    /// The session's identifier, the last segment of its resource URL.
    pub id: String,
    /// The path of the mount the session plays.
    pub mount_path: String,
    /// The viewer's address once ICE has selected one.
    pub remote_addr: Option<SocketAddr>,
    /// The session's ICE connection state.
    pub ice_state: IceState,
    /// How long the session has been connected, once it is.
    pub connected_for: Option<Duration>,
    /// RTP packets sent to the viewer.
    pub rtp_packets: u64,
    /// RTP bytes sent to the viewer.
    pub rtp_bytes: u64,
    /// NACKs received from the viewer.
    pub nacks: u64,
    /// Picture loss indications received from the viewer.
    pub plis: u64,
    /// KLV messages sent on the viewer's data channel.
    pub dc_messages: u64,
    /// RTP payloads dropped because the viewer's queue was full.
    pub packets_dropped_queue_full: u64,
    /// KLV messages dropped for the viewer: its queue was full, or its data
    /// channel refused or failed the write.
    pub dc_messages_dropped: u64,
}

/// The ICE connection state of a viewer session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IceState {
    /// No connectivity check has run yet.
    New,
    /// Connectivity checks are running.
    Checking,
    /// A candidate pair is selected and media flows.
    Connected,
    /// Consent was lost; the session may recover.
    Disconnected,
    /// ICE failed; the session will be closed.
    Failed,
    /// The session is closed.
    Closed,
}

/// The live counters behind [`MountStats`], one atomic per field.
#[derive(Debug, Default)]
pub(crate) struct MountCounters {
    pub(crate) frames_pushed: AtomicU64,
    pub(crate) keyframes_pushed: AtomicU64,
    pub(crate) keyframe_flag_mismatch: AtomicU64,
    /// Written by the media thread.
    pub(crate) viewers: AtomicU64,
    /// Written by the media thread.
    pub(crate) viewers_waiting_keyframe: AtomicU64,
    pub(crate) klv_records_pushed: AtomicU64,
    pub(crate) klv_records_dropped_unbound: AtomicU64,
    pub(crate) klv_sei_inserted: AtomicU64,
    pub(crate) klv_sei_dropped_oversize: AtomicU64,
    /// Written by the media thread.
    pub(crate) klv_dc_messages_sent: AtomicU64,
    pub(crate) klv_dc_dropped_oversize: AtomicU64,
    /// Written by the push calls (media queue full) and by the media
    /// thread.
    pub(crate) klv_dc_dropped_viewer: AtomicU64,
    /// Written by `push_audio` under `AudioPolicy::PassthroughOpus`.
    pub(crate) audio_frames_pushed: AtomicU64,
    pub(crate) audio_frames_dropped: AtomicU64,
    /// Written by the media thread.
    pub(crate) plis_received: AtomicU64,
    /// Written by the media thread.
    pub(crate) nacks_received: AtomicU64,
    /// Written by the push calls (media queue full) and by the media
    /// thread (a viewer's queue full), in RTP payloads.
    pub(crate) viewer_packets_dropped_queue_full: AtomicU64,
    pub(crate) rtp_clock_rebases: AtomicU64,
    pub(crate) frames_pts_reordered: AtomicU64,
    /// Written by an MP2T mount's demuxer.
    pub(crate) demux_nonconformant: AtomicU64,
}

/// Adds one to a counter.
pub(crate) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl MountCounters {
    /// A consistent-enough copy: each counter is read once, relaxed.
    pub(crate) fn snapshot(&self) -> MountStats {
        let r = |c: &AtomicU64| c.load(Ordering::Relaxed);
        MountStats {
            frames_pushed: r(&self.frames_pushed),
            keyframes_pushed: r(&self.keyframes_pushed),
            keyframe_flag_mismatch: r(&self.keyframe_flag_mismatch),
            viewers: r(&self.viewers),
            viewers_waiting_keyframe: r(&self.viewers_waiting_keyframe),
            klv_records_pushed: r(&self.klv_records_pushed),
            klv_records_dropped_unbound: r(&self.klv_records_dropped_unbound),
            klv_sei_inserted: r(&self.klv_sei_inserted),
            klv_sei_dropped_oversize: r(&self.klv_sei_dropped_oversize),
            klv_dc_messages_sent: r(&self.klv_dc_messages_sent),
            klv_dc_dropped_oversize: r(&self.klv_dc_dropped_oversize),
            klv_dc_dropped_viewer: r(&self.klv_dc_dropped_viewer),
            audio_frames_pushed: r(&self.audio_frames_pushed),
            audio_frames_dropped: r(&self.audio_frames_dropped),
            plis_received: r(&self.plis_received),
            nacks_received: r(&self.nacks_received),
            viewer_packets_dropped_queue_full: r(&self.viewer_packets_dropped_queue_full),
            rtp_clock_rebases: r(&self.rtp_clock_rebases),
            frames_pts_reordered: r(&self.frames_pts_reordered),
            demux_nonconformant: r(&self.demux_nonconformant),
        }
    }
}
