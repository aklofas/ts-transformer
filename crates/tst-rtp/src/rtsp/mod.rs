//! RTSP/1.0 + RTSP/2.0 client (sync facade).
//! RTSP server (sync facade over internal tokio Runtime).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

pub(crate) mod auth;
pub mod client;
pub(crate) mod digest;
// Hidden: `pub` only so the fuzz workspace reaches the framing functions
// (see the module's items); not part of the supported surface.
#[doc(hidden)]
pub mod framing;
// Hidden: `pub` only so the fuzz workspace reaches the wire-message parsers;
// not part of the supported surface.
#[doc(hidden)]
pub mod message;
#[cfg(feature = "rtsp-server")]
pub mod server;
