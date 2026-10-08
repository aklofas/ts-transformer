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
    /// Read by `handle_record`'s spawn of
    /// [`super::udp_ingest::spawn_udp_ingest`].
    Udp {
        rtp: Arc<tokio::net::UdpSocket>,
        rtcp: Arc<tokio::net::UdpSocket>,
    },
}

/// One announced track plus the transport its SETUP allocated, if any.
pub(crate) struct PublishTrack {
    pub(crate) announced: AnnouncedTrack,
    pub(crate) transport: Option<TrackTransport>,
    /// Set once `handle_record` has spawned this track's UDP ingest task
    /// — keeps a later RECORD on the same session (RFC 2326
    /// §10.11 allows one) from spawning a duplicate. Always stays
    /// `false` for a track whose transport is `Interleaved` (nothing to
    /// spawn there).
    pub(crate) udp_spawned: bool,
}

/// Per-publisher session state. Lives on `ServerSessionState::publish`
/// from a successful ANNOUNCE until TEARDOWN or the session ends.
pub(crate) struct PublishSession {
    pub(crate) mount: Arc<PublishMountState>,
    /// Generation of the mount's publisher slot this session claimed at
    /// ANNOUNCE; `handle_record` refuses once the mount no longer holds
    /// it for this session (see `PublishMountState::holds_publisher`).
    pub(crate) slot_generation: u64,
    pub(crate) tracks: Vec<PublishTrack>,
    pub(crate) recording: bool,
    pub(crate) adapter: Arc<Mutex<Box<dyn PublishAdapter>>>,
    /// Cancelled by [`Self::end`] to stop any UDP ingest tasks spawned
    /// per `TrackTransport::Udp` track — read (cloned) by
    /// `handle_record` and handed to each
    /// [`super::udp_ingest::spawn_udp_ingest`] call.
    pub(crate) udp_cancel: CancellationToken,
    /// `JoinHandle`s for this session's UDP ingest tasks, one
    /// per `TrackTransport::Udp` track, pushed by `handle_record`.
    /// [`Self::end`] cancels `udp_cancel` first (letting each task exit
    /// through its own `select!`'s cancel arm) and then aborts any
    /// still running, as a backstop.
    pub(crate) udp_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Instant-based millis of the last RTP packet accepted on any
    /// track — written by the session loop's interleaved `$` arm
    /// and by the UDP ingest tasks; read by [`Self::media_within`].
    /// `AtomicU64` so a UDP ingest task can update it without taking a
    /// lock.
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
        slot_generation: u64,
        announced: AnnounceShape,
        adapter: Box<dyn PublishAdapter>,
    ) -> Self {
        let tracks = announced
            .tracks
            .into_iter()
            .map(|announced| PublishTrack {
                announced,
                transport: None,
                udp_spawned: false,
            })
            .collect();
        Self {
            mount,
            slot_generation,
            tracks,
            recording: false,
            adapter: Arc::new(Mutex::new(adapter)),
            udp_cancel: CancellationToken::new(),
            udp_tasks: Vec::new(),
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
    /// announced tracks' `control` values, independent of whether
    /// `segment` looks like a recognized `trackID=`/`streamid=`/`stream=`
    /// control segment — gst-rtsp-server's `rtspclientsink` announces
    /// bare `a=control:stream=0`, and other tools may use yet other
    /// conventions. A relative control value matches by exact text. An
    /// absolute `rtsp(s)://` control value matches when its path's last
    /// segment equals `segment`; the host is ignored, because a
    /// publisher writes its own view of the server's address. An absolute
    /// value whose path is the mount itself (a single-track announce
    /// naming the aggregate URL) never matches here: stripping its last
    /// segment would name the mount's parent. `None` when nothing
    /// matches; the caller falls back to [`Self::track_for_control`]'s
    /// prefix-aware resolution, which also covers the no-`a=control`,
    /// bare-mount-URI, single-track case.
    pub(crate) fn track_for_raw_segment(&self, segment: &str) -> Option<usize> {
        use crate::rtsp::server::handlers::{absolute_control_path, last_path_segment};
        self.tracks
            .iter()
            .position(|t| match t.announced.control.as_deref() {
                None => false,
                Some(control) => match absolute_control_path(control) {
                    Some(path) => {
                        path != self.mount.path && last_path_segment(&path) == Some(segment)
                    }
                    None => control == segment,
                },
            })
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

    /// The interleaved channel pair for a track's `mode=record` SETUP:
    /// the publisher's `requested` pair when it has one that no other
    /// track of this session uses (and whose two channels differ),
    /// otherwise the lowest free even/odd pair. Channels are scoped to
    /// this connection (RFC 2326 §10.12), so honouring the request costs
    /// nothing and serves publishers that send on the channels they asked
    /// for without reading the SETUP answer. `None` only if every pair is
    /// taken, which four tracks cannot reach.
    pub(crate) fn interleaved_pair_for(&self, requested: Option<(u8, u8)>) -> Option<(u8, u8)> {
        let used: Vec<u8> = self
            .tracks
            .iter()
            .filter_map(|t| match t.transport {
                Some(TrackTransport::Interleaved { rtp, rtcp }) => Some([rtp, rtcp]),
                _ => None,
            })
            .flatten()
            .collect();
        let free = |(a, b): (u8, u8)| a != b && !used.contains(&a) && !used.contains(&b);
        requested.filter(|&p| free(p)).or_else(|| {
            (0..=u8::MAX - 1)
                .step_by(2)
                .map(|a| (a, a + 1))
                .find(|&p| free(p))
        })
    }

    /// Milliseconds since the process-wide media-liveness epoch. Shared
    /// by the session loop's interleaved `$`-frame arm and the UDP
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
        for h in self.udp_tasks.drain(..) {
            h.abort();
        }
        self.mount.end_publisher();
    }
}

impl Drop for PublishSession {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::server::publish::adapter::Mp2tAdapter;
    use crate::rtsp::server::publish::shape::TrackKind;

    fn two_track_session() -> PublishSession {
        let mount = PublishMountState::new("/p", 8);
        let track = |control: &str| AnnouncedTrack {
            index: 0,
            control: Some(control.into()),
            payload_type: 33,
            kind: TrackKind::Mp2t,
            h264_fmtp: None,
        };
        let shape = AnnounceShape {
            shape: super::super::PublishShape::Mp2t,
            tracks: vec![track("a"), track("b")],
        };
        let adapter = Box::new(Mp2tAdapter::new(mount.clone(), 33));
        PublishSession::new(mount, 0, shape, adapter)
    }

    #[test]
    fn interleaved_pair_honours_a_free_request_and_falls_back_to_the_lowest_free_pair() {
        let mut s = two_track_session();
        assert_eq!(s.interleaved_pair_for(Some((6, 7))), Some((6, 7)));
        assert_eq!(s.interleaved_pair_for(None), Some((0, 1)));
        s.tracks[0].transport = Some(TrackTransport::Interleaved { rtp: 0, rtcp: 1 });
        // The second track asks for a pair the first already holds.
        assert_eq!(s.interleaved_pair_for(Some((0, 1))), Some((2, 3)));
        assert_eq!(s.interleaved_pair_for(Some((1, 2))), Some((2, 3)));
        // A degenerate request (one channel for both) is not honoured.
        assert_eq!(s.interleaved_pair_for(Some((4, 4))), Some((2, 3)));
        assert_eq!(s.interleaved_pair_for(Some((8, 9))), Some((8, 9)));
    }
}
