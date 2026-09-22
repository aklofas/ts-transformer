//! Python bindings for tst-tcp (`tstrans.tcp`). Gated on `feature = "tcp"`.
//!
//! Mirrors the udp/ module
//! structure but wraps a single dual-trait `TcpTransport` that implements
//! both `Transport` (sender) and `RecvTransport` (receiver).
//!
//! Key differences from udp/:
//! - ONE `Transport` PyClass covers both send and recv. The Rust
//!   `TcpTransport` is a bytestream handle — the caller decides whether to
//!   use it as a sender, a receiver, or both. The Python binding does NOT
//!   enforce mutual exclusion (Rust doesn't either).
//! - `Listener` wraps `tst_tcp::TcpListener`; `accept_blocking()` returns
//!   a `Transport`.
//! - TLS (`tcps://`) is compiled in when the tst-py `tls` feature is on
//!   (default; published wheels ship it). Callers verify against native
//!   trust roots or a custom CA via the `?ca=<pem path>` URL param;
//!   listeners serve TLS via `ListenerBuilder.tls(cert, key)`. A source
//!   build without `tls` raises `TcpError(kind=TLS_DISABLED)` at build()
//!   time. `TlsConfig` and `ClientCert` remain forward-compat dataclasses
//!   — mTLS client certs have no tst-tcp backend yet.
//!
//! GIL boundaries:
//! - `send`, `recv`, `build()` (both builders), `accept_blocking` ->
//!   `py.allow_threads(...)` so concurrent Python threads remain live.
//! - `close`, `stats`, `peer_addr`, `repr` -> also release the GIL during
//!   mutex acquisition; `close` fires the cancel handle first so a parked
//!   `recv` unblocks within ≤100 ms, making the lock promptly available.
//! - `Listener.close` / `local_port` / `repr` -> likewise release the GIL
//!   around the lock, and `Listener.close` fires the listener's own cancel
//!   handle first so a parked `accept_blocking` returns within ≤100 ms.
//!
//! Bytes-like extraction in `Transport.send(payload)` follows the abi3-py310
//! two-path pattern from udp/mod.rs: fast zero-copy `&[u8]` for `bytes`,
//! fallback through Python `bytes()` builtin for `bytearray`/`memoryview`.
//!
//! Error mapping: every failure is a `tst_pipeline::binding::BindingError`
//! raised on `TcpError` through `crate::raise` — the kind's `name()` is
//! resolved on `tstrans.exceptions.TcpErrorKind` and checked at
//! `import tstrans`.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyMemoryView};

use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::binding::{BindingError, BindingErrorKind, HandleState, Owned, SendHalf};
use tst_tcp::error::TcpError;
use tst_tcp::{TcpListener, TcpStats, TcpTransport};

use crate::raise::{TCP, pyok, pyres, raise};
use crate::util::{CancelSource, close_owned};

// ---------------------------------------------------------------------------
// PyTcpStats — frozen mirror of TcpStats
// ---------------------------------------------------------------------------

/// Cumulative stats snapshot for a TCP transport handle.
///
/// Returned by `Transport.stats()`. Both send and receive counters are
/// populated on the same handle (TCP is full-duplex).
#[pyclass(frozen, get_all, name = "SocketStats", module = "tstrans.tcp")]
pub(crate) struct PyTcpStats {
    /// Bytes successfully sent.
    pub bytes_sent: u64,
    /// Bytes successfully received.
    pub bytes_received: u64,
    /// Number of successful send calls.
    pub send_calls: u64,
    /// Number of successful recv calls.
    pub recv_calls: u64,
    /// Send-side I/O errors.
    pub send_errors: u64,
    /// Receive-side I/O errors.
    pub recv_errors: u64,
}

impl From<TcpStats> for PyTcpStats {
    fn from(s: TcpStats) -> Self {
        Self {
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
            send_calls: s.send_calls,
            recv_calls: s.recv_calls,
            send_errors: s.send_errors,
            recv_errors: s.recv_errors,
        }
    }
}

#[pymethods]
impl PyTcpStats {
    fn __repr__(&self) -> String {
        format!(
            "SocketStats(bytes_sent={}, bytes_received={}, \
             send_calls={}, recv_calls={})",
            self.bytes_sent, self.bytes_received, self.send_calls, self.recv_calls,
        )
    }
}

// ---------------------------------------------------------------------------
// PyTcpTransport -- wraps tst_tcp::TcpTransport (dual-trait send+recv)
// ---------------------------------------------------------------------------

/// TCP transport -- wraps `tst_tcp::TcpTransport`.
///
/// Implements BOTH the sender and receiver roles on a single handle.
/// The connection is established by the builder (`Transport.builder()`) or
/// returned by `Listener.accept_blocking()`. Which role the caller uses
/// (send vs recv vs both) is the caller's choice -- TCP is full-duplex.
///
/// Construct via:
/// ```python
/// transport = tcp.Transport.builder().url("tcp://host:port").build()
/// ```
///
/// GIL is released during `send`, `recv`, `stats`, `peer_addr`, `close`,
/// and `repr` so other Python threads remain live during I/O and mutex
/// acquisition. `close()` fires the cancel handle BEFORE acquiring the
/// inner mutex, so a thread parked in `recv()` unblocks promptly (within
/// ≤100 ms) and the lock becomes available without holding the GIL.
#[pyclass(name = "Transport", module = "tstrans.tcp")]
pub(crate) struct PyTcpTransport {
    /// The binding layer's handle state machine (Arc 2). The shell's
    /// `Arc<dyn TransportCancel>` is a `CancelSource` over the real
    /// `TcpCancelHandle`, so `close()` fires the handle before taking the
    /// slot and a parked `recv()` ends within about one poll boundary.
    owned: Owned<SendHalf<TcpTransport>>,
}

#[pymethods]
impl PyTcpTransport {
    /// Return a builder for configuring and constructing a `Transport`.
    #[staticmethod]
    fn builder() -> PyTcpTransportBuilder {
        PyTcpTransportBuilder::default()
    }

    /// Send a payload over the TCP connection. Accepts any bytes-like object:
    /// `bytes`, `bytearray`, `memoryview`, or any buffer-protocol object.
    ///
    /// Raises `TcpError(kind=TOO_LARGE)` if `len(payload)` exceeds
    /// the configured `pkt_size` (default 64 KiB).
    ///
    /// Releases the GIL during the kernel send.
    fn send(&self, py: Python<'_>, payload: &Bound<'_, PyAny>) -> PyResult<()> {
        // Coerce to owned bytes before crossing the allow_threads boundary.
        // `Python<'_>` is `!Send` so nothing involving it can enter the closure.
        let owned: Vec<u8> = if let Ok(slice) = payload.extract::<&[u8]>() {
            slice.to_vec()
        } else {
            // Fallback: bytearray / memoryview / etc. -- coerce through Python
            // `bytes()` builtin (one C copy). Required under abi3-py310 since
            // PyBuffer is gated on not(Py_LIMITED_API) in PyO3 0.22.
            let coerced: Bound<'_, PyBytes> = py
                .import_bound("builtins")?
                .getattr(intern!(py, "bytes"))?
                .call1((payload,))?
                .downcast_into::<PyBytes>()?;
            coerced.as_bytes().to_vec()
        };
        pyres(
            py,
            &TCP,
            py.allow_threads(|| self.owned.with_mut(|t| t.0.send_bytes(&owned))),
        )
    }

    /// Receive bytes from the TCP connection into a pre-allocated `bytearray`.
    ///
    /// Returns the number of bytes written into `buf`. The caller is
    /// responsible for sizing `buf` to at least the expected chunk size
    /// (`Transport.builder().pkt_size(N)` controls the sender cap, which
    /// defaults to 64 KiB).
    ///
    /// `buf` is exported for the duration of the call: a resize of the
    /// same bytearray from another thread while this call is blocked
    /// raises `BufferError` in that thread, and the bytes land in the
    /// unchanged buffer. An empty `buf` raises `ValueError` before any
    /// socket read (a zero-length read would otherwise be indistinguishable
    /// from peer EOF).
    ///
    /// Raises `TcpError(kind=CLOSED)` if the transport has been closed.
    /// Raises `TcpError(kind=IO)` on connection errors (including peer close).
    ///
    /// Releases the GIL while blocking on kernel recv.
    fn recv(&self, py: Python<'_>, buf: &Bound<'_, pyo3::types::PyByteArray>) -> PyResult<usize> {
        let buf_len = buf.len();
        if buf_len == 0 {
            return Err(PyValueError::new_err(
                "recv(): destination bytearray is empty; pass a buffer of at least 1 byte",
            ));
        }
        // Pin the destination's length across the GIL release: a memoryview
        // over the bytearray holds a buffer export, and CPython refuses to
        // resize an exported bytearray (`BufferError: Existing exports of
        // data: object cannot be re-sized`). `pyo3::buffer::PyBuffer` would
        // be the direct export, but it is compiled out under abi3-py310
        // (`any(not(Py_LIMITED_API), Py_3_11)`); `PyMemoryView_FromObject`
        // is in the stable ABI.
        let export = PyMemoryView::from_bound(buf.as_any())?;
        // We need an owned buffer to cross the allow_threads boundary --
        // `PyByteArray` is a Python object and is !Send.
        let mut owned = vec![0u8; buf_len];
        let res = py.allow_threads(|| {
            self.owned
                .with_mut(|t| t.0.recv_bytes(owned.as_mut_slice()))
        });
        let copied: PyResult<usize> = pyres(py, &TCP, res).and_then(|n| {
            // Unreachable while the export is live; a real check, not a
            // debug assert, because the copy below is `unsafe`.
            if buf.len() != buf_len {
                return Err(raise(
                    py,
                    &TCP,
                    BindingError::new(
                        BindingErrorKind::TcpIo,
                        "recv(): destination bytearray changed length during the call",
                    ),
                ));
            }
            // Safety: we hold the GIL; the only other reference to this
            // bytearray's storage is our own memoryview export, which no
            // Python code can reach.
            let dest = unsafe { buf.as_bytes_mut() };
            dest[..n].copy_from_slice(&owned[..n]);
            Ok(n)
        });
        // Release the export explicitly (dropping the Bound would too, but
        // this makes "resizable again once recv() returns" deterministic).
        export.call_method0(intern!(py, "release"))?;
        copied
    }

    /// Peer address as a `"host:port"` string. Returns `""` if the transport
    /// has been closed.
    ///
    /// Releases the GIL during mutex acquisition so a concurrent `recv()`
    /// parked in another thread cannot freeze the interpreter.
    fn peer_addr(&self, py: Python<'_>) -> String {
        py.allow_threads(|| self.owned.with_ref(|t| t.0.peer().to_string()))
            .unwrap_or_default()
    }

    /// Close the transport. Idempotent -- further `.send()` / `.recv()` calls
    /// raise `TcpError(kind=CLOSED)`.
    ///
    /// Fires the cancel handle BEFORE acquiring the inner mutex so any thread
    /// parked in `recv()` unblocks within ≤100 ms, making the lock available
    /// without holding the GIL.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &TCP, &self.owned)
    }

    /// Snapshot of wire-level statistics. Counters are cumulative and never
    /// wrap (saturating add).
    ///
    /// Releases the GIL during mutex acquisition so a concurrent `recv()`
    /// parked in another thread cannot freeze the interpreter.
    fn stats(&self, py: Python<'_>) -> PyResult<Py<PyTcpStats>> {
        let s = pyok(
            py,
            &TCP,
            py.allow_threads(|| self.owned.with_ref(|t| t.0.stats())),
        )?;
        Py::new(py, PyTcpStats::from(s))
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

    fn __repr__(&self, py: Python<'_>) -> String {
        match py.allow_threads(|| {
            self.owned
                .with_ref(|t| format!("Transport(peer={})", t.0.peer()))
        }) {
            Ok(s) => s,
            Err(_) => "Transport(closed)".to_string(),
        }
    }
}

/// Construct a `PyTcpTransport` from an already-connected `TcpTransport`.
/// Used internally by `PyTcpListenerBuilder::build()` / `accept_blocking`.
fn make_py_tcp_transport(t: TcpTransport) -> PyTcpTransport {
    // tcp HAS a real cancel handle, and since WP-C1 its `is_cancelled()` is a
    // cancel latch rather than `!alive`, so a clean peer EOF no longer reads
    // as a caller cancel through it. `CancelSource` still owns its own latch
    // (Python `close()` goes through it) and ORs the handle's in.
    let cancel = CancelSource::new(Arc::new(t.cancel_handle()));
    PyTcpTransport {
        owned: Owned::new(SendHalf(t), cancel.as_dyn(), ()),
    }
}

// ---------------------------------------------------------------------------
// PyTcpTransportBuilder -- builder for PyTcpTransport
// ---------------------------------------------------------------------------

/// Builder for `Transport`. Chain setter calls, then call `.build()`.
///
/// Example:
/// ```python
/// transport = tcp.Transport.builder() \
///     .url("tcp://192.168.1.100:5001") \
///     .nodelay(True) \
///     .connect_timeout_ms(5000) \
///     .build()
/// ```
#[pyclass(name = "TransportBuilder", module = "tstrans.tcp")]
#[derive(Default)]
pub(crate) struct PyTcpTransportBuilder {
    url: Option<String>,
    nodelay: Option<bool>,
    keepalive_ms: Option<u64>,
    rcvbuf: Option<usize>,
    sndbuf: Option<usize>,
    pkt_size: Option<usize>,
    connect_timeout_ms: Option<u64>,
}

#[pymethods]
impl PyTcpTransportBuilder {
    /// Set the destination URL. Required. Must be `tcp://host:port` or
    /// `tcps://host:port` (TLS; verify against a custom CA with
    /// `?ca=<pem path>`. Raises `TcpError(kind=TLS_DISABLED)` at build
    /// time only in source builds without the `tls` feature).
    fn url<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.url = Some(s.to_string());
        slf
    }

    /// Enable or disable TCP_NODELAY (Nagle's algorithm).
    ///
    /// `True` is typically preferred for low-latency streaming.
    fn nodelay(mut slf: PyRefMut<'_, Self>, v: bool) -> PyRefMut<'_, Self> {
        slf.nodelay = Some(v);
        slf
    }

    /// Set SO_KEEPALIVE idle timeout in milliseconds.
    fn keepalive_ms(mut slf: PyRefMut<'_, Self>, v: u64) -> PyRefMut<'_, Self> {
        slf.keepalive_ms = Some(v);
        slf
    }

    /// `SO_RCVBUF` size in bytes.
    fn rcvbuf(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.rcvbuf = Some(v);
        slf
    }

    /// `SO_SNDBUF` size in bytes.
    fn sndbuf(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.sndbuf = Some(v);
        slf
    }

    /// Maximum payload chunk size per `send()` call (default 64 KiB).
    fn pkt_size(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.pkt_size = Some(v);
        slf
    }

    /// Connection timeout in milliseconds (default 10 000 ms).
    fn connect_timeout_ms(mut slf: PyRefMut<'_, Self>, v: u64) -> PyRefMut<'_, Self> {
        slf.connect_timeout_ms = Some(v);
        slf
    }

    /// Build the `Transport` by establishing a TCP connection.
    ///
    /// Raises `TcpError(kind=URL)` for a malformed URL.
    /// Raises `TcpError(kind=CONNECT_TIMEOUT)` or `TcpError(kind=IO)` on
    /// connection failures.
    /// Raises `TcpError(kind=TLS)` on TLS handshake / certificate errors,
    /// or `TcpError(kind=TLS_DISABLED)` if `tcps://` was used in a source
    /// build without the `tls` feature.
    fn build(&self, py: Python<'_>) -> PyResult<PyTcpTransport> {
        let url_str = self
            .url
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("url(...) is required before build()"))?
            .clone();

        let nodelay = self.nodelay;
        let keepalive_ms = self.keepalive_ms;
        let rcvbuf = self.rcvbuf;
        let sndbuf = self.sndbuf;
        let pkt_size = self.pkt_size;
        let connect_timeout_ms = self.connect_timeout_ms;

        let t = py.allow_threads(|| -> Result<TcpTransport, TcpError> {
            let mut b = tst_tcp::TcpTransportBuilder::from_url(&url_str).map_err(TcpError::Url)?;
            if let Some(v) = nodelay {
                b.nodelay(v);
            }
            if let Some(ms) = keepalive_ms {
                b.keepalive(std::time::Duration::from_millis(ms));
            }
            if let Some(v) = rcvbuf {
                b.rcvbuf(v);
            }
            if let Some(v) = sndbuf {
                b.sndbuf(v);
            }
            if let Some(v) = pkt_size {
                b.pkt_size(v);
            }
            if let Some(ms) = connect_timeout_ms {
                b.connect_timeout(std::time::Duration::from_millis(ms));
            }
            b.build()
        });

        match t {
            Ok(transport) => Ok(make_py_tcp_transport(transport)),
            Err(e) => Err(raise(py, &TCP, BindingError::from(e))),
        }
    }

    fn __repr__(&self) -> String {
        format!("TransportBuilder(url={:?})", self.url)
    }
}

// ---------------------------------------------------------------------------
// PyTcpListener -- wraps tst_tcp::TcpListener
// ---------------------------------------------------------------------------

/// `tst_tcp::TcpListener` behind the binding layer's `Close`.
pub(crate) struct TcpListenerHeld(pub TcpListener);

impl tst_pipeline::binding::Close for TcpListenerHeld {
    type Error = core::convert::Infallible;

    fn close(&mut self) -> Result<(), Self::Error> {
        self.0.close();
        Ok(())
    }
}

/// TCP listener -- wraps `tst_tcp::TcpListener`.
///
/// Construct via `Listener.builder().bind("host:port").build()`, then call
/// `accept_blocking()` to receive a `Transport` per inbound connection.
///
/// Binding to port 0 lets the kernel pick a free ephemeral port; read it
/// back via `local_port()` before accepting.
///
/// GIL is released during `accept_blocking` so other Python threads
/// remain live while waiting for a connection.
#[pyclass(name = "Listener", module = "tstrans.tcp")]
pub(crate) struct PyTcpListener {
    /// The binding layer's handle state machine (Arc 2). Snapshot = the
    /// bound port read at `build()`, so `local_port()` never waits behind a
    /// parked `accept_blocking()`.
    owned: Owned<TcpListenerHeld, Option<u16>>,
}

#[pymethods]
impl PyTcpListener {
    /// Return a builder for configuring and constructing a `Listener`.
    #[staticmethod]
    fn builder() -> PyTcpListenerBuilder {
        PyTcpListenerBuilder::default()
    }

    /// Block until a new inbound connection arrives. Returns a `Transport`
    /// wrapping the accepted connection.
    ///
    /// Raises `TcpError(kind=IO)` on accept failure.
    /// Raises `TcpError(kind=CLOSED)` if the listener has been closed —
    /// including a `close()` from another thread while this call is parked.
    ///
    /// Releases the GIL while waiting.
    fn accept_blocking(&self, py: Python<'_>) -> PyResult<PyTcpTransport> {
        // Two-step: accept inside allow_threads (returns Result<TcpTransport, TcpError>
        // where TcpError is Send), then map to PyErr after re-acquiring the GIL.
        let res = py.allow_threads(|| self.owned.with_ref(|l| l.0.accept_blocking()));
        Ok(make_py_tcp_transport(pyres(py, &TCP, res)?))
    }

    /// Local bound port. Non-zero after successful `build()`.
    ///
    /// Use this to discover the ephemeral port when `.bind("127.0.0.1:0")`
    /// was used. Answered ONLY from the `build()`-time snapshot (spec
    /// §3.2's snapshot-getter rule), so it NEVER waits behind an
    /// `accept_blocking()` parked on another thread — the PR #234 class.
    /// Raises `TcpError(kind=CLOSED)` once the listener is closed, and
    /// `TcpError(kind=IO)` in the one case where the snapshot is absent:
    /// the bound listener's `local_addr()` errored at `build()`, which the
    /// kernel does not otherwise do, so there is no port to report.
    fn local_port(&self, py: Python<'_>) -> PyResult<u16> {
        match self.owned.snapshot() {
            Some(port) if !self.owned.is_closed() => Ok(*port),
            Some(_) => Err(raise(py, &TCP, BindingError::from(HandleState::Closed))),
            None => Err(raise(
                py,
                &TCP,
                BindingError::new(
                    BindingErrorKind::TcpIo,
                    "local port unavailable: getsockname failed when the listener \
                     was built",
                ),
            )),
        }
    }

    /// Close the listener. Fires the cancel handle BEFORE taking the lock,
    /// so an `accept_blocking()` parked on another thread ends with
    /// `TcpError(kind=CLOSED)` within ≤100 ms; then frees the listener.
    /// Idempotent -- further `accept_blocking()` calls raise
    /// `TcpError(kind=CLOSED)`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        close_owned(py, &TCP, &self.owned)
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
            "Listener(closed)".to_string()
        } else {
            "Listener(open)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyTcpListenerBuilder -- builder for PyTcpListener
// ---------------------------------------------------------------------------

/// Builder for `Listener`. Chain setter calls, then call `.build()`.
///
/// Example:
/// ```python
/// listener = tcp.Listener.builder() \
///     .bind("127.0.0.1:0") \
///     .nodelay(True) \
///     .build()
/// port = listener.local_port()
/// ```
#[pyclass(name = "ListenerBuilder", module = "tstrans.tcp")]
#[derive(Default)]
pub(crate) struct PyTcpListenerBuilder {
    bind_addr: Option<String>,
    nodelay: Option<bool>,
    rcvbuf: Option<usize>,
    sndbuf: Option<usize>,
    pkt_size: Option<usize>,
    tls_cert_key: Option<(String, String)>,
}

#[pymethods]
impl PyTcpListenerBuilder {
    /// Set the bind address as `"host:port"` (e.g. `"127.0.0.1:0"` or
    /// `"0.0.0.0:5001"`). Required. Port 0 requests an ephemeral port.
    fn bind<'py>(mut slf: PyRefMut<'py, Self>, s: &str) -> PyRefMut<'py, Self> {
        slf.bind_addr = Some(s.to_string());
        slf
    }

    /// Enable or disable TCP_NODELAY for accepted connections.
    fn nodelay(mut slf: PyRefMut<'_, Self>, v: bool) -> PyRefMut<'_, Self> {
        slf.nodelay = Some(v);
        slf
    }

    /// `SO_RCVBUF` size in bytes for accepted connections.
    fn rcvbuf(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.rcvbuf = Some(v);
        slf
    }

    /// `SO_SNDBUF` size in bytes for accepted connections.
    fn sndbuf(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.sndbuf = Some(v);
        slf
    }

    /// Maximum payload chunk size for accepted connections (default 64 KiB).
    fn pkt_size(mut slf: PyRefMut<'_, Self>, v: usize) -> PyRefMut<'_, Self> {
        slf.pkt_size = Some(v);
        slf
    }

    /// Serve TLS (`tcps://`) on accepted connections. `cert` and `key` are
    /// PEM certificate-chain / private-key *file paths*, read at `build()`
    /// (missing or malformed files raise `TcpError(kind=TLS)`). A source
    /// build without the `tls` feature raises `TcpError(kind=TLS_DISABLED)`
    /// at `build()`. Paths ride the internal listener URL, so a path
    /// containing `&`, `#`, or `?` raises `TcpError(kind=INVALID_CONFIG)`
    /// at `build()`.
    fn tls<'py>(mut slf: PyRefMut<'py, Self>, cert: &str, key: &str) -> PyRefMut<'py, Self> {
        slf.tls_cert_key = Some((cert.to_string(), key.to_string()));
        slf
    }

    /// Bind the listener socket.
    ///
    /// Raises `ValueError` if `bind(...)` was not called.
    /// Raises `TcpError(kind=IO)` if the port is in use or permissions are
    /// insufficient.
    fn build(&self, py: Python<'_>) -> PyResult<PyTcpListener> {
        let addr_str = self
            .bind_addr
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("bind(...) is required before build()"))?
            .clone();

        let nodelay = self.nodelay;
        let rcvbuf = self.rcvbuf;
        let sndbuf = self.sndbuf;
        let pkt_size = self.pkt_size;
        let tls_cert_key = self.tls_cert_key.clone();
        // The cert/key paths are interpolated into the internal listener
        // URL below — a `&`, `#`, or `?` would be parsed as URL structure
        // and silently change which files are read. Fail fast instead.
        if let Some((cert, key)) = &tls_cert_key {
            for (label, path) in [("cert", cert), ("key", key)] {
                if path.contains(['&', '#', '?']) {
                    return Err(raise(
                        py,
                        &TCP,
                        BindingError::new(
                            BindingErrorKind::TcpInvalidConfig,
                            format!(
                                "tls {label} path contains a URL-structural character \
                             ('&', '#', or '?') and cannot be used: {path}"
                            ),
                        ),
                    ));
                }
            }
        }

        let listener = py.allow_threads(|| -> Result<TcpListener, TcpError> {
            // Build a listener URL: tcp://addr:port?listen=1, or the tcps://
            // variant carrying the cert + key paths when tls(...) was set.
            let listen_url = match &tls_cert_key {
                Some((cert, key)) => {
                    format!("tcps://{addr_str}?listen=1&cert={cert}&key={key}")
                }
                None => format!("tcp://{}?listen=1", addr_str),
            };
            let mut b =
                tst_tcp::TcpListenerBuilder::from_url(&listen_url).map_err(TcpError::Url)?;
            if let Some(v) = nodelay {
                b.nodelay(v);
            }
            if let Some(v) = rcvbuf {
                b.rcvbuf(v);
            }
            if let Some(v) = sndbuf {
                b.sndbuf(v);
            }
            if let Some(v) = pkt_size {
                b.pkt_size(v);
            }
            b.build()
        });

        match listener {
            Ok(l) => {
                let cancel = CancelSource::new(Arc::new(l.cancel_handle()));
                let local_port = l.local_addr().ok().map(|a| a.port());
                Ok(PyTcpListener {
                    owned: Owned::new(TcpListenerHeld(l), cancel.as_dyn(), local_port),
                })
            }
            Err(e) => Err(raise(py, &TCP, BindingError::from(e))),
        }
    }

    fn __repr__(&self) -> String {
        format!("ListenerBuilder(bind={:?})", self.bind_addr)
    }
}

// ---------------------------------------------------------------------------
// TlsConfig / ClientCert -- forward-compat dataclasses for tcps:// callers
// ---------------------------------------------------------------------------

/// TLS configuration dataclass for `tcps://` transports.
///
/// **Note:** these dataclasses are forward-compat surface only — the
/// builder accepts them but does not read them. The working knobs are:
/// callers verify against a custom CA with the `?ca=<pem path>` URL
/// param (native trust roots otherwise); listeners serve TLS via
/// `ListenerBuilder.tls(cert, key)`. mTLS client certificates
/// (`ClientCert`) have no tst-tcp backend yet.
#[pyclass(name = "TlsConfig", module = "tstrans.tcp", frozen)]
#[derive(Clone)]
pub(crate) struct PyTlsConfig {
    /// PEM-encoded CA certificate bundle. Used for server certificate
    /// verification when connecting to `tcps://` endpoints.
    pub ca_pem: Vec<u8>,
    /// If `True` (default), the server hostname is verified against the
    /// certificate CN / SAN fields.
    pub verify_hostname: bool,
    /// Optional client certificate for mutual TLS authentication.
    pub client_cert: Option<PyClientCert>,
}

#[pymethods]
impl PyTlsConfig {
    #[new]
    #[pyo3(signature = (ca_pem = None, *, verify_hostname = true, client_cert = None))]
    fn new(
        ca_pem: Option<&[u8]>,
        verify_hostname: bool,
        client_cert: Option<PyClientCert>,
    ) -> Self {
        Self {
            ca_pem: ca_pem.unwrap_or_default().to_vec(),
            verify_hostname,
            client_cert,
        }
    }

    // Explicit getters — `ca_pem` returns `bytes` (a `get_all` auto-getter
    // would expose the `Vec<u8>` as `list[int]`, mismatching the stub + tests).
    #[getter]
    fn ca_pem<'py>(&self, py: Python<'py>) -> pyo3::Bound<'py, pyo3::types::PyBytes> {
        pyo3::types::PyBytes::new_bound(py, &self.ca_pem)
    }
    #[getter]
    fn verify_hostname(&self) -> bool {
        self.verify_hostname
    }
    #[getter]
    fn client_cert(&self) -> Option<PyClientCert> {
        self.client_cert.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "TlsConfig(ca_pem=<{} bytes>, verify_hostname={})",
            self.ca_pem.len(),
            self.verify_hostname
        )
    }
}

/// Client certificate for mutual TLS authentication.
#[pyclass(name = "ClientCert", module = "tstrans.tcp", frozen)]
#[derive(Clone)]
pub(crate) struct PyClientCert {
    /// PEM-encoded client certificate.
    pub cert_pem: Vec<u8>,
    /// PEM-encoded private key. Treat as sensitive; avoid logging.
    pub key_pem: Vec<u8>,
}

#[pymethods]
impl PyClientCert {
    #[new]
    fn new(cert_pem: &[u8], key_pem: &[u8]) -> Self {
        Self {
            cert_pem: cert_pem.to_vec(),
            key_pem: key_pem.to_vec(),
        }
    }

    // Explicit getters returning `bytes` (not the `list[int]` a `get_all`
    // auto-getter would expose for a `Vec<u8>` field).
    #[getter]
    fn cert_pem<'py>(&self, py: Python<'py>) -> pyo3::Bound<'py, pyo3::types::PyBytes> {
        pyo3::types::PyBytes::new_bound(py, &self.cert_pem)
    }
    #[getter]
    fn key_pem<'py>(&self, py: Python<'py>) -> pyo3::Bound<'py, pyo3::types::PyBytes> {
        pyo3::types::PyBytes::new_bound(py, &self.key_pem)
    }

    fn __repr__(&self) -> String {
        format!(
            "ClientCert(cert_pem=<{} bytes>, key_pem=<redacted {}>)",
            self.cert_pem.len(),
            self.key_pem.len()
        )
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "tcp")?;
    m.add_class::<PyTcpStats>()?;
    m.add_class::<PyTcpTransport>()?;
    m.add_class::<PyTcpTransportBuilder>()?;
    m.add_class::<PyTcpListener>()?;
    m.add_class::<PyTcpListenerBuilder>()?;
    m.add_class::<PyTlsConfig>()?;
    m.add_class::<PyClientCert>()?;
    parent.add_submodule(&m)?;
    Ok(())
}
