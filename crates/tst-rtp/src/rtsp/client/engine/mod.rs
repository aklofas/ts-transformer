//! The RTSP client's protocol state, without a socket: it is handed
//! replies, the current time, requests and the random bytes of a Digest
//! answer, and hands back the bytes to write, events and its next
//! deadline. Nothing here reads a clock or a random source, opens a
//! socket or spawns a thread.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use bytes::Bytes;
use secrecy::SecretString;
use tst_core::transport::EndReason;

use crate::error::RtspError;
use crate::rtsp::auth::{AuthSchemes, answers_with_digest, build_authorization};
use crate::rtsp::client::{AuthState, duration_ms_saturating};
use crate::rtsp::message::{RtspMethod, RtspRequest, RtspResponse};
use crate::rtsp::transport_negotiation::{RtspTransportKind, parse_transport_response};
use crate::url::RtspVersion;

mod inbound;
use inbound::FrontScan;

/// The random bytes of one Digest answer's `cnonce`. The engine's owner
/// draws them from the OS when the engine asks
/// ([`ClientEngine::submit_wants_cnonce`],
/// [`ClientEngine::timeout_wants_cnonce`], [`ClientEngine::wants_cnonce`])
/// and hands them in with [`ClientEngine::handle_cnonce`]. It answers
/// [`ClientEngine::wants_cnonce`] in the pass that handled the input,
/// before it waits for the stream again: a request whose second attempt
/// waits moves on nothing else but its own reply deadline, and one without
/// a deadline would wait for ever.
pub(crate) type Cnonce = [u8; 16];

/// The user name and password the client answers a challenge with.
#[derive(Debug)]
pub(crate) struct Credentials {
    pub username: String,
    pub password: SecretString,
}

/// What the engine needs to know about the connection it speaks for.
#[derive(Debug)]
pub(crate) struct EngineConfig {
    /// Request-URI of the keepalive pings (`RtspUrl::render_no_credentials`).
    pub uri: String,
    pub version: RtspVersion,
    pub user_agent: String,
    pub credentials: Option<Credentials>,
    pub schemes: AuthSchemes,
}

/// A caller's request: the engine adds `CSeq`, `User-Agent` and
/// `Authorization`, and `Content-Type` and `Content-Length` for a body.
#[derive(Debug)]
pub(crate) struct RequestSpec {
    pub method: RtspMethod,
    pub uri: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<RequestBody>,
    /// On an ANNOUNCE: the number of tracks its description holds,
    /// `1..=MAX_ROUTES`; each is set up in turn, as route `0..n`.
    pub announce_tracks: Option<u8>,
    /// On a SETUP: the route its reply's interleaved channel pair is
    /// installed under.
    pub setup: Option<SetupRoute>,
}

/// A request's body and its `Content-Type`.
#[derive(Debug)]
pub(crate) struct RequestBody {
    pub content_type: &'static str,
    pub bytes: Bytes,
}

/// Interleaved routes one connection can hold.
pub(crate) const MAX_ROUTES: usize = 4;

/// Names one interleaved route: `0..MAX_ROUTES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteId(pub u8);

/// Which way a route's media flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteKind {
    /// The peer sends RTP on the route's RTP channel.
    Pull,
    /// This side sends the RTP: a frame arriving on the RTP channel is
    /// dropped.
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SetupRoute {
    pub id: RouteId,
    pub kind: RouteKind,
}

/// Where a publish session (ANNOUNCE, a SETUP per track, RECORD) stands.
/// It moves when the engine accepts a request's final `2xx` reply, and to
/// `Ended` when a TEARDOWN is submitted or the session or stream ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushPhase {
    Announced,
    /// `done` of the `total` announced tracks are set up, `0 < done < total`.
    SetUp {
        done: u8,
        total: u8,
    },
    Ready,
    Recording,
    Paused,
    Ended,
}

/// The channel pair a SETUP reply installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SetupAccepted {
    pub route: RouteId,
    pub rtp_channel: u8,
    pub rtcp_channel: u8,
}

/// A request's final reply and, for a SETUP that named a route, what the
/// reply installed (`None` for a reply that is not interleaved).
#[derive(Debug)]
pub(crate) struct ReplyOutcome {
    pub response: RtspResponse,
    pub setup: Option<SetupAccepted>,
}

/// Names one caller transaction, from `submit` to its `Reply`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct TxnId(u64);

/// Bytes for the driver to write, in order.
pub(crate) struct Transmit {
    pub bytes: Vec<u8>,
    /// `Some` on a caller request's first attempt (not on its retry after
    /// a `401`, and not on a ping).
    pub txn: Option<TxnId>,
    /// The bytes are a keepalive ping.
    pub is_ping: bool,
}

// By hand: the bytes can carry an `Authorization` header, and a Basic one
// carries the password.
impl std::fmt::Debug for Transmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transmit")
            .field("len", &self.bytes.len())
            .field("txn", &self.txn)
            .field("is_ping", &self.is_ping)
            .finish()
    }
}

/// Why the control stream ended.
#[derive(Debug)]
pub(crate) enum StreamEnd {
    PeerClosed,
    ReadFailed(io::Error),
    WriteFailed(io::Error),
    /// Queued bytes made no progress for the write timeout; `ping` when
    /// the oldest unwritten bytes are a keepalive ping.
    WriteStalled {
        ping: bool,
    },
    Stopped,
}

#[derive(Debug)]
pub(crate) enum Event {
    Reply {
        txn: TxnId,
        result: Result<ReplyOutcome, RtspError>,
    },
    /// A whole RTP packet from a `Pull` route's RTP channel.
    Media {
        route: RouteId,
        bytes: Bytes,
    },
    /// An RTCP packet from a route's RTCP channel.
    Rtcp {
        route: RouteId,
        bytes: Bytes,
    },
    /// A frame on a `Push` route's RTP channel, dropped.
    InboundDropped,
    /// The session is known dead. The `Ended` that names why follows at
    /// once.
    SessionDead,
    Ended(EndReason),
}

/// An installed route: the channel pair a SETUP reply named (the client
/// asks for `interleaved=N-M`; the server echoes or reassigns).
#[derive(Debug, Clone, Copy)]
struct Route {
    kind: RouteKind,
    /// Channel number for RTP data frames.
    rtp: u8,
    /// Channel number for RTCP frames (conventionally `rtp + 1`). On a
    /// `Pull` route it can equal `rtp`: the channel is then RTP only.
    rtcp: u8,
}

const SETUP_WITHOUT_TRANSPORT: &str = "SETUP 200 missing Transport: header";
const ROUTE_ID_OUT_OF_RANGE: &str = "SETUP names more interleaved routes than a connection holds";
const ROUTE_CHANNELS_EQUAL: &str = "SETUP reply names one interleaved channel for RTP and RTCP";
const ROUTE_CHANNEL_IN_USE: &str = "SETUP reply names an interleaved channel another stream uses";
const SETUP_OTHER_SESSION: &str = "SETUP reply names a session other than the first SETUP's";
const SETUP_RECORD_NOT_INTERLEAVED: &str = "SETUP reply for recording is not interleaved";

/// Two refusals the publish facade also gives itself, before it makes the
/// request.
pub(crate) const SETUP_BEFORE_ANNOUNCE: &str = "SETUP for recording before ANNOUNCE";
pub(crate) const RECORD_BEFORE_SETUP: &str = "RECORD before every track is set up";

#[derive(Debug, Default)]
pub(crate) struct EngineCounters {
    /// Complete RTSP messages read, ping replies excepted.
    pub rtsp_messages: u64,
    /// Frames on an RTP channel, malformed and dropped ones included.
    pub rtp_frames: u64,
    pub rtcp_frames: u64,
    /// Frames dropped (unknown channel, bad RTP header, non-UTF-8 headers)
    /// and framing violations that closed the stream.
    pub malformed_frames: u64,
    pub keepalive_responses: u64,
    pub unmatched_replies: u64,
    /// `1xx` replies to a request or ping still waiting for its final one.
    pub interim_replies: u64,
    /// Calls to `rtsp_frame_decision`: a header dribbled in costs a few,
    /// not one per read.
    pub frame_decisions: u64,
}

/// Session timeout until a SETUP reply names one.
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// Pings awaiting a reply that are remembered; an older one's late reply
/// is counted as unmatched.
const MAX_PINGS_AWAITING_REPLY: usize = 32;

const WRITE_STALLED_MSG: &str = "control TCP write made no progress for the write timeout";

/// The shortest interval the session timeout can set; an interval the
/// caller chose is floored at 1 ms only.
const MIN_LEARNED_INTERVAL_MS: u64 = 100;

/// Headers the engine writes itself; a spec must not carry them.
const ENGINE_HEADERS: [&str; 5] = [
    "cseq",
    "user-agent",
    "authorization",
    "content-type",
    "content-length",
];

#[derive(Debug)]
enum Keepalive {
    NotEnabled,
    On {
        interval: Duration,
        overridden: bool,
        /// Enable time, then the time of the last ping, sent or deferred.
        anchor: Instant,
        /// `None` when `anchor + interval` is not representable.
        next_due: Option<Instant>,
    },
    Off,
}

#[derive(Debug)]
struct CallerTxn {
    id: TxnId,
    /// CSeq of the attempt in flight.
    cseq: u32,
    spec: RequestSpec,
    deadline: Option<Instant>,
    retried: bool,
    /// The `401` challenge whose Digest answer waits for a cnonce, and
    /// the nonce count taken for that answer when the `401` was read.
    /// While it is set no attempt is in flight, so no reply is this
    /// request's.
    challenge: Option<(String, u32)>,
}

pub(crate) struct ClientEngine {
    cfg: EngineConfig,
    auth: AuthState,
    /// The cnonce handed in for the next Digest answer.
    cnonce: Option<Cnonce>,
    /// Last CSeq used, by a caller request or a ping: one counter, because
    /// some servers end the connection unless each request's CSeq is one
    /// above the last.
    cseq: u32,
    next_txn: u64,
    caller: Option<CallerTxn>,
    pings: VecDeque<u32>,
    session_id: Option<String>,
    session_timeout: Duration,
    keepalive: Keepalive,
    /// The kind every request gets once the stream has ended.
    closed: Option<io::ErrorKind>,
    transmits: VecDeque<Transmit>,
    events: VecDeque<Event>,
    counters: EngineCounters,
    /// Control-stream bytes not yet framed: at most one incomplete frame
    /// between calls to `handle_input`.
    inbuf: Vec<u8>,
    /// The scan of an incomplete RTSP message at the front of `inbuf`.
    front: Option<FrontScan>,
    /// Installed interleaved routes, by `RouteId`; until one is installed
    /// every `$` frame is counted as malformed and dropped.
    routes: [Option<Route>; MAX_ROUTES],
    /// `None` until an ANNOUNCE is accepted: a pull client.
    push: Option<PushPhase>,
    /// The track count of the accepted ANNOUNCE.
    push_tracks: u8,
    /// A TEARDOWN was submitted on a publish session: no request follows
    /// it. A session ended another way still takes its one TEARDOWN.
    push_torn_down: bool,
    /// A pull SETUP has named a session: the client cannot publish.
    pulled: bool,
}

// By hand: the queued transmits can carry an `Authorization` header.
impl std::fmt::Debug for ClientEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientEngine")
            .field("cseq", &self.cseq)
            .field("caller", &self.caller.as_ref().map(|t| t.id))
            .field("pings_awaiting_reply", &self.pings.len())
            .field("has_session", &self.session_id.is_some())
            .field("session_timeout", &self.session_timeout)
            .field("keepalive", &self.keepalive)
            .field("closed", &self.closed)
            .field("transmits", &self.transmits.len())
            .field("events", &self.events.len())
            .field("counters", &self.counters)
            .field("buffered", &self.inbuf.len())
            .field("routes", &self.routes)
            .field("push", &self.push)
            .finish_non_exhaustive()
    }
}

/// The phase after track `done` of `total` is set up.
fn after_track_setup(done: u8, total: u8) -> PushPhase {
    let done = done.saturating_add(1);
    if done >= total {
        PushPhase::Ready
    } else {
        PushPhase::SetUp { done, total }
    }
}

/// `d` in whole milliseconds, at least `floor_ms`.
fn whole_ms_at_least(d: Duration, floor_ms: u64) -> Duration {
    Duration::from_millis(duration_ms_saturating(d).max(floor_ms))
}

/// Whether a TEARDOWN reply with `status` ended the session: a `2xx`, or
/// `454 Session Not Found` (already gone, which is what was asked for).
pub(crate) fn teardown_accepted(status: u16) -> bool {
    (200..300).contains(&status) || status == 454
}

impl ClientEngine {
    pub(crate) fn new(cfg: EngineConfig) -> Self {
        let auth = AuthState {
            schemes: cfg.schemes,
            ..AuthState::default()
        };
        Self {
            cfg,
            auth,
            cnonce: None,
            cseq: 0,
            next_txn: 0,
            caller: None,
            pings: VecDeque::new(),
            session_id: None,
            session_timeout: DEFAULT_SESSION_TIMEOUT,
            keepalive: Keepalive::NotEnabled,
            closed: None,
            transmits: VecDeque::new(),
            events: VecDeque::new(),
            counters: EngineCounters::default(),
            inbuf: Vec::new(),
            front: None,
            routes: [None; MAX_ROUTES],
            push: None,
            push_tracks: 0,
            push_torn_down: false,
            pulled: false,
        }
    }

    /// Whether [`Self::submit`] of `spec` will answer the cached challenge
    /// with Digest and no cnonce is at hand: hand one in first.
    pub(crate) fn submit_wants_cnonce(&self, spec: &RequestSpec) -> bool {
        self.cnonce.is_none()
            && self.closed.is_none()
            && self.refusal(spec).is_none()
            && self.answers_cached_digest()
    }

    /// Whether [`Self::handle_timeout`] with the same arguments will send
    /// a ping that answers the cached challenge with Digest, and no cnonce
    /// is at hand: hand one in first.
    pub(crate) fn timeout_wants_cnonce(&self, now: Instant, cancelled: bool) -> bool {
        self.cnonce.is_none()
            && self.closed.is_none()
            && !cancelled
            && self.ping_due(now)
            && self.answers_cached_digest()
    }

    /// Whether the caller's request was answered `401` with a Digest
    /// challenge and its second attempt waits for a cnonce. Asked after
    /// input was handled; [`Self::handle_cnonce`] ends the wait.
    pub(crate) fn wants_cnonce(&self) -> bool {
        self.caller.as_ref().is_some_and(|t| t.challenge.is_some())
    }

    /// A cnonce the engine asked for, or `None` when the random source
    /// failed. A request waiting for one makes its second attempt now, and
    /// without a cnonce fails as any Digest answer does; otherwise the
    /// cnonce is kept for the next Digest answer.
    pub(crate) fn handle_cnonce(&mut self, cnonce: Option<Cnonce>) {
        if cnonce.is_some() {
            self.cnonce = cnonce;
        }
        if let Some(mut txn) = self.caller.take_if(|t| t.challenge.is_some())
            && let Some((www, nc)) = txn.challenge.take()
        {
            self.retry(txn, &www, Some(nc));
        }
    }

    /// Queue a caller request. It replaces any earlier caller transaction
    /// still waiting for its reply; that one gets no `Reply`.
    pub(crate) fn submit(
        &mut self,
        spec: RequestSpec,
        reply_deadline: Option<Instant>,
        _now: Instant,
    ) -> Result<TxnId, RtspError> {
        debug_assert!(
            !spec
                .headers
                .iter()
                .any(|(n, _)| ENGINE_HEADERS.iter().any(|h| n.eq_ignore_ascii_case(h))),
            "a request spec carries a header the engine writes itself"
        );
        if let Some(kind) = self.closed {
            return Err(RtspError::Io(kind));
        }
        // Before anything is encoded, so a refused request takes no CSeq.
        if let Some(detail) = self.refusal(&spec) {
            return Err(RtspError::NotAllowed { detail });
        }
        let authorization = match self.preemptive_authorization(spec.method, &spec.uri) {
            Ok(a) => a,
            // A TEARDOWN goes out unauthenticated rather than not at all.
            Err(_) if spec.method == RtspMethod::Teardown => None,
            Err(e) => return Err(e),
        };
        let (bytes, cseq) = self.encode_caller_request(&spec, authorization)?;
        if spec.method == RtspMethod::Teardown {
            self.session_id = None;
            self.keepalive = Keepalive::Off;
            if self.push.is_some() {
                // Media stops now, not at the reply.
                self.push = Some(PushPhase::Ended);
                self.push_torn_down = true;
            }
        }
        self.next_txn += 1;
        let id = TxnId(self.next_txn);
        self.caller = Some(CallerTxn {
            id,
            cseq,
            spec,
            deadline: reply_deadline,
            retried: false,
            challenge: None,
        });
        self.transmits.push_back(Transmit {
            bytes: bytes.into(),
            txn: Some(id),
            is_ping: false,
        });
        Ok(id)
    }

    /// Start pinging one interval from `now`. Only the first call acts.
    pub(crate) fn enable_keepalive(&mut self, interval_override: Option<Duration>, now: Instant) {
        if !matches!(self.keepalive, Keepalive::NotEnabled) || self.closed.is_some() {
            return;
        }
        let interval = match interval_override {
            Some(d) => whole_ms_at_least(d, 1),
            None => whole_ms_at_least(self.session_timeout / 2, MIN_LEARNED_INTERVAL_MS),
        };
        self.keepalive = Keepalive::On {
            interval,
            overridden: interval_override.is_some(),
            anchor: now,
            next_due: now.checked_add(interval),
        };
    }

    pub(crate) fn handle_reply(&mut self, resp: RtspResponse, now: Instant) {
        if self.closed.is_some() {
            return;
        }
        if (100..=199).contains(&resp.status) {
            self.handle_interim_reply(&resp);
            return;
        }
        let Some(cseq) = resp.cseq() else {
            // A reply with no `CSeq` at all goes to the caller's request, the
            // only one whose caller is waiting: a server that omits `CSeq`
            // would otherwise time every request out. Such a reply to a ping
            // can be taken for the request's. One whose `CSeq` does not parse
            // answers nothing.
            match self.caller.take() {
                Some(txn) if txn.challenge.is_none() && !resp.headers.contains_key("cseq") => {
                    self.handle_caller_reply(txn, resp, now);
                }
                other => {
                    self.caller = other;
                    self.counters.unmatched_replies += 1;
                }
            }
            return;
        };
        if let Some(i) = self.pings.iter().position(|&c| c == cseq) {
            self.pings.remove(i);
            self.counters.keepalive_responses += 1;
            self.handle_ping_reply(&resp);
            return;
        }
        match self.caller.take() {
            Some(txn) if txn.challenge.is_none() && txn.cseq == cseq => {
                self.handle_caller_reply(txn, resp, now)
            }
            other => {
                self.caller = other;
                self.counters.unmatched_replies += 1;
            }
        }
    }

    /// Time out the caller transaction and send a due ping. `cancelled`
    /// turns the keepalive off for good.
    pub(crate) fn handle_timeout(&mut self, now: Instant, cancelled: bool) {
        if self.closed.is_some() {
            return;
        }
        if self
            .caller
            .as_ref()
            .is_some_and(|t| t.deadline.is_some_and(|d| now >= d))
            && let Some(t) = self.caller.take()
        {
            self.reply(t.id, Err(RtspError::Timeout));
        }
        if cancelled {
            self.keepalive = Keepalive::Off;
            return;
        }
        if self.ping_due(now) {
            self.send_ping(now);
        }
    }

    /// Whether the keepalive is on and its next ping is due at `now`.
    fn ping_due(&self, now: Instant) -> bool {
        matches!(self.keepalive, Keepalive::On { next_due: Some(due), .. } if now >= due)
    }

    /// An earlier ping is not wholly written yet: a ping due at `now` is
    /// not sent, and the next is due one interval from `now`. Called ahead
    /// of [`Self::handle_timeout`].
    pub(crate) fn defer_ping(&mut self, now: Instant) {
        if let Keepalive::On {
            interval,
            anchor,
            next_due,
            ..
        } = &mut self.keepalive
            && next_due.is_some_and(|due| now >= due)
        {
            *anchor = now;
            *next_due = now.checked_add(*interval);
        }
    }

    /// The stream has ended. Only the first call acts.
    pub(crate) fn handle_closed(&mut self, end: StreamEnd) {
        if self.closed.is_some() {
            return;
        }
        let stopped = matches!(end, StreamEnd::Stopped);
        let (kind, reason) = match end {
            StreamEnd::PeerClosed => (io::ErrorKind::UnexpectedEof, EndReason::CleanTeardown),
            StreamEnd::ReadFailed(e) => (
                e.kind(),
                EndReason::TransportFailed {
                    msg: format!("TCP read failed: {e}"),
                },
            ),
            StreamEnd::WriteFailed(e) => (
                e.kind(),
                EndReason::TransportFailed {
                    msg: format!("TCP write failed: {e}"),
                },
            ),
            StreamEnd::WriteStalled { ping } => {
                let msg = WRITE_STALLED_MSG.to_string();
                let reason = if ping {
                    EndReason::KeepaliveFailed { msg }
                } else {
                    EndReason::TransportFailed { msg }
                };
                (io::ErrorKind::TimedOut, reason)
            }
            StreamEnd::Stopped => (io::ErrorKind::UnexpectedEof, EndReason::Cancelled),
        };
        self.end_stream(kind, reason, !stopped, RtspError::Io(kind));
    }

    /// The stream has ended, by `handle_closed` or a framing violation.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        if self.closed.is_some() {
            return None;
        }
        let ping = match self.keepalive {
            Keepalive::On { next_due, .. } => next_due,
            _ => None,
        };
        let deadline = self.caller.as_ref().and_then(|t| t.deadline);
        match (ping, deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// `None` on a client that has not published.
    pub(crate) fn push_phase(&self) -> Option<PushPhase> {
        self.push
    }

    /// Media may be written: a publish session recording or paused, its
    /// stream open. A submitted TEARDOWN has moved the phase to `Ended`,
    /// and nothing moves it on from there.
    pub(crate) fn media_allowed(&self) -> bool {
        matches!(self.push, Some(PushPhase::Recording | PushPhase::Paused)) && self.closed.is_none()
    }

    /// Why `spec` may not be sent now, if it may not. A publish session
    /// sends its requests in order, one role per connection: some servers
    /// answer an out-of-order request by closing the connection.
    fn refusal(&self, spec: &RequestSpec) -> Option<&'static str> {
        use RtspMethod as M;
        let record_setup = spec.setup.filter(|r| r.kind == RouteKind::Push);
        let Some(phase) = self.push else {
            let tracks_ok = spec
                .announce_tracks
                .is_some_and(|n| (1..=MAX_ROUTES).contains(&usize::from(n)));
            return match spec.method {
                // RTSP 2.0 has no ANNOUNCE or RECORD.
                M::Announce if self.cfg.version == RtspVersion::V2_0 => {
                    Some("ANNOUNCE on an RTSP/2.0 client")
                }
                M::Announce if self.pulled => Some("ANNOUNCE on a client that has a play session"),
                M::Announce if !tracks_ok => Some("ANNOUNCE without a track count of 1 to 4"),
                M::Setup if record_setup.is_some() => Some(SETUP_BEFORE_ANNOUNCE),
                M::Record => Some("RECORD before ANNOUNCE"),
                _ => None,
            };
        };
        if self.push_torn_down {
            return Some("request after TEARDOWN");
        }
        let next_track = match phase {
            PushPhase::Announced => Some(0),
            PushPhase::SetUp { done, .. } => Some(done),
            _ => None,
        };
        match (spec.method, phase) {
            // Above `Ended`: a session that expired is still closed with a
            // TEARDOWN on the open connection.
            (M::Teardown, _) => None,
            (_, PushPhase::Ended) => Some("request after the publish session ended"),
            (M::Options, _) => None,
            (M::Setup, _) if record_setup.is_some_and(|r| Some(r.id.0) == next_track) => None,
            (M::Setup, _) if record_setup.is_some() => {
                Some("SETUP for recording out of track order")
            }
            (M::Setup, _) => Some("SETUP for playing in a publish session"),
            (M::Record, PushPhase::Ready | PushPhase::Paused) => None,
            (M::Record, PushPhase::Recording) => Some("RECORD while recording"),
            (M::Record, _) => Some(RECORD_BEFORE_SETUP),
            (M::Pause, PushPhase::Recording) => None,
            (M::Pause, _) => Some("PAUSE while not recording"),
            (M::Announce, _) => Some("a second ANNOUNCE"),
            (M::Describe, _) => Some("DESCRIBE in a publish session"),
            (M::Play, _) => Some("PLAY in a publish session"),
            (M::GetParameter, _) => Some("GET_PARAMETER in a publish session"),
        }
    }

    /// A publish session ends for good: media stops.
    fn end_push(&mut self) {
        if self.push.is_some() {
            self.push = Some(PushPhase::Ended);
        }
    }

    /// The accepted final reply to `method` moves the publish session on;
    /// a reply that finds the session elsewhere (ended, say) moves nothing.
    fn advance_push(&mut self, spec: &RequestSpec) {
        let total = self.push_tracks;
        let track = spec.setup.map(|r| r.id.0);
        self.push = match (spec.method, self.push) {
            (RtspMethod::Announce, None) => {
                self.push_tracks = spec.announce_tracks.unwrap_or(0);
                Some(PushPhase::Announced)
            }
            (RtspMethod::Setup, Some(PushPhase::Announced)) if track == Some(0) => {
                Some(after_track_setup(0, total))
            }
            (RtspMethod::Setup, Some(PushPhase::SetUp { done, total })) if track == Some(done) => {
                Some(after_track_setup(done, total))
            }
            (RtspMethod::Record, Some(PushPhase::Ready | PushPhase::Paused)) => {
                Some(PushPhase::Recording)
            }
            (RtspMethod::Pause, Some(PushPhase::Recording)) => Some(PushPhase::Paused),
            (_, unchanged) => unchanged,
        };
    }

    #[cfg(test)]
    pub(crate) fn counters(&self) -> &EngineCounters {
        &self.counters
    }

    /// The next request or ping carries `n`.
    #[cfg(test)]
    pub(crate) fn set_next_cseq(&mut self, n: u32) {
        self.cseq = n.wrapping_sub(1);
    }

    /// A `1xx` says the final reply is still to come: it answers nothing
    /// and moves no deadline. One without a `CSeq` header is taken as the
    /// waiting caller's, as a final reply would be.
    fn handle_interim_reply(&mut self, resp: &RtspResponse) {
        let awaited = match resp.cseq() {
            Some(c) => self.pings.contains(&c) || self.caller.as_ref().is_some_and(|t| t.cseq == c),
            None => self.caller.is_some() && !resp.headers.contains_key("cseq"),
        };
        if awaited {
            self.counters.interim_replies += 1;
        } else {
            self.counters.unmatched_replies += 1;
        }
    }

    /// End the stream for good: queued transmits, awaited pings and
    /// buffered input are dropped, and a waiting request gets `caller_error`
    /// after the `Ended`, so whoever its reply wakes already sees why.
    fn end_stream(
        &mut self,
        kind: io::ErrorKind,
        reason: EndReason,
        session_dead: bool,
        caller_error: RtspError,
    ) {
        self.closed = Some(kind);
        self.end_push();
        self.keepalive = Keepalive::Off;
        self.pings.clear();
        self.transmits.clear();
        self.inbuf = Vec::new();
        self.front = None;
        if session_dead {
            self.events.push_back(Event::SessionDead);
        }
        self.events.push_back(Event::Ended(reason));
        if let Some(t) = self.caller.take() {
            self.reply(t.id, Err(caller_error));
        }
    }

    fn reply(&mut self, txn: TxnId, result: Result<ReplyOutcome, RtspError>) {
        self.events.push_back(Event::Reply { txn, result });
    }

    /// An `Authorization` value for `method` at `uri` answering `www`,
    /// under the next nonce count. `AuthFailed` without credentials. A
    /// Digest answer uses up the cnonce at hand, and without one is the
    /// random source's failure, `Io(Other)`.
    fn authorization(
        &mut self,
        method: RtspMethod,
        uri: &str,
        www: &str,
    ) -> Result<String, RtspError> {
        if self.cfg.credentials.is_none() {
            return Err(RtspError::AuthFailed);
        }
        let nc = self.next_nonce_count();
        self.authorization_under(method, uri, www, nc)
    }

    /// Take the next nonce count of the cached challenge.
    fn next_nonce_count(&mut self) -> u32 {
        self.auth.nc = self.auth.nc.wrapping_add(1);
        self.auth.nc
    }

    /// [`Self::authorization`] under a nonce count taken before.
    fn authorization_under(
        &mut self,
        method: RtspMethod,
        uri: &str,
        www: &str,
        nc: u32,
    ) -> Result<String, RtspError> {
        let creds = self.cfg.credentials.as_ref().ok_or(RtspError::AuthFailed)?;
        let cnonce = if answers_with_digest(www) {
            self.cnonce.take()
        } else {
            None
        };
        build_authorization(
            method,
            uri,
            www,
            &creds.username,
            &creds.password,
            nc,
            self.auth.schemes,
            cnonce.as_ref(),
        )
    }

    /// Whether a request or ping sent now answers the cached challenge
    /// with Digest.
    fn answers_cached_digest(&self) -> bool {
        self.cfg.credentials.is_some()
            && self
                .auth
                .challenge
                .as_deref()
                .is_some_and(answers_with_digest)
    }

    /// The cached challenge's answer, or `None` without a challenge or
    /// without credentials.
    fn preemptive_authorization(
        &mut self,
        method: RtspMethod,
        uri: &str,
    ) -> Result<Option<String>, RtspError> {
        if self.cfg.credentials.is_none() {
            return Ok(None);
        }
        let Some(www) = self.auth.challenge.clone() else {
            return Ok(None);
        };
        self.authorization(method, uri, &www).map(Some)
    }

    /// `spec` under the next CSeq, with `User-Agent`, `authorization` and
    /// the body, encoded. A request refused here never reaches the wire, so
    /// it takes no CSeq.
    fn encode_caller_request(
        &mut self,
        spec: &RequestSpec,
        authorization: Option<String>,
    ) -> Result<(Bytes, u32), RtspError> {
        let cseq = self.cseq.wrapping_add(1);
        let mut req = RtspRequest::new(spec.method, spec.uri.as_str(), self.cfg.version)
            .header("cseq", cseq.to_string())
            .header("user-agent", self.cfg.user_agent.as_str());
        for (name, value) in &spec.headers {
            req = req.header(name, value.as_str());
        }
        if let Some(a) = authorization {
            req = req.header("authorization", a);
        }
        if let Some(body) = &spec.body {
            req = req
                .header("content-type", body.content_type)
                .body(body.bytes.clone());
        }
        let bytes = req.encode_checked()?;
        self.cseq = cseq;
        Ok((bytes, cseq))
    }

    fn handle_caller_reply(&mut self, mut txn: CallerTxn, resp: RtspResponse, now: Instant) {
        if txn.deadline.is_some_and(|d| now >= d) {
            self.reply(txn.id, Err(RtspError::Timeout));
            return;
        }
        if resp.status == 401 {
            if txn.retried {
                self.reply(txn.id, Err(RtspError::AuthFailed));
                return;
            }
            let Some(www) = resp.headers.get("www-authenticate") else {
                self.reply(
                    txn.id,
                    Err(RtspError::BadResponse {
                        detail: "401 without WWW-Authenticate header",
                    }),
                );
                return;
            };
            self.auth.cache_challenge(www.clone());
            if self.cnonce.is_none() && self.cfg.credentials.is_some() && answers_with_digest(www) {
                // The second attempt is made when the cnonce is handed in,
                // under the count it would have taken here: a challenge
                // cached meanwhile starts its own count.
                txn.challenge = Some((www.clone(), self.next_nonce_count()));
                self.caller = Some(txn);
                return;
            }
            self.retry(txn, www, None);
            return;
        }
        let mut setup = None;
        let is_2xx = (200..300).contains(&resp.status);
        let record_setup = txn.spec.setup.is_some_and(|r| r.kind == RouteKind::Push);
        match txn.spec.method {
            RtspMethod::Setup if is_2xx => match self.take_setup_reply(&txn.spec, &resp) {
                Ok(accepted) => setup = accepted,
                Err(e) => {
                    self.reply(txn.id, Err(e));
                    return;
                }
            },
            RtspMethod::Teardown if teardown_accepted(resp.status) => {
                self.events
                    .push_back(Event::Ended(EndReason::CleanTeardown));
            }
            _ => {}
        }
        if is_2xx && (record_setup || txn.spec.method != RtspMethod::Setup) {
            self.advance_push(&txn.spec);
        }
        self.reply(
            txn.id,
            Ok(ReplyOutcome {
                response: resp,
                setup,
            }),
        );
    }

    /// Take a `2xx` SETUP reply, or refuse it. The pull side takes exactly
    /// `200` (its facade refuses any other); a publish session takes any
    /// `2xx` that names no other session than its first SETUP's and whose
    /// interleaved route installs. Only a reply taken installs a route or
    /// teaches the session (its id, its timeout); one not taken changes
    /// nothing.
    fn take_setup_reply(
        &mut self,
        spec: &RequestSpec,
        resp: &RtspResponse,
    ) -> Result<Option<SetupAccepted>, RtspError> {
        let record = spec.setup.is_some_and(|r| r.kind == RouteKind::Push);
        if !record && resp.status != 200 {
            return Ok(None);
        }
        // A publish session is one session: a broken server that names
        // another is not followed.
        if record
            && let (Some(known), Some(named)) = (&self.session_id, resp.session_id())
            && known != named
        {
            return Err(RtspError::BadResponse {
                detail: SETUP_OTHER_SESSION,
            });
        }
        // A pull reply without a session keeps no route either.
        let route = spec
            .setup
            .filter(|r| r.kind == RouteKind::Push || resp.session_id().is_some());
        // Here, not by a later call: frames can follow the reply in the
        // same read, and they are routed next.
        let accepted = match route {
            Some(r) => self.accept_setup(r, resp)?,
            None => None,
        };
        if record && accepted.is_none() {
            return Err(RtspError::BadResponse {
                detail: SETUP_RECORD_NOT_INTERLEAVED,
            });
        }
        self.pulled |= !record && resp.session_id().is_some();
        self.learn_session(resp);
        Ok(accepted)
    }

    /// Install `route` from an accepted SETUP reply's `Transport`: its
    /// `interleaved=` pair, or `0-1` when a TCP reply names none. A reply
    /// that is not interleaved installs nothing.
    fn accept_setup(
        &mut self,
        route: SetupRoute,
        resp: &RtspResponse,
    ) -> Result<Option<SetupAccepted>, RtspError> {
        let header = resp
            .headers
            .get("transport")
            .ok_or(RtspError::BadResponse {
                detail: SETUP_WITHOUT_TRANSPORT,
            })?;
        let transport = parse_transport_response(header)?;
        if transport.kind != RtspTransportKind::TcpInterleaved {
            return Ok(None);
        }
        let (rtp, rtcp) = transport.interleaved.unwrap_or((0, 1));
        self.install_route(route, rtp, rtcp)
            .map_err(|detail| RtspError::BadResponse { detail })?;
        Ok(Some(SetupAccepted {
            route: route.id,
            rtp_channel: rtp,
            rtcp_channel: rtcp,
        }))
    }

    /// Route `$` frames on `rtp` and `rtcp` to `route`, in place of the
    /// pair installed under the same id. Refused, with nothing changed: an
    /// id past `MAX_ROUTES`, a channel another installed route uses, or,
    /// on a route this side sends on, one channel for both. A receiving
    /// route takes one channel for both: its frames are all read as RTP.
    pub(crate) fn install_route(
        &mut self,
        route: SetupRoute,
        rtp: u8,
        rtcp: u8,
    ) -> Result<(), &'static str> {
        let at = usize::from(route.id.0);
        if at >= MAX_ROUTES {
            return Err(ROUTE_ID_OUT_OF_RANGE);
        }
        // A server that names one channel twice in a play SETUP reply
        // sends media on it; a publisher needs the second channel for the
        // server's reports.
        if rtp == rtcp && route.kind == RouteKind::Push {
            return Err(ROUTE_CHANNELS_EQUAL);
        }
        let taken = self.routes.iter().enumerate().any(|(i, other)| {
            i != at
                && other.is_some_and(|o| [o.rtp, o.rtcp].iter().any(|c| *c == rtp || *c == rtcp))
        });
        if taken {
            return Err(ROUTE_CHANNEL_IN_USE);
        }
        if let Some(slot) = self.routes.get_mut(at) {
            *slot = Some(Route {
                kind: route.kind,
                rtp,
                rtcp,
            });
        }
        Ok(())
    }

    /// Queue `txn`'s second attempt, answering the `401`'s challenge
    /// `www` under the nonce count `taken` for it, or under the next one;
    /// a second attempt that cannot be built is the request's reply.
    fn retry(&mut self, mut txn: CallerTxn, www: &str, taken: Option<u32>) {
        let authorization = match taken {
            Some(nc) => self.authorization_under(txn.spec.method, &txn.spec.uri, www, nc),
            None => self.authorization(txn.spec.method, &txn.spec.uri, www),
        };
        let attempt = authorization
            .and_then(|authorization| self.encode_caller_request(&txn.spec, Some(authorization)));
        match attempt {
            Ok((bytes, cseq)) => {
                txn.cseq = cseq;
                txn.retried = true;
                // The caller was told "written" on the first attempt.
                self.transmits.push_back(Transmit {
                    bytes: bytes.into(),
                    txn: None,
                    is_ping: false,
                });
                self.caller = Some(txn);
            }
            Err(e) => self.reply(txn.id, Err(e)),
        }
    }

    /// Record a SETUP reply's session and retune a keepalive the caller did
    /// not pin to an interval.
    fn learn_session(&mut self, resp: &RtspResponse) {
        let Some(sid) = resp.session_id() else {
            return;
        };
        self.session_id = Some(sid.to_string());
        if let Some(t) = resp.session_timeout_secs() {
            self.session_timeout = Duration::from_secs(t);
        }
        // The floor keeps a `timeout=0` from turning the pings into a busy loop.
        let retuned = whole_ms_at_least(self.session_timeout / 2, MIN_LEARNED_INTERVAL_MS);
        if let Keepalive::On {
            interval,
            overridden: false,
            anchor,
            next_due,
        } = &mut self.keepalive
        {
            *interval = retuned;
            *next_due = anchor.checked_add(retuned);
        }
    }

    fn handle_ping_reply(&mut self, resp: &RtspResponse) {
        match resp.status {
            401 => {
                if let Some(www) = resp.headers.get("www-authenticate") {
                    tracing::debug!(
                        target: "tst_rtp::client::keepalive",
                        "keepalive ping got 401; refreshing cached auth challenge"
                    );
                    self.auth.cache_challenge(www.clone());
                }
            }
            454 => {
                tracing::warn!(
                    target: "tst_rtp::client::keepalive",
                    "keepalive ping got 454 Session Not Found; marking session dead"
                );
                self.keepalive = Keepalive::Off;
                self.pings.clear();
                self.end_push();
                self.events.push_back(Event::SessionDead);
                self.events
                    .push_back(Event::Ended(EndReason::SessionExpired));
            }
            _ => {}
        }
    }

    fn send_ping(&mut self, now: Instant) {
        // Taken only once the ping encodes, as for a caller request.
        let cseq = self.cseq.wrapping_add(1);
        let uri = self.cfg.uri.clone();
        let mut req = RtspRequest::new(RtspMethod::Options, uri.as_str(), self.cfg.version)
            .header("cseq", cseq.to_string())
            .header("user-agent", self.cfg.user_agent.as_str());
        if let Some(sid) = &self.session_id {
            req = req.header("session", sid.as_str());
        }
        // A ping whose header cannot be built goes out without one.
        if let Ok(Some(a)) = self.preemptive_authorization(RtspMethod::Options, &uri) {
            req = req.header("authorization", a);
        }
        match req.encode_checked() {
            Ok(bytes) => {
                self.cseq = cseq;
                if self.pings.len() == MAX_PINGS_AWAITING_REPLY {
                    self.pings.pop_front();
                }
                self.pings.push_back(cseq);
                if let Keepalive::On {
                    interval,
                    anchor,
                    next_due,
                    ..
                } = &mut self.keepalive
                {
                    *anchor = now;
                    *next_due = now.checked_add(*interval);
                }
                self.transmits.push_back(Transmit {
                    bytes: bytes.into(),
                    txn: None,
                    is_ping: true,
                });
            }
            Err(e) => {
                tracing::warn!(
                    target: "tst_rtp::client::keepalive",
                    error = %e,
                    "keepalive ping failed the header-injection guard; marking session dead"
                );
                self.keepalive = Keepalive::Off;
                self.end_push();
                self.events.push_back(Event::SessionDead);
                self.events
                    .push_back(Event::Ended(EndReason::KeepaliveFailed {
                        msg: format!("keepalive ping failed to encode: {e}"),
                    }));
            }
        }
    }
}

#[cfg(test)]
mod tests;
