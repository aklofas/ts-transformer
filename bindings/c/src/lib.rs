//! `tst-c` — the C ABI artifact crate (`libtstrans.so` / `libtstrans.a` / `tstrans.h`).
//!
//! All C-ABI logic lives in the embeddable `tst-c-core` rlib; this leaf crate
//! exists to produce the cdylib/staticlib + the cbindgen header. Re-exporting
//! `tst_c_core::*` makes every `#[no_mangle]` entry point reachable from this
//! crate root, so it is retained + exported in the cdylib/staticlib.
// A binding never aborts its host process on a panic it could have avoided:
// no `.expect(` and no `unreachable!` outside tests.
#![cfg_attr(not(test), deny(clippy::expect_used, clippy::unreachable))]
pub use tst_c_core::*;
