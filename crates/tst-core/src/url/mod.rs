//! URL parsing helpers shared across transport crates.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Today's consumers: [`tst_srt::url`](https://docs.rs/tst-srt) and (soon)
//! `tst_rtp::url`. The module deliberately knows nothing about transport
//! semantics — it parses the scheme/authority/path/query structure and
//! leaves scheme-specific key recognition to each transport crate.
//!
//! Hand-rolled (no `url` crate dependency). Our schemes (`srt://`, `rtp://`,
//! `rtsp://`, `rtsps://`) all use the same simple shape:
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

pub use common::{
    ParsedUrl, UrlError, is_multicast_v4, is_multicast_v6, parse_bool, parse_host_port, parse_int,
    parse_size, parse_url,
};
