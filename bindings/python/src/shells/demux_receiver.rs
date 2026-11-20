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
//!   `Owned`, so `end_reason()` keeps answering after `close()`; so do
//!   `peer_addr()` and `source_rejected()`, which read the transport's
//!   source record from the snapshot, and `rtp_stats()`, which reads the
//!   RTP counters' handle taken before the transport was boxed.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::{Arc, Mutex};

use pyo3::Py;
use pyo3::prelude::*;
use pyo3::types::PyType;

use tst_core::transport::{RecvTransport, SourceHandle, TransportCancel};
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, HandleState, Opened, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
    TsBytesDemuxedHandle,
};

use crate::generated::stats_shells::{DemuxReceiverStats, SocketStats};
use crate::mpegts::demux_error_to_pyerr;
use crate::raise::{pyok, raise};
use crate::shells::cancel::{PyCancelHandle, cancel_source};
use crate::shells::end_reason::{end_detail, end_reason_to_py};
use crate::shells::open::{OpenedRecv, RtpStatsSource};
use crate::shells::open_cancel::{PyOpenCancel, object_of};
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

/// The shell's construction-time handles, read without the slot.
struct Handles {
    ts_bytes: TsBytesDemuxedHandle,
    /// The transport's source record; `None` for a transport that keeps none.
    source: Option<SourceHandle>,
    /// The RTP counters; `None` off RTP.
    #[cfg_attr(not(feature = "rtp"), allow(dead_code))]
    rtp_stats: RtpStatsSource,
    /// The address the open bound; `None` for a transport whose opener
    /// records none.
    local_addr: Option<std::net::SocketAddr>,
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
    owned: Owned<RustDemuxReceiver<Box<dyn RecvTransport>>, Snapshot<Handles>>,
    /// Shared cancel state — see `crate::util::CancelSource`.
    cancel: Arc<CancelSource>,
    /// The application-cancel latch of the publish mount this receiver was
    /// taken from; `None` on every other receiver. The shared binding layer
    /// reads only a handle's own latch, so this layer carries the mount's
    /// cancel across: a mount cancel must beat events already queued, like
    /// the receiver's own cancel.
    mount_cancel: Option<Arc<dyn TransportCancel>>,
    /// What the transport needs alive besides itself: the client of the RTSP
    /// session this receiver was taken from, so collecting the session does
    /// not end the stream. Released after the transport closes, at `close()`
    /// or collection; `None` on every other receiver.
    keep_alive: Mutex<Option<KeepAlive>>,
}

/// Whether a call on another thread holds the slot. Test-only.
#[cfg(feature = "py-test-hooks")]
impl PyDemuxReceiver {
    pub(crate) fn slot_held(&self) -> bool {
        self.owned.is_held()
    }
}

/// An owner a receiver keeps alive until it closes.
pub(crate) type KeepAlive = Arc<dyn std::any::Any + Send + Sync>;

impl Drop for PyDemuxReceiver {
    fn drop(&mut self) {
        drop_owned(&self.owned.snapshot().domain, &self.owned);
        let held = self
            .keep_alive
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if held.is_some() {
            crate::util::drop_blocking(move || drop(held));
        }
    }
}

impl PyDemuxReceiver {
    /// The address the open bound, or the transport's `CLOSED` once closed.
    fn bound(&self, py: Python<'_>) -> PyResult<Option<std::net::SocketAddr>> {
        if self.owned.is_closed() {
            Err(raise(
                py,
                &self.owned.snapshot().domain,
                BindingError::from(HandleState::Closed),
            ))
        } else {
            Ok(self.owned.snapshot().handles.local_addr)
        }
    }

    /// When `pid` last carried an item, or `None`; the transport's `CLOSED`
    /// once closed. Waits behind a parked `__next__`, as `stats()`.
    fn last_seen_instant(
        &self,
        py: Python<'_>,
        pid: u16,
    ) -> PyResult<Option<std::time::SystemTime>> {
        let raw = crate::util::allow_threads_parking(py, || {
            self.owned
                // per-PID timestamp lives in the demuxer stats inside the shell; blocking caveat documented with the watchdog recipe
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |rx| {
                    rx.stats().per_stream.get(&pid).and_then(|s| s.last_seen)
                })
        });
        pyok(py, &self.owned.snapshot().domain, raw)
    }

    /// Wrap an already-opened transport (the `open`,
    /// `Socket.into_demux_receiver()` and RTSP `into_demux_receiver()`
    /// paths meet here).
    // Only the srt and tcp conversions open a shell without RTP counters.
    #[cfg_attr(not(any(feature = "srt", feature = "tcp")), allow(dead_code))]
    pub(crate) fn from_opened(
        py: Python<'_>,
        opened: Opened<Box<dyn RecvTransport>>,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> PyResult<Self> {
        Self::from_recv(py, OpenedRecv::plain(opened), opts)
    }

    /// [`Self::from_opened`] with the RTP counters of an `rtp://` transport.
    pub(crate) fn from_recv(
        _py: Python<'_>,
        opened: OpenedRecv,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> PyResult<Self> {
        let OpenedRecv { opened, rtp_stats } = opened;
        let Opened {
            transport,
            cancel,
            domain,
            local_addr,
            ..
        } = opened;
        let cancel = cancel_source(&cancel);
        // Taken before the transport moves into the shell.
        let end_reason = transport.end_reason_handle();
        let source = transport.source_handle();
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
                handles: Handles {
                    ts_bytes,
                    source,
                    rtp_stats,
                    local_addr,
                },
            },
        );
        if let Some(h) = end_reason {
            owned = owned.with_end_reason(h);
        }
        Ok(Self {
            owned,
            cancel,
            mount_cancel: None,
            keep_alive: Mutex::new(None),
        })
    }

    /// Keep `owner` alive until this receiver closes or is collected.
    #[cfg(feature = "rtp")]
    pub(crate) fn keeping_alive(mut self, owner: KeepAlive) -> Self {
        *self.keep_alive.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(owner);
        self
    }

    /// [`Self::from_opened`] for the transport of a publish mount, whose
    /// cancel latch the mount's `cancel()` sets.
    #[cfg(feature = "rtp")]
    pub(crate) fn from_publish_mount(
        py: Python<'_>,
        opened: OpenedRecv,
        opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    ) -> PyResult<Self> {
        let mount_cancel = Arc::clone(&opened.opened.cancel);
        let mut rx = Self::from_recv(py, opened, opts)?;
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

    /// The one open path: translate the config dataclass with the GIL held
    /// (after the cancelled-object check, before the URL), then open the
    /// transport.
    fn open_with(
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
        cancel: Option<&Bound<'_, PyOpenCancel>>,
    ) -> PyResult<Self> {
        let oc = object_of(cancel);
        let (opened, demux_opts) = crate::shells::open::open_recv(py, url, &oc, || {
            demux_config
                .map(|cfg_obj| crate::mpegts::build_demuxer_config(py, cfg_obj))
                .transpose()
        })?;
        Self::from_recv(py, opened, demux_opts)
    }
}

#[pymethods]
impl PyDemuxReceiver {
    /// Open a `DemuxReceiver` on `url` (GIL released). For `srt://` the URL
    /// must carry `?mode=listener`, and the open blocks in its first accept
    /// until a peer connects.
    ///
    /// `demux_config` is an optional `tstrans.mpegts.DemuxerConfig`
    /// dataclass; when `None`, defaults are used.
    ///
    /// `cancel`: no handle exists until this returns, so the first accept
    /// is cancelled through an `OpenCancel` made beforehand: its
    /// `cancel()` from another thread ends the accept with
    /// `SrtError(CLOSED)`, and an object already cancelled refuses the
    /// open with the transport's `CLOSED` before `demux_config` and the URL
    /// are looked at.
    ///
    /// Raises `ValueError` for an unsupported scheme, then the field error of
    /// a `demux_config` it cannot read, then the transport's error class for
    /// a bad URL. An `srt://` bind or accept fault raises
    /// `SrtError(kind=BROKEN)` with the message prefixed `bind: ` or
    /// `accept: `.
    #[classmethod]
    #[pyo3(signature = (url, demux_config = None, *, cancel = None))]
    fn open(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        url: &str,
        demux_config: Option<&Bound<'_, PyAny>>,
        cancel: Option<&Bound<'_, PyOpenCancel>>,
    ) -> PyResult<Self> {
        Self::open_with(py, url, demux_config, cancel)
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
        self.owned.snapshot().handles.ts_bytes.get()
    }

    /// The receive counters (`DemuxReceiverStats`): the packets the
    /// syncer handed out, the program maps and their versions, the
    /// discontinuities and the nonconformant items. Waits behind a parked
    /// `__next__`, as `socket_stats()` does; a record taken earlier keeps
    /// its values.
    ///
    /// Returns the transport's `CLOSED` kind if the receiver has been closed.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<DemuxReceiverStats>> {
        let raw = crate::util::allow_threads_parking(py, || {
            // counters live inside the shell and have no value to report while a parked call holds it
            self.owned
                .with_ref_blocking(BlockingReason::NoAnswerWithoutSlot, |rx| rx.stats())
        });
        let stats = pyok(py, &self.owned.snapshot().domain, raw)?;
        Py::new(py, DemuxReceiverStats::from(&stats))
    }

    /// The RTP counters (`tstrans.rtp.RtpStats`) of a receiver over
    /// `rtp://` or an RTSP session; `None` on any other transport. Never
    /// waits behind a parked `__next__`; the last value after `close()`.
    #[cfg(feature = "rtp")]
    fn rtp_stats(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Py<crate::generated::stats_rtp::RtpStats>>> {
        rtp_stats_of(py, &self.owned.snapshot().handles.rtp_stats)
    }

    /// Wall-clock time the stream identified by `pid` last carried an
    /// item through this receiver (last emitted event), as a UTC-aware
    /// `datetime.datetime`. `None` if `pid` was never seen, including an
    /// unrecognized PID (no range check beyond the native `u16`, mirror
    /// of `Muxer.stream_codec_stats`'s pid handling: unknown → `None`,
    /// no dedicated "bad pid" error).
    ///
    /// Raises the transport's `CLOSED` kind if the receiver has been closed — same
    /// lock discipline as `stats()`.
    fn last_seen(&self, py: Python<'_>, pid: u16) -> PyResult<Option<Py<PyAny>>> {
        self.last_seen_instant(py, pid)?
            .map(|t| crate::util::utc_datetime(py, t))
            .transpose()
    }

    /// `last_seen(pid)` as a Unix-epoch microsecond count. Deprecated:
    /// warns first; removed in 0.12.0.
    fn last_seen_micros(&self, py: Python<'_>, pid: u16) -> PyResult<Option<u64>> {
        PyErr::warn(
            py,
            &py.get_type::<pyo3::exceptions::PyDeprecationWarning>(),
            c"DemuxReceiver.last_seen_micros() is deprecated: use last_seen(); removed in 0.12.0",
            1,
        )?;
        Ok(self
            .last_seen_instant(py, pid)?
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

    /// `(host, port)` the open bound: a `udp://`, `rist://@` or `rtp://`
    /// bind (the kernel's port when the URL named 0), or an `srt://`
    /// listener once its caller connected; `None` for a transport whose
    /// opener records none. Never waits behind a parked `__next__`; raises
    /// the transport's `CLOSED` kind once the receiver is closed.
    fn local_addr(&self, py: Python<'_>) -> PyResult<Option<crate::util::PyAddr>> {
        Ok(self.bound(py)?.map(crate::util::addr_tuple))
    }

    /// The port of `local_addr()`, or `None` where that is `None`. Same
    /// rules as `local_addr()`.
    fn local_port(&self, py: Python<'_>) -> PyResult<Option<u16>> {
        Ok(self.bound(py)?.map(|a| a.port()))
    }

    /// `(host, port)` of the sender whose datagram (`udp://`) or block
    /// (`rist://`) was delivered last; `None` before the first one and for
    /// a transport that keeps no source record. Never waits behind a
    /// parked `__next__`; the last value after `close()`.
    fn peer_addr(&self) -> Option<crate::util::PyAddr> {
        crate::util::peer_addr_tuple(self.owned.snapshot().handles.source.as_ref())
    }

    /// Datagrams or blocks the `source` key of a `udp://` or `rist://` URL
    /// turned away; 0 for a transport that keeps no source record. Never
    /// waits behind a parked `__next__`; the last value after `close()`.
    fn source_rejected(&self) -> u64 {
        self.owned
            .snapshot()
            .handles
            .source
            .as_ref()
            .map_or(0, SourceHandle::rejected)
    }

    /// Close the receiver. Idempotent. Fires the cancel handle BEFORE
    /// acquiring the mutex so a concurrent `__next__` parked in
    /// `recv_event` unparks promptly.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let closed = close_owned(py, &self.owned.snapshot().domain, &self.owned);
        // After the transport: dropping the last holder of an RTSP client
        // writes its TEARDOWN and stops its thread.
        crate::util::allow_threads_parking(py, || {
            drop(
                self.keep_alive
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(),
            )
        });
        closed
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

/// A shell's `rtp_stats()` answer from its handle.
#[cfg(feature = "rtp")]
pub(crate) fn rtp_stats_of(
    py: Python<'_>,
    handle: &RtpStatsSource,
) -> PyResult<Option<Py<crate::generated::stats_rtp::RtpStats>>> {
    handle
        .as_ref()
        .map(|h| Py::new(py, crate::generated::stats_rtp::RtpStats::from(&h.stats())))
        .transpose()
}
