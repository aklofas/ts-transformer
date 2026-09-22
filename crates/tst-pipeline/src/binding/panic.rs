//! Panic isolation shared by every binding's outer boundary.
//!
//! `tst-c`'s `ffi_catch` and `tst-jni`'s `jni_catch` each carried a private
//! copy of the payload-to-string helper and of the
//! `catch_unwind(AssertUnwindSafe(f))` shape; this module is the single
//! definition both delegate to (Arc 2 §3.1 `panic.rs`).

use std::panic::AssertUnwindSafe;

/// Run `f` inside a panic boundary: `Ok(f())`, or `Err(detail)` where
/// `detail` is [`payload_message`] of the panic payload.
///
/// The std default panic hook still prints the panic to stderr before
/// this returns — bindings that want silence install their own hook; this
/// helper only stops the unwind from crossing a foreign frame.
///
/// `AssertUnwindSafe` is sound for the same reason it is at every binding
/// boundary today: the caller only ever *reports* the panic (an error kind
/// plus `detail`) and never re-enters state the closure may have torn —
/// `Owned::with_mut` hands the closure a `&mut T` borrowed from a guard
/// that lives OUTSIDE this boundary, so the mutex is not poisoned; what
/// happens to the slot is `Owned`'s decision (spec Arc 2 §3.2 as amended:
/// a mutator panic drops `T`, a reader panic keeps it).
pub fn catch<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|p| payload_message(&*p))
}

/// Best-effort detail string from a `catch_unwind` payload: the `&'static
/// str` of a literal `panic!`, the `String` of a formatted one, or the
/// fixed text `"non-string panic payload"` for anything else
/// (`panic_any`). Byte-identical to the strings `tst-c` and `tst-jni`
/// produced before Arc 2, so their error-detail tests are unchanged.
pub fn payload_message(payload: &(dyn core::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        String::from(*s)
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::from("non-string panic payload")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_returns_the_closure_value() {
        assert_eq!(catch(|| 42u32), Ok(42));
    }

    #[test]
    fn catch_reports_a_static_str_payload() {
        // `panic!("literal")` with no format args carries a `&'static str`.
        assert_eq!(
            catch(|| -> u32 { panic!("static boom") }),
            Err(String::from("static boom"))
        );
    }

    #[test]
    fn catch_reports_a_formatted_string_payload() {
        // A formatted panic carries an owned `String`.
        let n = 7;
        assert_eq!(
            catch(|| -> u32 { panic!("boom {n}") }),
            Err(String::from("boom 7"))
        );
    }

    #[test]
    fn payload_message_falls_back_for_non_string_payloads() {
        // `std::panic::panic_any` lets a test raise an arbitrary payload.
        // The closure diverges; pin `R = ()` so never-type fallback is not
        // relied on.
        let r: Result<(), _> = std::panic::catch_unwind(|| std::panic::panic_any(123i32));
        let payload = r.expect_err("panic_any must unwind");
        assert_eq!(payload_message(&*payload), "non-string panic payload");
    }

    #[test]
    fn payload_message_handles_both_string_shapes() {
        let s: Box<dyn core::any::Any + Send> = Box::new("as &str");
        assert_eq!(payload_message(&*s), "as &str");
        let s: Box<dyn core::any::Any + Send> = Box::new(String::from("as String"));
        assert_eq!(payload_message(&*s), "as String");
    }
}
