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
//!  ┌─  tst_rtsp_session_into_demux_receiver() ───────►│   consumes session
//!  │                                                   │   returns TstRtpDemuxReceiver
//!  └──► tst_rtp_demux_receiver_next_event()  ────────────► event loop
//!       tst_rtp_demux_receiver_cancel()
//!       tst_rtp_demux_receiver_close()
//!
//!   OR, to close without consuming the transport:
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
//! a `Mutex` gives close-idempotence and panic isolation identical to `Handle<T>`,
//! but lets `into_demux_receiver` consume the pair (move it out of the `Option`)
//! in a single lock acquisition.  Using `Handle<T>` directly is not possible
//! here because `with_inner_mut` does not support moving values out of the
//! closure.
//!
//! # Cancel semantics
//!
//! Cancel signals the `RtspClient`'s internal cancel flag, breaking out of any
//! blocking I/O on the control channel.  It does NOT cancel the RTP data plane —
//! once `into_demux_receiver` has been called, cancel on the returned
//! `TstRtpDemuxReceiver` governs data-plane shutdown.

use std::sync::Mutex;

use secrecy::SecretString;
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
/// [`tst_rtsp_session_teardown_and_free`] (sends TEARDOWN first) or
/// [`tst_rtsp_session_into_demux_receiver`] (consumes the data-plane
/// transport into a new `TstRtpDemuxReceiver` handle).
///
/// Call [`tst_rtsp_session_play`] after obtaining the session handle to
/// begin the RTP data flow; the builder's `_connect` step only runs
/// OPTIONS + DESCRIBE + SETUP, not PLAY.
pub struct TstRtspSession {
    /// Mutex-wrapped optional pair `(RtspClient, RtspSession)`.
    ///
    /// Using `Mutex<Option<Box<(…)>>>` rather than `Handle<Box<(…)>>` lets
    /// `into_demux_receiver` move the value out of the `Option` in a single
    /// lock acquisition.  `Handle::with_inner_mut` does not support consuming
    /// moves.  `None` after `into_demux_receiver` or `teardown_and_free`
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
/// 1. Reconstructs an `RtspClientBuilder` from fields stored by the
///    `tst_rtsp_client_builder_*` setters.
/// 2. Calls `.connect()` to open the TCP control channel and spawn the
///    auto-keepalive thread (unless disabled via `tst_rtsp_client_builder_keepalive`).
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
        let b = unsafe { TstRtspClientBuilder::from_raw(builder) };

        // Build the URL string, applying any transport-preference override.
        let mut url_to_use = b.url.clone();
        if let Some(pref) = b.transport_pref {
            url_to_use.transport_preference = pref;
        }
        let url_str = url_to_use.render_no_credentials();

        // Reconstruct the Rust builder from the stored fields.
        let mut rust_builder = match RtspClientBuilder::new(&url_str) {
            Ok(rb) => rb,
            Err(e) => {
                crate::error::record_with_context(e, "RTSP URL parse error");
                return std::ptr::null_mut();
            }
        };

        // Apply auth credentials if set by any _auth_* setter.
        if let (Some(user), Some(pass)) = (b.username.clone(), b.password.clone()) {
            rust_builder = rust_builder.auth(user, SecretString::from(pass));
        }

        // Apply keepalive preference: builder default is auto-keepalive enabled.
        if !b.auto_keepalive {
            rust_builder = rust_builder.no_auto_keepalive(true);
        }

        // TLS root certificates: stored by tst_rtsp_client_builder_tls_root_cert_pem
        // for rtsps:// connections, but parsing them into a rustls::RootCertStore
        // requires the tst-rtp `tls` cargo feature, which tst-c does not enable
        // directly. When a user calls _tls_root_cert_pem and then _connect on a
        // plain `rtsp://` URL the PEM bytes are silently unused (harmless).
        // For rtsps:// URLs without custom certs the platform trust store is used.
        // Custom PEM cert injection for rtsps:// via the C ABI is deferred to a
        // future tst-c `tls` feature (see docs/project/deferred-features.md).

        // Step 2: TCP connect + optional keepalive-thread spawn.
        let mut client = match rust_builder.connect() {
            Ok(c) => c,
            Err(e) => {
                crate::error::record_with_context(e, "RTSP connect failed");
                return std::ptr::null_mut();
            }
        };

        // Obtain the cancel handle before any blocking I/O so that
        // tst_rtsp_session_cancel can signal it from another thread while
        // DESCRIBE or SETUP is in flight. RtspCancelHandle is Clone + Send + Sync.
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
/// Sends an RTSP TEARDOWN request, then unconditionally drops the session
/// and its `RtspClient` regardless of whether TEARDOWN succeeded (network
/// errors at teardown time are recorded in last-error but do not prevent
/// the handle from being freed).
///
/// After this call the `session` pointer is invalid; any further use is
/// undefined behavior. NULL is a no-op.
///
/// # Safety
///
/// `session` must be NULL, or a pointer returned by
/// `tst_rtsp_client_builder_connect` that has not yet been freed or
/// consumed by [`tst_rtsp_session_into_demux_receiver`].
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
        // Signal cancel so any concurrent blocking I/O wakes up.
        boxed.rtsp_cancel.cancel();
        // Attempt TEARDOWN; surface last-error but don't block the free.
        let rc = {
            let mut guard = match boxed.pair.lock() {
                Ok(g) => g,
                Err(_) => {
                    // Mutex poisoned — skip teardown, still free.
                    set_last_error(TstError::Internal, "rtsp session mutex poisoned");
                    return TstError::Internal as i32;
                }
            };
            match guard.as_mut() {
                None => 0, // already consumed
                Some(pair) => {
                    let (client, _session) = pair.as_mut();
                    match client.teardown() {
                        Ok(()) => 0,
                        Err(e) => crate::error::record_with_context(
                            e,
                            "RTSP TEARDOWN failed (continuing free)",
                        ),
                    }
                }
            }
            // guard released; pair is still in the Mutex but boxed drops next.
        };
        // Drop the box (drops RtspClient + RtspSession).
        drop(boxed);
        rc
    })
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// Cancel any blocking RTSP I/O on the control channel.
///
/// Sets the session's cancel flag so that any thread blocked inside a RTSP
/// request/response cycle (e.g. a blocking DESCRIBE or PLAY) will break out
/// at the next poll interval.  Safe to call from any thread. Idempotent.
///
/// Note: cancels the RTSP *control plane* only. If
/// [`tst_rtsp_session_into_demux_receiver`] has already been called, the
/// RTP data plane is governed by `tst_rtp_demux_receiver_cancel`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `session` is NULL.
///
/// # Safety
///
/// `session` must be NULL or a valid non-freed `*mut TstRtspSession`.
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
/// 1. Locks the session's pair Mutex and moves out `(RtspClient, RtspSession)`.
/// 2. Calls `RtspSession::into_recv_transport()` to get the `RtpRecvTransport`
///    (either a bound UDP socket or an mpsc channel fed by the TCP-interleaved
///    pump thread, depending on what SETUP negotiated).
/// 3. Wraps the transport in a `DemuxReceiver` using the supplied `demux_cfg`
///    (or default options if NULL).
/// 4. Returns a `*mut TstRtpDemuxReceiver` using the same opaque type as
///    `tst_rtp_demux_receiver_open`, so the caller can use the existing
///    `tst_rtp_demux_receiver_next_event` / `_cancel` / `_close` data-path
///    entry points unchanged.
///
/// After this call the `session` handle is consumed (the inner pair is None).
/// The RTSP control channel (`RtspClient`) is also dropped — if you need to
/// send TEARDOWN before consuming the transport, call
/// [`tst_rtsp_session_teardown_and_free`] instead.
///
/// The returned `tst_rtp_demux_receiver_t` must eventually be freed with
/// `tst_rtp_demux_receiver_close`.  Returns NULL on failure.
///
/// # Safety
///
/// - `session` must be a non-NULL, non-consumed pointer from
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

        // Step 1: move the (RtspClient, RtspSession) pair out of the Mutex.
        // After this, `pair` is None and subsequent calls return TST_E_CLOSED.
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

        // Destructure. The RtspClient is dropped here — its Drop impl joins the
        // keepalive thread and (for TCP-interleaved) the pump thread.  This is
        // intentional: the RTSP control channel is no longer needed once we hand
        // the data-plane transport to DemuxReceiver.
        let (_client, rtsp_session) = *pair;

        // Step 2: convert RtspSession into RtpRecvTransport.
        // For UDP: wraps the SETUP-allocated UDP socket pair.
        // For TCP-interleaved: wraps the mpsc::Receiver<Bytes> fed by the pump.
        // into_recv_transport() also swaps in the owning RtspClient's shared
        // end-reason slot (see its doc), so end_reason_handle() below
        // captures a handle onto reasons recorded by the RTSP
        // keepalive/pump threads too, not just this transport's own close.
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

        Box::into_raw(Box::new(TstRtpDemuxReceiver {
            inner: CHandle::new(receiver, cancel, RtpRecvSnap { end_reason }),
            arena: Mutex::new(EventArena::new()),
            stream_stats_buf: Mutex::new(Vec::new()),
        }))
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_connect_returns_null() {
        let p = unsafe { tst_rtsp_client_builder_connect(std::ptr::null_mut()) };
        assert!(p.is_null());
    }

    #[test]
    fn null_play_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_session_play(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_pause_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_session_pause(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_teardown_and_free_is_noop() {
        let rc = unsafe { tst_rtsp_session_teardown_and_free(std::ptr::null_mut()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn null_cancel_returns_invalid_config() {
        let rc = unsafe { tst_rtsp_session_cancel(std::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn null_into_demux_receiver_returns_null() {
        let p =
            unsafe { tst_rtsp_session_into_demux_receiver(std::ptr::null_mut(), std::ptr::null()) };
        assert!(p.is_null());
    }
}
