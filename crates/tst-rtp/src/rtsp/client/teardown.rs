//! `RtspClient::teardown` + `Drop` impl (best-effort TEARDOWN on drop).

use crate::error::RtspError;
use crate::rtsp::client::RtspClient;
use crate::rtsp::message::RtspMethod;

impl RtspClient {
    /// Send TEARDOWN. After this returns, no further methods succeed.
    ///
    /// If no session is currently established (no prior successful
    /// SETUP, or [`Self::teardown`] already ran), this is a no-op that
    /// returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Io`] on socket-level failure writing the request
    ///   or reading the response.
    /// - [`RtspError::BadResponse`] (or another variant) surfaced by
    ///   the response reader if the server replies malformed bytes.
    /// - [`RtspError::Timeout`] when the response deadline
    ///   (`RtspClientBuilder::request_timeout`, default 10 s) elapses;
    ///   `session_id` is still cleared.
    pub fn teardown(&mut self) -> Result<(), RtspError> {
        // checked_add: see `send_and_read`'s copy of this idiom — a
        // `request_timeout` too large to represent as an `Instant` (e.g.
        // `Duration::MAX`) saturates to "no deadline" rather than panicking,
        // even though (with no session) this deadline is never waited on.
        let deadline = self
            .request_timeout
            .and_then(|t| std::time::Instant::now().checked_add(t));
        self.teardown_with_deadline(deadline)
    }

    /// Variant of [`Self::teardown`] with an optional response deadline.
    /// When `deadline` elapses with no TEARDOWN response, returns
    /// [`RtspError::Timeout`] but still clears `session_id` (so callers
    /// know not to retry teardown).
    ///
    /// Called from `Drop for RtspClient` with a ~500 ms deadline so the
    /// destructor stays bounded even when the peer silently half-closed
    /// (no FIN on the wire). The wire bytes still go out (write_all
    /// succeeds against the kernel send buffer); we just don't wait
    /// forever for an answer.
    pub(crate) fn teardown_with_deadline(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), RtspError> {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return Ok(()), // nothing to tear down
        };
        let uri = self.url.render_no_credentials();
        // Best-effort: a failure to build the header (e.g. an OS RNG
        // failure in the cnonce path) degrades to an unauthenticated
        // TEARDOWN rather than aborting — the wire attempt and the
        // session_id clearing below must always happen.
        let preauth = self
            .preemptive_authorization(RtspMethod::Teardown, &uri)
            .unwrap_or(None);
        let mut req = self
            .base_request(RtspMethod::Teardown, uri)
            .header("session", sid);
        if let Some(a) = &preauth {
            req = req.header("authorization", a.as_str());
        }
        let bytes = req.encode_checked()?;
        // Deadline-aware on BOTH read paths (the non-pump path used to
        // ignore `deadline`, bounding only via cancel — a silent peer
        // could stall a deadline-carrying caller that hadn't cancelled).
        let r = self.send_and_read_with_deadline(&bytes, deadline);
        // Always clear session_id — caller asked us to tear down; if it
        // failed, retrying won't help and we want Drop to skip on a
        // second pass.
        self.session_id = None;
        r.map(|_| ())
    }
}

// `Drop for RtspClient` is implemented in `client/mod.rs` (combines
// teardown + keepalive-thread join). This file holds only the
// teardown method itself.

#[cfg(test)]
mod tests {
    use crate::error::RtspError;
    use crate::rtsp::client::RtspClient;
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// A silent half-open peer: reads the TEARDOWN request, never
    /// responds, and holds the socket open until the test signals done
    /// (so the client sees neither a response nor an EOF — the exact
    /// shape `teardown_with_deadline`'s deadline exists to bound).
    #[test]
    fn teardown_with_deadline_bounds_nonpump_silent_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let mut chunk = [0u8; 4096];
                let _ = sock.read(&mut chunk); // consume the TEARDOWN
                // Hold the connection open (no reply, no FIN) until the
                // client side has finished asserting.
                let _ = done_rx.recv_timeout(Duration::from_secs(10));
            }
        });

        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        // Simulate an established session without a wire SETUP exchange —
        // teardown only requires `session_id` to be present, and this
        // test targets the read path, not session establishment.
        client.session_id = Some("silent-peer-test".into());

        let deadline = Instant::now() + Duration::from_millis(300);
        let t0 = Instant::now();
        let r = client.teardown_with_deadline(Some(deadline));
        let elapsed = t0.elapsed();

        assert!(
            matches!(r, Err(RtspError::Timeout)),
            "expected Timeout, got {r:?}"
        );
        // Lower bound: the deadline was actually honored as a wait, not
        // an instant bail. Upper bound: generous CI slack, but far below
        // the pre-fix forever-hang this regression test pins down.
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5),
            "teardown returned in {elapsed:?}, outside the deadline envelope"
        );
        // Documented contract: session_id clears even on timeout so
        // callers (and Drop) know not to retry.
        assert!(client.session_id.is_none());

        let _ = done_tx.send(());
        let _ = server.join();
    }
}
