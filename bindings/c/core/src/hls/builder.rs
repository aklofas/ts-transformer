//! `TstHlsPublisherBuilder` — opaque builder for the HLS publisher.
//!
//! `tst_hls::HlsPublisherBuilder` uses move-style chain setters
//! (`fn bind(self, ...) -> Self`), which don't map cleanly to in-place C
//! mutation. The opaque builder here wraps it in an `Option` so each
//! setter can `take()` the inner builder, apply the move-style method, and
//! store the result back — leaving the C-visible pointer stable across the
//! whole chain. A null inner (only reachable after a panic mid-chain or a
//! double-build) makes setters no-ops and `_build` fail with HlsConfig.
//!
//! Build with the chain:
//!
//! ```c
//! TstHlsPublisherBuilder *b = tst_hls_publisher_builder_new();
//! tst_hls_publisher_builder_bind(b, "127.0.0.1:8080");
//! tst_hls_publisher_builder_output_dir(b, "/tmp/hls");
//! tst_hls_publisher_builder_segment_duration_ms(b, 2000);
//! TstPublisher *pub = tst_hls_publisher_builder_build(b); // consumes b
//! ```

use std::net::SocketAddr;
use std::os::raw::c_char;
use std::time::Duration;

use tst_hls::{HlsMode, HlsPublisherBuilder};

use crate::error::{TstError, set_last_error};
use crate::hls::publisher::{PublisherImpl, TstPublisher};

// ---------------------------------------------------------------------------
// Handle type
// ---------------------------------------------------------------------------

/// Opaque accumulator for HLS publisher configuration.
///
/// Allocated by [`tst_hls_publisher_builder_new`], mutated by the
/// `_builder_*` setters, and consumed by
/// [`tst_hls_publisher_builder_build`] (or freed by
/// [`tst_hls_publisher_builder_free`]).
pub struct TstHlsPublisherBuilder {
    /// `Option` so the move-style chain setters can `take()` + replace.
    /// `None` only after `build` consumes it or a mid-chain panic.
    inner: Option<HlsPublisherBuilder>,
}

/// Apply a move-style setter to the inner builder in place. No-op (records
/// nothing) if the inner has already been consumed.
fn map_inner(
    b: &mut TstHlsPublisherBuilder,
    f: impl FnOnce(HlsPublisherBuilder) -> HlsPublisherBuilder,
) {
    if let Some(inner) = b.inner.take() {
        b.inner = Some(f(inner));
    }
}

// ---------------------------------------------------------------------------
// new / free
// ---------------------------------------------------------------------------

/// Create a new HLS publisher builder seeded with library defaults
/// (LIVE mode, 4 s segments, bind `127.0.0.1:8080`, output dir
/// `<system-temp>/tstrans-hls`). Returns `NULL` only on allocation failure.
///
/// # Safety
///
/// Sound under any caller invocation. The returned builder must eventually
/// be consumed by `tst_hls_publisher_builder_build` or released with
/// `tst_hls_publisher_builder_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_new() -> *mut TstHlsPublisherBuilder {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        Box::into_raw(Box::new(TstHlsPublisherBuilder {
            inner: Some(HlsPublisherBuilder::new()),
        }))
    })
}

/// Free an HLS publisher builder previously returned by
/// `tst_hls_publisher_builder_new`.
///
/// Safe to call with `NULL` (no-op). Not needed after a successful
/// `tst_hls_publisher_builder_build` — that call consumes the builder.
///
/// # Safety
///
/// `b` must be NULL or a valid non-freed `*mut TstHlsPublisherBuilder`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_free(b: *mut TstHlsPublisherBuilder) {
    crate::panic::ffi_catch((), || {
        if !b.is_null() {
            drop(unsafe { Box::from_raw(b) });
        }
    });
}

// ---------------------------------------------------------------------------
// Setters
// ---------------------------------------------------------------------------

/// Set the HTTP server bind address (e.g., `"127.0.0.1:8080"`,
/// `"0.0.0.0:0"` for an ephemeral port, or an IPv6 literal in brackets).
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null, or
/// `TST_E_HLS_CONFIG` if `bind_addr` is not a parseable socket address.
///
/// # Safety
///
/// `b` must be a valid non-freed builder. `bind_addr` must be a
/// NUL-terminated C string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_bind(
    b: *mut TstHlsPublisherBuilder,
    bind_addr: *const c_char,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(s) =
            (unsafe { crate::c_str::parse_c_str(bind_addr, TstError::HlsConfig, "bind_addr") })
        else {
            return TstError::HlsConfig as i32;
        };
        let addr: SocketAddr = match s.parse() {
            Ok(a) => a,
            Err(e) => {
                set_last_error(
                    TstError::HlsConfig,
                    &format!("bind_addr '{s}' is not a socket address: {e}"),
                );
                return TstError::HlsConfig as i32;
            }
        };
        map_inner(builder, |inner| inner.bind(addr));
        0
    })
}

/// Set the filesystem directory for `.ts` segments + `playlist.m3u8`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null, or
/// `TST_E_HLS_CONFIG` if `path` is null / not valid UTF-8.
///
/// # Safety
///
/// `b` must be a valid non-freed builder. `path` must be a NUL-terminated
/// C string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_output_dir(
    b: *mut TstHlsPublisherBuilder,
    path: *const c_char,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(s) =
            (unsafe { crate::c_str::parse_c_str(path, TstError::HlsConfig, "output_dir") })
        else {
            return TstError::HlsConfig as i32;
        };
        let owned = s.to_string();
        map_inner(builder, |inner| inner.output_dir(owned));
        0
    })
}

/// Set the target segment duration in milliseconds.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null.
///
/// # Safety
///
/// `b` must be a valid non-freed builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_segment_duration_ms(
    b: *mut TstHlsPublisherBuilder,
    ms: u32,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        map_inner(builder, |inner| {
            inner.segment_duration(Duration::from_millis(ms as u64))
        });
        0
    })
}

/// Set the hard upper bound on an open segment's wall-clock age (in
/// milliseconds) in the keyframe-driven flow. When a keyframe is overdue,
/// the segmenter force-cuts at this cap so segments never grow unbounded.
///
/// A non-zero `ms` must be at least the target segment duration; a smaller
/// value is rejected later at `tst_hls_publisher_builder_build` with
/// `TST_E_HLS_CONFIG`.
///
/// Passing `ms == 0` is a no-op that leaves the library default in effect
/// (`2 × segment_duration`). A fresh builder already carries this default,
/// so `0` is only meaningful as "do not override the cap"; the underlying
/// `tst-hls` builder exposes no reset-to-default setter, so a `0` after a
/// prior non-zero call does **not** clear the earlier value. Callers that
/// need the default should simply never call this setter.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null.
///
/// # Safety
///
/// `b` must be a valid non-freed builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_max_segment_duration_ms(
    b: *mut TstHlsPublisherBuilder,
    ms: u64,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        // 0 => leave the library default (None) in place: a fresh builder is
        // already seeded with the default, so applying nothing is correct.
        if ms != 0 {
            map_inner(builder, |inner| {
                inner.max_segment_duration(Duration::from_millis(ms))
            });
        }
        0
    })
}

/// Set the rolling playlist window size (segment count) used in LIVE mode.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null.
///
/// # Safety
///
/// `b` must be a valid non-freed builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_playlist_window(
    b: *mut TstHlsPublisherBuilder,
    n: u32,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        map_inner(builder, |inner| inner.playlist_window(n as usize));
        0
    })
}

/// Set the playlist mode: `0` = LIVE, `1` = EVENT, `2` = VOD.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null, or
/// `TST_E_HLS_CONFIG` for an out-of-range `mode` (the builder is left
/// unchanged in that case).
///
/// # Safety
///
/// `b` must be a valid non-freed builder.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_mode(
    b: *mut TstHlsPublisherBuilder,
    mode: u32,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        let hls_mode = match mode {
            0 => HlsMode::Live,
            1 => HlsMode::Event,
            2 => HlsMode::Vod,
            other => {
                set_last_error(
                    TstError::HlsConfig,
                    &format!("invalid hls mode {other} (expected 0=LIVE, 1=EVENT, 2=VOD)"),
                );
                return TstError::HlsConfig as i32;
            }
        };
        map_inner(builder, |inner| inner.mode(hls_mode));
        0
    })
}

/// Enable HTTP Basic auth on the playlist + segment endpoints.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null, or
/// `TST_E_HLS_CONFIG` if `user` / `pass` is null / not valid UTF-8.
///
/// # Safety
///
/// `b` must be a valid non-freed builder. `user` and `pass` must be
/// NUL-terminated C strings valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_basic_auth(
    b: *mut TstHlsPublisherBuilder,
    user: *const c_char,
    pass: *const c_char,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(u) = (unsafe { crate::c_str::parse_c_str(user, TstError::HlsConfig, "user") })
        else {
            return TstError::HlsConfig as i32;
        };
        let Some(p) = (unsafe { crate::c_str::parse_c_str(pass, TstError::HlsConfig, "pass") })
        else {
            return TstError::HlsConfig as i32;
        };
        let (u, p) = (u.to_string(), p.to_string());
        map_inner(builder, |inner| inner.basic_auth(u, p));
        0
    })
}

/// Enable HTTPS by supplying PEM cert + key paths.
///
/// **Note:** the `tst-tcp` `tls` cargo feature is not enabled in `tst-c`,
/// so `tst_hls_publisher_builder_build` will reject a TLS-configured
/// builder with `TST_E_HLS_TLS`. This setter is wired for forward-
/// compatibility once a `tls` feature lights up in `tst-c`.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null, or
/// `TST_E_HLS_CONFIG` if either path is null / not valid UTF-8.
///
/// # Safety
///
/// `b` must be a valid non-freed builder. `cert_path` and `key_path` must
/// be NUL-terminated C strings valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_enable_tls(
    b: *mut TstHlsPublisherBuilder,
    cert_path: *const c_char,
    key_path: *const c_char,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(cert) =
            (unsafe { crate::c_str::parse_c_str(cert_path, TstError::HlsConfig, "cert_path") })
        else {
            return TstError::HlsConfig as i32;
        };
        let Some(key) =
            (unsafe { crate::c_str::parse_c_str(key_path, TstError::HlsConfig, "key_path") })
        else {
            return TstError::HlsConfig as i32;
        };
        let (cert, key) = (cert.to_string(), key.to_string());
        map_inner(builder, |inner| inner.enable_tls(cert, key));
        0
    })
}

/// Replace the builder's accumulated config by parsing an `hls://` or
/// `hlss://` URL (e.g., `"hls://127.0.0.1:9100?mode=vod&playlist_window=10"`).
///
/// Any prior setter calls on this builder are discarded — `from_url`
/// reseeds the whole config from the URL.
///
/// Returns 0 on success, `TST_E_INVALID_CONFIG` if `b` is null, or
/// `TST_E_HLS_CONFIG` if `url` is null / not valid UTF-8 / not a valid
/// HLS URL (the builder's prior inner is preserved on parse failure).
///
/// # Safety
///
/// `b` must be a valid non-freed builder. `url` must be a NUL-terminated C
/// string valid for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_from_url(
    b: *mut TstHlsPublisherBuilder,
    url: *const c_char,
) -> libc::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(builder) = (unsafe { b.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return TstError::InvalidConfig as i32;
        };
        let Some(url_str) = (unsafe { crate::c_str::parse_c_str(url, TstError::HlsConfig, "url") })
        else {
            return TstError::HlsConfig as i32;
        };
        match HlsPublisherBuilder::from_url(url_str) {
            Ok(parsed) => {
                builder.inner = Some(parsed);
                0
            }
            Err(e) => {
                // Preserve the prior inner on failure.
                set_last_error(TstError::HlsConfig, &format!("hls url parse: {e}"));
                TstError::HlsConfig as i32
            }
        }
    })
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

/// Consume the builder and construct the HLS publisher (binds the HTTP
/// server immediately). Returns `NULL` on error; check
/// `tst_get_last_error()` for the negative code.
///
/// On success the builder is consumed — do **not** call
/// `tst_hls_publisher_builder_free` afterward. On failure the builder is
/// still consumed (the `Box` is reclaimed); allocate a fresh one to retry.
/// The returned `TstPublisher` must be freed with `tst_publisher_free`.
///
/// A builder configured with `enable_tls` fails here with
/// `TST_E_HLS_TLS` because `tst-c` does not enable the `tst-tcp` `tls`
/// feature.
///
/// # Safety
///
/// `b` must be a valid non-freed `*mut TstHlsPublisherBuilder`. The
/// returned handle must eventually be freed with `tst_publisher_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_hls_publisher_builder_build(
    b: *mut TstHlsPublisherBuilder,
) -> *mut TstPublisher {
    crate::panic::ffi_catch(std::ptr::null_mut(), || {
        if b.is_null() {
            set_last_error(TstError::InvalidConfig, "null hls builder pointer");
            return std::ptr::null_mut();
        }
        // Consume the builder Box regardless of outcome.
        let mut boxed = unsafe { Box::from_raw(b) };
        let Some(inner) = boxed.inner.take() else {
            set_last_error(TstError::HlsConfig, "hls builder already consumed");
            return std::ptr::null_mut();
        };
        match inner.build() {
            Ok(hls) => Box::into_raw(Box::new(TstPublisher {
                inner: Some(PublisherImpl::Hls(hls)),
            })),
            Err(e) => {
                crate::error::record_with_context(e, "hls build");
                std::ptr::null_mut()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_then_free_is_safe() {
        let b = unsafe { tst_hls_publisher_builder_new() };
        assert!(!b.is_null());
        unsafe { tst_hls_publisher_builder_free(b) };
    }

    #[test]
    fn null_free_is_safe() {
        unsafe { tst_hls_publisher_builder_free(std::ptr::null_mut()) };
    }

    #[test]
    fn null_bind_returns_invalid_config() {
        let rc = unsafe { tst_hls_publisher_builder_bind(std::ptr::null_mut(), std::ptr::null()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);
    }

    #[test]
    fn bad_bind_addr_returns_hls_config() {
        let b = unsafe { tst_hls_publisher_builder_new() };
        let bad = std::ffi::CString::new("not-an-addr").unwrap();
        let rc = unsafe { tst_hls_publisher_builder_bind(b, bad.as_ptr()) };
        assert_eq!(rc, TstError::HlsConfig as i32);
        unsafe { tst_hls_publisher_builder_free(b) };
    }

    #[test]
    fn invalid_mode_returns_hls_config() {
        let b = unsafe { tst_hls_publisher_builder_new() };
        let rc = unsafe { tst_hls_publisher_builder_mode(b, 99) };
        assert_eq!(rc, TstError::HlsConfig as i32);
        unsafe { tst_hls_publisher_builder_free(b) };
    }

    #[test]
    fn null_build_returns_null() {
        let p = unsafe { tst_hls_publisher_builder_build(std::ptr::null_mut()) };
        assert!(p.is_null());
    }
}
