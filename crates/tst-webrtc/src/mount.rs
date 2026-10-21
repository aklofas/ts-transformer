//! A mount: the push side of one WHEP resource.
//!
//! A push call turns its input into messages for the server's media
//! thread. A video access unit is packetized once, here, and the media
//! thread hands the same payloads to every viewer, writing each viewer's
//! own sequence numbers and SSRC. KLV records wait in the mount until the
//! first frame whose PTS is at or after theirs, then travel inside that
//! frame (as an SEI message) and next to it (as a data-channel message
//! with the frame's RTP timestamp).
//!
//! The mount's state sits behind one mutex that is never held while a
//! message is queued: the queue is bounded and filled with `try_send`, so
//! a push never blocks on the media thread. When the queue is full the
//! message is dropped and counted.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tst_core::codec::annexb;
use tst_core::codec::misp_time::insert_sei_before_first_vcl;
use tst_core::codec::nal::{is_vcl, nal_unit_type};
use tst_core::codec::nal_framing::{ParameterSets, extract_parameter_sets};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, MetadataKind, ProgramMap, SamplePayload, StreamId,
    StreamKind,
};

use crate::config::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec};
use crate::error::{PushError, WhepServerError};
use crate::packetize::{DEFAULT_MTU_PAYLOAD, Packetizer, RtpPayload, packetizer_for};
use crate::server::ControlLink;
use crate::server::media::Waker;
use crate::stats::{MountCounters, MountStats, SessionStats, bump};

/// The bytes a data-channel message adds before the KLV record: the
/// format version (1), the frame's RTP timestamp (4) and the record's PTS
/// (8).
pub(crate) const KLV_DC_HEADER_BYTES: usize = 13;

/// The largest KLV record a mount accepts on the data channel: 256 KiB
/// (262 144 bytes).
///
/// The server sizes each session's SCTP send buffer (512 KiB) to hold a
/// record at this limit with room for others still in flight. A record is
/// written only when the viewer's buffer has room for it; otherwise it is
/// dropped for that viewer and counted, never queued behind the video.
/// A record is sent as one data-channel message of the record plus a
/// 13-byte header; for a viewer whose `max-message-size` is smaller than
/// that message the record is dropped and counted. The size is 64 KiB when
/// the viewer states none and 256 KiB in current Chrome and Safari, so
/// their largest record is 262 131 bytes; Firefox takes any record up to
/// the limit. Drops are
/// counted in [`MountStats::klv_dc_dropped_viewer`] and
/// [`SessionStats::dc_messages_dropped`](crate::SessionStats::dc_messages_dropped).
pub const KLV_DC_LIMIT: u64 = 256 * 1024;

/// The SCTP send buffer of each viewer session (512 KiB): a record at
/// [`KLV_DC_LIMIT`] plus its header fits with room for others still
/// unacknowledged. Accepted data stays in memory until the viewer
/// acknowledges it, so 64 viewers hold at most 32 MiB.
pub(crate) const SCTP_SEND_BUFFER_BYTES: usize = 512 * 1024;

const _: () = assert!(KLV_DC_LIMIT as usize + KLV_DC_HEADER_BYTES <= SCTP_SEND_BUFFER_BYTES);

/// KLV records waiting for a frame; when full the oldest is dropped.
const PENDING_KLV_CAP: usize = 1024;

/// The longest Opus packet: three 1275-byte frames (120 ms, RFC 6716 §3.2).
const OPUS_MAX_PACKET_BYTES: usize = 1275 * 3;

/// A PTS more than this far behind the previous frame rebases the RTP
/// clock (10 s).
const REBASE_BACKWARDS_TICKS: i64 = 90_000 * 10;

/// A PTS more than this far ahead of the previous frame rebases the RTP
/// clock (1 h).
const REBASE_FORWARDS_TICKS: i64 = 90_000 * 3600;

/// The RTP timestamp step a rebase advances by: one frame at 30 frames
/// per second.
const REBASE_STEP_TICKS: u32 = 3000;

/// The RTP timestamp step an audio rebase advances by: one 20 ms Opus
/// frame at 48 kHz.
const AUDIO_REBASE_STEP_TICKS: u32 = 960;

/// Identifies a mount inside the server.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MountId(pub u32);

/// A message from a mount to the server's media thread.
#[doc(hidden)]
#[derive(Debug)]
pub enum MediaMsg {
    /// A packetized video frame.
    Frame {
        /// The mount it belongs to.
        mount: MountId,
        /// The frame, shared by every viewer.
        packet: Arc<FramePacket>,
    },
    /// A KLV record bound to a frame, for the data channel.
    Klv {
        /// The mount it belongs to.
        mount: MountId,
        /// The record and the frame it is bound to.
        msg: Arc<KlvMessage>,
    },
    /// An audio frame.
    Audio {
        /// The mount it belongs to.
        mount: MountId,
        /// The Opus packet, shared by every viewer.
        packet: Arc<AudioPacket>,
    },
    /// The mount was removed.
    MountRemoved(MountId),
    /// The server is shutting down.
    Stop,
}

/// One packetized frame.
#[doc(hidden)]
#[derive(Debug)]
pub struct FramePacket {
    /// The frame's RTP timestamp (90 kHz).
    pub rtp_ts: u32,
    /// `true` when a viewer can start decoding at this frame: an
    /// IDR/CRA/BLA (IRAP) picture with its parameter sets.
    pub keyframe: bool,
    /// The video codec.
    pub codec: VideoCodec,
    /// The RTP payloads, shared by every viewer; sequence numbers and the
    /// SSRC are written per viewer.
    pub payloads: Vec<RtpPayload>,
    /// On a keyframe, the parameter-set NAL units (VPS, SPS, PPS; header
    /// included, no start code) carried in this frame.
    pub parameter_sets: Option<Vec<Vec<u8>>>,
}

/// One Opus packet (RFC 7587: one packet per RTP payload).
#[doc(hidden)]
#[derive(Debug)]
pub struct AudioPacket {
    /// The packet's RTP timestamp (48 kHz).
    pub rtp_ts: u32,
    /// The Opus packet as pushed.
    pub payload: Vec<u8>,
}

/// A KLV record bound to a frame. The media thread builds the
/// data-channel body from it.
#[doc(hidden)]
#[derive(Debug)]
pub struct KlvMessage {
    /// The RTP timestamp of the frame the record is bound to.
    pub rtp_ts: u32,
    /// The record's own 90 kHz PTS.
    pub klv_pts_90k: u64,
    /// The KLV record.
    pub klv: Vec<u8>,
}

/// The push side of one mount. Cloning it is cheap; every clone feeds
/// the same mount.
#[derive(Clone)]
pub struct MountHandle {
    pub(crate) state: Arc<MountState>,
}

impl std::fmt::Debug for MountHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountHandle")
            .field("path", &self.state.path)
            .finish_non_exhaustive()
    }
}

/// A mount's shared state.
pub(crate) struct MountState {
    pub(crate) id: MountId,
    pub(crate) path: String,
    pub(crate) cfg: MountConfig,
    pub(crate) closed: AtomicBool,
    /// Set when the media queue dropped a frame, cleared when a keyframe
    /// is queued: until then the frames that follow would reference the
    /// lost one, so they are dropped too.
    video_gap: AtomicBool,
    /// Never held across a queue send.
    inner: Mutex<MountInner>,
    /// The demuxer behind `push_ts`; `Some` on an MP2T mount. Held while
    /// its events are dispatched, which takes `inner` and never blocks.
    demuxer: Mutex<Option<Mp2tInput>>,
    pub(crate) tx: SyncSender<MediaMsg>,
    pub(crate) wake: Waker,
    pub(crate) stats: MountCounters,
    /// The server's control path, for the session table; `None` for a
    /// mount driven without a server.
    control: Option<ControlLink>,
}

/// The state a push call changes.
/// An MP2T mount's demuxer and the stream it plays.
struct Mp2tInput {
    demuxer: Demuxer,
    /// The video stream the mount plays, once one of the mount's codec is
    /// seen. One packetizer and one parameter-set cache serve the mount,
    /// so a second video stream (a second sensor in the multiplex) would
    /// interleave two pictures into one track.
    playing: Option<Playing>,
}

/// The video stream an MP2T mount plays; KLV binds to its frames only
/// from the same program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Playing {
    pid: u16,
    program_number: u16,
}

impl Mp2tInput {
    /// Picks the stream to play from `map` when none is chosen yet, and
    /// follows a PMT update of the playing program (its video may move to
    /// another PID).
    fn on_program_map(&mut self, map: &ProgramMap, codec: VideoCodec) {
        if self
            .playing
            .is_some_and(|p| p.program_number != map.program_number)
        {
            return;
        }
        let first = map
            .streams
            .iter()
            .find(|st| st.kind == StreamKind::Video(codec))
            .map(|st| Playing {
                pid: st.pid,
                program_number: map.program_number,
            });
        if first.is_some() || self.playing.is_some() {
            self.playing = first;
        }
    }

    /// Whether a video access unit of the mount's codec on `stream` is
    /// played; the first one seen chooses the stream when no PMT has.
    fn plays_video(&mut self, stream: StreamId) -> bool {
        let this = Playing {
            pid: stream.pid,
            program_number: stream.program_number,
        };
        *self.playing.get_or_insert(this) == this
    }

    /// Whether KLV on `stream` binds to the played frames.
    fn plays_klv(&self, stream: StreamId) -> bool {
        self.playing
            .is_some_and(|p| p.program_number == stream.program_number)
    }
}

struct MountInner {
    packetizer: Box<dyn Packetizer>,
    /// The latest parameter set of each kind seen in any access unit.
    ps: ParameterSets,
    /// KLV records (PTS, bytes) waiting for a frame, in push order.
    pending_klv: VecDeque<(i64, Vec<u8>)>,
    video_clock: RtpClock,
    audio_clock: RtpClock,
}

/// One media stream's RTP timestamp clock.
#[derive(Default)]
struct RtpClock {
    /// Added to a tick count (truncated to 32 bits) to give the RTP
    /// timestamp.
    base: u32,
    /// The previous frame's PTS and RTP timestamp.
    last: Option<(i64, u32)>,
}

/// One frame's place on an [`RtpClock`].
struct ClockStep {
    /// The frame's RTP timestamp.
    rtp: u32,
    /// The PTS jumped out of the window and the clock was rebased.
    rebased: bool,
    /// The PTS is below the previous frame's, within the window.
    reordered: bool,
}

impl RtpClock {
    /// The RTP timestamp of a frame at `pts`, whose tick count on this
    /// stream's clock is `ticks`. A PTS jump of more than 10 s back or 1 h
    /// ahead rebases the clock so the RTP timestamp moves on by `step`
    /// ticks; a smaller step backwards is reported as reordered.
    fn ts(&mut self, pts: i64, ticks: u32, step: u32) -> ClockStep {
        let mut rebased = false;
        let mut reordered = false;
        match self.last {
            None => self.base = random_u32().wrapping_sub(ticks),
            Some((last_pts, last_rtp)) => {
                if pts < last_pts.saturating_sub(REBASE_BACKWARDS_TICKS)
                    || pts > last_pts.saturating_add(REBASE_FORWARDS_TICKS)
                {
                    self.base = last_rtp.wrapping_add(step).wrapping_sub(ticks);
                    rebased = true;
                } else if pts < last_pts {
                    reordered = true;
                }
            }
        }
        let rtp = ticks.wrapping_add(self.base);
        self.last = Some((pts, rtp));
        ClockStep {
            rtp,
            rebased,
            reordered,
        }
    }
}

impl MountState {
    /// A mount with a validated configuration.
    pub(crate) fn new(
        id: MountId,
        path: String,
        cfg: MountConfig,
        tx: SyncSender<MediaMsg>,
        wake: Waker,
        control: Option<ControlLink>,
    ) -> Result<Self, WhepServerError> {
        cfg.validate()?;
        let cfg_kind = cfg.kind;
        let packetizer =
            packetizer_for(cfg.video).ok_or_else(|| WhepServerError::InvalidConfig {
                detail: format!("no packetizer for {:?}", cfg.video),
            })?;
        Ok(Self {
            id,
            path,
            cfg,
            closed: AtomicBool::new(false),
            video_gap: AtomicBool::new(false),
            inner: Mutex::new(MountInner {
                packetizer,
                ps: ParameterSets::default(),
                pending_klv: VecDeque::new(),
                video_clock: RtpClock::default(),
                audio_clock: RtpClock::default(),
            }),
            demuxer: Mutex::new(match cfg_kind {
                MountKind::Mp2t => Some(Mp2tInput {
                    demuxer: Demuxer::with_config(DemuxerConfig::default()),
                    playing: None,
                }),
                _ => None,
            }),
            tx,
            wake,
            stats: MountCounters::default(),
            control,
        })
    }

    /// The `profile-level-id` bytes of the latest cached H.264 SPS, which
    /// steer the payload choice when a viewer's offer is negotiated.
    pub(crate) fn sps_profile_level_id(&self) -> Option<[u8; 3]> {
        let inner = self.lock();
        let sps = inner.ps.sps.last()?;
        crate::server::negotiate::profile_level_id_from_sps(self.cfg.video, sps)
    }

    fn lock(&self) -> MutexGuard<'_, MountInner> {
        // The state stays consistent between statements, so a panic in
        // another push leaves nothing half-written worth refusing over.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `Closed` once the mount is closed, `WrongMountKind` when the call
    /// belongs to the other mount kind.
    fn check(&self, kind: MountKind) -> Result<(), PushError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PushError::Closed);
        }
        if self.cfg.kind != kind {
            return Err(PushError::WrongMountKind);
        }
        Ok(())
    }

    /// Queue `msgs` for the media thread, in order, without blocking.
    ///
    /// When the frame (the first message) is dropped, the KLV messages
    /// bound to it are dropped too, as they name its RTP timestamp. A
    /// dropped frame counts its RTP payloads in
    /// `viewer_packets_dropped_queue_full`, a dropped KLV message counts
    /// one in `klv_dc_dropped_viewer`.
    ///
    /// After a dropped frame, every later frame that is not a keyframe is
    /// dropped and counted the same way, with its KLV messages, until a
    /// keyframe is queued: viewers resume at a picture they can decode
    /// rather than at frames that reference the lost one.
    fn send(&self, msgs: Vec<MediaMsg>) -> Result<(), PushError> {
        let mut sent = false;
        let mut dropping = matches!(
            msgs.first(),
            Some(MediaMsg::Frame { packet, .. }) if !packet.keyframe
        ) && self.video_gap.load(Ordering::Acquire);
        for msg in msgs {
            if dropping {
                self.count_dropped(&msg);
                continue;
            }
            let keyframe = match &msg {
                MediaMsg::Frame { packet, .. } => Some(packet.keyframe),
                _ => None,
            };
            match self.tx.try_send(msg) {
                Ok(()) => {
                    sent = true;
                    if keyframe == Some(true) {
                        self.video_gap.store(false, Ordering::Release);
                    }
                }
                Err(TrySendError::Full(msg)) => {
                    self.count_dropped(&msg);
                    if keyframe.is_some() {
                        self.video_gap.store(true, Ordering::Release);
                    }
                    dropping = !sent;
                }
                Err(TrySendError::Disconnected(_)) => return Err(PushError::Closed),
            }
        }
        if sent {
            self.wake.wake();
        }
        Ok(())
    }
}

impl MountState {
    /// Counts a message the full media queue did not take.
    fn count_dropped(&self, msg: &MediaMsg) {
        match msg {
            MediaMsg::Frame { packet, .. } => {
                self.stats
                    .viewer_packets_dropped_queue_full
                    .fetch_add(packet.payloads.len() as u64, Ordering::Relaxed);
            }
            MediaMsg::Klv { .. } => bump(&self.stats.klv_dc_dropped_viewer),
            MediaMsg::Audio { .. } => bump(&self.stats.viewer_packets_dropped_queue_full),
            MediaMsg::MountRemoved(_) | MediaMsg::Stop => {}
        }
    }
}

/// `true` for a picture a viewer can start decoding at: an H.264 IDR
/// (type 5) or an H.265 IRAP picture (BLA, IDR or CRA, types 16-21,
/// H.265 §7.4.2.2). Local because tst-core's `is_idr` leaves out CRA and
/// BLA and it has no public IRAP predicate.
fn is_random_access(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => nal_type == 5,
        VideoCodec::H265 => matches!(nal_type, 16..=21),
        VideoCodec::H266 | VideoCodec::Av1 => false,
    }
}

/// `true` for an H.264 SPS/PPS or an H.265 VPS/SPS/PPS NAL type.
fn is_parameter_set(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => matches!(nal_type, 7 | 8),
        VideoCodec::H265 => matches!(nal_type, 32..=34),
        VideoCodec::H266 | VideoCodec::Av1 => false,
    }
}

/// `true` for an access unit delimiter NAL type (H.264 9, H.265 35).
fn is_aud(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => nal_type == 9,
        VideoCodec::H265 => nal_type == 35,
        VideoCodec::H266 | VideoCodec::Av1 => false,
    }
}

/// `true` when `ps` holds every kind of parameter set `codec` needs.
fn is_complete(codec: VideoCodec, ps: &ParameterSets) -> bool {
    let vps_ok = codec != VideoCodec::H265 || !ps.vps.is_empty();
    vps_ok && !ps.sps.is_empty() && !ps.pps.is_empty()
}

/// The parameter sets in decoding order: VPS, SPS, PPS.
fn ordered(ps: &ParameterSets) -> Vec<Vec<u8>> {
    ps.vps
        .iter()
        .chain(&ps.sps)
        .chain(&ps.pps)
        .cloned()
        .collect()
}

/// A PTS as the non-negative 90 kHz value carried to the viewer; a
/// negative PTS is carried as 0.
fn pts_u64(pts: i64) -> u64 {
    u64::try_from(pts).unwrap_or(0)
}

/// A random 32-bit RTP timestamp origin (RFC 3550 §5.1). Should the
/// system's random source fail, the origin is 0, which is still a valid
/// timestamp.
fn random_u32() -> u32 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    u32::from_be_bytes(bytes)
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

impl MountInner {
    /// Replace each kind of cached parameter set the access unit carries.
    fn cache(&mut self, own: &ParameterSets) {
        if !own.vps.is_empty() {
            self.ps.vps.clone_from(&own.vps);
        }
        if !own.sps.is_empty() {
            self.ps.sps.clone_from(&own.sps);
        }
        if !own.pps.is_empty() {
            self.ps.pps.clone_from(&own.pps);
        }
    }

    /// Take the KLV records that bind to a frame at `pts`: every record
    /// at or before it, or every waiting record after a clock rebase
    /// (their PTS belongs to the old timeline).
    fn take_klv(&mut self, pts: i64, all: bool) -> Vec<(i64, Vec<u8>)> {
        if all {
            return self.pending_klv.drain(..).collect();
        }
        let (bound, keep) = self
            .pending_klv
            .drain(..)
            .partition(|(klv_pts, _)| *klv_pts <= pts);
        self.pending_klv = keep;
        bound.into_iter().collect()
    }
}

impl MountHandle {
    /// A mount driven without a server, and the receiving end of its
    /// media queue.
    #[doc(hidden)]
    pub fn for_test(cfg: MountConfig) -> (MountHandle, Receiver<MediaMsg>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(4096);
        let state = MountState::new(MountId(0), "test".into(), cfg, tx, Waker::for_test(), None)
            .expect("for_test takes a valid MountConfig");
        (
            MountHandle {
                state: Arc::new(state),
            },
            rx,
        )
    }

    /// Close the mount; later pushes return [`PushError::Closed`].
    #[doc(hidden)]
    pub fn close_for_test(&self) {
        self.state.closed.store(true, Ordering::Release);
    }

    /// The mount's path.
    pub fn mount_path(&self) -> &str {
        &self.state.path
    }

    /// The mount's configuration.
    pub fn config(&self) -> &MountConfig {
        &self.state.cfg
    }

    /// Push one Annex-B video access unit with its 90 kHz PTS.
    ///
    /// The NAL units decide whether the frame is a keyframe: an
    /// IDR/CRA/BLA (IRAP) picture with its parameter sets, carried in the access unit or
    /// cached from an earlier one and prepended. `key_frame` is advisory;
    /// a disagreement is counted in
    /// [`MountStats::keyframe_flag_mismatch`]. An IRAP picture before any
    /// parameter set was seen is forwarded as a plain frame, so viewers
    /// keep waiting for a decodable start.
    ///
    /// KLV records waiting with a PTS at or before `pts` are bound to this
    /// frame: each goes into an SEI message before the first slice while
    /// the mount's `sei_budget_bytes` allows, and onto the data channel,
    /// as the mount's [`KlvCarriage`] says. An access unit without slice
    /// data (parameter sets alone, for instance) updates the cache and
    /// sends nothing.
    ///
    /// A full media queue drops the frame and counts it, and later frames
    /// up to the next keyframe are dropped and counted too (they would
    /// reference the lost one); the call never
    /// blocks.
    ///
    /// WebRTC carries frames in decode order with the RTP timestamp as the
    /// presentation time. A source with B-frames (a PTS below the previous
    /// frame's) is counted in [`MountStats::frames_pts_reordered`] and
    /// plays badly; feed I/P-only streams.
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed, and
    /// [`PushError::InvalidAccessUnit`] when `au` holds no NAL unit.
    pub fn push_video(&self, au: &[u8], pts: Pts90khz, key_frame: bool) -> Result<(), PushError> {
        self.state.check(MountKind::Elementary)?;
        self.video(au, pts, key_frame)
    }

    /// The video path shared by `push_video` and the MP2T demuxer.
    fn video(&self, au: &[u8], pts: Pts90khz, key_frame: bool) -> Result<(), PushError> {
        let s = &*self.state;
        let codec = s.cfg.video;
        let types: Vec<Option<u8>> = annexb::nals(au)
            .filter(|nal| !nal.is_empty())
            .map(|nal| nal_unit_type(codec, nal))
            .collect();
        if types.is_empty() {
            return Err(PushError::InvalidAccessUnit {
                detail: "no NAL units".into(),
            });
        }
        let has = |f: fn(VideoCodec, u8) -> bool| types.iter().flatten().any(|&t| f(codec, t));
        let has_vcl = has(is_vcl);
        let has_irap = has(is_random_access);
        bump(&s.stats.frames_pushed);
        if key_frame != has_irap {
            bump(&s.stats.keyframe_flag_mismatch);
        }
        let own = extract_parameter_sets(au, codec);
        let pts = pts.as_ticks();

        let msgs = {
            let mut inner = s.lock();
            inner.cache(&own);
            if !has_vcl {
                return Ok(());
            }
            let keyframe = has_irap && is_complete(codec, &inner.ps);
            let (mut buf, parameter_sets) = if !keyframe {
                (au.to_vec(), None)
            } else if is_complete(codec, &own) {
                (au.to_vec(), Some(ordered(&own)))
            } else {
                // Leading AUD, then every cached parameter set in order,
                // then the access unit's other NAL units.
                let cached = ordered(&inner.ps);
                let mut out = Vec::with_capacity(
                    au.len() + cached.iter().map(|n| n.len() + 4).sum::<usize>(),
                );
                let mut nals = annexb::nals(au).filter(|nal| !nal.is_empty()).peekable();
                if let Some(first) =
                    nals.next_if(|nal| nal_unit_type(codec, nal).is_some_and(|t| is_aud(codec, t)))
                {
                    push_nal(&mut out, first);
                }
                for nal in &cached {
                    push_nal(&mut out, nal);
                }
                for nal in nals {
                    if !nal_unit_type(codec, nal).is_some_and(|t| is_parameter_set(codec, t)) {
                        push_nal(&mut out, nal);
                    }
                }
                (out, Some(cached))
            };
            if keyframe {
                bump(&s.stats.keyframes_pushed);
            }

            let ClockStep {
                rtp: rtp_ts,
                rebased,
                reordered,
            } = inner.video_clock.ts(pts, pts as u32, REBASE_STEP_TICKS);
            if rebased {
                bump(&s.stats.rtp_clock_rebases);
            }
            if reordered {
                bump(&s.stats.frames_pts_reordered);
            }
            let bound = inner.take_klv(pts, rebased);

            let to_sei = matches!(
                s.cfg.klv,
                KlvCarriage::SeiAndDataChannel | KlvCarriage::SeiOnly
            );
            let to_dc = matches!(
                s.cfg.klv,
                KlvCarriage::SeiAndDataChannel | KlvCarriage::DataChannelOnly
            );
            if to_sei {
                let budget = s.cfg.sei_budget_bytes as usize;
                let mut used = 0usize;
                // The SEI NAL units that fit, joined by start codes, spliced
                // into the access unit once.
                let mut seis: Vec<u8> = Vec::new();
                for (klv_pts, klv) in &bound {
                    // Fails only for a codec validate() refuses.
                    let Ok(sei) = crate::sei::build_klv_sei_nal(codec, pts_u64(*klv_pts), klv)
                    else {
                        continue;
                    };
                    // The escaped NAL's real size plus its start code.
                    let cost = sei.len() + 3;
                    if used + cost > budget {
                        bump(&s.stats.klv_sei_dropped_oversize);
                        continue;
                    }
                    if !seis.is_empty() {
                        seis.extend_from_slice(&[0, 0, 1]);
                    }
                    seis.extend_from_slice(&sei);
                    used += cost;
                    bump(&s.stats.klv_sei_inserted);
                }
                if !seis.is_empty() {
                    // Fails only for an access unit without a slice,
                    // excluded above. `seis` joins every NAL unit with a
                    // start code, so this one byte splice inserts all of
                    // them before the first slice.
                    if let Ok(spliced) = insert_sei_before_first_vcl(&buf, &seis, codec) {
                        buf = spliced;
                    }
                }
            }

            let nals: Vec<&[u8]> = annexb::nals(&buf).filter(|nal| !nal.is_empty()).collect();
            let payloads = inner.packetizer.packetize(&nals, DEFAULT_MTU_PAYLOAD);
            drop(inner);

            let mut msgs = Vec::with_capacity(1 + bound.len());
            msgs.push(MediaMsg::Frame {
                mount: s.id,
                packet: Arc::new(FramePacket {
                    rtp_ts,
                    keyframe,
                    codec,
                    payloads,
                    parameter_sets,
                }),
            });
            if to_dc {
                for (klv_pts, klv) in bound {
                    msgs.push(MediaMsg::Klv {
                        mount: s.id,
                        msg: Arc::new(KlvMessage {
                            rtp_ts,
                            klv_pts_90k: pts_u64(klv_pts),
                            klv,
                        }),
                    });
                }
            }
            msgs
        };
        s.send(msgs)
    }

    /// Push one KLV record (raw local-set bytes) with its 90 kHz PTS.
    ///
    /// The record waits for the first frame whose PTS is at or after its
    /// own; a record older than every later frame binds to the next one.
    /// A clock rebase (a PTS jump of more than 10 s back or 1 h ahead)
    /// binds every waiting record to the rebasing frame.
    /// At most 1024 records wait; beyond that the oldest is dropped and
    /// counted in [`MountStats::klv_records_dropped_unbound`].
    /// `metadata_service_id` is accepted for parity with the RTSP mount's
    /// `push_klv` and ignored: a WebRTC mount carries one KLV stream.
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed, and
    /// [`PushError::KlvTooLarge`] for a record over [`KLV_DC_LIMIT`]
    /// (256 KiB).
    ///
    /// The server's SCTP send buffer is sized for a record at the limit. A
    /// record is dropped for a viewer, and counted in
    /// [`MountStats::klv_dc_dropped_viewer`], when that viewer's buffer has
    /// no room for it. A record is sent as one data-channel message of the
    /// record plus a 13-byte header; for a viewer whose `max-message-size`
    /// is smaller than that message the record is dropped the same way. The
    /// size is 64 KiB when the viewer states none and 256 KiB in current
    /// Chrome and Safari, so their largest record is 262 131 bytes; Firefox
    /// takes any record up to the limit.
    pub fn push_klv(
        &self,
        record: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), PushError> {
        self.state.check(MountKind::Elementary)?;
        self.klv(record, pts, metadata_service_id)
    }

    /// The KLV path shared by `push_klv` and the MP2T demuxer.
    fn klv(&self, record: &[u8], pts: Pts90khz, metadata_service_id: u8) -> Result<(), PushError> {
        let _ = metadata_service_id;
        let s = &*self.state;
        let len = record.len() as u64;
        if len > KLV_DC_LIMIT {
            bump(&s.stats.klv_dc_dropped_oversize);
            return Err(PushError::KlvTooLarge {
                len,
                limit: KLV_DC_LIMIT,
            });
        }
        let mut inner = s.lock();
        if inner.pending_klv.len() >= PENDING_KLV_CAP {
            inner.pending_klv.pop_front();
            bump(&s.stats.klv_records_dropped_unbound);
        }
        inner
            .pending_klv
            .push_back((pts.as_ticks(), record.to_vec()));
        drop(inner);
        bump(&s.stats.klv_records_pushed);
        Ok(())
    }

    /// Push one audio packet with its 90 kHz PTS.
    ///
    /// Under [`AudioPolicy::Drop`] the packet is discarded and counted in
    /// [`MountStats::audio_frames_dropped`]; nothing is checked.
    ///
    /// Under [`AudioPolicy::PassthroughOpus`] `frames` is exactly one Opus
    /// packet (RFC 6716 framing, up to three 1275-byte frames, 120 ms). It
    /// goes to every viewer that offered Opus as one RTP payload
    /// (RFC 7587), unchanged and counted in
    /// [`MountStats::audio_frames_pushed`]. The RTP timestamp is the PTS on
    /// the 48 kHz clock, `floor(pts * 8 / 15)` plus a random per-mount
    /// origin, computed from each PTS on its own so the rounding never
    /// accumulates (a 20 ms frame is exactly 960). A PTS jump of more than
    /// 10 s back or 1 h ahead rebases the audio clock on its own, as video's
    /// is: the timestamp moves on by one 20 ms frame
    /// ([`MountStats::rtp_clock_rebases`] counts both). The marker bit is
    /// never set (RFC 7587 sets it on the first packet after silence; a
    /// pass-through mount does not detect silence). A viewer whose queue is
    /// full loses the packet, counted in
    /// [`MountStats::viewer_packets_dropped_queue_full`]. The call never
    /// blocks.
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed,
    /// [`PushError::WrongMountKind`] on an MP2T mount, and
    /// [`PushError::InvalidAudioFrame`] under `PassthroughOpus` for an empty
    /// slice or one longer than 3825 bytes.
    pub fn push_audio(&self, frames: &[u8], pts: Pts90khz) -> Result<(), PushError> {
        self.state.check(MountKind::Elementary)?;
        self.audio(frames, pts)
    }

    /// The audio path of `push_audio`.
    fn audio(&self, frames: &[u8], pts: Pts90khz) -> Result<(), PushError> {
        let s = &*self.state;
        match s.cfg.audio {
            AudioPolicy::Drop => {
                bump(&s.stats.audio_frames_dropped);
                Ok(())
            }
            AudioPolicy::PassthroughOpus => {
                if frames.is_empty() || frames.len() > OPUS_MAX_PACKET_BYTES {
                    return Err(PushError::InvalidAudioFrame);
                }
                // 48 kHz ticks = 90 kHz ticks * 8 / 15, floored; the
                // cast keeps the low 32 bits, as RTP does.
                let pts = pts.as_ticks();
                let ticks = pts.saturating_mul(8).div_euclid(15);
                let ClockStep {
                    rtp: rtp_ts,
                    rebased,
                    ..
                } = s
                    .lock()
                    .audio_clock
                    .ts(pts, ticks as u32, AUDIO_REBASE_STEP_TICKS);
                if rebased {
                    bump(&s.stats.rtp_clock_rebases);
                }
                let msg = MediaMsg::Audio {
                    mount: s.id,
                    packet: Arc::new(AudioPacket {
                        rtp_ts,
                        payload: frames.to_vec(),
                    }),
                };
                s.send(vec![msg])?;
                bump(&s.stats.audio_frames_pushed);
                Ok(())
            }
        }
    }

    /// Push MPEG-TS bytes into an MP2T mount.
    ///
    /// The bytes go through the mount's demuxer, in slices of any size (a
    /// packet may span two calls). The mount plays one video stream: the
    /// first of the mount's codec the TS declares (or, without a PMT
    /// naming one, carries), with the KLV of that stream's program; access
    /// units and KLV records of every other stream are counted in
    /// [`MountStats::demux_nonconformant`] and dropped. Each played access
    /// unit takes the [`push_video`](Self::push_video) path with the TS
    /// random-access flag as `key_frame`; each played KLV record takes the
    /// [`push_klv`](Self::push_klv) path (the Metadata_AU_cell header is
    /// already removed by the demuxer). Audio is discarded and counted in
    /// [`MountStats::audio_frames_dropped`] whatever the mount's
    /// [`AudioPolicy`]: the demuxer delivers MPEG audio, AAC and AC-3 (it
    /// has no Opus mapping), and one event holds several frames, so no
    /// event is one Opus packet to forward.
    ///
    /// The demuxer emits an access unit when the next one starts, so a
    /// live mount runs one frame behind the bytes pushed, and the last
    /// access unit waits until more bytes arrive. A KLV record binds to the
    /// first frame whose PTS is at or after its own, so a record that
    /// arrives after its frame was emitted binds to the next frame.
    ///
    /// Stream damage never fails the call: a demuxer error, a
    /// non-conformance report, a video access unit of another codec or one
    /// without a NAL unit, and an oversize KLV record are counted
    /// ([`MountStats::demux_nonconformant`], and the KLV counters) and
    /// skipped.
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed, and
    /// [`PushError::WrongMountKind`] on an elementary mount.
    pub fn push_ts(&self, bytes: &[u8]) -> Result<(), PushError> {
        let s = &*self.state;
        s.check(MountKind::Mp2t)?;
        // The demuxer stays locked through the dispatch so concurrent
        // callers' events keep their order. Nothing below blocks: every
        // push path ends in `try_send`.
        let mut guard = s.demuxer.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(input) = guard.as_mut() else {
            return Err(PushError::WrongMountKind);
        };
        if let Err(e) = input.demuxer.feed(bytes) {
            tracing::debug!(mount = %s.path, error = %e, "MP2T demux error");
            bump(&s.stats.demux_nonconformant);
        }
        while let Some(event) = input.demuxer.next_event() {
            self.dispatch(event, input)?;
        }
        Ok(())
    }

    /// Routes one demuxed event into the elementary push paths.
    fn dispatch(&self, event: DemuxEvent, input: &mut Mp2tInput) -> Result<(), PushError> {
        let s = &*self.state;
        let skipped = |r: Result<(), PushError>| match r {
            Err(PushError::Closed) => Err(PushError::Closed),
            Err(e) => {
                tracing::debug!(mount = %s.path, error = %e, "MP2T unit skipped");
                bump(&s.stats.demux_nonconformant);
                Ok(())
            }
            Ok(()) => Ok(()),
        };
        match event {
            DemuxEvent::ProgramMap(map) => {
                input.on_program_map(&map, s.cfg.video);
                Ok(())
            }
            DemuxEvent::Sample {
                stream,
                pts,
                payload:
                    SamplePayload::Video {
                        codec,
                        raw,
                        random_access_indicator,
                        ..
                    },
                ..
            } => {
                if codec != s.cfg.video {
                    tracing::debug!(mount = %s.path, ?codec, "MP2T video codec differs from the mount's");
                    bump(&s.stats.demux_nonconformant);
                    return Ok(());
                }
                if !input.plays_video(stream) {
                    tracing::debug!(mount = %s.path, pid = stream.pid, "MP2T video stream not played");
                    bump(&s.stats.demux_nonconformant);
                    return Ok(());
                }
                skipped(self.video(&raw, pts, random_access_indicator))
            }
            DemuxEvent::Sample {
                pts,
                payload: SamplePayload::Audio { frames, .. },
                ..
            } => {
                // The demuxer's audio is MPEG-1/2 audio, AAC or AC-3, never
                // Opus, and one event holds several frames: nothing can be
                // forwarded as Opus, so it is discarded and counted.
                let _ = (frames, pts);
                bump(&s.stats.audio_frames_dropped);
                Ok(())
            }
            DemuxEvent::Metadata {
                stream,
                pts,
                kind,
                payload,
            } => {
                if !input.plays_klv(stream) {
                    tracing::debug!(mount = %s.path, pid = stream.pid, "MP2T KLV outside the played program");
                    bump(&s.stats.demux_nonconformant);
                    return Ok(());
                }
                let service_id = match kind {
                    MetadataKind::KlvSyncAuCell {
                        metadata_service_id,
                        ..
                    } => metadata_service_id,
                    _ => 0,
                };
                // An oversize record is counted by `klv` itself.
                match self.klv(&payload, pts, service_id) {
                    Err(PushError::Closed) => Err(PushError::Closed),
                    _ => Ok(()),
                }
            }
            DemuxEvent::NonConformant { .. } => {
                bump(&s.stats.demux_nonconformant);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A snapshot of the mount's counters.
    pub fn stats(&self) -> MountStats {
        self.state.stats.snapshot()
    }

    /// Statistics of each viewer session on this mount, read from the
    /// server's media thread (a bounded wait of at most 2 seconds). Empty
    /// once the mount is closed, and for a mount driven without a server.
    pub fn sessions(&self) -> Vec<SessionStats> {
        let s = &*self.state;
        if s.closed.load(Ordering::Acquire) {
            return Vec::new();
        }
        let Some((_, sessions)) = s.control.as_ref().and_then(ControlLink::sessions) else {
            return Vec::new();
        };
        sessions
            .into_iter()
            .filter(|session| session.mount_path == s.path)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    const P: &[u8] = &[0, 0, 1, 0x41, 0x9A, 0x01];

    /// A mount whose media queue holds one message, and that queue's
    /// receiving end. Data channel only, so a P-frame stays one payload.
    fn one_slot_mount() -> (MountHandle, Receiver<MediaMsg>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let cfg = MountConfig {
            klv: KlvCarriage::DataChannelOnly,
            ..MountConfig::default()
        };
        let state =
            MountState::new(MountId(0), "test".into(), cfg, tx, Waker::for_test(), None).unwrap();
        let m = MountHandle {
            state: Arc::new(state),
        };
        (m, rx)
    }

    /// The keyframe flag of each frame waiting in the queue, draining it.
    fn drain(rx: &Receiver<MediaMsg>) -> Vec<bool> {
        rx.try_iter()
            .filter_map(|msg| match msg {
                MediaMsg::Frame { packet, .. } => Some(packet.keyframe),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn after_a_media_queue_drop_frames_wait_for_a_keyframe() {
        let (m, rx) = one_slot_mount();
        let key = [SPS, PPS, IDR].concat();
        m.push_video(&key, Pts90khz::new(0), true).unwrap();
        // The queue is full: this P-frame is dropped.
        m.push_video(P, Pts90khz::new(3000), false).unwrap();
        assert!(m.state.video_gap.load(Ordering::Acquire));
        assert_eq!(drain(&rx), [true]);

        // The queue has room, but the P-frame references the lost one.
        m.push_klv(b"a", Pts90khz::new(6000), 0).unwrap();
        m.push_video(P, Pts90khz::new(6000), false).unwrap();
        assert!(drain(&rx).is_empty());
        let s = m.stats();
        assert_eq!(s.viewer_packets_dropped_queue_full, 2);
        assert_eq!(s.klv_dc_dropped_viewer, 1);

        // A keyframe is queued and ends the wait.
        m.push_video(&key, Pts90khz::new(9000), true).unwrap();
        assert!(!m.state.video_gap.load(Ordering::Acquire));
        assert_eq!(drain(&rx), [true]);
        m.push_video(P, Pts90khz::new(12000), false).unwrap();
        assert_eq!(drain(&rx), [false]);
    }

    #[test]
    fn a_full_media_queue_counts_frames_in_payloads_and_klv_apart() {
        let (m, rx) = one_slot_mount();
        m.push_video(&[SPS, PPS, IDR].concat(), Pts90khz::new(0), true)
            .unwrap();
        // The queue holds the keyframe: the next frame (one payload) and
        // the two KLV messages bound to it are dropped.
        m.push_klv(b"a", Pts90khz::new(3000), 0).unwrap();
        m.push_klv(b"b", Pts90khz::new(3000), 0).unwrap();
        m.push_video(P, Pts90khz::new(3000), false).unwrap();
        let s = m.stats();
        assert_eq!(s.viewer_packets_dropped_queue_full, 1);
        assert_eq!(s.klv_dc_dropped_viewer, 2);
        drop(rx);
    }
}
