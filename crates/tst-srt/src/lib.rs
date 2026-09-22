#![doc = include_str!("../README.md")]
//!
//! TS Transformer SRT transport — safe libsrt wrapper, Socket / Listener /
//! Builder, URL parsing, Transport + RecvTransport implementations.
//!
//! This crate provides the SRT-specific concrete types. The transport
//! traits themselves live in [`tst_core`]; the transport-agnostic
//! Sender/Receiver shells live in `tst_pipeline`.
//!
//! Quick start:
//!
//! ```no_run
//! use tst_srt::SocketBuilder;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut b = SocketBuilder::new();
//! b.latency_ms(120);
//! let mut socket = b.connect("127.0.0.1:1234")?;
//!
//! socket.send(b"hello")?;
//! # Ok(())
//! # }
//! ```

#![warn(rustdoc::broken_intra_doc_links)]

pub mod addr;
pub mod builder;
pub mod config;
pub mod error;
pub mod init;
pub mod listener;
pub mod options;
pub mod socket;
pub mod transport;
pub mod url;

mod binding_kind;

// Top-level re-exports for the most common types.
pub use builder::{ListenerBuilder, SocketBuilder};
pub use config::{ListenerConfig, SocketConfig};
pub use error::{AcceptError, BindError, ConnectError, RecvError, SendError, SrtError};
pub use listener::Listener;
pub use options::{Congestion, KeyLength, MaxBandwidth, PacketFilter, Passphrase, Role, StreamId};
pub use socket::{Socket, Stats};
pub use transport::SrtTransport;
// `SrtCancelHandle` is the SRT-shaped cancellation primitive (defined
// in `tst-core` for layering reasons but the implementation is sized for
// libsrt's `SRTSOCKET` integer handle); re-exported here so
// `tst_srt::SrtCancelHandle` works without an explicit `tst_core` import.
pub use tst_core::SrtCancelHandle;
pub use url::{SrtUrl, UrlError, UrlOverlay};
