//! `Receiver<R>` — pull bytes from a `RecvTransport`, run TS sync recovery,
//! and emit 188-byte aligned packets.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! `Receiver` is the receive-side counterpart to `Sender`: where
//! `Sender` wraps raw bytes into TS packets, `Receiver` receives a byte
//! stream and recovers packet alignment via the [`sync::Syncer`] state machine.
//!
//! Feed the muxed TS stream to the underlying `RecvTransport`; call
//! [`Receiver::next_packet`] repeatedly to drain one 188-byte packet at a
//! time. On a network gap or resync, the syncer silently re-hunts and
//! returns the next aligned packet.

pub mod sync;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use sync::Syncer;
use tracing::info_span;
use tst_core::mpegts::common::TS_PACKET_SIZE;
use tst_core::transport::RecvTransport;
use tst_core::transport::TransportError;

use crate::shell_error::ShellErrorKind;

/// Application-level stats for [`Receiver`].
///
/// Mirrors the shape of [`crate::SenderStats`] on the receive
/// side. The sync-recovery counters (`bytes_skipped_for_sync`,
/// `resync_events`) reflect the [`sync::Syncer`] state machine: bytes
/// drained while hunting for alignment, and successful lock acquisitions
/// (initial lock-on and re-locks after losing sync mid-stream).
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    /// Total bytes returned to callers as valid 188-byte TS packets.
    pub bytes_received: u64,
    /// Bytes discarded while scanning for the 0x47 sync byte (HUNT and
    /// failed-VERIFY drops combined).
    pub bytes_skipped_for_sync: u64,
    /// Number of times the syncer transitioned from HUNT/VERIFY to LOCKED —
    /// counts both the initial lock-on and any re-locks after sync loss.
    pub resync_events: u64,
    /// Number of 188-byte TS packets returned to callers.
    pub packets_received: u64,
}

/// Receive shell that emits one 188-byte TS packet per call, with automatic
/// sync recovery.
///
/// `R` is any [`RecvTransport`] — typically `SrtTransport` for live
/// connections, or a test mock. The underlying transport framing (SRT live
/// mode) guarantees that each `recv_bytes` call returns one complete SRT
/// message; `Receiver` then feeds those bytes through the TS syncer to
/// extract correctly-aligned 188-byte packets.
///
/// # Closing
///
/// `Receiver` supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl emits a tracing event and lets the
///    underlying transport's `Drop` close the libsrt socket. Synchronous;
///    bounded by `SRTO_LINGER` (libsrt default 30 s, configurable via
///    `SocketBuilder::linger`).
/// 2. **Explicit close** — call [`Self::close`]. Closes the underlying
///    transport; subsequent `next_packet` calls return
///    [`TransportError::Closed`] once the syncer's internal buffer is
///    exhausted. Idempotent.
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `next_packet` within one libsrt I/O cycle (~3-10 ms).
///
/// # C ABI
///
/// `tst_receiver_close` (plain) — see `bindings/c/include/tstrans.h`.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `let _ = rx;` (Drop) or `rx.cancel_handle().map(\|c\| c.cancel());` (cross-thread) |
/// | Java | Wrap as `AutoCloseable`; `try-with-resources` calls `close()` on exit |
/// | Kotlin | Wrap as `AutoCloseable`; `.use { }` calls `close()` on exit |
/// | Swift | `deinit` calls drop; `defer { handle.cancel() }` for explicit cross-thread |
/// | Python | Wrap as `__enter__`/`__exit__`; `with ... as rx:` calls `close()` on exit |
/// | C | `tst_receiver_close(rx)` (explicit; mirrors `Drop`); `tst_receiver_cancel(handle)` from any thread |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct Receiver<R: RecvTransport> {
    transport: R,
    syncer: Syncer,
    /// Reusable scratch buffer sized to `transport.max_payload()` on
    /// construction. Avoids a per-call heap allocation for the recv itself.
    recv_buf: Vec<u8>,
    /// Transport-level counters (bytes and packets received). The
    /// sync-recovery counters live in `self.syncer` and are read out on
    /// each `stats()` call.
    bytes_received: u64,
    packets_received: u64,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

impl<R: RecvTransport> core::fmt::Debug for Receiver<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Receiver")
            .field("is_alive", &self.is_alive())
            .field("recv_buf_capacity", &self.recv_buf.capacity())
            .field("transport_kind", &core::any::type_name::<R>())
            .finish()
    }
}

/// Construction parameters for [`Receiver`].
///
/// Currently empty; reserved for future knobs (`recv_timeout`, custom
/// `Syncer` settings, etc.) that can be added non-breakingly thanks to
/// the `#[non_exhaustive]` annotation. Construct via `Default::default()`
/// and assign overrides as more fields land.
///
/// Symmetric with [`crate::SenderConfig`] and [`crate::RawSenderConfig`]
/// on the send side; the symmetry is documented in
/// `docs/reference/conventions.md`.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Default, Clone)]
pub struct ReceiverConfig {}

/// Error returned by [`Receiver::next_packet`].
///
/// # Categorization
///
/// Bindings categorize failures via [`Self::kind`] (one of 6
/// [`ShellErrorKind`] variants); power users inspect [`Self::source`]
/// for the typed inner error.
///
/// # Reachable kinds
///
/// `Receiver` can produce: `Backpressure`, `TransportBroken`, `Closed`,
/// `EndOfStream`. `Backpressure` is produced when the underlying transport
/// (e.g. `SrtTransport`) returns `TransportError::Backpressure` on a
/// recv timeout. `InputMalformed` is reachable only via `DemuxReceiver`
/// (which adds the demuxer layer on top); plain `Receiver` is
/// byte-pass-through and doesn't validate TS structure.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("Receiver error ({kind:?}): {source}")]
pub struct ReceiverError {
    pub kind: ShellErrorKind,
    #[source]
    pub source: ReceiverErrorSource,
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ReceiverErrorSource {
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl From<TransportError> for ReceiverError {
    fn from(e: TransportError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_transport(&e, crate::shell_error::Direction::Recv),
            source: ReceiverErrorSource::Transport(e),
        }
    }
}

impl crate::shell_error::ShellError for ReceiverError {
    fn kind(&self) -> ShellErrorKind {
        self.kind
    }

    fn errno_code(&self) -> Option<i32> {
        match &self.source {
            ReceiverErrorSource::Transport(t) => crate::shell_error::errno_code_from_transport(t),
        }
    }
}

impl<R: RecvTransport> Receiver<R> {
    /// Wrap a transport with the supplied config. Allocates an
    /// internal receive buffer sized to `transport.max_payload()`.
    ///
    /// `ReceiverConfig` is currently empty; construct via
    /// [`ReceiverConfig::default()`].
    pub fn new(transport: R, _config: ReceiverConfig) -> Self {
        let span = info_span!(
            target: "tst_pipeline::receiver",
            "receiver",
            transport_kind = core::any::type_name::<R>(),
        );
        let _enter = span.enter();
        tracing::info!("Receiver opened");
        drop(_enter);
        let cap = crate::clamp_recv_capacity(transport.max_payload());
        Self {
            transport,
            syncer: Syncer::new(),
            recv_buf: vec![0u8; cap],
            bytes_received: 0,
            packets_received: 0,
            _span: core::panic::AssertUnwindSafe(span),
        }
    }

    /// Block until at least one 188-byte TS packet is ready and return it.
    ///
    /// # C ABI
    ///
    /// `tst_receiver_recv_packet` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Details
    ///
    /// Internally:
    /// 1. Check whether the syncer already has a packet buffered (fast path,
    ///    avoids a transport call).
    /// 2. If not, call `recv_bytes` once to pull more data from the transport,
    ///    feed the bytes to the syncer, then retry.
    /// 3. Repeat until a packet is available or the transport closes.
    ///
    /// # Errors
    ///
    /// Returns [`ReceiverError`] with `kind` one of:
    /// - [`ShellErrorKind::TransportBroken`] — transport socket is broken.
    /// - [`ShellErrorKind::Closed`] — caller invoked `close()` (or for
    ///   `ManagedRecvTransport`, the cancel signal fired).
    /// - [`ShellErrorKind::EndOfStream`] — peer closed the connection.
    ///
    /// # Example
    /// ```
    /// use std::collections::VecDeque;
    /// use tst_pipeline::Receiver;
    /// use tst_core::transport::{RecvTransport, TransportError};
    ///
    /// // In-memory source; real callers plug in `tst_srt::SrtTransport`.
    /// struct Source(VecDeque<Vec<u8>>);
    /// impl RecvTransport for Source {
    ///     fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
    ///         match self.0.pop_front() {
    ///             Some(v) => {
    ///                 let n = v.len().min(buf.len());
    ///                 buf[..n].copy_from_slice(&v[..n]);
    ///                 Ok(n)
    ///             }
    ///             None => Err(TransportError::Closed),
    ///         }
    ///     }
    ///     fn max_payload(&self) -> usize { 1316 }
    ///     fn is_alive(&self) -> bool { !self.0.is_empty() }
    /// }
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use tst_pipeline::ReceiverConfig;
    /// // The TS syncer needs four consecutive 0x47 bytes at 188-byte
    /// // intervals to declare lock and emit the first packet — feed five
    /// // aligned packets in one transport message to satisfy that.
    /// let mut stream = Vec::new();
    /// for _ in 0..5 {
    ///     stream.push(0x47);
    ///     stream.extend(vec![0u8; 187]);
    /// }
    /// let mut rx = Receiver::new(
    ///     Source(VecDeque::from(vec![stream])),
    ///     ReceiverConfig::default(),
    /// );
    ///
    /// let pkt = rx.next_packet()?;
    /// assert_eq!(pkt[0], 0x47);
    /// assert_eq!(pkt.len(), 188);
    /// # Ok(())
    /// # }
    /// ```
    pub fn next_packet(&mut self) -> Result<[u8; 188], ReceiverError> {
        loop {
            if let Some(pkt) = self.syncer.next_packet() {
                self.bytes_received += TS_PACKET_SIZE as u64;
                self.packets_received += 1;
                return Ok(pkt);
            }
            let n = self.transport.recv_bytes(&mut self.recv_buf)?;
            // The RecvTransport contract says closed/broken transports return
            // Err(Closed), not Ok(0). Defensively treating Ok(0) as closed
            // guards against implementors that follow the io::Read convention
            // instead, and makes the loop terminate rather than spin.
            if n == 0 {
                return Err(TransportError::Closed.into());
            }
            self.syncer.push(&self.recv_buf[..n]);
        }
    }

    /// Advisory liveness check. Delegates to the underlying transport.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    /// Drop any buffered bytes and force the syncer back to HUNT.
    ///
    /// Intended for reconnect scenarios at a higher composition layer: when
    /// the underlying transport has been re-established, bytes left over
    /// from the dead connection must not seed the new alignment search.
    /// Note that `ManagedRecvTransport` itself does **not** own the
    /// `Receiver` (it lives one layer up, inside `DemuxReceiver`);
    /// `ManagedDemuxReceiver` calls this on the reconnect-detected path.
    pub fn reset_sync(&mut self) {
        self.syncer.reset();
    }

    /// Snapshot of application-level receive stats.
    ///
    /// The sync-recovery counters are read from the [`sync::Syncer`] (where
    /// the recovery logic lives); transport counters are owned by this struct.
    ///
    /// # C ABI
    ///
    /// `tst_receiver_get_stats` — see `bindings/c/include/tstrans.h`.
    pub fn stats(&self) -> ReceiverStats {
        ReceiverStats {
            bytes_received: self.bytes_received,
            packets_received: self.packets_received,
            bytes_skipped_for_sync: self.syncer.bytes_skipped_for_sync,
            resync_events: self.syncer.resync_events,
        }
    }

    /// Wire-level transport stats (RTT, packet loss, bandwidth, queue
    /// depths) sourced from the underlying
    /// [`RecvTransport::socket_stats`] implementation. Returns `None`
    /// when the transport doesn't expose comparable telemetry (test
    /// mocks) or when a managed wrapper has no live inner socket.
    ///
    /// # C ABI
    ///
    /// `tst_receiver_get_socket_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        self.transport.socket_stats()
    }

    /// Zero all stats counters. Does not affect transport state or sync state.
    ///
    /// # C ABI
    ///
    /// `tst_receiver_reset_stats` — see `bindings/c/include/tstrans.h`.
    pub fn reset_stats(&mut self) {
        self.bytes_received = 0;
        self.packets_received = 0;
        self.syncer.reset_stats();
    }

    /// Close the underlying transport. Idempotent. After close, `next_packet`
    /// will return `TransportError::Closed` once the syncer's internal buffer
    /// is exhausted. Mirrors `RawReceiver::close`.
    ///
    /// # C ABI
    ///
    /// `tst_receiver_close` — see `bindings/c/include/tstrans.h`.
    pub fn close(&mut self) {
        self.transport.close();
    }

    /// Snapshot of the underlying recv-transport's cancel handle.
    ///
    /// # C ABI
    ///
    /// `tst_receiver_cancel` — see `bindings/c/include/tstrans.h`.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.transport.cancel_handle()
    }

    /// Borrow the underlying recv-transport. Bindings use this to reach
    /// transport-specific telemetry not surfaced through the abstract
    /// [`RecvTransport::socket_stats`] (e.g. SRT's 17-field `Stats`).
    ///
    /// Returns a shared reference; mutation is intentionally not
    /// exposed — the shell owns the transport's recv-side lifecycle.
    pub fn transport(&self) -> &R {
        &self.transport
    }
}

/// Type alias for [`Receiver`] with a boxed [`RecvTransport`] trait object.
///
/// Bindings code (`tst-jni`, `tst-uniffi`, `tst-pyo3`) targets this single
/// concrete type instead of cubing per-`R` instantiation. Rust callers with a
/// custom transport keep the generic `Receiver<MyTransport>` shape.
///
/// # Example — opaque receiver from a runtime-chosen transport
/// ```no_run
/// use tst_pipeline::receiver::BoxedReceiver;
/// use tst_pipeline::{Receiver, ReceiverConfig};
/// use tst_core::RecvTransport;
///
/// fn open(transport: Box<dyn RecvTransport>) -> BoxedReceiver {
///     Receiver::new(transport, ReceiverConfig::default())
/// }
/// ```
pub type BoxedReceiver = Receiver<Box<dyn crate::RecvTransport>>;

impl<R: RecvTransport> Drop for Receiver<R> {
    fn drop(&mut self) {
        let _enter = self._span.0.enter();
        tracing::info!("Receiver closed");
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;
    use std::collections::VecDeque;
    use tst_core::transport::TransportError;

    struct MemRecv {
        queue: VecDeque<Vec<u8>>,
        alive: bool,
    }

    impl RecvTransport for MemRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            match self.queue.pop_front() {
                Some(v) => {
                    let n = v.len().min(buf.len());
                    buf[..n].copy_from_slice(&v[..n]);
                    Ok(n)
                }
                None => Err(TransportError::Closed),
            }
        }

        fn max_payload(&self) -> usize {
            1316
        }

        fn is_alive(&self) -> bool {
            self.alive
        }

        fn close(&mut self) {
            self.alive = false;
        }
    }

    fn one_packet() -> Vec<u8> {
        let mut v = vec![0x47];
        v.extend_from_slice(&[0u8; 187]);
        v
    }

    #[test]
    fn stats_starts_zero() {
        let r = Receiver::new(
            MemRecv {
                queue: VecDeque::new(),
                alive: true,
            },
            ReceiverConfig::default(),
        );
        let st = r.stats();
        assert_eq!(st.bytes_received, 0);
        assert_eq!(st.packets_received, 0);
        assert_eq!(st.bytes_skipped_for_sync, 0);
        assert_eq!(st.resync_events, 0);
    }

    #[test]
    fn stats_increment_on_aligned_packet() {
        // Feed 5 identical aligned packets so the syncer locks (needs 4
        // confirmations) and emits the first packet.
        let mut queue = VecDeque::new();
        let mut stream = Vec::new();
        for _ in 0..5 {
            stream.extend_from_slice(&one_packet());
        }
        queue.push_back(stream);
        let mut r = Receiver::new(MemRecv { queue, alive: true }, ReceiverConfig::default());
        let _ = r.next_packet();
        let st = r.stats();
        assert_eq!(st.bytes_received, 188);
        assert_eq!(st.packets_received, 1);
    }

    #[test]
    fn reset_zeros_counters() {
        let mut queue = VecDeque::new();
        let mut stream = Vec::new();
        for _ in 0..5 {
            stream.extend_from_slice(&one_packet());
        }
        queue.push_back(stream);
        let mut r = Receiver::new(MemRecv { queue, alive: true }, ReceiverConfig::default());
        let _ = r.next_packet();
        r.reset_stats();
        let st = r.stats();
        assert_eq!(st.bytes_received, 0);
        assert_eq!(st.packets_received, 0);
        assert_eq!(st.bytes_skipped_for_sync, 0);
        assert_eq!(st.resync_events, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::TransportError;

    /// Minimal `RecvTransport` mock that plays back a fixed sequence of byte
    /// messages then signals closed. Each `recv_bytes` call returns one message
    /// in its entirety — mirroring SRT live-mode framing.
    struct MockRecv {
        messages: Vec<Vec<u8>>,
        pos: usize,
        max: usize,
    }

    impl MockRecv {
        fn new(messages: Vec<Vec<u8>>) -> Self {
            let max = messages.iter().map(|m| m.len()).max().unwrap_or(1316);
            Self {
                messages,
                pos: 0,
                max,
            }
        }
    }

    impl RecvTransport for MockRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            if self.pos >= self.messages.len() {
                return Err(TransportError::Closed);
            }
            let msg = &self.messages[self.pos];
            let n = msg.len();
            buf[..n].copy_from_slice(msg);
            self.pos += 1;
            Ok(n)
        }

        fn max_payload(&self) -> usize {
            self.max
        }

        fn is_alive(&self) -> bool {
            self.pos < self.messages.len()
        }
    }

    /// Build a syntactically valid 188-byte TS packet with the given PID.
    fn ts_packet(pid: u16) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x10;
        buf
    }

    /// Happy path: a well-formed TS stream arrives as a single large message.
    /// Receiver locks, emits all packets, then returns Closed.
    #[test]
    fn emits_packets_from_single_message() {
        let mut stream = Vec::new();
        for i in 0..6u16 {
            stream.extend_from_slice(&ts_packet(i));
        }
        let mut rx = Receiver::new(MockRecv::new(vec![stream]), ReceiverConfig::default());

        let mut got = 0;
        loop {
            match rx.next_packet() {
                Ok(_) => got += 1,
                Err(e) if e.kind == crate::shell_error::ShellErrorKind::EndOfStream => break,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert_eq!(got, 6);
    }

    /// Packets split across multiple transport messages (multi-call recv path).
    #[test]
    fn emits_packets_across_transport_messages() {
        // Each transport message is exactly one TS packet.
        let messages: Vec<Vec<u8>> = (0..5u16).map(|i| ts_packet(i).to_vec()).collect();
        let mut rx = Receiver::new(MockRecv::new(messages), ReceiverConfig::default());

        let mut got = 0;
        loop {
            match rx.next_packet() {
                Ok(_) => got += 1,
                Err(e) if e.kind == crate::shell_error::ShellErrorKind::EndOfStream => break,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert_eq!(got, 5);
    }

    /// Closed transport with no prior data returns Closed immediately.
    #[test]
    fn closed_transport_returns_closed() {
        let mut rx = Receiver::new(MockRecv::new(vec![]), ReceiverConfig::default());
        assert_eq!(
            rx.next_packet().unwrap_err().kind,
            crate::shell_error::ShellErrorKind::EndOfStream,
        );
    }
}
