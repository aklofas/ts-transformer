//! The RTSP client's protocol state, without a socket: it is handed
//! replies, the current time and requests, and hands back the bytes to
//! write, events and its next deadline. Nothing here reads a clock, opens
//! a socket or spawns a thread.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use bytes::Bytes;
use secrecy::SecretString;
use tst_core::transport::EndReason;

use crate::error::RtspError;
use crate::packet::RtpHeader;
use crate::rtsp::auth::{AuthSchemes, build_authorization};
use crate::rtsp::client::{AuthState, duration_ms_saturating};
use crate::rtsp::framing::parse_binary_frame_header;
use crate::rtsp::message::{
    RtspFraming, RtspMethod, RtspRequest, RtspResponse, rtsp_frame_decision,
};
use crate::url::RtspVersion;

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
            // A reply with no `CSeq` at all answers the waiting request, as
            // RTSP is not pipelined; one whose `CSeq` does not parse does not.
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

    /// Bytes read from the control stream: interleaved `$` frames and RTSP
    /// messages, split at any byte. Ignored once the stream has ended.
    pub(crate) fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        if self.closed.is_some() {
            return;
        }
        self.inbuf.extend_from_slice(bytes);
        // Frames are consumed by offset and the buffer drained once, so a
        // read holding many small frames costs one copy, not one per frame.
        let mut at = 0;
        while self.closed.is_none() {
            let rest = self.inbuf.get(at..).unwrap_or_default();
            if rest.is_empty() {
                break;
            }
            let used = if rest.first() == Some(&b'$') {
                let Some((channel, total_len)) = parse_binary_frame_header(rest) else {
                    break;
                };
                let payload = Bytes::copy_from_slice(rest.get(4..total_len).unwrap_or_default());
                self.route_frame(channel, payload);
                total_len
            } else {
                match rtsp_frame_decision(rest) {
                    RtspFraming::NeedMore => break,
                    RtspFraming::Complete { total_len } => {
                        let msg = rest.get(..total_len).unwrap_or_default();
                        let response_like = msg.starts_with(b"RTSP/");
                        let parsed = RtspResponse::parse(msg).map(|(resp, _)| resp);
                        self.route_message(parsed, response_like, now);
                        total_len
                    }
                    RtspFraming::NonUtf8Headers { skip } => {
                        self.counters.malformed_frames += 1;
                        if let Some(t) = self.caller.take() {
                            self.reply(
                                t.id,
                                Err(RtspError::BadResponse {
                                    detail: "non-UTF8 RTSP headers",
                                }),
                            );
                        }
                        skip
                    }
                    RtspFraming::HeadersTooLong => {
                        let msg = format!(
                            "control stream buffer exceeded RTSP header/body caps ({} bytes buffered)",
                            rest.len()
                        );
                        self.framing_violation("RTSP response headers exceed maximum", msg);
                        break;
                    }
                    RtspFraming::BadContentLength(detail) => {
                        let msg = format!("malformed RTSP Content-Length: {detail}");
                        self.framing_violation(detail, msg);
                        break;
                    }
                    RtspFraming::BodyOffsetOverflow => {
                        let detail = "RTSP body offset overflow";
                        self.framing_violation(detail, detail.to_string());
                        break;
                    }
                }
            };
            at += used;
        }
        if self.closed.is_none() {
            self.inbuf.drain(..at.min(self.inbuf.len()));
        }
    }

    /// The interleaved channels SETUP agreed; until called, every `$` frame
    /// is counted as malformed and dropped.
    pub(crate) fn set_interleaved(&mut self, channels: InterleavedChannels) {
        self.channels = Some(channels);
    }

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
        if session_dead {
            self.events.push_back(Event::SessionDead);
        }
        self.events.push_back(Event::Ended(reason));
        if let Some(t) = self.caller.take() {
            self.reply(t.id, Err(caller_error));
        }
    }

    /// The control stream broke a framing rule it cannot be resynced past.
    fn framing_violation(&mut self, detail: &'static str, msg: String) {
        tracing::warn!(target: "tst_rtp::client::pump", %msg, "closing the control stream");
        self.counters.malformed_frames += 1;
        self.end_stream(
            io::ErrorKind::InvalidData,
            EndReason::ProtocolError { msg },
            true,
            RtspError::BadResponse { detail },
        );
    }

    /// One `$` frame: whole RTP packets on the RTP channel (their header is
    /// checked, not stripped), payloads on the RTCP channel.
    fn route_frame(&mut self, channel: u8, payload: Bytes) {
        match self.channels {
            Some(c) if channel == c.rtp => {
                self.counters.rtp_frames += 1;
                match RtpHeader::decode(&payload) {
                    Ok(_) => self.events.push_back(Event::Media(payload)),
                    Err(e) => {
                        self.counters.malformed_frames += 1;
                        tracing::debug!(
                            target: "tst_rtp::client::pump",
                            error = ?e,
                            "interleaved RTP frame rejected; counter ticked",
                        );
                    }
                }
            }
            Some(c) if channel == c.rtcp => {
                self.counters.rtcp_frames += 1;
                self.events.push_back(Event::Rtcp(payload));
            }
            _ => {
                self.counters.malformed_frames += 1;
                tracing::debug!(
                    target: "tst_rtp::client::pump",
                    channel,
                    "frame on unknown channel; dropping"
                );
            }
        }
    }

    /// One complete RTSP message. A response that does not parse fails the
    /// waiting request with the parse error; a message that is not a
    /// response (a server request) answers nothing.
    fn route_message(
        &mut self,
        parsed: Result<RtspResponse, RtspError>,
        response_like: bool,
        now: Instant,
    ) {
        let resp = match parsed {
            Ok(resp) => resp,
            Err(e) => {
                self.counters.rtsp_messages += 1;
                match self.caller.take() {
                    Some(t) if response_like => self.reply(t.id, Err(e)),
                    other => {
                        self.caller = other;
                        self.counters.unmatched_replies += 1;
                    }
                }
                return;
            }
        };
        if !resp.cseq().is_some_and(|c| self.pings.contains(&c)) {
            self.counters.rtsp_messages += 1;
        }
        self.handle_reply(resp, now);
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
            RtspMethod::Teardown if (200..300).contains(&resp.status) || resp.status == 454 => {
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
mod tests {
    use super::*;

    const URI: &str = "rtsp://cam.example/live";

    fn cfg(creds: Option<(&str, &str)>) -> EngineConfig {
        EngineConfig {
            uri: URI.to_string(),
            version: RtspVersion::V1_0,
            user_agent: "tst-rtp/test".to_string(),
            credentials: creds.map(|(u, p)| Credentials {
                username: u.to_string(),
                password: SecretString::new(p.into()),
            }),
            schemes: AuthSchemes::Automatic,
        }
    }

    fn spec(method: RtspMethod, uri: &str) -> RequestSpec {
        RequestSpec {
            method,
            uri: uri.to_string(),
            headers: Vec::new(),
        }
    }

    fn resp(bytes: &[u8]) -> RtspResponse {
        RtspResponse::parse(bytes).unwrap().0
    }

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    fn sent(t: &Transmit) -> RtspRequest {
        RtspRequest::parse(&t.bytes).unwrap().0
    }

    fn cseq(t: &Transmit) -> u32 {
        sent(t).headers["cseq"].parse().unwrap()
    }

    fn authz(t: &Transmit) -> Option<String> {
        sent(t).headers.get("authorization").cloned()
    }

    fn param<'a>(authorization: &'a str, key: &str) -> &'a str {
        let at = authorization
            .find(&format!("{key}="))
            .unwrap_or_else(|| panic!("no {key} in {authorization}"))
            + key.len()
            + 1;
        authorization[at..]
            .split(',')
            .next()
            .unwrap()
            .trim_matches('"')
    }

    fn transmits(e: &mut ClientEngine) -> Vec<Transmit> {
        std::iter::from_fn(|| e.poll_transmit()).collect()
    }

    fn events(e: &mut ClientEngine) -> Vec<Event> {
        std::iter::from_fn(|| e.poll_event()).collect()
    }

    fn one(e: &mut ClientEngine) -> Transmit {
        let mut t = transmits(e);
        assert_eq!(t.len(), 1, "expected one transmit, got {t:?}");
        t.remove(0)
    }

    /// The only event is the reply to `txn`; returns its result.
    fn reply(e: &mut ClientEngine, txn: TxnId) -> Result<RtspResponse, RtspError> {
        let mut ev = events(e);
        assert_eq!(ev.len(), 1, "expected one event, got {ev:?}");
        match ev.remove(0) {
            Event::Reply { txn: t, result } => {
                assert_eq!(t, txn);
                result
            }
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    const BASIC_401: &[u8] =
        b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n";

    fn md5_hex(s: &str) -> String {
        use md5::{Digest, Md5};
        Md5::digest(s.as_bytes())
            .iter()
            .fold(String::new(), |mut out, b| {
                use std::fmt::Write as _;
                let _ = write!(out, "{b:02x}");
                out
            })
    }

    /// An engine whose cached challenge is `www`, learned from a `401` to an
    /// OPTIONS; the retry and its `200` are consumed.
    fn challenged(creds: Option<(&str, &str)>, www: &str) -> ClientEngine {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(creds));
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let first = one(&mut e);
        let c = cseq(&first);
        e.handle_reply(
            resp(
                format!(
                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: {www}\r\n\r\n"
                )
                .as_bytes(),
            ),
            t0,
        );
        if creds.is_some() {
            let retry = one(&mut e);
            e.handle_reply(
                resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&retry)).as_bytes()),
                t0,
            );
            assert_eq!(reply(&mut e, a).unwrap().status, 200);
        } else {
            assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
        }
        e
    }

    // --- submit ---

    #[test]
    fn submit_writes_cseq_user_agent_and_the_spec_headers() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let mut describe = spec(RtspMethod::Describe, URI);
        describe
            .headers
            .push(("accept", "application/sdp".to_string()));
        let a = e.submit(describe, None, t0).unwrap();
        let t = one(&mut e);
        assert_eq!(t.txn, Some(a));
        assert!(!t.is_ping);
        assert!(
            t.bytes
                .starts_with(b"DESCRIBE rtsp://cam.example/live RTSP/1.0\r\n"),
            "{:?}",
            String::from_utf8_lossy(&t.bytes)
        );
        let r = sent(&t);
        let mut names: Vec<_> = r.headers.keys().cloned().collect();
        names.sort();
        assert_eq!(names, ["accept", "cseq", "user-agent"]);
        assert_eq!(r.headers["cseq"], "1");
        assert_eq!(r.headers["user-agent"], "tst-rtp/test");
        assert_eq!(r.headers["accept"], "application/sdp");

        let b = e
            .submit(spec(RtspMethod::Play, "rtsp://cam.example/live/"), None, t0)
            .unwrap();
        assert_ne!(a, b);
        let t = one(&mut e);
        assert!(
            t.bytes
                .starts_with(b"PLAY rtsp://cam.example/live/ RTSP/1.0\r\n")
        );
        assert_eq!(cseq(&t), 2);
        assert!(events(&mut e).is_empty());
    }

    #[test]
    fn submit_uses_the_configured_version() {
        let mut c = cfg(None);
        c.version = RtspVersion::V2_0;
        let mut e = ClientEngine::new(c);
        e.submit(spec(RtspMethod::Options, URI), None, Instant::now())
            .unwrap();
        assert!(
            one(&mut e)
                .bytes
                .starts_with(b"OPTIONS rtsp://cam.example/live RTSP/2.0\r\n")
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a header the engine writes itself")]
    fn a_spec_carrying_an_engine_header_is_a_bug() {
        let mut bad = spec(RtspMethod::Describe, URI);
        bad.headers.push(("CSeq", "9".to_string()));
        let _ = ClientEngine::new(cfg(None)).submit(bad, None, Instant::now());
    }

    #[test]
    fn debug_shows_no_request_bytes_and_no_credentials() {
        let mut e = challenged(Some(("user", "s3cret-pw")), r#"Basic realm="cam""#);
        e.submit(spec(RtspMethod::Describe, URI), None, Instant::now())
            .unwrap();
        // The queued request is authorised.
        let basic = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode("user:s3cret-pw")
        };
        let t = e.poll_transmit().unwrap();
        assert!(
            String::from_utf8_lossy(&t.bytes).contains(&basic),
            "the fixture must hold the Basic header"
        );
        e.transmits.push_front(t);
        // A derived `Debug` would print the bytes as a list of numbers.
        let as_numbers = format!("{:?}", basic.as_bytes());
        let as_numbers = as_numbers.trim_matches(['[', ']']);
        for text in [
            format!("{e:?}"),
            format!("{e:#?}"),
            format!("{:?}", e.transmits),
        ] {
            assert!(!text.contains("s3cret-pw"), "{text}");
            assert!(!text.contains(&basic), "{text}");
            assert!(!text.contains(as_numbers), "{text}");
        }
    }

    #[test]
    fn an_injected_header_is_refused_and_nothing_is_queued() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let mut bad = spec(RtspMethod::Describe, URI);
        bad.headers
            .push(("x-custom", "a\r\nInjected: 1".to_string()));
        assert!(matches!(
            e.submit(bad, None, t0),
            Err(RtspError::InvalidHeader { .. })
        ));
        assert!(e.poll_transmit().is_none());
        assert!(e.poll_event().is_none());
        assert!(matches!(
            e.submit(
                spec(RtspMethod::Setup, "rtsp://cam.example/live\r\nX: y"),
                None,
                t0
            ),
            Err(RtspError::InvalidHeader { .. })
        ));
        assert!(e.poll_transmit().is_none());
    }

    #[test]
    fn a_new_submit_forgets_an_unanswered_transaction() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let b = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        assert_eq!(transmits(&mut e).len(), 2);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
        assert!(events(&mut e).is_empty(), "A was forgotten");
        assert_eq!(e.counters().unmatched_replies, 1);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n"), t0);
        assert_eq!(reply(&mut e, b).unwrap().status, 200);
        assert_ne!(a, b);
    }

    // --- authentication ---

    /// Port of `options_authenticates_on_challenge`.
    #[test]
    fn a_401_is_retried_once_with_credentials_under_the_same_txn() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let first = one(&mut e);
        assert_eq!(authz(&first), None);
        e.handle_reply(resp(BASIC_401), t0);
        assert!(events(&mut e).is_empty());
        let retry = one(&mut e);
        assert_eq!(retry.txn, None, "the retry reports no second write");
        assert!(!retry.is_ping);
        assert_eq!(cseq(&retry), 2);
        assert!(
            authz(&retry)
                .unwrap()
                .to_ascii_lowercase()
                .starts_with("basic "),
            "{retry:?}"
        );
        e.handle_reply(
            resp(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY\r\n\r\n"),
            t0,
        );
        let ok = reply(&mut e, a).unwrap();
        assert!(ok.headers["public"].contains("DESCRIBE"));
    }

    #[test]
    fn a_401_without_www_authenticate_is_bad_response() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\n\r\n"), t0);
        assert!(matches!(
            reply(&mut e, a),
            Err(RtspError::BadResponse {
                detail: "401 without WWW-Authenticate header"
            })
        ));
        assert!(e.poll_transmit().is_none());
    }

    #[test]
    fn a_401_without_credentials_is_auth_failed() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(BASIC_401), t0);
        assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
        assert!(e.poll_transmit().is_none());
    }

    #[test]
    fn a_second_401_is_auth_failed() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(BASIC_401), t0);
        let retry = one(&mut e);
        e.handle_reply(
            resp(
                format!(
                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n",
                    cseq(&retry)
                )
                .as_bytes(),
            ),
            t0,
        );
        assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
        assert!(e.poll_transmit().is_none(), "no third attempt");
    }

    #[test]
    fn a_scheme_refused_by_the_policy_is_auth_unsupported() {
        let t0 = Instant::now();
        let mut c = cfg(Some(("user", "pw")));
        c.schemes = AuthSchemes::DigestOnly;
        let mut e = ClientEngine::new(c);
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(BASIC_401), t0);
        assert!(matches!(
            reply(&mut e, a),
            Err(RtspError::AuthUnsupported { .. })
        ));
        assert!(e.poll_transmit().is_none());
        // The challenge stays cached, so the next request's pre-emptive
        // header fails the same way and nothing is queued.
        assert!(matches!(
            e.submit(spec(RtspMethod::Describe, URI), None, t0),
            Err(RtspError::AuthUnsupported { .. })
        ));
        assert!(e.poll_transmit().is_none());
    }

    #[test]
    fn teardown_is_retried_after_a_401_too() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        let mut td = spec(RtspMethod::Teardown, URI);
        td.headers.push(("session", "abc".to_string()));
        let a = e.submit(td, None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(BASIC_401), t0);
        let retry = one(&mut e);
        let r = sent(&retry);
        assert_eq!(r.method, RtspMethod::Teardown);
        assert_eq!(r.headers["session"], "abc");
        assert!(r.headers.contains_key("authorization"));
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&retry)).as_bytes()),
            t0,
        );
        let ev = events(&mut e);
        assert!(
            matches!(
                ev.as_slice(),
                [Event::Ended(EndReason::CleanTeardown), Event::Reply { txn, result: Ok(_) }] if *txn == a
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn a_teardown_whose_preemptive_header_fails_goes_out_without_one() {
        let t0 = Instant::now();
        let mut c = cfg(Some(("user", "pw")));
        c.schemes = AuthSchemes::DigestOnly;
        let mut e = ClientEngine::new(c);
        e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(BASIC_401), t0);
        events(&mut e);
        e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
        assert_eq!(authz(&one(&mut e)), None);
    }

    /// Port of `preemptive_authorization_builds_setup_digest_for_control_uri`.
    #[test]
    fn a_cached_challenge_signs_setup_against_its_control_uri() {
        let mut e = challenged(
            Some(("user", "pass")),
            r#"Digest realm="tstrans", nonce="abc123""#,
        );
        let control = "rtsp://cam.example/live/trackID=0";
        e.submit(spec(RtspMethod::Setup, control), None, Instant::now())
            .unwrap();
        let t = one(&mut e);
        let hdr = authz(&t).expect("cached challenge + credentials → pre-emptive header");
        let ha1 = md5_hex("user:tstrans:pass");
        let ha2 = md5_hex(&format!("SETUP:{control}"));
        let expected = md5_hex(&format!("{ha1}:abc123:{ha2}"));
        assert!(hdr.contains(&format!("response=\"{expected}\"")), "{hdr}");
        assert!(hdr.contains(&format!("uri=\"{control}\"")), "{hdr}");
        assert!(hdr.contains(r#"username="user""#), "{hdr}");
    }

    /// Port of `preemptive_authorization_inert_without_challenge_or_creds`.
    #[test]
    fn no_preemptive_header_without_a_challenge_or_credentials() {
        let t0 = Instant::now();
        // Credentials, no challenge yet.
        let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
        e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
        assert_eq!(authz(&one(&mut e)), None);
        // A challenge, no credentials.
        let mut e = challenged(None, r#"Digest realm="R", nonce="n""#);
        e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
        assert_eq!(authz(&one(&mut e)), None);
    }

    /// Port of `qop_auth_nonce_count_increments_and_resets_on_new_challenge`.
    #[test]
    fn the_nonce_count_counts_per_header_and_restarts_on_a_new_challenge() {
        let t0 = Instant::now();
        let www1 = r#"Digest realm="R", nonce="n1", qop="auth""#;
        let mut e = challenged(Some(("user", "pass")), www1);
        // The challenge's own retry used nc=1.
        let mut ncs = Vec::new();
        for m in [RtspMethod::Describe, RtspMethod::Setup, RtspMethod::Play] {
            e.submit(spec(m, URI), None, t0).unwrap();
            let a = authz(&one(&mut e)).unwrap();
            assert!(a.contains("qop=auth"), "{a}");
            assert_eq!(param(&a, "nonce"), "n1");
            ncs.push(param(&a, "nc").to_string());
        }
        assert_eq!(ncs, ["00000002", "00000003", "00000004"]);

        // A different challenge (new nonce) restarts the count: its retry is nc=1.
        e.submit(spec(RtspMethod::Setup, URI), None, t0).unwrap();
        let c = cseq(&one(&mut e));
        e.handle_reply(
            resp(
                format!(
                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n2\", qop=\"auth\"\r\n\r\n"
                )
                .as_bytes(),
            ),
            t0,
        );
        let retry = one(&mut e);
        let a = authz(&retry).unwrap();
        assert_eq!((param(&a, "nonce"), param(&a, "nc")), ("n2", "00000001"));

        // The same challenge again does not restart it.
        e.handle_reply(
            resp(
                format!(
                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n2\", qop=\"auth\"\r\n\r\n",
                    cseq(&retry)
                )
                .as_bytes(),
            ),
            t0,
        );
        events(&mut e);
        e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
        let a = authz(&one(&mut e)).unwrap();
        assert_eq!((param(&a, "nonce"), param(&a, "nc")), ("n2", "00000002"));
    }

    // --- session and keepalive timing ---

    fn setup_200(cseq: u32, session: &str) -> RtspResponse {
        resp(
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nSession: {session}\r\n\r\n"
            )
            .as_bytes(),
        )
    }

    /// Runs a SETUP answered with `Session: <session>` at `at`.
    fn run_setup(e: &mut ClientEngine, session: &str, at: Instant) {
        let a = e.submit(spec(RtspMethod::Setup, URI), None, at).unwrap();
        let c = cseq(&one(e));
        e.handle_reply(setup_200(c, session), at);
        assert_eq!(reply(e, a).unwrap().status, 200);
    }

    #[test]
    fn the_first_ping_is_due_one_interval_after_enable_and_the_first_call_wins() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        assert_eq!(e.poll_timeout(), None);
        e.enable_keepalive(Some(Duration::from_secs(2)), t0);
        e.enable_keepalive(Some(Duration::from_secs(5)), ms(t0, 10));
        assert_eq!(e.poll_timeout(), Some(ms(t0, 2000)));
        e.handle_timeout(ms(t0, 1999), false);
        assert!(e.poll_transmit().is_none());
        e.handle_timeout(ms(t0, 2000), false);
        let p = one(&mut e);
        assert!(p.is_ping);
        assert_eq!(p.txn, None);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 4000)));
    }

    #[test]
    fn without_an_override_the_interval_is_half_the_default_session_timeout() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(None, t0);
        assert_eq!(e.poll_timeout(), Some(t0 + Duration::from_secs(30)));
    }

    #[test]
    fn a_ping_carries_its_cseq_user_agent_session_and_a_preemptive_header() {
        let t0 = Instant::now();
        let mut e = challenged(
            Some(("user", "pass")),
            r#"Digest realm="R", nonce="n1", qop="auth""#,
        );
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.handle_timeout(ms(t0, 1000), false);
        let p = one(&mut e);
        assert!(
            p.bytes
                .starts_with(b"OPTIONS rtsp://cam.example/live RTSP/1.0\r\n")
        );
        let r = sent(&p);
        assert_eq!(r.headers["cseq"], "1000001");
        assert_eq!(r.headers["user-agent"], "tst-rtp/test");
        assert!(!r.headers.contains_key("session"), "no session yet");
        let a = &r.headers["authorization"];
        assert!(a.contains(&format!("uri=\"{URI}\"")), "{a}");
        assert_eq!(param(a, "nc"), "00000002");

        run_setup(&mut e, "abc;timeout=60", ms(t0, 1500));
        // The override holds the 1 s cadence; later pings carry the session.
        e.handle_timeout(ms(t0, 2000), false);
        let r = sent(&one(&mut e));
        assert_eq!(r.headers["cseq"], "1000002");
        assert_eq!(r.headers["session"], "abc");
    }

    /// Port of `keepalive_retunes_to_server_advertised_timeout`.
    #[test]
    fn a_setup_200_records_the_session_and_retunes_the_interval() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(None, t0);
        run_setup(&mut e, "abc;timeout=1", ms(t0, 100));
        assert_eq!(
            e.poll_timeout(),
            Some(ms(t0, 500)),
            "half of 1 s from enable"
        );
        e.handle_timeout(ms(t0, 500), false);
        let r = sent(&one(&mut e));
        assert_eq!(r.headers["session"], "abc");
        assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
    }

    #[test]
    fn a_retune_past_the_due_time_pings_at_once() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(None, t0);
        run_setup(&mut e, "abc;timeout=1", ms(t0, 900));
        assert_eq!(e.poll_timeout(), Some(ms(t0, 500)));
        e.handle_timeout(ms(t0, 900), false);
        assert!(one(&mut e).is_ping);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 1400)));
    }

    #[test]
    fn a_timeout_of_zero_floors_the_interval_at_100_ms() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(None, t0);
        run_setup(&mut e, "abc;timeout=0", t0);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 100)));
    }

    #[test]
    fn a_timeout_of_zero_learned_before_enable_floors_at_100_ms_too() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        run_setup(&mut e, "abc;timeout=0", t0);
        e.enable_keepalive(None, t0);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 100)));
    }

    #[test]
    fn an_interval_override_is_floored_at_1_ms_only() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        run_setup(&mut e, "abc;timeout=0", t0);
        e.enable_keepalive(Some(Duration::ZERO), t0);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 1)));
    }

    #[test]
    fn a_timeout_near_u64_max_saturates_without_panicking() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(None, t0);
        run_setup(&mut e, &format!("abc;timeout={}", u64::MAX), t0);
        // The due time is centuries away, or not representable at all.
        let far = t0 + Duration::from_secs(100 * 365 * 24 * 3600);
        assert!(e.poll_timeout().is_none_or(|d| d > far));
        e.handle_timeout(t0 + Duration::from_secs(3600), false);
        assert!(e.poll_transmit().is_none());
    }

    /// Port of `builder_interval_override_wins_over_server_timeout`.
    #[test]
    fn an_interval_override_wins_over_the_server_timeout() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(10)), t0);
        run_setup(&mut e, "abc;timeout=1", t0);
        assert_eq!(e.poll_timeout(), Some(t0 + Duration::from_secs(10)));
        e.handle_timeout(ms(t0, 2000), false);
        assert!(e.poll_transmit().is_none());
    }

    #[test]
    fn a_setup_learned_before_enable_sets_the_first_interval() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        run_setup(&mut e, "abc;timeout=4", t0);
        e.enable_keepalive(None, ms(t0, 100));
        assert_eq!(e.poll_timeout(), Some(ms(t0, 2100)));
    }

    #[test]
    fn submitting_a_teardown_clears_the_session_and_ends_the_pings() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        run_setup(&mut e, "abc", t0);
        let mut td = spec(RtspMethod::Teardown, URI);
        td.headers.push(("session", "abc".to_string()));
        let a = e.submit(td, None, ms(t0, 10)).unwrap();
        one(&mut e);
        assert_eq!(e.poll_timeout(), None, "no ping due after a TEARDOWN");
        e.handle_timeout(t0 + Duration::from_secs(10), false);
        assert!(e.poll_transmit().is_none());
        // Whatever the reply.
        e.handle_reply(
            resp(b"RTSP/1.0 500 Internal Server Error\r\nCSeq: 2\r\n\r\n"),
            t0,
        );
        assert_eq!(reply(&mut e, a).unwrap().status, 500);
        e.handle_timeout(t0 + Duration::from_secs(20), false);
        assert!(e.poll_transmit().is_none());
    }

    #[test]
    fn the_cancel_turns_the_keepalive_off_for_good() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
        e.handle_timeout(ms(t0, 1000), true);
        assert!(e.poll_transmit().is_none());
        assert_eq!(e.poll_timeout(), None);
        e.handle_timeout(ms(t0, 5000), false);
        assert!(e.poll_transmit().is_none());
        e.enable_keepalive(Some(Duration::from_secs(1)), ms(t0, 5000));
        assert_eq!(e.poll_timeout(), None);
    }

    #[test]
    fn a_ping_that_fails_to_encode_ends_the_session() {
        let t0 = Instant::now();
        let mut c = cfg(None);
        c.user_agent = "bad\r\nX: y".to_string();
        let mut e = ClientEngine::new(c);
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.handle_timeout(ms(t0, 1000), false);
        assert!(e.poll_transmit().is_none());
        let ev = events(&mut e);
        match ev.as_slice() {
            [
                Event::SessionDead,
                Event::Ended(EndReason::KeepaliveFailed { msg }),
            ] => {
                assert!(
                    msg.starts_with("keepalive ping failed to encode: "),
                    "{msg}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(e.poll_timeout(), None);
    }

    fn pinged(creds: Option<(&str, &str)>) -> (ClientEngine, Instant, u32) {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(creds));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.handle_timeout(ms(t0, 1000), false);
        let c = cseq(&one(&mut e));
        (e, t0, c)
    }

    #[test]
    fn a_ping_200_emits_nothing() {
        let (mut e, t0, c) = pinged(None);
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            t0,
        );
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().keepalive_responses, 1);
        assert_eq!(e.counters().unmatched_replies, 0);
    }

    #[test]
    fn a_ping_401_refreshes_the_challenge() {
        let (mut e, t0, c) = pinged(Some(("user", "pass")));
        e.handle_reply(
            resp(
                format!(
                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"fresh\"\r\n\r\n"
                )
                .as_bytes(),
            ),
            t0,
        );
        assert!(events(&mut e).is_empty());
        assert!(e.poll_transmit().is_none(), "a ping is never retried");
        e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
        let a = authz(&one(&mut e)).expect("the ping's challenge signs the next request");
        assert_eq!(param(&a, "nonce"), "fresh");
    }

    #[test]
    fn a_ping_454_is_session_expired() {
        let (mut e, t0, c) = pinged(None);
        e.handle_reply(
            resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            t0,
        );
        let ev = events(&mut e);
        assert!(
            matches!(
                ev.as_slice(),
                [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn only_the_latest_unanswered_pings_are_remembered() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        let n = MAX_PINGS_AWAITING_REPLY as u64 + 1;
        for i in 1..=n {
            e.handle_timeout(ms(t0, 1000 * i), false);
        }
        assert_eq!(transmits(&mut e).len(), n as usize);
        // The oldest was forgotten; the next one is still known.
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1000001\r\n\r\n"), t0);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1000002\r\n\r\n"), t0);
        assert_eq!(e.counters().unmatched_replies, 1);
        assert_eq!(e.counters().keepalive_responses, 1);
    }

    // --- deadlines ---

    #[test]
    fn a_transaction_past_its_deadline_times_out_once() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e
            .submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 200)), t0)
            .unwrap();
        one(&mut e);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 200)));
        e.handle_timeout(ms(t0, 199), false);
        assert!(events(&mut e).is_empty());
        e.handle_timeout(ms(t0, 200), false);
        assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
        assert_eq!(e.poll_timeout(), None);
        e.handle_timeout(ms(t0, 300), false);
        assert!(events(&mut e).is_empty());
        // Its late reply matches nothing live.
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), ms(t0, 400));
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 1);
    }

    #[test]
    fn a_reply_at_or_after_the_deadline_is_a_timeout() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e
            .submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 200)), t0)
            .unwrap();
        one(&mut e);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), ms(t0, 200));
        assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
        e.handle_timeout(ms(t0, 300), false);
        assert!(events(&mut e).is_empty());
    }

    #[test]
    fn the_retry_after_a_401_keeps_the_original_deadline() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        let a = e
            .submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 1000)), t0)
            .unwrap();
        one(&mut e);
        e.handle_reply(resp(BASIC_401), ms(t0, 600));
        one(&mut e);
        assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
        e.handle_timeout(ms(t0, 1000), false);
        assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
    }

    #[test]
    fn poll_timeout_is_the_earlier_of_the_ping_and_the_deadline() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 300)), t0)
            .unwrap();
        assert_eq!(e.poll_timeout(), Some(ms(t0, 300)));
        e.submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 3000)), t0)
            .unwrap();
        assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
    }

    // --- unmatched replies ---

    #[test]
    fn a_reply_matching_nothing_live_is_counted_and_dropped() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n"), t0);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1000001\r\n\r\n"), t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 2);
        assert_eq!(e.counters().keepalive_responses, 0);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
        // A second reply to the answered transaction is unmatched too.
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 3);
    }

    // --- the end of the stream ---

    fn closed_with(end: StreamEnd) -> (Vec<Event>, ClientEngine, TxnId) {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        let a = e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
        e.handle_closed(end);
        let ev = events(&mut e);
        (ev, e, a)
    }

    fn assert_closed(
        end: StreamEnd,
        kind: io::ErrorKind,
        dead: bool,
        reason: impl Fn(&EndReason) -> bool,
    ) {
        let (ev, mut e, a) = closed_with(end);
        let (head, rest) = if dead {
            assert!(matches!(ev.first(), Some(Event::SessionDead)), "{ev:?}");
            (&ev[1..2], &ev[2..])
        } else {
            (&ev[0..1], &ev[1..])
        };
        match head {
            [Event::Ended(r)] => assert!(reason(r), "{r:?}"),
            other => panic!("{other:?} in {ev:?}"),
        }
        match rest {
            [
                Event::Reply {
                    txn,
                    result: Err(RtspError::Io(k)),
                },
            ] => {
                assert_eq!(*txn, a);
                assert_eq!(*k, kind);
            }
            other => panic!("{other:?} in {ev:?}"),
        }
        assert!(e.poll_transmit().is_none(), "queued bytes are dropped");
        assert_eq!(e.poll_timeout(), None);
        // C7: a later request is answered at once with the same kind.
        assert!(matches!(
            e.submit(spec(RtspMethod::Teardown, URI), None, Instant::now()),
            Err(RtspError::Io(k)) if k == kind
        ));
        assert!(e.poll_transmit().is_none());
        // A second end changes nothing.
        e.handle_closed(StreamEnd::Stopped);
        assert!(e.poll_event().is_none());
    }

    #[test]
    fn a_peer_close_fails_the_request_and_ends_clean() {
        assert_closed(
            StreamEnd::PeerClosed,
            io::ErrorKind::UnexpectedEof,
            true,
            |r| matches!(r, EndReason::CleanTeardown),
        );
    }

    #[test]
    fn a_read_failure_ends_with_transport_failed() {
        assert_closed(
            StreamEnd::ReadFailed(io::Error::from(io::ErrorKind::ConnectionReset)),
            io::ErrorKind::ConnectionReset,
            true,
            |r| matches!(r, EndReason::TransportFailed { msg } if msg.starts_with("TCP read failed: ")),
        );
    }

    #[test]
    fn a_write_failure_ends_with_transport_failed() {
        assert_closed(
            StreamEnd::WriteFailed(io::Error::from(io::ErrorKind::BrokenPipe)),
            io::ErrorKind::BrokenPipe,
            true,
            |r| matches!(r, EndReason::TransportFailed { msg } if msg.starts_with("TCP write failed: ")),
        );
    }

    #[test]
    fn a_stalled_write_ends_by_what_was_stalled() {
        const MSG: &str = "control TCP write made no progress for the write timeout";
        assert_closed(
            StreamEnd::WriteStalled { ping: true },
            io::ErrorKind::TimedOut,
            true,
            |r| matches!(r, EndReason::KeepaliveFailed { msg } if msg == MSG),
        );
        assert_closed(
            StreamEnd::WriteStalled { ping: false },
            io::ErrorKind::TimedOut,
            true,
            |r| matches!(r, EndReason::TransportFailed { msg } if msg == MSG),
        );
    }

    #[test]
    fn a_stop_ends_as_cancelled_without_session_dead() {
        assert_closed(
            StreamEnd::Stopped,
            io::ErrorKind::UnexpectedEof,
            false,
            |r| matches!(r, EndReason::Cancelled),
        );
    }

    #[test]
    fn replies_and_timeouts_after_the_end_do_nothing() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.submit(spec(RtspMethod::Play, URI), Some(ms(t0, 500)), t0)
            .unwrap();
        e.handle_closed(StreamEnd::PeerClosed);
        events(&mut e);
        transmits(&mut e);
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
        e.handle_reply(
            resp(b"RTSP/1.0 454 Session Not Found\r\nCSeq: 1000001\r\n\r\n"),
            t0,
        );
        e.handle_timeout(t0 + Duration::from_secs(10), false);
        e.handle_timeout(t0 + Duration::from_secs(20), true);
        assert!(e.poll_event().is_none());
        assert!(e.poll_transmit().is_none());
        assert_eq!(e.counters().unmatched_replies, 0);
        assert_eq!(e.counters().keepalive_responses, 0);
    }

    // --- an accepted TEARDOWN ---

    fn teardown_answered(status_line: &str) -> (Vec<Event>, ClientEngine) {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(
            resp(format!("{status_line}\r\nCSeq: 1\r\n\r\n").as_bytes()),
            t0,
        );
        (events(&mut e), e)
    }

    #[test]
    fn an_accepted_teardown_ends_clean_and_the_engine_stays_open() {
        for status in [
            "RTSP/1.0 200 OK",
            "RTSP/1.0 204 No Content",
            "RTSP/1.0 454 Session Not Found",
        ] {
            let (ev, mut e) = teardown_answered(status);
            assert!(
                matches!(
                    ev.as_slice(),
                    [
                        Event::Ended(EndReason::CleanTeardown),
                        Event::Reply { result: Ok(_), .. }
                    ]
                ),
                "{status}: {ev:?}"
            );
            e.submit(spec(RtspMethod::Options, URI), None, Instant::now())
                .unwrap();
            assert_eq!(cseq(&one(&mut e)), 2);
        }
    }

    #[test]
    fn a_refused_teardown_does_not_end() {
        for status in [
            "RTSP/1.0 404 Not Found",
            "RTSP/1.0 500 Internal Server Error",
        ] {
            let (ev, _) = teardown_answered(status);
            assert!(
                matches!(ev.as_slice(), [Event::Reply { result: Ok(_), .. }]),
                "{status}: {ev:?}"
            );
        }
    }

    #[test]
    fn a_non_teardown_454_does_not_end() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(
            resp(b"RTSP/1.0 454 Session Not Found\r\nCSeq: 1\r\n\r\n"),
            t0,
        );
        assert_eq!(reply(&mut e, a).unwrap().status, 454);
    }

    // --- a request racing a keepalive ping ---

    #[test]
    fn a_ping_reply_before_the_request_reply_is_not_the_request_reply() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        let a = e
            .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
            .unwrap();
        let req = cseq(&one(&mut e));
        e.handle_timeout(ms(t0, 1000), false);
        let ping = cseq(&one(&mut e));
        assert_ne!(req, ping);
        // The ping's reply first: consumed, the request still waits.
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {ping}\r\n\r\n").as_bytes()),
            ms(t0, 1010),
        );
        assert!(events(&mut e).is_empty());
        e.handle_reply(
            resp(
                format!("RTSP/1.0 200 OK\r\nCSeq: {req}\r\nContent-Length: 3\r\n\r\nv=0")
                    .as_bytes(),
            ),
            ms(t0, 1020),
        );
        let r = reply(&mut e, a).unwrap();
        assert_eq!(&r.body[..], b"v=0");
        // And a ping answered after the request's reply is still a ping reply.
        e.handle_timeout(ms(t0, 2000), false);
        let ping2 = cseq(&one(&mut e));
        let b = e
            .submit(spec(RtspMethod::Play, URI), None, ms(t0, 2001))
            .unwrap();
        let req2 = cseq(&one(&mut e));
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {req2}\r\n\r\n").as_bytes()),
            ms(t0, 2002),
        );
        assert_eq!(reply(&mut e, b).unwrap().status, 200);
        e.handle_reply(
            resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {ping2}\r\n\r\n").as_bytes()),
            ms(t0, 2003),
        );
        assert!(matches!(
            events(&mut e).as_slice(),
            [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
        ));
        assert_eq!(e.counters().keepalive_responses, 2);
        assert_eq!(e.counters().unmatched_replies, 0);
    }

    #[test]
    fn a_ping_reply_between_a_401_and_the_retry_reply_leaves_the_retry_its_own() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        let a = e
            .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
            .unwrap();
        let req = cseq(&one(&mut e));
        e.handle_timeout(ms(t0, 1000), false);
        let ping = cseq(&one(&mut e));
        e.handle_reply(
            resp(
                String::from_utf8_lossy(BASIC_401)
                    .replace("CSeq: 1", &format!("CSeq: {req}"))
                    .as_bytes(),
            ),
            ms(t0, 1010),
        );
        let retry = cseq(&one(&mut e));
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {ping}\r\n\r\n").as_bytes()),
            ms(t0, 1020),
        );
        assert!(
            events(&mut e).is_empty(),
            "the ping's reply is not the retry's"
        );
        e.handle_reply(
            resp(
                format!("RTSP/1.0 200 OK\r\nCSeq: {retry}\r\nContent-Length: 3\r\n\r\nv=0")
                    .as_bytes(),
            ),
            ms(t0, 1030),
        );
        assert_eq!(&reply(&mut e, a).unwrap().body[..], b"v=0");
        assert_eq!(e.counters().keepalive_responses, 1);
        assert_eq!(e.counters().unmatched_replies, 0);
    }

    #[test]
    fn a_ping_401_between_request_and_reply_rotates_the_nonce_once() {
        let t0 = Instant::now();
        let mut e = challenged(
            Some(("user", "pass")),
            r#"Digest realm="R", nonce="n1", qop="auth""#,
        );
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        let a = e
            .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
            .unwrap();
        let req = one(&mut e);
        assert_eq!(param(&authz(&req).unwrap(), "nonce"), "n1");
        e.handle_timeout(ms(t0, 1000), false);
        let ping = one(&mut e);
        assert_eq!(param(&authz(&ping).unwrap(), "nonce"), "n1");
        const N2_401: &str = "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n2\", qop=\"auth\"\r\n\r\n";
        // The server rotates the nonce: the ping's 401 arrives first...
        e.handle_reply(
            resp(N2_401.replace("{}", &cseq(&ping).to_string()).as_bytes()),
            ms(t0, 1010),
        );
        assert!(events(&mut e).is_empty());
        // ...then the request's, with the same new challenge.
        e.handle_reply(
            resp(N2_401.replace("{}", &cseq(&req).to_string()).as_bytes()),
            ms(t0, 1020),
        );
        let retry = one(&mut e);
        let ra = authz(&retry).unwrap();
        assert_eq!((param(&ra, "nonce"), param(&ra, "nc")), ("n2", "00000001"));
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&retry)).as_bytes()),
            ms(t0, 1030),
        );
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
        // The count went on from the retry: the rotation reset it once.
        e.handle_timeout(ms(t0, 2000), false);
        let pa = authz(&one(&mut e)).unwrap();
        assert_eq!((param(&pa, "nonce"), param(&pa, "nc")), ("n2", "00000002"));
    }

    #[test]
    fn no_nonce_count_is_used_twice() {
        let t0 = Instant::now();
        let mut e = challenged(
            Some(("user", "pass")),
            r#"Digest realm="R", nonce="n1", qop="auth""#,
        );
        e.enable_keepalive(Some(Duration::from_millis(100)), t0);
        let mut seen = std::collections::HashSet::new();
        seen.insert(("n1".to_string(), "00000001".to_string()));
        for round in 0..40u64 {
            let now = ms(t0, 100 * (round + 1));
            // A request and a ping in flight together.
            e.submit(spec(RtspMethod::GetParameter, URI), None, now)
                .unwrap();
            e.handle_timeout(now, false);
            let out = transmits(&mut e);
            assert_eq!(out.len(), 2);
            let (req, ping) = if out[0].is_ping {
                (&out[1], &out[0])
            } else {
                (&out[0], &out[1])
            };
            // Every fifth round the server rotates the nonce on the ping
            // AND challenges the request, in either order.
            if round % 5 == 4 {
                let nonce = format!("n{}", round + 2);
                let challenge = |c: u32| {
                    resp(
                        format!(
                            "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"{nonce}\", qop=\"auth\"\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                };
                if round % 2 == 0 {
                    e.handle_reply(challenge(cseq(ping)), now);
                    e.handle_reply(challenge(cseq(req)), now);
                } else {
                    e.handle_reply(challenge(cseq(req)), now);
                    e.handle_reply(challenge(cseq(ping)), now);
                }
            }
            for t in out.iter().chain(transmits(&mut e).iter()) {
                if let Some(a) = authz(t) {
                    let key = (param(&a, "nonce").to_string(), param(&a, "nc").to_string());
                    assert!(seen.insert(key.clone()), "{key:?} used twice");
                }
            }
            events(&mut e);
        }
        assert!(seen.len() > 80, "{}", seen.len());
    }

    // --- the inbound half: framing, media, RTCP, caps ---

    use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T, RtpHeader};
    use crate::rtsp::message::{MAX_RTSP_BODY_BYTES, MAX_RTSP_MESSAGE_BYTES};

    const CHANNELS: InterleavedChannels = InterleavedChannels { rtp: 0, rtcp: 1 };

    /// An engine fed from an interleaved stream on channels 0/1.
    fn interleaved() -> ClientEngine {
        let mut e = ClientEngine::new(cfg(None));
        e.set_interleaved(CHANNELS);
        e
    }

    /// A minimal valid fixed RTP header (V=2, PT=33).
    fn valid_rtp_header() -> [u8; RTP_HEADER_LEN] {
        let mut h = [0u8; RTP_HEADER_LEN];
        RtpHeader::new(0, 0, 0).encode_into(&mut h);
        h
    }

    /// `body` in one interleaved frame on `channel`.
    fn interleaved_frame(channel: u8, body: &[u8]) -> Vec<u8> {
        let mut f = vec![b'$', channel];
        f.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    fn media(ev: &[Event]) -> Vec<&[u8]> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Media(b) => Some(&b[..]),
                _ => None,
            })
            .collect()
    }

    /// The events of a stream closed by a framing violation: `SessionDead`,
    /// then `ProtocolError`, then (when one was pending) the request's
    /// `BadResponse`. Returns the end message and the reply detail.
    fn protocol_end(ev: &[Event]) -> (String, Option<&'static str>) {
        match ev {
            [
                Event::SessionDead,
                Event::Ended(EndReason::ProtocolError { msg }),
            ] => (msg.clone(), None),
            [
                Event::SessionDead,
                Event::Ended(EndReason::ProtocolError { msg }),
                Event::Reply {
                    result: Err(RtspError::BadResponse { detail }),
                    ..
                },
            ] => (msg.clone(), Some(*detail)),
            other => panic!("expected a protocol end, got {other:?}"),
        }
    }

    /// The engine is closed: input is ignored and a request is refused.
    fn assert_closed_by_protocol_error(e: &mut ClientEngine, t0: Instant) {
        assert!(e.inbuf.is_empty(), "nothing is kept once closed");
        e.handle_input(&interleaved_frame(0, &valid_rtp_header()), t0);
        assert!(e.poll_event().is_none(), "input after the end is ignored");
        assert!(e.inbuf.is_empty());
        assert!(matches!(
            e.submit(spec(RtspMethod::Options, URI), None, t0),
            Err(RtspError::Io(io::ErrorKind::InvalidData))
        ));
        assert!(e.poll_transmit().is_none());
        assert_eq!(e.poll_timeout(), None);
    }

    /// Port of `rtp_whole_packet_delivered`.
    #[test]
    fn rtp_whole_packet_delivered() {
        let t0 = Instant::now();
        let header = valid_rtp_header();
        let mut raw = vec![b'$', 0u8, 0x00, 20];
        raw.extend_from_slice(&header);
        raw.extend_from_slice(b"PAYLOAD!");
        let mut e = interleaved();
        e.handle_input(&raw, t0);
        let mut expected = header.to_vec();
        expected.extend_from_slice(b"PAYLOAD!");
        let ev = events(&mut e);
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(media(&ev), [expected.as_slice()]);
        assert_eq!(e.counters().rtp_frames, 1);
        assert_eq!(e.counters().malformed_frames, 0);
        assert!(e.inbuf.is_empty());
    }

    /// Port of `rtcp_frame_routed_to_rtcp_rx`.
    #[test]
    fn rtcp_frame_routed() {
        let mut raw = vec![b'$', 1u8, 0x00, 4];
        raw.extend_from_slice(b"\xDE\xAD\xBE\xEF");
        let mut e = interleaved();
        e.handle_input(&raw, Instant::now());
        let ev = events(&mut e);
        assert!(
            matches!(ev.as_slice(), [Event::Rtcp(p)] if p[..] == [0xDE, 0xAD, 0xBE, 0xEF]),
            "{ev:?}"
        );
        assert_eq!(e.counters().rtcp_frames, 1);
    }

    /// Port of `pump_accepts_full_u16_binary_frame`.
    #[test]
    fn engine_accepts_full_u16_binary_frame() {
        let mut raw = vec![b'$', 1u8, 0xFF, 0xFF];
        raw.extend(std::iter::repeat_n(0xABu8, 65535));
        let mut e = interleaved();
        e.handle_input(&raw, Instant::now());
        let ev = events(&mut e);
        match ev.as_slice() {
            [Event::Rtcp(p)] => {
                assert_eq!(p.len(), 65535, "full payload must round-trip intact");
                assert!(p.iter().all(|&b| b == 0xAB));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(e.counters().rtcp_frames, 1);
        assert_eq!(e.counters().malformed_frames, 0);
        assert!(e.poll_event().is_none());
    }

    /// Port of `rtsp_response_with_body_routed_to_ctrl_rx`: the body is
    /// awaited before the reply is complete.
    #[test]
    fn rtsp_response_with_body() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        transmits(&mut e);
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\nHELLO";
        e.handle_input(&raw[..raw.len() - 1], t0);
        assert!(e.poll_event().is_none(), "the body is not complete yet");
        e.handle_input(&raw[raw.len() - 1..], t0);
        assert_eq!(&reply(&mut e, a).unwrap().body[..], b"HELLO");
        assert_eq!(e.counters().rtsp_messages, 1);
        assert!(e.inbuf.is_empty());
    }

    /// Port of `keepalive_responses_consumed_not_queued`: more ping replies
    /// than the old control queue held are consumed, none reaches a caller.
    #[test]
    fn keepalive_responses_consumed_not_queued() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        let n = 40u64;
        for i in 1..=n {
            e.handle_timeout(ms(t0, 1000 * i), false);
            let c = cseq(&one(&mut e));
            e.handle_input(
                format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes(),
                ms(t0, 1000 * i + 1),
            );
        }
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().keepalive_responses, n);
        assert_eq!(e.counters().rtsp_messages, 0);
        assert_eq!(e.counters().malformed_frames, 0);
        assert_eq!(e.counters().unmatched_replies, 0);
    }

    /// Port of `keepalive_401_refreshes_shared_auth_challenge`.
    #[test]
    fn keepalive_401_refreshes_shared_auth_challenge() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.auth.challenge = Some(r#"Digest realm="cam", nonce="old""#.into());
        e.auth.nc = 41;
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.handle_timeout(ms(t0, 1000), false);
        let c = cseq(&one(&mut e));
        e.handle_input(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: \
                 Digest realm=\"cam\", nonce=\"rotated\", stale=true\r\n\r\n"
            )
            .as_bytes(),
            t0,
        );
        assert!(
            e.auth
                .challenge
                .as_deref()
                .is_some_and(|c| c.contains("rotated")),
            "the challenge must be refreshed from the ping's 401, got {:?}",
            e.auth.challenge
        );
        assert_eq!(e.auth.nc, 0, "nonce-count must reset for the new challenge");
        assert_eq!(e.counters().keepalive_responses, 1);
    }

    /// Port of `keepalive_454_flips_session_dead`.
    #[test]
    fn keepalive_454_flips_session_dead() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.handle_timeout(ms(t0, 1000), false);
        let c = cseq(&one(&mut e));
        e.handle_input(
            format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes(),
            t0,
        );
        let ev = events(&mut e);
        assert!(
            matches!(
                ev.as_slice(),
                [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
            ),
            "{ev:?}"
        );
    }

    /// Port of `rtsp_response_closes_pump_on_malformed_content_length`.
    #[test]
    fn rtsp_response_closes_engine_on_malformed_content_length() {
        for header in [
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: nope\r\n\r\n",
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 2000000000\r\n\r\n",
        ] {
            let t0 = Instant::now();
            let mut e = interleaved();
            e.handle_input(header.as_bytes(), t0);
            let (msg, detail) = protocol_end(&events(&mut e));
            assert!(
                msg.starts_with("malformed RTSP Content-Length: "),
                "{header:?}: {msg}"
            );
            assert_eq!(detail, None);
            assert_eq!(e.counters().malformed_frames, 1);
            assert_eq!(e.counters().rtsp_messages, 0);
            assert_closed_by_protocol_error(&mut e, t0);
        }
    }

    /// Port of `pump_closes_on_unterminated_header_flood`.
    #[test]
    fn engine_closes_on_unterminated_header_flood() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.handle_input(&vec![b'A'; 128 * 1024], t0);
        let (msg, detail) = protocol_end(&events(&mut e));
        assert!(
            msg.starts_with("control stream buffer exceeded RTSP header/body caps ("),
            "{msg}"
        );
        assert_eq!(detail, None);
        assert_eq!(e.counters().malformed_frames, 1);
        assert_eq!(e.counters().rtsp_messages, 0);
        assert_closed_by_protocol_error(&mut e, t0);
    }

    /// The same flood dribbled in: the cap holds at 64 KiB of headers.
    #[test]
    fn a_dribbled_header_flood_is_refused_at_the_cap() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let chunk = [b'A'; 1024];
        let mut fed = 0;
        while e.poll_event().is_none() {
            assert!(fed <= MAX_RTSP_MESSAGE_BYTES, "no end after {fed} bytes");
            e.handle_input(&chunk, t0);
            fed += chunk.len();
        }
        assert_eq!(fed, MAX_RTSP_MESSAGE_BYTES + chunk.len());
        assert!(e.inbuf.is_empty());
    }

    /// Port of `unknown_channel_frame_counted_and_dropped`.
    #[test]
    fn unknown_channel_frame_counted_and_dropped() {
        let mut e = interleaved();
        e.handle_input(&[b'$', 7u8, 0x00, 2, 0xAB, 0xCD], Instant::now());
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().malformed_frames, 1);
        assert_eq!(e.counters().rtp_frames, 0);
        assert_eq!(e.counters().rtcp_frames, 0);
        assert!(e.inbuf.is_empty());
    }

    #[test]
    fn a_binary_frame_before_set_interleaved_is_counted_and_dropped() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let mut raw = interleaved_frame(0, &valid_rtp_header());
        raw.extend(interleaved_frame(1, b"\xDE\xAD\xBE\xEF"));
        e.handle_input(&raw, t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().malformed_frames, 2);
        assert_eq!(e.counters().rtp_frames, 0);
        assert_eq!(e.counters().rtcp_frames, 0);
        // The stream goes on.
        e.set_interleaved(CHANNELS);
        e.handle_input(&interleaved_frame(0, &valid_rtp_header()), t0);
        assert_eq!(media(&events(&mut e)).len(), 1);
    }

    /// Port of `undersized_rtp_frame_counted_and_dropped`.
    #[test]
    fn undersized_rtp_frame_counted_and_dropped() {
        let mut e = interleaved();
        e.handle_input(
            &[b'$', 0u8, 0x00, 4, 0x00, 0x00, 0x00, 0x00],
            Instant::now(),
        );
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().rtp_frames, 1);
        assert_eq!(e.counters().malformed_frames, 1);
    }

    /// Port of `pump_skips_a_non_utf8_header_block_after_a_binary_frame`,
    /// with no request waiting.
    #[test]
    fn engine_skips_a_non_utf8_header_block_after_a_binary_frame() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        e.handle_timeout(ms(t0, 1000), false);
        let c = cseq(&one(&mut e));
        let mut raw = interleaved_frame(0, &valid_rtp_header());
        raw.extend_from_slice(b"RTSP/1.0 200 OK\r\nX: \xff\r\n\r\n");
        raw.extend_from_slice(
            format!(
                "RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\nContent-Length: 5\r\n\r\nHELLO"
            )
            .as_bytes(),
        );
        e.handle_input(&raw, t0);
        let ev = events(&mut e);
        assert!(
            matches!(
                ev.as_slice(),
                [
                    Event::Media(_),
                    Event::SessionDead,
                    Event::Ended(EndReason::SessionExpired)
                ]
            ),
            "the reply after the skipped block still arrives: {ev:?}"
        );
        assert_eq!(e.counters().malformed_frames, 1);
        assert_eq!(e.counters().keepalive_responses, 1);
        assert!(e.inbuf.is_empty());
    }

    /// Mirror of `send_and_read_rejects_non_utf8_headers_with_their_own_detail`:
    /// the waiting request gets the detail and the stream goes on.
    #[test]
    fn non_utf8_headers_answer_a_waiting_request_and_the_stream_goes_on() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        transmits(&mut e);
        let mut raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nX: \xff\r\n\r\n".to_vec();
        raw.extend(interleaved_frame(0, &valid_rtp_header()));
        e.handle_input(&raw, t0);
        let ev = events(&mut e);
        match ev.as_slice() {
            [
                Event::Reply {
                    txn,
                    result:
                        Err(RtspError::BadResponse {
                            detail: "non-UTF8 RTSP headers",
                        }),
                },
                Event::Media(_),
            ] => assert_eq!(*txn, a),
            other => panic!("{other:?}"),
        }
        assert_eq!(e.counters().malformed_frames, 1);
        let b = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n", t0);
        assert_eq!(reply(&mut e, b).unwrap().status, 200);
    }

    /// Mirror of `send_and_read_rejects_unterminated_headers`, whole and
    /// in read-sized slices.
    #[test]
    fn unterminated_headers_answer_a_waiting_request_and_close() {
        let mut raw = b"RTSP/1.0 200 OK\r\nX-Junk: ".to_vec();
        raw.extend(std::iter::repeat_n(b'A', 128 * 1024));
        for slice in [raw.len(), 4096] {
            let t0 = Instant::now();
            let mut e = interleaved();
            let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
            for c in raw.chunks(slice) {
                e.handle_input(c, t0);
            }
            let ev = events(&mut e);
            let (msg, detail) = protocol_end(&ev);
            assert!(msg.starts_with("control stream buffer exceeded"), "{msg}");
            assert_eq!(detail, Some("RTSP response headers exceed maximum"));
            assert!(matches!(ev.last(), Some(Event::Reply { txn, .. }) if *txn == a));
            assert_closed_by_protocol_error(&mut e, t0);
        }
    }

    /// Mirror of `send_and_read_rejects_oversized_declared_body`.
    #[test]
    fn an_oversized_declared_body_answers_a_waiting_request_and_closes() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        e.handle_input(
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
                MAX_RTSP_BODY_BYTES + 1
            )
            .as_bytes(),
            t0,
        );
        let (msg, detail) = protocol_end(&events(&mut e));
        assert_eq!(
            msg,
            "malformed RTSP Content-Length: Content-Length exceeds maximum"
        );
        assert_eq!(detail, Some("Content-Length exceeds maximum"));
        assert_closed_by_protocol_error(&mut e, t0);
    }

    /// A body as large as the cap allows is awaited, not refused.
    #[test]
    fn a_body_at_the_cap_is_awaited_in_full() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        let mut raw =
            format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {MAX_RTSP_BODY_BYTES}\r\n\r\n")
                .into_bytes();
        raw.extend(std::iter::repeat_n(b'v', MAX_RTSP_BODY_BYTES));
        for c in raw.chunks(64 * 1024) {
            e.handle_input(c, t0);
        }
        assert_eq!(reply(&mut e, a).unwrap().body.len(), MAX_RTSP_BODY_BYTES);
        assert!(e.inbuf.is_empty());
    }

    /// Port of `interleaved_rtp_complex_header_delivered_as_whole_packet`.
    #[test]
    fn interleaved_rtp_complex_header_delivered_as_whole_packet() {
        // CC=1, X=1, P=1: CSRC, one extension word, 4 payload bytes and 2
        // padding bytes.
        let mut rtp = vec![0xB1, RTP_PT_MP2T];
        rtp.extend_from_slice(&[0u8; 10]);
        rtp.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        rtp.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]);
        rtp.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        rtp.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        rtp.extend_from_slice(&[0x00, 0x02]);
        let mut e = interleaved();
        e.handle_input(&interleaved_frame(0, &rtp), Instant::now());
        assert_eq!(media(&events(&mut e)), [rtp.as_slice()]);
        assert_eq!(e.counters().rtp_frames, 1);
        assert_eq!(e.counters().malformed_frames, 0);
    }

    /// Port of `interleaved_malformed_rtp_dropped_and_counted`.
    #[test]
    fn interleaved_malformed_rtp_dropped_and_counted() {
        // X=1 but the extension header is missing.
        let rtp = vec![0x90, RTP_PT_MP2T, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut e = interleaved();
        e.handle_input(&interleaved_frame(0, &rtp), Instant::now());
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().rtp_frames, 1);
        assert_eq!(e.counters().malformed_frames, 1);
    }

    #[test]
    fn a_frame_split_at_any_offset_is_delivered_once() {
        let mut rtp = valid_rtp_header().to_vec();
        rtp.extend_from_slice(b"PAYLOAD!");
        let mut raw = interleaved_frame(0, &rtp);
        raw.extend_from_slice(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nv=0");
        let check = |e: &mut ClientEngine, a: TxnId, at: &str| {
            let ev = events(e);
            match ev.as_slice() {
                [Event::Media(m), Event::Reply { txn, result: Ok(r) }] => {
                    assert_eq!(&m[..], rtp.as_slice(), "{at}");
                    assert_eq!(*txn, a, "{at}");
                    assert_eq!(&r.body[..], b"v=0", "{at}");
                }
                other => panic!("{at}: {other:?}"),
            }
            assert_eq!(e.counters().rtp_frames, 1, "{at}");
            assert_eq!(e.counters().rtsp_messages, 1, "{at}");
            assert_eq!(e.counters().malformed_frames, 0, "{at}");
            assert!(e.inbuf.is_empty(), "{at}");
        };
        for k in 0..=raw.len() {
            let t0 = Instant::now();
            let mut e = interleaved();
            let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
            e.handle_input(&raw[..k], t0);
            e.handle_input(&raw[k..], t0);
            check(&mut e, a, &format!("split at {k}"));
        }
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        for b in &raw {
            e.handle_input(std::slice::from_ref(b), t0);
        }
        check(&mut e, a, "one byte at a time");
    }

    #[test]
    fn ten_thousand_unmatched_replies_are_counted_and_keep_no_memory() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let one_reply = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n";
        let flood: Vec<u8> = one_reply.repeat(5_000);
        e.handle_input(&flood, t0);
        for _ in 0..5_000 {
            e.handle_input(one_reply, t0);
        }
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 10_000);
        assert_eq!(e.counters().rtsp_messages, 10_000);
        assert!(e.inbuf.is_empty());
        // The request is still waiting for its own reply.
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
    }

    #[test]
    fn a_reply_in_the_keepalive_range_with_no_ping_is_dropped() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        e.handle_input(
            b"RTSP/1.0 454 Session Not Found\r\nCSeq: 1000001\r\n\r\n",
            t0,
        );
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 1);
        assert_eq!(e.counters().keepalive_responses, 0);
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
    }

    #[test]
    fn a_server_request_never_fails_a_pending_request() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let mut raw =
            b"ANNOUNCE rtsp://cam.example/live RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nv=0"
                .to_vec();
        raw.extend_from_slice(b"OPTIONS * RTSP/1.0\r\n\r\n");
        raw.extend_from_slice(b"GET_PARAMETER rtsp://cam.example/live RTSP/1.0\r\nCSeq: x\r\n\r\n");
        e.handle_input(&raw, t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 3);
        assert_eq!(e.counters().rtsp_messages, 3);
        assert!(e.inbuf.is_empty());
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
    }

    #[test]
    fn a_response_without_a_cseq_goes_to_the_pending_request() {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
        e.set_interleaved(CHANNELS);
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        one(&mut e);
        // The 401 retry runs for a reply without a CSeq too.
        e.handle_input(
            b"RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n",
            t0,
        );
        assert!(events(&mut e).is_empty());
        let retry = one(&mut e);
        assert!(authz(&retry).is_some());
        e.handle_input(b"RTSP/1.0 200 OK\r\nPublic: OPTIONS, DESCRIBE\r\n\r\n", t0);
        let ok = reply(&mut e, a).unwrap();
        assert!(ok.headers["public"].contains("DESCRIBE"));
        assert_eq!(e.counters().unmatched_replies, 0);
        assert_eq!(e.counters().rtsp_messages, 2);
    }

    #[test]
    fn a_response_without_a_cseq_and_nothing_pending_is_counted() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.handle_input(b"RTSP/1.0 200 OK\r\n\r\n", t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 1);
        // A CSeq that is present but matches nothing stays unmatched, and
        // the waiting request keeps waiting.
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n", t0);
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: x\r\n\r\n", t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 3);
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
        // Once answered, a reply without a CSeq is unmatched again.
        e.handle_input(b"RTSP/1.0 200 OK\r\n\r\n", t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 4);
    }

    #[test]
    fn an_unparseable_response_fails_the_pending_request_with_the_parse_error() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let bad = b"RTSP/1.0 abc OK\r\nCSeq: 1\r\n\r\n";
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let mut raw = bad.to_vec();
        raw.extend(interleaved_frame(0, &valid_rtp_header()));
        e.handle_input(&raw, t0);
        let ev = events(&mut e);
        match ev.as_slice() {
            [
                Event::Reply {
                    txn,
                    result:
                        Err(RtspError::BadResponse {
                            detail: "non-numeric status code",
                        }),
                },
                Event::Media(_),
            ] => assert_eq!(*txn, a),
            other => panic!("{other:?}"),
        }
        assert_eq!(e.counters().unmatched_replies, 0);
        // With nothing waiting it is counted, and the stream goes on.
        e.handle_input(bad, t0);
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 1);
        let b = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n", t0);
        assert_eq!(reply(&mut e, b).unwrap().status, 200);
    }

    /// Answers a SETUP through the input path.
    fn setup_over_input(e: &mut ClientEngine, session: &str, at: Instant) {
        let a = e.submit(spec(RtspMethod::Setup, URI), None, at).unwrap();
        let c = cseq(&one(e));
        e.handle_input(
            format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\nSession: {session}\r\n\r\n").as_bytes(),
            at,
        );
        assert_eq!(reply(e, a).unwrap().status, 200);
    }

    #[test]
    fn a_session_timeout_near_u64_max_does_not_wrap_the_interval() {
        for enable_first in [true, false] {
            let t0 = Instant::now();
            let mut e = interleaved();
            if enable_first {
                e.enable_keepalive(None, t0);
            }
            setup_over_input(&mut e, &format!("abc;timeout={}", u64::MAX), t0);
            if !enable_first {
                e.enable_keepalive(None, t0);
            }
            let far = t0 + Duration::from_secs(100 * 365 * 24 * 3600);
            assert!(
                e.poll_timeout().is_none_or(|d| d > far),
                "enable first: {enable_first}"
            );
            e.handle_timeout(t0 + Duration::from_secs(3600), false);
            assert!(e.poll_transmit().is_none());
        }
    }

    #[test]
    fn timeout_zero_floors_the_interval_at_100_ms() {
        for enable_first in [true, false] {
            let t0 = Instant::now();
            let mut e = interleaved();
            if enable_first {
                e.enable_keepalive(None, t0);
            }
            setup_over_input(&mut e, "abc;timeout=0", t0);
            if !enable_first {
                e.enable_keepalive(None, t0);
            }
            assert_eq!(
                e.poll_timeout(),
                Some(ms(t0, 100)),
                "enable first: {enable_first}"
            );
        }
    }

    #[test]
    fn input_after_the_stream_ended_is_ignored() {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.handle_input(b"RTSP/1.0 200 OK\r\n", t0);
        e.handle_closed(StreamEnd::PeerClosed);
        events(&mut e);
        assert!(e.inbuf.is_empty());
        e.handle_input(&interleaved_frame(0, &valid_rtp_header()), t0);
        assert!(e.poll_event().is_none());
        assert_eq!(e.counters().rtp_frames, 0);
        assert!(e.inbuf.is_empty());
    }

    #[test]
    fn an_accepted_teardown_does_not_stop_the_input() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
        let mut raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n".to_vec();
        raw.extend(interleaved_frame(0, &valid_rtp_header()));
        e.handle_input(&raw, t0);
        let ev = events(&mut e);
        assert!(
            matches!(
                ev.as_slice(),
                [
                    Event::Ended(EndReason::CleanTeardown),
                    Event::Reply { txn, result: Ok(_) },
                    Event::Media(_)
                ] if *txn == a
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn the_fuzz_hook_returns_on_hostile_input() {
        let mut flood = vec![b'A'; 70 * 1024];
        flood.extend_from_slice(b"\r\n\r\n");
        for data in [
            &b""[..],
            b"$",
            b"$\x00\xff\xff",
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: nope\r\n\r\n",
            b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Digest\r\n\r\n",
            b"\xff\xfe\r\n\r\n$\x01\x00\x00",
            &flood,
        ] {
            crate::rtsp::client::fuzz_client_engine_input(data);
        }
    }
}
