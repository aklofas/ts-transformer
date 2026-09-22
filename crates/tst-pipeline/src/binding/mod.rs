//! Binding-shared layer: the one handle state machine, panic policy and
//! error-kind table that `tst-c`, `tst-py` and `tst-jni` project instead
//! of re-implementing.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! `std`-only: it exists to sit between a foreign runtime (C caller, the
//! Python GIL, a JNI critical region) and a pipeline shell, and every one
//! of those has a standard library. Nothing here is reachable from the
//! `no_std` sender/receiver shells.
//!
//! - [`owned`] — [`Owned<T, S>`](owned::Owned): `Mutex<Option<T>>` slot +
//!   lock-free cancel + construction-time snapshot, with the poison and
//!   panic policy of spec Arc 2 §3.2 (readers recover, mutators refuse; a
//!   panic inside a closure is reported, never poisons the slot).
//! - [`mod@panic`] — `catch_unwind` + payload-to-string, once, for every
//!   binding's outer boundary (`ffi_catch` / `jni_catch` delegate here
//!   once the bindings re-point in Arc 2 WP-B).

pub mod owned;
pub mod panic;

pub use owned::{Close, CloseFailure, HandleState, Owned};
