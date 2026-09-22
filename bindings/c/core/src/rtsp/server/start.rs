//! `tst_rtsp_server_builder_start` — consume a builder and start the server.
//!
//! The builder is allocated by `tst_rtsp_server_builder_new` and
//! configured via the `tst_rtsp_server_builder_*` setter chain. This entry
//! point consumes the builder, calls `RtspServerBuilder::build()` to create
//! the internal tokio Runtime, then calls `RtspServer::start()` to spawn the
//! listener task and wait for the bind to complete.
//!
//! On success the builder pointer is **consumed** (freed). On failure the
//! builder is also freed; the caller should check
//! `tst_get_last_error()` / `tst_get_last_error_str()`.
//!
//! # Server lifecycle (C ABI perspective)
//!
//! ```text
//! tst_rtsp_server_builder_new(url)
//!     │
//!     ├─ tst_rtsp_server_builder_auth_basic(…)     ← optional
//!     ├─ tst_rtsp_server_builder_max_sessions(…)   ← optional
//!     └─ tst_rtsp_server_builder_fanout_cap(…)     ← optional
//!
//! tst_rtsp_server_builder_start(builder)
//!     │ ← consumes builder; spawns tokio Runtime + listener
//!     ▼
//! TstRtspServer*  ← opaque handle
//!     │
//!     ├─ tst_rtsp_server_add_unicast_mount(…)    → TstRtspMountHandle*
//!     ├─ tst_rtsp_server_add_multicast_mount(…)  → TstRtspMountHandle*
//!     ├─ tst_rtsp_mount_handle push_*(…)
//!     ├─ tst_rtsp_server_stats(…)
//!     └─ tst_rtsp_server_stop(…) / tst_rtsp_server_free(…)
//! ```
//!
//! # Error mapping
//!
//! `RtspServerError` variants all map to `TST_E_RTSP_SERVER` (code -24) via
//! `crate::error::record_with_context` (the shared `From<RtspServerError>`
//! row of the binding kind table). The detail string from the
//! Rust `Display` impl is forwarded into the thread-local last-error message.

use crate::error::{TstError, set_last_error};
use crate::handle::TstRtspServerBuilder;
use crate::panic::ffi_catch;
use crate::rtsp::server::types::TstRtspServer;

/// Consume a builder and start the RTSP server.
///
/// Internally:
///
/// 1. Validates and consumes the `TstRtspServerBuilder` into a
///    `tst_rtp::RtspServerBuilder`.
/// 2. Calls `RtspServerBuilder::build()` to construct the tokio `Runtime`
///    and allocate the `ServerState`.
/// 3. Calls `RtspServer::start()` to spawn the listener task and spin-wait
///    up to 1 s for the listener to bind.
///
/// On success the `builder` pointer is consumed (freed). On failure the
/// builder is also freed; check `tst_get_last_error()` for the negative
/// `TST_E_*` code and `tst_get_last_error_str()` for a human-readable
/// message.
///
/// Returns a non-NULL `tst_rtsp_server_t*` on success, NULL on failure.
/// The returned pointer must eventually be freed:
/// - Call `tst_rtsp_server_stop` for graceful shutdown (sends RFC 7826
///   Notice 5402 "Server-Initiated TEARDOWN" to each active session), then
///   `tst_rtsp_server_free` to release the handle.
/// - Or simply `tst_rtsp_server_free` for a hard-cancel Drop path.
///
/// # Safety
///
/// `builder` must be a non-NULL pointer returned by
/// `tst_rtsp_server_builder_new` that has not yet been freed or consumed.
/// After this call the `builder` pointer is invalid regardless of success or
/// failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_start(
    builder: *mut TstRtspServerBuilder,
) -> *mut TstRtspServer {
    ffi_catch(std::ptr::null_mut(), || {
        if builder.is_null() {
            set_last_error(TstError::InvalidConfig, "builder is null");
            return std::ptr::null_mut();
        }
        // SAFETY: caller guarantees valid, unaliased pointer from _new.
        // Consuming the builder here — it will not be accessible after this
        // call whether we succeed or fail.
        let b = unsafe { TstRtspServerBuilder::from_raw(builder) };

        // Step 1: build the RtspServer (allocates tokio Runtime + ServerState).
        let server = match b.build_server() {
            Ok(s) => s,
            Err(e) => {
                crate::error::record_with_context(e, "RTSP server build failed");
                return std::ptr::null_mut();
            }
        };

        // Step 2: start the listener. This spawns the tokio listener task
        // and spin-waits up to 1 s for the kernel to assign a local address.
        // On port-0 binds, local_addr() is authoritative after start().
        if let Err(e) = server.start() {
            crate::error::record_with_context(e, "RTSP server start failed");
            // server is dropped here — its Drop impl fires the hard-cancel
            // path and shuts down the tokio Runtime cleanly.
            return std::ptr::null_mut();
        }

        // Wrap in the opaque handle. cancel_handle() is cloned before the
        // Box is moved into the Mutex so tst_rtsp_server_cancel_handle can
        // fire it without acquiring the inner Mutex.
        Box::into_raw(Box::new(TstRtspServer::new(server)))
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_builder_returns_null() {
        // null builder → null return, last-error is TST_E_INVALID_CONFIG.
        // Does not require the server to actually start.
        let p = unsafe { tst_rtsp_server_builder_start(std::ptr::null_mut()) };
        assert!(p.is_null());
        let code = unsafe { crate::error::tst_get_last_error() };
        assert_eq!(code, TstError::InvalidConfig as i32);
    }

    /// Integration smoke: build a real server at 127.0.0.1:0 via the builder
    /// helpers and verify _start returns a non-null handle.
    #[test]
    fn valid_builder_starts_server_and_returns_handle() {
        // Construct the builder directly via the Rust path (rather than the
        // C setter-chain entry points) — exercises what _start delegates to
        // without depending on the builder module's own coverage.
        let b = TstRtspServerBuilder::from_url("rtsp://127.0.0.1:0").expect("test url parses");
        let raw = TstRtspServerBuilder::into_raw(Box::new(b));

        let handle = unsafe { tst_rtsp_server_builder_start(raw) };
        assert!(!handle.is_null(), "start returned null; last error: {}", {
            let s = unsafe { std::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str()) };
            s.to_str().unwrap_or("<invalid utf8>")
        });

        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(handle) };
    }

    /// TLS PEM bytes on a builder must fail `_start` rather than silently
    /// starting a PLAINTEXT server: tst-c is built without tst-rtp's `tls`
    /// feature, so the stored bytes can never take effect. The PEM content
    /// is never parsed (the guard fires before any TLS machinery), so
    /// dummy bytes suffice.
    #[test]
    fn tls_cert_pem_on_plaintext_bind_fails_start() {
        let b = TstRtspServerBuilder::from_url("rtsp://127.0.0.1:0").expect("test url parses");
        let raw = TstRtspServerBuilder::into_raw(Box::new(b));
        let cert = b"-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n";
        let key = b"-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n";
        unsafe {
            crate::rtsp::server::builder::tst_rtsp_server_builder_tls_cert_pem(
                raw,
                cert.as_ptr(),
                cert.len(),
                key.as_ptr(),
                key.len(),
            );
        }

        let handle = unsafe { tst_rtsp_server_builder_start(raw) };
        assert!(
            handle.is_null(),
            "start must refuse TLS bytes it cannot honor"
        );
        let code = unsafe { crate::error::tst_get_last_error() };
        // WP-A2 K5: `RtspServerError::Tls` now carries the RTSP `TLS` kind
        // (TST_E_RTSP_TLS, -21) instead of the catch-all -24 the retired
        // `rtsp_server_error_to_code` emitted for every server variant.
        assert_eq!(code, TstError::RtspTls as i32);
        let msg = unsafe { std::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str()) }
            .to_str()
            .unwrap();
        assert!(msg.contains("TLS"), "message must name the cause: {msg}");
    }
}
