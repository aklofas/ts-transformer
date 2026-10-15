//! Test-only helpers for the inline `#[cfg(test)] mod tests` blocks.
//!
//! Declared `#[cfg(test)] mod test_support;` in `lib.rs`, so no library
//! build and no cbindgen run ever sees it.

/// Generate one `#[test]` per row for the C ABI's NULL-argument contract,
/// each under the name it had as a hand-written test.
///
/// Row: `name => arm { body };`. `body` is exactly the statements and the
/// final call the hand-written test ran (its `let` preludes included); the
/// macro wraps all of it in one `unsafe` block. Arms:
///
/// * `no_crash` — run `body`; a NULL `_close` / `_free` / `_cancel` must not crash
///   (the arm asserts nothing beyond reaching the end).
/// * `invalid_config` — `body` returns `TST_E_INVALID_CONFIG`.
/// * `negative` — `body` returns a negative `TST_E_*` code.
/// * `null` — `body` returns a null pointer.
/// * `eq(expected)` — `body` returns `expected` (a sentinel handle, or `0`).
///
/// Only the tests are generated. The production NULL guards stay written
/// out at every entry point, where a reader of that entry point sees them.
macro_rules! null_entry_tests {
    (@one [$($attr:tt)*] $name:ident no_crash { $($body:tt)* }) => {
        $($attr)*
        #[test]
        #[allow(unused_unsafe)]
        fn $name() {
            unsafe { $($body)* };
        }
    };
    (@one [$($attr:tt)*] $name:ident invalid_config { $($body:tt)* }) => {
        $($attr)*
        #[test]
        #[allow(unused_unsafe)]
        fn $name() {
            let rc = unsafe { $($body)* };
            assert_eq!(rc, $crate::error::TstError::InvalidConfig as i32);
        }
    };
    (@one [$($attr:tt)*] $name:ident negative { $($body:tt)* }) => {
        $($attr)*
        #[test]
        #[allow(unused_unsafe)]
        fn $name() {
            let rc = unsafe { $($body)* };
            assert!(rc < 0, "expected a negative TST_E_* code, got {rc}");
        }
    };
    (@one [$($attr:tt)*] $name:ident null { $($body:tt)* }) => {
        $($attr)*
        #[test]
        #[allow(unused_unsafe)]
        fn $name() {
            let p = unsafe { $($body)* };
            assert!(p.is_null());
        }
    };
    (@one [$($attr:tt)*] $name:ident eq ($expected:expr) { $($body:tt)* }) => {
        $($attr)*
        #[test]
        #[allow(unused_unsafe)]
        fn $name() {
            let got = unsafe { $($body)* };
            assert_eq!(got, $expected);
        }
    };
    ($( $(#[$attr:meta])* $name:ident => $arm:ident $(($expected:expr))? { $($body:tt)* } );+ $(;)?) => {
        $(
            $crate::test_support::null_entry_tests!(
                @one [$(#[$attr])*] $name $arm $(($expected))? { $($body)* }
            );
        )+
    };
}
pub(crate) use null_entry_tests;
