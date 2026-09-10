//! FFI panic isolation helper.
//!
//! Every outermost `extern "C"` body in `tst-c` is wrapped in
//! [`ffi_catch`], which converts a panic into a recorded
//! `TstError::PanicCaught` last-error plus a caller-supplied default
//! return value. This prevents Rust unwinding from crossing the C
//! frame, which is undefined behavior under `panic="unwind"` and an
//! abort (with no last-error visibility) under `panic="abort"`.
//!
//! The inner `Handle::with_inner_{mut,ref}` helpers in `handle.rs`
//! already wrap data-path closures; `ffi_catch` covers the open path
//! and config-builder setters where no `Handle` exists yet.
//!
//! # Default-value conventions
//!
//! Pass the right default per the entry point's return type:
//!
//! | Return type                  | Default to pass                          |
//! |------------------------------|------------------------------------------|
//! | `*mut T` (opaque handle)     | `core::ptr::null_mut()`                  |
//! | `*const T`                   | `core::ptr::null()` (or static fallback) |
//! | `crate::c_types::c_int`      | `TstError::Internal as i32` (-10)        |
//! | `TstVideoStreamHandle` /     | `TST_INVALID_STREAM_HANDLE` (u32::MAX)   |
//! | `TstKlvStreamHandle`         |                                          |
//! | `TstProgramHandle`           | `TST_INVALID_PROGRAM_HANDLE`             |
//! | `()` (void, e.g., `_free`)   | `()`                                     |
//!
//! `TstError::Internal as i32` is the default for `c_int` returns
//! because `ffi_catch` already records `PanicCaught` to last-error
//! on the panic arm — the *return value* is just a signal that
//! something went wrong. The success path returns 0 or a specific
//! `TstError::* as i32` directly; the default is only observed on
//! actual panic, where `PanicCaught` is in last-error.

#[cfg(feature = "std")]
use crate::error::record_panic_caught;

/// Run `f` inside a panic boundary. On panic, record `PanicCaught` to
/// the thread-local last-error (with a best-effort detail extracted
/// from the panic payload) and return `default`. On success, return
/// `f()`'s value unchanged.
///
/// Under `std` this uses `std::panic::catch_unwind`. Under `no_std`
/// (bare-metal targets where panic = abort) the closure is run directly —
/// no unwinding is possible, so no wrapper is needed.
///
/// `AssertUnwindSafe` is sound here because every caller in `tst-c`
/// satisfies one of:
///   - The closure body mutates only thread-local state (last-error)
///     and Box-allocated data the caller has not yet observed.
///   - The closure body mutates `&mut TstMuxConfig` (or sibling
///     builder), and a panic mid-mutation leaves the builder in a
///     consistent-but-partial state; subsequent calls either complete
///     the build or `tst_mux_config_free` it. Both outcomes are sound
///     because the inner `Vec` / `Option` fields don't have
///     cross-field invariants that a partial push would violate.
///   - The closure body returns a freshly-allocated `Box::into_raw(...)`
///     that the caller now owns; a panic before `Box::into_raw` simply
///     drops the in-progress `Box` (Rust unwinding semantics) and
///     leaks nothing.
#[cfg(feature = "std")]
pub(crate) fn ffi_catch<R, F>(default: R, f: F) -> R
where
    F: FnOnce() -> R,
{
    use core::panic::AssertUnwindSafe;
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => {
            let detail = panic_payload_message(&*payload);
            record_panic_caught(&detail);
            default
        }
    }
}

/// Under no_std (panic = abort), there is no unwinding. Run the closure
/// directly; the `default` value is never used but is kept in the signature
/// so call sites are identical in both modes.
#[cfg(not(feature = "std"))]
pub(crate) fn ffi_catch<R, F>(_default: R, f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

/// Best-effort detail string from a `catch_unwind` payload.
/// Used by both the `handle` module and `jni_catch` panic-isolation paths.
#[cfg(feature = "std")]
pub(crate) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> alloc::string::String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        alloc::string::String::from(*s)
    } else if let Some(s) = payload.downcast_ref::<alloc::string::String>() {
        s.clone()
    } else {
        alloc::string::String::from("non-string panic payload")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "std")]
    use crate::error::TstError;
    use crate::error::{clear_last_error_for_test, tst_get_last_error};

    #[test]
    fn no_panic_returns_closure_value() {
        clear_last_error_for_test();
        let rc: i32 = ffi_catch(-10, || 42);
        assert_eq!(rc, 42);
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn panic_returns_default_and_records_last_error() {
        clear_last_error_for_test();
        let rc: i32 = ffi_catch(-10, || panic!("boom"));
        assert_eq!(rc, -10);
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::PanicCaught as i32
        );
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn panic_with_formatted_string_payload_captures_detail() {
        clear_last_error_for_test();
        let _: i32 = ffi_catch(-10, || panic!("dynamic-{}", 42));
        let ptr = unsafe { crate::error::tst_get_last_error_str() };
        let cstr = unsafe { core::ffi::CStr::from_ptr(ptr) };
        let msg = cstr.to_str().unwrap();
        assert!(msg.contains("dynamic-42"), "got: {msg}");
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn null_ptr_default_for_pointer_returns() {
        clear_last_error_for_test();
        struct Dummy;
        let p: *mut Dummy = ffi_catch(core::ptr::null_mut(), || panic!("ptr panic"));
        assert!(p.is_null());
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::PanicCaught as i32
        );
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn unit_default_for_void_returns() {
        clear_last_error_for_test();
        ffi_catch((), || panic!("void panic"));
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::PanicCaught as i32
        );
    }

    /// Pin the architectural property at the unit-test level: a panic
    /// inside `ffi_catch` (simulating `Socket::connect_with` deep panic
    /// inside libsrt, or `Vec::push` OOM in a config builder) is caught,
    /// recorded as `PanicCaught`, and the default sentinel is returned.
    /// Integration-test-level coverage was considered but would require
    /// an exposed panic-injection hook; the unit test captures the same
    /// property without leaking internals.
    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn open_path_simulated_panic_is_caught() {
        clear_last_error_for_test();
        let p: *mut u8 = ffi_catch(core::ptr::null_mut(), || panic!("simulated connect panic"));
        assert!(p.is_null());
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::PanicCaught as i32
        );
        let ptr = unsafe { crate::error::tst_get_last_error_str() };
        let msg = unsafe { core::ffi::CStr::from_ptr(ptr) }.to_str().unwrap();
        assert!(msg.contains("simulated connect"), "got: {msg}");
    }

    /// Validate-1 D1: lifecycle entries (`_close` / `_cancel`) now wrap
    /// their bodies in `ffi_catch` so a panic raised inside (e.g., a
    /// poisoned-mutex unwrap deep in `boxed.inner.close()`, or a
    /// `cancel_handle()` Drop that itself panics) cannot unwind across
    /// the C boundary. Both default-value conventions are exercised:
    /// `_close` uses `()` and `_cancel` uses `TstError::Internal as i32`
    /// (the c_int convention from the table at the top of this file).
    /// The C-side observable is: `_close` returns control normally,
    /// `_cancel` returns -10, and `tst_get_last_error()` reads
    /// `PanicCaught` afterward.
    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn close_path_simulated_panic_returns_unit_and_records() {
        clear_last_error_for_test();
        // _close entries return `()`. The lifecycle body roughly is
        // `if p.is_null() { return; } let boxed = Box::from_raw(p); ...
        // boxed.inner.close(); drop(boxed);`. A panic in any of those
        // inner steps must NOT unwind past the C boundary.
        ffi_catch((), || panic!("simulated close-path panic"));
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::PanicCaught as i32
        );
    }

    #[cfg(feature = "std")] // exercises catch_unwind; no_std has no unwinding
    #[test]
    fn cancel_path_simulated_panic_returns_internal_and_records() {
        clear_last_error_for_test();
        // _cancel entries return `c_int`. Convention from the table at
        // the top of this file: pass `TstError::Internal as i32` (-10)
        // as the default. Last-error reads `PanicCaught` so the caller
        // can distinguish a panic from a normal Internal return.
        let rc: i32 = ffi_catch(TstError::Internal as i32, || {
            panic!("simulated cancel-path panic")
        });
        assert_eq!(rc, TstError::Internal as i32);
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::PanicCaught as i32
        );
        let ptr = unsafe { crate::error::tst_get_last_error_str() };
        let msg = unsafe { core::ffi::CStr::from_ptr(ptr) }.to_str().unwrap();
        assert!(msg.contains("simulated cancel-path"), "got: {msg}");
    }
}
