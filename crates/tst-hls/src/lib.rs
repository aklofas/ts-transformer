#![doc = include_str!("../README.md")]
//!
//! HLS publisher — segments MPEG-TS to disk + optional built-in HTTP server.
//!
//! See [`HlsPublisher`] for the entry point. Modes: LIVE (rolling window,
//! never an ENDLIST), EVENT and VOD (every segment kept; `#EXT-X-ENDLIST`
//! on finish; they differ only in `#EXT-X-PLAYLIST-TYPE`). KLV stays
//! inside the .ts segments.
//!
//! Segment files are written to `output_dir` as bytes arrive. `playlist.m3u8`
//! is rewritten there (atomically, via a staging file and rename) on every
//! segment cut, so an external static server can serve the stream while it
//! runs; the finish path ([`Publisher::finish`] / `HlsPublisher::finish_serving`)
//! writes the terminal playlist. The built-in server renders the playlist
//! from memory on each request.
//!
//! [`Publisher::finish`]: tst_core::publisher::Publisher::finish
//!
//! The `serve` feature (default-on) provides the built-in HTTP server and
//! `hls://` / `hlss://` URL parsing. Serving HTTPS additionally needs the
//! `tls` feature (default-on, implies `serve`); without it a configured
//! certificate or key is refused with [`HlsError::TlsDisabled`]. Without
//! `serve`, the crate only writes segments plus the playlist: an external
//! web server (nginx, a media server, a CDN origin) pointed at `output_dir`
//! serves the stream, live or finished.

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
