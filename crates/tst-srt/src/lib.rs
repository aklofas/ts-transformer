#![doc = include_str!("../README.md")]
//!
//! TS Transformer SRT transport — safe libsrt wrapper, SrtSocket / SrtListener /
//! Builder, URL parsing, Transport + RecvTransport implementations, and the
//! URL → managed-shell `from_url` family ([`shells`]).
//!
//! This crate provides the SRT-specific concrete types. The transport
//! traits themselves live in [`tst_core`]; the transport-agnostic
//! Sender/Receiver shells live in `tst_pipeline`.
//!
//! Quick start:
//!
//! ```no_run
//! use tst_srt::SrtSocketBuilder;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut b = SrtSocketBuilder::new();
//! b.latency_ms(120);
//! let mut socket = b.connect("127.0.0.1:1234")?;
//!
//! socket.send(b"hello")?;
//! # Ok(())
//! # }
//! ```

pub mod addr;
pub mod builder;
pub mod config;
pub mod error;
pub mod init;
pub mod listen_rules;
pub mod listener;
pub mod opener;
pub mod options;
pub mod shells;
pub mod socket;
pub mod transport;
pub mod url;

mod binding_kind;
mod exit_guard;

// Top-level re-exports for the most common types.
pub use builder::{SrtListenerBuilder, SrtSocketBuilder};
pub use config::{SrtListenerConfig, SrtSocketConfig};
pub use error::{AcceptError, BindError, RecvError, SendError, SrtConnectError, SrtError};
pub use listen_rules::{ListenRule, ListenRuleError, ListenRules};
pub use listener::SrtListener;
pub use options::{Congestion, KeyLength, MaxBandwidth, PacketFilter, Passphrase, Role, StreamId};
pub use socket::{SrtSocket, SrtStats};
pub use transport::SrtTransport;
// `ResourceCancel` is the SRT-shaped cancellation primitive (defined
// in `tst-core` for layering reasons but the implementation is sized for
// libsrt's `SRTSOCKET` integer handle); re-exported here so
// `tst_srt::ResourceCancel` works without an explicit `tst_core` import.
pub use tst_core::ResourceCancel;
#[allow(deprecated)]
pub use tst_core::SrtCancelHandle;
pub use url::{SrtUrl, SrtUrlError};

// The names these types had before the `Srt` prefix; removed in 0.10.0.
#[allow(deprecated)]
pub use builder::{ListenerBuilder, SocketBuilder};
#[allow(deprecated)]
pub use config::{ListenerConfig, SocketConfig};
#[allow(deprecated)]
pub use error::ConnectError;
#[allow(deprecated)]
pub use listener::Listener;
#[allow(deprecated)]
pub use socket::{Socket, Stats};
#[allow(deprecated)]
pub use url::UrlError;

/// Number of `tst-srt` operations that have entered and not yet returned.
/// **Unstable; exists so the exit tests can observe a parked thread.**
#[doc(hidden)]
pub use exit_guard::operations_in_flight;
