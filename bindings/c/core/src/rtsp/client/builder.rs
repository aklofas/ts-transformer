//! `tst_rtsp_client_builder_*` C entry points.
//!
//! Wraps `tst_rtp::RtspClientBuilder` with a mutable opaque handle so C
//! callers can configure a client incrementally before calling
//! `tst_rtsp_client_builder_connect`.
//!
//! # Builder pattern divergence from the Rust API
//!
//! `RtspClientBuilder` uses consuming `mut self -> Self` chain methods.
//! To support mutable-in-place C setter semantics we store the configuration
//! fields directly in `TstRtspClientBuilder` (see `bindings/c/core/src/handle.rs`)
//! and reconstruct the Rust builder from them at `connect` time. This avoids
//! the `mem::replace(inner, RtspClientBuilder::new(...))` dance which would
//! require passing a dummy URL and re-parsing it on every setter.
//!
//! # Transport preference
//!
//! `RtspTransportPref` is encoded in the URL query string
//! (`?transport=tcp|udp`) for callers using `RtspClient::connect` directly.
//! The C ABI exposes it as an integer enum so callers do not need to
//! manipulate URL strings themselves.  The pref is stored in
//! `TstRtspClientBuilder.transport_pref` and applied at connect time with
//! `RtspClientBuilder::transport_preference`, overriding the URL's query.
//!
//! # Auth scheme
//!
//! `RtspClientBuilder::auth(username, password)` provides credentials that
//! the client sends in response to the server's `WWW-Authenticate` challenge.
//! The actual auth *scheme* (Basic vs Digest MD5 vs Digest SHA-256) is always
//! negotiated by the server — the three C auth entry points are named for
//! discoverability and documentation purposes.  Internally all three store the
//! same username + password; the builder does not pre-select a scheme.
//!
//! # TLS root certificates
//!
//! `RtspClientBuilder::tls_root_certs` takes a `rustls::RootCertStore`.
//! The C ABI stores the raw PEM bytes and parses them into a `RootCertStore`
//! at connect time in a build with the `tls` feature (`TST_HAS_TLS`).
//! Without it, calling `tst_rtsp_client_builder_tls_root_cert_pem` still
//! succeeds (it stores the bytes) but `_connect` will surface an error if the
//! URL uses `rtsps://`.

use std::os::raw::c_char;
use std::ptr;

use tst_rtp::url::RtspTransportPref;

use crate::error::{TstError, set_last_error};
use crate::handle::TstRtspClientBuilder;
use crate::panic::ffi_catch;

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// Allocate a new RTSP client builder targeting `url`.
///
/// `url` must be a NUL-terminated `rtsp://` or `rtsps://` URL string.
/// Transport preference, auth credentials, keepalive policy, and (for
/// `rtsps://`) TLS root certificates may be set with the `_transport_pref`,
/// `_auth_*`, `_keepalive`, and `_tls_root_cert_pem` setters before calling
/// `tst_rtsp_client_builder_connect` to open a live session.
///
/// `?recv_timeout=<ms>` on `url` configures a persistent receive deadline
/// on the transport `tst_rtsp_session_into_demux_receiver` returns —
/// expiry surfaces through `tst_rtp_demux_receiver_next_event` as
/// `TST_E_BUFFER_FULL` (-4), retryable. Client-local — never rendered
/// onto the RTSP request line.
///
/// Returns a non-NULL builder pointer on success, or NULL with the
/// thread-local last-error populated on failure (bad URL, allocation
/// failure, etc.).  The caller must eventually pass the pointer to
/// `tst_rtsp_client_builder_connect` (which consumes it) or
/// `tst_rtsp_client_builder_free` (which discards it).
///
/// # Safety
///
/// `url` must be a valid, NUL-terminated C string that lives for the
/// duration of this call.  The returned pointer is owned by the caller
/// and must not be aliased.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_new(
    url: *const c_char,
) -> *mut TstRtspClientBuilder {
    ffi_catch(ptr::null_mut(), || {
        let Some(url_str) =
            (unsafe { crate::c_str::c_str_arg(url, TstError::InvalidConfig, "url") })
        else {
            return ptr::null_mut();
        };
        match TstRtspClientBuilder::from_url(url_str) {
            Ok(b) => TstRtspClientBuilder::into_raw(Box::new(b)),
            Err(e) => {
                set_last_error(TstError::InvalidConfig, &format!("URL parse error: {e}"));
                ptr::null_mut()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Setters
// ---------------------------------------------------------------------------

/// Set the RTSP transport preference.
///
/// `pref` values:
/// - `0` — prefer UDP; fall back to TCP-interleaved on 461 (default)
/// - `1` — force UDP; surface an error on 461 instead of falling back
/// - `2` — force TCP-interleaved; skip the UDP attempt entirely
///
/// Must be called before `tst_rtsp_client_builder_connect`.
/// No-op (with last-error set to `TST_E_INVALID_CONFIG`) if `pref` is
/// out of range or `builder` is NULL.
///
/// # Safety
///
/// `builder` must be a non-NULL pointer returned by
/// `tst_rtsp_client_builder_new` and not yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_transport_pref(
    builder: *mut TstRtspClientBuilder,
    pref: u32,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        let pref_enum = match pref {
            0 => RtspTransportPref::PreferUdp,
            1 => RtspTransportPref::ForceUdp,
            2 => RtspTransportPref::ForceTcp,
            other => {
                set_last_error(
                    TstError::InvalidConfig,
                    &format!(
                        "invalid transport_pref {other}; expected 0 (PreferUdp), 1 (ForceUdp), or 2 (ForceTcp)"
                    ),
                );
                return;
            }
        };
        b.transport_pref = Some(pref_enum);
    });
}

/// Enable or disable the auto-keepalive background thread.
///
/// When `enabled` is `true` (the default), the builder's connect call
/// spawns a keepalive thread that sends periodic OPTIONS requests at
/// `session_timeout / 2` to prevent the server from expiring the
/// session.  Pass `false` to suppress the keepalive thread — useful
/// for short-lived sessions or when the caller manages keepalives
/// manually.
///
/// Must be called before `tst_rtsp_client_builder_connect`.
///
/// # Safety
///
/// `builder` must be a non-NULL pointer returned by
/// `tst_rtsp_client_builder_new` and not yet freed or consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_keepalive(
    builder: *mut TstRtspClientBuilder,
    enabled: bool,
) {
    ffi_catch((), || match unsafe { builder.as_mut() } {
        Some(b) => b.auto_keepalive = enabled,
        None => set_last_error(TstError::InvalidConfig, "builder is null"),
    });
}

/// Supply a PEM-encoded CA certificate bundle for `rtsps://` connections.
///
/// `cert_pem` must point to `cert_len` bytes of PEM-encoded certificate
/// data (one or more X.509 certificates in `-----BEGIN CERTIFICATE-----`
/// / `-----END CERTIFICATE-----` blocks).  The bytes are copied into the
/// builder; the caller's buffer does not need to outlive this call.
///
/// When the builder connects to an `rtsps://` URL, the PEM bytes are
/// parsed into a root certificate store and used to validate the server's
/// TLS certificate chain.  If this setter is not called, the system native
/// trust store is used as the default.
///
/// No-op (with last-error set) if `builder` or `cert_pem` is NULL, or if
/// `cert_len` is zero.
///
/// Works in a build with the `tls` feature (`TST_HAS_TLS`). The setter stores
/// the raw PEM bytes either way; without the feature they are never used, and
/// `tst_rtsp_client_builder_connect` with an `rtsps://` URL fails with
/// `TST_E_RTSP_TLS`.
///
/// # Safety
///
/// - `builder` must be non-NULL and returned by `tst_rtsp_client_builder_new`.
/// - `cert_pem` must point to at least `cert_len` valid bytes.
/// - Neither pointer needs to outlive this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_tls_root_cert_pem(
    builder: *mut TstRtspClientBuilder,
    cert_pem: *const u8,
    cert_len: usize,
) {
    ffi_catch((), || {
        let b = match unsafe { builder.as_mut() } {
            Some(b) => b,
            None => {
                set_last_error(TstError::InvalidConfig, "builder is null");
                return;
            }
        };
        if cert_pem.is_null() || cert_len == 0 {
            set_last_error(TstError::InvalidConfig, "cert_pem is null or cert_len is 0");
            return;
        }
        let bytes = match unsafe { crate::ffi_slice::ffi_slice(cert_pem, cert_len, "cert_pem") } {
            Ok(s) => s,
            Err(_) => return,
        };
        b.tls_root_cert_pem = Some(bytes.to_vec());
    });
}

/// Free a builder without connecting.
///
/// Use this on error paths where the builder was partially configured and
/// you want to discard it.  After this call the pointer is invalid; any
/// further use is undefined behavior.  NULL is a no-op.
///
/// Prefer `tst_rtsp_client_builder_connect` which also consumes the builder —
/// `_free` is the error-path companion.
///
/// # Safety
///
/// `builder` must be NULL, or a pointer returned by
/// `tst_rtsp_client_builder_new` that has not yet been freed or passed to
/// `_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_rtsp_client_builder_free(builder: *mut TstRtspClientBuilder) {
    ffi_catch((), || {
        if builder.is_null() {
            return;
        }
        // SAFETY: caller guarantees valid, unaliased, un-freed pointer.
        let _ = unsafe { TstRtspClientBuilder::from_raw(builder) };
        // Box drops at end of scope.
    });
}
