//! URL parsing helpers shared across transport crates.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Every transport crate, `tst-pipeline` and the C, Python and JVM bindings
//! use it. The module deliberately knows nothing about transport
//! semantics — it parses the scheme/authority/path/query structure and
//! leaves scheme-specific key recognition to each transport crate.
//!
//! Hand-rolled (no `url` crate dependency). [`parse_url`] splits one simple
//! shape:
//!
//! ```text
//! scheme://[user[:password]@]host[:port][/path][?query]
//! ```
//!
//! Fragment handling (`#frag`) is not implemented — none of our transport
//! URLs use fragments.

pub mod common;
#[doc(hidden)]
pub mod query;
mod redact;

pub use common::{
    ParsedUrl, UrlError, is_multicast_v4, is_multicast_v6, parse_bool, parse_host_port, parse_int,
    parse_size, parse_url,
};
pub use redact::{query_may_hold_userinfo, redact_url};
