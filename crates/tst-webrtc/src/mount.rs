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

use crate::config::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec};
use crate::error::{PushError, WhepServerError};
use crate::packetize::{DEFAULT_MTU_PAYLOAD, Packetizer, RtpPayload, packetizer_for};
use crate::server::media::Waker;
use crate::stats::{MountCounters, MountStats, SessionStats, bump};

/// The largest KLV record a data-channel message carries (256 KiB).
const KLV_DC_LIMIT: usize = 256 * 1024;

/// KLV records waiting for a frame; when full the oldest is dropped.
const PENDING_KLV_CAP: usize = 1024;

/// The fixed SEI bytes around each record's KLV: the 16-byte UUID, the
/// version byte and the 8-byte PTS.
const SEI_RECORD_PREFIX: usize = 25;

/// The SEI message and NAL framing around one record: NAL header,
/// payload type and size, the trailing bits and the start code, with
/// room for emulation-prevention bytes.
const SEI_RECORD_FRAMING: usize = 8;

/// A PTS more than this far behind the previous frame rebases the RTP
/// clock (10 s).
const REBASE_BACKWARDS_TICKS: i64 = 90_000 * 10;

/// A PTS more than this far ahead of the previous frame rebases the RTP
/// clock (1 h).
const REBASE_FORWARDS_TICKS: i64 = 90_000 * 3600;

/// The RTP timestamp step a rebase advances by: one frame at 30 frames
/// per second.
const REBASE_STEP_TICKS: u32 = 3000;

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
        /// The audio payloads.
        packet: Arc<FramePacket>,
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
    /// Never held across a queue send.
    inner: Mutex<MountInner>,
    pub(crate) tx: SyncSender<MediaMsg>,
    pub(crate) wake: Waker,
    pub(crate) stats: MountCounters,
}

/// The state a push call changes.
struct MountInner {
    packetizer: Box<dyn Packetizer>,
    /// The latest parameter set of each kind seen in any access unit.
    ps: ParameterSets,
    /// KLV records (PTS, bytes) waiting for a frame, in push order.
    pending_klv: VecDeque<(i64, Vec<u8>)>,
    /// Added to a PTS (truncated to 32 bits) to give its RTP timestamp.
    rtp_base: u32,
    /// The previous frame's PTS and RTP timestamp.
    last: Option<(i64, u32)>,
}

impl MountState {
    /// A mount with a validated configuration.
    pub(crate) fn new(
        id: MountId,
        path: String,
        cfg: MountConfig,
        tx: SyncSender<MediaMsg>,
        wake: Waker,
    ) -> Result<Self, WhepServerError> {
        cfg.validate()?;
        let packetizer =
            packetizer_for(cfg.video).ok_or_else(|| WhepServerError::InvalidConfig {
                detail: format!("no packetizer for {:?}", cfg.video),
            })?;
        Ok(Self {
            id,
            path,
            cfg,
            closed: AtomicBool::new(false),
            inner: Mutex::new(MountInner {
                packetizer,
                ps: ParameterSets::default(),
                pending_klv: VecDeque::new(),
                rtp_base: 0,
                last: None,
            }),
            tx,
            wake,
            stats: MountCounters::default(),
        })
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
    /// bound to it are dropped too, as they name its RTP timestamp.
    fn send(&self, msgs: Vec<MediaMsg>) -> Result<(), PushError> {
        let mut sent = false;
        let mut dropping = false;
        for msg in msgs {
            if dropping {
                bump(&self.stats.viewer_packets_dropped_queue_full);
                continue;
            }
            match self.tx.try_send(msg) {
                Ok(()) => sent = true,
                Err(TrySendError::Full(_)) => {
                    bump(&self.stats.viewer_packets_dropped_queue_full);
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

/// `true` for a picture a viewer can start decoding at: an H.264 IDR
/// (type 5) or an H.265 IRAP picture (BLA, IDR or CRA, types 16-21,
/// H.265 §7.4.2.2). Local because tst-core's `is_idr` leaves out CRA and
/// BLA and it has no public IRAP predicate.
fn is_random_access(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => nal_type == 5,
        VideoCodec::H265 => matches!(nal_type, 16..=21),
        _ => false,
    }
}

/// `true` for an H.264 SPS/PPS or an H.265 VPS/SPS/PPS NAL type.
fn is_parameter_set(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => matches!(nal_type, 7 | 8),
        VideoCodec::H265 => matches!(nal_type, 32..=34),
        _ => false,
    }
}

/// `true` for an access unit delimiter NAL type (H.264 9, H.265 35).
fn is_aud(codec: VideoCodec, nal_type: u8) -> bool {
    match codec {
        VideoCodec::H264 => nal_type == 9,
        VideoCodec::H265 => nal_type == 35,
        _ => false,
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

    /// The RTP timestamp of a frame at `pts`. A jump of more than 10 s
    /// back or 1 h ahead rebases the clock so the RTP timestamp moves on
    /// by one frame step; returns whether it rebased.
    fn rtp_ts(&mut self, pts: i64) -> (u32, bool) {
        let pts32 = pts as u32;
        let mut rebased = false;
        match self.last {
            None => self.rtp_base = random_u32().wrapping_sub(pts32),
            Some((last_pts, last_rtp)) => {
                if pts < last_pts.saturating_sub(REBASE_BACKWARDS_TICKS)
                    || pts > last_pts.saturating_add(REBASE_FORWARDS_TICKS)
                {
                    self.rtp_base = last_rtp.wrapping_add(REBASE_STEP_TICKS).wrapping_sub(pts32);
                    rebased = true;
                }
            }
        }
        let rtp = pts32.wrapping_add(self.rtp_base);
        self.last = Some((pts, rtp));
        (rtp, rebased)
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
        let state = MountState::new(MountId(0), "test".into(), cfg, tx, Waker::for_test())
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
    /// A full media queue drops the frame and counts it; the call never
    /// blocks.
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed,
    /// [`PushError::WrongMountKind`] on an MP2T mount, and
    /// [`PushError::InvalidAccessUnit`] when `au` holds no NAL unit.
    pub fn push_video(&self, au: &[u8], pts: Pts90khz, key_frame: bool) -> Result<(), PushError> {
        let s = &*self.state;
        s.check(MountKind::Elementary)?;
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

            let (rtp_ts, rebased) = inner.rtp_ts(pts);
            if rebased {
                bump(&s.stats.rtp_clock_rebases);
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
                for (klv_pts, klv) in &bound {
                    let cost = SEI_RECORD_PREFIX + klv.len() + SEI_RECORD_FRAMING;
                    if used + cost > budget {
                        bump(&s.stats.klv_sei_dropped_oversize);
                        continue;
                    }
                    // Both steps fail only for a codec validate() refuses
                    // or an access unit without a slice, excluded above.
                    let spliced = crate::sei::build_klv_sei_nal(codec, pts_u64(*klv_pts), klv)
                        .ok()
                        .and_then(|sei| insert_sei_before_first_vcl(&buf, &sei, codec).ok());
                    if let Some(spliced) = spliced {
                        buf = spliced;
                        used += cost;
                        bump(&s.stats.klv_sei_inserted);
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
    /// [`PushError::Closed`] after the mount closed,
    /// [`PushError::WrongMountKind`] on an MP2T mount, and
    /// [`PushError::KlvTooLarge`] for a record over 256 KiB.
    pub fn push_klv(
        &self,
        record: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), PushError> {
        let _ = metadata_service_id;
        let s = &*self.state;
        s.check(MountKind::Elementary)?;
        if record.len() > KLV_DC_LIMIT {
            bump(&s.stats.klv_dc_dropped_oversize);
            return Err(PushError::KlvTooLarge {
                len: record.len() as u64,
                limit: KLV_DC_LIMIT as u64,
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

    /// Push audio frames with their 90 kHz PTS.
    ///
    /// No audio track is negotiated with viewers in this build, so every
    /// frame is discarded and counted in
    /// [`MountStats::audio_frames_dropped`], whatever the mount's
    /// [`AudioPolicy`].
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed and
    /// [`PushError::WrongMountKind`] on an MP2T mount.
    pub fn push_audio(&self, frames: &[u8], pts: Pts90khz) -> Result<(), PushError> {
        let _ = (frames, pts);
        let s = &*self.state;
        s.check(MountKind::Elementary)?;
        match s.cfg.audio {
            AudioPolicy::Drop | AudioPolicy::PassthroughOpus => {
                bump(&s.stats.audio_frames_dropped);
            }
        }
        Ok(())
    }

    /// Push MPEG-TS bytes into an MP2T mount.
    ///
    /// This build has no demuxer behind an MP2T mount, so the call is
    /// refused on both mount kinds.
    ///
    /// # Errors
    ///
    /// [`PushError::Closed`] after the mount closed, and
    /// [`PushError::WrongMountKind`] otherwise.
    pub fn push_ts(&self, bytes: &[u8]) -> Result<(), PushError> {
        let _ = bytes;
        let s = &*self.state;
        s.check(MountKind::Mp2t)?;
        // MP2T mounts arrive with the demuxer integration.
        Err(PushError::WrongMountKind)
    }

    /// A snapshot of the mount's counters.
    pub fn stats(&self) -> MountStats {
        self.state.stats.snapshot()
    }

    /// Statistics of each viewer session on this mount. The session table
    /// belongs to the server's media thread; a mount without one has no
    /// sessions.
    pub fn sessions(&self) -> Vec<SessionStats> {
        Vec::new()
    }
}
