//! JNI panic isolation helper — the JVM twin of `tst-c`'s `crate::panic::ffi_catch`.
//!
//! Every `Java_org_tstrans_*` native body is run inside [`jni_catch`], which
//! converts a Rust panic into a thrown `java.lang.RuntimeException` plus a
//! caller-supplied default return value. Without this, a panic unwinding out of
//! `extern "system"` aborts the JVM process (panics across an `extern "C"`-ABI
//! boundary are an abort, not unwinding).
//!
//! # Default-value convention
//!
//! Once a Java exception is pending, the JVM **discards** the native's return
//! value — so the `default` passed here only has to be a valid value of the
//! return type, never a meaningful error signal (unlike `tst-c`, where the
//! return value *is* the error code). Pass the type's zero:
//!
//! | Native return type                  | Default to pass        |
//! |-------------------------------------|------------------------|
//! | `jlong` / `jint`                    | `0`                    |
//! | `jboolean`                          | `false` (`JNI_FALSE`)  |
//! | `jobject` / `jstring` / `jbyteArray`| `std::ptr::null_mut()` |
//! | `()` (void natives)                 | `()`                   |

use crate::jutil::FindClass;
use std::panic::{AssertUnwindSafe, catch_unwind};

use jni::strings::JNIString;
use jni::{Env, EnvUnowned, Outcome, jni_str};

/// Run `f` inside a panic boundary. On panic, throw a `RuntimeException`
/// (unless a Java exception is already pending) and return `default`. On
/// success, return `f`'s value unchanged.
///
/// `AssertUnwindSafe` is sound here for the same reason as `tst-c`'s
/// `ffi_catch`: the panic arm only *throws + returns a default*; it never
/// observes potentially-broken state behind the `&mut Env` or any captured
/// handle. A panic mid-body either drops an in-progress `Box` (leaking
/// nothing) or leaves the registry entry usable — the lock simply unlocks
/// (poison-tolerated) on unwind; any partial mutation persists.
pub(crate) fn jni_catch<'local, R, F>(env: &mut EnvUnowned<'local>, default: R, f: F) -> R
where
    F: FnOnce(&mut Env<'local>) -> R,
{
    // `with_env_no_catch`: this function keeps its own panic boundary (below)
    // so the thrown message stays what it was. The closure never returns
    // `Err`; `None` means the body panicked and the exception is thrown.
    let outcome = env.with_env_no_catch(|env| -> jni::errors::Result<Option<R>> {
        match catch_unwind(AssertUnwindSafe(|| f(env))) {
            Ok(value) => Ok(Some(value)),
            Err(payload) => {
                // Do not clobber an exception the body already raised before panicking.
                if !env.exception_check() {
                    let detail = panic_payload_message(&*payload);
                    let _ = env.throw_new(
                        FindClass(jni_str!("java/lang/RuntimeException")),
                        JNIString::from(format!("native panic in tst-jni: {detail}")),
                    );
                }
                Ok(None)
            }
        }
    });
    match outcome.into_outcome() {
        Outcome::Ok(Some(value)) => value,
        Outcome::Ok(None) | Outcome::Err(_) | Outcome::Panic(_) => default,
    }
}

/// Declares JNI entry points. Each block emits the `#[unsafe(no_mangle)] pub
/// extern "system"` function with its `EnvUnowned`/`JClass` parameters and runs
/// the body under [`jni_catch`] with the given default, so a Rust panic becomes
/// a Java `RuntimeException` and never unwinds into the JVM.
///
/// ```ignore
/// jni_native! {
///     fn Java_org_tstrans_X_nY<'local>(env, _class, handle: jlong) -> jboolean = false => {
///         /* body, with `env: &mut Env` */
///     }
/// }
/// ```
///
/// The author still writes the mangled name: the link against the Java
/// `native` declaration is the check that it is right. The lifetime
/// parameter is optional; without it the parameters use `'_`. A native that
/// returns nothing writes `= ()`. The rail `jni-catch-coverage.sh` refuses a
/// bare `extern "system"` entry point.
///
/// The environment parameter is always bound `mut`, because [`jni_catch`]
/// borrows it mutably; a native that does not use it still names it `_env`
/// and gets a `mut _env` binding, which is harmless. Inside the body the
/// same name is the closure's `&mut Env`.
macro_rules! jni_native {
    () => {};
    (
        $(#[$meta:meta])*
        fn $name:ident <$lt:lifetime> ( $env:ident, $class:ident $(, $arg:ident : $aty:ty)* $(,)? )
            $(-> $ret:ty)? = $default:expr => $body:block
        $($rest:tt)*
    ) => {
        $(#[$meta])*
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<$lt>(
            mut $env: ::jni::EnvUnowned<$lt>,
            $class: ::jni::objects::JClass<$lt>,
            $( $arg: $aty ),*
        ) $(-> $ret)? {
            $crate::panic::jni_catch(&mut $env, $default, |$env| $body)
        }
        $crate::panic::jni_native! { $($rest)* }
    };
    (
        $(#[$meta:meta])*
        fn $name:ident ( $env:ident, $class:ident $(, $arg:ident : $aty:ty)* $(,)? )
            $(-> $ret:ty)? = $default:expr => $body:block
        $($rest:tt)*
    ) => {
        $(#[$meta])*
        #[unsafe(no_mangle)]
        pub extern "system" fn $name(
            mut $env: ::jni::EnvUnowned<'_>,
            $class: ::jni::objects::JClass<'_>,
            $( $arg: $aty ),*
        ) $(-> $ret)? {
            $crate::panic::jni_catch(&mut $env, $default, |$env| $body)
        }
        $crate::panic::jni_native! { $($rest)* }
    };
}
pub(crate) use jni_native;

/// Best-effort detail string from a `catch_unwind` payload. Mirrors
/// `tst-c`'s `panic_payload_message`.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        String::from(*s)
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::from("non-string panic payload")
    }
}
