#![doc = include_str!("../README.md")]
//!
//! HLS publisher — segments MPEG-TS to disk + optional built-in HTTP server.
//!
//! See [`HlsPublisher`] for the entry point. Modes: LIVE (rolling window),
//! EVENT (monotone-growing + ENDLIST on finish), VOD (all segments at once
//! + ENDLIST on finish). KLV stays inside the .ts segments.
//!
//! The `serve` feature (default-on) provides the built-in HTTP server and
//! `hls://` / `hlss://` URL parsing. Without it, the crate is a pure
//! segmenter/playlist writer — bring your own web server (nginx, a media
//! server, a CDN origin).

#![warn(rustdoc::broken_intra_doc_links)]

pub mod builder;
pub mod config;
pub mod error;
pub mod publisher;
pub mod stats;
#[cfg(feature = "serve")]
pub mod url;

mod binding_kind;

#[cfg(feature = "serve")]
mod auth;
#[cfg(feature = "serve")]
mod http_server;
mod playlist;
mod segmenter;
#[cfg(feature = "tls")]
mod tls;

pub use builder::HlsPublisherBuilder;
pub use config::{HlsConfig, HlsMode};
pub use error::{HlsError, HlsErrorKind};
pub use publisher::HlsPublisher;
#[cfg(feature = "serve")]
pub use publisher::HlsServerHandle;
pub use stats::HlsStats;
#[cfg(feature = "serve")]
pub use url::{HlsUrl, HlsUrlError};
