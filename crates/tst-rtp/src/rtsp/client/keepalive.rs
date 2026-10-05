//! Background thread sending OPTIONS at `session_timeout / 2` intervals
//! (retuned in place when SETUP learns the server-advertised timeout).
//!
//! - Holds an `Arc<Mutex<Stream>>` clone — the SAME stream the main
//!   thread uses. Works uniformly for plain TCP and TLS; a
//!   `try_clone`d stream FD fails on rustls `ClientConnection` (it
//!   would silently disable keepalive for `rtsps://` sessions).
//! - This thread only WRITES. Responses to its pings are consumed at
//!   whichever site owns reads in the current mode (interleaved pump or
//!   the main thread's `send_and_read`), classified by the CSeq range —
//!   see `KEEPALIVE_CSEQ_BASE` and `handle_keepalive_response`.
//! - On cancel (or write error), the thread exits.
//! - The shared "session_dead" flag (polled via
//!   `RtspClient::is_session_alive`) flips when a ping write fails, or
//!   when a read site sees a ping answered `454 Session Not Found`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use secrecy::SecretString;

use crate::rtsp::client::end_reason::{EndReasonSlot, StreamEndReason};
use crate::rtsp::client::{AuthState, Stream};
use crate::rtsp::message::{RtspMethod, RtspRequest};
use crate::url::RtspVersion;

/// Lower bound of the keepalive thread's CSeq space — far above the main
/// thread's counter (which starts at 1 and increments per request). The
/// thread's counter starts here and increments BEFORE each ping, so
/// emitted CSeqs are strictly greater than the base; a response
/// classifies as answering a keepalive when its CSeq is
/// `>= KEEPALIVE_CSEQ_BASE` (consumed at the read site, never surfaced
/// to callers), anything below belongs to a main-thread request.
pub(crate) const KEEPALIVE_CSEQ_BASE: u32 = 1_000_000;

/// Act on a response to a keepalive OPTIONS ping.
///
/// Called by whichever read site owns the stream in the current mode —
/// the interleaved pump (post-SETUP TCP) or the main thread's
/// `send_and_read` (UDP / pre-SETUP) — after classifying the response by
/// its CSeq (`>= KEEPALIVE_CSEQ_BASE`). The response is consumed either
/// way; this decides whether it also carries a signal:
///
/// - **401**: the server rotated or expired the nonce mid-session
///   (RFC 7616 §3.3 `stale=true` and friends). Refresh the shared
///   challenge cache so the NEXT ping — at most one interval away —
///   signs against the fresh challenge. Without this every subsequent
///   ping would re-sign the dead nonce and the session would silently
///   die at the server timeout.
/// - **454 Session Not Found**: the server no longer honors the session;
///   flip `session_dead` so `RtspClient::is_session_alive` surfaces it.
/// - anything else (200 OK in the normal case): nothing to do.
pub(crate) fn handle_keepalive_response(
    resp: &crate::rtsp::message::RtspResponse,
    auth: &Mutex<AuthState>,
    session_dead: Option<&AtomicBool>,
    end_reason: &EndReasonSlot,
) {
    match resp.status {
        401 => {
            if let Some(www) = resp.headers.get("www-authenticate") {
                tracing::debug!(
                    target: "tst_rtp::client::keepalive",
                    "keepalive ping got 401; refreshing cached auth challenge"
                );
                crate::rtsp::client::lock_unpoisoned(auth).cache_challenge(www.clone());
            }
        }
        454 => {
            tracing::warn!(
                target: "tst_rtp::client::keepalive",
                "keepalive ping got 454 Session Not Found; marking session dead"
            );
            if let Some(flag) = session_dead {
                flag.store(true, Ordering::Relaxed);
            }
            end_reason.record(StreamEndReason::SessionExpired);
        }
        _ => {}
    }
}

/// Spawn the rtsp-keepalive background thread.
///
/// `cancel` flips to true when the [`crate::rtsp::client::RtspCancelHandle::cancel`]
/// is invoked (or when the `RtspClient` is dropped). The thread polls it
/// every 200 ms and exits cleanly at the next wake.
///
/// `session_dead` flips to true when a write to the control TCP fails;
/// the main thread can poll it (via `RtspClient::is_session_alive`) and
/// take recovery action.
///
/// `interval_ms` is the OPTIONS-ping cadence in milliseconds, read anew
/// at every wake — typically `session_timeout / 2` (so a 60 s server
/// timeout pings every 30 s). SETUP retunes it in place when the server
/// advertises its own `Session: <id>;timeout=N` (unless the caller
/// supplied an explicit override), and the retune takes effect within
/// one 200 ms poll step even if this thread is mid-wait toward a stale
/// deadline.
///
/// `session_id` is a shared cell the main thread updates when SETUP
/// returns a new ID; the keepalive emits `Session: <id>` when present.
/// Pre-SETUP keepalives (used as connectivity probes) omit the header.
///
/// CSeq starts at `1_000_000` to avoid colliding with the main thread's
/// counter (which starts at 1 and increments per request).
///
/// Returns `Err` if the OS refuses to spawn the thread (resource
/// exhaustion). This MUST be propagated rather than `.expect()`'d — the
/// keepalive is started on the RTSP connect path and the JVM/C bindings do
/// not catch unwinds across the FFI boundary, so a panic here would abort
/// the host process. The caller maps the `io::Error` to a typed
/// `RtspError`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    write_half: Arc<Mutex<Stream>>,
    cancel: Arc<AtomicBool>,
    session_dead: Arc<AtomicBool>,
    interval_ms: Arc<AtomicU64>,
    url: String,
    version: RtspVersion,
    session_id: Arc<Mutex<Option<String>>>,
    user_agent: String,
    auth: Arc<Mutex<AuthState>>,
    username: Option<String>,
    password: Option<SecretString>,
    write_gate: Arc<AtomicUsize>,
    end_reason: EndReasonSlot,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("rtsp-keepalive".to_string())
        .spawn(move || {
            // Incremented BEFORE each ping — emitted CSeqs are strictly
            // above the base; see the const doc for the classification.
            let mut cseq = KEEPALIVE_CSEQ_BASE;
            loop {
                // Wait out one interval, waking at least every 200 ms to
                // check the cancel flag (bounds teardown latency even at a
                // 30 s cadence) and re-reading the shared interval at every
                // wake: SETUP retunes the cadence in place when the server
                // advertises its own session timeout, and this thread may
                // be mid-wait toward a stale — possibly 30 s — deadline
                // when that happens. Sub-200 ms cadences are honored (the
                // sleep step never exceeds the remaining wait).
                let started = std::time::Instant::now();
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let interval =
                        Duration::from_millis(interval_ms.load(Ordering::Relaxed).max(1));
                    let remaining = interval.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        break;
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(200)));
                }
                // One more cancel check before we burn the bytes on the wire.
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                cseq += 1;
                let mut req = RtspRequest::new(RtspMethod::Options, url.clone(), version)
                    .header("cseq", cseq.to_string())
                    .header("user-agent", user_agent.as_str());
                if let Some(sid) = crate::rtsp::client::lock_unpoisoned(&session_id).clone() {
                    req = req.header("session", sid);
                }
                // Attach cached credentials pre-emptively so servers that
                // require auth on OPTIONS accept the ping and refresh the
                // session. No-op until the challenge is learned (at DESCRIBE).
                if let (Some(user), Some(pass)) = (username.as_ref(), password.as_ref()) {
                    // Snapshot the challenge and allocate its nonce-count
                    // under ONE lock acquisition, so this ping can never
                    // pair a stale challenge with a post-rotation count
                    // (or repeat an `nc` for a reused nonce — RFC 7616
                    // §3.4 replay protection). See `AuthState`.
                    let snapshot = {
                        let mut g = crate::rtsp::client::lock_unpoisoned(&auth);
                        match g.challenge.clone() {
                            Some(www) => {
                                g.nc += 1;
                                Some((www, g.nc))
                            }
                            None => None,
                        }
                    };
                    if let Some((www, nc)) = snapshot {
                        if let Ok(authz) = crate::rtsp::auth::build_authorization(
                            RtspMethod::Options,
                            &url,
                            &www,
                            user,
                            pass,
                            nc,
                        ) {
                            req = req.header("authorization", authz);
                        }
                    }
                }
                // Validate against header injection before writing. The
                // User-Agent was already validated at connect time and the
                // session id is server-issued, so this should never fail; if
                // it somehow does, treat the session as dead rather than
                // smuggling bytes onto the wire.
                let bytes = match req.encode_checked() {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            target: "tst_rtp::client::keepalive",
                            error = %e,
                            "keepalive ping failed the header-injection guard; marking session dead"
                        );
                        session_dead.store(true, Ordering::Relaxed);
                        end_reason.record(StreamEndReason::KeepaliveFailed {
                            msg: format!("keepalive ping failed to encode: {e}"),
                        });
                        return;
                    }
                };
                // Announce this write on the hand-off gate so an active
                // interleaved pump yields the stream mutex promptly —
                // the pump holds the lock across each blocking socket
                // read, and without the gate a hot data stream can
                // starve keepalive writes for many read cycles (see
                // `RtspClient::write_gate`). RAII guard: the decrement
                // must survive a panic while the lock is held, or the
                // pump would skip reads forever.
                //
                // The stream lock recovers from poison (`lock_unpoisoned`)
                // rather than propagating it — if the main thread panicked
                // mid-request, this thread's write just proceeds against
                // the (possibly half-written) stream state; a genuine wire
                // failure still surfaces below via `write_result`.
                let write_result = {
                    let _gate = crate::rtsp::client::WriteGateGuard::enter(&write_gate);
                    let mut g = crate::rtsp::client::lock_unpoisoned(&write_half);
                    g.write_all(&bytes)
                };
                if let Err(e) = write_result {
                    tracing::warn!(
                        target: "tst_rtp::client::keepalive",
                        error = %e,
                        "keepalive ping write failed; marking session dead"
                    );
                    session_dead.store(true, Ordering::Relaxed);
                    end_reason.record(StreamEndReason::KeepaliveFailed {
                        msg: format!("control TCP write failed: {e}"),
                    });
                    return;
                }
                // The response is consumed wherever reads happen in the
                // current mode: the interleaved pump (post-SETUP TCP) or
                // the main thread's next `send_and_read` (UDP/pre-SETUP)
                // recognizes the keepalive CSeq range and handles the
                // response without surfacing it to callers.
            }
        })
}
