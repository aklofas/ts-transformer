//! Python bindings for tst-rist (`tstrans.rist`). Gated on `feature = "rist"`.
//!
//! Provides:
//! - `Transport` / `RecvTransport` — send/recv RIST datagrams.
//! - `TransportBuilder` / `RecvTransportBuilder` — fluent builders.
//! - `EncryptionKey` — AES-128/192/256 PSK with SecretString discipline
//!   (secret never exposed through __repr__ or any getter).
//! - `RistProfile` — SIMPLE / MAIN (SCREAMING_SNAKE variant names per the
//!   tst-py convention; cf. UdpSocketKind, HlsMode).
//! - `RistStats` — frozen stats projection (8 fields from tst_rist::RistStats).
//!
//! GIL boundaries:
//! - `send`, `recv`, builder `build` / `connect`, `close`, `stats` — the
//!   slot lock is taken inside `py.allow_threads(...)`, so concurrent
//!   Python threads keep running during network I/O and while a getter
//!   waits for a parked call.
//!
//! Cross-thread cancel/close: both `tst_rist` transports expose a real
//! cancel handle (Arc 2 WP-D) and the shell's `CancelSource` forwards into
//! it, so `close()` cancels first and `Transport.cancel_handle()` /
//! `RecvTransport.cancel_handle()` hand the same shared state to Python as
//! `rist.CancelHandle`. `RecvTransport.recv()` polls librist in 100 ms
//! windows and re-checks the latch between windows, so a parked `recv()`
//! ends with `RistError(CLOSED)` ("cancelled from another thread") within
//! about one window. Every class holds an `Owned<T, S>` and borrows
//! `&self`. A cancel is NOT a close: the object stays open until
//! `close()`, which is quiet afterwards.
//!
//! Error mapping: every failure is a `tst_pipeline::binding::BindingError`
//! raised on `RistError` through `crate::raise` — the kind's `name()` is
//! resolved on `tstrans.exceptions.RistErrorKind` and checked at
//! `import tstrans`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_pipeline::binding::{BindingError, Owned, SendHalf};
use tst_rist::config::{EncryptionKey, RistProfile};
use tst_rist::recv::RistRecvTransport;
use tst_rist::transport::RistTransport;
use tst_rist::{RistRecvTransportBuilder, RistTransportBuilder};

use crate::raise::{RIST, pyok, pyres, raise};
use crate::util::{CancelSource, close_owned};

// ---------------------------------------------------------------------------
// PyRistStats — frozen mirror of tst_rist::RistStats
// ---------------------------------------------------------------------------

/// Cumulative stats snapshot for a RIST transport handle.
///
/// Returned by `Transport.stats()` and `RecvTransport.stats()`. Send-side
/// counters are zero on a receive-only handle and vice-versa.
///
/// `rtt_us` is a smoothed RTT in microseconds (0 until the first RTCP
/// exchange). `packets_missing` and `recovered_packets` reflect sequence-gap
/// tracking; the current librist polling path populates them as zero pending
/// the stats-callback integration.
#[pyclass(frozen, get_all, name = "RistStats", module = "tstrans.rist")]
#[derive(Clone)]
pub(crate) struct PyRistStats {
    /// Payload packets successfully sent (sender only).
    pub packets_sent: u64,
    /// Packets retransmitted due to ARQ (sender only).
    pub packets_retransmitted: u64,
    /// Packets dropped before transmission (sender only).
    pub packets_dropped: u64,
    /// Packets successfully received (receiver only).
    pub packets_received: u64,
    /// Sequence-number gaps seen (receiver only; 0 in this release).
    pub packets_missing: u64,
    /// Packets recovered via ARQ (receiver only; 0 in this release).
    pub recovered_packets: u64,
    /// Smoothed link bandwidth, kbps.
    pub current_bandwidth_kbps: u64,
    /// Smoothed RTT, microseconds (0 until first RTCP round-trip).
    pub rtt_us: u64,
}

impl From<tst_rist::stats::RistStats> for PyRistStats {
    fn from(s: tst_rist::stats::RistStats) -> Self {
        // `packets_missing` and `recovered_packets` are not tracked in the
        // current tst-rist simple polling path (librist exposes them via the
        // stats callback, not inline). Map available fields; others stay 0.
        Self {
            packets_sent: s.packets_sent,
            packets_retransmitted: s.packets_retransmitted,
            packets_dropped: s.packets_dropped,
            packets_received: s.packets_received,
            packets_missing: 0,
            recovered_packets: 0,
            current_bandwidth_kbps: s.bandwidth_kbps as u64,
            rtt_us: s.rtt_us as u64,
        }
    }
}

#[pymethods]
impl PyRistStats {
    fn __repr__(&self) -> String {
        format!(
            "RistStats(packets_sent={}, packets_received={}, \
             rtt_us={}, current_bandwidth_kbps={})",
            self.packets_sent, self.packets_received, self.rtt_us, self.current_bandwidth_kbps,
        )
    }
}

// ---------------------------------------------------------------------------
// PyEncryptionKey — AES PSK with SecretString discipline
// ---------------------------------------------------------------------------

/// AES pre-shared key for RIST encryption.
///
/// Construct via `EncryptionKey.aes128(secret)`, `.aes192(secret)`, or
/// `.aes256(secret)`. The `secret` argument may be `bytes` or `str`.
///
/// The secret is consumed at the FFI boundary and **never** exposed again —
/// `repr(key)` shows only the key size, not the secret bytes. There is no
/// getter that returns the secret.
///
/// To enable encryption on a sender or receiver, pass the key to the builder's
/// `.encryption(key)` method.
#[pyclass(frozen, name = "EncryptionKey", module = "tstrans.rist")]
pub(crate) struct PyEncryptionKey {
    inner: EncryptionKey,
    aes_bits: u32,
}

#[pymethods]
impl PyEncryptionKey {
    /// AES-128 PSK. `secret` may be `bytes` or `str`.
    #[staticmethod]
    fn aes128(secret: &Bound<'_, PyAny>) -> PyResult<Self> {
        let s = extract_secret_string(secret)?;
        Ok(Self {
            inner: EncryptionKey::aes128(s),
            aes_bits: 128,
        })
    }

    /// AES-192 PSK. `secret` may be `bytes` or `str`.
    #[staticmethod]
    fn aes192(secret: &Bound<'_, PyAny>) -> PyResult<Self> {
        let s = extract_secret_string(secret)?;
        Ok(Self {
            inner: EncryptionKey::aes192(s),
            aes_bits: 192,
        })
    }

    /// AES-256 PSK. `secret` may be `bytes` or `str`.
    #[staticmethod]
    fn aes256(secret: &Bound<'_, PyAny>) -> PyResult<Self> {
        let s = extract_secret_string(secret)?;
        Ok(Self {
            inner: EncryptionKey::aes256(s),
            aes_bits: 256,
        })
    }

    /// Redacts the secret — safe to log or print.
    fn __repr__(&self) -> String {
        format!("EncryptionKey(aes{}-bit, [redacted])", self.aes_bits)
    }
}

/// Extract a secret as a `String` from a Python `bytes` or `str` argument.
///
/// The extracted `String` is passed immediately to the Rust `EncryptionKey`
/// constructor and then dropped — it lives on the stack for the duration of
/// this call only.
fn extract_secret_string(secret: &Bound<'_, PyAny>) -> PyResult<String> {
    // Fast path: str.
    if let Ok(s) = secret.extract::<String>() {
        return Ok(s);
    }
    // Bytes path: raw bytes treated as UTF-8 (lossy).
    let bytes: &[u8] = secret.extract()?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

// ---------------------------------------------------------------------------
// PyRistProfile — RIST protocol profile
// ---------------------------------------------------------------------------

/// RIST transport profile.
///
/// - `SIMPLE` — VSF TR-06-1: basic ARQ + multiplexing.
/// - `MAIN` — VSF TR-06-2: adds encryption, RTCP, tunneling.
///
/// The default profile for new builders is `MAIN`. Setting `.encryption(key)`
/// on a builder forces the profile to `MAIN` regardless of this setting.
#[pyclass(eq, hash, frozen, name = "RistProfile", module = "tstrans.rist")]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::upper_case_acronyms)]
pub(crate) enum PyRistProfile {
    SIMPLE = 0,
    MAIN = 1,
}

impl From<PyRistProfile> for RistProfile {
    fn from(p: PyRistProfile) -> Self {
        match p {
            PyRistProfile::SIMPLE => RistProfile::Simple,
            PyRistProfile::MAIN => RistProfile::Main,
        }
    }
}

// ---------------------------------------------------------------------------
// PyRistTransport — wraps tst_rist::RistTransport
// ---------------------------------------------------------------------------

/// Python-side cancel handle. Wraps the shell's shared
/// [`crate::util::CancelSource`], which forwards into the transport's real
/// `RistCancelHandle`: every handle obtained from the same shell — and the
/// shell's own `close()` — flips one flag, so `is_cancelled()` reports the
/// shell's state, not this wrapper's history.
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.rist")]
pub(crate) struct PyRistCancelHandle {
    src: Arc<CancelSource>,
}

#[pymethods]
impl PyRistCancelHandle {
    /// Signal cancellation. Idempotent — repeated calls are a no-op.
    /// A `RecvTransport.recv()` parked on librist's poll raises
    /// `RistError(CLOSED)` (detail "cancelled from another thread") within
    /// about one 100 ms window, and every later `send()` / `recv()` on the
    /// originating object raises the same. The object itself is NOT
    /// closed — call `close()` (quiet after a cancel) to release it.
    fn cancel(&self) {
        tst_core::transport::TransportCancel::cancel(&*self.src);
    }

    /// `True` once the shell was cancelled or closed through ANY handle
    /// or its own `close()` (shared state).
    fn is_cancelled(&self) -> bool {
        self.src.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("CancelHandle(cancelled={})", self.is_cancelled())
    }
}

impl PyRistCancelHandle {
    /// The single constructor both rist transport classes use.
    pub(crate) fn from_source(src: &Arc<CancelSource>) -> Self {
        Self {
            src: Arc::clone(src),
        }
    }
}

/// RIST sender — wraps `tst_rist::RistTransport`.
///
/// Construct via `Transport.builder().url("rist://host:port").build()`.
///
/// GIL is released during `send` so other Python threads remain live while
/// the kernel socket call blocks.
#[pyclass(name = "Transport", module = "tstrans.rist")]
pub(crate) struct PyRistTransport {
    /// The binding layer's handle state machine (Arc 2): a `send` in
    /// flight on another thread holds the slot with the GIL released and
    /// `close()` cancels-then-takes, so the next `send` raises
    /// `RistError(CLOSED)` rather than `RuntimeError: Already borrowed`.
    /// Snapshot =
    /// `peer_url()`, so `repr()` never waits behind a send.
    owned: Owned<SendHalf<RistTransport>, String>,
    /// Shared cancel state, wrapping the transport's real `RistCancelHandle`
    /// (Arc 2 WP-D). Held beside the slot so `cancel_handle()` never waits
    /// behind an in-flight `send` (the PR #189 lease-bug class).
    cancel: Arc<CancelSource>,
}

#[pymethods]
impl PyRistTransport {
    /// Obtain a cross-thread cancel handle. Lock-free: never waits behind
    /// an in-flight `send()` (the handle lives outside the slot).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyRistCancelHandle>> {
        Py::new(py, PyRistCancelHandle::from_source(&self.cancel))
    }

    /// Return a builder for configuring and constructing a `Transport`.
    #[staticmethod]
    fn builder() -> PyRistTransportBuilder {
        PyRistTransportBuilder::default()
    }

    /// Send one payload. Accepts any bytes-like object: `bytes`, `bytearray`,
    /// `memoryview`, or any buffer-protocol object.
    ///
    /// Raises `RistError(kind=TOO_LARGE)` if `len(payload)` exceeds
    /// the configured `pkt_size` (default 1316 bytes / 7 TS packets).
    ///
    /// Releases the GIL during the underlying socket send call.
    fn send(&self, py: Python<'_>, payload: &Bound<'_, PyAny>) -> PyResult<()> {
        // Zero-copy for `bytes`; one C copy through `bytes()` otherwise
        // (PyBuffer is unavailable under abi3-py310).
        let coerced = crate::util::coerce_bytes_like(py, payload)?;
        let slice: &[u8] = coerced.as_bytes();
        pyres(
            py,
            &RIST,
            py.allow_threads(|| self.owned.with_mut(|t| t.0.send_bytes(slice))),
        )
    }

    /// Close the sender. Idempotent and safe from any thread — further
    /// `.send()` calls raise `RistError(kind=CLOSED)`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &RIST, &self.owned)
    }

    /// Snapshot of cumulative wire-level statistics.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyRistStats>> {
        let s = pyok(
            py,
            &RIST,
            py.allow_threads(|| self.owned.with_ref(|t| t.0.stats())),
        )?;
        Py::new(py, PyRistStats::from(s))
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
            "Transport(closed)".to_string()
        } else {
            format!("Transport(peer={:?})", self.owned.snapshot())
        }
    }
}

// ---------------------------------------------------------------------------
// PyRistTransportBuilder — builder for PyRistTransport
// ---------------------------------------------------------------------------

/// Builder for `Transport`. Chain setter calls, then call `.build()`.
///
/// Example:
/// ```python
/// tx = rist.Transport.builder() \
///     .url("rist://127.0.0.1:8000") \
///     .profile(rist.RistProfile.SIMPLE) \
///     .buffer_ms(200) \
///     .build()
/// ```
#[pyclass(name = "TransportBuilder", module = "tstrans.rist")]
#[derive(Default)]
pub(crate) struct PyRistTransportBuilder {
    url: Option<String>,
    profile: Option<PyRistProfile>,
    bandwidth_kbps: Option<u32>,
    buffer_ms: Option<u64>,
    cname: Option<String>,
    encryption: Option<PyObject>,
    recovery_maxbitrate_kbps: Option<u32>,
    pkt_size: Option<usize>,
    compression: Option<bool>,
}

#[pymethods]
impl PyRistTransportBuilder {
    /// Set the destination URL. Required. Must be `rist://host:port`.
    fn url<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.url = Some(s.to_string());
        slf
    }

    /// Override the RIST profile (`RistProfile.SIMPLE` or `RistProfile.MAIN`).
    fn profile(mut slf: PyRefMut<'_, Self>, p: PyRistProfile) -> PyRefMut<'_, Self> {
        slf.profile = Some(p);
        slf
    }

    /// Sender bandwidth cap, kbps.
    fn bandwidth_kbps(mut slf: PyRefMut<'_, Self>, v: u32) -> PyRefMut<'_, Self> {
        slf.bandwidth_kbps = Some(v);
        slf
    }

    /// Recovery buffer duration, milliseconds.
    fn buffer_ms(mut slf: PyRefMut<'_, Self>, ms: u64) -> PyRefMut<'_, Self> {
        slf.buffer_ms = Some(ms);
        slf
    }

    /// RTCP CNAME for this sender.
    fn cname<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.cname = Some(s.to_string());
        slf
    }

    /// AES encryption key. Forces profile to `MAIN`.
    fn encryption(mut slf: PyRefMut<'_, Self>, key: Py<PyEncryptionKey>) -> PyRefMut<'_, Self> {
        slf.encryption = Some(key.into_any());
        slf
    }

    /// Retransmit bandwidth cap, kbps.
    fn recovery_maxbitrate_kbps(mut slf: PyRefMut<'_, Self>, v: u32) -> PyRefMut<'_, Self> {
        slf.recovery_maxbitrate_kbps = Some(v);
        slf
    }

    /// Per-send-call payload cap in bytes (default 1316 = 7 × 188 TS bytes).
    fn pkt_size(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.pkt_size = Some(v);
        slf
    }

    /// Enable NULL-packet deletion / compression.
    fn compression(mut slf: PyRefMut<'_, Self>, v: bool) -> PyRefMut<'_, Self> {
        slf.compression = Some(v);
        slf
    }

    /// Build the `Transport`. Raises `RistError(kind=URL)` for a bad URL,
    /// `RistError(kind=CONTEXT_CREATE_FAILED)` / `PEER_CREATE_FAILED` for
    /// librist session failures, `RistError(kind=ENCRYPTION_DISABLED)` if
    /// encryption is requested but the `mbedtls` feature is disabled.
    fn build(&self, py: Python<'_>) -> PyResult<PyRistTransport> {
        let url_str = self.url.as_deref().ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("url(...) is required before build()")
        })?;
        // `RistTransportBuilder::new` parses the URL and seeds the config from
        // URL params (via RistConfig::merge_from_url).
        let mut b = RistTransportBuilder::new(url_str)
            .map_err(|e| raise(py, &RIST, BindingError::from(e)))?;
        if let Some(p) = self.profile {
            b = b.profile(p.into());
        }
        if let Some(kbps) = self.bandwidth_kbps {
            b = b.bandwidth_kbps(kbps);
        }
        if let Some(ms) = self.buffer_ms {
            b = b.buffer(std::time::Duration::from_millis(ms));
        }
        if let Some(ref cname) = self.cname {
            b = b.cname(cname.as_str());
        }
        if let Some(ref key_obj) = self.encryption {
            let key_ref = key_obj.bind(py).downcast::<PyEncryptionKey>()?.clone();
            let inner_key = key_ref.borrow().inner.clone();
            b = b.encryption(inner_key);
        }
        if let Some(kbps) = self.recovery_maxbitrate_kbps {
            b = b.recovery_maxbitrate_kbps(kbps);
        }
        if let Some(v) = self.pkt_size {
            b = b.pkt_size(v);
        }
        if let Some(v) = self.compression {
            b = b.compression(v);
        }
        let t = py
            .allow_threads(|| b.connect())
            .map_err(|e| raise(py, &RIST, BindingError::from(e)))?;
        let peer_url = t.peer_url().to_owned();
        // Obtain-before-move: the transport's real cancel handle, captured
        // before `SendHalf` takes ownership. `CancelSource` latches its own
        // flag and forwards into it, so `close()` and `cancel_handle()`
        // drive the same state.
        let cancel = CancelSource::new(Arc::new(t.cancel_handle()));
        Ok(PyRistTransport {
            owned: Owned::new(SendHalf(t), cancel.as_dyn(), peer_url),
            cancel,
        })
    }

    fn __repr__(&self) -> String {
        format!("TransportBuilder(url={:?})", self.url)
    }
}

// ---------------------------------------------------------------------------
// PyRistRecvTransport — wraps tst_rist::RistRecvTransport
// ---------------------------------------------------------------------------

/// Transport + reusable scratch buffer under one lock.
struct RistRecvInner {
    transport: RistRecvTransport,
    scratch: Vec<u8>,
}

impl tst_pipeline::binding::Close for RistRecvInner {
    type Error = core::convert::Infallible;

    fn close(&mut self) -> Result<(), Self::Error> {
        RecvTransport::close(&mut self.transport);
        Ok(())
    }
}

/// RIST receiver — wraps `tst_rist::RistRecvTransport`.
///
/// Construct via `RecvTransport.builder().bind_url("rist://@0.0.0.0:8000").build()`.
/// The bind URL must include the `@` prefix per librist convention
/// (`rist://@host:port`).
///
/// GIL is released during `recv` so other Python threads remain live while
/// waiting for data.
///
/// Note: librist Simple profile requires even port numbers. Use `?buffer=NNN`
/// in the URL to set the recovery buffer size (milliseconds).
#[pyclass(name = "RecvTransport", module = "tstrans.rist")]
pub(crate) struct PyRistRecvTransport {
    /// The binding layer's handle state machine (Arc 2): a parked `recv`
    /// holds the slot with the GIL released, and `close()` latches the
    /// cancel BEFORE taking it, so the parked recv ends with
    /// `RistError(CLOSED)` within one librist poll window. Snapshot =
    /// `bind_url()`, so `repr()` never waits behind a parked `recv`.
    owned: Owned<RistRecvInner, String>,
    /// Shared cancel state, wrapping the transport's real `RistCancelHandle`
    /// (Arc 2 WP-D). `close()` and `cancel_handle().cancel()` both latch it
    /// and the poll loop checks it between librist's 100 ms windows.
    cancel: Arc<CancelSource>,
}

#[pymethods]
impl PyRistRecvTransport {
    /// Obtain a cross-thread cancel handle. Lock-free: never waits behind
    /// a parked `recv()` (the handle lives outside the slot).
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<Py<PyRistCancelHandle>> {
        Py::new(py, PyRistCancelHandle::from_source(&self.cancel))
    }

    /// Return a builder for configuring and constructing a `RecvTransport`.
    #[staticmethod]
    fn builder() -> PyRistRecvTransportBuilder {
        PyRistRecvTransportBuilder::default()
    }

    /// Receive one payload from the RIST session.
    ///
    /// `timeout_ms`: milliseconds to wait before raising
    /// `RistError(kind=BACKPRESSURE)` — retryable. `None` (default) blocks until a
    /// packet arrives.
    ///
    /// Implementation note: the underlying `recv_bytes` polls with a 100 ms
    /// window and returns `Backpressure` when no data arrived. This method
    /// retries on `Backpressure` until the deadline passes. Actual timeout
    /// latency may exceed `timeout_ms` by up to ~100 ms.
    ///
    /// Cross-thread `close()` is supported: it sets a stop flag this loop
    /// checks between windows, so a parked `recv()` ends with
    /// `RistError(kind=CLOSED)` within about 100 ms.
    ///
    /// Releases the GIL while waiting on the kernel.
    #[pyo3(signature = (timeout_ms = None))]
    fn recv(&self, py: Python<'_>, timeout_ms: Option<u64>) -> PyResult<Py<PyBytes>> {
        let cancel = Arc::clone(&self.cancel);
        let deadline =
            timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
        let res = py.allow_threads(|| {
            self.owned.with_mut(move |s| {
                loop {
                    if cancel.is_cancelled() {
                        return Err(TransportError::ExplicitClose);
                    }
                    match s.transport.recv_bytes(&mut s.scratch) {
                        Ok(n) => return Ok(s.scratch[..n].to_vec()),
                        Err(TransportError::Backpressure { .. }) => {
                            // 100 ms poll window expired with no data.
                            // Check deadline if one is set; otherwise retry.
                            if let Some(dl) = deadline {
                                if std::time::Instant::now() >= dl {
                                    return Err(TransportError::Backpressure {
                                        msg: "recv timed out".into(),
                                        errno_code: None,
                                    });
                                }
                            }
                            continue;
                        }
                        Err(other) => return Err(other),
                    }
                }
            })
        });
        let bytes = pyres(py, &RIST, res)?;
        Ok(PyBytes::new_bound(py, &bytes).unbind())
    }

    /// Close the receiver. Cancel-first (`Owned::close` latches the shared
    /// cancel before taking the slot), so a `recv()` parked on another
    /// thread ends with `RistError(kind=CLOSED)` within ~100 ms; further
    /// `.recv()` calls raise the same. Idempotent.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &RIST, &self.owned)
    }

    /// Snapshot of cumulative wire-level statistics.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyRistStats>> {
        let s = pyok(
            py,
            &RIST,
            py.allow_threads(|| self.owned.with_ref(|s| s.transport.stats())),
        )?;
        Py::new(py, PyRistStats::from(s))
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
            "RecvTransport(closed)".to_string()
        } else {
            format!("RecvTransport(bind={:?})", self.owned.snapshot())
        }
    }
}

// ---------------------------------------------------------------------------
// PyRistRecvTransportBuilder — builder for PyRistRecvTransport
// ---------------------------------------------------------------------------

/// Builder for `RecvTransport`. Chain setter calls, then call `.build()`.
///
/// Example:
/// ```python
/// rx = rist.RecvTransport.builder() \
///     .bind_url("rist://@0.0.0.0:8000") \
///     .buffer_ms(200) \
///     .build()
/// ```
#[pyclass(name = "RecvTransportBuilder", module = "tstrans.rist")]
#[derive(Default)]
pub(crate) struct PyRistRecvTransportBuilder {
    url: Option<String>,
    profile: Option<PyRistProfile>,
    buffer_ms: Option<u64>,
    cname: Option<String>,
    encryption: Option<PyObject>,
    session_timeout_ms: Option<u64>,
}

#[pymethods]
impl PyRistRecvTransportBuilder {
    /// Set the bind URL. Required. Must be `rist://@bind_addr:port`.
    /// The `@` prefix marks this as a receiver (listener) URL per the
    /// librist / ffmpeg convention.
    fn bind_url<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.url = Some(s.to_string());
        slf
    }

    /// Override the RIST profile.
    fn profile(mut slf: PyRefMut<'_, Self>, p: PyRistProfile) -> PyRefMut<'_, Self> {
        slf.profile = Some(p);
        slf
    }

    /// Recovery buffer duration, milliseconds.
    fn buffer_ms(mut slf: PyRefMut<'_, Self>, ms: u64) -> PyRefMut<'_, Self> {
        slf.buffer_ms = Some(ms);
        slf
    }

    /// RTCP CNAME for this receiver.
    fn cname<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.cname = Some(s.to_string());
        slf
    }

    /// AES decryption key. Forces profile to `MAIN`.
    fn encryption(mut slf: PyRefMut<'_, Self>, key: Py<PyEncryptionKey>) -> PyRefMut<'_, Self> {
        slf.encryption = Some(key.into_any());
        slf
    }

    /// Session timeout, milliseconds. Receiver disconnects after this many
    /// milliseconds with no sender traffic.
    fn session_timeout_ms(mut slf: PyRefMut<'_, Self>, ms: u64) -> PyRefMut<'_, Self> {
        slf.session_timeout_ms = Some(ms);
        slf
    }

    /// Build the `RecvTransport`. Raises `RistError(kind=URL)` for a bad bind
    /// URL, `RistError(kind=INVALID_CONFIG)` if the `@` prefix is missing,
    /// and `RistError(kind=CONTEXT_CREATE_FAILED)` / `PEER_CREATE_FAILED` for
    /// librist session failures.
    fn build(&self, py: Python<'_>) -> PyResult<PyRistRecvTransport> {
        let url_str = self.url.as_deref().ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("bind_url(...) is required before build()")
        })?;
        let mut b = RistRecvTransportBuilder::new(url_str)
            .map_err(|e| raise(py, &RIST, BindingError::from(e)))?;
        if let Some(p) = self.profile {
            b = b.profile(p.into());
        }
        if let Some(ms) = self.buffer_ms {
            b = b.buffer(std::time::Duration::from_millis(ms));
        }
        if let Some(ref cname) = self.cname {
            b = b.cname(cname.as_str());
        }
        if let Some(ref key_obj) = self.encryption {
            let key_ref = key_obj.bind(py).downcast::<PyEncryptionKey>()?.clone();
            let inner_key = key_ref.borrow().inner.clone();
            b = b.encryption(inner_key);
        }
        if let Some(ms) = self.session_timeout_ms {
            b = b.session_timeout(std::time::Duration::from_millis(ms));
        }
        let t = py
            .allow_threads(|| b.listen())
            .map_err(|e| raise(py, &RIST, BindingError::from(e)))?;
        let scratch_len = t.max_payload().max(65_536);
        let bind_url = t.bind_url().to_owned();
        // Obtain-before-move (see the sender twin).
        let cancel = CancelSource::new(Arc::new(t.cancel_handle()));
        Ok(PyRistRecvTransport {
            owned: Owned::new(
                RistRecvInner {
                    transport: t,
                    scratch: vec![0u8; scratch_len],
                },
                cancel.as_dyn(),
                bind_url,
            ),
            cancel,
        })
    }

    fn __repr__(&self) -> String {
        format!("RecvTransportBuilder(url={:?})", self.url)
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "rist")?;
    m.add_class::<PyRistCancelHandle>()?;
    m.add_class::<PyRistProfile>()?;
    m.add_class::<PyRistStats>()?;
    m.add_class::<PyEncryptionKey>()?;
    m.add_class::<PyRistTransport>()?;
    m.add_class::<PyRistTransportBuilder>()?;
    m.add_class::<PyRistRecvTransport>()?;
    m.add_class::<PyRistRecvTransportBuilder>()?;
    parent.add_submodule(&m)?;
    Ok(())
}
