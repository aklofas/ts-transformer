//! Shared FFI byte-slice helper.
//!
//! Establishes the safe pattern for converting a C `(ptr, len)` pair into
//! a Rust `&[u8]` or `&mut [u8]`. A bare
//! `if ptr.is_null() && len > 0` guard leaves UB on the table
//! for the `(NULL, 0)` case — Rust's [`std::slice::from_raw_parts`]
//! requires the pointer to be non-null and aligned even for zero-length
//! slices.
//!
//! ## Contract
//!
//! The C ABI accepts `(NULL, 0)` from callers passing empty payloads.
//! Both helpers return `Ok(&[])` / `Ok(&mut [])` for that case without
//! dereferencing the pointer. For `(NULL, len > 0)` or `(ptr, len >
//! isize::MAX)` they record `InvalidConfig` to the per-thread last-error
//! and return the negative code.
//!
//! The `len <= isize::MAX` ceiling is checked inside the helper, so callers
//! need not document it as a precondition. Use these helpers, never a
//! direct `slice::from_raw_parts(ptr, len)`, on a C-supplied pair.

use crate::error::{TstError, set_last_error};
use alloc::format;

/// Convert a C `(ptr, len)` byte pair into a Rust `&[u8]` without
/// dereferencing a null pointer.
///
/// Returns `Ok(&[])` for `len == 0` regardless of `ptr` (matches the C
/// ABI's contract that empty payloads are signaled by `len == 0`).
///
/// Returns `Err(TstError::InvalidConfig as i32)` if `ptr.is_null()` with
/// `len > 0`, or if `len > isize::MAX`, and records a per-thread
/// last-error message containing the caller-supplied `name` for diagnosis.
///
/// # Safety
///
/// When `ptr` is non-null AND `0 < len <= isize::MAX`, the caller must
/// guarantee (matching the C ABI contract documented per entry point):
///
/// - `ptr` is valid for reads of `len` bytes,
/// - `ptr` points to `len` consecutive bytes of initialized memory,
/// - the memory is not mutated for the lifetime `'a`.
///
/// The `(NULL, 0)` and `(non_null, 0)` cases return `Ok(&[])` without
/// any pointer dereference and are sound even if `ptr` is dangling.
pub(crate) unsafe fn ffi_slice<'a>(
    ptr: *const u8,
    len: usize,
    name: &str,
) -> Result<&'a [u8], i32> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        set_last_error(
            TstError::InvalidConfig,
            &format!("null {name} with non-zero len"),
        );
        return Err(TstError::InvalidConfig as i32);
    }
    if len > (isize::MAX as usize) {
        set_last_error(
            TstError::InvalidConfig,
            &format!("{name} length {len} exceeds isize::MAX"),
        );
        return Err(TstError::InvalidConfig as i32);
    }
    Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
}

/// Convert a C `(ptr, len)` byte pair into a Rust `&mut [u8]` without
/// dereferencing a null pointer.
///
/// Returns `Ok(&mut [])` for `len == 0` regardless of `ptr`.
///
/// Returns `Err(TstError::InvalidConfig as i32)` if `ptr.is_null()` with
/// `len > 0`, or if `len > isize::MAX`, and records a per-thread
/// last-error message containing the caller-supplied `name` for diagnosis.
///
/// # Safety
///
/// When `ptr` is non-null AND `0 < len <= isize::MAX`, the caller must
/// guarantee:
///
/// - `ptr` is valid for writes of `len` bytes,
/// - `ptr` points to `len` consecutive bytes of properly initialized memory,
/// - no other reference to the same memory exists for lifetime `'a`.
///
/// The `(NULL, 0)` and `(non_null, 0)` cases return `Ok(&mut [])` without
/// any pointer dereference and are sound even if `ptr` is dangling.
pub(crate) unsafe fn ffi_slice_mut<'a>(
    ptr: *mut u8,
    len: usize,
    name: &str,
) -> Result<&'a mut [u8], i32> {
    if len == 0 {
        return Ok(&mut []);
    }
    if ptr.is_null() {
        set_last_error(
            TstError::InvalidConfig,
            &format!("null {name} with non-zero len"),
        );
        return Err(TstError::InvalidConfig as i32);
    }
    if len > (isize::MAX as usize) {
        set_last_error(
            TstError::InvalidConfig,
            &format!("{name} length {len} exceeds isize::MAX"),
        );
        return Err(TstError::InvalidConfig as i32);
    }
    Ok(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{clear_last_error_for_test, tst_get_last_error};

    #[test]
    fn null_pointer_with_zero_len_returns_empty_slice() {
        // The whole point of this helper: (NULL, 0) is sound and yields &[],
        // never dereferencing the null pointer. The old per-site `if ptr.is_null()
        // && len > 0` guard let (NULL, 0) flow into `slice::from_raw_parts(null, 0)`
        // which is UB even though len is zero.
        clear_last_error_for_test();
        let result = unsafe { ffi_slice(core::ptr::null(), 0, "buf") };
        assert_eq!(result, Ok(&[][..]));
        assert_eq!(
            unsafe { tst_get_last_error() },
            0,
            "no error should be recorded for (NULL, 0)"
        );
    }

    #[test]
    fn null_pointer_with_nonzero_len_records_error_and_returns_code() {
        clear_last_error_for_test();
        let result = unsafe { ffi_slice(core::ptr::null(), 5, "nal") };
        assert_eq!(result, Err(TstError::InvalidConfig as i32));
        assert_eq!(
            unsafe { tst_get_last_error() },
            TstError::InvalidConfig as i32
        );
        // The last-error message must include the caller-supplied name so
        // the diagnostic points at the specific parameter that was wrong.
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert!(
            s.to_str().unwrap().contains("null nal"),
            "expected 'null nal' in message, got: {:?}",
            s.to_str().unwrap()
        );
    }

    #[test]
    fn non_null_pointer_with_zero_len_returns_empty_slice() {
        // Symmetric to the (NULL, 0) case: (non_null, 0) also yields &[]
        // without doing a dereference. Many existing call sites pre-validate
        // the buffer pointer; this helper preserves that semantic when len=0.
        clear_last_error_for_test();
        let buf = [0u8; 4];
        let result = unsafe { ffi_slice(buf.as_ptr(), 0, "buf") };
        assert_eq!(result, Ok(&[][..]));
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[test]
    fn non_null_pointer_with_nonzero_len_returns_slice_with_contents() {
        clear_last_error_for_test();
        let buf = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let result = unsafe { ffi_slice(buf.as_ptr(), buf.len(), "buf") };
        let slice = result.expect("ffi_slice should succeed for valid (ptr, len)");
        assert_eq!(slice, &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[test]
    fn huge_len_with_non_null_pointer_is_rejected_without_deref() {
        // A hostile C caller can pass a 1-byte buffer with usize::MAX as the
        // length. Rust slice construction requires len <= isize::MAX; the helper
        // must reject BEFORE from_raw_parts, never reading through the pointer.
        clear_last_error_for_test();
        let byte = 0u8;
        let result = unsafe { ffi_slice(&byte as *const u8, usize::MAX, "buf") };
        assert_eq!(result, Err(TstError::InvalidConfig as i32));
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert!(s.to_str().unwrap().contains("buf"));
    }

    #[test]
    fn boundary_len_at_isize_max_plus_one_is_rejected() {
        clear_last_error_for_test();
        let byte = 0u8;
        let result = unsafe { ffi_slice(&byte as *const u8, (isize::MAX as usize) + 1, "nal") };
        assert_eq!(result, Err(TstError::InvalidConfig as i32));
    }

    // --- ffi_slice_mut ---

    #[test]
    fn mut_null_pointer_with_zero_len_returns_empty_slice() {
        clear_last_error_for_test();
        let result = unsafe { ffi_slice_mut(core::ptr::null_mut(), 0, "out_buf") };
        assert_eq!(result, Ok(&mut [][..]));
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[test]
    fn mut_null_pointer_with_nonzero_len_records_error_and_returns_code() {
        clear_last_error_for_test();
        let result = unsafe { ffi_slice_mut(core::ptr::null_mut(), 5, "out_buf") };
        assert_eq!(result, Err(TstError::InvalidConfig as i32));
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert!(
            s.to_str().unwrap().contains("null out_buf"),
            "expected 'null out_buf' in message, got: {:?}",
            s.to_str().unwrap()
        );
    }

    #[test]
    fn mut_non_null_pointer_with_zero_len_returns_empty_slice() {
        clear_last_error_for_test();
        let mut buf = [0u8; 4];
        let result = unsafe { ffi_slice_mut(buf.as_mut_ptr(), 0, "out_buf") };
        assert_eq!(result, Ok(&mut [][..]));
        assert_eq!(unsafe { tst_get_last_error() }, 0);
    }

    #[test]
    fn mut_non_null_pointer_with_nonzero_len_returns_writable_slice() {
        clear_last_error_for_test();
        let mut buf = [0u8; 4];
        let result = unsafe { ffi_slice_mut(buf.as_mut_ptr(), buf.len(), "out_buf") };
        let slice = result.expect("ffi_slice_mut should succeed for valid (ptr, len)");
        // Verify it is actually writable — the contents differ from what the
        // helper would return if the code were wrong (e.g., an empty slice).
        slice[0] = 0xAB;
        assert_eq!(
            buf[0], 0xAB,
            "write through slice must reach the original buffer"
        );
    }

    #[test]
    fn mut_huge_len_with_non_null_pointer_is_rejected_without_deref() {
        clear_last_error_for_test();
        let mut byte = 0u8;
        let result = unsafe { ffi_slice_mut(&mut byte as *mut u8, usize::MAX, "out_buf") };
        assert_eq!(result, Err(TstError::InvalidConfig as i32));
        let s_ptr = unsafe { crate::error::tst_get_last_error_str() };
        let s = unsafe { core::ffi::CStr::from_ptr(s_ptr) };
        assert!(s.to_str().unwrap().contains("out_buf"));
    }

    #[test]
    fn mut_boundary_len_at_isize_max_plus_one_is_rejected() {
        clear_last_error_for_test();
        let mut byte = 0u8;
        let result =
            unsafe { ffi_slice_mut(&mut byte as *mut u8, (isize::MAX as usize) + 1, "cap") };
        assert_eq!(result, Err(TstError::InvalidConfig as i32));
    }
}
