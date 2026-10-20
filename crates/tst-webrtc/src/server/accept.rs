//! The server's accept thread: one TCP listener (wrapped in rustls for
//! `wheps://`), one WHEP exchange per connection, handled inline.
//!
//! An exchange is small (a request head, an SDP offer of a few
//! kilobytes, the answer), so connections are served one after another on
//! this thread. Each read waits a short slice at a time so a stopping
//! server is noticed between slices; the whole request must arrive within
//! [`REQUEST_DEADLINE`], else it is answered with 408. Every exchange ends
//! with `Connection: close`.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use tracing::debug;

use crate::server::Shared;
use crate::server::http::{self, HeadParse, MAX_HEAD, Request, Response, Route, resp};
use crate::server::media::{ControlMsg, OfferRequest, RejectReason};
use crate::server::negotiate::payload_types_are_consistent;
use crate::stats::bump;

/// How long a client has to send its whole request.
pub(crate) const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// How long writing the response may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest single read, between checks of the shutdown flag.
const READ_SLICE: Duration = Duration::from_millis(100);

/// How long a refused request's connection is drained before it closes.
const LINGER: Duration = Duration::from_millis(500);

/// The most bytes drained from a refused request's connection.
const LINGER_BYTES: usize = 64 * 1024;

/// The `Retry-After` value sent with a 503 at the viewer cap, in seconds.
const RETRY_AFTER_CAP_SECS: &str = "5";

/// An accepted connection: plain TCP, or TCP under a rustls server session.
/// The TLS handshake runs inside the first read, under the same slice and
/// deadline as any other read.
enum Conn {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Conn {
    fn new(stream: TcpStream, shared: &Shared) -> io::Result<Self> {
        #[cfg(feature = "tls")]
        if let Some(config) = &shared.tls {
            let conn = rustls::ServerConnection::new(Arc::clone(config))
                .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
            return Ok(Self::Tls(Box::new(rustls::StreamOwned::new(conn, stream))));
        }
        let _ = shared;
        Ok(Self::Plain(stream))
    }

    fn sock(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            #[cfg(feature = "tls")]
            Self::Tls(s) => &s.sock,
        }
    }

    /// Tells a TLS peer the response is complete, so it does not take the
    /// close for a truncation.
    fn finish(&mut self) {
        #[cfg(feature = "tls")]
        if let Self::Tls(s) = self {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.flush(),
        }
    }
}

/// The running accept thread.
pub(crate) struct AcceptThread {
    handle: JoinHandle<()>,
    /// Disconnects when the thread exits (or panics).
    done: Receiver<()>,
}

impl AcceptThread {
    /// Starts the thread on `listener`.
    pub(crate) fn spawn(listener: TcpListener, shared: Arc<Shared>) -> io::Result<Self> {
        let (done_tx, done) = mpsc::sync_channel::<()>(0);
        let handle = thread::Builder::new()
            .name("tst-webrtc-accept".into())
            .spawn(move || {
                let _done = done_tx;
                run(&listener, &shared);
            })?;
        Ok(Self { handle, done })
    }

    /// Waits up to `within` for the thread to exit; `true` when it did.
    /// The caller has set the shutdown flag and woken `accept()`.
    pub(crate) fn join(self, within: Duration) -> bool {
        match self.done.recv_timeout(within) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = self.handle.join();
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
        }
    }
}

/// The first wait after a failed `accept()`.
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(50);

/// The longest wait after repeated `accept()` failures.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// The wait after an `accept()` failure, given the previous one: 50 ms
/// first, doubling to at most 1 s.
fn next_backoff(previous: Option<Duration>) -> Duration {
    previous.map_or(ACCEPT_BACKOFF_MIN, |d| {
        d.saturating_mul(2).min(ACCEPT_BACKOFF_MAX)
    })
}

/// Accepts connections until the shutdown flag is set.
///
/// A failed `accept()` (out of descriptors, an aborted connection) never
/// stops the server, and never makes the loop spin: each failure waits
/// [`next_backoff`] before the next attempt, checking the shutdown flag
/// every 50 ms, and a successful accept resets the wait.
fn run(listener: &TcpListener, shared: &Shared) {
    let mut backoff = None;
    loop {
        let accepted = listener.accept();
        if shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        match accepted {
            Ok((stream, peer)) => {
                backoff = None;
                serve(stream, peer, shared);
            }
            Err(e) => {
                let wait = next_backoff(backoff);
                backoff = Some(wait);
                debug!(error = %e, ?wait, "WHEP accept failed");
                let end = Instant::now() + wait;
                loop {
                    if shared.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    thread::sleep(left.min(ACCEPT_BACKOFF_MIN));
                }
            }
        }
    }
}

/// Serves one connection: reads the request, answers it, closes.
fn serve(stream: TcpStream, peer: SocketAddr, shared: &Shared) {
    if let Err(e) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
        debug!(%peer, error = %e, "WHEP connection: set_write_timeout failed");
    }
    let mut stream = match Conn::new(stream, shared) {
        Ok(c) => c,
        Err(e) => {
            debug!(%peer, error = %e, "WHEP connection: TLS session setup failed");
            return;
        }
    };
    let (response, unread) = match read_request(&mut stream, shared) {
        Ok((request, body)) => (answer(&request, &body, peer, shared), false),
        Err(ReadFailure::Respond(r)) => (r, true),
        Err(ReadFailure::Closed) => return,
    };
    let bytes = http::render(&response, &shared.url.cors_origin);
    if let Err(e) = stream.write_all(&bytes).and_then(|()| stream.flush()) {
        debug!(%peer, error = %e, "WHEP response write failed");
        return;
    }
    stream.finish();
    if unread {
        linger(&mut stream);
    }
}

/// Ends the sending side and reads (and discards) what the client still
/// sends, briefly. Closing a socket with unread input resets the
/// connection, which can destroy the response before the client reads it.
fn linger(stream: &mut Conn) {
    let _ = stream.sock().shutdown(Shutdown::Write);
    let end = Instant::now() + LINGER;
    let mut sink = [0u8; 4096];
    let mut left = LINGER_BYTES;
    while left > 0 {
        let wait = end.saturating_duration_since(Instant::now());
        if wait.is_zero() || stream.sock().set_read_timeout(Some(wait)).is_err() {
            return;
        }
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => return,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}

/// Why no request was read.
enum ReadFailure {
    /// Answer with this response.
    Respond(Response),
    /// The client went away; nothing to answer.
    Closed,
}

/// Reads a request head and its body within [`REQUEST_DEADLINE`].
fn read_request(stream: &mut Conn, shared: &Shared) -> Result<(Request, Vec<u8>), ReadFailure> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut buf = Vec::with_capacity(4096);
    let (request, head_len) = loop {
        match http::parse_head(&buf) {
            HeadParse::Done { request, head_len } => break (request, head_len),
            HeadParse::TooLarge => return Err(ReadFailure::Respond(resp(431, ""))),
            HeadParse::Malformed => return Err(ReadFailure::Respond(resp(400, ""))),
            HeadParse::Incomplete => read_some(stream, &mut buf, MAX_HEAD + 1, deadline, shared)?,
        }
    };
    let total = head_len + request.content_length;
    while buf.len() < total {
        read_some(stream, &mut buf, total, deadline, shared)?;
    }
    let body = buf[head_len..total].to_vec();
    Ok((request, body))
}

/// Appends what one read returns to `buf`, never growing it past `cap`.
/// Waits in [`READ_SLICE`] steps, checking the shutdown flag between
/// them.
fn read_some(
    stream: &mut Conn,
    buf: &mut Vec<u8>,
    cap: usize,
    deadline: Instant,
    shared: &Shared,
) -> Result<(), ReadFailure> {
    let mut chunk = [0u8; 4096];
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return Err(ReadFailure::Respond(resp(503, "")));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ReadFailure::Respond(resp(408, "")));
        }
        if let Err(e) = stream.sock().set_read_timeout(Some(left.min(READ_SLICE))) {
            debug!(error = %e, "WHEP connection: set_read_timeout failed");
            return Err(ReadFailure::Closed);
        }
        let room = cap.saturating_sub(buf.len()).min(chunk.len());
        match stream.read(&mut chunk[..room]) {
            Ok(0) => return Err(ReadFailure::Closed),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                return Ok(());
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                debug!(error = %e, "WHEP connection read failed");
                return Err(ReadFailure::Closed);
            }
        }
    }
}

/// The response to a complete request.
fn answer(request: &Request, body: &[u8], peer: SocketAddr, shared: &Shared) -> Response {
    match http::route(request) {
        Route::Preflight => resp(204, ""),
        Route::MethodNotAllowed => with_header(resp(405, ""), "Allow", "POST, DELETE, OPTIONS"),
        Route::NotFound => resp(404, ""),
        Route::CreateSession { mount } => {
            if let Some(refused) = check_bearer(request, shared) {
                return refused;
            }
            create_session(&mount, body, peer, shared)
        }
        Route::DeleteSession { mount, id } => {
            if let Some(refused) = check_bearer(request, shared) {
                return refused;
            }
            let Some(mount) = shared.mount_by_path(&mount) else {
                return resp(404, "");
            };
            let mount = mount.id;
            match shared
                .control
                .request(|reply| ControlMsg::Delete { mount, id, reply })
            {
                Some(true) => resp(200, ""),
                Some(false) => resp(404, ""),
                None => resp(503, ""),
            }
        }
    }
}

/// `Some(401)` when the server takes a bearer token and the request does
/// not carry it.
fn check_bearer(request: &Request, shared: &Shared) -> Option<Response> {
    let want = shared.url.token.as_deref()?;
    if request.bearer.as_deref() == Some(want) {
        return None;
    }
    bump(&shared.rejected_auth);
    Some(with_header(resp(401, ""), "WWW-Authenticate", "Bearer"))
}

fn create_session(mount: &str, body: &[u8], peer: SocketAddr, shared: &Shared) -> Response {
    let Some(state) = shared.mount_by_path(mount) else {
        return resp(404, "");
    };
    let Some(offer) = std::str::from_utf8(body)
        .ok()
        .and_then(|sdp| SdpOffer::from_sdp_string(sdp).ok())
    else {
        return resp(400, "the body is not an SDP offer");
    };
    // str0m panics on such an offer instead of refusing it.
    if !payload_types_are_consistent(&offer.to_sdp_string()) {
        return resp(400, "the offer maps a payload type to two formats");
    }
    let outcome = shared.control.request(|reply| {
        ControlMsg::Offer(OfferRequest {
            mount: state.id,
            offer,
            remote_hint: Some(peer),
            reply,
        })
    });
    match outcome {
        Some(Ok(created)) => {
            let mut r = resp(201, created.answer.into_bytes());
            r.headers
                .push(("Content-Type", "application/sdp".to_string()));
            r.headers
                .push(("Location", format!("{mount}/sessions/{}", created.id)));
            r
        }
        Some(Err(RejectReason::Cap)) => {
            with_header(resp(503, ""), "Retry-After", RETRY_AFTER_CAP_SECS)
        }
        Some(Err(RejectReason::Codec)) => resp(406, "no offered video payload matches the mount"),
        Some(Err(RejectReason::Shutdown)) | None => resp(503, ""),
        Some(Err(RejectReason::Internal)) => resp(500, ""),
    }
}

fn with_header(mut r: Response, name: &'static str, value: &str) -> Response {
    r.headers.push((name, value.to_string()));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_failures_back_off_from_50_ms_to_at_most_1_s() {
        let mut waits = Vec::new();
        let mut backoff = None;
        for _ in 0..8 {
            let wait = next_backoff(backoff);
            waits.push(wait.as_millis());
            backoff = Some(wait);
        }
        assert_eq!(waits, [50, 100, 200, 400, 800, 1000, 1000, 1000]);
        // Every failure waits at least the minimum, so the loop never spins.
        assert!(waits.iter().all(|&w| w >= 50));
    }
}
