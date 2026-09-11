//! TS Transformer pipeline shells.
//!
//! Provides the standard sender and receiver shells over the
//! transport traits defined in [`tst_core`]. Concrete transport
//! impls live in dedicated crates: `tst-srt`, `tst-udp`, `tst-rtp`
//! (which also carries the RTSP client/server), `tst-tcp`, and
//! `tst-rist`.
//!
//! ## Quick start — push pre-muxed TS bytes through any [`Transport`]
//!
//! ```
//! use tst_pipeline::{Sender, SenderConfig};
//! use tst_core::transport::{Transport, TransportError};
//!
//! // Trivial in-memory sink so the example needs no network. Real
//! // consumers plug in `tst_srt::SrtTransport` (or any other
//! // `Transport` impl) here.
//! struct Sink(Vec<u8>);
//! impl Transport for Sink {
//!     fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
//!         self.0.extend_from_slice(b);
//!         Ok(())
//!     }
//!     fn max_payload(&self) -> usize { 1316 }
//!     fn close(&mut self) {}
//!     fn is_alive(&self) -> bool { true }
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut sender = Sender::new(Sink(Vec::new()), SenderConfig::default());
//!
//! // One pre-muxed TS packet (188 bytes, sync byte 0x47 first).
//! let mut pkt = vec![0x47u8];
//! pkt.extend(vec![0u8; 187]);
//! sender.send_ts(&pkt)?;
//! sender.flush()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Cargo features
//!
//! - `std` (default-on) — the full pipeline surface, adding the `Managed*`
//!   reconnect wrappers, `MuxPublisher`, and `ext::pairing` on top of the
//!   sender and receiver shells. With `--no-default-features` the crate is
//!   `#![no_std]` + `alloc` and exposes the **sender** shells
//!   (`MuxSender`/`Sender`/`RawSender`) and the **receiver** shells
//!   (`Receiver`/`DemuxReceiver`/`RawReceiver`) for bare-metal / FreeRTOS
//!   use; the `Managed*` wrappers, `MuxPublisher`, and `ext::pairing` stay
//!   `std`-only. The embedding binary must supply a `#[global_allocator]`.
//!   Verified in CI against `thumbv7em-none-eabihf` and
//!   `riscv32imac-unknown-none-elf`, and at runtime under QEMU. Under no_std
//!   the `MuxSender` lock is a `spin::Mutex` with no priority inheritance
//!   and no interrupt masking — drive each sender shell from a **single
//!   task** (one-sender-per-task). The type system still permits
//!   cross-task sharing (`MuxSender` is `Sync`), but doing so under a
//!   preemptive scheduler risks priority-inversion livelock: a
//!   higher-priority task spinning on a lock a preempted task holds. See
//!   the `MuxSender` docs' "`no_std` concurrency" section. The receiver
//!   shells hold no internal lock (their hot-path methods take `&mut
//!   self`), so this concurrency caveat does not apply to them.

#![warn(rustdoc::broken_intra_doc_links)]
#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
extern crate alloc;

mod mutex;

pub mod demux_receiver;
pub mod dyn_aliases;
#[cfg(feature = "std")]
pub mod ext;
#[cfg(feature = "std")]
pub mod managed_demux_receiver;
#[cfg(feature = "std")]
pub mod managed_receive;
#[cfg(feature = "std")]
pub mod mux_publisher;
pub mod mux_sender;
pub mod raw_receiver;
pub mod raw_sender;

/// Defensive ceiling on the receive buffer a shell eagerly pre-allocates
/// from `transport.max_payload()`.
///
/// A hostile or buggy `RecvTransport` could report an absurd `max_payload()`
/// (e.g. via a URL `pkt_size` that overflowed before the URL parsers were
/// bounds-checked), turning the `vec![0u8; cap]` pre-allocation in
/// [`receiver::Receiver::new`] / [`raw_receiver::RawReceiver::new`] into an
/// OOM. 256 MiB exceeds any realistic transport payload (UDP datagrams cap
/// at 64 KiB, SRT/TCP buffers at tens of MiB), so clamping here never
/// truncates legitimate traffic — it only bounds the eager allocation.
pub(crate) const MAX_RECV_BUFFER: usize = 256 * 1024 * 1024;

/// Clamp an eager receive-buffer pre-allocation to [`MAX_RECV_BUFFER`].
pub(crate) fn clamp_recv_capacity(cap: usize) -> usize {
    if cap > MAX_RECV_BUFFER {
        tracing::warn!(
            requested = cap,
            ceiling = MAX_RECV_BUFFER,
            "transport.max_payload() exceeds recv-buffer ceiling; clamping pre-allocation"
        );
        MAX_RECV_BUFFER
    } else {
        cap
    }
}
pub mod receiver;
#[cfg(feature = "std")]
pub mod reconnect;
pub mod sender;
pub mod shell_error;

// Top-level re-exports of the most common types.
pub use demux_receiver::{
    ByteSink, DemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource, DemuxReceiverStats,
};
pub use dyn_aliases::{
    BoxedDemuxReceiver, BoxedMuxSender, BoxedRawReceiver, BoxedRawSender, BoxedReceiver,
    BoxedSender,
};
#[cfg(feature = "std")]
pub use managed_demux_receiver::{ManagedDemuxReceiver, ManagedDemuxReceiverConfig};
#[cfg(feature = "std")]
pub use managed_receive::{FactoryCancel, ManagedRecvTransport};
#[cfg(feature = "std")]
pub use mux_publisher::{MuxPublisher, MuxPublisherError, MuxPublisherStats};
pub use mux_sender::{MuxSender, MuxSenderError, MuxSenderErrorSource, MuxSenderStats};
// Pairing is intentionally NOT re-exported at the crate root. It lives
// under `ext::pairing` to signal its opt-in, extension-module nature.
// Callers write `use tst_pipeline::ext::pairing::Pairer` explicitly.
// See `crate::ext` for rationale.
pub use raw_receiver::{
    RawReceiver, RawReceiverConfig, RawReceiverError, RawReceiverErrorSource, RawRecvStats,
};
pub use raw_sender::{
    RawSendStats, RawSender, RawSenderConfig, RawSenderError, RawSenderErrorSource,
};
pub use receiver::{Receiver, ReceiverConfig, ReceiverError, ReceiverErrorSource, ReceiverStats};
#[cfg(feature = "std")]
pub use reconnect::{
    BackoffStrategy, GapBuffer, ManagedStatsHandle, ManagedTransport, ManagedTransportStats,
    OverflowPolicy, ReconnectMode, ReconnectPolicy, RecvEndReason, RecvEndReasonHandle,
};
pub use sender::{
    Sender, SenderConfig, SenderError, SenderErrorSource, SenderStats, TsFramingMode,
};
pub use shell_error::{ShellError, ShellErrorKind};

// Re-export the core trait types for caller convenience.
pub use tst_core::transport::{
    BrokenCause, RecvTransport, Transport, TransportCancel, TransportError,
};

// Re-export the concrete SRT cross-thread shutdown primitive at the
// crate root so FFI binding authors (`tst-jni`, `tst-uniffi`,
// `tst-pyo3`, `tst-c`) have a single import path:
// `tst_pipeline::SrtCancelHandle`.
//
// `SrtCancelHandle` is SRT-shaped (wraps a libsrt `SRTSOCKET` integer
// handle with `i64::MIN` reserved as the cancelled sentinel). It is
// defined in `tst-core` for layering reasons — non-SRT transports that
// arrive in the future will add their own cancel primitives shaped for
// their underlying I/O. The pipeline-layer trait abstraction is
// `TransportCancel` above; shells accept `Option<Arc<dyn TransportCancel
// + Send + Sync>>` via `cancel_handle()`. This re-export lets binding
// authors name the concrete SRT-side type when they need to construct
// one or type-erase to it. See [`crate`]'s `srt-cancel-handle.md` doc
// for the full pattern.
pub use tst_core::SrtCancelHandle;
