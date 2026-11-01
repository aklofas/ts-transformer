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
use tracing::{debug, warn};

use crate::clock::Clock;
use crate::config::AudioPolicy;
use crate::mount::{
    AudioPacket, FramePacket, KLV_DC_HEADER_BYTES, KlvMessage, MediaMsg, MountId, MountState,
    SCTP_SEND_BUFFER_BYTES,
};
use crate::server::negotiate::{Negotiated, repair_rejected_media, select_audio, select_video};
use crate::server::session::{
    Enqueued, MAX_PRECONNECT_SOURCES, Queued, ReapCause, Viewer, reap_decision,
};
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

/// The label of the KLV data channel the server opens on each session.
const KLV_LABEL: &str = "klv";

/// The `klv` channel: in-band (DCEP), reliable and unordered. A record
/// never waits behind an earlier one; each message names its frame's RTP
/// timestamp.
fn klv_channel_config() -> ChannelConfig {
    ChannelConfig {
        label: KLV_LABEL.into(),
        ordered: false,
        ..Default::default()
    }
}

/// The data-channel message format version (the body's first byte).
const KLV_DC_VERSION: u8 = 1;

#[cfg(test)]
mod klv_channel_tests {
    use super::klv_channel_config;
    use str0m::channel::Reliability;

    /// The spec's channel: in-band DCEP (no negotiated id), reliable and
    /// unordered. str0m's `add_channel` default is ordered, so a revert to
    /// it fails here rather than passing every browser cell.
    #[test]
    fn the_klv_channel_is_in_band_reliable_and_unordered() {
        let c = klv_channel_config();
        assert_eq!(c.label, "klv");
        assert!(!c.ordered, "the klv channel is unordered");
        assert_eq!(c.reliability, Reliability::Reliable);
        assert_eq!(
            c.negotiated, None,
            "opened in-band by DCEP, not pre-negotiated"
        );
    }
}

const _: () = assert!(
    KLV_DC_HEADER_BYTES == 1 + 4 + 8,
    "the header is the version, the RTP timestamp and the PTS"
);

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
    /// Close a session; the reply says whether it existed on that mount.
    Delete {
        /// The mount the request named.
        mount: MountId,
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
    /// Answered once the pass that read it has run to its end (timers,
    /// reapers, gauges), so a test that moved a manual clock knows the
    /// thread has acted on the new time.
    Sync {
        /// Receives `()` at the end of that pass.
        reply: SyncSender<()>,
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
    /// Which snapshot this is: every snapshot the thread takes gets the
    /// next number, so a reader holding two can tell the newer one.
    pub(crate) snapshot_seq: u64,
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
            syncs: Vec::new(),
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
/// statistics every second (for the NACK count), an SCTP send buffer
/// sized for the largest KLV record ([`SCTP_SEND_BUFFER_BYTES`]), str0m's
/// default video codecs plus a mode-1 Constrained High H.264 payload.
///
/// The audio codecs are the ones the mount's `audio` policy serves: Opus
/// under [`AudioPolicy::PassthroughOpus`], none under
/// [`AudioPolicy::Drop`]. str0m rejects (port 0) an audio section of an
/// offer with no codec in common, so a dropping mount answers a viewer's
/// audio section as rejected rather than as a stream that never carries
/// anything (see [`repair_rejected_media`] for its format list).
pub(crate) fn server_rtc(now: Instant, audio: AudioPolicy) -> Rtc {
    let mut builder = Rtc::builder()
        .set_ice_lite(true)
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .set_sctp_max_buffered_amount(SCTP_SEND_BUFFER_BYTES);
    let codecs = builder.codec_config();
    codecs.enable_opus(audio == AudioPolicy::PassthroughOpus, false);
    codecs.enable_pcmu(false, false);
    codecs.enable_pcma(false, false);
    codecs.enable_g722(false, false);
    codecs.enable_comfort_noise(false);
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
    let mut body = Vec::with_capacity(KLV_DC_HEADER_BYTES + msg.klv.len());
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
    /// str0m cannot take it now; it stays at the head of the queue. Only
    /// a video RTP write is ever refused (no send stream yet).
    Refused,
    /// It was dropped (and counted); the flush goes on with the next entry.
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
    /// `Sync` requests read this pass, answered at its end.
    syncs: Vec<SyncSender<()>>,
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
            for reply in self.syncs.drain(..) {
                let _ = reply.try_send(());
            }
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
        if !admit_source(v, source) {
            self.counters.datagrams_unrouted += 1;
            return;
        }
        // RFC 7983: a first byte in 20..=63 is DTLS. The handshake is bound
        // to the first source that sent a record: records are accepted from
        // it only, and the server's flights are sent to it only (see
        // `drain_rtc`), so the pin is an address that both sent and
        // received the exchange.
        if v.pinned.is_none() && (20..=63).contains(&self.buf[0]) {
            match v.dtls_source {
                None => v.dtls_source = Some(source),
                Some(d) if d != source => {
                    self.counters.datagrams_unrouted += 1;
                    return;
                }
                Some(_) => {}
            }
        }
        v.last_source = Some(source);
        if let Err(e) = v.rtc.handle_input(input) {
            debug!(session = %v.id, error = %e, "datagram input failed");
        }
        drain_rtc(v, &self.socket, now);
    }

    fn drain_control(&mut self) {
        while let Ok(msg) = self.ctrl_rx.try_recv() {
            match msg {
                ControlMsg::Offer(req) => self.handle_offer(req),
                ControlMsg::Delete { mount, id, reply } => {
                    let before = self.viewers.len();
                    // A session id names a session only on its own mount.
                    self.viewers.retain(|v| !(v.id == id && v.mount == mount));
                    let found = self.viewers.len() != before;
                    self.gauges_dirty |= found;
                    let _ = reply.try_send(found);
                }
                ControlMsg::Stats { reply } => {
                    let _ = reply.try_send(self.snapshot());
                }
                ControlMsg::Sync { reply } => self.syncs.push(reply),
            }
        }
    }

    fn snapshot(&mut self) -> (ServerCounters, Vec<SessionStats>) {
        let now = self.clock.now();
        self.counters.snapshot_seq += 1;
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
        let mut rtc = server_rtc(now, state.cfg.audio);
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
        // Nothing is registered for the viewer before this call returns, so a
        // refused or panicking offer leaves no state behind: `rtc` is dropped.
        let (v, answer, id) = contain_panic(|| {
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
            // Audio never decides whether an offer is served: no Opus in the
            // offer, or a mount that drops audio, means a video-only session.
            let audio = match state.cfg.audio {
                AudioPolicy::PassthroughOpus => select_audio(&rtc, &offer_sdp),
                AudioPolicy::Drop => None,
            };
            // The channel is opened in-band (DCEP) on the SCTP association the
            // offer's data-channel section brings up, so any viewer receives
            // it by label and apply() has no offer to return. Unordered: a
            // record never waits behind an earlier one; each message names its
            // frame's RTP timestamp, so the page orders them itself.
            let mut api = rtc.sdp_api();
            let klv_channel = api.add_channel_with_config(klv_channel_config());
            if api.apply().is_some() {
                // str0m offering a data-channel section means the offer had
                // none: the session has no SCTP transport, so the channel never
                // opens and the viewer gets video (and KLV in SEI) only.
                debug!("the offer has no data-channel section; the klv channel is not opened");
            }
            let id = session_id().ok_or(RejectReason::Internal)?;
            let mut v = Viewer::new(id.clone(), state, rtc, random_seq(), now);
            v.mid = Some(mid);
            v.pt = Some(pt);
            v.klv_channel = Some(klv_channel);
            v.audio = audio;
            v.audio_seq = random_seq();
            drain_rtc(&mut v, &self.socket, now);
            Ok((v, answer, id))
        })??;
        self.viewers.push(v);
        self.gauges_dirty = true;
        Ok(CreatedSession {
            id,
            answer: repair_rejected_media(&answer.to_sdp_string()),
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
                            self.gauges_dirty |= !was_waiting;
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
                            .klv_dc_dropped_viewer
                            .fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
            MediaMsg::Audio { mount, packet } => {
                for v in self
                    .viewers
                    .iter_mut()
                    .filter(|v| v.mount == mount && v.connected)
                {
                    if let Enqueued::Dropped(n) = v.enqueue_audio(&packet, now) {
                        v.mount_state
                            .stats
                            .viewer_packets_dropped_queue_full
                            .fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
            MediaMsg::MountRemoved(id) => {
                let before = self.viewers.len();
                self.viewers.retain(|v| {
                    if v.mount != id {
                        return true;
                    }
                    zero_gauges(&v.mount_state);
                    false
                });
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
            // A removed mount's sessions close even when its
            // MountRemoved message was dropped at a full queue.
            if v.mount_state.closed.load(Ordering::Acquire) {
                zero_gauges(&v.mount_state);
                return false;
            }
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
                ControlMsg::Sync { reply } => {
                    let _ = reply.try_send(());
                }
            }
        }
        for reply in self.syncs.drain(..) {
            let _ = reply.try_send(());
        }
    }
}

/// Where a session's datagrams go: the media socket, or a test's sink.
trait DatagramSink {
    /// Sends one datagram; a failure is logged and the datagram is lost,
    /// as on any UDP path.
    fn send(&self, contents: &[u8], to: SocketAddr);
}

impl DatagramSink for UdpSocket {
    fn send(&self, contents: &[u8], to: SocketAddr) {
        if let Err(e) = self.send_to(contents, to) {
            debug!(%to, error = %e, "send_to failed");
        }
    }
}

/// Whether a datagram from `source` may reach `v`, recording the source
/// when it is new.
///
/// Once the session is pinned only the pinned address is admitted; before
/// that, at most [`MAX_PRECONNECT_SOURCES`] distinct sources are. The caller
/// further limits DTLS records to the source of the first one.
fn admit_source(v: &mut Viewer, source: SocketAddr) -> bool {
    if let Some(p) = v.pinned {
        return source == p;
    }
    if v.sources.contains(&source) {
        return true;
    }
    if v.sources.len() >= MAX_PRECONNECT_SOURCES {
        return false;
    }
    v.sources.push(source);
    true
}

/// Polls `v` until str0m reports its next timeout, sending every datagram
/// it emits and applying every event.
fn drain_rtc(v: &mut Viewer, socket: &dyn DatagramSink, now: Instant) {
    loop {
        match v.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                v.timeout = t;
                return;
            }
            Ok(Output::Transmit(t)) => {
                // Before the pin, handshake flights go only to the source
                // that sent the first DTLS record, whatever pair ICE has
                // nominated since; STUN is not filtered, so ICE still works.
                if v.pinned.is_none()
                    && v.dtls_source.is_some_and(|d| d != t.destination)
                    && t.contents.first().is_some_and(|b| (20..=63).contains(b))
                {
                    debug!(session = %v.id, to = %t.destination, "handshake flight for another address than the DTLS source dropped");
                    continue;
                }
                // ICE may still have nominated another admitted source;
                // nothing leaves for an address other than the pinned one.
                if v.pinned.is_some_and(|p| p != t.destination) {
                    debug!(session = %v.id, to = %t.destination, "datagram for an address other than the pinned one dropped");
                    continue;
                }
                v.remote = Some(t.destination);
                socket.send(&t.contents, t.destination);
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
            // The handshake ran with the source of its first DTLS record, and
            // the server's flights went only there.
            if v.pinned.is_none() {
                v.pinned = v.dtls_source.or(v.last_source);
            }
        }
        Event::IceConnectionStateChange(s) => v.set_ice(ice_state(s), now),
        // The viewer's own channels open here too, whatever their labels:
        // only the channel the server added is the klv channel.
        Event::ChannelOpen(id, _) if Some(id) == v.klv_channel => v.dc = Some(id),
        Event::ChannelClose(id) if Some(id) == v.klv_channel => v.dc = None,
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
fn flush_viewer(
    v: &mut Viewer,
    socket: &dyn DatagramSink,
    now: Instant,
    counters: &mut ServerCounters,
) {
    if !v.connected {
        return;
    }
    while let Some(item) = v.queue.pop_front() {
        let outcome = match &item {
            Queued::Rtp(packet, i) => write_rtp(v, packet, *i, now, counters),
            Queued::Dc(msg) => write_dc(v, msg),
            Queued::Audio(packet) => write_audio(v, packet, now, counters),
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
    v.seq += 1;
    let len = chunk.payload.len() as u64;
    v.stats.rtp_packets += 1;
    v.stats.rtp_bytes += len;
    counters.rtp_packets_sent += 1;
    counters.rtp_bytes_sent += len;
    Write::Written
}

/// Writes one Opus packet on the viewer's audio stream: its own sequence
/// numbers, no marker (RFC 7587 sets it after silence only), not
/// retransmittable (browsers do not NACK Opus). A packet with no send
/// stream to take it is dropped and counted, never re-queued: the video
/// behind it must not wait on audio.
fn write_audio(
    v: &mut Viewer,
    packet: &AudioPacket,
    now: Instant,
    counters: &mut ServerCounters,
) -> Write {
    let Some((mid, pt)) = v.audio else {
        return Write::Discarded;
    };
    let mut api = v.rtc.direct_api();
    let Some(tx) = api.stream_tx_by_mid(mid, None) else {
        debug!(session = %v.id, "opus packet dropped: no audio send stream");
        bump(&v.mount_state.stats.audio_frames_dropped);
        return Write::Discarded;
    };
    tx.write_rtp(
        RtpWrite::new(
            pt,
            SeqNo::from(v.audio_seq),
            packet.rtp_ts,
            now,
            packet.payload.as_slice(),
        )
        .marker(false)
        .nackable(false),
    );
    v.audio_seq += 1;
    let len = packet.payload.len() as u64;
    v.stats.rtp_packets += 1;
    v.stats.rtp_bytes += len;
    counters.rtp_packets_sent += 1;
    counters.rtp_bytes_sent += len;
    Write::Written
}

/// Writes one KLV message. A message str0m does not take is dropped and
/// counted, never re-queued: the video queued behind it must not wait on
/// the data channel. str0m refuses a body larger than the free part of the
/// session's SCTP send buffer and fails one above the browser's
/// `max-message-size`.
fn write_dc(v: &mut Viewer, msg: &KlvMessage) -> Write {
    let outcome = match v.dc.and_then(|cid| v.rtc.channel(cid)) {
        None => Err("the data channel is not open".to_string()),
        Some(mut channel) => match channel.write(true, &klv_body(msg)) {
            Ok(true) => Ok(()),
            Ok(false) => Err("the send buffer is full".to_string()),
            Err(e) => Err(e.to_string()),
        },
    };
    match outcome {
        Ok(()) => {
            v.stats.dc_messages += 1;
            bump(&v.mount_state.stats.klv_dc_messages_sent);
            Write::Written
        }
        Err(reason) => {
            debug!(session = %v.id, reason, len = msg.klv.len(), "klv message dropped");
            v.stats.dc_messages_dropped += 1;
            bump(&v.mount_state.stats.klv_dc_dropped_viewer);
            Write::Discarded
        }
    }
}

/// Zeroes the viewer gauges of a mount whose viewers were dropped; a retained
/// handle would otherwise keep reading the last counts of a mount that is no
/// longer in the map `update_gauges` visits.
fn zero_gauges(state: &MountState) {
    state.stats.viewers.store(0, Ordering::Relaxed);
    state
        .stats
        .viewers_waiting_keyframe
        .store(0, Ordering::Relaxed);
}

/// Runs `f`, turning a panic into a refusal. One thread serves every
/// viewer, so a str0m panic on one remote-controlled offer must refuse
/// that offer, not end the thread.
fn contain_panic<T>(f: impl FnOnce() -> T) -> Result<T, RejectReason> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic");
            warn!(
                message,
                "str0m panicked while applying an offer; the offer is refused"
            );
            // A payload whose `Drop` panics would unwind out of this frame.
            std::mem::forget(payload);
            Err(RejectReason::Internal)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VideoCodec;

    const SAFARI: &str = include_str!("../../tests/fixtures/offers/safari-26.sdp");

    #[test]
    fn a_panicking_closure_is_contained_as_an_internal_refusal() {
        let refused = contain_panic(|| -> u8 { panic!("boom") });
        assert!(matches!(refused, Err(RejectReason::Internal)));
        let refused = contain_panic(|| -> u8 { panic!("{}", String::from("formatted")) });
        assert!(matches!(refused, Err(RejectReason::Internal)));
        assert!(matches!(contain_panic(|| 7), Ok(7)));
    }

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
        let rtc = accept(server_rtc(Instant::now(), AudioPolicy::Drop), SAFARI);
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

    /// Counts datagrams instead of sending them.
    #[derive(Default)]
    struct CountingSink(std::cell::Cell<usize>);

    impl DatagramSink for CountingSink {
        fn send(&self, _contents: &[u8], _to: SocketAddr) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Records where each datagram was sent.
    #[derive(Default)]
    struct RecordingSink(std::cell::RefCell<Vec<SocketAddr>>);

    impl DatagramSink for RecordingSink {
        fn send(&self, _contents: &[u8], to: SocketAddr) {
            self.0.borrow_mut().push(to);
        }
    }

    /// The value of the first `a=<attr>:` line of `sdp`.
    fn sdp_attr<'a>(sdp: &'a str, attr: &str) -> &'a str {
        let prefix = format!("a={attr}:");
        sdp.lines()
            .find_map(|l| l.trim_end().strip_prefix(prefix.as_str()))
            .expect("the attribute")
    }

    #[test]
    fn a_pinned_session_sends_nothing_to_another_address() {
        use crate::MountConfig;
        use crate::mount::MountHandle;
        use str0m::ice::{StunMessage, TransId};

        let now = Instant::now();
        let mut rtc = server_rtc(now, AudioPolicy::Drop);
        let local: SocketAddr = "192.0.2.1:40000".parse().unwrap();
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
        let answer = rtc
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(SAFARI).unwrap())
            .expect("the offer is accepted")
            .to_sdp_string();
        let (handle, _rx) = MountHandle::for_test(MountConfig::default());
        let mut v = Viewer::new("0".repeat(32), Arc::clone(&handle.state), rtc, 0, now);
        drain_rtc(&mut v, &CountingSink::default(), now);

        // An authenticated check from `other` makes the ICE-lite agent
        // answer it there.
        let username = format!(
            "{}:{}",
            sdp_attr(&answer, "ice-ufrag"),
            sdp_attr(SAFARI, "ice-ufrag")
        );
        let password = sdp_attr(&answer, "ice-pwd").to_string();
        let provider = str0m::crypto::from_feature_flags();
        let check = |v: &mut Viewer, from: SocketAddr| {
            let msg = StunMessage::binding_request(&username, TransId::new(), true, 1, 1, true);
            let mut buf = vec![0; 1500];
            let n = msg
                .to_bytes(Some(password.as_bytes()), &mut buf, |k, p| {
                    provider.sha1_hmac_provider.sha1_hmac(k, p)
                })
                .unwrap();
            let input = Input::Receive(
                now,
                Receive {
                    proto: Protocol::Udp,
                    source: from,
                    destination: local,
                    contents: buf[..n].try_into().unwrap(),
                },
            );
            assert!(v.rtc.accepts(&input));
            v.rtc.handle_input(input).unwrap();
        };
        let peer: SocketAddr = "198.51.100.1:5000".parse().unwrap();
        let other: SocketAddr = "198.51.100.2:5000".parse().unwrap();

        // Unpinned, the answer goes out.
        let sink = RecordingSink::default();
        check(&mut v, other);
        drain_rtc(&mut v, &sink, now);
        assert_eq!(*sink.0.borrow(), [other]);

        // Pinned to `peer`, the answer to `other` is dropped and `remote`
        // keeps the pinned address.
        v.pinned = Some(peer);
        v.remote = Some(peer);
        let sink = RecordingSink::default();
        check(&mut v, other);
        drain_rtc(&mut v, &sink, now);
        assert!(sink.0.borrow().is_empty(), "{:?}", sink.0.borrow());
        assert_eq!(v.remote, Some(peer));
        check(&mut v, peer);
        drain_rtc(&mut v, &sink, now);
        assert_eq!(*sink.0.borrow(), [peer]);
    }

    /// A media thread's state with one session on a fresh mount whose
    /// viewer gauges read 3 and 2, never run.
    fn media_with_seeded_session() -> (Media, Arc<MountState>, Instant) {
        use crate::MountConfig;
        use crate::clock::ManualClock;
        use crate::mount::MountHandle;

        let clock = Arc::new(ManualClock::new());
        let now = clock.now();
        let cfg = MediaConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertise: None,
            max_viewers: 4,
            ice_timeout: Duration::from_secs(30),
            disconnect_timeout: Duration::from_secs(30),
        };
        let socket = UdpSocket::bind(cfg.bind).unwrap();
        let local = socket.local_addr().unwrap();
        let (_media_tx, media_rx) = mpsc::sync_channel(4);
        let (_ctrl_tx, ctrl_rx) = mpsc::sync_channel(4);
        let (handle, _frames) = MountHandle::for_test(MountConfig::default());
        let state = Arc::clone(&handle.state);
        state.stats.viewers.store(3, Ordering::Relaxed);
        state
            .stats
            .viewers_waiting_keyframe
            .store(2, Ordering::Relaxed);
        let rtc = accept(server_rtc(now, AudioPolicy::Drop), SAFARI);
        let media = Media {
            cfg,
            clock,
            socket,
            local,
            wake_addr: local,
            pending: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            media_rx,
            ctrl_rx,
            mounts: Arc::new(Mutex::new(HashMap::new())),
            candidates: Vec::new(),
            viewers: vec![Viewer::new("0".repeat(32), Arc::clone(&state), rtc, 0, now)],
            counters: ServerCounters::default(),
            gauges_dirty: false,
            busy: false,
            syncs: Vec::new(),
            buf: Vec::new(),
        };
        (media, state, now)
    }

    fn gauges(state: &MountState) -> (u64, u64) {
        (
            state.stats.viewers.load(Ordering::Relaxed),
            state.stats.viewers_waiting_keyframe.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn a_removed_mount_message_zeroes_its_gauges_and_drops_its_sessions() {
        let (mut media, state, now) = media_with_seeded_session();
        let id = state.id;
        media.handle_media_msg(MediaMsg::MountRemoved(id), now);
        assert!(media.viewers.is_empty());
        assert_eq!(gauges(&state), (0, 0));
    }

    #[test]
    fn reaping_a_closed_mount_zeroes_its_gauges_and_drops_its_sessions() {
        let (mut media, state, now) = media_with_seeded_session();
        media.reap(now);
        assert_eq!(media.viewers.len(), 1, "an open mount's session stays");
        assert_eq!(gauges(&state), (3, 2));
        state.closed.store(true, Ordering::Release);
        media.reap(now);
        assert!(media.viewers.is_empty());
        assert_eq!(gauges(&state), (0, 0));
    }

    #[test]
    fn connected_pins_the_source_of_the_last_datagram_handed_over() {
        use crate::MountConfig;
        use crate::mount::MountHandle;

        let now = Instant::now();
        let rtc = accept(server_rtc(now, AudioPolicy::Drop), SAFARI);
        let (handle, _rx) = MountHandle::for_test(MountConfig::default());
        let mut v = Viewer::new("0".repeat(32), Arc::clone(&handle.state), rtc, 0, now);
        let peer: SocketAddr = "198.51.100.1:5000".parse().unwrap();
        v.last_source = Some(peer);
        handle_event(&mut v, Event::Connected, now);
        assert_eq!(v.pinned, Some(peer));
        // A later Connected (ICE recovering) keeps the first pin.
        v.last_source = Some("198.51.100.2:5000".parse().unwrap());
        handle_event(&mut v, Event::Connected, now);
        assert_eq!(v.pinned, Some(peer));
    }

    #[test]
    fn an_unwritable_data_channel_message_never_blocks_the_video_behind_it() {
        use crate::MountConfig;
        use crate::mount::MountHandle;
        use crate::packetize::{DEFAULT_MTU_PAYLOAD, H264Packetizer, Packetizer};

        let now = Instant::now();
        let mut rtc = accept(server_rtc(now, AudioPolicy::Drop), SAFARI);
        let Negotiated::Video { mid, pt, .. } = select_video(&rtc, SAFARI, VideoCodec::H264, None)
        else {
            panic!("no H.264 payload");
        };
        let mut api = rtc.sdp_api();
        let cid = api.add_channel_with_config(klv_channel_config());
        let _ = api.apply();
        let (handle, _rx) = MountHandle::for_test(MountConfig::default());
        let mut v = Viewer::new("0".repeat(32), Arc::clone(&handle.state), rtc, 0, now);
        v.mid = Some(mid);
        v.pt = Some(pt);
        v.connected = true;
        // The channel id exists but SCTP never came up: str0m cannot take
        // the message, as it cannot when its send buffer is full.
        v.dc = Some(cid);
        let frame = Arc::new(FramePacket {
            rtp_ts: 9000,
            keyframe: true,
            codec: VideoCodec::H264,
            payloads: H264Packetizer.packetize(&[&[0x65, 1], &[0x65, 2]], DEFAULT_MTU_PAYLOAD),
            parameter_sets: None,
        });
        v.queue.push_back(Queued::Dc(Arc::new(KlvMessage {
            rtp_ts: 9000,
            klv_pts_90k: 0,
            klv: vec![1, 2, 3],
        })));
        v.queue.push_back(Queued::Rtp(Arc::clone(&frame), 0));
        v.queue.push_back(Queued::Rtp(frame, 1));

        let mut counters = ServerCounters::default();
        flush_viewer(&mut v, &CountingSink::default(), now, &mut counters);

        assert!(v.queue.is_empty(), "the queue drained: {:?}", v.queue);
        assert_eq!(v.stats.rtp_packets, 2);
        assert_eq!(counters.rtp_packets_sent, 2);
        assert_eq!(v.stats.dc_messages_dropped, 1);
        assert_eq!(handle.stats().klv_dc_dropped_viewer, 1);
        assert_eq!(v.snapshot(now).dc_messages_dropped, 1);
    }

    #[test]
    fn an_opus_packet_without_an_audio_send_stream_never_blocks_the_video_behind_it() {
        use str0m::format::Codec;
        use str0m::media::{Direction, MediaKind};

        use crate::MountConfig;
        use crate::mount::{AudioPacket, MountHandle};
        use crate::packetize::{DEFAULT_MTU_PAYLOAD, H264Packetizer, Packetizer};

        // The viewer only sends audio, so str0m opens no audio send stream.
        let now = Instant::now();
        let mut client = Rtc::builder().build(now);
        let mut api = client.sdp_api();
        api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio_mid = api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        let offer = api.apply().expect("an offer").0.to_sdp_string();
        let mut rtc = accept(server_rtc(now, AudioPolicy::PassthroughOpus), &offer);
        let Negotiated::Video { mid, pt, .. } = select_video(&rtc, &offer, VideoCodec::H264, None)
        else {
            panic!("no H.264 payload");
        };
        let opus = rtc
            .codec_config()
            .params()
            .iter()
            .find(|p| p.spec().codec == Codec::Opus)
            .expect("Opus is configured")
            .pt();
        assert!(rtc.direct_api().stream_tx_by_mid(audio_mid, None).is_none());

        let (handle, _rx) = MountHandle::for_test(MountConfig {
            audio: AudioPolicy::PassthroughOpus,
            ..MountConfig::default()
        });
        let mut v = Viewer::new("0".repeat(32), Arc::clone(&handle.state), rtc, 0, now);
        v.mid = Some(mid);
        v.pt = Some(pt);
        // As if negotiation had chosen the section anyway: the write must
        // not depend on it never doing so.
        v.audio = Some((audio_mid, opus));
        v.connected = true;
        let frame = Arc::new(FramePacket {
            rtp_ts: 9000,
            keyframe: true,
            codec: VideoCodec::H264,
            payloads: H264Packetizer.packetize(&[&[0x65, 1], &[0x65, 2]], DEFAULT_MTU_PAYLOAD),
            parameter_sets: None,
        });
        let audio = || {
            Queued::Audio(Arc::new(AudioPacket {
                rtp_ts: 480,
                payload: vec![0xFC, 1, 2],
            }))
        };
        v.queue.push_back(audio());
        v.queue.push_back(Queued::Rtp(Arc::clone(&frame), 0));
        v.queue.push_back(audio());
        v.queue.push_back(Queued::Rtp(frame, 1));

        let mut counters = ServerCounters::default();
        flush_viewer(&mut v, &CountingSink::default(), now, &mut counters);

        assert!(v.queue.is_empty(), "the queue drained: {:?}", v.queue);
        assert_eq!(v.stats.rtp_packets, 2, "both video payloads were written");
        assert_eq!(handle.stats().audio_frames_dropped, 2);
    }

    /// The offer of a str0m client asking for video and audio.
    fn audio_offer() -> String {
        use str0m::media::{Direction, MediaKind};
        let mut client = Rtc::builder().build(Instant::now());
        let mut api = client.sdp_api();
        api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, _pending) = api.apply().expect("an offer");
        offer.to_sdp_string()
    }

    /// The answer `server_rtc(.., audio)` gives to `offer`, with the
    /// rtc that gave it.
    fn answer_to(offer: &str, audio: AudioPolicy) -> (Rtc, String) {
        let mut rtc = server_rtc(Instant::now(), audio);
        let candidate = Candidate::host("192.0.2.1:40000".parse().unwrap(), "udp").unwrap();
        rtc.add_local_candidate(candidate);
        let parsed = SdpOffer::from_sdp_string(offer).expect("the offer parses");
        let answer = rtc.sdp_api().accept_offer(parsed).expect("accepted");
        let text = repair_rejected_media(&answer.to_sdp_string());
        (rtc, text)
    }

    /// The answer's `m=audio` line.
    fn audio_line(answer: &str) -> &str {
        answer
            .lines()
            .find(|l| l.starts_with("m=audio"))
            .expect("an audio section")
            .trim_end()
    }

    #[test]
    fn a_dropping_server_rejects_audio_with_a_valid_format_list() {
        let offer = audio_offer();
        let (rtc, answer) = answer_to(&offer, AudioPolicy::Drop);
        assert!(select_audio(&rtc, &offer).is_none());
        let line = audio_line(&answer);
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields, ["m=audio", "0", "UDP/TLS/RTP/SAVPF", "0"], "{line}");
        // The repaired answer parses, which str0m's own parser enforces.
        str0m::change::SdpAnswer::from_sdp_string(&answer).expect("the answer parses");
    }

    #[test]
    fn a_passthrough_server_answers_audio_with_opus_only() {
        let offer = audio_offer();
        let (rtc, answer) = answer_to(&offer, AudioPolicy::PassthroughOpus);
        assert!(select_audio(&rtc, &offer).is_some());
        let line = audio_line(&answer);
        assert!(!line.contains(" 0 UDP"), "{line}");
        let rtpmaps: Vec<&str> = answer
            .lines()
            .filter(|l| l.starts_with("a=rtpmap:") && l.contains("/48000/2"))
            .collect();
        assert_eq!(rtpmaps.len(), 1, "{rtpmaps:?}");
        assert!(rtpmaps[0].to_ascii_lowercase().contains("opus"));
        for gone in ["PCMU", "PCMA", "G722", "CN/"] {
            assert!(!answer.contains(gone), "{gone} is not offered back");
        }
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
