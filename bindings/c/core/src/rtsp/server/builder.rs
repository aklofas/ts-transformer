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
//! together, via a single `tls_cert_pem` call, and hands them to the server's
//! `tls_cert_pem` in a build with the `tls` feature (`TST_HAS_TLS`). Without
//! that feature `tst_rtsp_server_builder_start` FAILS (`TST_E_RTSP_TLS`)
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
//! - `tst_rtsp_server_builder_allow_udp_media` — whether media may travel over UDP
//! - `tst_rtsp_server_builder_accept_digest_without_qop` — accept the RFC 2069 Digest answer
//! - `tst_rtsp_server_builder_max_readers` — cap on reader sessions
//! - `tst_rtsp_server_builder_free` — discard without starting

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
/// A bind URL must not carry a user name or a password: this call takes it,
/// and `tst_rtsp_server_builder_start` refuses it (NULL, last-error
/// `TST_E_INVALID_CONFIG`). Configure the server's credential with
/// `_auth_basic` or `_auth_digest_*`.
///
/// Auth, TLS, session limits, and drain policy may be set with the
/// `_auth_*`, `_tls_cert_pem`, `_max_sessions`, `_session_timeout`,
/// `_fanout_capacity`, and `_graceful_shutdown_drain_ms` setters before
/// calling `tst_rtsp_server_builder_start` to bind the listener and begin
/// accepting connections.
///
/// Returns a non-NULL builder pointer on success, or NULL with the
/// thread-local last-error populated on failure (bad URL, non-IP-literal
/// host, or a panic caught inside the call; an allocation failure aborts
/// the process).  The caller must eventually pass the
/// pointer to `tst_rtsp_server_builder_start` (which consumes it) or
/// `tst_rtsp_server_builder_free` (which discards it).
///
/// A builder is a single-thread object; see "Threading and lifetime" in the
/// header preamble.
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
        let Some(addr_str) =
            (unsafe { crate::c_str::c_str_arg(addr, TstError::InvalidConfig, "addr") })
        else {
            return ptr::null_mut();
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
/// allocating a new builder. As for `_new`, a URL that carries a user name
/// or a password is taken here and refused by
/// `tst_rtsp_server_builder_start` (`TST_E_INVALID_CONFIG`).
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
        let Some(addr_str) =
            (unsafe { crate::c_str::c_str_arg(addr, TstError::InvalidConfig, "addr") })
        else {
            return;
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
/// A user name that contains `:` is refused by
/// `tst_rtsp_server_builder_start` (NULL, last-error `TST_E_INVALID_CONFIG`):
/// Basic authentication cannot carry it (RFC 7617 §2).
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
        if let Some((u, p)) = unsafe { crate::c_str::credentials(user, pass, "auth_basic") } {
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
        if let Some((u, p)) = unsafe { crate::c_str::credentials(user, pass, "auth_digest_md5") } {
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
        if let Some((u, p)) = unsafe { crate::c_str::credentials(user, pass, "auth_digest_sha256") }
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

/// Set the cap on reader sessions at once. A reader holds a slot from its
/// SETUP to the end of its role: its TEARDOWN, a refusal on its own mount
/// or the end of its connection. A reader set up again on its own or
/// another mount keeps the one slot it holds. Readers of a multicast mount
/// count; publishers do not. A reader's SETUP over the cap is answered
/// `453 Not Enough Bandwidth` and its connection is closed after the reply
/// (a C client's `tst_rtsp_client_builder_connect` then returns NULL with
/// `TST_E_RTSP_PROTOCOL`).
///
/// Not calling it (the default): no cap besides
/// `tst_rtsp_server_builder_max_sessions`. `n = 0`: no readers at all, an
/// ingest-only server (unlike `_max_sessions`, which floors 0 to 1). With
/// `n` below `max_sessions` the difference can never be held by readers,
/// so readers cannot use up every connection of a server that also takes
/// publishers; a cap at or above `max_sessions` protects nothing. A
/// connection without a session still holds one of the `max_sessions`
/// until its 30 s admission deadline, so a publisher's connection is not
/// promised. The last call wins.
///
/// Must be called before `tst_rtsp_server_builder_start`. A NULL `builder`
/// sets last-error `TST_E_INVALID_CONFIG`.
///
/// **Stability: Provisional.**
///
/// # Safety
///
/// `builder` must be NULL or returned by `tst_rtsp_server_builder_new`, not
/// yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_max_readers(
    builder: *mut TstRtspServerBuilder,
    n: u32,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.max_readers = Some(n),
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

/// Set the server-wide default for publish mounts: when `end` is true, a
/// publisher that reached RECORD ends its mount when it leaves. The mount
/// is closed as `tst_rtsp_server_remove_mount` closes it: a demux receiver
/// taken from it reads `TST_E_END_OF_STREAM` once it has drained what was
/// already queued, and the mount refuses later publishers. A publisher
/// that never recorded does not end it. Default
/// false: the mount idles for the next publisher. On-demand mounts follow
/// it. To accept a publisher after the mount ended, call
/// `tst_rtsp_server_remove_mount` and add the mount again; an ended
/// on-demand mount stays in the table, and counts toward the on-demand cap,
/// until it is removed.
///
/// Must be called before `tst_rtsp_server_builder_start`. A NULL `builder`
/// sets last-error `TST_E_INVALID_CONFIG`.
///
/// # Safety
///
/// `builder` must be NULL or returned by `tst_rtsp_server_builder_new`, not
/// yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_end_publish_mount_on_publisher_exit(
    builder: *mut TstRtspServerBuilder,
    end: bool,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.end_publish_mount_on_publisher_exit = end,
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// End the session of a recording publisher that sends no media for `secs`
/// seconds, timed from RECORD or its last RTP packet. `0` (the default)
/// leaves it unset, so only the session timeout governs. A publisher that
/// has not reached RECORD is never ended by it. A value longer than the
/// session timeout still matters: a publisher that keeps its session alive
/// with RTSP requests while sending no media is ended by this timeout, not
/// the session timeout.
///
/// Must be called before `tst_rtsp_server_builder_start`. A NULL `builder`
/// sets last-error `TST_E_INVALID_CONFIG`.
///
/// # Safety
///
/// `builder` must be NULL or returned by `tst_rtsp_server_builder_new`, not
/// yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_publish_idle_timeout(
    builder: *mut TstRtspServerBuilder,
    secs: u32,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.publish_idle_timeout_secs = secs,
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Whether sessions may carry media over UDP: a reader's or a publisher's
/// SETUP with a transport that is not TCP-interleaved, and multicast
/// mounts. A server that does not allow it answers such a SETUP
/// `461 Unsupported Transport`; an interleaved SETUP is served as before.
///
/// Default (when this is never called): yes on an `rtsp://` bind, NO on an
/// `rtsps://` bind, so that an `rtsps://` server sends and takes media only
/// inside the TLS connection. `yes = true` on an `rtsps://` server restores
/// the behaviour of 0.10.0, in which the media travels unencrypted;
/// `yes = false` makes a plain server TCP-only. The last call wins.
///
/// Must be called before `tst_rtsp_server_builder_start`. A NULL `builder`
/// sets last-error `TST_E_INVALID_CONFIG`.
///
/// **Stability: Provisional.**
///
/// # Safety
///
/// `builder` must be NULL or returned by `tst_rtsp_server_builder_new`, not
/// yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_allow_udp_media(
    builder: *mut TstRtspServerBuilder,
    yes: bool,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.allow_udp_media = Some(yes),
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Accept a Digest answer that carries no `qop`: the RFC 2069 form, which
/// GStreamer 1.24's `rtspsrc` and `rtspclientsink` send. Applies to both
/// Digest schemes; the challenge still offers `qop="auth"`, and an answer
/// WITH `qop` is checked as before, its nonce count included. Default:
/// false, so such an answer is refused as a downgrade (`401`).
///
/// What is given up, only for a client that sends no `qop` and only with
/// this on: replay protection inside one connection. Such an answer has no
/// nonce count, so a captured `Authorization` value for one method and URI
/// can be sent again on the same connection while it lives. The nonce is
/// still 128 random bits per TCP connection and is never accepted on
/// another.
///
/// GStreamer 1.24 authenticates with Basic, or with Digest MD5 under this
/// option, never with Digest SHA-256: it answers a SHA-256 challenge with
/// an MD5 response.
///
/// Must be called before `tst_rtsp_server_builder_start`. A NULL `builder`
/// sets last-error `TST_E_INVALID_CONFIG`.
///
/// **Stability: Provisional.**
///
/// # Safety
///
/// `builder` must be NULL or returned by `tst_rtsp_server_builder_new`, not
/// yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_server_builder_accept_digest_without_qop(
    builder: *mut TstRtspServerBuilder,
    yes: bool,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.accept_digest_without_qop = yes,
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
/// `secs` is used as-is. A value of 0 does not disable the timeout: SETUP
/// advertises `timeout=0` and idle readers are reaped after 2 s (the
/// floor). Use a positive value. Must be called before
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
/// When `tst_rtsp_server_stop` is called, the server runs these phases in
/// order. Each session gets an RFC 7826 §13.5.1 Notice 5402
/// ("Server-Initiated TEARDOWN") ANNOUNCE, with the write bounded at 1 s;
/// sessions are notified serially. The server then waits for the sessions
/// to end, for at most `ms` milliseconds plus a 500 ms scheduling margin.
/// Each session's write half is then shut down, bounded at 500 ms and also
/// serial. When the last reference to the server drops, the runtime shuts
/// down in at most 5 s. Default: 100 ms.
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
/// Works in a build with the `tls` feature (`TST_HAS_TLS`): the PEM bytes are
/// loaded when the server starts. Without it the bytes are stored, and
/// `tst_rtsp_server_builder_start` FAILS (NULL + `TST_E_RTSP_TLS`
/// last-error) when they are set — the alternative would be a server that
/// silently starts PLAINTEXT with the supplied certs ignored. A plaintext
/// `rtsp://` bind with these bytes set is refused in either build.
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
        b.tls_key_pem = Some(Box::<[u8]>::from(key_bytes).into());
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

/// Auth scheme tag stored in `TstRtspServerBuilder`.
///
/// Mirrors `tst_rtp::rtsp::server::builder::ServerAuthScheme` (which is `pub(crate)`)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_options_refuse_a_null_builder() {
        let setters: [unsafe extern "C" fn(*mut TstRtspServerBuilder, bool); 2] = [
            tst_rtsp_server_builder_allow_udp_media,
            tst_rtsp_server_builder_accept_digest_without_qop,
        ];
        for set in setters {
            crate::error::clear_last_error_for_test();
            unsafe { set(ptr::null_mut(), true) };
            assert_eq!(
                crate::error::test_last_error_code(),
                TstError::InvalidConfig as i32
            );
        }
        crate::error::clear_last_error_for_test();
        unsafe { tst_rtsp_server_builder_max_readers(ptr::null_mut(), 1) };
        assert_eq!(
            crate::error::test_last_error_code(),
            TstError::InvalidConfig as i32
        );
    }

    fn new_builder(url: &str) -> *mut TstRtspServerBuilder {
        let url = std::ffi::CString::new(url).unwrap();
        let b = unsafe { tst_rtsp_server_builder_new(url.as_ptr()) };
        assert!(!b.is_null(), "{}", crate::error::test_last_error_msg());
        b
    }

    /// Starts `b` and expects the refusal at build: NULL, a text that names
    /// the server and `field`, and `TST_E_INVALID_CONFIG`, the code every
    /// setting the server's build refuses reads in C. Returns the text.
    fn refused_at_start(b: *mut TstRtspServerBuilder, field: &str) -> String {
        crate::error::clear_last_error_for_test();
        let server = unsafe { crate::rtsp::server::start::tst_rtsp_server_builder_start(b) };
        if !server.is_null() {
            unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
            panic!("the server started");
        }
        assert_eq!(
            crate::error::test_last_error_code(),
            TstError::InvalidConfig as i32
        );
        let msg = crate::error::test_last_error_msg();
        assert!(msg.contains("invalid server config"), "{msg}");
        assert!(msg.contains(field), "{msg}");
        msg
    }

    /// The URL is taken by `_new` and `_bind` (it parses) and refused when
    /// the server is built, through either door, with or without a
    /// credential of the server's own; the text carries no part of it.
    #[test]
    fn a_bind_url_with_userinfo_is_refused_at_start() {
        let urls = [
            "rtsp://admin:secret@127.0.0.1:0",
            "rtsp://admin@127.0.0.1:0",
            "rtsp://:secret@127.0.0.1:0",
            "rtsps://admin:secret@127.0.0.1:0",
        ];
        for url in urls {
            for with_auth in [false, true] {
                let through_new = new_builder(url);
                let through_bind = new_builder("rtsp://127.0.0.1:0");
                let bound = std::ffi::CString::new(url).unwrap();
                crate::error::clear_last_error_for_test();
                unsafe { tst_rtsp_server_builder_bind(through_bind, bound.as_ptr()) };
                assert_eq!(crate::error::test_last_error_code(), 0, "_bind took {url}");
                for b in [through_new, through_bind] {
                    if with_auth {
                        let (u, p) = (c"viewer", c"pw");
                        unsafe {
                            tst_rtsp_server_builder_auth_digest_md5(b, u.as_ptr(), p.as_ptr())
                        };
                    }
                    let msg = refused_at_start(b, "bind URL");
                    assert!(
                        !msg.contains("admin") && !msg.contains("secret"),
                        "{url}: {msg}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_basic_user_name_with_a_colon_is_refused_at_start() {
        let b = new_builder("rtsp://127.0.0.1:0");
        let (u, p) = (c"a:b", c"x");
        unsafe { tst_rtsp_server_builder_auth_basic(b, u.as_ptr(), p.as_ptr()) };
        refused_at_start(b, "auth username");
    }

    #[test]
    fn an_empty_auth_user_name_is_refused_at_start() {
        let b = new_builder("rtsp://127.0.0.1:0");
        let (u, p) = (c"", c"x");
        unsafe { tst_rtsp_server_builder_auth_digest_md5(b, u.as_ptr(), p.as_ptr()) };
        refused_at_start(b, "auth username");
    }

    /// Digest carries the user name quoted, so a colon in it is fine.
    #[test]
    fn a_digest_user_name_with_a_colon_is_accepted() {
        let b = new_builder("rtsp://127.0.0.1:0");
        let (u, p) = (c"a:b", c"x");
        unsafe { tst_rtsp_server_builder_auth_digest_md5(b, u.as_ptr(), p.as_ptr()) };
        let server = unsafe { crate::rtsp::server::start::tst_rtsp_server_builder_start(b) };
        assert!(!server.is_null(), "{}", crate::error::test_last_error_msg());
        unsafe { crate::rtsp::server::stop::tst_rtsp_server_free(server) };
    }
}
