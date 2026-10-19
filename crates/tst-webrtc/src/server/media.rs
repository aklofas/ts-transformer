//! The server's media thread.
//!
//! One thread owns one UDP socket and every viewer session. Each session
//! is a sans-IO str0m `Rtc`: the thread feeds it datagrams and timeouts,
//! drains its output after every input, and sends what it emits from the
//! one socket. Datagrams are routed to the first session whose `Rtc`
//! accepts them, so every viewer shares the socket's port.
//!
//! Mounts queue frames and KLV messages on a bounded channel and wake the
//! thread through a [`Waker`]: a one-byte datagram from a second, local
//! socket, which ends the media socket's bounded read. The control side
//! (offers, deletes, statistics) sends [`ControlMsg`]s and wakes the
//! thread the same way. Time comes only from the server's [`Clock`].

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::channel::ChannelConfig;
use str0m::media::Pt;
use str0m::net::{Protocol, Receive};
use str0m::rtp::{RtpWrite, SeqNo};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tracing::debug;

use crate::clock::Clock;
use crate::mount::{FramePacket, KlvMessage, MediaMsg, MountId, MountState};
use crate::server::negotiate::{Negotiated, select_video};
use crate::server::session::{Enqueued, Queued, ReapCause, Viewer, reap_decision};
use crate::stats::{IceState, ServerStats, SessionStats, bump};

/// The longest the thread waits for a datagram before it re-checks its
/// timers.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// The shortest read timeout (a zero timeout is an error on a socket).
const MIN_WAIT: Duration = Duration::from_millis(1);

/// How soon a session whose `poll_output` failed is polled again.
const ERROR_RETRY: Duration = Duration::from_millis(10);

/// The most mount messages handled per pass, so a busy mount cannot keep
/// the thread from its socket.
const MEDIA_BATCH: usize = 1024;

/// The largest datagram read.
const DATAGRAM_MAX: usize = 65_535;

/// The label of the negotiated KLV data channel.
const KLV_LABEL: &str = "klv";

/// The data-channel message format version (the body's first byte).
const KLV_DC_VERSION: u8 = 1;

/// Constrained High, level 3.1: the H.264 profile Safari offers first. str0m
/// matches profiles exactly and has no Constrained High entry of its own.
const H264_CONSTRAINED_HIGH: u32 = 0x640c1f;

/// Wakes the media thread after a message was queued for it.
///
/// A wake is one datagram; `pending` suppresses further datagrams until
/// the thread has looked at its queues again.
#[derive(Clone, Debug)]
pub(crate) struct Waker {
    target: Option<WakeTarget>,
}

#[derive(Clone, Debug)]
struct WakeTarget {
    sock: Arc<UdpSocket>,
    target: SocketAddr,
    pending: Arc<AtomicBool>,
}

impl Waker {
    /// A waker that wakes nothing, for a mount driven without a server.
    pub(crate) fn for_test() -> Self {
        Self { target: None }
    }

    /// Signal the media thread that a message is queued.
    pub(crate) fn wake(&self) {
        let Some(w) = &self.target else {
            return;
        };
        if !w.pending.swap(true, Ordering::AcqRel) && w.sock.send_to(&[0], w.target).is_err() {
            // Nothing is in flight; the next wake must try again.
            w.pending.store(false, Ordering::Release);
        }
    }
}

/// A browser's offer for a mount.
pub(crate) struct OfferRequest {
    /// The mount the viewer asked for.
    pub(crate) mount: MountId,
    /// The parsed offer.
    pub(crate) offer: SdpOffer,
    /// The address the request came from, for the log.
    pub(crate) remote_hint: Option<SocketAddr>,
    /// Where the outcome goes. The thread never blocks on it: the channel
    /// needs room for one message.
    pub(crate) reply: SyncSender<Result<CreatedSession, RejectReason>>,
}

/// A session the media thread created for an offer.
#[derive(Debug)]
pub(crate) struct CreatedSession {
    /// The session id, 32 hex characters.
    pub(crate) id: String,
    /// The SDP answer.
    pub(crate) answer: String,
}

/// Why an offer got no session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    /// The server's viewer cap is reached.
    Cap,
    /// The offer cannot carry the mount's video.
    Codec,
    /// The server is shutting down.
    Shutdown,
    /// Anything else: the mount is gone, str0m refused the offer, or the
    /// random source failed.
    Internal,
}

/// A request from the server's control side. Every reply channel needs
/// room for one message; the media thread never blocks on a reply.
/// The sender wakes the thread after queueing.
pub(crate) enum ControlMsg {
    /// Create a session for an offer.
    Offer(OfferRequest),
    /// Close a session; the reply says whether it existed.
    Delete {
        /// The session id.
        id: String,
        /// `true` when the session existed.
        reply: SyncSender<bool>,
    },
    /// A statistics snapshot.
    Stats {
        /// The thread's counters and every session's statistics.
        reply: SyncSender<(ServerCounters, Vec<SessionStats>)>,
    },
}

/// The media thread's configuration.
///
/// The local ICE candidates are `bind` itself when it names a specific
/// address, plus `advertise` (on the bound port) when set. An unspecified
/// `bind` (`0.0.0.0`, `::`) publishes `advertise` when set and the
/// loopback address of its family, nothing else: without `advertise` such
/// a server is reachable only from the same host, so any other deployment
/// sets `advertise=`.
#[derive(Debug, Clone)]
pub(crate) struct MediaConfig {
    /// The media socket's address (`media_port`).
    pub(crate) bind: SocketAddr,
    /// The address published to viewers in addition to (or instead of) the
    /// bound one.
    pub(crate) advertise: Option<IpAddr>,
    /// The most sessions open at once.
    pub(crate) max_viewers: usize,
    /// How long a session may take to connect (ICE and DTLS).
    pub(crate) ice_timeout: Duration,
    /// How long a session may stay disconnected, or its queue stay full,
    /// before it is closed.
    pub(crate) disconnect_timeout: Duration,
}

/// The counters the media thread owns. The server fills in the rest of
/// [`ServerStats`].
#[derive(Debug, Default, Clone)]
pub(crate) struct ServerCounters {
    /// Sessions open now.
    pub(crate) active_sessions: u64,
    /// Sessions created since the start.
    pub(crate) sessions_total: u64,
    /// Offers refused at the viewer cap.
    pub(crate) sessions_rejected_cap: u64,
    /// Offers refused for the codec.
    pub(crate) sessions_rejected_codec: u64,
    /// Sessions closed by the ICE timeout.
    pub(crate) sessions_reaped_ice_timeout: u64,
    /// Sessions closed because they stayed disconnected or their queue
    /// stayed full.
    pub(crate) sessions_reaped_disconnected: u64,
    /// RTP packets written to every session.
    pub(crate) rtp_packets_sent: u64,
    /// RTP payload bytes written to every session.
    pub(crate) rtp_bytes_sent: u64,
    /// Datagrams no session accepted.
    pub(crate) datagrams_unrouted: u64,
}

impl ServerCounters {
    /// The server statistics, with the two counts the media thread does not
    /// own: the number of mounts and the offers refused by authentication.
    pub(crate) fn into_stats(self, mounts: u64, sessions_rejected_auth: u64) -> ServerStats {
        ServerStats {
            mounts,
            active_sessions: self.active_sessions,
            sessions_total: self.sessions_total,
            sessions_rejected_cap: self.sessions_rejected_cap,
            sessions_rejected_codec: self.sessions_rejected_codec,
            sessions_rejected_auth,
            sessions_reaped_ice_timeout: self.sessions_reaped_ice_timeout,
            sessions_reaped_disconnected: self.sessions_reaped_disconnected,
            rtp_packets_sent: self.rtp_packets_sent,
            rtp_bytes_sent: self.rtp_bytes_sent,
            datagrams_unrouted: self.datagrams_unrouted,
        }
    }
}

/// The running media thread.
pub(crate) struct MediaThread {
    handle: JoinHandle<()>,
    /// Disconnects when the thread exits (or panics).
    done: Receiver<()>,
    shutdown: Arc<AtomicBool>,
    waker: Waker,
}

impl MediaThread {
    /// Binds the media socket and its wake socket and starts the thread.
    ///
    /// Returns the thread, the waker mounts and the control side use, and
    /// the media socket's address.
    pub(crate) fn spawn(
        cfg: MediaConfig,
        clock: Arc<dyn Clock>,
        media_rx: Receiver<MediaMsg>,
        ctrl_rx: Receiver<ControlMsg>,
        mounts: Arc<Mutex<HashMap<MountId, Arc<MountState>>>>,
    ) -> io::Result<(Self, Waker, SocketAddr)> {
        let socket = UdpSocket::bind(cfg.bind)?;
        let local = socket.local_addr()?;
        // The wake socket sends to the media socket on an address it is
        // sure to reach: the bound one, or loopback for a wildcard bind.
        let wake_ip = if local.ip().is_unspecified() {
            loopback(local.ip())
        } else {
            local.ip()
        };
        let wake_sock = UdpSocket::bind(SocketAddr::new(wake_ip, 0))?;
        let wake_addr = wake_sock.local_addr()?;
        let pending = Arc::new(AtomicBool::new(false));
        let waker = Waker {
            target: Some(WakeTarget {
                sock: Arc::new(wake_sock),
                target: SocketAddr::new(wake_ip, local.port()),
                pending: Arc::clone(&pending),
            }),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let candidates = candidate_addrs(local, cfg.advertise);
        let media = Media {
            cfg,
            clock,
            socket,
            local,
            wake_addr,
            pending,
            shutdown: Arc::clone(&shutdown),
            media_rx,
            ctrl_rx,
            mounts,
            candidates,
            viewers: Vec::new(),
            counters: ServerCounters::default(),
            gauges_dirty: true,
            busy: false,
            buf: vec![0; DATAGRAM_MAX],
        };
        let (done_tx, done) = mpsc::sync_channel::<()>(0);
        let handle = thread::Builder::new()
            .name("tst-webrtc-media".into())
            .spawn(move || {
                let _done = done_tx;
                media.run();
            })?;
        Ok((
            Self {
                handle,
                done,
                shutdown,
                waker: waker.clone(),
            },
            waker,
            local,
        ))
    }

    /// Asks the thread to stop and waits up to `within` for it. `true`
    /// when it exited in time; otherwise it is left to finish on its own.
    pub(crate) fn join(self, within: Duration) -> bool {
        self.shutdown.store(true, Ordering::Release);
        self.waker.wake();
        match self.done.recv_timeout(within) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                // The thread is past its last statement; a panic was
                // already reported by the panic hook.
                let _ = self.handle.join();
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
        }
    }
}

/// The server's `Rtc` for one session: ice-lite, RTP mode, egress
/// statistics every second (for the NACK count), str0m's default codecs
/// plus a mode-1 Constrained High H.264 payload.
pub(crate) fn server_rtc(now: Instant) -> Rtc {
    let mut builder = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)));
    let codecs = builder.codec_config();
    let used: Vec<u8> = codecs
        .params()
        .iter()
        .flat_map(|p| [Some(*p.pt()), p.resend().map(|r| *r)])
        .flatten()
        .collect();
    let mut free = (96u8..=127)
        .chain(35..=63)
        .filter(|pt| !used.contains(pt))
        .map(Pt::from);
    if let (Some(pt), Some(rtx)) = (free.next(), free.next()) {
        codecs.add_h264(pt, Some(rtx), true, H264_CONSTRAINED_HIGH);
    }
    builder.build(now)
}

/// The loopback address of `ip`'s family.
fn loopback(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    }
}

/// The local ICE candidate addresses (see [`MediaConfig`]).
fn candidate_addrs(local: SocketAddr, advertise: Option<IpAddr>) -> Vec<SocketAddr> {
    let port = local.port();
    let mut out = Vec::new();
    if local.ip().is_unspecified() {
        out.extend(advertise.map(|ip| SocketAddr::new(ip, port)));
        out.push(SocketAddr::new(loopback(local.ip()), port));
    } else {
        out.push(local);
        out.extend(
            advertise
                .filter(|ip| *ip != local.ip())
                .map(|ip| SocketAddr::new(ip, port)),
        );
    }
    out
}

/// The candidate a datagram from `source` arrived on. std's `recv_from`
/// does not report the destination address, and str0m's ICE only answers
/// checks addressed to one of its candidates. A specific bind has one
/// receiving address; on a wildcard bind loopback traffic arrived on the
/// loopback candidate and anything else on the advertised one.
fn destination_for(candidates: &[SocketAddr], local: SocketAddr, source: SocketAddr) -> SocketAddr {
    if !local.ip().is_unspecified() {
        return local;
    }
    let want_loopback = source.ip().is_loopback();
    candidates
        .iter()
        .copied()
        .find(|c| c.ip().is_loopback() == want_loopback)
        .unwrap_or(local)
}

/// 32 hex characters from the system's random source.
fn session_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A random initial RTP sequence number (RFC 3550 §5.1); 0 should the
/// random source fail.
fn random_seq() -> u64 {
    let mut bytes = [0u8; 2];
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    u64::from(u16::from_be_bytes(bytes))
}

/// The data-channel body of a KLV message: version, the frame's RTP
/// timestamp, the record's PTS (both big-endian), the record.
fn klv_body(msg: &KlvMessage) -> Vec<u8> {
    let mut body = Vec::with_capacity(13 + msg.klv.len());
    body.push(KLV_DC_VERSION);
    body.extend_from_slice(&msg.rtp_ts.to_be_bytes());
    body.extend_from_slice(&msg.klv_pts_90k.to_be_bytes());
    body.extend_from_slice(&msg.klv);
    body
}

/// What became of one queued write.
enum Write {
    /// str0m took it.
    Written,
    /// str0m cannot take it now; it stays at the head of the queue.
    Refused,
    /// It can never be written (its channel closed); it is discarded.
    Discarded,
}

/// Whether the thread keeps running.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// The thread's state.
struct Media {
    cfg: MediaConfig,
    clock: Arc<dyn Clock>,
    socket: UdpSocket,
    /// The media socket's bound address.
    local: SocketAddr,
    wake_addr: SocketAddr,
    pending: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    media_rx: Receiver<MediaMsg>,
    ctrl_rx: Receiver<ControlMsg>,
    mounts: Arc<Mutex<HashMap<MountId, Arc<MountState>>>>,
    candidates: Vec<SocketAddr>,
    viewers: Vec<Viewer>,
    counters: ServerCounters,
    /// The per-mount viewer gauges need rewriting.
    gauges_dirty: bool,
    /// The last pass left mount messages queued; do not wait.
    busy: bool,
    buf: Vec<u8>,
}

impl Media {
    fn run(mut self) {
        while !self.shutdown.load(Ordering::Acquire) {
            let now = self.clock.now();
            let wait = self
                .next_deadline(now)
                .saturating_duration_since(now)
                .clamp(MIN_WAIT, MAX_WAIT);
            if let Err(e) = self.socket.set_read_timeout(Some(wait)) {
                debug!(error = %e, "media socket: set_read_timeout failed");
            }
            self.receive();
            // Cleared before the queues are read: a message queued from
            // here on sends a fresh wake.
            self.pending.store(false, Ordering::Release);
            self.drain_control();
            if self.drain_media() == Flow::Stop {
                break;
            }
            let now = self.clock.now();
            for v in &mut self.viewers {
                flush_viewer(v, &self.socket, now, &mut self.counters);
            }
            for v in &mut self.viewers {
                if v.timeout <= now {
                    if let Err(e) = v.rtc.handle_input(Input::Timeout(now)) {
                        debug!(session = %v.id, error = %e, "timeout input failed");
                    }
                    drain_rtc(v, &self.socket, now);
                }
            }
            self.reap(now);
            self.update_gauges();
        }
        self.finish();
    }

    /// The earliest session deadline, at most [`MAX_WAIT`] away; `now`
    /// when mount messages are still queued.
    fn next_deadline(&self, now: Instant) -> Instant {
        if self.busy {
            return now;
        }
        self.viewers
            .iter()
            .map(|v| v.timeout)
            .fold(now + MAX_WAIT, Instant::min)
    }

    /// Reads at most one datagram (bounded by the read timeout) and hands
    /// it to the session that accepts it.
    fn receive(&mut self) {
        let (n, source) = match self.socket.recv_from(&mut self.buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return,
            Err(e) => {
                debug!(error = %e, "media socket: recv_from failed");
                return;
            }
        };
        if source == self.wake_addr {
            return;
        }
        let now = self.clock.now();
        let destination = destination_for(&self.candidates, self.local, source);
        let Ok(contents) = self.buf[..n].try_into() else {
            self.counters.datagrams_unrouted += 1;
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents,
            },
        );
        let Some(v) = self.viewers.iter_mut().find(|v| v.rtc.accepts(&input)) else {
            self.counters.datagrams_unrouted += 1;
            return;
        };
        if let Err(e) = v.rtc.handle_input(input) {
            debug!(session = %v.id, error = %e, "datagram input failed");
        }
        drain_rtc(v, &self.socket, now);
    }

    fn drain_control(&mut self) {
        while let Ok(msg) = self.ctrl_rx.try_recv() {
            match msg {
                ControlMsg::Offer(req) => self.handle_offer(req),
                ControlMsg::Delete { id, reply } => {
                    let before = self.viewers.len();
                    self.viewers.retain(|v| v.id != id);
                    let found = self.viewers.len() != before;
                    self.gauges_dirty |= found;
                    let _ = reply.try_send(found);
                }
                ControlMsg::Stats { reply } => {
                    let _ = reply.try_send(self.snapshot());
                }
            }
        }
    }

    fn snapshot(&self) -> (ServerCounters, Vec<SessionStats>) {
        let now = self.clock.now();
        let mut counters = self.counters.clone();
        counters.active_sessions = self.viewers.len() as u64;
        let sessions = self.viewers.iter().map(|v| v.snapshot(now)).collect();
        (counters, sessions)
    }

    fn handle_offer(&mut self, req: OfferRequest) {
        let result = self.create_session(req.mount, req.offer);
        match &result {
            Ok(created) => {
                self.counters.sessions_total += 1;
                debug!(session = %created.id, remote = ?req.remote_hint, "session created");
            }
            Err(RejectReason::Cap) => self.counters.sessions_rejected_cap += 1,
            Err(RejectReason::Codec) => self.counters.sessions_rejected_codec += 1,
            Err(RejectReason::Shutdown | RejectReason::Internal) => {}
        }
        let _ = req.reply.try_send(result);
    }

    fn create_session(
        &mut self,
        mount: MountId,
        offer: SdpOffer,
    ) -> Result<CreatedSession, RejectReason> {
        if self.viewers.len() >= self.cfg.max_viewers {
            return Err(RejectReason::Cap);
        }
        let state = self
            .mounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&mount)
            .cloned()
            .ok_or(RejectReason::Internal)?;
        let now = self.clock.now();
        let mut rtc = server_rtc(now);
        for addr in &self.candidates {
            match Candidate::host(*addr, "udp") {
                Ok(c) => {
                    rtc.add_local_candidate(c);
                }
                Err(e) => debug!(%addr, error = %e, "local candidate refused"),
            }
        }
        // select_video reads the offer's own payload lines.
        let offer_sdp = offer.to_sdp_string();
        let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| {
            debug!(error = %e, "offer refused by str0m");
            RejectReason::Internal
        })?;
        let (mid, pt) = match select_video(
            &rtc,
            &offer_sdp,
            state.cfg.video,
            state.sps_profile_level_id(),
        ) {
            Negotiated::Video { mid, pt, .. } => (mid, pt),
            Negotiated::Rejected { reason } => {
                debug!(reason, "offer rejected");
                return Err(RejectReason::Codec);
            }
        };
        // Both sides open the channel as id 0, so no renegotiation follows
        // and apply() has no offer to return.
        let mut api = rtc.sdp_api();
        api.add_channel_with_config(ChannelConfig {
            label: KLV_LABEL.into(),
            negotiated: Some(0),
            ..Default::default()
        });
        let _ = api.apply();
        let id = session_id().ok_or(RejectReason::Internal)?;
        let mut v = Viewer::new(id.clone(), state, rtc, random_seq(), now);
        v.mid = Some(mid);
        v.pt = Some(pt);
        drain_rtc(&mut v, &self.socket, now);
        self.viewers.push(v);
        self.gauges_dirty = true;
        Ok(CreatedSession {
            id,
            answer: answer.to_sdp_string(),
        })
    }

    /// Hands queued mount messages to the sessions, at most
    /// [`MEDIA_BATCH`] per pass.
    fn drain_media(&mut self) -> Flow {
        self.busy = false;
        let now = self.clock.now();
        for _ in 0..MEDIA_BATCH {
            match self.media_rx.try_recv() {
                Ok(msg) => {
                    if self.handle_media_msg(msg, now) == Flow::Stop {
                        return Flow::Stop;
                    }
                }
                Err(TryRecvError::Empty) => return Flow::Continue,
                // Nothing can feed the thread any more.
                Err(TryRecvError::Disconnected) => return Flow::Stop,
            }
        }
        self.busy = true;
        Flow::Continue
    }

    fn handle_media_msg(&mut self, msg: MediaMsg, now: Instant) -> Flow {
        match msg {
            MediaMsg::Frame { mount, packet } => {
                for v in self
                    .viewers
                    .iter_mut()
                    .filter(|v| v.mount == mount && v.connected)
                {
                    let was_waiting = v.waiting_keyframe;
                    match v.enqueue_frame(&packet, now) {
                        Enqueued::Dropped(n) => {
                            v.mount_state
                                .stats
                                .viewer_packets_dropped_queue_full
                                .fetch_add(n, Ordering::Relaxed);
                        }
                        Enqueued::Queued => self.gauges_dirty |= was_waiting,
                        Enqueued::Skipped => {}
                    }
                }
            }
            MediaMsg::Klv { mount, msg } => {
                for v in self
                    .viewers
                    .iter_mut()
                    .filter(|v| v.mount == mount && v.connected)
                {
                    if let Enqueued::Dropped(n) = v.enqueue_klv(&msg, now) {
                        v.mount_state
                            .stats
                            .viewer_packets_dropped_queue_full
                            .fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
            // No session carries an audio track; the mount counts the
            // frames it discards.
            MediaMsg::Audio { .. } => {}
            MediaMsg::MountRemoved(id) => {
                let before = self.viewers.len();
                self.viewers.retain(|v| v.mount != id);
                self.gauges_dirty |= self.viewers.len() != before;
            }
            MediaMsg::Stop => return Flow::Stop,
        }
        Flow::Continue
    }

    fn reap(&mut self, now: Instant) {
        let cfg = &self.cfg;
        let counters = &mut self.counters;
        let before = self.viewers.len();
        self.viewers.retain(|v| {
            let Some(cause) = reap_decision(v, now, cfg) else {
                return true;
            };
            debug!(session = %v.id, ?cause, "session reaped");
            match cause {
                ReapCause::Ice => counters.sessions_reaped_ice_timeout += 1,
                ReapCause::Disconnected | ReapCause::QueueStuck => {
                    counters.sessions_reaped_disconnected += 1;
                }
            }
            false
        });
        self.gauges_dirty |= self.viewers.len() != before;
    }

    /// Rewrites every mount's `viewers` and `viewers_waiting_keyframe`.
    fn update_gauges(&mut self) {
        if !std::mem::take(&mut self.gauges_dirty) {
            return;
        }
        let mounts = self.mounts.lock().unwrap_or_else(PoisonError::into_inner);
        for (id, state) in mounts.iter() {
            let of_mount = || self.viewers.iter().filter(|v| v.mount == *id);
            let viewers = of_mount().count() as u64;
            let waiting = of_mount().filter(|v| v.waiting_keyframe).count() as u64;
            state.stats.viewers.store(viewers, Ordering::Relaxed);
            state
                .stats
                .viewers_waiting_keyframe
                .store(waiting, Ordering::Relaxed);
        }
    }

    /// Drops every session and answers what is still queued.
    fn finish(mut self) {
        self.viewers.clear();
        self.gauges_dirty = true;
        self.update_gauges();
        while let Ok(msg) = self.ctrl_rx.try_recv() {
            match msg {
                ControlMsg::Offer(req) => {
                    let _ = req.reply.try_send(Err(RejectReason::Shutdown));
                }
                ControlMsg::Delete { reply, .. } => {
                    let _ = reply.try_send(false);
                }
                ControlMsg::Stats { reply } => {
                    let _ = reply.try_send(self.snapshot());
                }
            }
        }
    }
}

/// Polls `v` until str0m reports its next timeout, sending every datagram
/// it emits and applying every event.
fn drain_rtc(v: &mut Viewer, socket: &UdpSocket, now: Instant) {
    loop {
        match v.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                v.timeout = t;
                return;
            }
            Ok(Output::Transmit(t)) => {
                v.remote = Some(t.destination);
                if let Err(e) = socket.send_to(&t.contents, t.destination) {
                    debug!(session = %v.id, error = %e, "send_to failed");
                }
            }
            Ok(Output::Event(ev)) => handle_event(v, ev, now),
            Err(e) => {
                // One failed output does not end the session; the reapers
                // close it if it stops working.
                debug!(session = %v.id, error = %e, "poll_output failed");
                v.timeout = now + ERROR_RETRY;
                return;
            }
        }
    }
}

fn handle_event(v: &mut Viewer, ev: Event, now: Instant) {
    match ev {
        Event::Connected => {
            v.connected = true;
            v.connected_at.get_or_insert(now);
            if let Some(mid) = v.mid {
                v.ssrc_known = v.rtc.direct_api().stream_tx_by_mid(mid, None).is_some();
            }
        }
        Event::IceConnectionStateChange(s) => v.set_ice(ice_state(s), now),
        Event::ChannelOpen(id, label) if label == KLV_LABEL => v.dc = Some(id),
        Event::ChannelClose(id) if v.dc == Some(id) => v.dc = None,
        Event::KeyframeRequest(_) => {
            v.stats.plis += 1;
            bump(&v.mount_state.stats.plis_received);
        }
        Event::MediaEgressStats(s) if s.nacks > v.stats.nacks => {
            v.mount_state
                .stats
                .nacks_received
                .fetch_add(s.nacks - v.stats.nacks, Ordering::Relaxed);
            v.stats.nacks = s.nacks;
        }
        _ => {}
    }
}

/// str0m's ICE state as the server reports it. str0m never reports
/// Failed or Closed (an ice-lite agent can always recover).
fn ice_state(s: IceConnectionState) -> IceState {
    match s {
        IceConnectionState::New => IceState::New,
        IceConnectionState::Checking => IceState::Checking,
        IceConnectionState::Connected | IceConnectionState::Completed => IceState::Connected,
        IceConnectionState::Disconnected => IceState::Disconnected,
    }
}

/// Writes `v`'s queue to str0m until str0m refuses or the queue is empty,
/// draining str0m's output after each write.
fn flush_viewer(v: &mut Viewer, socket: &UdpSocket, now: Instant, counters: &mut ServerCounters) {
    if !v.connected {
        return;
    }
    while let Some(item) = v.queue.pop_front() {
        let outcome = match &item {
            Queued::Rtp(packet, i) => write_rtp(v, packet, *i, now, counters),
            Queued::Dc(msg) => write_dc(v, msg),
        };
        match outcome {
            Write::Written => {
                v.queue_full_since = None;
                drain_rtc(v, socket, now);
            }
            Write::Discarded => {}
            Write::Refused => {
                v.queue.push_front(item);
                return;
            }
        }
    }
}

fn write_rtp(
    v: &mut Viewer,
    packet: &FramePacket,
    i: usize,
    now: Instant,
    counters: &mut ServerCounters,
) -> Write {
    let Some(chunk) = packet.payloads.get(i) else {
        return Write::Discarded;
    };
    let (Some(mid), Some(pt)) = (v.mid, v.pt) else {
        return Write::Refused;
    };
    let mut api = v.rtc.direct_api();
    let Some(tx) = api.stream_tx_by_mid(mid, None) else {
        return Write::Refused;
    };
    tx.write_rtp(
        RtpWrite::new(
            pt,
            SeqNo::from(v.seq),
            packet.rtp_ts,
            now,
            chunk.payload.as_slice(),
        )
        .marker(chunk.marker)
        .nackable(true),
    );
    v.ssrc_known = true;
    v.seq += 1;
    let len = chunk.payload.len() as u64;
    v.stats.rtp_packets += 1;
    v.stats.rtp_bytes += len;
    counters.rtp_packets_sent += 1;
    counters.rtp_bytes_sent += len;
    Write::Written
}

fn write_dc(v: &mut Viewer, msg: &KlvMessage) -> Write {
    let Some(mut channel) = v.dc.and_then(|cid| v.rtc.channel(cid)) else {
        return Write::Discarded;
    };
    match channel.write(true, &klv_body(msg)) {
        Ok(true) => {
            v.stats.dc_messages += 1;
            bump(&v.mount_state.stats.klv_dc_messages_sent);
            Write::Written
        }
        Ok(false) => Write::Refused,
        Err(e) => {
            debug!(session = %v.id, error = %e, "data channel write failed");
            Write::Discarded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VideoCodec;

    const SAFARI: &str = include_str!("../../tests/fixtures/offers/safari-26.sdp");

    /// Accepts `offer` on `rtc` with a TEST-NET-1 candidate (never bound).
    fn accept(mut rtc: Rtc, offer: &str) -> Rtc {
        let candidate = Candidate::host("192.0.2.1:40000".parse().unwrap(), "udp").unwrap();
        rtc.add_local_candidate(candidate);
        let parsed = SdpOffer::from_sdp_string(offer).expect("the offer parses");
        rtc.sdp_api()
            .accept_offer(parsed)
            .expect("the offer is accepted");
        rtc
    }

    const CONSTRAINED_HIGH_SPS: [u8; 3] = [0x64, 0x0c, 0x1f];

    #[test]
    fn a_constrained_high_mode_1_offer_selects_that_payload() {
        let rtc = accept(server_rtc(Instant::now()), SAFARI);
        let got = select_video(&rtc, SAFARI, VideoCodec::H264, Some(CONSTRAINED_HIGH_SPS));
        let Negotiated::Video { pt, .. } = got else {
            panic!("no H.264 payload: {got:?}");
        };
        // Safari's `a=fmtp:96 ...packetization-mode=1;profile-level-id=640c1f`.
        assert_eq!(*pt, 96);
    }

    #[test]
    fn without_the_constrained_high_entry_safari_falls_back_to_baseline() {
        let plain = Rtc::builder()
            .set_ice_lite(true)
            .set_rtp_mode(true)
            .build(Instant::now());
        let rtc = accept(plain, SAFARI);
        let got = select_video(&rtc, SAFARI, VideoCodec::H264, Some(CONSTRAINED_HIGH_SPS));
        let Negotiated::Video { pt, .. } = got else {
            panic!("no H.264 payload: {got:?}");
        };
        // `a=fmtp:98 ...packetization-mode=1;profile-level-id=42e01f`.
        assert_eq!(*pt, 98);
    }

    #[test]
    fn candidates_follow_the_bind_and_advertise_addresses() {
        let specific: SocketAddr = "192.0.2.1:40000".parse().unwrap();
        let public: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(candidate_addrs(specific, None), [specific]);
        assert_eq!(
            candidate_addrs(specific, Some(public)),
            [specific, SocketAddr::new(public, 40000)]
        );
        assert_eq!(destination_for(&[specific], specific, specific), specific);

        let wildcard: SocketAddr = "0.0.0.0:40000".parse().unwrap();
        let only_loopback = candidate_addrs(wildcard, None);
        assert_eq!(only_loopback.len(), 1);
        assert!(only_loopback[0].ip().is_loopback());
        let both = candidate_addrs(wildcard, Some(public));
        assert_eq!(both[0], SocketAddr::new(public, 40000));
        assert!(both[1].ip().is_loopback());
        let remote: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        assert_eq!(destination_for(&both, wildcard, remote), both[0]);
        assert_eq!(destination_for(&both, wildcard, both[1]), both[1]);
    }

    #[test]
    fn the_klv_body_is_version_rtp_ts_pts_then_the_record() {
        let body = klv_body(&KlvMessage {
            rtp_ts: 0x0102_0304,
            klv_pts_90k: 0x0506_0708_090a_0b0c,
            klv: vec![0xAA, 0xBB],
        });
        assert_eq!(body, [1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0xAA, 0xBB]);
    }

    #[test]
    fn session_ids_are_32_hex_characters() {
        let id = session_id().expect("random source");
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
}
