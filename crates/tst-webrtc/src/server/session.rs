//! One viewer session as the media thread holds it, and the pure rules the
//! thread applies to it: the per-viewer packet queue and the reapers.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::Rtc;
use str0m::channel::ChannelId;
use str0m::media::{Mid, Pt};

use crate::mount::{AudioPacket, FramePacket, KlvMessage, MountId, MountState};
use crate::server::media::MediaConfig;
use crate::stats::{IceState, SessionStats};

/// How many distinct source addresses a session admits before ICE and
/// DTLS complete. A browser checks from every local socket and relay it
/// has, so a multi-homed or relayed client can present several; the cap
/// bounds the peer-reflexive candidates the ICE agent adds.
pub(crate) const MAX_PRECONNECT_SOURCES: usize = 16;

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
    /// KLV messages dropped: the queue was full, or the data channel
    /// refused or failed the write.
    pub(crate) dc_messages_dropped: u64,
}

/// One queued write for a viewer.
#[derive(Debug)]
pub(crate) enum Queued {
    /// Payload `.1` of a frame.
    Rtp(Arc<FramePacket>, usize),
    /// A KLV message for the data channel.
    Dc(Arc<KlvMessage>),
    /// An Opus packet for the audio stream.
    Audio(Arc<AudioPacket>),
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
///
/// A session's media goes only to the address it connected from: the
/// source of the datagram that completed DTLS is pinned, datagrams from
/// any other address are dropped, and nothing is sent anywhere else.
/// Before that, at most [`MAX_PRECONNECT_SOURCES`] distinct sources are
/// admitted.
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
    /// The audio media's mid and the Opus payload type, when the viewer's
    /// offer asks to receive Opus and the mount passes it through.
    pub(crate) audio: Option<(Mid, Pt)>,
    /// The next audio RTP sequence number (extended, 64 bits).
    pub(crate) audio_seq: u64,
    /// The next RTP sequence number (extended, 64 bits).
    pub(crate) seq: u64,
    /// `true` once ICE and DTLS completed (str0m's `Event::Connected`).
    pub(crate) connected: bool,
    /// `true` until the first keyframe is queued; frames before it are
    /// skipped, as the viewer could not decode them.
    pub(crate) waiting_keyframe: bool,
    /// The `klv` data channel the server added to the session; it is the
    /// one gated on when a channel opens, so a channel the viewer labels
    /// `klv` is never taken for it.
    pub(crate) klv_channel: Option<ChannelId>,
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
    /// message). A keyframe larger than this is admitted into an empty
    /// queue, so the queue can briefly hold more.
    pub(crate) queue_cap: usize,
    /// Since when frames have been dropped for lack of room without the
    /// queue draining.
    pub(crate) queue_full_since: Option<Instant>,
    /// The session's counters.
    pub(crate) stats: SessionCounters,
    /// Where str0m last sent this session's datagrams.
    pub(crate) remote: Option<SocketAddr>,
    /// The distinct source addresses seen before the handshake completed,
    /// at most [`MAX_PRECONNECT_SOURCES`]; a datagram from a further
    /// source is dropped.
    pub(crate) sources: Vec<SocketAddr>,
    /// The source of the datagram str0m was last handed; the pin is taken
    /// from it when str0m reports the session connected.
    pub(crate) last_source: Option<SocketAddr>,
    /// The source of the first DTLS record the session received. Later DTLS
    /// records from another source are dropped, and the pin is this address:
    /// the handshake proves who holds the keys, not where it was sent from.
    pub(crate) dtls_source: Option<SocketAddr>,
    /// The address the session connected from. Once set, only datagrams
    /// from it reach the session and only it is sent to: a NAT rebinding
    /// or an interface change needs a new session.
    pub(crate) pinned: Option<SocketAddr>,
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
            queue_cap: usize::try_from(mount_state.cfg.viewer_queue_packets).unwrap_or(usize::MAX),
            mount_state,
            rtc,
            mid: None,
            pt: None,
            audio: None,
            audio_seq: 0,
            seq,
            connected: false,
            waiting_keyframe: true,
            klv_channel: None,
            dc: None,
            created: now,
            connected_at: None,
            last_connected: now,
            ice: IceState::New,
            queue: VecDeque::new(),
            queue_full_since: None,
            stats: SessionCounters::default(),
            remote: None,
            sources: Vec::new(),
            last_source: None,
            dtls_source: None,
            pinned: None,
            timeout: now,
        }
    }

    /// Queues one payload entry per RTP payload of `packet`.
    ///
    /// A viewer waiting for a keyframe skips every other frame; a keyframe
    /// clears the wait. A frame that does not fit the remaining capacity is
    /// dropped whole (a partial frame cannot be decoded) and its payloads
    /// are counted in `packets_dropped_queue_full`; the first such drop
    /// starts the queue-stuck clock. A dropped frame returns the viewer to
    /// waiting for a keyframe: later frames may reference the dropped one,
    /// and the viewer sees no sequence gap to recover from.
    ///
    /// The one exception is a keyframe with more payloads than the whole
    /// queue holds: it is queued whenever the queue is empty. Dropping it
    /// would drop every later keyframe of the same size too, and the viewer
    /// would never start.
    pub(crate) fn enqueue_frame(&mut self, packet: &Arc<FramePacket>, now: Instant) -> Enqueued {
        if self.waiting_keyframe && !packet.keyframe {
            return Enqueued::Skipped;
        }
        let n = packet.payloads.len();
        let oversize_start = packet.keyframe && self.queue.is_empty();
        if self.queue.len() + n > self.queue_cap && !oversize_start {
            self.queue_full_since.get_or_insert(now);
            self.waiting_keyframe = true;
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
    /// dropped and counted in `dc_messages_dropped` when the queue is full.
    pub(crate) fn enqueue_klv(&mut self, msg: &Arc<KlvMessage>, now: Instant) -> Enqueued {
        if self.dc.is_none() || self.waiting_keyframe {
            return Enqueued::Skipped;
        }
        if self.queue.len() >= self.queue_cap {
            self.queue_full_since.get_or_insert(now);
            self.stats.dc_messages_dropped += 1;
            return Enqueued::Dropped(1);
        }
        self.queue.push_back(Queued::Dc(Arc::clone(msg)));
        Enqueued::Queued
    }

    /// Queues an Opus packet. Skipped when the viewer has no audio stream;
    /// dropped and counted in `packets_dropped_queue_full` when the queue
    /// is full. Audio does not wait for a keyframe: every Opus packet
    /// decodes on its own.
    pub(crate) fn enqueue_audio(&mut self, packet: &Arc<AudioPacket>, now: Instant) -> Enqueued {
        if self.audio.is_none() {
            return Enqueued::Skipped;
        }
        if self.queue.len() >= self.queue_cap {
            self.queue_full_since.get_or_insert(now);
            self.stats.packets_dropped_queue_full += 1;
            return Enqueued::Dropped(1);
        }
        self.queue.push_back(Queued::Audio(Arc::clone(packet)));
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
            dc_messages_dropped: self.stats.dc_messages_dropped,
        }
    }
}

/// Whether the media thread closes `v` at `now`, and why.
///
/// - [`ReapCause::Ice`]: not connected within `cfg.ice_timeout` of its
///   creation.
/// - [`ReapCause::Disconnected`]: the session's `Rtc` is no longer alive,
///   or, once connected, its ICE state has been anything but Connected
///   (Disconnected, Checking, Failed) for `cfg.disconnect_timeout` since
///   it was last connected.
/// - [`ReapCause::QueueStuck`]: frames have been dropped for lack of room
///   for `cfg.disconnect_timeout` without the queue draining.
pub(crate) fn reap_decision(v: &Viewer, now: Instant, cfg: &MediaConfig) -> Option<ReapCause> {
    if !v.rtc.is_alive() {
        return Some(ReapCause::Disconnected);
    }
    if !v.connected {
        return (elapsed(v.created, now) >= cfg.ice_timeout).then_some(ReapCause::Ice);
    }
    // Any state but Connected counts: str0m's agent moves Disconnected to
    // Checking whenever a new candidate pair turns up, and a viewer can
    // sit there indefinitely.
    if v.ice != IceState::Connected && elapsed(v.last_connected, now) >= cfg.disconnect_timeout {
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
    use crate::clock::{Clock, ManualClock};
    use crate::mount::MountHandle;
    use crate::packetize::{DEFAULT_MTU_PAYLOAD, H264Packetizer, Packetizer};
    use crate::server::media::server_rtc;
    use crate::{AudioPolicy, MountConfig, VideoCodec};

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
            server_rtc(clock.now(), AudioPolicy::Drop),
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
    fn a_session_stuck_checking_after_connecting_is_reaped() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        clock.advance(Duration::from_secs(1));
        connect(&mut v, clock.now());
        clock.advance(Duration::from_secs(60));
        v.set_ice(IceState::Disconnected, clock.now());
        // str0m's agent moves Disconnected to Checking on a new candidate
        // pair; the clock keeps running from the last connected instant.
        clock.advance(Duration::from_secs(5));
        v.set_ice(IceState::Checking, clock.now());
        clock.advance(Duration::from_millis(24_900));
        assert_eq!(reap_decision(&v, clock.now(), &cfg()), None);
        clock.advance(Duration::from_millis(100));
        assert_eq!(
            reap_decision(&v, clock.now(), &cfg()),
            Some(ReapCause::Disconnected)
        );
    }

    #[test]
    fn a_klv_message_that_does_not_fit_counts_as_a_data_channel_drop() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 2);
        connect(&mut v, clock.now());
        let mut api = v.rtc.sdp_api();
        let cid = api.add_channel_with_config(str0m::channel::ChannelConfig {
            label: "klv".into(),
            negotiated: Some(0),
            ..Default::default()
        });
        let _ = api.apply();
        v.dc = Some(cid);
        assert_eq!(
            v.enqueue_frame(&frame(2, true), clock.now()),
            Enqueued::Queued
        );
        let msg = Arc::new(KlvMessage {
            rtp_ts: 9000,
            klv_pts_90k: 0,
            klv: vec![1, 2, 3],
        });
        assert_eq!(v.enqueue_klv(&msg, clock.now()), Enqueued::Dropped(1));
        assert_eq!(v.stats.dc_messages_dropped, 1);
        assert_eq!(v.stats.packets_dropped_queue_full, 0);
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
            v.enqueue_frame(&frame(4, true), clock.now()),
            Enqueued::Queued
        );
        // A keyframe behind a non-empty queue must fit like any frame.
        assert_eq!(
            v.enqueue_frame(&frame(10, true), clock.now()),
            Enqueued::Dropped(10)
        );
        assert_eq!(v.queue.len(), 4);
        assert_eq!(v.stats.packets_dropped_queue_full, 10);
        assert!(v.queue_full_since.is_some());
        v.queue.clear();

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
                Queued::Dc(_) | Queued::Audio(_) => usize::MAX,
            })
            .collect();
        assert_eq!(indices, [0, 1, 2, 3, 0, 1, 2, 3]);
    }

    #[test]
    fn a_dropped_frame_returns_the_viewer_to_waiting_for_a_keyframe() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        connect(&mut v, clock.now());
        assert_eq!(
            v.enqueue_frame(&frame(6, true), clock.now()),
            Enqueued::Queued
        );
        assert!(!v.waiting_keyframe);
        // The queue holds 6 of 8: a 3-payload P-frame does not fit.
        assert_eq!(
            v.enqueue_frame(&frame(3, false), clock.now()),
            Enqueued::Dropped(3)
        );
        assert!(v.waiting_keyframe);
        // A P-frame that would fit is skipped: its reference is gone.
        assert_eq!(
            v.enqueue_frame(&frame(1, false), clock.now()),
            Enqueued::Skipped
        );
        assert_eq!(v.queue.len(), 6);
        assert_eq!(
            v.enqueue_frame(&frame(2, true), clock.now()),
            Enqueued::Queued
        );
        assert!(!v.waiting_keyframe);
        assert_eq!(v.queue.len(), 8);
    }

    #[test]
    fn a_keyframe_larger_than_the_queue_starts_a_viewer_with_an_empty_queue() {
        let clock = ManualClock::new();
        let mut v = viewer(&clock, 8);
        connect(&mut v, clock.now());
        assert_eq!(
            v.enqueue_frame(&frame(10, true), clock.now()),
            Enqueued::Queued
        );
        assert!(!v.waiting_keyframe);
        assert_eq!(v.queue.len(), 10);
        assert_eq!(v.stats.packets_dropped_queue_full, 0);
        assert!(v.queue_full_since.is_none());
        // Over capacity now: a P-frame behind it is dropped as usual.
        assert_eq!(
            v.enqueue_frame(&frame(1, false), clock.now()),
            Enqueued::Dropped(1)
        );
        // An oversize P-frame never gets the exception, even into an empty
        // queue: the viewer waits for the next keyframe.
        v.queue.clear();
        assert_eq!(
            v.enqueue_frame(&frame(2, true), clock.now()),
            Enqueued::Queued
        );
        v.queue.clear();
        assert_eq!(
            v.enqueue_frame(&frame(9, false), clock.now()),
            Enqueued::Dropped(9)
        );
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
