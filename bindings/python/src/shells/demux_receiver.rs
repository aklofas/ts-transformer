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

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyType};

use tst_core::transport::RecvTransport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
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

thread_local! {
    /// The `sink_key`s of every receiver whose byte sink is running on this
    /// thread, innermost last. Each sink runs while its receiver's
    /// `__next__` holds the slot, so a slot-taking call on ANY of them from
    /// this thread would re-lock a held slot and deadlock. A stack, not a
    /// single key: A's sink may pull B, and B's sink may then call into A.
    static IN_SINK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Pops this sink's key on scope exit, unwind included.
struct InSinkGuard;

impl InSinkGuard {
    fn enter(key: usize) -> Self {
        IN_SINK.with(|s| s.borrow_mut().push(key));
        Self
    }
}

impl Drop for InSinkGuard {
    fn drop(&mut self) {
        IN_SINK.with(|s| {
            s.borrow_mut().pop();
        });
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
///             case DemuxEvent.Sample(...): ...
///             case DemuxEvent.ProgramMap(...): ...
/// ```
#[pyclass(name = "DemuxReceiver", module = "tstrans.shells")]
pub(crate) struct PyDemuxReceiver {
    /// The binding layer's handle state machine: a `__next__` parked in
    /// `recv_event` holds the slot only inside `with_mut`, under
    /// `py.detach`; `close()` (cancel-first) ends it instead of
    /// waiting behind it.
    owned: Owned<RustDemuxReceiver<Box<dyn RecvTransport>>, Snapshot<()>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
    /// First exception raised by a registered byte sink (see
    /// `add_byte_sink`). The sink closure runs inside `recv_event` (under
    /// `detach`) where it can't return a `PyResult` to the
    /// iterator, so on error it stashes the `PyErr` here (first error
    /// wins). `__next__` drains this slot AFTER `recv_event` returns and
    /// re-raises fail-loud. Separate from the slot so the closure never
    /// touches the lock it runs underneath.
    sink_error: Arc<Mutex<Option<PyErr>>>,
    /// Set by a `close()` called from this receiver's own byte sink: the
    /// slot is held by the `__next__` the sink runs under, so the close
    /// only cancels there and that `__next__` finishes it.
    close_pending: AtomicBool,
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
        let mut owned = Owned::new(
            receiver,
            cancel.as_dyn(),
            Snapshot {
                domain,
                handles: (),
            },
        );
        if let Some(h) = end_reason {
            owned = owned.with_end_reason(h);
        }
        Ok(Self {
            owned,
            cancel,
            sink_error: Arc::new(Mutex::new(None)),
            close_pending: AtomicBool::new(false),
        })
    }

    /// This receiver's `IN_SINK` key: the address of its `sink_error`,
    /// unique while the receiver lives and shared with its sink closures.
    fn sink_key(&self) -> usize {
        Arc::as_ptr(&self.sink_error) as usize
    }

    /// `true` when called, on this thread, from inside one of this
    /// receiver's own byte sinks, however many receivers deep.
    fn in_own_sink(&self) -> bool {
        let key = self.sink_key();
        IN_SINK.with(|s| s.borrow().contains(&key))
    }

    /// Refuse a slot-taking call from this receiver's own byte sink, which
    /// would otherwise deadlock on the slot its `__next__` holds.
    fn refuse_reentry(&self, method: &str) -> PyResult<()> {
        if self.in_own_sink() {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                "DemuxReceiver.{method}() cannot be called from this receiver's own byte-sink callback; only close() is allowed there"
            )));
        }
        Ok(())
    }

    /// Complete a close deferred by the own sink and end the iteration.
    fn finish_deferred_close(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        close_owned(py, &self.owned.snapshot().domain, &self.owned)?;
        Err(pyo3::exceptions::PyStopIteration::new_err(()))
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

    /// Deprecated: use `open`.
    #[staticmethod]
    #[pyo3(signature = (url, *, demux_config = None))]
    fn from_url(
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        crate::util::deprecated(
            py,
            "`DemuxReceiver.from_url` is deprecated; use `DemuxReceiver.open(url, demux_config)`",
        )?;
        Self::open_with(py, url, demux_config)
    }

    /// Deprecated: use `DemuxReceiver.open(url, demux_config)`.
    #[new]
    #[pyo3(signature = (url, *, demux_config = None))]
    fn new(py: Python<'_>, url: &str, demux_config: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        crate::util::deprecated(
            py,
            "constructing `DemuxReceiver(url, ...)` is deprecated; use `DemuxReceiver.open(url, demux_config)`",
        )?;
        Self::open_with(py, url, demux_config)
    }

    /// Iterator protocol: `iter(rx)` returns `self`.
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Register a fan-out callback that receives every 188-byte TS
    /// packet — as a fresh `bytes` — BEFORE the demuxer parses it.
    /// `callback` is `Callable[[bytes], None]`. Sinks fire in
    /// registration order; registration is append-only for the
    /// receiver's lifetime (there is no removal). Useful for tee'ing
    /// the raw transport stream (record-to-disk, parallel parser, etc.)
    /// without consuming the demuxed event iterator.
    ///
    /// Fail-loud: if `callback` raises, the exception is captured and
    /// re-raised from the *next* `__next__` / event pull, and iteration
    /// stops. Only the first sink error is surfaced; later per-packet
    /// errors are dropped.
    ///
    /// Thread-safe to call concurrently with iteration: registration
    /// acquires the `inner` lock with the GIL released (like `close()`),
    /// so it simply blocks until the in-flight `recv_event` yields,
    /// rather than deadlocking against a sink firing under the GIL.
    ///
    /// Inside a sink, calls on the same receiver are limited (also when
    /// reached through another receiver the sink drives): `close()`
    /// cancels, the in-flight pull still delivers its event, and the
    /// iteration ends with `StopIteration` on the following pull;
    /// `__next__`, `add_byte_sink`, `stats`, `socket_stats` and
    /// `last_seen_micros` raise `RuntimeError`. `cancel_handle`,
    /// `is_alive`, `end_reason`, `end_detail` and other receivers work.
    ///
    /// Cost: each registered sink re-acquires the GIL once per packet
    /// inside the recv loop, so on high-bitrate streams a slow sink (or
    /// many sinks) throttles the receiver. Keep sink bodies cheap.
    ///
    /// Raises the transport's `CLOSED` kind if the receiver is already closed.
    fn add_byte_sink(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        self.refuse_reentry("add_byte_sink")?;
        // The closure runs inside `recv_event` (under `detach`),
        // re-acquires the GIL per packet, and itself only touches
        // `callback` + `sink_error`, never the slot the parked `__next__`
        // holds. The callback may call back into this receiver; `IN_SINK`
        // marks that so the slot-taking methods refuse instead of
        // re-locking the slot on this thread.
        let sink_error = self.sink_error.clone();
        let key = self.sink_key();
        // Take the slot with the GIL RELEASED (matching `close()` /
        // `socket_stats()`). If we held the GIL here while a concurrent
        // `__next__` held the slot inside `recv_event`, a sink firing on
        // the recv thread would block re-acquiring the GIL while we
        // block on the slot — a deadlock. Registering the sink (a
        // Vec push) needs no GIL and never re-enters Python, so it is
        // safe to do inside the released-GIL block.
        let res = crate::util::allow_threads_parking(py, move || {
            self.owned.with_mut(|rx| {
                rx.add_byte_sink(Box::new(move |pkt: &[u8]| {
                    // The interpreter is finalising: re-taking the GIL now
                    // would abort the process. Drop the packet instead.
                    if crate::util::exiting() {
                        return;
                    }
                    Python::attach(|py| {
                        let b = PyBytes::new(py, pkt);
                        let _in_sink = InSinkGuard::enter(key);
                        if let Err(e) = callback.call1(py, (b,)) {
                            // First error wins; later packet errors are dropped.
                            if let Ok(mut slot) = sink_error.lock()
                                && slot.is_none()
                            {
                                *slot = Some(e);
                            }
                        }
                    });
                }));
            })
        });
        pyok(py, &self.owned.snapshot().domain, res)
    }

    /// Block until the next `DemuxEvent` is available. Returns a
    /// `tstrans.mpegts.DemuxEvent.*` subclass instance.
    ///
    /// Raises `StopIteration` on clean EOF (transport closed cleanly,
    /// demuxer drained); the transport's error class on transport-side failure;
    /// `DemuxError` on demuxer-side failure (strict-mode rejection,
    /// malformed PMT/PES); or any exception raised by a registered
    /// byte sink (see `add_byte_sink`), re-raised fail-loud.
    fn __next__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.refuse_reentry("__next__")?;
        // A `close()` from this receiver's own sink during the previous
        // pull, which delivered its event: finish the close now.
        if self.close_pending.swap(false, Ordering::AcqRel) {
            return self.finish_deferred_close(py);
        }
        let res =
            crate::util::allow_threads_parking(py, || self.owned.with_mut(|rx| rx.recv_event()));
        // Fail-loud: surface any sink exception captured during this
        // `recv_event` (the slot guard has been dropped above, so touching
        // `sink_error` here can't nest under it). Take it so a resumed
        // iteration after a caught error isn't permanently poisoned.
        if let Ok(mut slot) = self.sink_error.lock()
            && let Some(err) = slot.take()
        {
            return Err(err);
        }
        // A `close()` from this receiver's own sink only cancelled. An
        // event this pull produced is still delivered and the close waits
        // for the next entry; otherwise finish it now.
        if self.close_pending.load(Ordering::Acquire) {
            if let Ok(Ok(Some(ev))) = &res {
                return crate::mpegts::convert_event(py, ev);
            }
            self.close_pending.store(false, Ordering::Release);
            return self.finish_deferred_close(py);
        }
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
        Py::new(py, PyCancelHandle::from_source(&self.cancel))
    }

    /// Snapshot of the scheme-neutral 16-field wire stats.
    fn socket_stats(&self, py: Python<'_>) -> PyResult<Py<SocketStats>> {
        self.refuse_reentry("socket_stats")?;
        // Release the GIL before taking the inner lock. A registered byte
        // sink fires `Python::attach` inside `recv_event` while holding
        // this same lock; if we held the GIL here we would deadlock
        // (ABBA: iterator holds lock + blocks on GIL; this call holds GIL
        // + blocks on lock). Matches the `close()` / `add_byte_sink()`
        // pattern in this file.
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

    /// Tuple of `(SocketStats, MuxerStats)`. Same shape as
    /// `MuxSender.stats()`, so callers can read the same
    /// `(SocketStats, MuxerStats)` tuple on both MuxSender and
    /// DemuxReceiver.
    ///
    /// Returns the transport's `CLOSED` kind if the receiver has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<(Py<SocketStats>, Py<MuxerStats>)> {
        self.refuse_reentry("stats")?;
        // Release the GIL before taking the inner lock — same rationale
        // as `socket_stats` above (GIL↔mutex ABBA deadlock with byte
        // sinks). Extract plain Rust values under the lock, then build
        // Python objects after the guard is dropped and the GIL is
        // reacquired.
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
                    let mux_stats = tst_core::mpegts::mux::MuxerStats {
                        ts_packets_emitted: combined.packets_received,
                        ts_bytes_emitted: combined.bytes_received,
                        programs_configured: u32::try_from(combined.program_maps_seen)
                            .unwrap_or(u32::MAX),
                        subtitle_streams_configured: 0,
                        per_stream: combined.per_stream,
                    };
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
        self.refuse_reentry("last_seen_micros")?;
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
    ///
    /// From this receiver's own byte sink, the slot is held by the
    /// `__next__` the sink runs under: the close cancels and returns; that
    /// `__next__` still delivers an event it pulled, and the close
    /// completes with `StopIteration` on the following pull. A plain
    /// close supersedes a deferred one.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        if self.in_own_sink() {
            // The cancel can reach native teardown; release the GIL for it.
            let cancel = &*self.cancel;
            crate::util::allow_threads_parking(py, || {
                tst_core::transport::TransportCancel::cancel(cancel)
            });
            self.close_pending.store(true, Ordering::Release);
            return Ok(());
        }
        // Done here and now: a later `__next__` must report the closed
        // receiver, not end the iteration as the deferred close would.
        self.close_pending.store(false, Ordering::Release);
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
