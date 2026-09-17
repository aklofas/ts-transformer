//! `RtspClient::options` + `RtspClient::describe`. Single-request
//! synchronous send + recv against the control connection.

use std::collections::HashMap;
use std::io::{Read, Write};

use crate::error::RtspError;
use crate::rtsp::client::RtspClient;
use crate::rtsp::client::keepalive::{KEEPALIVE_CSEQ_BASE, handle_keepalive_response};
use crate::rtsp::message::{RtspFraming, RtspMethod, RtspRequest, RtspResponse};
use crate::sdp::Sdp;

/// Response shape returned by [`RtspClient::options`]. Exposes the
/// server's advertised methods (parsed from the `Public:` header) plus
/// the raw header map for callers that need to look at other fields.
#[derive(Debug, Clone)]
pub struct OptionsResponse {
    /// Methods advertised in the `Public:` header, split on `,` and
    /// trimmed. Empty if the server didn't return the header.
    pub public_methods: Vec<String>,
    /// All response headers (lowercase keys), for callers needing
    /// e.g. `Server:` or `Date:`.
    pub headers: HashMap<String, String>,
}

impl RtspClient {
    /// Build a base RTSP request: bump the CSeq counter, create the
    /// [`RtspRequest`] for `method` at `uri`, and add the mandatory
    /// `CSeq:` and `User-Agent:` headers. Callers add method-specific
    /// headers (e.g., `Session:`, `Transport:`) on the returned value.
    pub(crate) fn base_request(&mut self, method: RtspMethod, uri: String) -> RtspRequest {
        let cseq = self.bump_cseq();
        RtspRequest::new(method, uri, self.url.rtsp_version)
            .header("cseq", cseq.to_string())
            .header("user-agent", self.user_agent.as_str())
    }

    /// Check that `resp` carries a 200 OK status.
    ///
    /// Returns [`RtspError::Protocol`] for any other status. Used as a
    /// one-liner after every synchronous `send_and_read` call that
    /// expects a 200.
    pub(crate) fn expect_ok(&self, resp: &RtspResponse) -> Result<(), RtspError> {
        if resp.status != 200 {
            return Err(RtspError::Protocol {
                code: resp.status,
                reason: resp.reason.clone(),
            });
        }
        Ok(())
    }

    /// Send an OPTIONS request; parse the `Public:` header into the
    /// list of methods the server supports. Authenticates via the shared
    /// `send_authenticated` path (pre-emptive + one reactive 401 retry) —
    /// RFC 7826 servers may challenge any method, OPTIONS included.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Io`] on socket-level failure.
    /// - [`RtspError::BadResponse`] on malformed response bytes.
    /// - [`RtspError::AuthFailed`] if the server rejects the credentials.
    /// - [`RtspError::Protocol`] on any other non-200 status.
    /// - [`RtspError::LocalCancel`] if the cancel handle was triggered
    ///   mid-read.
    pub fn options(&mut self) -> Result<OptionsResponse, RtspError> {
        let uri = self.url.render_no_credentials();
        let mut extra: Vec<(&str, String)> = Vec::new();
        if let Some(sid) = &self.session_id {
            extra.push(("session", sid.clone()));
        }
        let resp = self.send_authenticated(RtspMethod::Options, &uri, &extra)?;
        self.last_server_version = resp.version;
        self.expect_ok(&resp)?;
        let public_methods = resp
            .headers
            .get("public")
            .map(|s| s.split(',').map(|p| p.trim().to_string()).collect())
            .unwrap_or_default();
        Ok(OptionsResponse {
            public_methods,
            headers: resp.headers,
        })
    }

    /// Send a DESCRIBE request and parse the SDP body. Authenticates via the
    /// shared `send_authenticated` path (pre-emptive + one reactive
    /// 401 retry).
    ///
    /// # Errors
    ///
    /// - [`RtspError::Io`] on socket-level failure.
    /// - [`RtspError::BadResponse`] on malformed response bytes.
    /// - [`RtspError::AuthFailed`] if the server rejects the credentials.
    /// - [`RtspError::Protocol`] on any other non-200 status.
    /// - [`RtspError::BadSdp`] if the response body isn't parseable SDP.
    /// - [`RtspError::LocalCancel`] if the cancel handle was triggered
    ///   mid-read.
    pub fn describe(&mut self) -> Result<Sdp, RtspError> {
        let uri = self.url.render_no_credentials();
        let mut extra: Vec<(&str, String)> = vec![("accept", "application/sdp".to_string())];
        if let Some(sid) = &self.session_id {
            extra.push(("session", sid.clone()));
        }
        let resp = self.send_authenticated(RtspMethod::Describe, &uri, &extra)?;
        self.last_server_version = resp.version;
        if resp.status != 200 {
            return Err(RtspError::Protocol {
                code: resp.status,
                reason: resp.reason,
            });
        }
        Sdp::parse(&resp.body)
    }

    /// Build an `Authorization:` header for `method` at `uri` from a raw
    /// `WWW-Authenticate` challenge, using the URL credentials and the next
    /// `qop=auth` nonce-count. Thin wrapper over
    /// [`crate::rtsp::auth::build_authorization`]. The `uri` MUST be the exact
    /// request-URI of the target method — gortsplib/MediaMTX hash the Digest
    /// HA2 against the request URI, so SETUP must pass its control URI
    /// (`…/trackID=0`), not the base URL.
    ///
    /// # Errors
    ///
    /// - [`RtspError::AuthFailed`] if the URL carries no credentials.
    /// - [`RtspError::AuthUnsupported`] if no recognized scheme is present.
    pub(crate) fn authorization_from_challenge(
        &self,
        method: RtspMethod,
        uri: &str,
        www_auth: &str,
    ) -> Result<String, RtspError> {
        let username = self.url.username.as_deref().ok_or(RtspError::AuthFailed)?;
        let password = self.url.password.as_ref().ok_or(RtspError::AuthFailed)?;
        // Next nonce-count for qop=auth (ignored by the no-qop form),
        // allocated under the shared auth lock so no other thread (the
        // keepalive) can be issued the same `nc` for the same nonce —
        // a qop=auth server rejects a repeated `nc` as a replay
        // (RFC 7616 §3.4).
        let nc = {
            let mut auth = crate::rtsp::client::lock_unpoisoned(&self.auth);
            auth.nc += 1;
            auth.nc
        };
        crate::rtsp::auth::build_authorization(method, uri, www_auth, username, password, nc)
    }

    /// Pre-emptive `Authorization:` header for `method` at `uri`, computed
    /// from the challenge cached at the first 401 (see
    /// [`RtspClient::auth`]). Returns `Ok(None)` when no challenge
    /// has been seen yet or the URL carries no credentials — so
    /// unauthenticated servers are unaffected.
    pub(crate) fn preemptive_authorization(
        &self,
        method: RtspMethod,
        uri: &str,
    ) -> Result<Option<String>, RtspError> {
        let username = match self.url.username.as_deref() {
            Some(u) => u,
            None => return Ok(None),
        };
        let password = match self.url.password.as_ref() {
            Some(p) => p,
            None => return Ok(None),
        };
        // Snapshot the challenge and allocate its nonce-count under ONE
        // lock acquisition, so the pair can never mix a stale challenge
        // with a post-rotation count (see `AuthState`).
        let (www, nc) = {
            let mut auth = crate::rtsp::client::lock_unpoisoned(&self.auth);
            match auth.challenge.clone() {
                Some(www) => {
                    auth.nc += 1;
                    (www, auth.nc)
                }
                None => return Ok(None),
            }
        };
        Ok(Some(crate::rtsp::auth::build_authorization(
            method, uri, &www, username, password, nc,
        )?))
    }

    /// Cache the `WWW-Authenticate` challenge from a 401 for pre-emptive use
    /// on later requests. A *changed* challenge (new nonce) resets the
    /// `qop=auth` nonce-count so it restarts at 1. Challenge and count are
    /// updated under the same lock the keepalive thread reads them with, so
    /// the rotation is atomic (see `AuthState`).
    pub(crate) fn cache_auth_challenge(&mut self, www_auth: String) {
        crate::rtsp::client::lock_unpoisoned(&self.auth).cache_challenge(www_auth);
    }

    /// Send `method` at `uri` with `extra_headers`, attaching cached
    /// credentials pre-emptively and retrying once on a fresh 401 challenge.
    /// Each attempt is a fresh request (new CSeq). Unauthenticated servers are
    /// unaffected — pre-emptive auth is a no-op until a challenge is seen.
    ///
    /// This is the single auth-aware send path shared by OPTIONS / DESCRIBE / SETUP /
    /// PLAY / PAUSE, so every method authenticates uniformly (gortsplib /
    /// MediaMTX challenge them all, not just DESCRIBE).
    pub(crate) fn send_authenticated(
        &mut self,
        method: RtspMethod,
        uri: &str,
        extra_headers: &[(&str, String)],
    ) -> Result<RtspResponse, RtspError> {
        // Attempt 1 — pre-emptive (no-op until a challenge is cached).
        let preauth = self.preemptive_authorization(method, uri)?;
        let mut req = self.base_request(method, uri.to_string());
        for (k, v) in extra_headers {
            req = req.header(k, v.as_str());
        }
        if let Some(a) = &preauth {
            req = req.header("authorization", a.as_str());
        }
        let resp = self.send_and_read(&req.encode_checked()?)?;
        if resp.status != 401 {
            return Ok(resp);
        }
        // Attempt 2 — reactive: cache the fresh challenge, retry once with a
        // new CSeq. Covers servers that challenge this method without having
        // challenged DESCRIBE, and stale-nonce rotation.
        let www = resp
            .headers
            .get("www-authenticate")
            .ok_or(RtspError::BadResponse {
                detail: "401 without WWW-Authenticate header",
            })?
            .clone();
        self.cache_auth_challenge(www.clone());
        let authorization = self.authorization_from_challenge(method, uri, &www)?;
        let mut retry = self.base_request(method, uri.to_string());
        for (k, v) in extra_headers {
            retry = retry.header(k, v.as_str());
        }
        retry = retry.header("authorization", authorization);
        let retry_resp = self.send_and_read(&retry.encode_checked()?)?;
        if retry_resp.status == 401 {
            return Err(RtspError::AuthFailed);
        }
        Ok(retry_resp)
    }

    /// Write a serialized RTSP request, then read one complete response.
    ///
    /// Two read paths:
    ///
    /// 1. **Pump inactive** (UDP transport or pre-SETUP): writes + reads
    ///    happen under a single stream-mutex acquisition. The cancel
    ///    flag is checked between read polls; the underlying stream has
    ///    a short read timeout (100 ms, set in
    ///    [`RtspClient::connect_with`]), so each
    ///    `WouldBlock`/`TimedOut` round-trip is a cancel-check
    ///    opportunity. Holding the lock through the whole exchange
    ///    means the keepalive thread waits if a request is in flight —
    ///    correct since RTSP isn't pipelined.
    ///
    /// 2. **Pump active** (TCP-interleaved post-SETUP): writes happen
    ///    under the stream mutex (briefly, then released). The response
    ///    is read from `pump_state.ctrl_rx` matched by CSeq, since the
    ///    background pump thread owns reads in this mode (reading the
    ///    stream directly here would race with the pump). Responses
    ///    with CSeq >= [`KEEPALIVE_CSEQ_BASE`](crate::rtsp::client::keepalive::KEEPALIVE_CSEQ_BASE)
    ///    never reach `ctrl_rx` — the pump consumes them itself via
    ///    `keepalive::handle_keepalive_response` (a 401 refreshes the
    ///    shared auth challenge cache; a 454 marks the session dead).
    pub(crate) fn send_and_read(
        &mut self,
        request_bytes: &[u8],
    ) -> Result<RtspResponse, RtspError> {
        // `RtspClientBuilder::request_timeout` (CORR-26): every request
        // method funnels through here, so this one line is the producer of
        // `RtspError::Timeout`. `None` keeps the unbounded pre-knob wait.
        // `checked_add` mirrors `H264Receiver::recv_au`'s idiom: a timeout
        // too large to represent as an `Instant` (e.g. `Duration::MAX`)
        // saturates to "no deadline" rather than panicking on overflow.
        let deadline = self
            .request_timeout
            .and_then(|t| std::time::Instant::now().checked_add(t));
        self.send_and_read_with_deadline(request_bytes, deadline)
    }

    /// Variant of [`Self::send_and_read`] with an optional hard deadline,
    /// honored on BOTH read paths. When `deadline` elapses with no
    /// complete response, returns [`RtspError::Timeout`]. Deadline
    /// granularity is one read-poll cycle (~100 ms — the stream read
    /// timeout set in [`RtspClient::connect_with`]) on the non-pump path,
    /// one `ctrl_rx` poll on the pump path.
    pub(crate) fn send_and_read_with_deadline(
        &mut self,
        request_bytes: &[u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<RtspResponse, RtspError> {
        if self.pump_state.is_some() {
            return self.send_and_read_via_pump_with_deadline(request_bytes, deadline);
        }
        let mut s = crate::rtsp::client::lock_unpoisoned(&self.stream);
        s.write_all(request_bytes)
            .map_err(|e| RtspError::Io(e.kind()))?;
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RtspError::LocalCancel);
            }
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                return Err(RtspError::Timeout);
            }
            match s.read(&mut chunk) {
                Ok(0) => return Err(RtspError::Io(std::io::ErrorKind::UnexpectedEof)),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // Body-aware accumulation via the shared `rtsp_frame_decision`
                    // two-phase policy — the SAME helper + constants the server
                    // session request loop uses, so client and server agree
                    // byte-for-byte:
                    //
                    // Phase 1 — pre-terminator (no CRLFCRLF yet): header
                    // accumulation is capped at MAX_RTSP_MESSAGE_BYTES (64 KiB),
                    // preserving the unterminated-header DoS bound.
                    //
                    // Phase 2 — post-terminator: the declared Content-Length is
                    // parsed up front; a malformed/duplicate/over-cap
                    // (> MAX_RTSP_BODY_BYTES) value is fatal NOW rather than read
                    // toward EOF. A legitimate body up to 1 MiB is awaited in
                    // full (the exact header + 4 + content_length ceiling bounds
                    // a peer dribbling past its declared body forever).
                    //
                    // Inner loop (not a single decision): a keepalive ping's
                    // response can sit in the buffer AHEAD of this request's
                    // response — the keepalive thread never reads, and in
                    // non-pump mode nothing else drains the stream between
                    // requests. Such a message (CSeq in the keepalive range)
                    // is consumed here — never surfaced as the caller's
                    // response — and parsing continues on the bytes already
                    // buffered rather than waiting for another read.
                    loop {
                        match crate::rtsp::message::rtsp_frame_decision(&buf) {
                            RtspFraming::NeedMore => break, // bounded; read more
                            RtspFraming::HeadersTooLong => {
                                return Err(RtspError::BadResponse {
                                    detail: "RTSP response headers exceed maximum",
                                });
                            }
                            RtspFraming::BadContentLength(detail) => {
                                return Err(RtspError::BadResponse { detail });
                            }
                            RtspFraming::Complete { total_len } => {
                                // Full message present — parse. `total_len` is
                                // the exact byte length of this message.
                                let (resp, _consumed) = RtspResponse::parse(&buf[..total_len])?;
                                if resp.cseq().is_some_and(|c| c >= KEEPALIVE_CSEQ_BASE) {
                                    handle_keepalive_response(
                                        &resp,
                                        &self.auth,
                                        self.session_dead.as_deref(),
                                        &self.end_reason,
                                    );
                                    buf.drain(..total_len);
                                    continue;
                                }
                                return Ok(resp);
                            }
                        }
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // read timeout; check cancel + retry
                    continue;
                }
                Err(e) => return Err(RtspError::Io(e.kind())),
            }
        }
    }

    /// Pump-active variant of [`Self::send_and_read`] with an optional
    /// hard deadline. When `deadline` elapses with no matching response,
    /// returns [`RtspError::Timeout`].
    ///
    /// Used by [`Self::teardown`] from within `Drop`: if the server has
    /// silently half-closed (no FIN on the wire — e.g. when
    /// `RtspServer::stop` cancels per-session tasks but leaves the
    /// write half open via lingering `ActiveSession` Arcs), the
    /// in-flight TEARDOWN write succeeds into the kernel buffer but
    /// no response ever arrives. Without a deadline, `Drop` blocks
    /// the whole test scope.
    pub(crate) fn send_and_read_via_pump_with_deadline(
        &mut self,
        request_bytes: &[u8],
        deadline: Option<std::time::Instant>,
    ) -> Result<RtspResponse, RtspError> {
        let req_cseq = parse_cseq_from_request(request_bytes);
        // Ask the interleaved pump to yield the shared stream lock while we
        // write this request. `SharedStreamReader::read` holds the mutex
        // across each blocking ~100 ms socket read, so without this hand-off
        // the pump monopolizes the lock and this write-lock acquisition can
        // be starved unboundedly on a contended runner (the in-session
        // sibling of the teardown starvation bounded in `RtspClient::Drop`).
        // The pump skips its next read while the gate is set, so we acquire
        // within at most one in-flight read cycle. The gate is cleared after
        // the lock is released (including on write error).
        let write_gate = self
            .pump_state
            .as_ref()
            .expect("pump_state is Some — checked by caller")
            .write_gate
            .clone();
        // RAII guard (not a manual increment/decrement pair): the
        // decrement must survive any panic while the gate is held (the
        // stream lock below recovers from poison rather than panicking —
        // see `lock_unpoisoned` — but e.g. a panic inside `write_all`
        // would otherwise leave the gate stuck nonzero and the pump would
        // skip reads forever).
        let write_result = {
            let _gate = crate::rtsp::client::WriteGateGuard::enter(&write_gate);
            let mut s = crate::rtsp::client::lock_unpoisoned(&self.stream);
            let r = s
                .write_all(request_bytes)
                .map_err(|e| RtspError::Io(e.kind()));
            drop(s);
            r
        };
        write_result?;
        let pump = self
            .pump_state
            .as_ref()
            .expect("pump_state is Some — checked by caller");
        loop {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(RtspError::LocalCancel);
            }
            if let Some(d) = deadline {
                if std::time::Instant::now() >= d {
                    return Err(RtspError::Timeout);
                }
            }
            match pump
                .ctrl_rx
                .recv_timeout(std::time::Duration::from_millis(100))
            {
                Ok(msg_bytes) => {
                    let (resp, _consumed) = match RtspResponse::parse(&msg_bytes) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    match (req_cseq, resp.cseq()) {
                        (Some(req), Some(got)) if req == got => return Ok(resp),
                        _ => continue,
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RtspError::Io(std::io::ErrorKind::UnexpectedEof));
                }
            }
        }
    }
}

/// Best-effort scan for the `CSeq:` header in a serialized request.
/// Returns the parsed integer when found. Used only by the pump-active
/// read path to match responses by CSeq.
fn parse_cseq_from_request(bytes: &[u8]) -> Option<u32> {
    // Only look at the header section (up to the first CRLFCRLF).
    let end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(bytes.len());
    let header_text = std::str::from_utf8(&bytes[..end]).ok()?;
    for line in header_text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("cseq:") {
            return v.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    /// Spawn a one-shot mock RTSP server that serves a single response.
    fn mock_server(canned_response: &'static [u8]) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Read whatever the client sent (ignore content)
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                sock.write_all(canned_response).unwrap();
            }
        });
        (port, h)
    }

    /// Spawn a one-shot mock server that writes `response` in full, ignoring
    /// write errors (the client may close early once it trips an accumulation
    /// cap), then closes the connection.
    ///
    /// Deterministic by construction: the server writes a fixed, bounded blob
    /// and then drops the socket — there is no fill/timeout race. For DoS
    /// fixtures the blob is built to be > the relevant cap (so the client trips
    /// the cap before EOF); for the positive guard it's a complete, valid
    /// response (so the client parses + returns OK). The `response` is owned so
    /// callers can build large bodies at runtime without leaking.
    fn serve_then_close(response: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut req = [0u8; 4096];
                let _ = sock.read(&mut req);
                // Ignore the error if the client closed after tripping a cap.
                let _ = sock.write_all(&response);
                // Socket drops at scope end → clean FIN, deterministic EOF.
            }
        });
        (port, h)
    }

    /// Read from `sock` until a full RTSP request head (terminated by
    /// `\r\n\r\n`) has arrived, or EOF/timeout. TCP may split a request
    /// across reads — a single read() races the client's write and can
    /// capture only a prefix (or, for the second request, the tail of a
    /// split first request).
    fn read_request_head(sock: &mut std::net::TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        buf
    }

    /// Two-exchange mock: reply `first` to the first request, then read
    /// again and reply `second`. Returns the raw bytes of the SECOND
    /// request so the test can assert on its headers.
    fn mock_server_2(
        first: &'static [u8],
        second: &'static [u8],
    ) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let mut captured = Vec::new();
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = read_request_head(&mut sock);
                let _ = sock.write_all(first);
                captured = read_request_head(&mut sock);
                let _ = sock.write_all(second);
            }
            captured
        });
        (port, h)
    }

    #[test]
    fn options_authenticates_on_challenge() {
        // RFC 7826 servers may challenge ANY method, OPTIONS included
        // (gortsplib/MediaMTX do). options() must retry with credentials
        // via the shared send_authenticated path instead of surfacing a
        // raw Protocol{401}. Regression: options() used to bypass auth.
        let (port, h) = mock_server_2(
            b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY\r\n\r\n",
        );
        let mut client =
            RtspClient::connect(&format!("rtsp://user:pw@127.0.0.1:{port}/test")).unwrap();
        let opts = client
            .options()
            .expect("OPTIONS must authenticate after a 401 challenge");
        assert!(opts.public_methods.contains(&"DESCRIBE".to_string()));
        let retry = h.join().unwrap();
        let retry_txt = String::from_utf8_lossy(&retry).to_ascii_lowercase();
        assert!(
            retry_txt.contains("authorization: basic"),
            "retry must carry credentials, got:\n{retry_txt}"
        );
    }

    #[test]
    fn options_parses_public_header() {
        let (port, h) = mock_server(
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n",
        );
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{}/test", port)).unwrap();
        let opts = client.options().unwrap();
        assert!(opts.public_methods.contains(&"DESCRIBE".to_string()));
        assert!(opts.public_methods.contains(&"PLAY".to_string()));
        h.join().unwrap();
    }

    #[test]
    fn describe_parses_sdp() {
        let body =
            b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nt=0 0\r\nm=application 0 RTP/AVP 33\r\n";
        let resp = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut full = resp.into_bytes();
        full.extend_from_slice(body);
        let leaked: &'static [u8] = Box::leak(full.into_boxed_slice());
        let (port, h) = mock_server(leaked);
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{}/test", port)).unwrap();
        let sdp = client.describe().unwrap();
        assert_eq!(sdp.media.len(), 1);
        assert_eq!(sdp.media[0].payload_types, vec![33]);
        h.join().unwrap();
    }

    // --- B2: bounded client response accumulation (adversarial + positive) ---

    /// POSITIVE regression guard for Finding 1 (false-reject of valid large
    /// bodies). A legitimate DESCRIBE-style response with a body well above the
    /// 64 KiB accumulation cap but ≤ `MAX_RTSP_BODY_BYTES` (1 MiB) — e.g. a
    /// large SDP with many media sections — must be ACCEPTED and fully parsed,
    /// not rejected at 64 KiB. Guards against the body-unaware cap ever
    /// returning. Deterministic: the server writes the COMPLETE response, then
    /// closes — no fill/timeout race.
    #[test]
    fn send_and_read_accepts_valid_body_above_header_cap() {
        // ~512 KiB body — comfortably above the 64 KiB accumulation cap and
        // below the 1 MiB body cap. Built as valid SDP-ish bytes; only the size
        // and exact echo matter here.
        let body = vec![b'v'; 512 * 1024];
        let mut response = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let (port, h) = serve_then_close(response);
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{}/test", port)).unwrap();
        // `options()` parses + returns the response (status 200) without error.
        // It exercises `send_and_read` directly and proves the >64 KiB body is
        // accumulated and parsed rather than rejected.
        let opts = client.options().expect("valid ≤1 MiB body must parse OK");
        // Reaching Ok proves `send_and_read` accumulated past the 64 KiB header
        // cap, awaited the full 512 KiB body, and parsed it. The declared
        // Content-Length is surfaced in the headers.
        assert_eq!(
            opts.headers.get("content-length").map(String::as_str),
            Some((512 * 1024).to_string().as_str())
        );
        h.join().unwrap();
    }

    /// DoS guard: a response whose headers NEVER terminate (no `CRLFCRLF`
    /// within the 64 KiB header budget) must be rejected with `BadResponse`,
    /// not buffered unboundedly. Deterministic: the server writes a fixed
    /// 128 KiB junk blob (> the 64 KiB header cap) then closes, so the client
    /// trips the header cap before EOF on every run — no socket-fill timing
    /// dependence. Asserts ONLY on the error variant.
    #[test]
    fn send_and_read_rejects_unterminated_headers() {
        // 128 KiB of header-junk, never a CRLFCRLF.
        let mut response = b"RTSP/1.0 200 OK\r\nX-Junk: ".to_vec();
        response.extend(std::iter::repeat(b'A').take(128 * 1024));
        let (port, h) = serve_then_close(response);
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{}/test", port)).unwrap();
        let err = client.options().unwrap_err();
        assert!(
            matches!(err, RtspError::BadResponse { .. }),
            "unterminated headers must be rejected as BadResponse, got {err:?}"
        );
        h.join().unwrap();
    }

    /// DoS guard: a response declaring a body LARGER than `MAX_RTSP_BODY_BYTES`
    /// (1 MiB) must be rejected with `BadResponse` (the parser's existing body
    /// cap), never accepted. Deterministic: the server writes only the headers
    /// (a complete `CRLFCRLF`) declaring an over-cap Content-Length, then
    /// closes; `RtspResponse::parse` rejects the declared length immediately
    /// once the terminator is seen. Asserts ONLY on the error variant.
    #[test]
    fn send_and_read_rejects_oversized_declared_body() {
        let response = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
            crate::rtsp::message::MAX_RTSP_BODY_BYTES + 1
        )
        .into_bytes();
        let (port, h) = serve_then_close(response);
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{}/test", port)).unwrap();
        let err = client.options().unwrap_err();
        assert!(
            matches!(err, RtspError::BadResponse { .. }),
            "over-1-MiB declared body must be rejected as BadResponse, got {err:?}"
        );
        h.join().unwrap();
    }

    // --- Auth applied to every method (SETUP/PLAY/… not just DESCRIBE) ---

    fn md5_hex(s: &str) -> String {
        use md5::{Digest, Md5};
        let mut h = Md5::new();
        h.update(s.as_bytes());
        h.finalize().iter().fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    /// Accept one TCP connection and hold it briefly (enough for `connect`
    /// to complete). No RTSP bytes are exchanged — these tests drive the
    /// pure auth-header builder, not the network.
    fn accept_and_hold() -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                std::thread::sleep(std::time::Duration::from_millis(200));
                drop(sock);
            }
        });
        (port, h)
    }

    /// Regression guard for the SETUP-auth gap: once the DESCRIBE challenge
    /// is cached, `preemptive_authorization` must produce a correct MD5
    /// (RFC 2617 no-qop) Digest header whose HA2 hashes the **SETUP control
    /// URI** (`…/trackID=0`) — the exact shape gortsplib/MediaMTX validate.
    #[test]
    fn preemptive_authorization_builds_setup_digest_for_control_uri() {
        let (port, h) = accept_and_hold();
        let mut client =
            RtspClient::connect(&format!("rtsp://user:pass@127.0.0.1:{port}/cam")).unwrap();
        // The challenge as gortsplib/MediaMTX send it (no algorithm, no qop).
        client.cache_auth_challenge(r#"Digest realm="tstrans", nonce="abc123""#.to_string());

        let control_uri = format!("rtsp://127.0.0.1:{port}/cam/trackID=0");
        let hdr = client
            .preemptive_authorization(RtspMethod::Setup, &control_uri)
            .unwrap()
            .expect("cached challenge + URL creds → Some");

        let ha1 = md5_hex("user:tstrans:pass");
        let ha2 = md5_hex(&format!("SETUP:{control_uri}"));
        let expected = md5_hex(&format!("{ha1}:abc123:{ha2}"));
        assert!(
            hdr.contains(&format!("response=\"{expected}\"")),
            "SETUP digest must be MD5(HA1:nonce:HA2) over the control URI; got {hdr}"
        );
        assert!(hdr.contains(&format!("uri=\"{control_uri}\"")));
        assert!(hdr.contains(r#"username="user""#));
        h.join().unwrap();
    }

    /// Pre-emptive auth stays inert for unauthenticated servers: no header
    /// until a challenge has been seen, and none when the URL carries no
    /// credentials.
    #[test]
    fn preemptive_authorization_inert_without_challenge_or_creds() {
        // Creds present, but no challenge cached yet → None.
        let (p1, h1) = accept_and_hold();
        let client = RtspClient::connect(&format!("rtsp://user:pass@127.0.0.1:{p1}/cam")).unwrap();
        assert!(
            client
                .preemptive_authorization(RtspMethod::Play, "rtsp://x/cam")
                .unwrap()
                .is_none()
        );
        h1.join().unwrap();

        // Challenge cached but the URL has no credentials → None.
        let (p2, h2) = accept_and_hold();
        let mut client2 = RtspClient::connect(&format!("rtsp://127.0.0.1:{p2}/cam")).unwrap();
        client2.cache_auth_challenge(r#"Digest realm="R", nonce="n""#.to_string());
        assert!(
            client2
                .preemptive_authorization(RtspMethod::Play, "rtsp://x/cam")
                .unwrap()
                .is_none()
        );
        h2.join().unwrap();
    }

    /// Finding-2 regression: reusing one cached nonce across requests must
    /// emit a strictly increasing `nc` for `qop=auth`, and caching a *new*
    /// challenge (new nonce) restarts the count at 1 while re-caching the same
    /// challenge keeps counting.
    #[test]
    fn qop_auth_nonce_count_increments_and_resets_on_new_challenge() {
        let (port, h) = accept_and_hold();
        let mut client =
            RtspClient::connect(&format!("rtsp://user:pass@127.0.0.1:{port}/cam")).unwrap();

        let www1 = r#"Digest realm="R", nonce="n1", qop="auth""#.to_string();
        client.cache_auth_challenge(www1.clone());
        let a1 = client
            .authorization_from_challenge(RtspMethod::Describe, "rtsp://x/cam", &www1)
            .unwrap();
        let a2 = client
            .authorization_from_challenge(RtspMethod::Setup, "rtsp://x/cam/trackID=0", &www1)
            .unwrap();
        let a3 = client
            .authorization_from_challenge(RtspMethod::Play, "rtsp://x/cam", &www1)
            .unwrap();
        assert!(
            a1.contains("qop=auth") && a1.contains("nc=00000001"),
            "{a1}"
        );
        assert!(a2.contains("nc=00000002"), "{a2}");
        assert!(a3.contains("nc=00000003"), "{a3}");

        // A different challenge (new nonce) restarts nc at 1.
        let www2 = r#"Digest realm="R", nonce="n2", qop="auth""#.to_string();
        client.cache_auth_challenge(www2.clone());
        let a4 = client
            .authorization_from_challenge(RtspMethod::Setup, "rtsp://x/cam", &www2)
            .unwrap();
        assert!(
            a4.contains("nc=00000001"),
            "new nonce restarts nc; got {a4}"
        );

        // Re-caching the SAME challenge does NOT reset the count.
        client.cache_auth_challenge(r#"Digest realm="R", nonce="n2", qop="auth""#.to_string());
        let a5 = client
            .authorization_from_challenge(RtspMethod::Play, "rtsp://x/cam", &www2)
            .unwrap();
        assert!(
            a5.contains("nc=00000002"),
            "same nonce keeps counting; got {a5}"
        );
        h.join().unwrap();
    }
}
