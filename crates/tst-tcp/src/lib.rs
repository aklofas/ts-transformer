#![doc = include_str!("../README.md")]
//!
//! Raw MPEG-TS over TCP — caller + listener, plain + TLS.
//!
//! This crate provides a single [`TcpTransport`] type that implements both
//! `tst_core::transport::Transport` (sender) and `tst_core::transport::RecvTransport`
//! (receiver). The role is determined by which pipeline shell consumes it —
//! all four combinations work:
//!
//! 1. Caller + sender: `TcpTransport::connect("tcp://host:port")` then
//!    `MuxSender::new(transport, cfg)`
//! 2. Caller + receiver: `TcpTransport::connect("tcp://host:port")` then
//!    `DemuxReceiver::new(transport)`
//! 3. Listener + sender: `TcpListener::bind("0.0.0.0:7001")?.accept_blocking()?`
//!    then `MuxSender::new(transport, cfg)`
//! 4. Listener + receiver: same listener path then `DemuxReceiver::new`
//!
//! URL schemes:
//! - `tcp://host:port` — plain TCP caller
//! - `tcps://host:port` — TLS caller (rustls 0.23, native cert store)
//! - `tcp://0.0.0.0:port?listen=1` — plain TCP listener
//! - `tcps://0.0.0.0:port?listen=1&cert=path/server.crt&key=path/server.key` — TLS listener

#![warn(rustdoc::broken_intra_doc_links)]

pub mod builder;
pub mod config;
pub mod error;
pub mod listener;
pub mod stats;
pub mod transport;
pub mod url;

#[cfg(feature = "tls")]
pub mod tls;

mod binding_kind;

mod recv_knobs;

pub use builder::{TcpListenerBuilder, TcpTransportBuilder};
pub use config::SocketConfig;
pub use listener::TcpListener;
pub use stats::TcpStats;
pub use transport::{TcpCancelHandle, TcpTransport};
