#![doc = include_str!("../README.md")]
//!
//! Raw MPEG-TS over UDP — unicast + multicast (IPv4 + IPv6).
//!
//! This crate provides [`UdpTransport`] (sender) and [`UdpRecvTransport`]
//! (receiver) implementing the `tst_core::transport::{Transport, RecvTransport}`
//! traits. URL semantics are ffmpeg-compatible:
//!
//! - `udp://host:port` — unicast send/recv
//! - `udp://@group:port` — multicast recv (the `@` prefix is the ffmpeg convention)
//! - `udp://group:port` (group in 224.0.0.0/4 or ff00::/8) — multicast send
//!
//! Query parameters: `iface`, `ttl`, `tos`, `rcvbuf`, `sndbuf`, `pkt_size (send-only)`, `localaddr`.
//!
//! # Examples
//!
//! See `examples/` for end-to-end runnable code.

#![warn(rustdoc::broken_intra_doc_links)]

pub mod builder;
pub mod config;
pub mod error;
pub mod recv;
pub mod stats;
pub mod transport;
pub mod url;

mod binding_kind;

pub use builder::{UdpRecvTransportBuilder, UdpTransportBuilder};
pub use config::SocketConfig;
pub use error::{UdpError, UdpErrorKind};
pub use recv::UdpRecvTransport;
pub use stats::UdpStats;
pub use transport::UdpTransport;
pub use url::{UdpUrl, UdpUrlError};
