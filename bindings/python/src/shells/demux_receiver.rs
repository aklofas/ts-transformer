//! `DemuxReceiver`: any transport in, `DemuxEvent`s out.
//!
//! Wraps `tst_pipeline::DemuxReceiver<Box<dyn RecvTransport>>`, opened from a
//! URL by `DemuxReceiver.open(url, demux_config)`, and iterated for
//! `DemuxEvent` instances.
//!
//! - `__iter__` returns self; `__next__` blocks (releasing the GIL) on the
//!   next `recv_event()` until an event arrives or the transport closes,
//!   errors or cancels.
//! - Events are converted by the same path as
//!   `tstrans.mpegts.Demuxer.__next__` (`crate::mpegts::convert_event`),
//!   so Python sees the existing `tstrans.mpegts.DemuxEvent.*` hierarchy.
//! - The optional `DemuxerConfig` dataclass is lifted onto
//!   `DemuxReceiver::with_demux_options` by
//!   `crate::mpegts::build_demuxer_config`.
//! - The shell lives in an `Owned` and every method takes `&self`; a
//!   concurrent `close()` latches the shared cancel FIRST (outside the
//!   slot), which wakes the parked recv, and only then takes the inner.
//! - The transport's error domain rides in the snapshot and stays readable
//!   after `close()`; the transport's end-reason handle is attached to the
//!   `Owned`, so `end_reason()` keeps answering after `close()`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyType;

use tst_core::transport::{RecvTransport, TransportCancel};
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
    TsBytesDemuxedHandle,
};

use crate::generated::stats_mpegts::MuxerStats;
use crate::generated::stats_shells::SocketStats;
use crate::mpegts::demux_error_to_pyerr;
use crate::raise::{pyok, raise};
use crate::shells::cancel::{PyCancelHandle, cancel_source};
use crate::shells::end_reason::{end_detail, end_reason_to_py};
use crate::util::{CancelSource, close_owned, drop_owned};

/// Demux-sourced failures keep `demux_error_to_pyerr` (a `DemuxError`);
/// transport-sourced ones are `BindingError`s raised in `domain`, and the
/// peer-EOS rule maps a receiver shell's `Transport(TransportError::Closed)`
/// to `EndOfStream`, which an iterator reports as `StopIteration` (a clean
/// end), never as an error.
pub(crate) fn demux_recv_err(py: Python<'_>, domain: &Domain, e: DemuxReceiverError) -> PyErr {
    match e.source {
        DemuxReceiverErrorSource::Demux(d) => demux_error_to_pyerr(py, &d),
        _ => {
            let be = BindingError::from(e);
            if be.kind == BindingErrorKind::EndOfStream {
                pyo3::exceptions::PyStopIteration::new_err(())
            } else {
                raise(py, domain, be)
            }
        }
    }
}

/// Python demux receiver over any transport: owns a `Demuxer` and a
/// transport. Open it with a URL (`srt://:7000?mode=listener`,
/// `udp://127.0.0.1:5000`, ...) and iterate the emitted `DemuxEvent`s.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// from tstrans.shells import DemuxReceiver
///
/// with DemuxReceiver.open("srt://:7000?mode=listener") as rx:
///     for event in rx:
///         match event:
///             case DemuxEvent.Video(...): ...
///             case DemuxEvent.Metadata(...): ...
///             case DemuxEvent.ProgramMap(...): ...
/// ```
#[pyclass(name = "DemuxReceiver", module = "tstrans.shells")]
pub(crate) struct PyDemuxReceiver {
    /// The binding layer's handle state machine: a `__next__` parked in
    /// `recv_event` holds the slot only inside `with_mut`, under
    /// `py.detach`; `close()` (cancel-first) ends it instead of
    /// waiting behind it.
    owned: Owned<RustDemuxReceiver<Box<dyn RecvTransport>>, Snapshot<TsBytesDemuxedHandle>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
    /// The application-cancel latch of the publish mount this receiver was
    /// taken from; `None` on every other receiver. The shared binding layer
    /// reads only a handle's own latch, so this layer carries the mount's
    /// cancel across: a mount cancel must beat events already queued, like
    /// the receiver's own cancel.
    mount_cancel: Option<Arc<dyn TransportCancel>>,
}

impl Drop for PyDemuxReceiver {
    fn drop(&mut self) {
        drop_owned(&self.owned.snapshot().domain, &self.owned);
    }
}

impl PyDemuxReceiver {
    /// Wrap an already-opened transport (the `open`,
    /// `Socket.into_demux_receiver()` and RTSP `into_demux_receiver()`
    /// paths meet here).
    pub(crate) fn from_opened(
        _py: Python<'_>,
        opened: Opened<Box<dyn RecvTransport>>,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> PyResult<Self> {
        let Opened {
            transport,
            cancel,
            domain,
            ..
        } = opened;
        let cancel = cancel_source(&cancel);
        // Taken before the transport moves into the shell.
        let end_reason = transport.end_reason_handle();
        let receiver = match opts {
            None => RustDemuxReceiver::new(transport),
            Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
        };
        let ts_bytes = receiver.ts_bytes_demuxed_handle();
        let mut owned = Owned::new(
            receiver,
            cancel.as_dyn(),
            Snapshot {
                domain,
                handles: ts_bytes,
            },
        );
        if let Some(h) = end_reason {
            owned = owned.with_end_reason(h);
        }
        Ok(Self {
            owned,
            cancel,
            mount_cancel: None,
        })
    }

    /// [`Self::from_opened`] for the transport of a publish mount, whose
    /// cancel latch the mount's `cancel()` sets.
    pub(crate) fn from_publish_mount(
        py: Python<'_>,
        opened: Opened<Box<dyn RecvTransport>>,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> PyResult<Self> {
        let mount_cancel = Arc::clone(&opened.cancel);
        let mut rx = Self::from_opened(py, opened, opts)?;
        rx.mount_cancel = Some(mount_cancel);
        Ok(rx)
    }

    /// Latches the receiver's own cancel when its mount was cancelled, so
    /// the gate refuses with CLOSED before the shell returns queued events.
    /// A mount cancel racing this call may still lose to it, like any
    /// concurrent cancel.
    fn observe_mount_cancel(&self) {
        if self.mount_cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            self.owned.cancel();
        }
    }

    /// The one open path: translate the config dataclass with the GIL held,
    /// then open the transport.
    fn open_with(
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let demux_opts = match demux_config {
            None => None,
            Some(cfg_obj) => Some(crate::mpegts::build_demuxer_config(py, cfg_obj)?),
        };
        let opened = crate::shells::open::open_recv(py, url)?;
        Self::from_opened(py, opened, demux_opts)
    }
}

#[pymethods]
impl PyDemuxReceiver {
    /// Open a `DemuxReceiver` on `url` (GIL released). For `srt://` the URL
    /// must carry `?mode=listener`; the first accept is not cancellable.
    ///
    /// `demux_config` is an optional `tstrans.mpegts.DemuxerConfig`
    /// dataclass; when `None`, defaults are used.
    ///
    /// Raises `ValueError` for an unsupported scheme and the transport's
    /// error class for a bad URL. An `srt://` bind or accept fault raises
    /// `SrtError(kind=BROKEN)` with the message prefixed `bind: ` or
    /// `accept: `.
    #[classmethod]
    #[pyo3(signature = (url, demux_config = None))]
    fn open(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Self::open_with(py, url, demux_config)
    }

    /// Iterator protocol: `iter(rx)` returns `self`.
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Block until the next `DemuxEvent` is available. Returns a
    /// `tstrans.mpegts.DemuxEvent.*` subclass instance.
    ///
    /// Raises `StopIteration` on clean EOF (transport closed cleanly,
    /// demuxer drained); the transport's error class on transport-side failure;
    /// `DemuxError` on demuxer-side failure (strict-mode rejection,
    /// malformed PMT/PES).
    fn __next__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.observe_mount_cancel();
        let res = crate::util::allow_threads_parking(py, || {
            self.owned.with_mut_unless_cancelled(|rx| rx.recv_event())
        });
        match res {
            Err(state) => Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(state),
            )),
            Ok(Ok(None)) => Err(pyo3::exceptions::PyStopIteration::new_err(())),
            Ok(Ok(Some(ev))) => crate::mpegts::convert_event(py, &ev),
            Ok(Err(e)) => Err(demux_recv_err(py, &self.owned.snapshot().domain, e)),
        }
    }

    /// Return a shareable cancel handle. Calling `.cancel()` on the
    /// returned handle wakes any thread currently parked in `__next__`.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyCancelHandle>> {
        Py::new(
            py,
            PyCancelHandle::for_shell(&self.cancel, self.owned.cancel_arc()),
        )
    }

    /// Snapshot of the scheme-neutral 16-field wire stats.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<SocketStats>> {
        // Release the GIL before taking the inner lock, like `close()`.
        let core = pyok(
            py,
            &self.owned.snapshot().domain,
            crate::util::allow_threads_parking(py, || {
                self.owned
                    // counters live inside the shell and have no value to report while a parked call holds it
                    .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |rx| {
                        rx.socket_stats().unwrap_or_default()
                    })
            }),
        )?;
        Py::new(py, SocketStats::from(&core))
    }

    /// Aligned TS bytes the demuxer has been fed. Unchanged by `stats()`
    /// reads, returns the last value after `close()`, and never waits for
    /// a parked `__next__`.
    fn ts_bytes_demuxed(&self) -> u64 {
        self.owned.snapshot().handles.get()
    }

    /// Tuple of `(SocketStats, MuxerStats)`. Same shape as
    /// `MuxSender.stats()`, so callers can read the same
    /// `(SocketStats, MuxerStats)` tuple on both MuxSender and
    /// DemuxReceiver.
    ///
    /// Returns the transport's `CLOSED` kind if the receiver has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<SocketStats>, Py<MuxerStats>)> {
        // Release the GIL before taking the inner lock, like `socket_stats`
        // above. Extract plain Rust values under the lock, then build Python
        // objects after the guard is dropped and the GIL is reacquired.
        let raw = crate::util::allow_threads_parking(py, || {
            // counters live inside the shell and have no value to report while a parked call holds it
            self.owned
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |rx| {
                    let combined = rx.stats();
                    // SocketStats from the wire counters tracked at the pipeline
                    // layer (full SocketStats via the transport accessor isn't
                    // surfaced through the pipeline shell).
                    let mut sock_stats = tst_core::transport::SocketStats::default();
                    sock_stats.bytes_received = combined.bytes_received;
                    sock_stats.packets_received = combined.packets_received;
                    // Re-shape the demux side as a MuxerStats projection so callers
                    // get the same `(SocketStats, MuxerStats)` tuple shape on both
                    // MuxSender + DemuxReceiver.
                    let mut mux_stats = tst_core::mpegts::mux::MuxerStats::default();
                    mux_stats.ts_packets_emitted = combined.packets_received;
                    mux_stats.ts_bytes_emitted = combined.bytes_received;
                    mux_stats.programs_configured =
                        u32::try_from(combined.program_maps_seen).unwrap_or(u32::MAX);
                    mux_stats.per_stream = combined.per_stream;
                    (sock_stats, mux_stats)
                })
        });
        let (sock_stats, mux_stats) = pyok(py, &self.owned.snapshot().domain, raw)?;
        let sock_py = Py::new(py, SocketStats::from(&sock_stats))?;
        let mux_py = Py::new(py, MuxerStats::from(&mux_stats))?;
        Ok((sock_py, mux_py))
    }

    /// Wall-clock time the stream identified by `pid` last carried an
    /// item through this receiver (last emitted event), as a Unix-epoch
    /// microsecond count. `None` if `pid` was never seen — including an
    /// unrecognized PID (no range check beyond the native `u16`, mirror
    /// of `Muxer.stream_codec_stats`'s pid handling: unknown → `None`,
    /// no dedicated "bad pid" error).
    ///
    /// This deliberately differs from the C ABI's `0`-sentinel
    /// convention (the C getters have no `Option`) — Python's `None` is
    /// the honest "never" value.
    ///
    /// Raises the transport's `CLOSED` kind if the receiver has been closed — same
    /// lock discipline as `stats()`.
    fn last_seen_micros(&self, py: Python<'_>, pid: u16) -> PyResult<Option<u64>> {
        // Same GIL-released lock-then-extract shape as `stats()` above.
        let raw = crate::util::allow_threads_parking(py, || {
            self.owned
                // per-PID timestamp lives in the demuxer stats inside the shell; blocking caveat documented with the watchdog recipe
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |rx| {
                    rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen)
                })
        });
        let last_seen = pyok(py, &self.owned.snapshot().domain, raw)?;
        Ok(last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as u64))
    }

    /// Why the receive session ended, as an `EndReason`, or `None` while it
    /// has not ended (or the transport records no end reason). Lock-free
    /// and readable after `close()`.
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        match self.owned.end_reason() {
            Some(r) => end_reason_to_py(py, &r),
            None => Ok(None),
        }
    }

    /// Free-text detail for `end_reason()`: the `msg` carried by
    /// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`, else `None`.
    fn end_detail(&self) -> Option<String> {
        let reason = self.owned.end_reason()?;
        end_detail(&reason).map(str::to_owned)
    }

    /// Close the receiver. Idempotent. Fires the cancel handle BEFORE
    /// acquiring the mutex so a concurrent `__next__` parked in
    /// `recv_event` unparks promptly.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &self.owned.snapshot().domain, &self.owned)
    }

    /// `True` while the receiver owns a live transport (a `__next__`
    /// parked on another thread counts as live; the probe never waits).
    fn is_alive(&self) -> bool {
        self.owned.alive(|r| r.is_alive())
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> String {
        if self.owned.is_closed() {
            "DemuxReceiver(closed)".to_string()
        } else {
            "DemuxReceiver(open)".to_string()
        }
    }
}
