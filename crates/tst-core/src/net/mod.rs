//! Network primitives shared across transport crates.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! This module hosts low-level helpers used by `tst-rtp`, `tst-udp`, and
//! future transport crates. Public for binding-crate use; not part of
//! the user-facing API.

pub mod io_classify;
pub mod udp_socket;

pub use io_classify::{SendClass, classify_send_error};
