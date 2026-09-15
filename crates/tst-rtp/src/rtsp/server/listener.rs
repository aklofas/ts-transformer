//! Server accept loop. Binds a tokio `TcpListener` to the configured
//! bind URL, runs an async accept loop, and spawns
//! `session::handle_connection` per accepted client. For `rtsps://`
//! binds, the accepted TCP stream is handed to
//! `tls::TlsServerConfig::accept` (lands at Task 11) before being
//! passed to the per-session task.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::net::TcpListener;

use crate::error::RtspServerError;
use crate::rtsp::server::ServerState;

/// Run the accept loop until cancellation or unrecoverable error.
///
/// Reports the bind outcome (address or typed error) through
/// `state.startup_tx`, the one-shot channel `RtspServer::start` blocks on.
/// Sets `state.local_addr` once the listener binds. Continues accepting
/// until either the hard cancel handle is flipped or the graceful
/// cancellation token fires.
pub(crate) async fn run_listener(state: Arc<ServerState>) -> Result<(), RtspServerError> {
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
    if let Some(tx) = startup_tx {
        // Best-effort: start() may have timed out and dropped the receiver.
        let _ = tx.send(Ok(local));
    }

    // TLS config was pre-loaded by start() (synchronously — so bad
    // cert/key paths failed start() itself). Plaintext binds carry None.
    #[cfg(feature = "rtsp-server-tls")]
    let tls_config = state.tls_config.lock().unwrap().take();

    loop {
        if state.hard_cancel.is_cancelled() {
            tracing::info!(target: "tst_rtp::server", "hard cancel observed; listener exiting");
            return Ok(());
        }
        tokio::select! {
            accept_res = listener.accept() => {
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
                        // favor of `try_update`, which the pinned 1.85 toolchain
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
                                tokio::spawn(async move {
                                    // `slot` is moved into this task so the reserved
                                    // slot is released (its `Drop` fires) on the
                                    // handshake-failure path, the handshake-timeout
                                    // path AND the normal session path.
                                    let _slot = slot;
                                    // Bounded handshake (CORR-06): a peer that connects
                                    // and never sends a ClientHello used to park this
                                    // task — and its slot — forever; `max_sessions`
                                    // such connects wedged the server at its cap.
                                    match tokio::time::timeout(handshake_timeout, cfg.accept(tcp)).await {
                                        Ok(Ok(tls_stream)) => {
                                            if let Err(e) = crate::rtsp::server::session::handle_connection_tls(st, tls_stream, peer).await {
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
                        tokio::spawn(async move {
                            if let Err(e) = crate::rtsp::server::session::handle_connection(st, tcp, peer, slot).await {
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
            _ = state.cancel_token.cancelled() => {
                tracing::info!(target: "tst_rtp::server", "graceful cancel observed; listener exiting");
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                // Wake to check hard_cancel flag.
            }
        }
    }
}
