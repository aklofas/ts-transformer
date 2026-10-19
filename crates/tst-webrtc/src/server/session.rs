//! One viewer session as the media thread holds it, and the pure rules the
//! thread applies to it: the per-viewer packet queue and the reapers.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::Rtc;
use str0m::channel::ChannelId;
use str0m::media::{Mid, Pt};

use crate::VideoCodec;
use crate::mount::{FramePacket, KlvMessage, MountId, MountState};
use crate::server::media::MediaConfig;
use crate::stats::{IceState, SessionStats};

/// A viewer session's own counters.
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionCounters {
    /// RTP packets written to the viewer.
    pub(crate) rtp_packets: u64,
    /// RTP payload bytes written to the viewer.
    pub(crate) rtp_bytes: u64,
    /// NACKs the viewer sent, as str0m's egress statistics count them.
    pub(crate) nacks: u64,
    /// Picture loss indications the viewer sent.
    pub(crate) plis: u64,
    /// KLV messages written on the viewer's data channel.
    pub(crate) dc_messages: u64,
    /// RTP payloads dropped because the viewer's queue was full.
    pub(crate) packets_dropped_queue_full: u64,
}

/// One queued write for a viewer.
#[derive(Debug)]
pub(crate) enum Queued {
    /// Payload `.1` of a frame.
    Rtp(Arc<FramePacket>, usize),
    /// A KLV message for the data channel.
    Dc(Arc<KlvMessage>),
}

/// Why a session is closed by the media thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReapCause {
    /// ICE and DTLS did not complete within the ICE timeout.
    Ice,
    /// The viewer stayed disconnected (or the session died) for the
    /// disconnect timeout.
    Disconnected,
    /// The viewer's queue stayed full for the disconnect timeout.
    QueueStuck,
}

/// What [`Viewer::enqueue_frame`] did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enqueued {
    /// The viewer waits for a keyframe and this frame is not one.
    Skipped,
    /// Every payload of the frame is queued.
    Queued,
    /// The frame did not fit and was dropped whole; this many payloads
    /// were counted as dropped.
    Dropped(u64),
}

/// A viewer session.
pub(crate) struct Viewer {
    /// The session id, 32 hex characters.
    pub(crate) id: String,
    /// The mount the session plays.
    pub(crate) mount: MountId,
    /// The mount's shared state, for its path and counters.
    pub(crate) mount_state: Arc<MountState>,
    /// The session's WebRTC state machine.
    pub(crate) rtc: Rtc,
    /// The video media's mid, once negotiated.
    pub(crate) mid: Option<Mid>,
    /// The video payload type, once negotiated.
    pub(crate) pt: Option<Pt>,
    /// The video codec sent.
    pub(crate) codec: VideoCodec,
    /// The next RTP sequence number (extended, 64 bits).
    pub(crate) seq: u64,
    /// `true` once the send stream exists, so its SSRC is known.
    pub(crate) ssrc_known: bool,
    /// `true` once ICE and DTLS completed (str0m's `Event::Connected`).
    pub(crate) connected: bool,
    /// `true` until the first keyframe is queued; frames before it are
    /// skipped, as the viewer could not decode them.
    pub(crate) waiting_keyframe: bool,
    /// The `klv` data channel, once open.
    pub(crate) dc: Option<ChannelId>,
    /// When the session was created.
    pub(crate) created: Instant,
    /// When the session first connected.
    pub(crate) connected_at: Option<Instant>,
    /// The last instant the session was known connected: refreshed on
    /// every ICE Connected/Completed and stamped when it leaves them.
    pub(crate) last_connected: Instant,
    /// The ICE connection state.
    pub(crate) ice: IceState,
    /// Writes waiting for str0m.
    pub(crate) queue: VecDeque<Queued>,
    /// The queue's capacity in entries (one per RTP payload or KLV
    /// message).
    pub(crate) queue_cap: usize,
    /// Since when frames have been dropped for lack of room without the
    /// queue draining.
    pub(crate) queue_full_since: Option<Instant>,
    /// The session's counters.
    pub(crate) stats: SessionCounters,
    /// Where str0m last sent this session's datagrams.
    pub(crate) remote: Option<SocketAddr>,
    /// str0m's next deadline for this session.
    pub(crate) timeout: Instant,
}

impl Viewer {
    /// A session that has just accepted its offer.
    pub(crate) fn new(
        id: String,
        mount_state: Arc<MountState>,
        rtc: Rtc,
        seq: u64,
        now: Instant,
    ) -> Self {
        Self {
            id,
            mount: mount_state.id,
            codec: mount_state.cfg.video,
            queue_cap: usize::try_from(mount_state.cfg.viewer_queue_packets).unwrap_or(usize::MAX),
            mount_state,
            rtc,
            mid: None,
            pt: None,
            seq,
            ssrc_known: false,
            connected: false,
            waiting_keyframe: true,
            dc: None,
            created: now,
            connected_at: None,
            last_connected: now,
            ice: IceState::New,
            queue: VecDeque::new(),
            queue_full_since: None,
            stats: SessionCounters::default(),
            remote: None,
            timeout: now,
        }
    }

    /// Queues one payload entry per RTP payload of `packet`.
    ///
    /// A viewer waiting for a keyframe skips every other frame; a keyframe
    /// clears the wait. A frame that does not fit the remaining capacity is
    /// dropped whole (a partial frame cannot be decoded) and its payloads
    /// are counted in `packets_dropped_queue_full`; the first such drop
    /// starts the queue-stuck clock.
    pub(crate) fn enqueue_frame(&mut self, packet: &Arc<FramePacket>, now: Instant) -> Enqueued {
        if self.waiting_keyframe && !packet.keyframe {
            return Enqueued::Skipped;
        }
        let n = packet.payloads.len();
        if self.queue.len() + n > self.queue_cap {
            self.queue_full_since.get_or_insert(now);
            let dropped = n as u64;
            self.stats.packets_dropped_queue_full += dropped;
            return Enqueued::Dropped(dropped);
        }
        self.waiting_keyframe = false;
        self.queue
            .extend((0..n).map(|i| Queued::Rtp(Arc::clone(packet), i)));
        Enqueued::Queued
    }

    /// Queues a KLV message for the data channel. Skipped when the channel
    /// is not open or the viewer has not started on a keyframe yet;
    /// dropped and counted when the queue is full.
    pub(crate) fn enqueue_klv(&mut self, msg: &Arc<KlvMessage>, now: Instant) -> Enqueued {
        if self.dc.is_none() || self.waiting_keyframe {
            return Enqueued::Skipped;
        }
        if self.queue.len() >= self.queue_cap {
            self.queue_full_since.get_or_insert(now);
            self.stats.packets_dropped_queue_full += 1;
            return Enqueued::Dropped(1);
        }
        self.queue.push_back(Queued::Dc(Arc::clone(msg)));
        Enqueued::Queued
    }

    /// Records an ICE state change at `now`.
    pub(crate) fn set_ice(&mut self, ice: IceState, now: Instant) {
        if ice == IceState::Connected || self.ice == IceState::Connected {
            // Entering or leaving Connected: either way the session was
            // connected up to now.
            self.last_connected = now;
        }
        self.ice = ice;
    }

    /// A snapshot of the session's statistics.
    pub(crate) fn snapshot(&self, now: Instant) -> SessionStats {
        SessionStats {
            id: self.id.clone(),
            mount_path: self.mount_state.path.clone(),
            remote_addr: self.remote,
            ice_state: self.ice,
            connected_for: self.connected_at.map(|t| now.saturating_duration_since(t)),
            rtp_packets: self.stats.rtp_packets,
            rtp_bytes: self.stats.rtp_bytes,
            nacks: self.stats.nacks,
            plis: self.stats.plis,
            dc_messages: self.stats.dc_messages,
            packets_dropped_queue_full: self.stats.packets_dropped_queue_full,
        }
    }
}

/// Whether the media thread closes `v` at `now`, and why.
///
/// - [`ReapCause::Ice`]: not connected within `cfg.ice_timeout` of its
///   creation.
/// - [`ReapCause::Disconnected`]: the session's `Rtc` is no longer alive,
///   or its ICE state has been Disconnected or Failed for
///   `cfg.disconnect_timeout`.
/// - [`ReapCause::QueueStuck`]: frames have been dropped for lack of room
///   for `cfg.disconnect_timeout` without the queue draining.
pub(crate) fn reap_decision(v: &Viewer, now: Instant, cfg: &MediaConfig) -> Option<ReapCause> {
    if !v.rtc.is_alive() {
        return Some(ReapCause::Disconnected);
    }
    if !v.connected {
        return (elapsed(v.created, now) >= cfg.ice_timeout).then_some(ReapCause::Ice);
    }
    if matches!(v.ice, IceState::Disconnected | IceState::Failed)
        && elapsed(v.last_connected, now) >= cfg.disconnect_timeout
    {
        return Some(ReapCause::Disconnected);
    }
    v.queue_full_since
        .filter(|since| elapsed(*since, now) >= cfg.disconnect_timeout)
        .map(|_| ReapCause::QueueStuck)
}

/// `now - since`, zero when `since` is later.
fn elapsed(since: Instant, now: Instant) -> Duration {
    now.saturating_duration_since(since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MountConfig;
    use crate::clock::{Clock, ManualClock};
    use crate::mount::MountHandle;
    use crate::packetize::{DEFAULT_MTU_PAYLOAD, H264Packetizer, Packetizer};
    use crate::server::media::server_rtc;

    fn cfg() -> MediaConfig {
        MediaConfig {
            // TEST-NET-1: the configuration is read, never bound.
            bind: "192.0.2.1:40000".parse().unwrap(),
            advertise: None,
            max_viewers: 16,
            ice_timeout: Duration::from_secs(10),
            disconnect_timeout: Duration::from_secs(30),
        }
    }

    fn viewer(clock: &ManualClock, queue_cap: usize) -> Viewer {
        let (handle, _rx) = MountHandle::for_test(MountConfig::default());
        let mut v = Viewer::new(
            "0".repeat(32),
            Arc::clone(&handle.state),
            server_rtc(clock.now()),
            0,
            clock.now(),
        );
        v.queue_cap = queue_cap;
        v
    }

    /// A frame of `payloads` single-NAL-unit payloads.
    fn frame(payloads: usize, keyframe: bool) -> Arc<FramePacket> {
        let bodies: Vec<[u8; 2]> = (0..payloads)
            .map(|i| [0x41, u8::try_from(i).unwrap()])
            .collect();
        let nals: Vec<&[u8]> = bodies.iter().map(<[u8; 2]>::as_slice).collect();
        Arc::new(FramePacket {
            rtp_ts: 9000,
            keyframe,
            codec: VideoCodec::H264,
            payloads: H264Packetizer.packetize(&nals, DEFAULT_MTU_PAYLOAD),
            parameter_sets: None,
        })
    }

    fn connect(v: &mut Viewer, now: Instant) {
        v.connected = true;
        v.connected_at = Some(now);
        v.set_ice(IceState::Connected, now);
    }

    #[test]
    fn a_session_that_never_connects_is_reaped_at_the_ice_timeout() {
        let clock = ManualClock::new();
        let v = viewer(&clock, 8);
        clock.advance(Duration::from_millis(9_900));
        assert_eq!(reap_decision(&v, clock.now(), &cfg()), None);
        clock.advance(Duration::from_millis(100));
        assert_eq!(reap_decision(&v, clock.now(), &cfg()), Some(ReapCause::Ice));
    }

    #[test]
    fn a_disconnected_session_is_reaped_after_the_disconnect_timeout() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        clock.advance(Duration::from_secs(1));
        connect(&mut v, clock.now());
        clock.advance(Duration::from_secs(60));
        v.set_ice(IceState::Disconnected, clock.now());
        clock.advance(Duration::from_millis(29_900));
        assert_eq!(reap_decision(&v, clock.now(), &cfg()), None);
        clock.advance(Duration::from_millis(100));
        assert_eq!(
            reap_decision(&v, clock.now(), &cfg()),
            Some(ReapCause::Disconnected)
        );
    }

    #[test]
    fn a_healthy_session_is_kept_after_an_hour() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        clock.advance(Duration::from_secs(1));
        connect(&mut v, clock.now());
        clock.advance(Duration::from_secs(3600));
        assert_eq!(reap_decision(&v, clock.now(), &cfg()), None);
    }

    #[test]
    fn a_queue_full_for_the_disconnect_timeout_is_reaped() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        clock.advance(Duration::from_secs(1));
        connect(&mut v, clock.now());
        assert_eq!(
            v.enqueue_frame(&frame(8, true), clock.now()),
            Enqueued::Queued
        );
        assert_eq!(
            v.enqueue_frame(&frame(1, false), clock.now()),
            Enqueued::Dropped(1)
        );
        clock.advance(Duration::from_millis(29_900));
        assert_eq!(reap_decision(&v, clock.now(), &cfg()), None);
        clock.advance(Duration::from_millis(100));
        assert_eq!(
            reap_decision(&v, clock.now(), &cfg()),
            Some(ReapCause::QueueStuck)
        );
    }

    #[test]
    fn a_frame_that_does_not_fit_is_dropped_whole_and_counted() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        connect(&mut v, clock.now());
        assert_eq!(
            v.enqueue_frame(&frame(10, true), clock.now()),
            Enqueued::Dropped(10)
        );
        assert!(v.queue.is_empty());
        assert_eq!(v.stats.packets_dropped_queue_full, 10);
        assert!(v.queue_full_since.is_some());

        assert_eq!(
            v.enqueue_frame(&frame(4, true), clock.now()),
            Enqueued::Queued
        );
        assert_eq!(
            v.enqueue_frame(&frame(4, false), clock.now()),
            Enqueued::Queued
        );
        assert_eq!(v.queue.len(), 8);
        assert_eq!(v.stats.packets_dropped_queue_full, 10);
        let indices: Vec<usize> = v
            .queue
            .iter()
            .map(|q| match q {
                Queued::Rtp(_, i) => *i,
                Queued::Dc(_) => usize::MAX,
            })
            .collect();
        assert_eq!(indices, [0, 1, 2, 3, 0, 1, 2, 3]);
    }

    #[test]
    fn frames_before_the_first_keyframe_are_skipped() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        connect(&mut v, clock.now());
        assert_eq!(
            v.enqueue_frame(&frame(2, false), clock.now()),
            Enqueued::Skipped
        );
        assert!(v.waiting_keyframe);
        assert_eq!(
            v.enqueue_frame(&frame(2, true), clock.now()),
            Enqueued::Queued
        );
        assert!(!v.waiting_keyframe);
        assert_eq!(
            v.enqueue_frame(&frame(2, false), clock.now()),
            Enqueued::Queued
        );
        assert_eq!(v.queue.len(), 4);
    }
}
