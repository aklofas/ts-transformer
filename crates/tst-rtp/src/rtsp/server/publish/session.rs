//! Per-publisher session state tracked across ANNOUNCE/SETUP/RECORD.
//!
//! One [`PublishSession`] lives on [`crate::rtsp::server::session::ServerSessionState::publish`]
//! for the lifetime of a publisher's RTSP connection: `handle_announce`
//! creates it (claiming the mount's publisher slot), `handle_setup_record`
//! fills in each track's transport, `handle_record` flips `recording`,
//! and TEARDOWN/disconnect ends it — see [`PublishSession::end`] and its
//! `Drop` twin.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::adapter::PublishAdapter;
use super::mount::PublishMountState;
use super::shape::{AnnounceShape, AnnouncedTrack};

/// The transport a publisher's SETUP allocated for one announced track.
#[derive(Debug)]
pub(crate) enum TrackTransport {
    /// RFC 7826 §14 TCP-interleaved channel pair on the publisher's
    /// control connection.
    Interleaved { rtp: u8, rtcp: u8 },
    /// Server-bound UDP RTP+RTCP socket pair the publisher sends to.
    /// Read by Task 8's UDP ingest loop — unread until that lands, hence
    /// the allow below.
    #[allow(dead_code)]
    Udp {
        rtp: Arc<tokio::net::UdpSocket>,
        rtcp: Arc<tokio::net::UdpSocket>,
    },
}

/// One announced track plus the transport its SETUP allocated, if any.
pub(crate) struct PublishTrack {
    pub(crate) announced: AnnouncedTrack,
    pub(crate) transport: Option<TrackTransport>,
}

/// Per-publisher session state. Lives on `ServerSessionState::publish`
/// from a successful ANNOUNCE until TEARDOWN or the session ends.
pub(crate) struct PublishSession {
    pub(crate) mount: Arc<PublishMountState>,
    /// Read in this module's own tests today; production reads it once
    /// an `Elementary` shape has a real adapter (PR 2 of this arc) and
    /// once stats reporting names the active shape.
    #[allow(dead_code)]
    pub(crate) shape: super::PublishShape,
    pub(crate) tracks: Vec<PublishTrack>,
    pub(crate) recording: bool,
    pub(crate) adapter: Arc<Mutex<Box<dyn PublishAdapter>>>,
    /// Cancelled by [`Self::end`] to stop any UDP ingest tasks Task 8
    /// spawns per `TrackTransport::Udp` track; unread until that lands.
    pub(crate) udp_cancel: CancellationToken,
    /// Instant-based millis of the last RTP packet accepted on any
    /// track — written by the session loop's interleaved `$` arm (Task 7)
    /// and (once that lands) Task 8's UDP ingest tasks; read by
    /// [`Self::media_within`]. `AtomicU64` so a UDP ingest task can
    /// update it without taking a lock.
    pub(crate) last_media_ms: Arc<AtomicU64>,
    /// Makes [`Self::end`] idempotent — TEARDOWN calls it explicitly and
    /// `Drop` calls it again on every exit path; the second call must be
    /// a no-op (in particular, must not double-bump the mount's
    /// publisher generation).
    ended: bool,
}

/// Process-wide epoch for [`PublishSession::now_ms`]. Values derived from
/// it are only ever compared to each other (never persisted or sent over
/// the wire), so an arbitrary fixed origin is fine; anchoring on
/// [`Instant`] rather than [`std::time::SystemTime`] means a wall-clock
/// adjustment can never make a liveness check go backwards.
static MEDIA_CLOCK_EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

impl PublishSession {
    pub(crate) fn new(
        mount: Arc<PublishMountState>,
        announced: AnnounceShape,
        adapter: Box<dyn PublishAdapter>,
    ) -> Self {
        let shape = announced.shape;
        let tracks = announced
            .tracks
            .into_iter()
            .map(|announced| PublishTrack {
                announced,
                transport: None,
            })
            .collect();
        Self {
            mount,
            shape,
            tracks,
            recording: false,
            adapter: Arc::new(Mutex::new(adapter)),
            udp_cancel: CancellationToken::new(),
            last_media_ms: Arc::new(AtomicU64::new(0)),
            ended: false,
        }
    }

    /// Every announced track has a SETUP-allocated transport. Gates
    /// RECORD (RFC 2326 §10.11 requires the stream to be set up first).
    pub(crate) fn all_tracks_set_up(&self) -> bool {
        self.tracks.iter().all(|t| t.transport.is_some())
    }

    /// Resolve a URI's raw trailing path segment against this session's
    /// announced tracks' `control` values with an exact string match,
    /// independent of whether `segment` looks like a recognized
    /// `trackID=`/`streamid=`/`stream=` control segment — gst-rtsp-
    /// server's `rtspclientsink` announces bare `a=control:stream=0`,
    /// and other tools may use yet other conventions, so an exact match
    /// against whatever the SDP actually said is the one check that
    /// works for all of them. `None` when nothing matches; the caller
    /// falls back to [`Self::track_for_control`]'s prefix-aware
    /// resolution (which also covers the no-`a=control`,
    /// bare-mount-URI, single-track case this exact match cannot — a
    /// track with no announced control never matches here).
    pub(crate) fn track_for_raw_segment(&self, segment: &str) -> Option<usize> {
        self.tracks
            .iter()
            .position(|t| t.announced.control.as_deref() == Some(segment))
    }

    /// Resolve a SETUP URI's control segment (see
    /// [`crate::rtsp::server::handlers::control_segment`]) to a track
    /// index. A single-track announce accepts a SETUP with no control
    /// segment at all (some clients SETUP the bare mount URI when
    /// there's only one track to disambiguate); otherwise the segment
    /// must exactly match the track's announced `a=control` value
    /// (including the no-control-at-all case, where both sides are
    /// `None`).
    pub(crate) fn track_for_control(&mut self, segment: Option<&str>) -> Option<usize> {
        if segment.is_none() && self.tracks.len() == 1 {
            return Some(0);
        }
        self.tracks
            .iter()
            .position(|t| t.announced.control.as_deref() == segment)
    }

    /// Resolve an incoming RFC 7826 §14 `$<channel>` interleaved frame's
    /// channel number to its track, and whether it's the RTP or RTCP
    /// channel of that track's pair. Read by the session loop's
    /// interleaved frame dispatch.
    pub(crate) fn track_for_channel(&self, ch: u8) -> Option<(usize, bool)> {
        self.tracks
            .iter()
            .enumerate()
            .find_map(|(i, t)| match t.transport {
                Some(TrackTransport::Interleaved { rtp, .. }) if rtp == ch => Some((i, false)),
                Some(TrackTransport::Interleaved { rtcp, .. }) if rtcp == ch => Some((i, true)),
                _ => None,
            })
    }

    /// Milliseconds since the process-wide media-liveness epoch. Shared
    /// by the session loop's interleaved `$`-frame arm and Task 8's UDP
    /// ingest tasks — both stamp [`Self::last_media_ms`] with this same
    /// clock, so [`Self::media_within`] never compares values taken from
    /// two different origins.
    pub(crate) fn now_ms() -> u64 {
        MEDIA_CLOCK_EPOCH.elapsed().as_millis() as u64
    }

    /// Whether RTP media arrived on this publisher within `d` of now.
    /// Read by the session loop's idle-timeout arm: a TCP-interleaved
    /// publisher's frames already re-arm the read-idle sleep by landing
    /// as bytes on the control socket, but a UDP-transport publisher
    /// sends its RTP on a different socket that read loop never
    /// touches — this is what keeps that session alive instead. Never
    /// "recent" before the first packet (`last_media_ms == 0`).
    pub(crate) fn media_within(&self, d: Duration) -> bool {
        let last = self.last_media_ms.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        Self::now_ms().saturating_sub(last) <= d.as_millis() as u64
    }

    /// End the publisher: flush whatever the adapter has pending, cancel
    /// any UDP ingest tasks, and free the mount's publisher slot so a
    /// fresh ANNOUNCE can claim it. Idempotent — safe to call from both
    /// `handle_teardown` and (unconditionally) `Drop`.
    pub(crate) fn end(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.adapter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush();
        self.udp_cancel.cancel();
        self.mount.end_publisher();
    }
}

impl Drop for PublishSession {
    fn drop(&mut self) {
        self.end();
    }
}
