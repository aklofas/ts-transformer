//! Plan A5b Wave C T12 — `HlsPublisher` + `HlsPublisherBuilder`.
//!
//! `tstrans.hls.HlsPublisher` wraps `tst_hls::HlsPublisher`: an
//! outbound-only, segment-aware MPEG-TS sink that writes `.ts` segments +
//! a `playlist.m3u8` to disk and serves them over a built-in HTTP(S)
//! server. It is a concrete `impl Publisher` (NOT bridged through
//! Python) and is registered (in `tstrans/hls.py`) as a virtual subclass
//! of the pure-Python `Publisher` ABC so `isinstance(pub, Publisher)`
//! holds.
//!
//! Lifecycle:
//! - `finish()` consumes the inner publisher (writes the terminal
//!   playlist, tears down the HTTP server). Stored as `Option<...>` +
//!   `take()` on finish; subsequent ops raise `HlsError(FINISHED)`.
//! - `MuxPublisher.with_config_hls(pub, ...)` *also* consumes the inner
//!   via `take_inner()` (moving it into the shell). Either path leaves
//!   the handle closed.
//!
//! GIL: `push_ts` / `cut_segment` / `finish` release the GIL via
//! `py.allow_threads` (the disk + HTTP work is pure Rust). Fast read-only
//! getters (`stats`, `hls_stats`, `local_addr`, `render_playlist`) don't
//! release it.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisher, HlsPublisherBuilder, HlsServerHandle};

use crate::hls::config::{PyHlsMode, PyHlsStats};
use crate::hls::publisher_abc::PyPublisherStats;
use crate::hls::{map_hls_error, map_hls_url_error};
use crate::raise::{HLS, raise};
use tst_pipeline::binding::{BindingError, BindingErrorKind};

// ---------------------------------------------------------------------------
// PyHlsPublisher — wraps tst_hls::HlsPublisher
// ---------------------------------------------------------------------------

/// HLS publisher (`tstrans.hls.HlsPublisher`). Segments MPEG-TS to disk +
/// serves a built-in HTTP playlist. Build via `HlsPublisher.builder()`.
///
/// Registered (in `tstrans/hls.py`) as a *virtual* subclass of the
/// pure-Python `Publisher` ABC via `Publisher.register(HlsPublisher)`, so
/// `isinstance(pub, Publisher)` is True without native inheritance. We do
/// NOT make this a native subclass of the ABC: a native base would force
/// every constructor (`build`, `with_config_hls`, `finish_into_publisher`)
/// to return a `PyClassInitializer` and would fight the builder pattern.
/// Virtual registration gives the `isinstance` contract cleanly.
///
/// Either `finish()` or handing it to `MuxPublisher.with_config_hls`
/// consumes the inner publisher; subsequent operations raise
/// `HlsError(FINISHED)`.
#[pyclass(name = "HlsPublisher", module = "tstrans.hls")]
pub(crate) struct PyHlsPublisher {
    /// `Option` so `finish()` / `MuxPublisher.with_config_hls` can move
    /// the inner publisher out while leaving the PyClass addressable.
    /// `Mutex` to allow `&self` methods (push_ts / cut_segment) plus a
    /// `take()` on finish.
    inner: Mutex<Option<HlsPublisher>>,
}

impl PyHlsPublisher {
    /// Wrap an owned Rust `HlsPublisher` (used by
    /// `MuxPublisher.finish_into_publisher`).
    pub(crate) fn from_inner(inner: HlsPublisher) -> Self {
        Self {
            inner: Mutex::new(Some(inner)),
        }
    }

    /// Move the inner publisher out (consumes the handle). Used by
    /// `MuxPublisher.with_config_hls`. Returns `None` if already consumed.
    pub(crate) fn take_inner(&mut self) -> Option<HlsPublisher> {
        self.inner.get_mut().ok().and_then(|o| o.take())
    }
}

#[pymethods]
impl PyHlsPublisher {
    /// Return a fresh `HlsPublisherBuilder`.
    #[staticmethod]
    fn builder() -> PyHlsPublisherBuilder {
        PyHlsPublisherBuilder::default()
    }

    /// Push pre-muxed MPEG-TS bytes (must be a whole multiple of 188).
    /// Raises `HlsError(UNALIGNED_PUSH_TS)` otherwise.
    fn push_ts(&self, py: Python<'_>, ts_bytes: &Bound<'_, PyAny>) -> PyResult<()> {
        let coerced = crate::util::coerce_bytes_like(py, ts_bytes)?;
        // Bind the &[u8] before the closure: the `Bound<PyBytes>` stays on
        // the stack pinning the bytes; the borrowed slice is `Ungil` even
        // though the `Bound` itself is not.
        let slice: &[u8] = coerced.as_bytes();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_mut().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        py.allow_threads(|| Publisher::push_ts(inner, slice))
            .map_err(|e| map_hls_error(py, e))
    }

    /// Hint that the next `push_ts` should start a new segment.
    fn cut_segment(&self, py: Python<'_>) -> PyResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_mut().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        py.allow_threads(|| Publisher::cut_segment(inner))
            .map_err(|e| map_hls_error(py, e))
    }

    /// Hint a new segment, supplying its media-presentation duration in
    /// microseconds. Records this as `#EXTINF` instead of wall-clock time.
    fn cut_segment_with_duration(&self, py: Python<'_>, media_duration_us: u64) -> PyResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_mut().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        let dur = std::time::Duration::from_micros(media_duration_us);
        py.allow_threads(|| Publisher::cut_segment_with_duration(inner, dur))
            .map_err(|e| map_hls_error(py, e))
    }

    /// Finalize: flush the open segment, write the terminal playlist,
    /// tear down the HTTP server. **Consumes** the inner publisher;
    /// subsequent calls raise `HlsError(FINISHED)`.
    fn finish(&self, py: Python<'_>) -> PyResult<()> {
        let inner = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
            guard.take().ok_or_else(|| {
                raise(
                    py,
                    &HLS,
                    BindingError::new(
                        BindingErrorKind::HlsFinished,
                        "HlsPublisher already finished",
                    ),
                )
            })?
        };
        py.allow_threads(|| Publisher::finish(inner))
            .map_err(|e| map_hls_error(py, e))
    }

    /// Like `finish()`, but keep the built-in HTTP server serving the
    /// completed (terminal) playlist + segments until the returned
    /// `HlsServerHandle` is shut down / dropped. This is how a VOD or EVENT
    /// stream stays observable after the stream ends. **Consumes** the inner
    /// publisher; subsequent ops raise `HlsError(FINISHED)`.
    fn finish_serving(&self, py: Python<'_>) -> PyResult<PyHlsServerHandle> {
        // Take the inner BEFORE the fallible consume (mirrors `finish()`):
        // `finish_serving` moves `self` by value on the Rust side, so we must
        // zero the Option first — a failure leaves the handle finished (the
        // Rust side already flipped its `finished` flag) rather than leaking a
        // half-consumed publisher.
        let inner = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
            guard.take().ok_or_else(|| {
                raise(
                    py,
                    &HLS,
                    BindingError::new(
                        BindingErrorKind::HlsFinished,
                        "HlsPublisher already finished",
                    ),
                )
            })?
        };
        let handle = py
            .allow_threads(|| inner.finish_serving())
            .map_err(|e| map_hls_error(py, e))?;
        Ok(PyHlsServerHandle::from_inner(handle))
    }

    /// Universal cross-publisher stats (`PublisherStats`).
    fn stats(&self, py: Python<'_>) -> PyResult<PyPublisherStats> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        Ok(PyPublisherStats::from_core(Publisher::stats(inner)))
    }

    /// Richer HLS-specific stats (`HlsStats`).
    fn hls_stats(&self, py: Python<'_>) -> PyResult<PyHlsStats> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        Ok(PyHlsStats::from(inner.hls_stats()))
    }

    /// Local socket address the HTTP server bound to, as `"ip:port"`, or
    /// `None` if the server is no longer running (e.g. after `finish`).
    fn local_addr(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        Ok(inner.local_addr().map(|a| a.to_string()))
    }

    /// Convenience: the bound TCP port (0 if no server). Raises
    /// `HlsError(FINISHED)` if consumed.
    fn local_port(&self, py: Python<'_>) -> PyResult<u16> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        Ok(inner.local_addr().map(|a| a.port()).unwrap_or(0))
    }

    /// Render the current playlist text. `is_event` selects the terminal
    /// (final) form when true (writes `#EXT-X-ENDLIST`).
    #[pyo3(signature = (is_event = false))]
    fn render_playlist(&self, py: Python<'_>, is_event: bool) -> PyResult<String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
        let inner = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsPublisher finished"),
            )
        })?;
        Ok(inner.render_playlist(is_event))
    }

    /// Close = `finish()` semantics but never raises if already finished
    /// (idempotent). Useful in `with`-style cleanup.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let inner = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("HlsPublisher mutex poisoned"))?;
            guard.take()
        };
        if let Some(inner) = inner {
            py.allow_threads(|| Publisher::finish(inner))
                .map_err(|e| map_hls_error(py, e))?;
        }
        Ok(())
    }

    fn __repr__(&self) -> String {
        let open = self.inner.lock().map(|g| g.is_some()).unwrap_or(false);
        if open {
            "HlsPublisher(open)".to_string()
        } else {
            "HlsPublisher(finished)".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// PyHlsPublisherBuilder — builder for PyHlsPublisher
// ---------------------------------------------------------------------------

/// Builder for `HlsPublisher`. Chain setters then call `.build()`.
///
/// The setters mirror `tst_hls::HlsPublisherBuilder`. Each setter
/// returns `self` for chaining.
#[pyclass(name = "HlsPublisherBuilder", module = "tstrans.hls")]
#[derive(Default)]
pub(crate) struct PyHlsPublisherBuilder {
    /// Stored as `Option` so each move-style setter can `take()` the inner
    /// Rust builder, apply the consuming method, and replace it (the Rust
    /// builder uses move-style `self -> Self` chaining).
    inner: Option<HlsPublisherBuilder>,
}

impl PyHlsPublisherBuilder {
    /// Apply a move-style mutation to the inner Rust builder.
    fn apply<F>(&mut self, f: F) -> PyResult<()>
    where
        F: FnOnce(HlsPublisherBuilder) -> HlsPublisherBuilder,
    {
        let b = self.inner.take().unwrap_or_default();
        self.inner = Some(f(b));
        Ok(())
    }
}

#[pymethods]
impl PyHlsPublisherBuilder {
    #[new]
    fn new() -> Self {
        Self {
            inner: Some(HlsPublisherBuilder::new()),
        }
    }

    /// HTTP server bind address (e.g. `"127.0.0.1:0"` for an OS-assigned
    /// port). Raises `ValueError` on a malformed socket address.
    fn bind<'py>(mut slf: PyRefMut<'py, Self>, addr: &str) -> PyResult<PyRefMut<'py, Self>> {
        let parsed: SocketAddr = addr
            .parse()
            .map_err(|e| PyValueError::new_err(format!("invalid bind address {addr:?}: {e}")))?;
        slf.apply(|b| b.bind(parsed))?;
        Ok(slf)
    }

    /// Filesystem directory for `.ts` segments + `playlist.m3u8`.
    fn output_dir<'py>(mut slf: PyRefMut<'py, Self>, path: &str) -> PyResult<PyRefMut<'py, Self>> {
        let p = path.to_string();
        slf.apply(|b| b.output_dir(p))?;
        Ok(slf)
    }

    /// Target segment duration in milliseconds.
    fn segment_duration_ms(mut slf: PyRefMut<'_, Self>, ms: u64) -> PyResult<PyRefMut<'_, Self>> {
        let d = Duration::from_millis(ms);
        slf.apply(|b| b.segment_duration(d))?;
        Ok(slf)
    }

    /// Hard upper bound on an open segment's wall-clock age in the
    /// keyframe-driven flow (force-cuts when a keyframe is overdue). Defaults
    /// to `2 × segment_duration`; must be `≥ segment_duration` at `build()`.
    ///
    /// `ms == 0` leaves the library default in place (matching the C ABI): a
    /// fresh builder is already seeded with the default, and `tst-hls`
    /// exposes no reset-to-default setter, so a `0` after a prior non-zero
    /// call does **not** clear the earlier value. Callers wanting the default
    /// simply never call this setter.
    fn max_segment_duration_ms(
        mut slf: PyRefMut<'_, Self>,
        ms: u64,
    ) -> PyResult<PyRefMut<'_, Self>> {
        if ms != 0 {
            let d = Duration::from_millis(ms);
            slf.apply(|b| b.max_segment_duration(d))?;
        }
        Ok(slf)
    }

    /// Rolling-window size (number of segments visible in a LIVE playlist).
    fn playlist_window(mut slf: PyRefMut<'_, Self>, n: usize) -> PyResult<PyRefMut<'_, Self>> {
        slf.apply(|b| b.playlist_window(n))?;
        Ok(slf)
    }

    /// Playlist mode (LIVE / EVENT / VOD).
    fn mode(mut slf: PyRefMut<'_, Self>, mode: PyHlsMode) -> PyResult<PyRefMut<'_, Self>> {
        let rust_mode: HlsMode = mode.into();
        slf.apply(|b| b.mode(rust_mode))?;
        Ok(slf)
    }

    /// Enable HTTP Basic auth with `(user, password)`.
    fn basic_auth<'py>(
        mut slf: PyRefMut<'py, Self>,
        user: &str,
        password: &str,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let (u, p) = (user.to_string(), password.to_string());
        slf.apply(|b| b.basic_auth(u, p))?;
        Ok(slf)
    }

    /// Enable HTTPS by supplying PEM cert + key file paths. Compiled in
    /// by default (tst-py `tls` feature; wheels ship it); a source build
    /// without `tls` raises `HlsError(TLS_DISABLED)` at `build()`.
    fn enable_tls<'py>(
        mut slf: PyRefMut<'py, Self>,
        cert: &str,
        key: &str,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let (c, k) = (cert.to_string(), key.to_string());
        slf.apply(|b| b.enable_tls(c, k))?;
        Ok(slf)
    }

    /// Seed the builder from an `hls://` / `hlss://` URL. Replaces the
    /// builder's accumulated config with the URL-derived one (subsequent
    /// setters overlay on top). Raises `HlsError(URL)` on a bad URL.
    fn from_url<'py>(mut slf: PyRefMut<'py, Self>, url: &str) -> PyResult<PyRefMut<'py, Self>> {
        let py = slf.py();
        let b = HlsPublisherBuilder::from_url(url).map_err(|e| map_hls_url_error(py, e))?;
        slf.inner = Some(b);
        Ok(slf)
    }

    /// Build the `HlsPublisher` (binds the HTTP server immediately).
    /// Raises `HlsError(BIND_FAILED)` / `HlsError(INVALID_CONFIG)` /
    /// `HlsError(TLS_DISABLED)` per the failure.
    fn build(&mut self, py: Python<'_>) -> PyResult<PyHlsPublisher> {
        let b = self
            .inner
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("HlsPublisherBuilder already consumed"))?;
        let pub_ = py
            .allow_threads(|| b.build())
            .map_err(|e| map_hls_error(py, e))?;
        Ok(PyHlsPublisher::from_inner(pub_))
    }

    fn __repr__(&self) -> &'static str {
        "HlsPublisherBuilder(...)"
    }
}

// ---------------------------------------------------------------------------
// PyHlsServerHandle — wraps tst_hls::HlsServerHandle
// ---------------------------------------------------------------------------

/// Live HTTP server serving a finished HLS playlist + its segments
/// (`tstrans.hls.HlsServerHandle`). Returned by
/// `HlsPublisher.finish_serving()`; keeps the built-in server up so clients
/// can fetch the terminal playlist and every segment file after the stream
/// has ended.
///
/// `shutdown()` (or the context-manager `__exit__`, or drop) stops serving
/// and drains the runtime. `shutdown()` is idempotent — a second call (via
/// `close()`, `__exit__`, or drop after an explicit shutdown) is a no-op.
#[pyclass(name = "HlsServerHandle", module = "tstrans.hls")]
pub(crate) struct PyHlsServerHandle {
    /// `Option` so `shutdown()` can move the handle out (its Rust
    /// `shutdown(self)` consumes by value) while leaving the PyClass
    /// addressable; `Mutex` to allow the `&self` methods plus a `take()` on
    /// shutdown.
    inner: Mutex<Option<HlsServerHandle>>,
}

impl PyHlsServerHandle {
    fn from_inner(inner: HlsServerHandle) -> Self {
        Self {
            inner: Mutex::new(Some(inner)),
        }
    }
}

#[pymethods]
impl PyHlsServerHandle {
    /// Local socket address the HTTP server is bound to, as `"ip:port"`.
    /// Raises `HlsError(FINISHED)` if the server has already been shut down.
    fn local_addr(&self, py: Python<'_>) -> PyResult<String> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsServerHandle mutex poisoned"))?;
        let handle = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsServerHandle shut down"),
            )
        })?;
        Ok(handle.local_addr().to_string())
    }

    /// Convenience: the bound TCP port. Raises `HlsError(FINISHED)` if the
    /// server has already been shut down.
    fn local_port(&self, py: Python<'_>) -> PyResult<u16> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PyRuntimeError::new_err("HlsServerHandle mutex poisoned"))?;
        let handle = guard.as_ref().ok_or_else(|| {
            raise(
                py,
                &HLS,
                BindingError::new(BindingErrorKind::HlsFinished, "HlsServerHandle shut down"),
            )
        })?;
        Ok(handle.local_addr().port())
    }

    /// Stop serving and drain the runtime. Idempotent: a second call is a
    /// no-op. Also happens automatically on drop.
    fn shutdown(&self, py: Python<'_>) -> PyResult<()> {
        let handle = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| PyRuntimeError::new_err("HlsServerHandle mutex poisoned"))?;
            guard.take()
        };
        if let Some(handle) = handle {
            py.allow_threads(|| handle.shutdown());
        }
        Ok(())
    }

    /// Alias for `shutdown()` (idempotent). Useful in `with`-style cleanup.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        self.shutdown(py)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<PyObject>,
        _exc_value: Option<PyObject>,
        _traceback: Option<PyObject>,
    ) -> PyResult<bool> {
        self.shutdown(py)?;
        // Return false so any in-context exception propagates.
        Ok(false)
    }

    fn __repr__(&self) -> String {
        let live = self.inner.lock().map(|g| g.is_some()).unwrap_or(false);
        if live {
            "HlsServerHandle(serving)".to_string()
        } else {
            "HlsServerHandle(shutdown)".to_string()
        }
    }
}
