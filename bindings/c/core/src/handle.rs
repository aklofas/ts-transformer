//! `Handle<T>` — the no_std wrapper for the offline C-side opaque pointers,
//! and `CHandle<T, S>` — the std transport-handle shape over
//! `tst_pipeline::binding::Owned`.
//!
//! `Handle<T> = Mutex<Option<T>>`. `_open` returns
//! `Box::into_raw(Box::new(Handle::new(inner)))`. Data-path entry points
//! call `Handle::with_inner_mut`; `_close` calls `Handle::close`. Drop of
//! the inner runs Drop, which closes the underlying transport / muxer.
//!
//! Since Arc 2 `Handle<T>` serves ONLY the three offline handles that must
//! build without std (`TstMuxer`, `TstDemuxer`, `TstSt0601`). Every
//! transport-bearing handle holds a `CHandle<T, S>` instead — the one
//! handle state machine shared with the Python and JVM bindings, which
//! carries the cancel handle and the construction-time snapshot beside the
//! slot so neither is reached through the lock. `binding` is std-only,
//! which is why both shapes live here.
//!
//! Under `no_std` the lock is the spin-backed `nostd_mutex::Mutex` — no
//! priority inheritance, no interrupt masking — so each C handle must be
//! driven from a single task (one-handle-per-task), the same contract
//! `tst-pipeline` documents for its sender shells. Sharing one handle
//! across preemptive tasks can livelock a higher-priority task against a
//! preempted lock-holder. (`std` builds use `std::sync::Mutex`; the
//! contract applies only to no_std/bare-metal embeddings.)

use crate::error::{TstError, record_internal, record_panic_caught, set_last_error};

#[cfg(not(feature = "std"))]
use crate::nostd_mutex::Mutex;
#[cfg(feature = "std")]
use std::sync::Mutex;

/// Run `f` catching any panic (std). Returns `Ok(result)` or `Err(detail)`.
///
/// Delegates to the shared binding-layer helper so C, Python and the JVM
/// render panic payloads identically (spec §3.6: the `panic_payload_message`
/// twin goes).
#[cfg(feature = "std")]
fn catch<R>(f: impl FnOnce() -> R) -> Result<R, alloc::string::String> {
    tst_pipeline::binding::panic::catch(f)
}

/// Under no_std (panic = abort), run the closure directly — no unwinding possible.
#[cfg(not(feature = "std"))]
fn catch<R>(f: impl FnOnce() -> R) -> Result<R, alloc::string::String> {
    Ok(f())
}

pub(crate) struct Handle<T> {
    inner: Mutex<Option<T>>,
}

impl<T> Handle<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(Some(value)),
        }
    }

    /// Run `f` against `&mut T` if the handle is live. If the handle is
    /// closed, sets `TST_E_CLOSED` and returns its code.
    ///
    /// The closure is run inside `std::panic::catch_unwind`. A panic
    /// transitively reachable from any tst-c data-path call is caught
    /// at the FFI boundary, recorded as `TST_E_PANIC_CAUGHT`, and the
    /// inner state is dropped (subsequent calls on the same handle
    /// return `TST_E_CLOSED`). `AssertUnwindSafe` is sound here because
    /// we catch and clear; no further use of `T` happens after a panic.
    pub(crate) fn with_inner_mut<F>(&self, f: F) -> i32
    where
        F: FnOnce(&mut T) -> i32,
    {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                record_internal("mutex poisoned");
                return TstError::Internal as i32;
            }
        };
        match guard.as_mut() {
            Some(t) => match catch(|| f(t)) {
                Ok(rc) => rc,
                Err(detail) => {
                    record_panic_caught(&detail);
                    // After a panic the inner state is indeterminate.
                    // Drop it so subsequent calls return Closed rather
                    // than reusing potentially-corrupted state.
                    *guard = None;
                    TstError::PanicCaught as i32
                }
            },
            None => {
                set_last_error(TstError::Closed, "handle is closed");
                TstError::Closed as i32
            }
        }
    }

    /// Run `f` against `&T` if the handle is live (same close semantics).
    ///
    /// Mirrors the panic-isolation behavior of `with_inner_mut`: a
    /// panic in `f` is caught at the FFI boundary and the inner state
    /// is dropped. Even though `&T` did not mutate the inner directly,
    /// the panic could have left external state (global mutexes, file
    /// descriptors, etc.) in an indeterminate state — defense-in-depth
    /// drops the inner anyway.
    pub(crate) fn with_inner_ref<F>(&self, f: F) -> i32
    where
        F: FnOnce(&T) -> i32,
    {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                record_internal("mutex poisoned");
                return TstError::Internal as i32;
            }
        };
        match guard.as_ref() {
            Some(t) => match catch(|| f(t)) {
                Ok(rc) => rc,
                Err(detail) => {
                    record_panic_caught(&detail);
                    *guard = None;
                    TstError::PanicCaught as i32
                }
            },
            None => {
                set_last_error(TstError::Closed, "handle is closed");
                TstError::Closed as i32
            }
        }
    }

    /// Take the inner value (idempotent — second call is a no-op).
    /// Triggers Drop of the inner, which closes the underlying resource.
    pub(crate) fn close(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = None;
        }
    }

    /// Run `f` against `&T` if the handle is live, WITHOUT touching the
    /// last-error channel on any path — closed handle, poisoned mutex, or
    /// a caught panic all just return `default`, silently. For
    /// side-channel query entry points (e.g. `tst_st0601_state`) that are
    /// documented to never clobber a caller's pending last-error just by
    /// being polled — `with_inner_ref` cannot be reused for those because
    /// it unconditionally calls `set_last_error`/`record_panic_caught` on
    /// its non-live paths.
    ///
    /// Panics are still caught (no unwinding crosses this boundary) and
    /// the inner state is still dropped afterward, for the same
    /// indeterminate-state reason as `with_inner_ref` — only the
    /// recording to last-error is skipped.
    pub(crate) fn with_inner_ref_silent<R, F>(&self, default: R, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return default,
        };
        match guard.as_ref() {
            Some(t) => match catch(|| f(t)) {
                Ok(r) => r,
                Err(_detail) => {
                    *guard = None;
                    default
                }
            },
            None => default,
        }
    }
}

// ---------------------------------------------------------------------------
// CHandle<T, S> — the std transport-handle shape (tst_pipeline::binding::Owned)
//
// Every transport-bearing C handle (srt/rtp/udp/tcp/rist senders and
// receivers, plain and managed) holds exactly one of these. `Handle<T>`
// above stays ONLY for the three no_std offline handles (muxer, demuxer,
// st0601): `tst_pipeline::binding` is std-only.
// ---------------------------------------------------------------------------

#[cfg(feature = "std")]
mod owned {
    use alloc::sync::Arc;

    use tst_core::transport::TransportCancel;
    use tst_pipeline::RecvEndReason;
    use tst_pipeline::RecvEndReasonHandle;
    use tst_pipeline::binding::{BindingError, Close, FlagCancel, Owned};

    use crate::error::record_binding_error;

    /// Newtype over [`Owned`] that projects the handle state machine onto
    /// the C `int` + thread-local last-error contract. The method names
    /// mirror the no_std [`super::Handle`] so family modules and the
    /// `extern-c-ffi-catch-coverage.sh` rail see the same call shape.
    pub(crate) struct CHandle<T, S = ()>(Owned<T, S>);

    impl<T, S> CHandle<T, S> {
        #[allow(dead_code)] // consumed by every `_open` (B1.4-B1.10)
        pub(crate) fn new(inner: T, cancel: Arc<dyn TransportCancel>, snapshot: S) -> Self {
            Self(Owned::new(inner, cancel, snapshot))
        }

        /// Receivers only: attach the end-reason cell read by `_end_reason`.
        #[allow(dead_code)] // consumed by the receiver families (B1.5-B1.9)
        pub(crate) fn with_end_reason(self, h: RecvEndReasonHandle) -> Self {
            Self(self.0.with_end_reason(h))
        }

        /// Run `f` against `&mut T`. Closed / poisoned / panicked handles are
        /// recorded through the ONE error path (`record_binding_error`):
        /// `Closed` → TST_E_CLOSED, `Poisoned` → TST_E_INTERNAL,
        /// `Panicked` → TST_E_PANIC_CAUGHT. As with the no_std `Handle`, a
        /// caught panic in a MUTATOR drops the inner (later calls are
        /// `Closed`); [`Self::with_inner_ref`] keeps it (spec §3.2 as
        /// amended at plan review).
        #[allow(dead_code)] // consumed by the data-path entry points (B1.5-B1.10)
        pub(crate) fn with_inner_mut(&self, f: impl FnOnce(&mut T) -> i32) -> i32 {
            match self.0.with_mut(f) {
                Ok(rc) => rc,
                Err(state) => record_binding_error(BindingError::from(state)),
            }
        }

        /// Reader twin of [`Self::with_inner_mut`] (recovers a poisoned lock,
        /// and a caught panic leaves the inner in place).
        #[allow(dead_code)] // consumed by the stats/is_alive getters (B1.5-B1.10)
        pub(crate) fn with_inner_ref(&self, f: impl FnOnce(&T) -> i32) -> i32 {
            match self.0.with_ref(f) {
                Ok(rc) => rc,
                Err(state) => record_binding_error(BindingError::from(state)),
            }
        }

        /// Lock-free: never touches the slot (the #189 lease-bug class).
        #[allow(dead_code)] // consumed by the `_cancel` entry points (B1.5-B1.10)
        pub(crate) fn cancel(&self) {
            self.0.cancel();
        }

        #[allow(dead_code)] // consumed by the recv-side CLOSED relabellers (B1.5-B1.9)
        pub(crate) fn is_cancelled(&self) -> bool {
            self.0.is_cancelled()
        }

        /// Construction-constant state (side-channel handles, addresses).
        /// Getters that are constant after `_open` read HERE, never the slot
        /// (`scripts/check/c/snapshot-getters.sh`).
        #[allow(dead_code)] // consumed by the snapshot-bearing families (B1.8/B1.9)
        pub(crate) fn snapshot(&self) -> &S {
            self.0.snapshot()
        }

        #[allow(dead_code)] // consumed by the `_end_reason` getters (B1.5-B1.9)
        pub(crate) fn end_reason(&self) -> Option<RecvEndReason> {
            self.0.end_reason()
        }
    }

    impl<T: Close, S> CHandle<T, S> {
        /// Cancel-first close. `_close` returns `void`, so a close failure
        /// is recorded to last-error and otherwise swallowed; a second
        /// close is `Ok(())` in `Owned`. `From<CloseFailure<E>>` is the
        /// shared rendering (`Inner` → INTERNAL, `Panicked` → PANIC_CAUGHT).
        #[allow(dead_code)] // consumed by the `_close` entry points (B1.5-B1.10)
        pub(crate) fn close(&self) {
            if let Err(f) = self.0.close() {
                record_binding_error(BindingError::from(f));
            }
        }
    }

    /// The shells' `cancel_handle()` returns an `Option`; this is the ONE
    /// place it is resolved. `None` → A1's `binding::FlagCancel` (a plain
    /// latch that records the request and wakes nothing).
    ///
    /// Since Arc 2 WP-D every transport this binding opens returns `Some`,
    /// so the `None` arm is a defensive fallback rather than a live path —
    /// UDP and RIST got their real handles there, and the udp/rist call
    /// sites picked them up through this function with no change. The
    /// function itself STAYS: the shell-level `cancel_handle()` is still
    /// `Option`-typed, so all ~22 call sites across every family need the
    /// resolution, and replacing it with `.expect()` would trade a safe
    /// fallback for a panic path (R2's no-`expect` gate).
    #[allow(dead_code)] // consumed by the udp/rist `_open` paths (B1.7/B1.10)
    pub(crate) fn cancel_or_latch(
        c: Option<Arc<dyn TransportCancel + Send + Sync>>,
    ) -> Arc<dyn TransportCancel> {
        match c {
            Some(c) => c,
            None => Arc::new(FlagCancel::new()),
        }
    }
}

// Re-exported unconditionally so the family modules (B1.4-B1.10) import
// `crate::handle::CHandle` exactly like `crate::handle::Handle`; until the
// first family converts, the only consumers are this module's unit tests.
#[cfg(feature = "std")]
#[allow(unused_imports)]
pub(crate) use owned::{CHandle, cancel_or_latch};

// ---------------------------------------------------------------------------
// RTSP client builder opaque handle (rtp feature)
// ---------------------------------------------------------------------------

/// Configuration accumulator for the RTSP client.
///
/// Allocated by `tst_rtsp_client_builder_new` and consumed (or freed) by
/// `tst_rtsp_client_builder_connect` / `tst_rtsp_client_builder_free`.
///
/// Fields are stored independently rather than wrapping `RtspClientBuilder`
/// directly because `RtspClientBuilder` uses consuming `mut self -> Self`
/// chain setters, making in-place C mutation cumbersome.  The connect path
/// constructs the final `RtspClientBuilder` from these fields.
// `#[allow(dead_code)]`: fields are read by the RTSP connect path which is
// gated behind the `rtp` feature; they appear unused in minimal builds.
#[cfg(feature = "rtp")]
#[allow(dead_code)]
pub struct TstRtspClientBuilder {
    /// Parsed `rtsp://` or `rtsps://` URL.  Transport preference is
    /// embedded via `url.transport_preference`; `transport_pref` below
    /// overrides it if the caller explicitly called `_transport_pref`.
    pub(crate) url: tst_rtp::url::RtspUrl,
    /// Override for `url.transport_preference` set via
    /// `tst_rtsp_client_builder_transport_pref`.  `None` means use the
    /// preference already encoded in the URL (default: `PreferUdp`).
    pub(crate) transport_pref: Option<tst_rtp::url::RtspTransportPref>,
    /// Credentials supplied via any `_auth_*` entry point.
    pub(crate) username: Option<String>,
    /// Password in plain text; wrapped in `secrecy::SecretString` at
    /// `connect` time so it is zeroed when the builder is consumed.
    pub(crate) password: Option<String>,
    /// When `true` (the default), `connect` spawns an auto-keepalive
    /// thread.  Set to `false` by `tst_rtsp_client_builder_keepalive(b, false)`.
    pub(crate) auto_keepalive: bool,
    /// Raw PEM bytes for `rtsps://` connections.  Parsed into a
    /// `rustls::RootCertStore` at connect time.  `None` → system trust store.
    pub(crate) tls_root_cert_pem: Option<Vec<u8>>,
}

#[cfg(feature = "rtp")]
impl TstRtspClientBuilder {
    /// Construct from a parsed URL string.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` with a user-facing message if the URL cannot
    /// be parsed as an RTSP URL.
    pub(crate) fn from_url(url_str: &str) -> Result<Self, String> {
        let url = tst_rtp::url::RtspUrl::parse(url_str).map_err(|e| e.to_string())?;
        Ok(Self {
            url,
            transport_pref: None,
            username: None,
            password: None,
            auto_keepalive: true,
            tls_root_cert_pem: None,
        })
    }

    /// Leak the `Box` and return a raw pointer suitable for FFI.
    pub(crate) fn into_raw(b: Box<Self>) -> *mut Self {
        Box::into_raw(b)
    }

    /// Reconstruct the `Box` from a raw pointer.
    ///
    /// # Safety
    ///
    /// Caller must ensure `p` was returned by `into_raw` and has not
    /// yet been freed or consumed.
    pub(crate) unsafe fn from_raw(p: *mut Self) -> Box<Self> {
        // SAFETY: forwarded from caller.
        unsafe { Box::from_raw(p) }
    }
}

// ---------------------------------------------------------------------------
// RTSP server builder opaque handle (rtp feature)
// ---------------------------------------------------------------------------

/// Configuration accumulator for the RTSP server.
///
/// Allocated by `tst_rtsp_server_builder_new` and consumed (or freed) by
/// `tst_rtsp_server_builder_start` / `tst_rtsp_server_builder_free`.
///
/// Fields are stored independently rather than wrapping `RtspServerBuilder`
/// directly.  Even though `RtspServerBuilder` uses `&mut self -> &mut Self`
/// chain setters (which are in-place friendly), storing fields independently
/// keeps the opaque struct layout stable across future Rust API changes and
/// matches the `TstRtspClientBuilder` pattern.  The server-start path
/// constructs the final `RtspServerBuilder` from these fields.
// `#[allow(dead_code)]`: fields are read by the RTSP server-start path which
// is gated behind the `rtp` feature; they appear unused in minimal builds.
#[cfg(feature = "rtp")]
#[allow(dead_code)]
pub struct TstRtspServerBuilder {
    /// Parsed bind URL. Replaced by `tst_rtsp_server_builder_bind`.
    pub(crate) bind_url: tst_rtp::url::RtspUrl,
    /// Auth scheme — `None` means no auth required (default).
    pub(crate) auth_scheme: Option<crate::rtsp::server::builder::TstRtspServerAuthScheme>,
    /// Auth realm — set to `"tst-rtp"` by any `_auth_*` setter.
    pub(crate) auth_realm: String,
    /// Username supplied via any `_auth_*` entry point.
    pub(crate) auth_username: Option<String>,
    /// Password in plain text; wrapped in `secrecy::SecretString` at
    /// `start` time so it is zeroed when the builder is consumed.
    pub(crate) auth_password: Option<String>,
    /// Cap on concurrent client sessions. Floored to 1. Default: 64.
    pub(crate) max_sessions: u32,
    /// Session timeout in seconds. Default: 60.
    pub(crate) session_timeout_secs: u32,
    /// Per-mount broadcast channel capacity (frame count). Floored to 1.
    /// Default: 256.
    pub(crate) fanout_capacity: u32,
    /// Graceful-shutdown drain window in milliseconds. Default: 100.
    pub(crate) graceful_shutdown_drain_ms: u32,
    /// Raw PEM bytes for the TLS certificate chain. Stored for a future
    /// tst-c `tls` feature; TLS is NOT available in this build, so
    /// `build_server` refuses to start when this is `Some` (the server
    /// would otherwise come up PLAINTEXT with the certs ignored).
    pub(crate) tls_cert_pem: Option<Vec<u8>>,
    /// Raw PEM bytes for the TLS private key. Same contract as
    /// [`Self::tls_cert_pem`]: stored but unusable in this build;
    /// presence makes `build_server` refuse.
    pub(crate) tls_key_pem: Option<Vec<u8>>,
}

#[cfg(feature = "rtp")]
impl TstRtspServerBuilder {
    /// Construct from a parsed URL string.
    ///
    /// Runs `validate_for_server_bind` to reject DNS names — the OS bind
    /// requires an IP literal.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` with a user-facing message if the URL cannot
    /// be parsed or fails the server-bind validation.
    pub(crate) fn from_url(url_str: &str) -> Result<Self, String> {
        use tst_rtp::url::RtspUrl;
        let url = RtspUrl::parse(url_str).map_err(|e| e.to_string())?;
        url.validate_for_server_bind().map_err(|e| e.to_string())?;
        Ok(Self {
            bind_url: url,
            auth_scheme: None,
            auth_realm: String::new(),
            auth_username: None,
            auth_password: None,
            max_sessions: 64,
            session_timeout_secs: 60,
            fanout_capacity: 256,
            graceful_shutdown_drain_ms: 100,
            tls_cert_pem: None,
            tls_key_pem: None,
        })
    }

    /// Leak the `Box` and return a raw pointer suitable for FFI.
    pub(crate) fn into_raw(b: Box<Self>) -> *mut Self {
        Box::into_raw(b)
    }

    /// Reconstruct the `Box` from a raw pointer.
    ///
    /// # Safety
    ///
    /// Caller must ensure `p` was returned by `into_raw` and has not
    /// yet been freed or consumed.
    pub(crate) unsafe fn from_raw(p: *mut Self) -> Box<Self> {
        // SAFETY: forwarded from caller.
        unsafe { Box::from_raw(p) }
    }

    /// Construct a `tst_rtp::RtspServer` from this accumulator's fields.
    /// Called by `tst_rtsp_server_builder_start` after consuming the
    /// builder via `from_raw`.
    ///
    /// TLS cert + key PEM bytes: tst-c is built without tst-rtp's `tls`
    /// feature, so stored bytes can never take effect — if either is
    /// present this returns `RtspServerError::Tls` instead of building a
    /// server that would silently start PLAINTEXT (see the guard below).
    pub(crate) fn build_server(
        self: Box<Self>,
    ) -> Result<tst_rtp::RtspServer, tst_rtp::RtspServerError> {
        use crate::rtsp::server::builder::TstRtspServerAuthScheme;
        use std::time::Duration;

        let mut b = tst_rtp::RtspServerBuilder::with_url(self.bind_url);

        if let (Some(scheme), Some(user), Some(pass)) =
            (self.auth_scheme, self.auth_username, self.auth_password)
        {
            let secret = secrecy::SecretString::new(pass.into());
            let realm = if self.auth_realm.is_empty() {
                "tst-rtp"
            } else {
                &self.auth_realm
            };
            match scheme {
                TstRtspServerAuthScheme::Basic => {
                    b.auth_basic(realm, &user, secret);
                }
                TstRtspServerAuthScheme::DigestMd5 => {
                    b.auth_digest_md5(realm, &user, secret);
                }
                TstRtspServerAuthScheme::DigestSha256 => {
                    b.auth_digest_sha256(realm, &user, secret);
                }
            }
        }

        b.max_sessions(self.max_sessions.max(1) as usize);
        b.session_timeout(Duration::from_secs(self.session_timeout_secs as u64));
        b.fanout_capacity(self.fanout_capacity.max(1) as usize);
        b.graceful_shutdown_drain(Duration::from_millis(
            self.graceful_shutdown_drain_ms as u64,
        ));

        // TLS bytes: tst-c has no `rtsp-server-tls` cargo feature, so
        // tst-rtp's `RtspServerBuilder::tls_cert` is compiled out of this
        // graph and stored PEM bytes can never take effect. Refuse to start
        // rather than come up PLAINTEXT while the caller believes TLS is
        // armed. (The setter still stores the bytes so a future tst-c
        // `rtsp-server-tls` feature can light this up without an ABI
        // change.)
        if self.tls_cert_pem.is_some() || self.tls_key_pem.is_some() {
            return Err(tst_rtp::RtspServerError::Tls(
                "tst_rtsp_server_builder_tls_cert_pem was called, but TLS is \
                 not compiled into this tst-c build — the server would start \
                 PLAINTEXT with the supplied certs ignored; drop the \
                 tls_cert_pem call (or use a TLS-capable binding)"
                    .into(),
            ));
        }

        b.build()
    }
}

// ---------------------------------------------------------------------------
// Stream handle types (multi-stream `mpegts::mux` fan-out)
// ---------------------------------------------------------------------------

/// Opaque per-program ordinal for a video elementary stream. Obtained from
/// `tst_mux_config_add_video_stream` at config time and reused with the
/// `_video_to` push siblings on every muxer-owning C variant.
///
/// Handles are stable across the config→open boundary and across managed
/// reconnects. They encode `(program_index, within_program_index)` as a
/// packed `u32` (bits 4..=7 = program, bits 0..=3 = within). They are NOT
/// interchangeable between muxers.
pub type TstVideoStreamHandle = u32;

/// Opaque per-program ordinal for a KLV elementary stream. Same packed
/// encoding as [`TstVideoStreamHandle`].
pub type TstKlvStreamHandle = u32;

/// Opaque per-program ordinal for an audio elementary stream. Same packed
/// encoding as [`TstVideoStreamHandle`].
pub type TstAudioStreamHandle = u32;

/// Opaque per-program ordinal for a subtitle elementary stream. Same packed
/// encoding as [`TstVideoStreamHandle`].
pub type TstSubtitleStreamHandle = u32;

/// Opaque per-program ordinal for a private/application data elementary
/// stream. Same packed encoding as [`TstVideoStreamHandle`].
pub type TstDataStreamHandle = u32;

/// Sentinel returned by `tst_mux_config_add_*_stream` on failure.
/// On failure, the last-error is also populated; check
/// `tst_get_last_error()` for the negative `TST_E_*` code.
pub const TST_INVALID_STREAM_HANDLE: u32 = u32::MAX;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_inner_runs_when_live() {
        let h = Handle::new(7i32);
        let rc = h.with_inner_mut(|n| {
            *n += 1;
            0
        });
        assert_eq!(rc, 0);
        let final_value = h.with_inner_ref(|n| *n);
        assert_eq!(final_value, 8);
    }

    #[test]
    fn with_inner_after_close_returns_closed_code() {
        let h = Handle::new(7i32);
        h.close();
        let rc = h.with_inner_mut(|_| 0);
        assert_eq!(rc, TstError::Closed as i32);
    }

    #[test]
    fn close_is_idempotent() {
        let h = Handle::new(7i32);
        h.close();
        h.close();
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn panic_in_inner_closure_is_caught() {
        use crate::error::clear_last_error_for_test;
        clear_last_error_for_test();
        let h = Handle::new(7i32);
        let rc = h.with_inner_mut(|_| panic!("test panic"));
        assert_eq!(rc, TstError::PanicCaught as i32);
        // After a caught panic, the inner is dropped: subsequent calls
        // see a closed handle.
        let rc2 = h.with_inner_mut(|_| 0);
        assert_eq!(rc2, TstError::Closed as i32);
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn panic_in_inner_ref_closure_is_caught() {
        use crate::error::clear_last_error_for_test;
        clear_last_error_for_test();
        let h = Handle::new(7i32);
        let rc = h.with_inner_ref(|_| panic!("test panic ref"));
        assert_eq!(rc, TstError::PanicCaught as i32);
        // Defense-in-depth: even though &T didn't mutate, external state
        // could be in an indeterminate post-panic state, so we drop the
        // inner. Subsequent calls return Closed.
        let rc2 = h.with_inner_ref(|_| 0);
        assert_eq!(rc2, TstError::Closed as i32);
    }

    #[test]
    fn with_inner_ref_silent_runs_when_live_and_does_not_touch_last_error() {
        use crate::error::{clear_last_error_for_test, tst_get_last_error};
        clear_last_error_for_test();
        let h = Handle::new(7i32);
        let value = h.with_inner_ref_silent(-1, |n| *n);
        assert_eq!(value, 7);
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[test]
    fn with_inner_ref_silent_on_closed_handle_returns_default_without_recording() {
        use crate::error::{clear_last_error_for_test, tst_get_last_error};
        clear_last_error_for_test();
        let h = Handle::new(7i32);
        h.close();
        let value = h.with_inner_ref_silent(-1, |n| *n);
        assert_eq!(value, -1);
        // The whole point of the _silent variant: unlike with_inner_ref,
        // a closed handle must NOT set last-error to Closed here.
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn with_inner_ref_silent_panic_is_caught_without_recording() {
        use crate::error::{clear_last_error_for_test, tst_get_last_error};
        clear_last_error_for_test();
        let h = Handle::new(7i32);
        let rc = h.with_inner_ref_silent(-1, |_| panic!("test silent panic"));
        assert_eq!(rc, -1);
        // Unlike with_inner_ref, a caught panic here must NOT record
        // PanicCaught to last-error.
        assert_eq!(unsafe { tst_get_last_error() }, 0);
        // Defense-in-depth still applies: the inner state is dropped, so
        // a subsequent call also returns the default.
        let rc2 = h.with_inner_ref_silent(-1, |_| 0);
        assert_eq!(rc2, -1);
    }

    // ---- CHandle (the std transport-handle shape over tst_pipeline::binding::Owned) ----

    #[cfg(feature = "std")]
    mod chandle {
        use super::super::{CHandle, cancel_or_latch};
        use crate::error::{
            TstError, clear_last_error_for_test, test_last_error_code, test_last_error_msg,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tst_core::transport::TransportCancel;

        /// Local probe (not `binding::FlagCancel`: this one is inspected
        /// through its own field so the test does not depend on `is_set`).
        struct ProbeCancel(AtomicBool);
        impl TransportCancel for ProbeCancel {
            fn cancel(&self) {
                self.0.store(true, Ordering::SeqCst);
            }
            fn is_cancelled(&self) -> bool {
                self.0.load(Ordering::SeqCst)
            }
        }

        fn fresh() -> (CHandle<i32>, Arc<ProbeCancel>) {
            let flag = Arc::new(ProbeCancel(AtomicBool::new(false)));
            let c: Arc<dyn TransportCancel> = flag.clone();
            (CHandle::new(7i32, c, ()), flag)
        }

        #[test]
        fn with_inner_mut_runs_and_returns_closure_code() {
            let (h, _) = fresh();
            assert_eq!(
                h.with_inner_mut(|n| {
                    *n += 1;
                    0
                }),
                0
            );
            assert_eq!(h.with_inner_ref(|n| *n), 8);
        }

        /// `cancel()` must never touch the slot (the #189 lease-bug class).
        ///
        /// Latch-and-poll, never a bare blocking call: the cancel runs on its
        /// OWN thread with a `done` latch, and this thread polls that latch
        /// against a bounded watchdog. A `cancel()` that took the lock would
        /// block behind the parked `with_inner_mut` forever, and the assert
        /// below FAILS the test at the watchdog instead of hanging until the
        /// harness kills the whole binary. The parked thread is released and
        /// joined on every exit path, including the failing one.
        #[test]
        fn cancel_is_lock_free_and_observable() {
            let (h, flag) = fresh();
            let h = Arc::new(h);
            let h2 = Arc::clone(&h);
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
            // Park a thread INSIDE the slot and keep it there until released.
            let parked = std::thread::spawn(move || {
                h2.with_inner_mut(|_| {
                    entered_tx.send(()).ok();
                    release_rx.recv().ok();
                    0
                })
            });
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("parked thread never entered with_inner_mut");

            let h3 = Arc::clone(&h);
            let done = Arc::new(AtomicBool::new(false));
            let done_w = Arc::clone(&done);
            let canceller = std::thread::spawn(move || {
                h3.cancel();
                done_w.store(true, Ordering::SeqCst);
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let returned = done.load(Ordering::SeqCst);

            // Release the parked thread FIRST so the join below cannot hang,
            // then assert — a failure must be a failure, not a wedge.
            release_tx.send(()).ok();
            assert_eq!(parked.join().unwrap(), 0);
            canceller.join().expect("cancel thread");

            assert!(
                returned,
                "cancel() did not return while another thread held the slot — it took the lock"
            );
            assert!(h.is_cancelled(), "is_cancelled must flip after cancel()");
            assert!(
                flag.0.load(Ordering::SeqCst),
                "cancel() must reach the transport handle"
            );
        }

        #[test]
        fn panic_in_mutator_records_panic_caught_and_closes_the_handle() {
            clear_last_error_for_test();
            let (h, _) = fresh();
            let rc = h.with_inner_mut(|_| panic!("boom-chandle"));
            assert_eq!(rc, TstError::PanicCaught as i32);
            assert_eq!(test_last_error_code(), TstError::PanicCaught as i32);
            assert!(
                test_last_error_msg().contains("boom-chandle"),
                "got {}",
                test_last_error_msg()
            );
            // Plan-review amendment of spec §3.2: a MUTATOR panic drops the
            // slot (same as the no_std `Handle` and 0.6.x C — see
            // `panic_in_inner_closure_is_caught` above); later calls are Closed.
            assert_eq!(h.with_inner_ref(|n| *n), TstError::Closed as i32);
            assert_eq!(test_last_error_code(), TstError::Closed as i32);
        }

        #[test]
        fn panic_in_reader_records_panic_caught_and_keeps_the_handle() {
            clear_last_error_for_test();
            let (h, _) = fresh();
            let rc = h.with_inner_ref(|_| panic!("boom-reader"));
            assert_eq!(rc, TstError::PanicCaught as i32);
            assert_eq!(
                h.with_inner_ref(|n| *n),
                7,
                "reader panics keep the slot (spec §3.2 as amended; 0.6.x with_inner_ref dropped it)"
            );
        }

        #[test]
        fn flag_cancel_stand_in_only_latches() {
            clear_last_error_for_test();
            let h: CHandle<i32> = CHandle::new(1, cancel_or_latch(None), ());
            assert!(!h.is_cancelled());
            h.cancel();
            assert!(h.is_cancelled());
            // The handle itself keeps working: nothing was closed.
            assert_eq!(h.with_inner_ref(|n| *n), 1);
        }

        #[test]
        fn cancel_or_latch_passes_a_real_handle_through() {
            let flag = Arc::new(ProbeCancel(AtomicBool::new(false)));
            let some: Option<Arc<dyn TransportCancel + Send + Sync>> = Some(flag.clone());
            let c = cancel_or_latch(some);
            c.cancel();
            assert!(flag.0.load(Ordering::SeqCst));
        }
    }
}
