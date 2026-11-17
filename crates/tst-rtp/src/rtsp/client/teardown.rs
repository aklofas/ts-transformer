//! `RtspClient::teardown` and `RtspClient::teardown_within`.

use crate::error::RtspError;
use crate::rtsp::client::RtspClient;
use crate::rtsp::client::engine::{RequestSpec, teardown_accepted};
use crate::rtsp::message::RtspMethod;

impl RtspClient {
    /// Send TEARDOWN. After this returns, no further methods succeed.
    ///
    /// If no session is currently established (no prior successful
    /// SETUP, or [`Self::teardown`] already ran), this is a no-op that
    /// returns `Ok(())`. A `2xx` reply, or `454 Session Not Found` (the
    /// session is already gone), is `Ok(())`. The session is cleared on
    /// every outcome, so a refused TEARDOWN is not retried.
    ///
    /// # Errors
    ///
    /// - [`RtspError::Protocol`] carrying the status when the server
    ///   answers anything but a `2xx` or `454`.
    /// - [`RtspError::AuthFailed`] when the server answers `401` and the
    ///   client has no credentials, or answers `401` again to the one
    ///   retry with credentials; [`RtspError::BadResponse`] for a `401`
    ///   without a `WWW-Authenticate` header.
    /// - [`RtspError::Io`] on socket-level failure writing the request
    ///   or reading the response.
    /// - [`RtspError::BadResponse`] (or another variant) surfaced by
    ///   the response reader if the server replies malformed bytes.
    /// - [`RtspError::Timeout`] when the response deadline
    ///   (`RtspClientBuilder::request_timeout`, default 10 s) elapses;
    ///   `session_id` is still cleared.
    /// - [`RtspError::LocalCancel`] when the cancel handle fires before
    ///   the reply arrives, or is set when the exchange fails. The
    ///   request is written even on a cancelled client.
    pub fn teardown(&mut self) -> Result<(), RtspError> {
        // checked_add: see `exchange`'s copy of this idiom — a
        // `request_timeout` too large to represent as an `Instant` (e.g.
        // `Duration::MAX`) saturates to "no deadline" rather than panicking,
        // even though (with no session) this deadline is never waited on.
        let deadline = self
            .request_timeout
            .and_then(|t| std::time::Instant::now().checked_add(t));
        self.teardown_with_deadline(deadline, None)
    }

    /// Send TEARDOWN with one deadline, `bound` from now, over getting the
    /// request to the wire and its reply. Unlike [`Self::teardown`], the
    /// request timeout does not apply. [`Self::close`] uses it with a
    /// 500 ms bound. Dropping the client uses the same bound after setting
    /// the cancel, so it waits for the write and not for the reply.
    ///
    /// This does not set the cancel flag: the reply is awaited up to the
    /// bound. A cancel already set, or set during the call, ends the
    /// reply wait at once with [`RtspError::LocalCancel`] and turns any
    /// failure of the exchange into it; the request is still written. If
    /// no session is established, this returns `Ok(())` without I/O. The
    /// reply's status is judged as [`Self::teardown`] judges it.
    /// `session_id` is cleared on every outcome, so no later teardown
    /// (nor `Drop`) retries.
    ///
    /// Bytes the peer takes none of for 5 s end the control connection,
    /// so with a `bound` over 5 s such a stall is [`RtspError::Io`].
    ///
    /// # Errors
    ///
    /// - [`RtspError::Timeout`] when the bound passes before every byte of
    ///   the request is with the kernel (the rest stays queued and may
    ///   still go out) or while waiting for the reply.
    /// - [`RtspError::LocalCancel`] when the cancel handle fires before
    ///   the reply arrives, or is set when the exchange fails (also when
    ///   the bound passes).
    /// - [`RtspError::Io`] on another socket failure, and
    ///   [`RtspError::BadResponse`] (or another variant) for a malformed
    ///   reply.
    /// - [`RtspError::Protocol`] and [`RtspError::AuthFailed`] for a
    ///   refused TEARDOWN, as for [`Self::teardown`].
    pub fn teardown_within(&mut self, bound: std::time::Duration) -> Result<(), RtspError> {
        // A bound too large to represent as an `Instant` saturates to "no
        // deadline", as in `teardown`.
        let deadline = std::time::Instant::now().checked_add(bound);
        self.teardown_with_deadline(deadline, deadline)
    }

    /// Variant of [`Self::teardown`] with an optional response deadline.
    /// When `deadline` elapses with no TEARDOWN response, returns
    /// [`RtspError::Timeout`] but still clears `session_id` (so callers
    /// know not to retry teardown). `send_deadline` also bounds getting
    /// the request to the wire ([`Self::request`]).
    pub(crate) fn teardown_with_deadline(
        &mut self,
        deadline: Option<std::time::Instant>,
        send_deadline: Option<std::time::Instant>,
    ) -> Result<(), RtspError> {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return Ok(()), // nothing to tear down
        };
        let spec = RequestSpec {
            method: RtspMethod::Teardown,
            uri: self.url.render_no_credentials(),
            headers: vec![("session", sid)],
        };
        let r = self.request(spec, deadline, send_deadline);
        // Always clear session_id — caller asked us to tear down; if it
        // failed, retrying won't help and we want Drop to skip on a
        // second pass.
        self.session_id = None;
        let resp = r?;
        if teardown_accepted(resp.status) {
            Ok(())
        } else {
            Err(RtspError::Protocol {
                code: resp.status,
                reason: resp.reason,
            })
        }
    }
}

// `Drop for RtspClient` is implemented in `client/mod.rs`. This file
// holds only the teardown methods themselves.

#[cfg(test)]
mod tests {
    use crate::error::RtspError;
    use crate::rtsp::client::RtspClient;
    #[cfg(target_os = "linux")]
    use crate::rtsp::client::test_sockets::non_reading_peer;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[cfg(target_os = "linux")]
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    #[cfg(target_os = "linux")]
    use tst_test_helpers::timing::scaled;

    /// A silent half-open peer: reads the TEARDOWN request, never
    /// responds, and holds the socket open until the test signals done
    /// (so the client sees neither a response nor an EOF — the exact
    /// shape `teardown_with_deadline`'s deadline exists to bound).
    #[test]
    fn teardown_with_deadline_bounds_a_silent_peer() {
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
        let r = client.teardown_with_deadline(Some(deadline), None);
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

    /// A client whose control socket's send buffer is pinned to 4 KiB (no
    /// autotuning, which could otherwise take a whole 1 MiB request), on a
    /// peer behind a 4 KiB receive buffer that never reads, with a session
    /// open (interleaved when `interleaved`) and 1 MiB queued ahead: a
    /// request with a 1 MiB header and a 50 ms send deadline, which times
    /// out with its bytes still queued. Linux only, for the reason given in
    /// `test_sockets`.
    #[cfg(target_os = "linux")]
    fn client_with_bytes_the_peer_does_not_take(
        interleaved: bool,
    ) -> (RtspClient, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        use crate::rtsp::client::driver::{Command, RtcpSink};
        use crate::rtsp::client::engine::{InterleavedChannels, RequestSpec};
        use crate::rtsp::message::RtspMethod;

        let (port, done_tx, peer) = non_reading_peer();
        let url = crate::url::RtspUrl::parse(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let (probe, socket) = mpsc::channel();
        let mut client = RtspClient::connect_with_params(
            &url,
            None,
            crate::rtsp::client::ConnectParams {
                socket_probe: Some(probe),
                ..Default::default()
            },
        )
        .unwrap();
        socket2::SockRef::from(&socket.recv().unwrap())
            .set_send_buffer_size(4096)
            .unwrap();
        if interleaved {
            let (data_tx, _data_rx) = mpsc::sync_channel(8);
            client
                .driver
                .send(Command::SetInterleaved {
                    channels: InterleavedChannels { rtp: 0, rtcp: 1 },
                    data_tx,
                    rtcp: RtcpSink {
                        stats: Default::default(),
                        ssrc: 1,
                    },
                })
                .unwrap();
        }
        client.session_id = Some("blocked".into());
        let ahead = Instant::now() + Duration::from_millis(50);
        let r = client.request(
            RequestSpec {
                method: RtspMethod::Options,
                uri: url.render_no_credentials(),
                headers: vec![("x-pad", "a".repeat(1 << 20))],
            },
            Some(ahead),
            Some(ahead),
        );
        assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
        (client, done_tx, peer)
    }

    /// The TEARDOWN queues behind bytes the peer does not take: the bound
    /// ends the call as a `Timeout` (or `LocalCancel` with the cancel set
    /// first), well before the 5 s write-stall bound would end the stream.
    #[cfg(target_os = "linux")]
    fn teardown_within_is_bounded(interleaved: bool, cancel: bool) {
        let (mut client, done_tx, peer) = client_with_bytes_the_peer_does_not_take(interleaved);
        if cancel {
            client.cancel_handle().cancel();
        }

        let t0 = Instant::now();
        let r = client.teardown_within(Duration::from_millis(500));
        let elapsed = t0.elapsed();

        if cancel {
            assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
        } else {
            assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
        }
        assert!(
            elapsed >= Duration::from_millis(500),
            "returned in {elapsed:?}, before the bound"
        );
        assert!(
            elapsed < scaled(Duration::from_secs(2)),
            "teardown_within took {elapsed:?}"
        );
        assert!(client.session_id.is_none());

        let _ = done_tx.send(());
        drop(client);
        peer.join().unwrap();
    }

    /// See `client_with_bytes_the_peer_does_not_take` for why this is Linux
    /// only.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_is_bounded_when_the_peer_takes_no_bytes() {
        teardown_within_is_bounded(false, false);
    }

    /// See `client_with_bytes_the_peer_does_not_take`.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_is_bounded_when_the_peer_takes_no_bytes_on_an_interleaved_session() {
        teardown_within_is_bounded(true, false);
    }

    /// See `client_with_bytes_the_peer_does_not_take`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_blocked_teardown_on_a_cancelled_client_reports_the_cancel() {
        teardown_within_is_bounded(false, true);
    }

    /// See `client_with_bytes_the_peer_does_not_take`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_blocked_teardown_on_a_cancelled_interleaved_client_reports_the_cancel() {
        teardown_within_is_bounded(true, true);
    }

    /// `Drop` sets the cancel and then writes TEARDOWN: the peer sees the
    /// cancel already set when the TEARDOWN arrives, and Drop waits for the
    /// write, not for a reply that never comes (it returns well inside its
    /// 500 ms TEARDOWN bound).
    #[test]
    fn drop_sets_the_cancel_before_its_teardown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<crate::RtspCancelHandle>();
        let peer = std::thread::spawn(move || {
            let cancel = cancel_rx.recv().unwrap();
            let (mut sock, _) = listener.accept().unwrap();
            // Never replies; reads until the TEARDOWN head is in.
            sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            while !String::from_utf8_lossy(&seen).contains("TEARDOWN ") {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
            }
            let cancelled_at_teardown = cancel.is_cancelled();
            // Hold the connection until the client hangs up.
            let _ = sock.read_to_end(&mut Vec::new());
            Some(cancelled_at_teardown)
        });

        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        client.session_id = Some("silent-drop".into());
        cancel_tx.send(client.cancel_handle()).unwrap();
        let t0 = Instant::now();
        drop(client);
        let elapsed = t0.elapsed();

        assert_eq!(
            peer.join().unwrap(),
            Some(true),
            "the TEARDOWN must arrive (Some) with the cancel already set (true)"
        );
        assert!(
            elapsed < tst_test_helpers::timing::scaled(Duration::from_millis(400)),
            "Drop waited {elapsed:?}: it waited for the TEARDOWN reply"
        );
    }

    /// A cancelled client still writes its TEARDOWN when dropped: nothing
    /// on the request path refuses a request because the cancel flag is
    /// set, and `Drop` relies on that.
    #[test]
    fn a_cancelled_client_still_sends_teardown_on_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let mut seen = Vec::new();
            if let Ok((mut sock, _)) = listener.accept() {
                // Bounds a client that never hangs up; not reached by a
                // passing test.
                sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let _ = sock.read_to_end(&mut seen);
            }
            String::from_utf8_lossy(&seen).into_owned()
        });

        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        client.session_id = Some("cancelled-drop".into());
        client.cancel_handle().cancel();
        drop(client);

        let seen = peer.join().unwrap();
        assert!(
            seen.lines().any(|l| l.starts_with("TEARDOWN ")),
            "the peer saw no TEARDOWN: {seen:?}"
        );
    }

    /// A peer that answers: `teardown_within(bound)` returns `Ok` and
    /// clears the session.
    fn teardown_within_a_healthy_peer(bound: Duration) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
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
            let head = String::from_utf8_lossy(&head).into_owned();
            let cseq = head
                .lines()
                .find_map(|l| l.strip_prefix("CSeq: "))
                .unwrap_or("1")
                .to_owned();
            let reply = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: healthy\r\n\r\n");
            let _ = sock.write_all(reply.as_bytes());
            // Hold the connection until the client hangs up.
            let _ = sock.read_to_end(&mut Vec::new());
        });

        let url = crate::url::RtspUrl::parse(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let configured = Duration::from_secs(2);
        let mut client = RtspClient::connect_with_params(
            &url,
            None,
            crate::rtsp::client::ConnectParams {
                read_timeout: configured,
                ..Default::default()
            },
        )
        .unwrap();
        client.session_id = Some("healthy".into());

        let r = client.teardown_within(bound);
        assert!(r.is_ok(), "got {r:?}");
        assert!(client.session_id.is_none());

        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn teardown_within_a_healthy_peer_returns_ok() {
        teardown_within_a_healthy_peer(Duration::from_millis(500));
    }

    /// A bound too large for an `Instant` does not panic: it is no deadline.
    #[test]
    fn teardown_within_duration_max_is_no_deadline() {
        teardown_within_a_healthy_peer(Duration::MAX);
    }

    /// `teardown_within` is ONE deadline over the write and the reply: the
    /// peer takes no bytes for half the bound (64 KiB is queued ahead of
    /// the TEARDOWN), then reads everything and never replies. The TEARDOWN
    /// reaches the peer inside the bound (`scaled(1 s)`), so the reply wait
    /// gets only what is left: the call ends at the bound, not a bound after
    /// the write. Linux only, for the reason given in `test_sockets`.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_is_one_deadline_over_the_write_and_the_reply() {
        use crate::rtsp::client::engine::RequestSpec;
        use crate::rtsp::message::RtspMethod;
        use socket2::{Domain, Socket, Type};

        // Proportional at every scale: a reply wait with a bound of its own
        // would end at least `peer_pause + bound` in, past `bound + slack`.
        let bound = scaled(Duration::from_secs(1));
        let peer_pause = bound / 2;
        let slack = bound / 4;

        let sock = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        sock.bind(
            &"127.0.0.1:0"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        )
        .unwrap();
        sock.listen(1).unwrap();
        let listener: TcpListener = sock.into();
        let port = listener.local_addr().unwrap().port();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (seen_tx, seen_rx) = mpsc::channel::<Instant>();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            go_rx.recv().unwrap();
            // Pacing, not a sync: the bytes queued ahead wait this long.
            std::thread::sleep(peer_pause);
            sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let mut tail = Vec::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => tail.extend_from_slice(&buf[..n]),
                }
                if String::from_utf8_lossy(&tail).contains("TEARDOWN ") {
                    let _ = seen_tx.send(Instant::now());
                    break;
                }
                // Keep only what could start a TEARDOWN split across reads.
                let keep = tail.len().saturating_sub(16);
                tail.drain(..keep);
            }
            // Never replies; holds the connection until the client hangs up.
            let _ = sock.read_to_end(&mut Vec::new());
        });

        let url = crate::url::RtspUrl::parse(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let (probe, socket) = mpsc::channel();
        let mut client = RtspClient::connect_with_params(
            &url,
            None,
            crate::rtsp::client::ConnectParams {
                socket_probe: Some(probe),
                ..Default::default()
            },
        )
        .unwrap();
        socket2::SockRef::from(&socket.recv().unwrap())
            .set_send_buffer_size(4096)
            .unwrap();
        client.session_id = Some("one-deadline".into());
        let ahead = Instant::now() + Duration::from_millis(50);
        let r = client.request(
            RequestSpec {
                method: RtspMethod::Options,
                uri: url.render_no_credentials(),
                headers: vec![("x-pad", "a".repeat(64 * 1024))],
            },
            Some(ahead),
            Some(ahead),
        );
        assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");

        go_tx.send(()).unwrap();
        let t0 = Instant::now();
        let r = client.teardown_within(bound);
        let returned = Instant::now();
        let elapsed = returned - t0;

        assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
        let seen = seen_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the TEARDOWN reached the peer");
        assert!(
            seen < returned,
            "the TEARDOWN reached the peer only {:?} after the call returned: the write did not finish inside the bound",
            seen - returned
        );
        assert!(
            elapsed >= bound,
            "returned in {elapsed:?}, before the bound"
        );
        assert!(
            elapsed < bound + slack,
            "returned in {elapsed:?}: the reply wait got a bound of its own after the write \
             (the TEARDOWN reached the peer {:?} after the call began)",
            seen - t0
        );

        drop(client);
        peer.join().unwrap();
    }
}
