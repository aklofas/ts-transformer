//! `tst_rtsp_server_builder_*` C entry points.
//!
//! Wraps `tst_rtp::RtspServerBuilder` with a mutable opaque handle so C
//! callers can configure a server incrementally before calling
//! `tst_rtsp_server_builder_start`.
//!
//! # Builder pattern divergence from the Rust API
//!
//! `RtspServerBuilder` uses `&mut self -> &mut Self` chain setters (which
//! is already FFI-friendly on the Rust side), but the C ABI stores fields
//! independently in `TstRtspServerBuilder` (see `bindings/c/core/src/handle.rs`)
//! and reconstructs the Rust builder from them at `start` time. This matches
//! the `TstRtspClientBuilder` pattern, keeps the opaque struct layout stable
//! across future Rust API changes, and avoids holding a partially-constructed
//! `RtspServerBuilder` inside an `Option<T>` across C setter calls.
//!
//! # Auth scheme
//!
//! Unlike the client builder (where the server negotiates the scheme),
//! the *server* builder pre-selects the scheme. The three auth entry points
//! (`_auth_basic`, `_auth_digest_md5`, `_auth_digest_sha256`) each record
//! the scheme alongside the credentials. The last call wins — calling
//! multiple auth setters overwrites the previous selection, matching
//! `RtspServerBuilder`'s own "last-call-wins" behavior.
//!
//! # TLS certificate + key
//!
//! `RtspServerBuilder::tls_cert` (only present when tst-rtp's
//! `rtsp-server-tls` feature is compiled in) takes `PathBuf` arguments
//! pointing at files on disk. The C ABI instead stores the raw PEM bytes in
//! `TstRtspServerBuilder`, both the cert chain and the private key
//! together, via a single `tls_cert_pem` call. TLS is not available in this
//! build of tst-c (there is no `tls` or `rtsp-server-tls` cargo feature
//! yet): `tst_rtsp_server_builder_start` FAILS (`TST_E_RTSP_SERVER`)
//! whenever these bytes are set — the alternative would be a server that
//! silently starts PLAINTEXT with the supplied certs ignored.
//!
//! # C ABI cross-reference
//!
//! The following entry points land in this module (no 1:1 Rust counterpart
//! in tst-pipeline / tst-srt / tst-core):
//!
//! - `tst_rtsp_server_builder_new` — allocate + parse bind URL
//! - `tst_rtsp_server_builder_bind` — change / override the bind URL
//! - `tst_rtsp_server_builder_auth_basic` — Basic auth credentials
//! - `tst_rtsp_server_builder_auth_digest_md5` — Digest MD5 credentials
//! - `tst_rtsp_server_builder_auth_digest_sha256` — Digest SHA-256 credentials
//! - `tst_rtsp_server_builder_max_sessions` — cap on concurrent sessions
//! - `tst_rtsp_server_builder_session_timeout` — session timeout in seconds
//! - `tst_rtsp_server_builder_fanout_capacity` — broadcast channel capacity
//! - `tst_rtsp_server_builder_graceful_shutdown_drain_ms` — drain window in ms
//! - `tst_rtsp_server_builder_tls_cert_pem` — TLS cert chain + private key PEM
//! - `tst_rtsp_server_builder_free` — discard without starting

use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr;

use crate::error::{TstError, set_last_error};
use crate::handle::TstRtspServerBuilder;
use crate::panic::ffi_catch;

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// Allocate a new RTSP server builder for binding to `addr`.
///
/// `addr` must be a NUL-terminated `rtsp://` or `rtsps://` URL string
/// specifying the bind address and port (e.g. `"rtsp://0.0.0.0:8554"`).
/// The host must be a literal IP address — DNS names are rejected at this
/// step because the OS kernel, not a resolver, performs the actual bind.
///
/// Auth, TLS, session limits, and drain policy may be set with the
/// `_auth_*`, `_tls_cert_pem`, `_max_sessions`, `_session_timeout`,
/// `_fanout_capacity`, and `_graceful_shutdown_drain_ms` setters before
/// calling `tst_rtsp_server_builder_start` to bind the listener and begin
/// accepting connections.
///
/// Returns a non-NULL builder pointer on success, or NULL with the
/// thread-local last-error populated on failure (bad URL, non-IP-literal
/// host, allocation failure, etc.).  The caller must eventually pass the
/// pointer to `tst_rtsp_server_builder_start` (which consumes it) or
/// `tst_rtsp_server_builder_free` (which discards it).
///
/// # Safety
///
/// `addr` must be a valid, NUL-terminated C string that lives for the
/// duration of this call.  The returned pointer is owned by the caller
/// and must not be aliased.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_new(
    addr: *const c_char,
) -> *mut TstRtspServerBuilder {
    ffi_catch(ptr::null_mut(), || {
        if addr.is_null() {
            set_last_error(TstError::InvalidConfig, "addr is null");
            return ptr::null_mut();
        }
        // SAFETY: caller guarantees NUL-terminated, valid-for-this-call.
        let addr_str = match unsafe { CStr::from_ptr(addr) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(TstError::InvalidConfig, "addr is not valid UTF-8");
                return ptr::null_mut();
            }
        };
        match TstRtspServerBuilder::from_url(addr_str) {
            Ok(b) => TstRtspServerBuilder::into_raw(Box::new(b)),
            Err(e) => {
                set_last_error(TstError::InvalidConfig, &format!("URL parse error: {e}"));
                ptr::null_mut()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Bind address override
// ---------------------------------------------------------------------------

/// Change or override the bind address.
///
/// `addr` must be a NUL-terminated `rtsp://` or `rtsps://` URL string.
/// Replaces the address set at `tst_rtsp_server_builder_new` time.  Call
/// this to redirect the server to a different interface or port without
/// allocating a new builder.
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder` is
/// NULL, `addr` is NULL, or the URL fails to parse.
///
/// Must be called before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// - `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`,
///   and not yet freed or consumed.
/// - `addr` must be a valid, NUL-terminated C string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_bind(
    builder: *mut TstRtspServerBuilder,
    addr: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        if addr.is_null() {
            set_last_error(TstError::InvalidConfig, "addr is null");
            return;
        }
        // SAFETY: caller guarantees NUL-terminated, valid-for-this-call.
        let addr_str = match unsafe { CStr::from_ptr(addr) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error(TstError::InvalidConfig, "addr is not valid UTF-8");
                return;
            }
        };
        match tst_rtp::url::RtspUrl::parse(addr_str) {
            Ok(url) => b.bind_url = url,
            Err(e) => set_last_error(
                TstError::InvalidConfig,
                &format!("bind URL parse error: {e}"),
            ),
        }
    });
}

// ---------------------------------------------------------------------------
// Auth setters
// ---------------------------------------------------------------------------

/// Require HTTP Basic authentication (RFC 7617) from connecting clients.
///
/// `user` and `pass` are the accepted credentials (NUL-terminated UTF-8).
/// The auth realm defaults to `"tst-rtp"`.  The strings are copied
/// immediately; the caller's buffers need not outlive this call.
///
/// Calling any `_auth_*` entry point multiple times overwrites the
/// previous selection — the last call wins (matches
/// `RtspServerBuilder::auth_basic` behavior).
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder`,
/// `user`, or `pass` is NULL, or if either string is not valid UTF-8.
/// Must be called before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// - `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`.
/// - `user` and `pass` must each be a valid, NUL-terminated C string valid
///   for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_auth_basic(
    builder: *mut TstRtspServerBuilder,
    user: *const c_char,
    pass: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        // SAFETY: user+pass lifetime guaranteed by caller for this call.
        if let Some((u, p)) = unsafe { extract_credentials_pair(user, pass, "auth_basic") } {
            b.auth_scheme = Some(TstRtspServerAuthScheme::Basic);
            b.auth_username = Some(u);
            b.auth_password = Some(p);
        }
    });
}

/// Require HTTP Digest MD5 authentication (RFC 7616 §3.4) from connecting
/// clients.
///
/// `user` and `pass` are the accepted credentials (NUL-terminated UTF-8).
/// The auth realm defaults to `"tst-rtp"`.  The strings are copied
/// immediately; the caller's buffers need not outlive this call.
///
/// Calling any `_auth_*` entry point multiple times overwrites the
/// previous selection — the last call wins.
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder`,
/// `user`, or `pass` is NULL, or if either string is not valid UTF-8.
/// Must be called before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// - `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`.
/// - `user` and `pass` must each be a valid, NUL-terminated C string valid
///   for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_auth_digest_md5(
    builder: *mut TstRtspServerBuilder,
    user: *const c_char,
    pass: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        // SAFETY: user+pass lifetime guaranteed by caller for this call.
        if let Some((u, p)) = unsafe { extract_credentials_pair(user, pass, "auth_digest_md5") } {
            b.auth_scheme = Some(TstRtspServerAuthScheme::DigestMd5);
            b.auth_username = Some(u);
            b.auth_password = Some(p);
        }
    });
}

/// Require HTTP Digest SHA-256 authentication (RFC 7616 §3.4) from
/// connecting clients.
///
/// `user` and `pass` are the accepted credentials (NUL-terminated UTF-8).
/// The auth realm defaults to `"tst-rtp"`.  The strings are copied
/// immediately; the caller's buffers need not outlive this call.
///
/// Calling any `_auth_*` entry point multiple times overwrites the
/// previous selection — the last call wins.
///
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `builder`,
/// `user`, or `pass` is NULL, or if either string is not valid UTF-8.
/// Must be called before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// - `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`.
/// - `user` and `pass` must each be a valid, NUL-terminated C string valid
///   for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_auth_digest_sha256(
    builder: *mut TstRtspServerBuilder,
    user: *const c_char,
    pass: *const c_char,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        // SAFETY: user+pass lifetime guaranteed by caller for this call.
        if let Some((u, p)) = unsafe { extract_credentials_pair(user, pass, "auth_digest_sha256") }
        {
            b.auth_scheme = Some(TstRtspServerAuthScheme::DigestSha256);
            b.auth_username = Some(u);
            b.auth_password = Some(p);
        }
    });
}

// ---------------------------------------------------------------------------
// Session / fanout / lifecycle setters
// ---------------------------------------------------------------------------

/// Set the maximum number of concurrent client sessions.
///
/// When the cap is reached, new incoming connections are accepted at the TCP
/// level and then immediately closed (with a `tracing::warn!` diagnostic).
/// This prevents the OS from accumulating a backlog of half-open connections
/// while still enforcing a resource ceiling.
///
/// `n` is floored to 1 (zero is treated as 1). Default: 64.
/// Must be called before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`,
/// and not yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_max_sessions(
    builder: *mut TstRtspServerBuilder,
    n: u32,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.max_sessions = n.max(1),
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Let an ANNOUNCE to an unregistered path create a publish mount on
/// demand. Each such mount is handed out once by
/// `tst_rtsp_server_next_publisher`; the announcing publisher already holds
/// it. Default: false (an ANNOUNCE to an unregistered path answers `404`).
///
/// Security: with this on, anyone who can reach the port (and pass its
/// auth, when configured) can create mounts. An on-demand mount stays in the
/// server after its publisher leaves, until `tst_rtsp_server_remove_mount`
/// removes it; at most 256 exist at once, and an ANNOUNCE that would create
/// one more answers `503`. A connected elementary-track publisher can hold
/// about 46 MB, so configure auth and size
/// `tst_rtsp_server_builder_max_sessions` before enabling this on a
/// reachable port.
///
/// Must be called before `tst_rtsp_server_builder_start`. A NULL `builder`
/// sets last-error `TST_E_INVALID_CONFIG`.
///
/// # Safety
///
/// `builder` must be NULL or returned by `tst_rtsp_server_builder_new`, not
/// yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_accept_unregistered_publishers(
    builder: *mut TstRtspServerBuilder,
    accept: bool,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.accept_unregistered_publishers = accept,
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Set the session timeout in seconds.
///
/// The server advertises this value to clients in the `Session:
/// <id>;timeout=N` response header. Clients are expected to send keepalive
/// pings at `N/2`; sessions that exceed `N` without any request may be
/// dropped (server policy). Defaults to 60 s.
///
/// `secs` is used as-is; a value of 0 means no advertised timeout
/// (not recommended for production). Must be called before
/// `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`,
/// and not yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_session_timeout(
    builder: *mut TstRtspServerBuilder,
    secs: u32,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.session_timeout_secs = secs,
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Set the per-mount broadcast channel capacity (frame count).
///
/// Each mount maintains an internal broadcast channel (tokio `broadcast`)
/// between the `MountHandle` push path and per-client fanout tasks. When a
/// client's task cannot keep up, the oldest enqueued frames are dropped for
/// that client; the per-peer dropped-frame counter increments but the muxer
/// is never back-pressured.
///
/// `cap` is floored to 1. Default: 256 frames. Must be called before
/// `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`,
/// and not yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_fanout_capacity(
    builder: *mut TstRtspServerBuilder,
    cap: u32,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.fanout_capacity = cap.max(1),
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Set the graceful-shutdown drain window in milliseconds.
///
/// When `tst_rtsp_server_stop` is called, the server sends an RFC
/// 7826 §13.5.1 Notice 5402 ("Server-Initiated TEARDOWN") ANNOUNCE to each
/// active session, then waits up to `ms` milliseconds (plus 1 s fixed
/// overhead) for in-flight RTP frames to drain before closing the listener
/// and runtime. Default: 100 ms.
///
/// Must be called before `tst_rtsp_server_builder_start`.
///
/// # Safety
///
/// `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`,
/// and not yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_graceful_shutdown_drain_ms(
    builder: *mut TstRtspServerBuilder,
    ms: u32,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.graceful_shutdown_drain_ms = ms,
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

// ---------------------------------------------------------------------------
// TLS setter
// ---------------------------------------------------------------------------

/// Supply a PEM-encoded TLS certificate chain and private key for
/// `rtsps://` binds.
///
/// `cert` must point to `cert_len` bytes of PEM-encoded certificate data
/// (one or more X.509 certificates in `-----BEGIN CERTIFICATE-----` /
/// `-----END CERTIFICATE-----` blocks, leaf first).  `key` must point to
/// `key_len` bytes of a PEM-encoded PKCS#8 or SEC1 private key in
/// `-----BEGIN PRIVATE KEY-----` / `-----END PRIVATE KEY-----` form.
///
/// Both buffers are copied immediately; the caller's pointers need not
/// outlive this call.
///
/// TLS is not available in this build of tst-c (there is no `tls` cargo
/// feature yet). The PEM bytes are stored on the builder, and
/// `tst_rtsp_server_builder_start` FAILS (NULL + `TST_E_RTSP_SERVER`
/// last-error) when they are set — the alternative would be a server that
/// silently starts PLAINTEXT with the supplied certs ignored. The setter and
/// its stored bytes exist so a future `tls` feature can activate them without
/// an ABI change.
///
/// No-op (with last-error set) if `builder`, `cert`, or `key` is NULL,
/// or if any length is zero.
///
/// # Safety
///
/// - `builder` must be non-NULL, returned by `tst_rtsp_server_builder_new`.
/// - `cert` must point to at least `cert_len` valid bytes.
/// - `key` must point to at least `key_len` valid bytes.
/// - No pointer needs to outlive this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_tls_cert_pem(
    builder: *mut TstRtspServerBuilder,
    cert: *const u8,
    cert_len: usize,
    key: *const u8,
    key_len: usize,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        if cert.is_null() || cert_len == 0 {
            set_last_error(
                TstError::InvalidConfig,
                "tls_cert_pem: cert is null or cert_len is 0",
            );
            return;
        }
        if key.is_null() || key_len == 0 {
            set_last_error(
                TstError::InvalidConfig,
                "tls_cert_pem: key is null or key_len is 0",
            );
            return;
        }
        let cert_bytes = match unsafe { crate::ffi_slice::ffi_slice(cert, cert_len, "cert") } {
            Ok(s) => s,
            Err(_) => return,
        };
        let key_bytes = match unsafe { crate::ffi_slice::ffi_slice(key, key_len, "key") } {
            Ok(s) => s,
            Err(_) => return,
        };
        b.tls_cert_pem = Some(cert_bytes.to_vec());
        b.tls_key_pem = Some(key_bytes.to_vec());
    });
}

// ---------------------------------------------------------------------------
// Destructor
// ---------------------------------------------------------------------------

/// Free a builder without starting the server.
///
/// Use this on error paths where the builder was partially configured and
/// you want to discard it.  After this call the pointer is invalid; any
/// further use is undefined behavior.  NULL is a no-op.
///
/// Prefer `tst_rtsp_server_builder_start` which also consumes the builder —
/// `_free` is the error-path companion.
///
/// # Safety
///
/// `builder` must be NULL, or a pointer returned by
/// `tst_rtsp_server_builder_new` that has not yet been freed or passed to
/// `_start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_free(builder: *mut TstRtspServerBuilder) {
    ffi_catch((), || {
        if builder.is_null() {
            return;
        }
        // SAFETY: caller guarantees valid, unaliased, un-freed pointer.
        let _ = unsafe { TstRtspServerBuilder::from_raw(builder) };
        // Box drops at end of scope.
    });
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Extract (user, pass) from two NUL-terminated C strings.
///
/// Returns `None` and sets last-error if either pointer is NULL or the
/// string is not valid UTF-8.
///
/// # Safety
///
/// `user` and `pass` must each be NULL or a valid, NUL-terminated C string
/// for the duration of this call.
unsafe fn extract_credentials_pair(
    user: *const c_char,
    pass: *const c_char,
    context: &str,
) -> Option<(String, String)> {
    if user.is_null() || pass.is_null() {
        set_last_error(
            TstError::InvalidConfig,
            &format!("{context}: user or pass pointer is null"),
        );
        return None;
    }
    // SAFETY: caller guarantees valid NUL-terminated strings.
    let u = match unsafe { CStr::from_ptr(user) }.to_str() {
        Ok(s) => s.to_owned(),
        Err(_) => {
            set_last_error(
                TstError::InvalidConfig,
                &format!("{context}: username is not valid UTF-8"),
            );
            return None;
        }
    };
    let p = match unsafe { CStr::from_ptr(pass) }.to_str() {
        Ok(s) => s.to_owned(),
        Err(_) => {
            set_last_error(
                TstError::InvalidConfig,
                &format!("{context}: password is not valid UTF-8"),
            );
            return None;
        }
    };
    Some((u, p))
}

/// Auth scheme tag stored in `TstRtspServerBuilder`.
///
/// Mirrors `tst_rtp::builder::ServerAuthScheme` (which is `pub(crate)`)
/// without depending on the private Rust enum across the crate boundary at
/// field-initialization time.  `tst_rtsp_server_builder_start` maps this tag
/// back to the corresponding `RtspServerBuilder::auth_basic` /
/// `auth_digest_md5` / `auth_digest_sha256` call when reconstructing the
/// Rust builder.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TstRtspServerAuthScheme {
    Basic,
    DigestMd5,
    DigestSha256,
}
