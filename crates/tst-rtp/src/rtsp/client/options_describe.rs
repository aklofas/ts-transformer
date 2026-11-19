//! `RtspClient::options` + `RtspClient::describe`, and the request path
//! every method shares: hand the request to the client's thread, wait for
//! it to be written, then for its reply.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use crate::error::RtspError;
use crate::rtsp::client::driver::{Command, Progress};
use crate::rtsp::client::engine::RequestSpec;
use crate::rtsp::client::{REQUEST_POLL_TICK, RtspClient};
use crate::rtsp::message::{RtspMethod, RtspResponse};
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
    /// Check that `resp` carries a 200 OK status.
    ///
    /// Returns [`RtspError::Protocol`] for any other status.
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
    /// list of methods the server supports. Authenticates like every
    /// method (pre-emptive once a challenge is cached, and one retry after
    /// a 401) — RFC 7826 servers may challenge any method, OPTIONS
    /// included.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Io`] on socket-level failure.
    /// - [`RtspError::BadResponse`] on malformed response bytes.
    /// - [`RtspError::AuthFailed`] if the server rejects the credentials.
    /// - [`RtspError::Protocol`] on any other non-200 status.
    /// - [`RtspError::LocalCancel`] if the cancel handle was triggered
    ///   mid-read, or was set when the exchange failed.
    pub fn options(&mut self) -> Result<OptionsResponse, RtspError> {
        let uri = self.url.render_no_credentials();
        let mut headers = Vec::new();
        if let Some(sid) = &self.session_id {
            headers.push(("session", sid.clone()));
        }
        let resp = self.exchange(RtspMethod::Options, uri, headers)?;
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

    /// Send a DESCRIBE request and parse the SDP body. Authenticates like
    /// every method (pre-emptive once a challenge is cached, and one retry
    /// after a 401).
    ///
    /// # Errors
    ///
    /// - [`RtspError::Io`] on socket-level failure.
    /// - [`RtspError::BadResponse`] on malformed response bytes.
    /// - [`RtspError::AuthFailed`] if the server rejects the credentials.
    /// - [`RtspError::Protocol`] on any other non-200 status.
    /// - [`RtspError::BadSdp`] if the response body isn't parseable SDP.
    /// - [`RtspError::LocalCancel`] if the cancel handle was triggered
    ///   mid-read, or was set when the exchange failed.
    pub fn describe(&mut self) -> Result<Sdp, RtspError> {
        let uri = self.url.render_no_credentials();
        let mut headers = vec![("accept", "application/sdp".to_string())];
        if let Some(sid) = &self.session_id {
            headers.push(("session", sid.clone()));
        }
        let resp = self.exchange(RtspMethod::Describe, uri, headers)?;
        if resp.status != 200 {
            return Err(RtspError::Protocol {
                code: resp.status,
                reason: resp.reason,
            });
        }
        Sdp::parse(&resp.body)
    }

    /// `method` at `uri` with `headers`, under the request timeout
    /// ([`crate::RtspClientBuilder::request_timeout`]). It is the reply
    /// deadline, which also ends the wait for the write; there is no
    /// separate send deadline.
    pub(crate) fn exchange(
        &mut self,
        method: RtspMethod,
        uri: String,
        headers: Vec<(&'static str, String)>,
    ) -> Result<RtspResponse, RtspError> {
        // A timeout too large to form an `Instant` (e.g. `Duration::MAX`)
        // means no deadline rather than a panic.
        let deadline = self
            .request_timeout
            .and_then(|t| Instant::now().checked_add(t));
        self.request(
            RequestSpec {
                method,
                uri,
                headers,
                body: None,
            },
            deadline,
            None,
        )
    }

    /// Hand `spec` to the client's thread and wait, first for the request
    /// to be written (up to `send_deadline`; without one the thread's own
    /// 5 s write-stall bound applies), then for its reply (up to
    /// `reply_deadline`). Expiry of either is [`RtspError::Timeout`]. The
    /// reply deadline also ends the wait for the write: the client's
    /// thread times the request out at it, written or not.
    ///
    /// The cancel flag is not checked before the write: a cancelled
    /// client still writes its request (`Drop` relies on this to send
    /// TEARDOWN). The reply wait checks the cancel before it takes a reply,
    /// at the start and every [`REQUEST_POLL_TICK`]: once it sees the
    /// cancel set it ends with [`RtspError::LocalCancel`], even when the
    /// reply is already delivered. A reply taken before that is returned
    /// whatever the flag says; an I/O error or a passed deadline while the
    /// cancel is set is reported as `LocalCancel` ([`report_cancel`]). With
    /// the client's thread gone, or its command queue full, the result is
    /// `Io(UnexpectedEof)`.
    pub(crate) fn request(
        &mut self,
        spec: RequestSpec,
        reply_deadline: Option<Instant>,
        send_deadline: Option<Instant>,
    ) -> Result<RtspResponse, RtspError> {
        let (progress, rx) = mpsc::channel();
        let r = self
            .driver
            .send(Command::Request {
                spec,
                reply_deadline,
                progress,
            })
            .and_then(|()| self.await_reply(&rx, reply_deadline, send_deadline));
        report_cancel(r, self.cancel.is_cancelled())
    }

    fn await_reply(
        &self,
        rx: &mpsc::Receiver<Progress>,
        reply_deadline: Option<Instant>,
        send_deadline: Option<Instant>,
    ) -> Result<RtspResponse, RtspError> {
        // The write. A `Done` can come with no `Written` before it (an
        // early reply, a failure before the write): it ends the request.
        let first = match send_deadline {
            Some(d) => rx
                .recv_timeout(d.saturating_duration_since(Instant::now()))
                .map_err(|e| match e {
                    RecvTimeoutError::Timeout => RtspError::Timeout,
                    RecvTimeoutError::Disconnected => thread_gone(),
                })?,
            None => rx.recv().map_err(|_| thread_gone())?,
        };
        if let Progress::Done(r) = first {
            return r;
        }
        // The reply.
        loop {
            if self.cancel.is_cancelled() {
                return Err(RtspError::LocalCancel);
            }
            let mut tick = REQUEST_POLL_TICK;
            if let Some(d) = reply_deadline {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(RtspError::Timeout);
                }
                tick = tick.min(left);
            }
            match rx.recv_timeout(tick) {
                Ok(Progress::Done(r)) => return r,
                Ok(Progress::Written) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(thread_gone()),
            }
        }
    }
}

/// What a request reports given whether the cancel is set when it ends:
/// an I/O error or a passed deadline is then the cancel; anything else, a
/// reply above all, is kept.
fn report_cancel(
    r: Result<RtspResponse, RtspError>,
    cancelled: bool,
) -> Result<RtspResponse, RtspError> {
    match r {
        Err(RtspError::Io(_) | RtspError::Timeout) if cancelled => Err(RtspError::LocalCancel),
        r => r,
    }
}

/// A request whose progress channel closed: the client's thread ended.
fn thread_gone() -> RtspError {
    RtspError::Io(std::io::ErrorKind::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
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

    // --- Bounded client response accumulation (adversarial + positive) ---

    /// POSITIVE regression guard against false-rejecting valid large
    /// bodies. A legitimate DESCRIBE-style response with a body well above the
    /// 64 KiB accumulation cap but ≤ `MAX_RTSP_BODY_BYTES` (1 MiB) — e.g. a
    /// large SDP with many media sections — must be ACCEPTED and fully parsed,
    /// not rejected at 64 KiB. Guards against the body-unaware cap ever
    /// returning. Deterministic: the server writes the COMPLETE response, then
    /// closes — no fill/timeout race.
    #[test]
    fn a_reply_body_above_the_header_cap_is_accepted() {
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
        // It proves the >64 KiB body is accumulated and parsed rather than
        // rejected.
        let opts = client.options().expect("valid ≤1 MiB body must parse OK");
        // Reaching Ok proves the client accumulated past the 64 KiB header
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
    fn unterminated_reply_headers_are_rejected() {
        // 128 KiB of header-junk, never a CRLFCRLF.
        let mut response = b"RTSP/1.0 200 OK\r\nX-Junk: ".to_vec();
        response.extend(std::iter::repeat_n(b'A', 128 * 1024));
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
    fn an_oversized_declared_reply_body_is_rejected() {
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

    /// A response whose header block is not UTF-8 is a `BadResponse` carrying
    /// exactly this detail.
    #[test]
    fn non_utf8_reply_headers_are_rejected_with_their_own_detail() {
        let (port, h) = serve_then_close(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nX: \xff\r\n\r\n".to_vec());
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{}/test", port)).unwrap();
        let err = client.options().unwrap_err();
        assert!(
            matches!(
                err,
                RtspError::BadResponse {
                    detail: "non-UTF8 RTSP headers"
                }
            ),
            "{err:?}"
        );
        h.join().unwrap();
    }

    /// A peer that accepts the connection, waits for the go signal (sent
    /// once `connect` has returned, so the reset cannot land while the client
    /// is still setting its socket up), then resets it (`SO_LINGER` 0, then
    /// close). It resets anyway after 10 s, so a failed test cannot leave
    /// the thread parked.
    #[cfg(target_os = "linux")]
    fn resetting_peer() -> (
        u16,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                let _ = go_rx.recv_timeout(Duration::from_secs(10));
                socket2::SockRef::from(&sock)
                    .set_linger(Some(Duration::ZERO))
                    .unwrap();
                drop(sock);
            }
        });
        (port, go_tx, h)
    }

    /// A request on a connection the peer has reset: `LocalCancel` when
    /// the client was cancelled before the request, the reset's own `Io`
    /// error otherwise. The client's thread reads the reset before the
    /// request is made, so the request meets an ended stream and nothing
    /// is written. Linux only: the reset-then-request sequence has not
    /// been measured on macOS or Windows.
    #[cfg(target_os = "linux")]
    fn request_after_a_reset(cancel: bool) -> Result<OptionsResponse, RtspError> {
        let (port, go, peer) = resetting_peer();
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        go.send(()).unwrap();
        peer.join().unwrap();
        let give_up = Instant::now() + Duration::from_secs(10);
        while client.is_session_alive() {
            assert!(Instant::now() < give_up, "the reset never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        if cancel {
            client.cancel_handle().cancel();
        }
        client.options()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_request_after_a_reset_on_a_cancelled_client_reports_the_cancel() {
        let r = request_after_a_reset(true);
        assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_request_after_a_reset_without_a_cancel_is_io() {
        let r = request_after_a_reset(false);
        assert!(matches!(r, Err(RtspError::Io(_))), "got {r:?}");
    }

    /// A peer that reads one request head, runs `end`, then closes the
    /// connection.
    fn peer_ending_after_request(
        end: impl FnOnce() + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => head.extend_from_slice(&chunk[..n]),
                }
            }
            end();
            drop(sock);
        });
        (port, h)
    }

    /// The request is written, then the connection closes without a reply.
    /// With `cancel`, the peer fires the client's cancel after it has the
    /// request and before it closes, so the cancel lands during the wait.
    /// Whichever of the two the client sees first, a cancelled client
    /// reports `LocalCancel`; otherwise the result is the
    /// `Io(UnexpectedEof)` it is without a cancel.
    fn ended_reply_wait(cancel: bool) -> Result<OptionsResponse, RtspError> {
        let (handle_tx, handle_rx) =
            std::sync::mpsc::channel::<crate::rtsp::client::RtspCancelHandle>();
        let (port, peer) = peer_ending_after_request(move || {
            if let Ok(h) = handle_rx.recv_timeout(Duration::from_secs(10)) {
                h.cancel();
            }
        });
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        if cancel {
            handle_tx.send(client.cancel_handle()).unwrap();
        }
        drop(handle_tx);
        let r = client.options();
        peer.join().unwrap();
        r
    }

    #[test]
    fn an_eof_on_a_cancelled_client_reports_the_cancel() {
        let r = ended_reply_wait(true);
        assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
    }

    #[test]
    fn an_eof_without_a_cancel_is_io() {
        let r = ended_reply_wait(false);
        assert!(
            matches!(r, Err(RtspError::Io(std::io::ErrorKind::UnexpectedEof))),
            "got {r:?}"
        );
    }

    /// A request on a client whose thread has ended: `Io(UnexpectedEof)`,
    /// or `LocalCancel` with the cancel set.
    fn request_on_an_ended_driver(cancel: bool) -> Result<OptionsResponse, RtspError> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            // Hold the connection until the client hangs up.
            let _ = sock.read_to_end(&mut Vec::new());
        });
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        client.driver.stop();
        assert!(client.driver.join_within(Duration::from_secs(10)));
        if cancel {
            client.cancel_handle().cancel();
        }
        let r = client.options();
        drop(client);
        peer.join().unwrap();
        r
    }

    #[test]
    fn a_request_on_an_ended_driver_reports_the_cancel() {
        let r = request_on_an_ended_driver(true);
        assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
    }

    #[test]
    fn a_request_on_an_ended_driver_is_io() {
        let r = request_on_an_ended_driver(false);
        assert!(
            matches!(r, Err(RtspError::Io(std::io::ErrorKind::UnexpectedEof))),
            "got {r:?}"
        );
    }

    fn ok_reply() -> RtspResponse {
        RtspResponse::parse(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n")
            .unwrap()
            .0
    }

    #[test]
    fn a_reply_with_the_cancel_set_is_the_reply() {
        let r = report_cancel(Ok(ok_reply()), true);
        assert!(matches!(&r, Ok(resp) if resp.status == 200), "got {r:?}");
    }

    #[test]
    fn an_io_error_with_the_cancel_set_is_the_cancel() {
        let r = report_cancel(
            Err(RtspError::Io(std::io::ErrorKind::ConnectionReset)),
            true,
        );
        assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
    }

    #[test]
    fn a_timeout_with_the_cancel_set_is_the_cancel() {
        let r = report_cancel(Err(RtspError::Timeout), true);
        assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
    }

    #[test]
    fn without_the_cancel_an_io_error_and_a_timeout_are_kept() {
        let r = report_cancel(
            Err(RtspError::Io(std::io::ErrorKind::ConnectionReset)),
            false,
        );
        assert!(
            matches!(r, Err(RtspError::Io(std::io::ErrorKind::ConnectionReset))),
            "got {r:?}"
        );
        let r = report_cancel(Err(RtspError::Timeout), false);
        assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
        let r = report_cancel(Ok(ok_reply()), false);
        assert!(r.is_ok(), "got {r:?}");
    }

    #[test]
    fn another_error_with_the_cancel_set_is_kept() {
        let r = report_cancel(Err(RtspError::BadResponse { detail: "x" }), true);
        assert!(
            matches!(r, Err(RtspError::BadResponse { detail: "x" })),
            "got {r:?}"
        );
        let r = report_cancel(Err(RtspError::AuthFailed), true);
        assert!(matches!(r, Err(RtspError::AuthFailed)), "got {r:?}");
    }
}
