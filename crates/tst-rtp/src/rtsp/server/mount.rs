//! Mount-handle surface — the public push API exposed to callers via
//! `RtspServer::add_mount` and `add_multicast_mount`.
//!
//! Architecture:
//! - One `Muxer` per mount (inside a `Mutex` for sync access from
//!   thread-safe push methods).
//! - One `broadcast::Sender<Bytes>` per mount fanning out TS bytes to
//!   N peers' per-session subscriber tasks.
//! - `MountHandle` carries an `Arc<MountState>` + the registered mount
//!   path string. Cloning the handle is cheap; multiple clones can push
//!   from different threads.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::broadcast;

use tst_core::mpegts::mux::{Muxer, MuxerConfig};

use super::publish::mount::PublishMountState;

/// Discriminant for mount type.
///
/// `Unicast` mounts pair the broadcast fanout with per-session
/// subscriber tasks; `Multicast` mounts use one shared UDP socket per
/// group.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MountKind {
    /// Unicast mount — each connected client gets its own per-session
    /// task subscribing to the broadcast.
    Unicast,
    /// Multicast mount — one shared UDP socket sends to the group;
    /// per-session tasks collapse to a counter bump.
    Multicast {
        group: SocketAddr,
        ttl: u8,
        iface: Option<String>,
    },
}

/// Internal per-mount state. Held inside `ServerState::mounts` as
/// `Arc<MountState>`. Public surface is via `MountHandle` only.
pub(crate) struct MountState {
    pub(crate) path: String,
    pub(crate) kind: MountKind,
    pub(crate) muxer: Mutex<Muxer>,
    /// Broadcast sender — fanout target for serialized TS bytes. Subscribers
    /// are created by the per-session subscriber task on PLAY.
    pub(crate) fanout: broadcast::Sender<Bytes>,
    pub(crate) stats: Mutex<MountStatsInner>,
    /// Mount-level dropped-frame total, summed across all peers' fanout
    /// tasks in real time (each peer's [`PeerDropCounter`] is linked here
    /// via `with_mount_total`). Lives outside `stats` so a lagging peer's
    /// fanout task can bump it without contending on the push-path stats
    /// mutex. Surfaced as `MountStats::frames_dropped_total`.
    ///
    /// [`PeerDropCounter`]: crate::rtsp::server::fanout::PeerDropCounter
    pub(crate) frames_dropped: Arc<AtomicU64>,
}

/// Internal stats accumulator. Public `MountStats` snapshot derived from this.
#[derive(Default)]
pub(crate) struct MountStatsInner {
    pub(crate) bytes_pushed: u64,
    pub(crate) packets_pushed: u64,
    pub(crate) per_stream: BTreeMap<u16, tst_core::mpegts::stats::StreamStats>,
}

impl MountState {
    /// Construct a fresh MountState from a MuxerConfig + MountKind +
    /// fanout capacity. Both `RtspServer::add_mount` (unicast) and
    /// `RtspServer::add_multicast_mount` construct through here.
    pub(crate) fn new(
        path: impl Into<String>,
        kind: MountKind,
        muxer_cfg: MuxerConfig,
        fanout_capacity: usize,
    ) -> Result<Arc<Self>, crate::error::RtspServerError> {
        let muxer =
            Muxer::new(muxer_cfg).map_err(|e| crate::error::RtspServerError::InvalidConfig {
                detail: format!("muxer construction failed: {e}"),
            })?;
        let (tx, _rx) = broadcast::channel(fanout_capacity.max(1));
        Ok(Arc::new(Self {
            path: path.into(),
            kind,
            muxer: Mutex::new(muxer),
            fanout: tx,
            stats: Mutex::new(MountStatsInner::default()),
            frames_dropped: Arc::new(AtomicU64::new(0)),
        }))
    }
}

/// One entry in `ServerState::mounts` — either a muxer-backed local mount
/// (`add_mount` / `add_multicast_mount`) or a publish mount (`add_publish_mount`)
/// fed by an ANNOUNCE/RECORD publisher. `handle_describe` / `handle_setup` /
/// `handle_play` read through the accessors below instead of matching the
/// variant directly, so the two mount kinds share one lookup path.
#[derive(Clone)]
pub(crate) enum MountEntry {
    Local(Arc<MountState>),
    Publish(Arc<PublishMountState>),
}

impl MountEntry {
    /// The broadcast fanout carrying serialized TS bytes — a local
    /// mount's muxer output, or a publish mount's re-served publisher
    /// input. PLAY readers subscribe to this for either kind.
    pub(crate) fn fanout(&self) -> &broadcast::Sender<Bytes> {
        match self {
            MountEntry::Local(m) => &m.fanout,
            MountEntry::Publish(m) => &m.fanout,
        }
    }

    /// Mount-level dropped-frame total for PLAY readers (see
    /// [`MountState::frames_dropped`] / `PublishMountState::frames_dropped_readers`).
    pub(crate) fn frames_dropped(&self) -> Arc<AtomicU64> {
        match self {
            MountEntry::Local(m) => m.frames_dropped.clone(),
            MountEntry::Publish(m) => m.frames_dropped_readers.clone(),
        }
    }

    /// A publish mount is never multicast — ANNOUNCE/RECORD targets a
    /// single unicast path; the publisher's bytes still re-serve
    /// unicast PLAY readers through [`Self::fanout`].
    pub(crate) fn is_multicast(&self) -> bool {
        match self {
            MountEntry::Local(m) => matches!(m.kind, MountKind::Multicast { .. }),
            MountEntry::Publish(_) => false,
        }
    }

    /// `Some(group)` for a local multicast mount; `None` for a local
    /// unicast mount or any publish mount.
    pub(crate) fn multicast_group(&self) -> Option<SocketAddr> {
        match self {
            MountEntry::Local(m) => match &m.kind {
                MountKind::Multicast { group, .. } => Some(*group),
                MountKind::Unicast => None,
            },
            MountEntry::Publish(_) => None,
        }
    }

    /// `Some(ttl)` for a local multicast mount; `None` otherwise. Paired
    /// with [`Self::multicast_group`] by `handle_setup` to rebuild the
    /// `Transport:` response header without matching on `MountKind`.
    pub(crate) fn multicast_ttl(&self) -> Option<u8> {
        match self {
            MountEntry::Local(m) => match &m.kind {
                MountKind::Multicast { ttl, .. } => Some(*ttl),
                MountKind::Unicast => None,
            },
            MountEntry::Publish(_) => None,
        }
    }
}

/// Snapshot of [`MountHandle::stats`].
///
/// `bytes_pushed` and `packets_pushed` are cumulative since the mount
/// was added. `peer_count` reflects the live subscriber count on the
/// broadcast channel. `frames_dropped_total` sums per-peer dropped-frame
/// counters reported by lagging subscribers.
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MountStats {
    pub bytes_pushed: u64,
    pub packets_pushed: u64,
    pub peer_count: usize,
    pub frames_dropped_total: u64,
    pub per_stream: BTreeMap<u16, tst_core::mpegts::stats::StreamStats>,
}

/// Public mount surface. Returned by `RtspServer::add_mount` /
/// `add_multicast_mount`. Cloning is cheap (clones the `Arc`); multiple
/// holders can push from different threads.
///
/// The push API (`push_video` / `push_klv` / `push_audio` /
/// `push_subtitle` / `push_data`, plus the `*_to` targeted variants and
/// `push_video_to_with_dts`) mirrors `tst_pipeline::MuxSender`'s `send_*`
/// signatures; accessors are `mount_path`, `peer_count`, `stats`,
/// `mount_kind`, and the `*_handles()` family.
#[derive(Clone)]
pub struct MountHandle {
    pub(crate) state: Arc<MountState>,
}

impl std::fmt::Debug for MountHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountHandle")
            .field("path", &self.state.path)
            .field("kind", &self.state.kind)
            .field("peer_count", &self.state.fanout.receiver_count())
            .finish()
    }
}

impl MountHandle {
    /// The mount path registered via `add_mount("/path", ...)`.
    pub fn mount_path(&self) -> &str {
        &self.state.path
    }

    /// Live subscriber count on the broadcast channel. For unicast
    /// mounts this is the number of currently-PLAYing clients; for
    /// multicast it's typically 1 (the per-mount multicast sender task).
    pub fn peer_count(&self) -> usize {
        self.state.fanout.receiver_count()
    }

    /// Snapshot of cumulative + live mount stats. Mutates nothing.
    pub fn stats(&self) -> MountStats {
        let stats_lock = match self.state.stats.lock() {
            Ok(g) => g,
            Err(_) => return MountStats::default(),
        };
        MountStats {
            bytes_pushed: stats_lock.bytes_pushed,
            packets_pushed: stats_lock.packets_pushed,
            peer_count: self.state.fanout.receiver_count(),
            frames_dropped_total: self.state.frames_dropped.load(Ordering::Relaxed),
            per_stream: stats_lock.per_stream.clone(),
        }
    }

    /// Discriminant for the mount type.
    pub fn mount_kind(&self) -> &MountKind {
        &self.state.kind
    }

    /// Lock the inner muxer, call `f`, and on success drain + broadcast
    /// the resulting TS bytes. This is the shared skeleton used by every
    /// `push_*` method below.
    ///
    /// Returns [`crate::error::MountError::Closed`] when the mutex is
    /// poisoned, or propagates any [`tst_core::error::MuxError`] returned
    /// by `f` as [`crate::error::MountError::Mux`].
    fn with_muxer<F>(&self, f: F) -> Result<(), crate::error::MountError>
    where
        F: FnOnce(&mut Muxer) -> Result<(), tst_core::error::MuxError>,
    {
        let mut muxer = self
            .state
            .muxer
            .lock()
            .map_err(|_| crate::error::MountError::Closed)?;
        f(&mut muxer)?;
        drain_and_broadcast(&mut muxer, &self.state);
        Ok(())
    }

    // ── Push surface ──────────────────────────────────────────────────────
    //
    // The push methods mirror `tst_pipeline::MuxSender`'s `send_*`
    // signatures — same argument order, same semantics. Internally each
    // push:
    //   1. Locks the inner Muxer (Mutex<Muxer>; mutex poisoning maps to
    //      `MountError::Closed`).
    //   2. Calls the matching Muxer::push_* method (any MuxError surfaces
    //      as `MountError::Mux`).
    //   3. Drains the muxer's TS output via `Muxer::pull` into a
    //      1316-byte (= 7 × 188) RTP-payload-sized buffer and broadcasts
    //      each non-empty pull through the mount's fanout channel.
    //
    // `broadcast::Sender::send` returns `Err` only when there are no
    // subscribers — that's the pre-PLAY state, NOT an error. We silently
    // drop those bytes; the muxer still consumed them so producers
    // remain free to push without first connecting a peer.

    /// Push one video access unit. Mirror of
    /// `tst_pipeline::MuxSender::send_video`.
    ///
    /// Drains the resulting TS bytes from the inner muxer and broadcasts
    /// them through this mount's fanout channel (chunked at 1316 bytes
    /// per send — the canonical RTP MPEG-TS payload size).
    ///
    /// # Errors
    /// - [`crate::error::MountError::Mux`] wraps any
    ///   [`tst_core::error::MuxError`] from the muxer (invalid NAL,
    ///   `BufferFull`, ambiguous target, etc.).
    /// - [`crate::error::MountError::Closed`] if the inner mutex is
    ///   poisoned (the mount's owning task panicked while holding the
    ///   lock).
    ///
    /// The push itself always succeeds if the muxer accepts the frame; a
    /// lagging peer's dropped frames are tracked in
    /// [`MountStats::frames_dropped_total`], not surfaced as an error here.
    pub fn push_video(
        &self,
        nal: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
        key_frame: bool,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_video(nal, pts, key_frame))
    }

    /// Push one KLV blob. Mirror of
    /// `tst_pipeline::MuxSender::send_klv`.
    ///
    /// See [`Self::push_video`] for the drain + broadcast contract and
    /// error mapping.
    pub fn push_klv(
        &self,
        klv: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_klv(klv, pts, metadata_service_id))
    }

    /// Push one audio frame buffer. Mirror of
    /// `tst_pipeline::MuxSender::send_audio`.
    ///
    /// See [`Self::push_video`] for the drain + broadcast contract and
    /// error mapping.
    pub fn push_audio(
        &self,
        frames: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_audio(frames, pts))
    }

    /// Push one subtitle payload. Mirror of
    /// `tst_pipeline::MuxSender::send_subtitle`.
    ///
    /// Note: argument order is `(payload, pts)` for parity with the other
    /// `push_*` methods on this type — the underlying
    /// `Muxer::push_subtitle(pts, payload)` swaps them. See
    /// [`Self::push_video`] for the drain + broadcast contract.
    pub fn push_subtitle(
        &self,
        payload: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_subtitle(pts, payload))
    }

    /// Push one data payload. Mirror of
    /// `tst_pipeline::MuxSender::send_data`.
    ///
    /// Pass-through: no AU-cell wrap, no framing, no inspection — `data`
    /// lands verbatim as one PES packet on `stream_id` 0xBD. See
    /// [`Self::push_video`] for the drain + broadcast contract and error
    /// mapping.
    pub fn push_data(
        &self,
        data: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_data(data, pts))
    }

    // ── Multi-stream / multi-program variants ────────────────────────────
    //
    // Use these when the mount's `MuxerConfig` declares more than one
    // stream of a given kind. The single-stream `push_*` methods above
    // return `MuxError::AmbiguousTarget` in that case. Obtain a handle
    // from the matching `*_handles()` accessor below.

    /// Push to a specific video stream handle. Mirror of
    /// `MuxSender::send_video_to`.
    pub fn push_video_to(
        &self,
        handle: tst_core::mpegts::mux::VideoStreamHandle,
        nal: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
        key_frame: bool,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_video_to(handle, nal, pts, key_frame))
    }

    /// Push one video access unit with an explicit decode timestamp.
    /// Mirror of `MuxSender::send_video_to_with_dts` — targeted-only,
    /// like every DTS push in the workspace (there is no single-stream
    /// `push_video_with_dts`; obtain the handle from
    /// [`Self::video_handles`]).
    ///
    /// Without this variant a mount always serves DTS == PTS; a capture
    /// replayer or restreamer needs it to reproduce real encoder timing
    /// (e.g. a constant PTS−DTS offset) for downstream DTS-handling
    /// paths.
    ///
    /// **Caller invariant:** `dts <= pts` per H.222.0 §2.4.3.6. The muxer
    /// does not enforce this; receivers will reject inverted timestamps.
    ///
    /// See [`Self::push_video`] for the drain + broadcast contract and
    /// error mapping.
    pub fn push_video_to_with_dts(
        &self,
        handle: tst_core::mpegts::mux::VideoStreamHandle,
        nal: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
        dts: tst_core::mpegts::common::Pts90khz,
        key_frame: bool,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_video_to_with_dts(handle, nal, pts, dts, key_frame))
    }

    /// Push to a specific KLV stream handle. Mirror of
    /// `MuxSender::send_klv_to`.
    pub fn push_klv_to(
        &self,
        handle: tst_core::mpegts::mux::KlvStreamHandle,
        klv: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_klv_to(handle, klv, pts, metadata_service_id))
    }

    /// Push to a specific audio stream handle. Mirror of
    /// `MuxSender::send_audio_to`.
    ///
    /// Argument order is `(handle, frames, pts)` for parity with the
    /// other `push_*_to` methods; the underlying
    /// `Muxer::push_audio_to(handle, pts, frames)` swaps the last two.
    pub fn push_audio_to(
        &self,
        handle: tst_core::mpegts::mux::AudioStreamHandle,
        frames: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_audio_to(handle, pts, frames))
    }

    /// Push to a specific subtitle stream handle. Mirror of
    /// `MuxSender::send_subtitle_to`.
    ///
    /// Argument order is `(handle, payload, pts)` for parity with the
    /// other `push_*_to` methods; the underlying
    /// `Muxer::push_subtitle_to(handle, pts, payload)` swaps the last
    /// two.
    pub fn push_subtitle_to(
        &self,
        handle: tst_core::mpegts::mux::SubtitleStreamHandle,
        payload: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_subtitle_to(handle, pts, payload))
    }

    /// Push to a specific data stream handle. Mirror of
    /// `MuxSender::send_data_to`.
    pub fn push_data_to(
        &self,
        handle: tst_core::mpegts::mux::DataStreamHandle,
        data: &[u8],
        pts: tst_core::mpegts::common::Pts90khz,
    ) -> Result<(), crate::error::MountError> {
        self.with_muxer(|m| m.push_data_to(handle, data, pts))
    }

    // ── Lifecycle helpers ────────────────────────────────────────────────

    /// Drain any TS packets queued in the inner muxer and broadcast them
    /// through the mount's fanout channel.
    ///
    /// Call after finishing a batch of `push_*` calls to ensure all
    /// buffered TS output is flushed to subscribers before e.g. a sleep or
    /// a stats snapshot. This is a no-op when the muxer has no pending
    /// output; it is always safe to call.
    ///
    /// Returns silently on a poisoned mutex (same interpretation as
    /// `stats()`: the mount is closed).
    pub fn flush(&self) {
        if let Ok(mut muxer) = self.state.muxer.lock() {
            drain_and_broadcast(&mut muxer, &self.state);
        }
    }

    /// Reset all flow counters on the mount to zero.
    ///
    /// Mirrors `MuxSender::reset_stats`. Clears both the mount-level
    /// `bytes_pushed` / `packets_pushed` / `frames_dropped_total`
    /// accumulators in `MountState::stats` and the inner `Muxer`'s
    /// per-stream counters. Per-stream entries are preserved (same
    /// semantics as `Muxer::reset_stats`).
    ///
    /// Silent no-op on mutex poison.
    pub fn reset_stats(&self) {
        if let Ok(mut muxer) = self.state.muxer.lock() {
            muxer.reset_stats();
        }
        if let Ok(mut s) = self.state.stats.lock() {
            s.bytes_pushed = 0;
            s.packets_pushed = 0;
        }
        self.state.frames_dropped.store(0, Ordering::Relaxed);
    }

    // ── Stream-handle accessors ──────────────────────────────────────────
    //
    // Mirror `MuxSender::*_handles`. Each returns the declared handles in
    // `(program, within-program)` order. A poisoned mutex returns an
    // empty Vec (consistent with the "mount-closed" interpretation used
    // by `stats()`).

    /// List the configured video stream handles.
    pub fn video_handles(&self) -> Vec<tst_core::mpegts::mux::VideoStreamHandle> {
        match self.state.muxer.lock() {
            Ok(m) => m.video_handles(),
            Err(_) => Vec::new(),
        }
    }

    /// List the configured KLV stream handles.
    pub fn klv_handles(&self) -> Vec<tst_core::mpegts::mux::KlvStreamHandle> {
        match self.state.muxer.lock() {
            Ok(m) => m.klv_handles(),
            Err(_) => Vec::new(),
        }
    }

    /// List the configured audio stream handles.
    pub fn audio_handles(&self) -> Vec<tst_core::mpegts::mux::AudioStreamHandle> {
        match self.state.muxer.lock() {
            Ok(m) => m.audio_handles(),
            Err(_) => Vec::new(),
        }
    }

    /// List the configured subtitle stream handles.
    pub fn subtitle_handles(&self) -> Vec<tst_core::mpegts::mux::SubtitleStreamHandle> {
        match self.state.muxer.lock() {
            Ok(m) => m.subtitle_handles(),
            Err(_) => Vec::new(),
        }
    }

    /// Configured data stream handles. Mirror of `Muxer::data_handles`.
    pub fn data_handles(&self) -> Vec<tst_core::mpegts::mux::DataStreamHandle> {
        match self.state.muxer.lock() {
            Ok(m) => m.data_handles(),
            Err(_) => Vec::new(),
        }
    }
}

/// Default RTP MPEG-TS payload size (7 × 188-byte TS packets). Matches
/// `DEFAULT_PKT_SIZE`.
pub(crate) const RTP_PAYLOAD_SIZE: usize = 1316;

/// Drain TS bytes from the locked muxer via `Muxer::pull` and broadcast
/// each non-empty chunk through the mount's fanout channel.
///
/// `Muxer::pull(buf)` already chunks at the buffer's size (rounded down
/// to a 188-byte multiple); sizing `buf` at exactly `RTP_PAYLOAD_SIZE`
/// (= 7 × 188) means each broadcast is a single RTP payload boundary.
///
/// `broadcast::Sender::send` returns `Err` when there are no
/// subscribers — silently dropped here (pre-PLAY mounts work; the
/// muxer still consumed the bytes).
fn drain_and_broadcast(
    muxer: &mut std::sync::MutexGuard<'_, tst_core::mpegts::mux::Muxer>,
    state: &MountState,
) {
    let mut buf = [0u8; RTP_PAYLOAD_SIZE];
    let mut bytes_total: u64 = 0;
    let mut packets_total: u64 = 0;
    loop {
        let n = muxer.pull(&mut buf);
        if n == 0 {
            break;
        }
        bytes_total = bytes_total.saturating_add(n as u64);
        packets_total = packets_total.saturating_add(1);
        // No subscribers → Err, suppressed.
        let _ = state.fanout.send(Bytes::copy_from_slice(&buf[..n]));
    }
    if bytes_total != 0 {
        if let Ok(mut s) = state.stats.lock() {
            s.bytes_pushed = s.bytes_pushed.saturating_add(bytes_total);
            s.packets_pushed = s.packets_pushed.saturating_add(packets_total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

    fn mock_unicast_mount_state() -> Arc<MountState> {
        // Minimal MuxerConfig: one program with H.264 video.
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let cfg = b.build().unwrap();
        MountState::new("/test", MountKind::Unicast, cfg, 256).unwrap()
    }

    #[test]
    fn mount_state_constructs() {
        let state = mock_unicast_mount_state();
        assert_eq!(state.path, "/test");
    }

    #[test]
    fn mount_handle_path() {
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        assert_eq!(handle.mount_path(), "/test");
    }

    #[test]
    fn mount_handle_peer_count_zero_initially() {
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        // No subscribers until a per-session task subscribes.
        assert_eq!(handle.peer_count(), 0);
    }

    #[test]
    fn mount_handle_stats_default() {
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        let stats = handle.stats();
        assert_eq!(stats.bytes_pushed, 0);
        assert_eq!(stats.packets_pushed, 0);
        assert_eq!(stats.peer_count, 0);
        assert_eq!(stats.frames_dropped_total, 0);
    }

    #[test]
    fn mount_handle_clone_is_cheap() {
        let state = mock_unicast_mount_state();
        let h1 = MountHandle { state };
        let h2 = h1.clone();
        // Both reference the same MountState (Arc count == 2).
        assert!(Arc::ptr_eq(&h1.state, &h2.state));
    }

    #[test]
    fn mount_handle_kind_is_unicast() {
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        assert!(matches!(handle.mount_kind(), MountKind::Unicast));
    }

    // ── push_* surface ────────────────────────────────────────────────────
    //
    // Annex-B IDR with NAL type 5: `0x00 0x00 0x00 0x01 0x65 0xBB`. The
    // muxer's first pull on a fresh stream emits PAT + PMT + video TS
    // packets, so a single push reliably produces drained TS bytes.

    use tst_core::mpegts::common::Pts90khz;

    #[test]
    fn push_video_without_subscribers_succeeds() {
        // Pre-PLAY: no broadcast subscribers. The muxer still accepts
        // the push; the drain happens normally and broadcast::send
        // returns Err which we suppress.
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
        handle
            .push_video(&nal, Pts90khz::new(0), true)
            .expect("push succeeds even with no peers");
    }

    #[test]
    fn push_video_updates_stats() {
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
        let initial = handle.stats();
        handle.push_video(&nal, Pts90khz::new(0), true).unwrap();
        let after = handle.stats();
        assert!(
            after.bytes_pushed > initial.bytes_pushed,
            "bytes_pushed should grow after a push (got {} -> {})",
            initial.bytes_pushed,
            after.bytes_pushed,
        );
        assert!(
            after.packets_pushed > initial.packets_pushed,
            "packets_pushed should grow after a push",
        );
    }

    #[test]
    fn push_video_with_subscriber_delivers_to_broadcast() {
        let state = mock_unicast_mount_state();
        let mut rx = state.fanout.subscribe();
        let handle = MountHandle { state };
        let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
        handle.push_video(&nal, Pts90khz::new(0), true).unwrap();
        // At least one chunk should arrive (PAT/PMT plus video PES).
        let payload = rx.try_recv().expect("broadcast received chunk");
        assert!(!payload.is_empty());
        // Each chunk is a multiple of 188 bytes and <= 1316.
        assert_eq!(payload.len() % 188, 0);
        assert!(payload.len() <= RTP_PAYLOAD_SIZE);
    }

    #[test]
    fn push_video_to_with_dts_serves_distinct_dts() {
        // A capture replayer serving a real encoder's timing must be able
        // to reproduce PTS != DTS through a mount (integrator field
        // report: gimbal encoders hold PTS-DTS at a constant offset).
        // Push with a 0.2 s offset (18000 ticks @ 90 kHz), then demux the
        // broadcast TS bytes and assert the offset survived.
        use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload};

        let state = mock_unicast_mount_state();
        let mut rx = state.fanout.subscribe();
        let handle = MountHandle { state };
        let vh = handle.video_handles()[0];
        let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
        handle
            .push_video_to_with_dts(vh, &nal, Pts90khz::new(18000), Pts90khz::new(0), true)
            .expect("dts push succeeds");

        let mut ts = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            ts.extend_from_slice(&chunk);
        }
        assert!(!ts.is_empty(), "push must broadcast TS bytes");

        let mut demux = Demuxer::new();
        demux.feed(&ts).expect("demux feed");
        demux.flush();
        let mut saw_video = false;
        while let Some(event) = demux.next_event() {
            // Match the video payload specifically — `Sample` also covers
            // audio/subtitles, and the assertion must not vacuously pass
            // on some other stream if the mock config ever grows one.
            if let DemuxEvent::Sample {
                pts,
                dts,
                payload: SamplePayload::Video { .. },
                ..
            } = event
            {
                assert_eq!(pts, Pts90khz::new(18000));
                assert_eq!(
                    dts,
                    Some(Pts90khz::new(0)),
                    "served AU must carry the distinct DTS, not DTS==PTS"
                );
                saw_video = true;
            }
        }
        assert!(saw_video, "expected a video sample from the served TS");
    }

    #[test]
    fn video_handles_returns_one_entry_for_one_program() {
        let state = mock_unicast_mount_state();
        let handle = MountHandle { state };
        let handles = handle.video_handles();
        assert_eq!(handles.len(), 1);
    }

    fn mock_unicast_mount_state_with_data() -> Arc<MountState> {
        // Program with H.264 video (for PCR) + one data stream (PCR-ineligible).
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_data(0x1012, 0x06, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let cfg = b.build().unwrap();
        MountState::new("/test", MountKind::Unicast, cfg, 256).unwrap()
    }

    #[test]
    fn push_data_without_subscribers_succeeds() {
        let state = mock_unicast_mount_state_with_data();
        let handle = MountHandle { state };
        handle
            .push_data(&[0xDE, 0xAD, 0xBE, 0xEF], Pts90khz::new(0))
            .expect("push_data succeeds even with no peers");
    }

    #[test]
    fn push_data_to_specific_handle_succeeds() {
        let state = mock_unicast_mount_state_with_data();
        let handle = MountHandle { state };
        let h = handle
            .data_handles()
            .into_iter()
            .next()
            .expect("one data handle configured");
        handle
            .push_data_to(h, &[0x01, 0x02], Pts90khz::new(900))
            .expect("push_data_to succeeds");
    }
}
