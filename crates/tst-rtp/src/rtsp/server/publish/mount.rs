//! Published-mount state and the handles given back to the application.
//!
//! A publish mount's byte flow has two sinks fed from one `emit` call:
//! the mount's `broadcast::Sender<Bytes>` (TS payload bytes, the same
//! fanout a muxer-backed [`super::super::mount::MountState`] uses to
//! re-serve PLAY readers) and an application-facing bounded mpsc channel
//! (whole RTP packets, PT 33) that [`PublishMountHandle::into_recv_transport`]
//! hands out as an [`RtpRecvTransport`] — the SAME constructor the RTSP
//! *client*'s TCP-interleaved path uses, so `DemuxReceiver` and every
//! binding's receiver work against a publish mount unchanged.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bytes::Bytes;

use crate::cancel::RtpCancelHandle;
use crate::error::RtspServerError;
use crate::rtsp::client::end_reason::{EndReasonSlot, StreamEndReason};
use crate::transport::RtpRecvTransport;

/// The application side of a publish mount, under one lock so a take and
/// a close never interleave.
#[derive(Default)]
struct AppSide {
    /// Producer side of the channel: `None` until [`PublishMountState::take_app_rx`]
    /// creates the channel, and again once [`PublishMountState::close`] has
    /// run — a dropped `SyncSender` is what makes the receiver observe
    /// `Disconnected`.
    tx: Option<std::sync::mpsc::SyncSender<Bytes>>,
    /// The receiver has been handed out; a second take returns `None`
    /// (`RtspServerError::TransportTaken`). [`PublishMountState::emit`]
    /// sends nothing before the take: a late `into_recv_transport()`
    /// caller must not read a backlog queued before it took the transport
    /// (possibly from an earlier publisher generation), and no frame
    /// counts in `frames_dropped_app` — there was no queue to drop from.
    taken: bool,
    /// [`PublishMountState::close`] has run: a later take builds a
    /// channel whose sender is already dropped, so the transport reads
    /// `Closed`.
    closed: bool,
}

/// Internal per-mount state for a publish mount. Held inside
/// `ServerState::mounts` as `Arc<PublishMountState>` (via
/// `MountEntry::Publish`). Public surface is via [`PublishMountHandle`]
/// only.
pub(crate) struct PublishMountState {
    pub(crate) path: String,
    /// Created by an ANNOUNCE on an unregistered path (see
    /// `RtspServerBuilder::accept_unregistered_publishers`), not by
    /// `add_publish_mount`. Only these count toward
    /// [`crate::rtsp::server::ON_DEMAND_MOUNT_CAP`], counted under the
    /// mounts lock, so removing the entry from the table is what frees a
    /// slot.
    pub(crate) on_demand: bool,
    /// Broadcast sender — TS payload bytes, re-serving PLAY readers.
    /// Fed by [`Self::emit`]'s `ts` argument, same as a muxer-backed
    /// mount's `MountState::fanout`.
    pub(crate) fanout: tokio::sync::broadcast::Sender<Bytes>,
    /// The application-facing bridge: a bounded mpsc channel of
    /// [`app_queue_bound`] frames, allocated by [`Self::take_app_rx`]
    /// (about 263 KB preallocated) rather than here, so a mount whose
    /// transport is never taken (an idle on-demand mount) costs only its
    /// reader broadcast. See [`AppSide`].
    app: Mutex<AppSide>,
    /// Shared with every `RtpRecvTransport` built from this mount (there
    /// is at most one live at a time, enforced by the take-once
    /// [`AppSide::taken`]) so [`PublishMountHandle::cancel`] can wake a
    /// parked `recv_bytes` from any thread, exactly like a real
    /// TCP-interleaved transport's cancel handle.
    app_cancel: Arc<RtpCancelHandle>,
    /// Shared with the `RtpRecvTransport` built from this mount so
    /// [`Self::close`] can record `CleanTeardown` before dropping the
    /// channel's sender — the same first-writer-wins remap
    /// `recv_bytes_inner` already applies to the RTSP client's
    /// interleaved pump disconnecting cleanly (see
    /// `interleaved_pump.rs`'s `Ok(0)` arm). Without this, a dropped
    /// `SyncSender` is indistinguishable from a wire failure and the
    /// transport would report `Broken`, not `Closed`.
    app_end_reason: EndReasonSlot,
    /// `pub(crate)` so the publisher handlers' own test module
    /// (`publish::handlers::tests`, a sibling of this module) can assert
    /// directly on the slot and generation counter.
    pub(crate) publisher: Mutex<Option<PublisherInfo>>,
    pub(crate) generation: AtomicU64,
    stats: Mutex<PublishMountStatsInner>,
    /// Mount-level dropped-frame total for PLAY readers lagging behind
    /// the fanout — mirrors `MountState::frames_dropped`. Lives outside
    /// `stats` so a lagging peer's fanout task can bump it without
    /// contending on the push-path stats mutex.
    pub(crate) frames_dropped_readers: Arc<AtomicU64>,
}

impl PublishMountState {
    /// Construct a fresh `PublishMountState`. `fanout_capacity` sizes
    /// the PLAY-reader broadcast (mirrors `MountState::new`); the
    /// application-facing bridge, created when the transport is taken, is
    /// sized at [`app_queue_bound`] frames (drop-newest past the bound,
    /// never block the publisher).
    pub(crate) fn new(path: &str, fanout_capacity: usize) -> Arc<Self> {
        Self::build(path, fanout_capacity, false)
    }

    /// [`Self::new`] for a mount an ANNOUNCE creates on demand
    /// (`on_demand` set).
    pub(crate) fn new_on_demand(path: &str, fanout_capacity: usize) -> Arc<Self> {
        Self::build(path, fanout_capacity, true)
    }

    fn build(path: &str, fanout_capacity: usize, on_demand: bool) -> Arc<Self> {
        let (fanout, _rx) = tokio::sync::broadcast::channel(fanout_capacity.max(1));
        Arc::new(Self {
            path: path.to_string(),
            on_demand,
            fanout,
            app: Mutex::new(AppSide::default()),
            app_cancel: RtpCancelHandle::new(),
            app_end_reason: EndReasonSlot::default(),
            publisher: Mutex::new(None),
            generation: AtomicU64::new(0),
            stats: Mutex::new(PublishMountStatsInner::default()),
            frames_dropped_readers: Arc::new(AtomicU64::new(0)),
        })
    }

    /// One frame to both sinks. `ts` = TS payload for readers; `rtp` =
    /// whole RTP packet (PT 33) for the app. A full application channel
    /// drops `rtp` (newest) and ticks `frames_dropped_app`; the reader
    /// fanout never blocks or drops here (a lagging reader's own fanout
    /// task tracks its drops via `frames_dropped_readers`, same as a
    /// muxer-backed mount).
    ///
    /// Before [`Self::take_app_rx`] has run there is no channel and the
    /// application side isn't attempted at all (see [`AppSide::taken`])
    /// — `frames_emitted` still counts the frame (readers got it), but
    /// `frames_dropped_app` does not, since nothing was dropped.
    pub(crate) fn emit(&self, ts: Bytes, rtp: Bytes) {
        let _ = self.fanout.send(ts); // no readers → Err, fine
        let dropped = match &self.app.lock().unwrap_or_else(|e| e.into_inner()).tx {
            Some(tx) => matches!(
                tx.try_send(rtp),
                Err(std::sync::mpsc::TrySendError::Full(_))
            ),
            // Not taken yet, or closed: nothing to count.
            None => false,
        };
        self.tick(|s| {
            s.frames_emitted += 1;
            if dropped {
                s.frames_dropped_app += 1;
            }
        });
    }

    /// Claim the publisher slot. Returns the claimed slot's generation, or
    /// `None` if another publisher already holds it (the slot is exclusive
    /// — only one ANNOUNCE/RECORD session may feed a mount at a time).
    pub(crate) fn try_begin_publisher(&self, mut info: PublisherInfo) -> Option<u64> {
        let mut g = self.publisher.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            return None;
        }
        let generation = self.generation.load(Ordering::Relaxed);
        info.generation = generation;
        *g = Some(info);
        Some(generation)
    }

    /// Whether the publisher slot is still held by the claim
    /// [`Self::try_begin_publisher`] returned `generation` for. `false`
    /// once that publisher ended or the mount was closed (`stop()`,
    /// `remove_mount()`), even if another publisher has claimed it since.
    pub(crate) fn holds_publisher(&self, generation: u64) -> bool {
        self.publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|i| i.generation == generation)
    }

    /// Release the publisher slot (RECORD session ended). Bumps
    /// `generation` so a stale publisher reference is observably out of
    /// date; does NOT close the application transport — a reader
    /// waiting on `into_recv_transport`'s output just sees the stream
    /// idle until the next publisher begins.
    pub(crate) fn end_publisher(&self) {
        let mut g = self.publisher.lock().unwrap_or_else(|e| e.into_inner());
        if g.take().is_some() {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Create the application-facing bridge and take its consumer side.
    /// Used by [`PublishMountHandle::into_recv_transport`] — take-once, so
    /// a second call (from another clone of the handle) sees `None`. After
    /// [`Self::close`] the channel's sender is dropped at once, so the
    /// receiver reads a disconnect.
    pub(crate) fn take_app_rx(&self) -> Option<std::sync::mpsc::Receiver<Bytes>> {
        let mut app = self.app.lock().unwrap_or_else(|e| e.into_inner());
        if app.taken {
            return None;
        }
        app.taken = true;
        let (tx, rx) = std::sync::mpsc::sync_channel(app_queue_bound());
        if !app.closed {
            app.tx = Some(tx);
        }
        Some(rx)
    }

    /// Permanently close the application transport: drops the channel's
    /// sender (and makes a later take build one already dropped) so a
    /// parked (or future) `recv_bytes` on the transport this mount
    /// handed out observes a clean disconnect, reported as
    /// `TransportError::Closed` — not `Broken` — because
    /// `app_end_reason` records `CleanTeardown` first (see the field
    /// doc). Also ends the current publisher, if any. Called by
    /// [`crate::rtsp::server::RtspServer::stop`] and
    /// [`crate::rtsp::server::RtspServer::remove_mount`] — not by
    /// `end_publisher`, which must NOT close the transport (a reader
    /// stays attached across publisher churn).
    pub(crate) fn close(&self) {
        self.app_end_reason.record(StreamEndReason::CleanTeardown);
        {
            let mut app = self.app.lock().unwrap_or_else(|e| e.into_inner());
            app.closed = true;
            app.tx = None;
        }
        self.end_publisher();
    }

    /// Test-only: whether the application-facing channel exists.
    #[cfg(test)]
    pub(crate) fn app_channel_allocated(&self) -> bool {
        self.app.lock().unwrap().tx.is_some()
    }

    /// Mutate the stats accumulator under its mutex.
    pub(crate) fn tick(&self, f: impl FnOnce(&mut PublishMountStatsInner)) {
        let mut s = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s);
    }

    /// Snapshot of cumulative + live mount stats. Mutates nothing.
    pub(crate) fn stats_snapshot(&self) -> PublishMountStats {
        let inner = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        PublishMountStats {
            rtp_packets_received: inner.rtp_packets_received,
            bytes_received: inner.bytes_received,
            malformed_packets: inner.malformed_packets,
            source_rejected: inner.source_rejected,
            frames_emitted: inner.frames_emitted,
            frames_dropped_app: inner.frames_dropped_app,
            frames_dropped_readers: self.frames_dropped_readers.load(Ordering::Relaxed),
            aus_emitted: inner.aus_emitted,
            aus_dropped: inner.aus_dropped,
            aus_reordered: inner.aus_reordered,
            klv_units_emitted: inner.klv_units_emitted,
            klv_units_dropped: inner.klv_units_dropped,
            alignment: inner.alignment,
            alignment_steps: inner.alignment_steps,
            ssrc_changes: inner.ssrc_changes,
            generation: self.generation.load(Ordering::Relaxed),
            peer_count: self.fanout.receiver_count(),
        }
    }
}

/// Bound of a publish mount's application-facing channel, in frames.
///
/// An elementary publisher's muxer emits a whole access unit in one
/// synchronous burst, so the channel must hold the largest AU the H.264
/// depacketizer emits (its default `max_au_bytes`, 8 MiB) even when the
/// application thread is not scheduled during the burst: the AU's TS
/// packets (⌈8 MiB / 184⌉ = 45 591) in 7-packet frames is 6 513 frames,
/// plus 64 frames of headroom for PSI, PCR and KLV — 6 577 frames, about
/// 8.7 MB of 1 328-byte RTP packets when full. The queue holds only what
/// the application has not read yet, so a draining application never
/// approaches it. The headroom does not cover held KLV released in the
/// same burst as the AU (up to the 4 MiB KLV hold budget); see
/// [`PublishMountStats::frames_dropped_app`].
pub(crate) fn app_queue_bound() -> usize {
    let max_au = crate::h264::H264DepayConfig::default().max_au_bytes;
    let frame_packets = crate::rtsp::server::mount::RTP_PAYLOAD_SIZE / 188;
    max_au.div_ceil(184).div_ceil(frame_packets) + 64
}

/// Which publisher currently (or most recently) holds the mount's
/// publisher slot.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PublisherInfo {
    /// Address of the publisher's RTSP control connection.
    pub peer: SocketAddr,
    /// Wire shape the publisher's ANNOUNCE declared.
    pub shape: super::PublishShape,
    /// When the ANNOUNCE claimed the mount.
    pub since: SystemTime,
    /// The mount's publisher generation while this publisher holds it
    /// (the count of publishers that ended on this mount before it).
    pub generation: u64,
}

/// How a publish mount aligns the timestamps of its announced tracks to
/// one clock. Only elementary shapes with a metadata (KLV) track need it;
/// an MP2T or video-only mount reads `NotApplicable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClockAlignment {
    /// Nothing to align: the mount's shape carries its own timing (MP2T),
    /// the publisher announced video only, or no publisher has announced
    /// yet.
    NotApplicable,
    /// The publisher announced a KLV track, but its clock is not yet
    /// related to the video clock: KLV units are held (not emitted) until
    /// RTCP sender reports arrive for both tracks or the two-second
    /// first-packet-coincidence fallback engages. Also reported after a
    /// video or KLV source restart (an SSRC change) until alignment is
    /// re-established. A mount that stays here receives no KLV.
    Pending,
    /// Tracks are aligned by first-packet coincidence because RTCP sender
    /// reports for every track did not arrive in time.
    Provisional,
    /// Tracks are aligned through RTCP sender reports (RFC 3550 §6.4.1).
    SenderReport,
}

impl Default for ClockAlignment {
    fn default() -> Self {
        Self::NotApplicable
    }
}

/// Internal stats accumulator. Public [`PublishMountStats`] snapshot
/// derived from this plus [`PublishMountState::frames_dropped_readers`],
/// `generation`, and the fanout's live subscriber count.
#[derive(Debug, Clone)]
pub(crate) struct PublishMountStatsInner {
    pub(crate) rtp_packets_received: u64,
    pub(crate) bytes_received: u64,
    pub(crate) malformed_packets: u64,
    pub(crate) source_rejected: u64,
    pub(crate) frames_emitted: u64,
    pub(crate) frames_dropped_app: u64,
    pub(crate) aus_emitted: u64,
    pub(crate) aus_dropped: u64,
    pub(crate) aus_reordered: u64,
    pub(crate) klv_units_emitted: u64,
    pub(crate) klv_units_dropped: u64,
    pub(crate) alignment: ClockAlignment,
    pub(crate) alignment_steps: u64,
    pub(crate) ssrc_changes: u64,
}

impl Default for PublishMountStatsInner {
    fn default() -> Self {
        Self {
            rtp_packets_received: 0,
            bytes_received: 0,
            malformed_packets: 0,
            source_rejected: 0,
            frames_emitted: 0,
            frames_dropped_app: 0,
            aus_emitted: 0,
            aus_dropped: 0,
            aus_reordered: 0,
            klv_units_emitted: 0,
            klv_units_dropped: 0,
            alignment: ClockAlignment::NotApplicable,
            alignment_steps: 0,
            ssrc_changes: 0,
        }
    }
}

/// Snapshot of [`PublishMountHandle::stats`]. Counters are cumulative
/// over the mount's life, across publishers.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PublishMountStats {
    /// RTP packets received from publishers, counted before validation.
    pub rtp_packets_received: u64,
    /// Bytes of those RTP packets, headers included.
    pub bytes_received: u64,
    /// Packets dropped as unusable: not RTP, the wrong payload type, an
    /// invalid MP2T payload, or an interleaved frame on an unknown channel.
    pub malformed_packets: u64,
    /// UDP datagrams dropped because they came from an IP other than the
    /// publisher's control connection.
    pub source_rejected: u64,
    /// Frames emitted to the mount's sinks (PLAY readers and the
    /// application transport).
    pub frames_emitted: u64,
    /// Frames dropped because the application transport's queue was full.
    /// The queue holds 6 577 frames (about 8.7 MB): one maximum-size
    /// (8 MiB) elementary access unit, re-muxed, plus 64 frames of
    /// headroom. It fills only when the application stops reading. KLV
    /// held for alignment can leave in the same burst as a large access
    /// unit, up to the KLV hold budget (4 MiB, about 3 257 frames), so
    /// only an 8 MiB access unit released together with about 4 MiB of
    /// held KLV can overflow the queue of an application that is not
    /// draining it.
    pub frames_dropped_app: u64,
    /// Frames dropped across PLAY readers that lagged behind the fan-out.
    pub frames_dropped_readers: u64,
    /// Access units emitted by an elementary-shape adapter.
    pub aus_emitted: u64,
    /// Access units an elementary-shape adapter dropped.
    pub aus_dropped: u64,
    /// Access units an elementary-shape adapter muxed with a PTS below one
    /// it had already muxed: a publisher sending B-frames (RTP carries
    /// presentation times in decode order). They are still muxed, but the
    /// re-muxed TS carries a PTS and no DTS, so a player slaved to its PCR
    /// may show them late. Nonzero means the publisher uses B-frames.
    pub aus_reordered: u64,
    /// KLV units emitted by an elementary-shape adapter.
    pub klv_units_emitted: u64,
    /// KLV units an elementary-shape adapter dropped: damaged or oversize
    /// on the wire, past the hold's bounds (4 096 units or 4 MiB, held
    /// for alignment and waiting for the video together; two seconds
    /// without a video track, or two seconds placed but not reached by
    /// the video), held at a source restart, placed before the video's
    /// first frame, or refused by the muxer.
    pub klv_units_dropped: u64,
    /// How the current publisher's tracks are aligned to one clock.
    pub alignment: ClockAlignment,
    /// Times a new clock mapping replaced the previous one.
    pub alignment_steps: u64,
    /// Source restarts on an elementary publisher's tracks: each time a
    /// video or KLV track's RTP SSRC changed mid-session, counted once per
    /// change per track. The track's depacketizer and its clock alignment
    /// restart (KLV units held at that moment are dropped and counted in
    /// [`Self::klv_units_dropped`]); RTCP sender reports are only taken
    /// from a track's current SSRC.
    pub ssrc_changes: u64,
    /// Publishers that have ended on this mount.
    pub generation: u64,
    /// Live PLAY readers subscribed to the mount's fan-out.
    pub peer_count: usize,
}

/// Public mount surface for a publish mount. Returned by
/// [`crate::rtsp::server::RtspServer::add_publish_mount`]. Cloning is
/// cheap (clones the `Arc`); the application transport, however, is
/// take-once — see [`Self::into_recv_transport`].
#[derive(Clone)]
pub struct PublishMountHandle {
    pub(crate) state: Arc<PublishMountState>,
}

impl PublishMountHandle {
    /// The mount path registered via `add_publish_mount("/path")`.
    pub fn mount_path(&self) -> &str {
        &self.state.path
    }

    /// Live subscriber count on the reader broadcast channel (PLAY
    /// readers re-served from the publisher's TS bytes).
    pub fn peer_count(&self) -> usize {
        self.state.fanout.receiver_count()
    }

    /// Current publisher generation — bumped every time a publisher's
    /// RECORD session ends (see `PublishMountState::end_publisher`).
    pub fn generation(&self) -> u64 {
        self.state.generation.load(Ordering::Relaxed)
    }

    /// The current publisher, if one holds the mount's publisher slot.
    pub fn publisher(&self) -> Option<PublisherInfo> {
        self.state
            .publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Snapshot of cumulative + live mount stats. Mutates nothing.
    pub fn stats(&self) -> PublishMountStats {
        self.state.stats_snapshot()
    }

    /// Ends the application side only: fires the shared cancel handle
    /// also installed on the `RtpRecvTransport` this mount hands out
    /// (see [`Self::into_recv_transport`]), waking a parked `recv_bytes`
    /// with `TransportError::ExplicitClose`. Does not affect PLAY
    /// readers or the publisher slot.
    pub fn cancel(&self) {
        self.state.app_cancel.cancel();
    }

    /// Take the application-facing transport. Take-once across every
    /// clone of this handle — a second call returns
    /// [`RtspServerError::TransportTaken`].
    ///
    /// The returned [`RtpRecvTransport`] is built through the same
    /// `from_mpsc_placeholder` constructor the RTSP *client*'s
    /// TCP-interleaved path uses, so `DemuxReceiver` and every binding's
    /// receiver work against it unchanged. Its cancel handle and
    /// end-reason slot are swapped for this mount's shared ones (mirrors
    /// [`crate::rtsp::client::session::RtspSession::into_recv_transport`])
    /// so [`Self::cancel`] and `PublishMountState::close` reach it from
    /// any thread, at any time — including after this handle (and the
    /// transport itself) have been dropped.
    ///
    /// The transport outlives publisher churn: when a publisher ends it
    /// stays open and silent until the next one. [`Self::cancel`] ends it
    /// with `TransportError::ExplicitClose`;
    /// [`crate::rtsp::server::RtspServer::stop`] and
    /// [`crate::rtsp::server::RtspServer::remove_mount`] end it with
    /// `TransportError::Closed`.
    pub fn into_recv_transport(self) -> Result<RtpRecvTransport, RtspServerError> {
        let rx = self
            .state
            .take_app_rx()
            .ok_or(RtspServerError::TransportTaken)?;
        let mut t = RtpRecvTransport::from_mpsc_placeholder(rx)
            .with_cancel_handle(self.state.app_cancel.clone());
        t.set_end_reason_slot(self.state.app_end_reason.clone());
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tst_core::transport::{RecvTransport, TransportError};

    fn info(g: u64) -> PublisherInfo {
        PublisherInfo {
            peer: "127.0.0.1:5000".parse::<SocketAddr>().unwrap(),
            shape: super::super::PublishShape::Mp2t,
            since: std::time::SystemTime::now(),
            generation: g,
        }
    }
    fn rtp_mp2t(n: usize) -> bytes::Bytes {
        let mut v = vec![0x80u8, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        for _ in 0..n {
            v.push(0x47);
            v.extend(std::iter::repeat_n(0u8, 187));
        }
        v.into()
    }

    #[test]
    fn publisher_slot_is_exclusive_and_generation_counts_ends() {
        let m = PublishMountState::new("/p", 16);
        assert_eq!(m.try_begin_publisher(info(0)), Some(0));
        assert!(
            m.try_begin_publisher(info(0)).is_none(),
            "second publisher refused"
        );
        assert_eq!(m.generation.load(std::sync::atomic::Ordering::Relaxed), 0);
        m.end_publisher();
        assert_eq!(m.generation.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(m.publisher.lock().unwrap().is_none());
        assert_eq!(m.try_begin_publisher(info(1)), Some(1));
    }

    #[test]
    fn emit_reaches_app_transport_and_readers() {
        let m = PublishMountState::new("/p", 16);
        let mut reader = m.fanout.subscribe();
        let h = PublishMountHandle { state: m.clone() };
        let mut t = h.clone().into_recv_transport().unwrap();
        let pkt = rtp_mp2t(2);
        m.emit(pkt.slice(12..), pkt.clone());
        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).unwrap();
        assert_eq!(&buf[..n], &pkt[12..]);
        assert_eq!(reader.try_recv().unwrap(), pkt.slice(12..));
        assert_eq!(m.stats.lock().unwrap().frames_emitted, 1);
    }

    #[test]
    fn emit_before_take_reaches_readers_only_a_late_taker_sees_no_backlog() {
        let m = PublishMountState::new("/p", 16);
        for _ in 0..5 {
            m.emit(rtp_mp2t(1).slice(12..), rtp_mp2t(1));
        }
        let mut t = PublishMountHandle { state: m.clone() }
            .into_recv_transport()
            .unwrap();
        let sixth = rtp_mp2t(1);
        m.emit(sixth.slice(12..), sixth.clone());
        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            &sixth[12..],
            "first read is the 6th frame, not a backlog"
        );
        let s = m.stats_snapshot();
        assert_eq!(s.frames_dropped_app, 0);
        assert_eq!(s.frames_emitted, 6);
    }

    #[test]
    fn the_application_channel_is_allocated_when_the_transport_is_taken() {
        let m = PublishMountState::new_on_demand("/p", 16);
        assert!(
            !m.app_channel_allocated(),
            "an untaken mount holds no channel"
        );
        m.emit(rtp_mp2t(1).slice(12..), rtp_mp2t(1));
        assert!(!m.app_channel_allocated(), "emitting allocates nothing");
        let _t = PublishMountHandle { state: m.clone() }
            .into_recv_transport()
            .unwrap();
        assert!(m.app_channel_allocated());
        m.close();
        assert!(!m.app_channel_allocated(), "close releases it");
    }

    #[test]
    fn close_before_the_take_still_ends_the_transport_with_closed() {
        let m = PublishMountState::new("/p", 16);
        m.close();
        let h = PublishMountHandle { state: m.clone() };
        let mut t = h.clone().into_recv_transport().unwrap();
        m.emit(rtp_mp2t(1).slice(12..), rtp_mp2t(1));
        let mut buf = vec![0u8; 2048];
        assert!(matches!(
            t.recv_bytes(&mut buf),
            Err(TransportError::Closed)
        ));
        assert!(matches!(
            h.into_recv_transport(),
            Err(crate::error::RtspServerError::TransportTaken)
        ));
        assert_eq!(m.stats_snapshot().frames_dropped_app, 0);
    }

    #[test]
    fn transport_can_be_taken_once() {
        let m = PublishMountState::new("/p", 16);
        let h = PublishMountHandle { state: m.clone() };
        let _t = h.clone().into_recv_transport().unwrap();
        assert!(matches!(
            h.into_recv_transport(),
            Err(crate::error::RtspServerError::TransportTaken)
        ));
    }

    #[test]
    fn full_app_channel_drops_newest_and_counts() {
        let m = PublishMountState::new("/p", 16);
        let h = PublishMountHandle { state: m.clone() };
        let _t = h.clone().into_recv_transport().unwrap(); // nobody drains
        let pkt = rtp_mp2t(1);
        for _ in 0..(app_queue_bound() + 3) {
            m.emit(pkt.slice(12..), pkt.clone());
        }
        assert_eq!(m.stats.lock().unwrap().frames_dropped_app, 3);
    }

    #[test]
    fn app_queue_holds_one_maximum_au() {
        // 8 MiB of H.264 in 184-byte TS payloads, 7 packets a frame, + 64.
        assert_eq!(app_queue_bound(), 45_591usize.div_ceil(7) + 64);
        assert_eq!(app_queue_bound(), 6_577);
    }

    #[test]
    fn close_makes_the_transport_read_closed_and_cancel_wakes_a_parked_recv() {
        let m = PublishMountState::new("/p", 16);
        let h = PublishMountHandle { state: m.clone() };
        let mut t = h.clone().into_recv_transport().unwrap();
        let mut buf = vec![0u8; 2048];
        // cancel from another thread while parked
        let h2 = h.clone();
        let j = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            h2.cancel();
        });
        assert!(matches!(
            t.recv_bytes(&mut buf),
            Err(TransportError::ExplicitClose)
        ));
        j.join().unwrap();
        // a fresh mount: close() → Closed
        let m2 = PublishMountState::new("/q", 16);
        let mut t2 = PublishMountHandle { state: m2.clone() }
            .into_recv_transport()
            .unwrap();
        m2.close();
        assert!(matches!(
            t2.recv_bytes(&mut buf),
            Err(TransportError::Closed)
        ));
    }

    #[test]
    fn mount_publisher_ending_does_not_close_the_transport() {
        let m = PublishMountState::new("/p", 16);
        let mut t = PublishMountHandle { state: m.clone() }
            .into_recv_transport()
            .unwrap();
        t.set_recv_timeout(Some(Duration::from_millis(150)));
        assert!(m.try_begin_publisher(info(0)).is_some());
        m.end_publisher();
        let mut buf = vec![0u8; 2048];
        assert!(
            matches!(
                t.recv_bytes(&mut buf),
                Err(TransportError::Backpressure { .. })
            ),
            "idle, still open"
        );
    }
}
