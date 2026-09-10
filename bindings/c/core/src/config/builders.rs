//! Sender-side opaque builder handles: `TstSenderConfig`,
//! `TstRawSenderConfig`, and `TstReconnectPolicy`.
//!
//! Each builder is a `Box<T>`. `_open` clones the inner before consuming
//! it, so the caller may free immediately after a successful open. The
//! `TstTsFramingMode` and `TstOverflowPolicy` enums that parameterize the
//! setters live alongside the builders since they are referenced nowhere
//! else.

use crate::error::{TstError, set_last_error};
use crate::panic::ffi_catch;
use core::time::Duration;
use tst_pipeline::{
    BackoffStrategy, OverflowPolicy, RawSenderConfig, ReconnectMode, ReconnectPolicy, SenderConfig,
    TsFramingMode,
};

// ------------------------------------------------------------------
// tst_sender_config_t
// ------------------------------------------------------------------

pub struct TstSenderConfig {
    pub(crate) inner: SenderConfig,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_config_new() -> *mut TstSenderConfig {
    ffi_catch(core::ptr::null_mut(), || {
        Box::into_raw(Box::new(TstSenderConfig {
            inner: SenderConfig::default(),
        }))
    })
}

/// Free a sender config previously returned by `tst_sender_config_new`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_config_free(p: *mut TstSenderConfig) {
    ffi_catch((), || {
        if !p.is_null() {
            unsafe { drop(Box::from_raw(p)) };
        }
    })
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum TstTsFramingMode {
    Recover = 0,
    Strict = 1,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_config_set_framing_mode(
    p: *mut TstSenderConfig,
    mode: TstTsFramingMode,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.framing_mode = match mode {
            TstTsFramingMode::Recover => TsFramingMode::Recover,
            TstTsFramingMode::Strict => TsFramingMode::Strict,
        };
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_sender_config_set_max_unsynced_bytes(
    p: *mut TstSenderConfig,
    n: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.max_unsynced_bytes = n;
        0
    })
}

// ------------------------------------------------------------------
// tst_raw_sender_config_t (empty today; reserved for future setters)
// ------------------------------------------------------------------

pub struct TstRawSenderConfig {
    #[allow(dead_code)] // read only by transport-feature-gated paths; unused in minimal builds
    pub(crate) inner: RawSenderConfig,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_sender_config_new() -> *mut TstRawSenderConfig {
    ffi_catch(core::ptr::null_mut(), || {
        Box::into_raw(Box::new(TstRawSenderConfig {
            inner: RawSenderConfig::default(),
        }))
    })
}

/// Free a raw sender config previously returned by
/// `tst_raw_sender_config_new`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_raw_sender_config_free(p: *mut TstRawSenderConfig) {
    ffi_catch((), || {
        if !p.is_null() {
            unsafe { drop(Box::from_raw(p)) };
        }
    })
}

// ------------------------------------------------------------------
// tst_reconnect_policy_t
// ------------------------------------------------------------------

pub struct TstReconnectPolicy {
    pub(crate) inner: ReconnectPolicy,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_new() -> *mut TstReconnectPolicy {
    ffi_catch(core::ptr::null_mut(), || {
        Box::into_raw(Box::new(TstReconnectPolicy {
            inner: ReconnectPolicy::default(),
        }))
    })
}

/// Free a reconnect policy previously returned by
/// `tst_reconnect_policy_new`.
///
/// Safe to call with NULL (no-op). After this call the pointer is
/// invalid; passing the same non-null pointer twice is undefined
/// behavior (use-after-free on the consumed `Box`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_free(p: *mut TstReconnectPolicy) {
    ffi_catch((), || {
        if !p.is_null() {
            unsafe { drop(Box::from_raw(p)) };
        }
    })
}

/// Set max reconnect attempts. `n < 0` means retry forever.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_set_max_attempts(
    p: *mut TstReconnectPolicy,
    n: i32,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.max_attempts = if n < 0 { None } else { Some(n as u32) };
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_set_backoff_constant_ms(
    p: *mut TstReconnectPolicy,
    ms: u32,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.backoff = BackoffStrategy::Constant(Duration::from_millis(ms as u64));
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_set_backoff_exponential_ms(
    p: *mut TstReconnectPolicy,
    base_ms: u32,
    max_ms: u32,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.backoff = BackoffStrategy::Exponential {
            base: Duration::from_millis(base_ms as u64),
            max: Duration::from_millis(max_ms as u64),
        };
        0
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_set_gap_buffer_capacity(
    p: *mut TstReconnectPolicy,
    n: usize,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.gap_buffer_capacity = n;
        0
    })
}

#[repr(C)]
#[derive(Clone, Copy)]
pub enum TstOverflowPolicy {
    DropOldest = 0,
    Reject = 1,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_set_overflow_policy(
    p: *mut TstReconnectPolicy,
    policy: TstOverflowPolicy,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.overflow_policy = match policy {
            TstOverflowPolicy::DropOldest => OverflowPolicy::DropOldest,
            TstOverflowPolicy::Reject => OverflowPolicy::Reject,
        };
        0
    })
}

/// Where a `ManagedTransport`'s reconnect loop runs after the inner
/// transport breaks.
///
/// - `Blocking` (default): reconnect runs on the caller's thread — a sink
///   outage blocks the caller inside a send call until reconnect succeeds
///   or `max_attempts` runs out.
/// - `Background`: reconnect runs on a dedicated per-outage worker thread.
///   Sends never wait out backoff or a factory call while the transport is
///   down; they enqueue into the gap buffer under the configured overflow
///   policy instead. On a managed *receive* open, `Background` is not
///   supported: the open logs a warning and degrades to `Blocking`.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum TstReconnectMode {
    Blocking = 0,
    Background = 1,
}

/// Set the reconnect-loop placement. See `TstReconnectMode` for the
/// semantics of each mode. Default: `Blocking`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_reconnect_policy_set_mode(
    p: *mut TstReconnectPolicy,
    mode: TstReconnectMode,
) -> crate::c_types::c_int {
    ffi_catch(TstError::Internal as i32, || {
        let Some(cfg) = (unsafe { p.as_mut() }) else {
            set_last_error(TstError::InvalidConfig, "null config pointer");
            return TstError::InvalidConfig as i32;
        };
        cfg.inner.mode = match mode {
            TstReconnectMode::Blocking => ReconnectMode::Blocking,
            TstReconnectMode::Background => ReconnectMode::Background,
        };
        0
    })
}

// These exercise the std-only builder surface (sender config + reconnect
// policy), so they live here, under the module's own `std` gate, rather
// than in config/mod.rs whose tests also run on the no_std plane
// (`cargo test -p tst-c-core --no-default-features`).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_sender_config_lifecycle() {
        unsafe {
            let p = tst_sender_config_new();
            assert_eq!(
                tst_sender_config_set_framing_mode(p, TstTsFramingMode::Strict),
                0,
            );
            assert_eq!(tst_sender_config_set_max_unsynced_bytes(p, 1024), 0);
            tst_sender_config_free(p);
        }
    }

    #[test]
    fn reconnect_policy_lifecycle() {
        unsafe {
            let p = tst_reconnect_policy_new();
            assert_eq!(tst_reconnect_policy_set_max_attempts(p, -1), 0); // forever
            assert_eq!(
                tst_reconnect_policy_set_backoff_exponential_ms(p, 100, 5_000),
                0,
            );
            assert_eq!(tst_reconnect_policy_set_gap_buffer_capacity(p, 128), 0);
            assert_eq!(
                tst_reconnect_policy_set_overflow_policy(p, TstOverflowPolicy::Reject),
                0,
            );
            assert_eq!(
                tst_reconnect_policy_set_mode(p, TstReconnectMode::Background),
                0,
            );
            tst_reconnect_policy_free(p);
        }
    }

    #[test]
    fn reconnect_policy_set_mode_updates_inner() {
        unsafe {
            let p = tst_reconnect_policy_new();
            // Default is Blocking (mirrors tst_pipeline::ReconnectPolicy::default()).
            assert_eq!((*p).inner.mode, tst_pipeline::ReconnectMode::Blocking);
            assert_eq!(
                tst_reconnect_policy_set_mode(p, TstReconnectMode::Background),
                0,
            );
            assert_eq!((*p).inner.mode, tst_pipeline::ReconnectMode::Background);
            tst_reconnect_policy_free(p);
        }
    }

    #[test]
    fn reconnect_policy_set_mode_null_returns_invalid_config() {
        unsafe {
            let rc =
                tst_reconnect_policy_set_mode(core::ptr::null_mut(), TstReconnectMode::Background);
            assert!(rc < 0);
        }
    }
}
