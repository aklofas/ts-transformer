//! `tst_rtsp_client_builder_connect` and `tst_rtsp_session_*` C entry points.
//!
//! `tst_rtsp_client_builder_connect` consumes a `TstRtspClientBuilder`,
//! reconstructs a `tst_rtp::RtspClientBuilder` from the stored fields, calls
//! `.connect()` (opens the TCP control channel), then runs the DESCRIBE +
//! SETUP sequence to obtain an `RtspSession`.  The result is a `TstRtspSession`
//! opaque handle that holds both the live `RtspClient` (which owns the control
//! channel for PLAY / PAUSE / TEARDOWN) and the `RtspSession` (which carries
//! the transport-side socket or mpsc channel for `into_recv_transport`).
//!
//! # Session lifecycle (C ABI perspective)
//!
//! ```text
//! builder  ─tst_rtsp_client_builder_connect()──►  session
//!                                                     │
//!           tst_rtsp_session_play()  ────────────────►│   PLAY sent
//!           tst_rtsp_session_pause() ────────────────►│   PAUSE sent
//!                                                     │
//!  ┌─  tst_rtsp_session_into_demux_receiver() ───────►│   consumes + frees session
//!  │                                                   │   returns TstRtpDemuxReceiver,
//!  │                                                   │   which now owns the RtspClient
//!  └──► tst_rtp_demux_receiver_next_event()  ────────────► event loop
//!       tst_rtp_demux_receiver_cancel()  ────────────────► data plane only
//!       tst_rtp_demux_receiver_close()   ────────────────► TEARDOWN sent; receiver freed
//!
//!   OR, to close without consuming the transport (also after the bridge
//!   returned NULL, which leaves the session with the caller):
//!       tst_rtsp_session_teardown_and_free() ────────► TEARDOWN sent; session freed
//! ```
//!
//! # Handle layout rationale
//!
//! `TstRtspSession` holds both `RtspClient` and `RtspSession`:
//!
//! - `client` owns the TCP control channel.  `play()`, `pause()`, `teardown()`
//!   are methods on `RtspClient` — they send RTSP messages over the control
//!   channel.
//! - `session` holds the data-plane socket or mpsc channel (from SETUP).
//!   `into_recv_transport()` is a consuming method on `RtspSession` that
//!   packages the socket into an `RtpRecvTransport`.
//!
//! Both must remain live and paired.  Wrapping them in a single `Option` behind
//! a `Mutex` gives close-idempotence and panic isolation like `CHandle`, and
//! lets `into_demux_receiver` consume the pair (move it out of the `Option`)
//! in a single lock acquisition.  `CHandle::take` can also move a value out;
//! migrating this handle to it needs its own panic-policy decision.
//!
//! # Cancel semantics
//!
//! Cancel signals the `RtspClient`'s internal cancel flag, breaking out of any
//! blocking I/O on the control channel.  It does NOT cancel the RTP data plane —
//! once `into_demux_receiver` has been called, cancel on the returned
//! `TstRtpDemuxReceiver` governs data-plane shutdown.

use std::sync::{Mutex, TryLockError};

use tst_core::RecvTransport;
use tst_pipeline::DemuxReceiver;
use tst_rtp::{RtspCancelHandle, RtspClient, RtspClientBuilder, RtspSession};

use crate::demux_config::TstDemuxConfig;
use crate::error::{TstError, set_last_error};
use crate::handle::{CHandle, TstRtspClientBuilder, cancel_or_latch};
use crate::panic::ffi_catch;
use crate::rtp::demux_receiver::TstRtpDemuxReceiver;

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque handle for a live RTSP client session.
///
/// Obtained from [`tst_rtsp_client_builder_connect`].  Freed by either
/// [`tst_rtsp_session_teardown_and_free`] (sends TEARDOWN first) or a
/// successful [`tst_rtsp_session_into_demux_receiver`] (moves the client and
/// the data-plane transport into a new `TstRtpDemuxReceiver` handle, which
/// sends TEARDOWN when it is closed). A NULL return from the bridge leaves
/// the session allocated and still the caller's to free.
///
/// Call [`tst_rtsp_session_play`] after obtaining the session handle to
/// begin the RTP data flow; the builder's `_connect` step only runs
/// OPTIONS + DESCRIBE + SETUP, not PLAY.
pub struct TstRtspSession {
    /// Mutex-wrapped optional pair `(RtspClient, RtspSession)`.
    ///
    /// `into_demux_receiver` moves the value out of the `Option` in a single
    /// lock acquisition.  This handle keeps its own mutex rather than
    /// `CHandle` (which would need its own panic-policy decision to adopt).
    /// `None` after `into_demux_receiver` or `teardown_and_free`
    /// consumes the pair; subsequent calls return `TST_E_CLOSED`.
    pub(crate) pair: Mutex<Option<Box<(RtspClient, RtspSession)>>>,
    /// Cancel handle for the `RtspClient`. Calling `.cancel()` breaks out of
    /// any blocking RTSP I/O on the control channel. `RtspCancelHandle` is
    /// `Clone`; it is thread-safe to call `.cancel()` concurrently with any
    /// blocked RTSP I/O on the client. Separate from the `RtpRecvTransport`
    /// cancel that governs the data plane post-`into_demux_receiver`.
    pub(crate) rtsp_cancel: RtspCancelHandle,
}

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

/// Consume a builder and open a live RTSP session (OPTIONS → DESCRIBE → SETUP).
///
/// Runs the full RTSP client-side connection sequence:
///
/// 1. Builds the Rust builder from the URL parsed at `_new`, keeping its
///    userinfo and query options; explicit setters override them.
/// 2. Calls `.connect()` to open the TCP control channel and start the
///    client's thread, with its keepalive pings unless disabled via
///    `tst_rtsp_client_builder_keepalive`.
/// 3. Calls `describe()` to fetch the server's SDP.
/// 4. Calls `setup_mp2t_auto(&sdp)` to select the first MPEG-TS media and
///    negotiate the transport (UDP or TCP-interleaved).
///
/// **PLAY is NOT sent automatically.**  Call [`tst_rtsp_session_play`] after
/// `_connect` to start the RTP data flow, then call
/// [`tst_rtsp_session_into_demux_receiver`] to obtain a
/// `tst_rtp_demux_receiver_t` for the event loop.
///
/// On success the `builder` pointer is consumed (freed).  On failure the
/// builder is also freed; check `tst_get_last_error()` / `tst_get_last_error_str()`.
///
/// Returns a non-NULL `tst_rtsp_session_t*` on success, NULL on failure.
///
/// # Safety
///
/// `builder` must be a non-NULL pointer returned by
/// `tst_rtsp_client_builder_new` that has not yet been freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_connect(
    builder: *mut TstRtspClientBuilder,
) -> *mut TstRtspSession {
    ffi_catch(std::ptr::null_mut(), || {
        if builder.is_null() {
            set_last_error(TstError::InvalidConfig, "builder is null");
            return std::ptr::null_mut();
        }
        // SAFETY: caller guarantees valid, unaliased pointer from _new.
        let mut b = unsafe { TstRtspClientBuilder::from_raw(builder) };

        // Start from the parsed URL itself: re-rendering it would drop the
        // userinfo and every query option. The explicit setters below then
        // override what the URL carries.
        let mut rust_builder = RtspClientBuilder::from_url(b.url.clone());
        if let Some(pref) = b.transport_pref {
            rust_builder = rust_builder.transport_preference(pref);
        }

        // Apply auth credentials if set by any _auth_* setter.
        if let (Some(user), Some(secret)) = (b.username.clone(), b.password.take()) {
            rust_builder = rust_builder.auth(user, secret);
        }

        // Apply keepalive preference: builder default is auto-keepalive enabled.
        if !b.auto_keepalive {
            rust_builder = rust_builder.no_auto_keepalive(true);
        }

        // TLS trust anchors from tst_rtsp_client_builder_tls_root_cert_pem.
        // With the `tls` feature they verify an rtsps:// server; without it
        // an rtsps:// connect fails anyway, and a plain rtsp:// URL ignores them.
        #[cfg(feature = "tls")]
        if let Some(pem) = &b.tls_root_cert_pem {
            match tst_core::net::tls::load_root_store(pem) {
                Ok(roots) => rust_builder = rust_builder.tls_root_certs(roots),
                Err(e) => {
                    set_last_error(TstError::RtspTls, &format!("tls_root_cert_pem: {e}"));
                    return std::ptr::null_mut();
                }
            }
        }

        // Step 2: TCP connect; the client's thread starts the keepalive
        // pings unless they are disabled.
        let mut client = match rust_builder.connect() {
            Ok(c) => c,
            Err(e) => {
                crate::error::record_with_context(e, "RTSP connect failed");
                return std::ptr::null_mut();
            }
        };

        // The cancel handle is taken here, but the session handle does not
        // exist before `_connect` returns, so `tst_rtsp_session_cancel`
        // cannot interrupt the connect, DESCRIBE or SETUP above and below.
        // RtspCancelHandle is Clone + Send + Sync.
        let rtsp_cancel = client.cancel_handle();

        // Step 3: DESCRIBE.
        let sdp = match client.describe() {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_with_context(e, "RTSP DESCRIBE failed");
                return std::ptr::null_mut();
            }
        };

        // Step 4: SETUP — selects the first MPEG-TS media, negotiates transport.
        let session = match client.setup_mp2t_auto(&sdp) {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_with_context(e, "RTSP SETUP failed");
                return std::ptr::null_mut();
            }
        };

        Box::into_raw(Box::new(TstRtspSession {
            pair: Mutex::new(Some(Box::new((client, session)))),
            rtsp_cancel,
        }))
    })
}

// ---------------------------------------------------------------------------
// Play
// ---------------------------------------------------------------------------

/// Send an RTSP PLAY request on the session.
///
/// Must be called after [`tst_rtsp_client_builder_connect`] and before
/// reading RTP data via [`tst_rtsp_session_into_demux_receiver`] +
/// `tst_rtp_demux_receiver_next_event`.
///
/// Returns 0 on success, or a negative `TST_E_RTSP_*` code on failure.
///
/// # Safety
///
/// `session` must be a non-NULL, non-freed pointer returned by
/// `tst_rtsp_client_builder_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_session_play(session: *mut TstRtspSession) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { session.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtsp session pointer");
            return TstError::InvalidConfig as i32;
        };
        let mut guard = match handle.pair.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(TstError::Internal, "rtsp session mutex poisoned");
                return TstError::Internal as i32;
            }
        };
        match guard.as_mut() {
            None => {
                set_last_error(TstError::Closed, "rtsp session already consumed or closed");
                TstError::Closed as i32
            }
            Some(pair) => {
                let (client, _session) = pair.as_mut();
                match client.play() {
                    Ok(_rtp_info) => 0,
                    Err(e) => crate::error::record_with_context(e, "RTSP PLAY failed"),
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Pause
// ---------------------------------------------------------------------------

/// Send an RTSP PAUSE request on the session.
///
/// Suspends the RTP data flow without tearing down the session. The caller
/// may resume by calling [`tst_rtsp_session_play`] again.
///
/// Returns 0 on success, or a negative `TST_E_RTSP_*` code on failure.
///
/// # Safety
///
/// `session` must be a non-NULL, non-freed pointer returned by
/// `tst_rtsp_client_builder_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_session_pause(session: *mut TstRtspSession) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { session.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtsp session pointer");
            return TstError::InvalidConfig as i32;
        };
        let mut guard = match handle.pair.lock() {
            Ok(g) => g,
            Err(_) => {
                set_last_error(TstError::Internal, "rtsp session mutex poisoned");
                return TstError::Internal as i32;
            }
        };
        match guard.as_mut() {
            None => {
                set_last_error(TstError::Closed, "rtsp session already consumed or closed");
                TstError::Closed as i32
            }
            Some(pair) => {
                let (client, _session) = pair.as_mut();
                match client.pause() {
                    Ok(()) => 0,
                    Err(e) => crate::error::record_with_context(e, "RTSP PAUSE failed"),
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Teardown + free
// ---------------------------------------------------------------------------

/// Send TEARDOWN and free the session handle.
///
/// Sends an RTSP TEARDOWN request, then ends the client and frees the
/// session regardless of whether TEARDOWN succeeded (network errors at
/// teardown time are recorded in last-error but do not prevent the handle
/// from being freed). A healthy peer's 2xx returns 0, and so
/// does `454 Session Not Found` (the session is already gone). A TEARDOWN
/// the server refuses with any other status returns that status's code
/// (`TST_E_RTSP_PROTOCOL`, or `TST_E_RTSP_NOT_FOUND` for a 404). A first
/// 401 is answered once with the credentials, unless it carries no
/// `WWW-Authenticate` header (`TST_E_RTSP_PROTOCOL`), the client has no
/// credentials (`TST_E_RTSP_AUTH_FAILED`) or its challenge offers no scheme
/// the client may answer (`TST_E_RTSP_AUTH_REQUIRED`); a 401 to that retry
/// returns `TST_E_RTSP_AUTH_FAILED`. The TEARDOWN exchange is bounded to
/// 500 ms, covering the request write and the reply: when the bound passes
/// (a peer that does not answer, or one that stopped reading) the call
/// returns `TST_E_RTSP_TIMEOUT` and the handle is still freed. A session
/// cancelled with [`tst_rtsp_session_cancel`] before the call still writes
/// the TEARDOWN but does not wait for the reply: it returns `TST_E_CLOSED`,
/// also when that write fails, and is still freed. If a caught panic had already
/// taken the control pair, there is nothing to tear down: the call
/// returns 0 and frees. After the TEARDOWN the call stops the client's
/// thread and waits up to 2 s for it, so it can take up to about 2.5 s;
/// when it returns the thread has closed the control connection, unless
/// those 2 s ran out (the thread then ends on its own).
///
/// The caller guarantees that no `_play`, `_pause` or receive call on this
/// session is in flight: call [`tst_rtsp_session_cancel`], wait for the
/// parked call to return, then free. A call still parked is not a
/// supported case; the implementation cancels it as a defence, but that
/// is not part of the contract.
///
/// After this call the `session` pointer is invalid; any further use is
/// undefined behavior. NULL is a no-op.
///
/// # Safety
///
/// `session` must be NULL, or a pointer returned by
/// `tst_rtsp_client_builder_connect` that has not yet been freed or
/// consumed by a successful [`tst_rtsp_session_into_demux_receiver`]. A
/// session the bridge returned NULL for is still valid here and must be
/// freed with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_session_teardown_and_free(
    session: *mut TstRtspSession,
) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        if session.is_null() {
            return 0;
        }
        // SAFETY: caller guarantees valid, unaliased, un-freed pointer.
        let boxed = unsafe { Box::from_raw(session) };
        let rc = match boxed.pair.try_lock() {
            // Nobody holds the session: `RtspClient::close` sends TEARDOWN
            // and reads the reply under its 500 ms bound, then waits for
            // the client's thread.
            Ok(mut guard) => close(guard.take()),
            // Defence only, not a contract (the caller must have waited for
            // any parked call): wake it first, then close the cancelled
            // client (its TEARDOWN is written, the reply is not awaited).
            Err(TryLockError::WouldBlock) => {
                boxed.rtsp_cancel.cancel();
                match boxed.pair.lock() {
                    Ok(mut guard) => close(guard.take()),
                    Err(_) => poisoned(),
                }
            }
            Err(TryLockError::Poisoned(_)) => {
                boxed.rtsp_cancel.cancel();
                poisoned()
            }
        };
        // Nothing may stay parked on the control channel once the handle is
        // gone.
        boxed.rtsp_cancel.cancel();
        // Drop the box (drops RtspClient + RtspSession).
        drop(boxed);
        rc
    })
}

/// `RtspClient::close` on the pair: TEARDOWN, then the wait for the
/// client's thread.
fn close(pair: Option<Box<(RtspClient, RtspSession)>>) -> libc::c_int {
    match pair {
        None => 0, // already consumed
        Some(pair) => {
            let (client, session) = *pair;
            let r = client.close();
            drop(session);
            teardown_rc(r)
        }
    }
}

fn teardown_rc(r: Result<(), tst_rtp::RtspError>) -> libc::c_int {
    match r {
        Ok(()) => 0,
        Err(e) => crate::error::record_with_context(e, "RTSP TEARDOWN failed (continuing free)"),
    }
}

fn poisoned() -> libc::c_int {
    // Mutex poisoned: skip teardown, still free.
    set_last_error(TstError::Internal, "rtsp session mutex poisoned");
    TstError::Internal as i32
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// Cancel any blocking RTSP I/O on the control channel.
///
/// Sets the session's cancel flag: a PLAY or PAUSE on another thread that
/// is waiting for its reply notices it within 100 ms. A request made after
/// the cancel is still written first. Callable from another thread while a
/// PLAY or PAUSE on the same session is in flight.  Idempotent.  The
/// interrupted call returns `TST_E_CLOSED` (-7).
///
/// Never call this after a successful
/// [`tst_rtsp_session_into_demux_receiver`] (the session is freed then), nor
/// concurrently with one: the bridge may free the session under you. To stop
/// a bridged receiver use `tst_rtp_demux_receiver_cancel`. After the bridge
/// returns NULL the session is still valid and this call is allowed.
///
/// A cancel is terminal for the session: a later
/// [`tst_rtsp_session_into_demux_receiver`] refuses it with `TST_E_CLOSED`,
/// and the session is then only good for
/// [`tst_rtsp_session_teardown_and_free`].
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `session` is NULL.
///
/// # Safety
///
/// `session` must be NULL or a valid non-freed `*mut TstRtspSession`, not
/// consumed by a successful `tst_rtsp_session_into_demux_receiver` and not
/// being passed to it concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_session_cancel(session: *mut TstRtspSession) -> libc::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { session.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtsp session pointer");
            return TstError::InvalidConfig as i32;
        };
        // Signal cancel without acquiring the pair Mutex — matches the
        // pattern used by tst_rtp_demux_receiver_cancel / tst_rtp_sender_cancel.
        handle.rtsp_cancel.cancel();
        0
    })
}

// ---------------------------------------------------------------------------
// Bridge: into_demux_receiver
// ---------------------------------------------------------------------------

/// Consume the session's data-plane transport and return a
/// `tst_rtp_demux_receiver_t` ready for event iteration.
///
/// This is the primary path for reading RTP data after
/// `tst_rtsp_client_builder_connect` + `tst_rtsp_session_play`:
///
/// 1. Locks the session's pair Mutex, moves out `(RtspClient, RtspSession)`
///    and frees the session allocation.
/// 2. Calls `RtspSession::into_recv_transport()` to get the `RtpRecvTransport`
///    (either a bound UDP socket or a queue the RTSP client's thread fills
///    from the TCP-interleaved stream, depending on what SETUP negotiated).
/// 3. Wraps the transport in a `DemuxReceiver` using the supplied `demux_cfg`
///    (or default options if NULL).
/// 4. Returns a `*mut TstRtpDemuxReceiver` using the same opaque type as
///    `tst_rtp_demux_receiver_open`, so the caller can use the existing
///    `tst_rtp_demux_receiver_next_event` / `_cancel` / `_close` data-path
///    entry points unchanged.
///
/// The returned receiver owns the RTSP control connection (`RtspClient`)
/// and the client's thread, which sends the keepalive pings and, for
/// TCP-interleaved, feeds the receiver, until
/// `tst_rtp_demux_receiver_close`, which sends TEARDOWN.
/// `tst_rtp_demux_receiver_cancel` stops only the data plane; the control
/// connection stays up until close.
///
/// A session already cancelled with [`tst_rtsp_session_cancel`] is refused:
/// the call returns NULL with `TST_E_CLOSED`, because the client starts no
/// keepalive ping after a cancel and the server would reap the session
/// after its timeout. The bridge hands the control connection to the
/// receiver; after a non-NULL return the session handle is spent and the
/// receiver's own cancel, `tst_rtp_demux_receiver_cancel`, is the one that
/// stops the receive side.
///
/// Ownership: a non-NULL return consumes `session` and frees it, so the
/// caller must not use or free it again. A NULL return never frees
/// `session`: the allocation is still the caller's to release with
/// [`tst_rtsp_session_teardown_and_free`]. Its state may already be closed:
/// after a cancel, teardown reports `TST_E_CLOSED` and only frees; after a
/// panic between taking the control pair and building the receiver, there
/// is nothing left to tear down, so teardown returns 0 and only frees.
///
/// The returned `tst_rtp_demux_receiver_t` must eventually be freed with
/// `tst_rtp_demux_receiver_close`.  Returns NULL on failure.
///
/// # Safety
///
/// - `session` must be a non-NULL, non-freed pointer from
///   `tst_rtsp_client_builder_connect`.
/// - `demux_cfg` may be NULL (default options) or a valid pointer from
///   `tst_demux_config_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_session_into_demux_receiver(
    session: *mut TstRtspSession,
    demux_cfg: *const TstDemuxConfig,
) -> *mut TstRtpDemuxReceiver {
    ffi_catch(std::ptr::null_mut(), || {
        let Some(handle) = (unsafe { session.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null rtsp session pointer");
            return std::ptr::null_mut();
        };

        // A cancel already stopped the client's keepalive: a receiver built
        // from it would be reaped by the server after its session timeout.
        if handle.rtsp_cancel.is_cancelled() {
            set_last_error(
                TstError::Closed,
                "rtsp session was cancelled; teardown_and_free it",
            );
            return std::ptr::null_mut();
        }

        // Step 1: move the (RtspClient, RtspSession) pair out of the Mutex.
        // Every early return below leaves the allocation alone: a NULL
        // return keeps the session with the caller.
        let pair = {
            let mut guard = match handle.pair.lock() {
                Ok(g) => g,
                Err(_) => {
                    set_last_error(TstError::Internal, "rtsp session mutex poisoned");
                    return std::ptr::null_mut();
                }
            };
            match guard.take() {
                Some(p) => p,
                None => {
                    set_last_error(TstError::Closed, "rtsp session already consumed or closed");
                    return std::ptr::null_mut();
                }
            }
        };

        // The client moves into the receiver: dropping it here would send
        // TEARDOWN, stop its thread and end an interleaved stream as a cancel
        // (`TST_E_CLOSED`) after the queued backlog.
        let (client, rtsp_session) = *pair;

        // Step 2: convert RtspSession into RtpRecvTransport.
        // For UDP: wraps the SETUP-allocated UDP socket pair.
        // For TCP-interleaved: wraps the mpsc::Receiver<Bytes> the client's
        // thread fills.
        // into_recv_transport() also swaps in the owning RtspClient's shared
        // end-reason slot (see its doc), so end_reason_handle() below
        // captures a handle onto reasons recorded by the RTSP client's
        // thread too, not just this transport's own close.
        let transport = rtsp_session.into_recv_transport();

        // Step 3: wrap in DemuxReceiver with caller-supplied config. Both
        // observers are captured BEFORE the transport moves into the shell.
        let cancel = cancel_or_latch(transport.cancel_handle());
        let end_reason = transport.end_reason_handle();
        let receiver = if let Some(cfg) = unsafe { demux_cfg.as_ref() } {
            DemuxReceiver::with_demux_options(transport, cfg.build_options())
        } else {
            DemuxReceiver::new(transport)
        };

        // Step 4: build TstRtpDemuxReceiver — same shape as the one returned
        // by tst_rtp_demux_receiver_open, so the caller can use the full
        // tst_rtp_demux_receiver_* data-path API without any distinction.
        use crate::event::EventArena;
        use crate::rtp::RtpRecvSnap;

        // Nothing below can fail, so this is where the session is consumed:
        // free its allocation only now, after every path that could still
        // return NULL (a panic above included), so a NULL return always
        // leaves it with the caller.
        // SAFETY: `session` came from `Box::into_raw` in
        // `tst_rtsp_client_builder_connect`, its mutex guard is released,
        // and the caller does not use it after a non-NULL return.
        drop(unsafe { Box::from_raw(session) });

        let ts_bytes_demuxed = receiver.ts_bytes_demuxed_handle();
        Box::into_raw(Box::new(TstRtpDemuxReceiver {
            ts_bytes_demuxed,
            // The same cell on the handle too: a receive refused after
            // `_cancel` records `Cancelled` there.
            inner: CHandle::new(
                receiver,
                cancel,
                RtpRecvSnap {
                    end_reason: end_reason.clone(),
                },
            )
            .with_end_reason(end_reason),
            arena: Mutex::new(EventArena::new()),
            stream_stats_buf: Mutex::new(Vec::new()),
            rtsp_client: Mutex::new(Some(client)),
            mount_cancel: None,
        }))
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::null_entry_tests;

    null_entry_tests! {
        null_connect_returns_null => null { tst_rtsp_client_builder_connect(std::ptr::null_mut()) };
        null_play_returns_invalid_config => invalid_config {
            tst_rtsp_session_play(std::ptr::null_mut())
        };
        null_pause_returns_invalid_config => invalid_config {
            tst_rtsp_session_pause(std::ptr::null_mut())
        };
        null_teardown_and_free_is_noop => eq(0) {
            tst_rtsp_session_teardown_and_free(std::ptr::null_mut())
        };
        null_cancel_returns_invalid_config => invalid_config {
            tst_rtsp_session_cancel(std::ptr::null_mut())
        };
        null_into_demux_receiver_returns_null => null {
            tst_rtsp_session_into_demux_receiver(std::ptr::null_mut(), std::ptr::null())
        };
    }

    /// A NULL return leaves the session with the caller: the bridge neither
    /// frees it (the teardown below would double-free) nor may the caller
    /// skip freeing it (LeakSanitizer would report the allocation).
    #[test]
    fn bridge_with_an_emptied_pair_returns_null_and_keeps_the_session() {
        let p = Box::into_raw(Box::new(TstRtspSession {
            pair: Mutex::new(None),
            rtsp_cancel: RtspCancelHandle::new(),
        }));
        let rx = unsafe { tst_rtsp_session_into_demux_receiver(p, std::ptr::null()) };
        assert!(rx.is_null());
        assert_eq!(
            crate::error::test_last_error_code(),
            TstError::Closed as i32
        );
        assert_eq!(unsafe { tst_rtsp_session_teardown_and_free(p) }, 0);
    }

    /// The defence path (a call still holding the session, outside the
    /// contract): the free fires the cancel first, waits for the holder to
    /// let go, then closes the cancelled client. Its TEARDOWN is written
    /// without awaiting the reply, so the code is `TST_E_CLOSED`.
    #[test]
    fn teardown_and_free_with_the_pair_held_cancels_first_then_closes() {
        use std::ffi::{CStr, CString};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let b = crate::handle::TstRtspServerBuilder::from_url("rtsp://127.0.0.1:0")
            .expect("test url parses");
        let raw_b = crate::handle::TstRtspServerBuilder::into_raw(Box::new(b));
        let server = unsafe { crate::rtsp::server::start::tst_rtsp_server_builder_start(raw_b) };
        assert!(!server.is_null());
        let cfg = unsafe { crate::config::tst_mux_config_new() };
        let prog = unsafe { crate::config::tst_mux_config_add_program(cfg, 1, 0x1000) };
        unsafe {
            crate::config::streams::tst_mux_config_add_video_stream(
                cfg,
                prog,
                0x1011,
                crate::config::streams::TstVideoCodec::H264 as i32,
            )
        };
        let path = CString::new("/live").unwrap();
        let mount = unsafe {
            crate::rtsp::server::mount::tst_rtsp_server_add_unicast_mount(
                server,
                path.as_ptr(),
                cfg as *const _,
            )
        };
        unsafe { crate::config::tst_mux_config_free(cfg) };
        assert!(!mount.is_null());
        let mut buf = [0 as std::ffi::c_char; 64];
        assert!(
            unsafe {
                crate::rtsp::server::stop::tst_rtsp_server_local_addr(
                    server,
                    buf.as_mut_ptr(),
                    buf.len(),
                )
            } > 0
        );
        let addr = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        let url = CString::new(format!("rtsp://{addr}/live")).unwrap();
        let cb = unsafe { crate::rtsp::client::builder::tst_rtsp_client_builder_new(url.as_ptr()) };
        let session = unsafe { tst_rtsp_client_builder_connect(cb) };
        assert!(
            !session.is_null(),
            "{}",
            crate::error::test_last_error_msg()
        );

        let cancel = unsafe { (*session).rtsp_cancel.clone() };
        let session_addr = session as usize;
        let (held_tx, held_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            // Stands in for a PLAY parked on another thread.
            let s = unsafe { &*(session_addr as *const TstRtspSession) };
            let guard = s.pair.lock().unwrap();
            held_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !cancel.is_cancelled() {
                assert!(Instant::now() < deadline, "the free never fired the cancel");
                std::thread::sleep(Duration::from_millis(5));
            }
            drop(guard);
        });
        held_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("holder took the pair");
        let rc = unsafe { tst_rtsp_session_teardown_and_free(session) };
        holder.join().unwrap();
        assert_eq!(
            rc,
            TstError::Closed as i32,
            "{}",
            crate::error::test_last_error_msg()
        );

        unsafe { crate::rtsp::server::mount::tst_rtsp_mount_handle_free(mount) };
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }
}
