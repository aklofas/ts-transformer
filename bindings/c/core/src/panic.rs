//! FFI panic isolation helper.
//!
//! Every outermost `extern "C"` body in `tst-c` is wrapped in
//! [`ffi_catch`], which converts a panic into a recorded
//! `TstError::PanicCaught` last-error plus a caller-supplied default
//! return value. This prevents Rust unwinding from crossing the C
//! frame, which is undefined behavior under `panic="unwind"` and an
//! abort (with no last-error visibility) under `panic="abort"`.
//!
//! The inner `CHandle::with_inner_{mut,ref}` helpers in `handle.rs`
//! already wrap data-path closures; `ffi_catch` covers the open path
//! and config-builder setters where no `CHandle` exists yet.
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

use crate::error::record_panic_caught;

/// Run `f` inside a panic boundary. On panic, record `PanicCaught` to
/// the last-error slot (with the payload rendered by
/// `tst_pipeline::binding::panic::payload_message`) and return `default`.
/// On success, return `f()`'s value unchanged.
///
/// One body for both planes: `tst_pipeline::binding::panic::catch` is
/// `catch_unwind` under `std` and runs the closure directly under `no_std`
/// (panic = abort; `default` is then never returned).
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
pub(crate) fn ffi_catch<R, F>(default: R, f: F) -> R
where
    F: FnOnce() -> R,
{
    match tst_pipeline::binding::panic::catch(f) {
        Ok(value) => value,
        Err(detail) => {
            record_panic_caught(&detail);
            default
        }
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
}
