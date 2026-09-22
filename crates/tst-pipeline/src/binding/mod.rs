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
//! - [`kind`] — [`BindingErrorKind`]: the one error-kind table (spec Arc 2
//!   §3.3) every binding resolves its per-domain kind name from,
//!   discriminated by the frozen C `TST_E_*` codes, plus [`BindingError`]
//!   = kind + detail.
//! - [`owned`] — [`Owned<T, S>`](owned::Owned): `Mutex<Option<T>>` slot +
//!   lock-free cancel + construction-time snapshot, with the poison and
//!   panic policy of spec Arc 2 §3.2 (readers recover, mutators refuse; a
//!   panic inside a closure is reported, never poisons the mutex).
//! - [`mod@panic`] — `catch_unwind` + payload-to-string, once, for every
//!   binding's outer boundary (`ffi_catch` / `jni_catch` delegate here
//!   once the bindings re-point in Arc 2 WP-B).
//! - [`shells`] — [`Close`] for the seven pipeline shells, and
//!   [`SendHalf`] / [`RecvHalf`] for raw transports. These live here
//!   because the orphan rule forbids the binding crates from writing them.

pub mod kind;
pub mod owned;
pub mod panic;
pub mod shells;

pub use kind::{BindingError, BindingErrorKind};
pub use owned::{Close, CloseFailure, FlagCancel, HandleState, Owned};
pub use shells::{RecvHalf, SendHalf};
