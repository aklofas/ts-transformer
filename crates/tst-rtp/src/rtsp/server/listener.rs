//! Server accept loop. Binds a tokio `TcpListener` to the configured
//! bind URL, runs an async accept loop, and spawns
//! `session::handle_tcp_connection` (or `handle_tls_connection`) per
//! accepted client. For `rtsps://` binds, the accepted TCP stream is
//! handed to `tls::TlsServerConfig::accept` before being passed to the
//! per-session task.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::net::TcpListener;

use crate::error::RtspServerError;
use crate::rtsp::server::ServerState;

/// Run the accept loop until cancellation or unrecoverable error.
///
/// Reports the bind outcome (address or typed error) through
/// `state.startup_tx`, the one-shot channel `RtspServer::start` blocks on.
/// Sets `state.local_addr` once the listener binds. Accepts until
/// `state.stop_accepting` fires (`stop()` has begun), then closes the
/// listening socket, reports it through `state.listener_closed` and goes on
/// reaping sessions; returns once the server's cancellation token fires:
/// `stop()`, Drop, or the server's hard-cancel watcher once the hard cancel
/// handle is flipped.
pub(crate) async fn run_listener(state: Arc<ServerState>) -> Result<(), RtspServerError> {
    run_listener_with(state, tokio::task::JoinSet::new()).await
}

/// [`run_listener`] with the set of session tasks it starts with: a test
/// puts a task there that outlives the cancel, to see what the drain waits
/// for.
async fn run_listener_with(
    state: Arc<ServerState>,
    mut sessions: tokio::task::JoinSet<()>,
) -> Result<(), RtspServerError> {
    // Bind. `state.builder.bind_url` is guaranteed to be IP-literal at
    // builder time (validate_for_server_bind enforced it).
    let bind_addr = format!(
        "{}:{}",
        state.builder.bind_url.host, state.builder.bind_url.port
    );

    // Take the startup channel installed by start(). Present exactly once
    // per start(); None only if the receiver was already dropped.
    let startup_tx = state.startup_tx.lock().unwrap().take();

    // Bind phase: any error here is reported through `startup_tx` so
    // start() returns it typed, instead of only logging it.
    let bind_result: Result<(TcpListener, std::net::SocketAddr), RtspServerError> = async {
        let listener = TcpListener::bind(&bind_addr).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                RtspServerError::BindAddrInUse
            } else {
                RtspServerError::Io(e.kind())
            }
        })?;
        let local = listener
            .local_addr()
            .map_err(|e| RtspServerError::Io(e.kind()))?;
        Ok((listener, local))
    }
    .await;
    let (listener, local) = match bind_result {
        Ok(ok) => ok,
        Err(e) => match startup_tx {
            // Delivered to start() — don't double-report via the task log.
            Some(tx) => match tx.send(Err(e)) {
                Ok(()) => return Ok(()),
                // start() stopped listening (timeout) — surface via the
                // spawn wrapper's error log instead.
                Err(std::sync::mpsc::SendError(Err(e))) => return Err(e),
                Err(std::sync::mpsc::SendError(Ok(_))) => unreachable!(),
            },
            None => return Err(e),
        },
    };
    // Resolve + publish the bound address BEFORE reporting success —
    // start() returning Ok guarantees local_addr() is populated.
    *state.local_addr.lock().unwrap() = Some(local);
    tracing::info!(target: "tst_rtp::server", "RTSP server listening at {local}");
    // Reports "every session task has ended" to `RtspServer::stop` on every
    // way out after a successful bind, the drain below included. Installed
    // only now, before start() hears of the success: a failed bind ran no
    // session, and latching the flag there would let a retried server's
    // `stop()` skip its drain.
    let _ended = SessionsEnded(state.sessions_ended.clone());
    if let Some(tx) = startup_tx {
        // Best-effort: start() may have timed out and dropped the receiver.
        let _ = tx.send(Ok(local));
    }

    // TLS config was pre-loaded by start() (synchronously — so bad
    // cert/key paths failed start() itself). Plaintext binds carry None.
    #[cfg(feature = "rtsp-server-tls")]
    let tls_config = state.tls_config.lock().unwrap().take();

    // `None` once `stop()` has begun: the socket is closed and nothing is
    // accepted, while the sessions live on to be sent their Notice.
    let mut listener = Some(listener);
    loop {
        tokio::select! {
            // Biased, in this order: once `stop()` has begun no connection
            // waiting in the backlog may be accepted (a random pick could take
            // the accept arm first), and a hard cancel racing `stop()` is
            // taken after the stop arm, which reports the closed socket that
            // `stop()` waits for.
            biased;
            () = state.stop_accepting.cancelled(), if listener.is_some() => {
                tracing::info!(target: "tst_rtp::server", "server stopping; listener closed");
                listener = None;
                state.listener_closed.send_replace(true);
            }
            _ = state.cancel_token.cancelled() => {
                tracing::info!(target: "tst_rtp::server", "server cancel observed; listener exiting");
                // Close the socket before the drain: `SessionsEnded` reports
                // "done" to `stop()` when this task returns, and the socket
                // must not outlive that report, or a connect made right after
                // `stop()` returns is accepted by the kernel and then reset.
                drop(listener);
                drain_sessions(&mut sessions, state.builder.graceful_shutdown_drain).await;
                return Ok(());
            }
            accept_res = accept(listener.as_ref()) => {
                match accept_res {
                    Ok((tcp, peer)) => {
                        // Max-sessions guard — reserve the slot ATOMICALLY here,
                        // in the single-threaded accept loop, BEFORE spawning the
                        // per-session task. A plain load-then-check raced an
                        // unauthenticated connection burst: the loop accepts +
                        // spawns faster than the spawned tasks get polled to
                        // increment, so every accept read the same low count and
                        // all passed the check, blowing past the cap. The CAS
                        // reservation (compare-exchange loop) only increments while below
                        // the cap, so the counter NEVER exceeds `max_sessions`,
                        // even transiently — `stats().active_sessions` reads this
                        // same atomic, and the earlier fetch_add-then-fetch_sub
                        // refusal briefly exposed cap+1 to pollers. On over-cap we
                        // refuse (drop the TCP + continue). On success the
                        // reservation is owned by a `SessionSlot` RAII guard moved
                        // into the spawned task, whose `Drop` releases the slot on
                        // EVERY exit path (including a TLS-handshake failure that
                        // returns before the session loop runs — the leak the old
                        // in-session `fetch_sub` couldn't cover).
                        // Hand-rolled compare-exchange loop rather than
                        // `fetch_update`: that helper is deprecated on nightly in
                        // favor of `try_update`, which the pinned toolchain
                        // does not have, and the loop is all `fetch_update` does.
                        let mut n = state.active_sessions.load(Ordering::Relaxed);
                        let reserved = loop {
                            if n >= state.builder.max_sessions {
                                break false;
                            }
                            match state.active_sessions.compare_exchange_weak(
                                n,
                                n + 1,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => break true,
                                Err(current) => n = current,
                            }
                        };
                        if !reserved {
                            tracing::warn!(
                                target: "tst_rtp::server",
                                "max sessions ({}) reached; refusing {peer}",
                                state.builder.max_sessions
                            );
                            drop(tcp);
                            continue;
                        }
                        let st = state.clone();
                        let slot = crate::rtsp::server::session::SessionSlot::new(state.clone());
                        #[cfg(feature = "rtsp-server-tls")]
                        {
                            let cfg = tls_config.clone();
                            if let Some(cfg) = cfg {
                                let handshake_timeout = state.builder.tls_handshake_timeout;
                                sessions.spawn(async move {
                                    // `slot` moves into the session on success and is
                                    // dropped by this task on the handshake-failure and
                                    // timeout paths, releasing the reserved slot either way.
                                    // Bounded handshake: a peer that connects and never
                                    // sends a ClientHello would otherwise park this
                                    // task — and its slot — forever; `max_sessions`
                                    // such connects would wedge the server at its cap.
                                    match tokio::time::timeout(handshake_timeout, cfg.accept(tcp)).await {
                                        Ok(Ok(tls_stream)) => {
                                            if let Err(e) = crate::rtsp::server::session::handle_tls_connection(st, tls_stream, peer, slot).await {
                                                tracing::warn!(
                                                    target: "tst_rtp::server",
                                                    peer = %peer, error = ?e,
                                                    "tls session ended with error"
                                                );
                                            }
                                        }
                                        Ok(Err(e)) => {
                                            tracing::warn!(
                                                target: "tst_rtp::server",
                                                peer = %peer, error = ?e,
                                                "TLS handshake failed"
                                            );
                                        }
                                        Err(_elapsed) => {
                                            tracing::warn!(
                                                target: "tst_rtp::server",
                                                peer = %peer,
                                                // `duration_ms_saturating`, not a bare
                                                // `as_millis() as u64`: the latter
                                                // silently truncates for a Duration
                                                // past ~584 million years — see its
                                                // doc comment.
                                                timeout_ms = crate::rtsp::client::duration_ms_saturating(handshake_timeout),
                                                "TLS handshake deadline elapsed; dropping connection"
                                            );
                                        }
                                    }
                                });
                                continue;
                            }
                        }
                        // Plain TCP path (always available regardless of `rtsp-server-tls` feature):
                        sessions.spawn(async move {
                            if let Err(e) = crate::rtsp::server::session::handle_tcp_connection(st, tcp, peer, slot).await {
                                tracing::warn!(
                                    target: "tst_rtp::server",
                                    peer = %peer, error = ?e,
                                    "session ended with error"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "tst_rtp::server",
                            error = %e,
                            "accept failed; continuing"
                        );
                    }
                }
            }
            // Reap finished sessions so the set stays as small as the live
            // count.
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
        }
    }
}

/// The next connection on `listener`; never resolves once it is closed.
async fn accept(
    listener: Option<&TcpListener>,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

/// Flips the shared "sessions ended" flag when the listener task is done,
/// however it got there.
struct SessionsEnded(Arc<tokio::sync::watch::Sender<bool>>);

impl Drop for SessionsEnded {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// Wait for every session task to finish, but no longer than `drain`. Tasks
/// still running at the bound are detached, not aborted: they keep ending on
/// their own cancel, as they did before the wait was tracked.
async fn drain_sessions(sessions: &mut tokio::task::JoinSet<()>, drain: std::time::Duration) {
    let all_ended = async { while sessions.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(drain, all_ended).await;
    sessions.detach_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::server::builder::RtspServerBuilder;
    use crate::rtsp::server::test_state_with;
    use std::time::Duration;
    use tokio::time::{Instant, timeout};

    /// The token alone ends the listener on a hard cancel or Drop: the
    /// listening socket is closed while the drain still waits for a session
    /// task that outlives the cancel, not when the task returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_listening_socket_is_closed_before_the_session_drain() {
        let mut builder = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.graceful_shutdown_drain(Duration::from_secs(60));
        let state = test_state_with(builder);
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        *state.startup_tx.lock().unwrap() = Some(startup_tx);
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(async move {
            let _ = released.await;
        });
        let listener = tokio::spawn(run_listener_with(state.clone(), sessions));
        let addr =
            tokio::task::spawn_blocking(move || startup_rx.recv_timeout(Duration::from_secs(10)))
                .await
                .unwrap()
                .expect("the listener reported its bind")
                .expect("the listener bound");
        let connect = || timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr));
        // Control: the address takes connections before the cancel.
        drop(connect().await.expect("connect in time").expect("accepted"));

        state.cancel_token.cancel();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(Err(e)) = connect().await
                && e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the listening socket outlived the cancel while the drain waited"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !*state.sessions_ended.borrow(),
            "the drain no longer waits for the session task"
        );
        assert!(!listener.is_finished());

        release.send(()).unwrap();
        listener.await.unwrap().unwrap();
        assert!(*state.sessions_ended.borrow());
    }

    /// Once the stop-accepting signal has fired, the accept loop takes the
    /// stop arm before another connection: during a burst of connects at
    /// most the one accept already past its `select!` is counted after the
    /// signal. Without the loop's bias a ready accept arm can be picked again
    /// and again. Linux only: a round holds about 1 800 descriptors.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_most_one_connection_is_accepted_after_the_stop_signal() {
        use std::sync::atomic::AtomicBool;
        use std::time::Instant as StdInstant;
        for round in 0..20 {
            let mut builder = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
            builder.max_sessions(100_000);
            builder.graceful_shutdown_drain(Duration::from_millis(10));
            let state = test_state_with(builder);
            let (startup_tx, startup_rx) = std::sync::mpsc::channel();
            *state.startup_tx.lock().unwrap() = Some(startup_tx);
            let listener = tokio::spawn(run_listener_with(
                state.clone(),
                tokio::task::JoinSet::new(),
            ));
            let addr = tokio::task::spawn_blocking(move || {
                startup_rx.recv_timeout(Duration::from_secs(10))
            })
            .await
            .unwrap()
            .expect("the listener reported its bind")
            .expect("the listener bound");

            // Connects as fast as it can until told to stop.
            let done = Arc::new(AtomicBool::new(false));
            let connector = {
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    let mut held = Vec::new();
                    while !done.load(Ordering::Relaxed) && held.len() < 900 {
                        if let Ok(tcp) = std::net::TcpStream::connect(addr) {
                            held.push(tcp);
                        }
                    }
                    held
                })
            };
            // In the middle of the burst: fire the signal, count, wait for
            // the listener's report, count again.
            let st = Arc::clone(&state);
            let (at_signal, at_report) = tokio::task::spawn_blocking(move || {
                let deadline = StdInstant::now() + Duration::from_secs(10);
                while st.active_sessions.load(Ordering::Relaxed) < 200 {
                    assert!(StdInstant::now() < deadline, "the burst never reached 200");
                    std::hint::spin_loop();
                }
                st.stop_accepting.cancel();
                let at_signal = st.active_sessions.load(Ordering::Relaxed);
                while !*st.listener_closed.borrow() {
                    assert!(StdInstant::now() < deadline, "the listener never reported");
                    std::hint::spin_loop();
                }
                (at_signal, st.active_sessions.load(Ordering::Relaxed))
            })
            .await
            .unwrap();
            done.store(true, Ordering::Relaxed);
            let held = tokio::task::spawn_blocking(move || connector.join().unwrap())
                .await
                .unwrap();
            assert!(
                at_report <= at_signal + 1,
                "round {round}: {} connections accepted after the stop signal",
                at_report - at_signal
            );
            state.cancel_token.cancel();
            drop(held);
            listener.await.unwrap().unwrap();
        }
    }
}
