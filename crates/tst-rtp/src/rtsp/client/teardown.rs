//! `RtspClient::teardown` and `RtspClient::teardown_within`.

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
        self.teardown_with_deadline(deadline, None)
    }

    /// Send TEARDOWN with one deadline, `bound` from now, over the whole
    /// exchange: the wait for the control connection (another thread,
    /// such as the keepalive pinger, may be using it), the request write
    /// and the reply. Unlike [`Self::teardown`], the request timeout does
    /// not apply.
    ///
    /// This does not set the cancel flag: the reply is awaited up to the
    /// bound. A cancel already set, or set during the call, ends the
    /// reply wait at once with [`RtspError::LocalCancel`]; the request is
    /// still written. If no session is established, this returns `Ok(())`
    /// without I/O. `session_id` is cleared on every outcome, so no later
    /// teardown (nor `Drop`) retries.
    ///
    /// A single socket write still waits at most 5 s, so with a `bound`
    /// over 5 s a write that stalls that long is [`RtspError::Io`].
    ///
    /// # Errors
    ///
    /// - [`RtspError::Timeout`] when the bound passes while waiting for
    ///   the connection (the TEARDOWN was not sent), during the write
    ///   (it may be partly sent) or while waiting for the reply.
    /// - [`RtspError::LocalCancel`] when the cancel handle fires before
    ///   the reply arrives.
    /// - [`RtspError::Io`] on another socket failure, and
    ///   [`RtspError::BadResponse`] (or another variant) for a malformed
    ///   reply.
    pub fn teardown_within(&mut self, bound: std::time::Duration) -> Result<(), RtspError> {
        // A bound too large to represent as an `Instant` saturates to "no
        // deadline", as in `teardown`.
        let deadline = std::time::Instant::now().checked_add(bound);
        self.teardown_with_deadline(deadline, deadline)
    }

    /// Variant of [`Self::teardown`] with an optional response deadline.
    /// When `deadline` elapses with no TEARDOWN response, returns
    /// [`RtspError::Timeout`] but still clears `session_id` (so callers
    /// know not to retry teardown). `send_deadline` also bounds the wait
    /// for the stream lock and the request write
    /// (`send_and_read_with_deadline`).
    pub(crate) fn teardown_with_deadline(
        &mut self,
        deadline: Option<std::time::Instant>,
        send_deadline: Option<std::time::Instant>,
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
        let r = self.send_and_read_with_deadline(&bytes, deadline, send_deadline);
        // Always clear session_id — caller asked us to tear down; if it
        // failed, retrying won't help and we want Drop to skip on a
        // second pass.
        self.session_id = None;
        r.map(|_| ())
    }
}

// `Drop for RtspClient` is implemented in `client/mod.rs` (combines
// teardown + keepalive-thread join). This file holds only the
// teardown methods themselves.

#[cfg(test)]
mod tests {
    use crate::error::RtspError;
    use crate::rtsp::client::{RtspClient, Stream, lock_unpoisoned};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[cfg(target_os = "linux")]
    use std::sync::atomic::AtomicBool;
    #[cfg(target_os = "linux")]
    use std::sync::{Arc, mpsc};
    #[cfg(target_os = "linux")]
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    #[cfg(target_os = "linux")]
    use tst_test_helpers::timing::scaled;

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

    /// The TCP socket of a plain `rtsp://` client's stream.
    fn plain_tcp(s: &Stream) -> &std::net::TcpStream {
        match s {
            Stream::Plain(tcp) => tcp,
            #[cfg(feature = "tls")]
            Stream::Tls(_) => unreachable!("plain rtsp:// connects yield Stream::Plain"),
        }
    }

    /// The socket read and write timeouts the client's control socket
    /// carries now.
    fn installed_timeouts(client: &RtspClient) -> (Option<Duration>, Option<Duration>) {
        let s = lock_unpoisoned(&client.stream);
        let tcp = plain_tcp(&s);
        (tcp.read_timeout().unwrap(), tcp.write_timeout().unwrap())
    }

    /// Stands in for an active interleaved pump: only the write gate and
    /// the control channel matter to the request path. The returned
    /// sender keeps the channel open.
    #[cfg(target_os = "linux")]
    fn stand_in_pump(client: &mut RtspClient) -> mpsc::SyncSender<bytes::Bytes> {
        let (ctrl_tx, ctrl_rx) = mpsc::sync_channel::<bytes::Bytes>(1);
        client.pump_state = Some(crate::rtsp::client::InterleavedPumpState {
            cancel: Arc::new(AtomicBool::new(false)),
            write_gate: client.write_gate.clone(),
            ctrl_rx,
            thread: None,
        });
        ctrl_tx
    }

    /// A peer behind a 4 KiB receive buffer (set before `listen`, so the
    /// accepted socket inherits it) that accepts and never reads. It
    /// drops the connection when `done` is signalled, which resets it
    /// and fails any write still blocked on it.
    #[cfg(target_os = "linux")]
    fn non_reading_peer() -> (u16, mpsc::Sender<()>, JoinHandle<()>) {
        use socket2::{Domain, Socket, Type};
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
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                let _ = done_rx.recv_timeout(Duration::from_secs(30));
                drop(sock);
            }
        });
        (port, done_tx, h)
    }

    /// Fills the client's send buffer: pins it to 4 KiB (no autotuning),
    /// writes 4 KiB chunks without blocking until the kernel refuses one,
    /// then single bytes until none has been taken for 300 ms. The quiet
    /// period outlasts the peer's delayed ACK (at most 200 ms on Linux),
    /// the last thing that frees send-buffer space: the peer never
    /// reads, so after it the buffer stays full and the next blocking
    /// write parks.
    #[cfg(target_os = "linux")]
    fn fill_send_buffer(client: &RtspClient) {
        let s = lock_unpoisoned(&client.stream);
        let tcp = plain_tcp(&s);
        socket2::SockRef::from(tcp)
            .set_send_buffer_size(4096)
            .unwrap();
        tcp.set_nonblocking(true).unwrap();
        let chunk = [0u8; 4096];
        let give_up = Instant::now() + Duration::from_secs(10);
        let would_block = |r: std::io::Result<usize>| match r {
            Ok(_) => false,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) => panic!("filling the send buffer: {e}"),
        };
        while !would_block((&*tcp).write(&chunk)) {
            assert!(Instant::now() < give_up, "the send buffer never filled");
        }
        let mut last_taken = Instant::now();
        while last_taken.elapsed() < Duration::from_millis(300) {
            assert!(
                Instant::now() < give_up,
                "the send buffer never stayed full"
            );
            if would_block((&*tcp).write(&chunk[..1])) {
                std::thread::sleep(Duration::from_millis(10));
            } else {
                last_taken = Instant::now();
            }
        }
        tcp.set_nonblocking(false).unwrap();
    }

    /// The request write blocks on a full send buffer: the bound ends it
    /// as a `Timeout`, well before the socket's own 5 s write timeout
    /// would end it as `Io`. Linux only: the fill latch is tuned to
    /// Linux TCP (a pinned 4 KiB send buffer, and a delayed ACK of at
    /// most 200 ms as the last thing that frees space) and has not been
    /// measured on macOS or Windows.
    #[cfg(target_os = "linux")]
    fn blocked_write_is_bounded(pump: bool) {
        let (port, done_tx, peer) = non_reading_peer();
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let _ctrl = pump.then(|| stand_in_pump(&mut client));
        client.session_id = Some("blocked-write".into());
        fill_send_buffer(&client);

        let t0 = Instant::now();
        let r = client.teardown_within(Duration::from_millis(500));
        let elapsed = t0.elapsed();

        assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
        assert!(
            elapsed < scaled(Duration::from_secs(2)),
            "teardown_within took {elapsed:?}"
        );
        assert!(client.session_id.is_none());
        assert_eq!(installed_timeouts(&client).1, Some(Duration::from_secs(5)));

        let _ = done_tx.send(());
        drop(client);
        peer.join().unwrap();
    }

    /// See [`blocked_write_is_bounded`], which says why this is Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_bounds_a_blocked_write() {
        blocked_write_is_bounded(false);
    }

    /// See [`blocked_write_is_bounded`], which says why this is Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_bounds_a_blocked_write_in_pump_mode() {
        blocked_write_is_bounded(true);
    }

    /// Another thread holds the stream lock while blocked in a write on a
    /// full send buffer (a keepalive ping's shape): the bound ends the
    /// lock wait as a `Timeout` instead of waiting out the holder's 5 s
    /// write timeout. Linux only, for the reason given on
    /// [`blocked_write_is_bounded`].
    #[cfg(target_os = "linux")]
    fn held_lock_is_bounded(pump: bool) {
        let (port, done_tx, peer) = non_reading_peer();
        let mut client = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
        let _ctrl = pump.then(|| stand_in_pump(&mut client));
        client.session_id = Some("held-lock".into());
        fill_send_buffer(&client);

        let stream = Arc::clone(&client.stream);
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let mut s = lock_unpoisoned(&stream);
            let _ = held_tx.send(());
            let _ = s.write_all(b"GET_PARAMETER rtsp://127.0.0.1/test RTSP/1.0\r\n\r\n");
        });
        held_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the holder never took the stream lock");

        let t0 = Instant::now();
        let r = client.teardown_within(Duration::from_millis(500));
        let elapsed = t0.elapsed();

        assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
        assert!(
            elapsed < scaled(Duration::from_secs(2)),
            "teardown_within took {elapsed:?}"
        );
        assert!(client.session_id.is_none());

        // Resetting the connection fails the holder's write at once.
        let _ = done_tx.send(());
        holder.join().unwrap();
        drop(client);
        peer.join().unwrap();
    }

    /// See [`held_lock_is_bounded`], which says why this is Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_bounds_a_held_stream_lock() {
        held_lock_is_bounded(false);
    }

    /// See [`held_lock_is_bounded`], which says why this is Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn teardown_within_bounds_a_held_stream_lock_in_pump_mode() {
        held_lock_is_bounded(true);
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

    /// A peer that answers: `teardown_within` returns `Ok`, and the
    /// socket carries the configured read timeout and the 5 s write
    /// timeout again afterwards.
    #[test]
    fn teardown_within_a_healthy_peer_restores_the_socket_timeouts() {
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

        let r = client.teardown_within(Duration::from_millis(500));
        assert!(r.is_ok(), "got {r:?}");
        assert!(client.session_id.is_none());
        assert_eq!(
            installed_timeouts(&client),
            (Some(configured), Some(Duration::from_secs(5)))
        );

        drop(client);
        peer.join().unwrap();
    }
}
