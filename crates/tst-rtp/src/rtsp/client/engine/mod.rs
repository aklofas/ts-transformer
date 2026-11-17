//! The RTSP client's protocol state, without a socket: it is handed
//! replies, the current time and requests, and hands back the bytes to
//! write, events and its next deadline. Nothing here reads a clock, opens
//! a socket or spawns a thread.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use bytes::Bytes;
use secrecy::SecretString;
use tst_core::transport::EndReason;

use crate::error::RtspError;
use crate::rtsp::auth::{AuthSchemes, build_authorization};
use crate::rtsp::client::{AuthState, duration_ms_saturating};
use crate::rtsp::message::{RtspMethod, RtspRequest, RtspResponse};
use crate::url::RtspVersion;

mod inbound;
use inbound::FrontScan;

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
/// `Authorization`.
#[derive(Debug)]
pub(crate) struct RequestSpec {
    pub method: RtspMethod,
    pub uri: String,
    pub headers: Vec<(&'static str, String)>,
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
        result: Result<RtspResponse, RtspError>,
    },
    Media(Bytes),
    Rtcp(Bytes),
    SessionDead,
    Ended(EndReason),
}

/// SETUP-allocated channel pair. The SETUP handler assigns these (the
/// client requests `interleaved=N-M` in its Transport header; the server
/// echoes or reassigns).
#[derive(Debug, Clone, Copy)]
pub(crate) struct InterleavedChannels {
    /// Channel number for RTP data frames.
    pub(crate) rtp: u8,
    /// Channel number for RTCP frames (conventionally `rtp + 1`).
    pub(crate) rtcp: u8,
}

#[derive(Debug, Default)]
pub(crate) struct EngineCounters {
    /// Complete RTSP messages read, ping replies excepted.
    pub rtsp_messages: u64,
    /// Frames on the RTP channel, malformed ones included.
    pub rtp_frames: u64,
    pub rtcp_frames: u64,
    /// Frames dropped (unknown channel, bad RTP header, non-UTF-8 headers)
    /// and framing violations that closed the stream.
    pub malformed_frames: u64,
    pub keepalive_responses: u64,
    pub unmatched_replies: u64,
    /// Calls to `rtsp_frame_decision`: a header dribbled in costs a few,
    /// not one per read.
    pub frame_decisions: u64,
}

/// The first ping's CSeq is one above this: far above the caller counter,
/// which starts at 1.
const PING_CSEQ_BASE: u32 = 1_000_000;

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
const ENGINE_HEADERS: [&str; 3] = ["cseq", "user-agent", "authorization"];

#[derive(Debug)]
enum Keepalive {
    NotEnabled,
    On {
        interval: Duration,
        overridden: bool,
        /// Enable time, then the time of the last ping.
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
}

pub(crate) struct ClientEngine {
    cfg: EngineConfig,
    auth: AuthState,
    /// Last caller CSeq used.
    cseq: u32,
    /// Last ping CSeq used.
    ping_cseq: u32,
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
    channels: Option<InterleavedChannels>,
}

// By hand: the queued transmits can carry an `Authorization` header.
impl std::fmt::Debug for ClientEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientEngine")
            .field("cseq", &self.cseq)
            .field("ping_cseq", &self.ping_cseq)
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
            .field("channels", &self.channels)
            .finish_non_exhaustive()
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
            cseq: 0,
            ping_cseq: PING_CSEQ_BASE,
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
            channels: None,
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
        }
        self.next_txn += 1;
        let id = TxnId(self.next_txn);
        self.caller = Some(CallerTxn {
            id,
            cseq,
            spec,
            deadline: reply_deadline,
            retried: false,
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
        let Some(cseq) = resp.cseq() else {
            // A reply with no `CSeq` at all goes to the caller's request, the
            // only one whose caller is waiting: a server that omits `CSeq`
            // would otherwise time every request out. Such a reply to a ping
            // can be taken for the request's. One whose `CSeq` does not parse
            // answers nothing.
            match self.caller.take() {
                Some(txn) if !resp.headers.contains_key("cseq") => {
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
            Some(txn) if txn.cseq == cseq => self.handle_caller_reply(txn, resp, now),
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
        if let Keepalive::On {
            next_due: Some(due),
            ..
        } = self.keepalive
            && now >= due
        {
            self.send_ping(now);
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

    #[cfg(test)]
    pub(crate) fn counters(&self) -> &EngineCounters {
        &self.counters
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

    fn reply(&mut self, txn: TxnId, result: Result<RtspResponse, RtspError>) {
        self.events.push_back(Event::Reply { txn, result });
    }

    /// An `Authorization` value for `method` at `uri` answering `www`,
    /// under the next nonce count. `AuthFailed` without credentials.
    fn authorization(
        &mut self,
        method: RtspMethod,
        uri: &str,
        www: &str,
    ) -> Result<String, RtspError> {
        let creds = self.cfg.credentials.as_ref().ok_or(RtspError::AuthFailed)?;
        self.auth.nc = self.auth.nc.wrapping_add(1);
        build_authorization(
            method,
            uri,
            www,
            &creds.username,
            &creds.password,
            self.auth.nc,
            self.auth.schemes,
        )
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

    /// `spec` under the next caller CSeq, with `User-Agent` and
    /// `authorization`, encoded. The CSeq is used even when encoding fails.
    fn encode_caller_request(
        &mut self,
        spec: &RequestSpec,
        authorization: Option<String>,
    ) -> Result<(Bytes, u32), RtspError> {
        self.cseq = self.cseq.wrapping_add(1);
        let mut req = RtspRequest::new(spec.method, spec.uri.as_str(), self.cfg.version)
            .header("cseq", self.cseq.to_string())
            .header("user-agent", self.cfg.user_agent.as_str());
        for (name, value) in &spec.headers {
            req = req.header(name, value.as_str());
        }
        if let Some(a) = authorization {
            req = req.header("authorization", a);
        }
        Ok((req.encode_checked()?, self.cseq))
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
            match self.retry_with_challenge(&mut txn, &resp) {
                Ok(bytes) => {
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
            return;
        }
        match txn.spec.method {
            RtspMethod::Setup if resp.status == 200 => self.learn_session(&resp),
            RtspMethod::Teardown if teardown_accepted(resp.status) => {
                self.events
                    .push_back(Event::Ended(EndReason::CleanTeardown));
            }
            _ => {}
        }
        self.reply(txn.id, Ok(resp));
    }

    /// Cache the `401`'s challenge and encode `txn`'s second attempt.
    fn retry_with_challenge(
        &mut self,
        txn: &mut CallerTxn,
        resp: &RtspResponse,
    ) -> Result<Bytes, RtspError> {
        let www = resp
            .headers
            .get("www-authenticate")
            .ok_or(RtspError::BadResponse {
                detail: "401 without WWW-Authenticate header",
            })?
            .clone();
        self.auth.cache_challenge(www.clone());
        let authorization = self.authorization(txn.spec.method, &txn.spec.uri, &www)?;
        let (bytes, cseq) = self.encode_caller_request(&txn.spec, Some(authorization))?;
        txn.cseq = cseq;
        txn.retried = true;
        Ok(bytes)
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
                self.events.push_back(Event::SessionDead);
                self.events
                    .push_back(Event::Ended(EndReason::SessionExpired));
            }
            _ => {}
        }
    }

    fn send_ping(&mut self, now: Instant) {
        self.ping_cseq = self.ping_cseq.wrapping_add(1);
        let uri = self.cfg.uri.clone();
        let mut req = RtspRequest::new(RtspMethod::Options, uri.as_str(), self.cfg.version)
            .header("cseq", self.ping_cseq.to_string())
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
                if self.pings.len() == MAX_PINGS_AWAITING_REPLY {
                    self.pings.pop_front();
                }
                self.pings.push_back(self.ping_cseq);
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
