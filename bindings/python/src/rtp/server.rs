//! `RtspServer`, `MountHandle`, `RtspServerConfig` for RTP/RTSP.
//!
//! Bindings for `tst_rtp::rtsp::server::RtspServer` + `MountHandle`.
//! Construction goes through the pure-Python `RtspServerConfig`
//! dataclass (defined in `tstrans/rtp.py`): `RtspServer.start(cfg)`
//! reads the dataclass attributes here, fluent-builds the Rust
//! `RtspServerBuilder`, and returns a PyRtspServer.
//!
//! GIL release boundaries (per spec §"GIL release boundaries"):
//! - `start`, `stop`, `add_unicast_mount`, `add_multicast_mount`,
//!   every `MountHandle.push_*` — wrap Rust work in `py.allow_threads`.
//! - Dataclass construction, `__enter__`/`__exit__`, `cancel_handle`,
//!   `stats`, handle getters — no release (pure Python or sub-microsecond).

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;
use std::time::Duration;

use pyo3::intern;
use pyo3::prelude::*;
use secrecy::SecretString;

use tst_rtp::builder::RtspServerBuilder;
use tst_rtp::cancel::RtspServerCancelHandle;
use tst_rtp::rtsp::server::mount::{MountHandle as RustMountHandle, MountKind as RustMountKind};
use tst_rtp::rtsp::server::{RtspServer as RustRtspServer, ServerStats as RustServerStats};

use crate::mux::{
    PyAudioStreamHandle, PyDataStreamHandle, PyKlvStreamHandle, PyMuxerProgramConfig,
    PySubtitleStreamHandle, PyVideoStreamHandle, py_pts90khz,
};
use crate::raise::{RTSP, raise};
use tst_pipeline::binding::{BindingError, BindingErrorKind};

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRtspServer>()?;
    m.add_class::<PyMountHandle>()?;
    m.add_class::<PyServerStats>()?;
    m.add_class::<PyMountStats>()?;
    m.add_class::<PyRtspServerCancelHandle>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PyRtspServerCancelHandle — cross-thread hard-cancel.
// ---------------------------------------------------------------------------

/// Cross-thread hard-cancel handle for an [`RtspServer`][PyRtspServer].
///
/// Returned by [`RtspServer.cancel_handle()`][PyRtspServer::cancel_handle].
/// Cloning is cheap (`Arc`); multiple holders can race the cancel call
/// (idempotent). Calling `.cancel()` aborts every in-flight session at
/// its next poll boundary, bypassing the graceful Notice-5402 path.
///
/// Note: this is the server-flavoured cancel handle. The `CancelHandle` for
/// `Sender` / `Receiver` is a distinct type (the server token is
/// tokio-aware; the transport one is thread-only).
#[pyclass(name = "RtspServerCancelHandle", module = "tstrans.rtp", frozen)]
#[derive(Clone)]
pub struct PyRtspServerCancelHandle {
    inner: RtspServerCancelHandle,
}

#[pymethods]
impl PyRtspServerCancelHandle {
    /// Fire the cancel signal. Idempotent — repeated calls are no-ops.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// True once `.cancel()` has been observed.
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!(
            "RtspServerCancelHandle(cancelled={})",
            self.inner.is_cancelled()
        )
    }
}

// ---------------------------------------------------------------------------
// PyServerStats + PyMountStats.
// ---------------------------------------------------------------------------

/// Frozen snapshot of aggregate [`RtspServer`][PyRtspServer] stats.
/// Returned by [`RtspServer.stats()`][PyRtspServer::stats].
#[pyclass(name = "ServerStats", module = "tstrans.rtp", frozen)]
#[derive(Clone, Copy)]
pub struct PyServerStats {
    inner: RustServerStats,
}

#[pymethods]
impl PyServerStats {
    /// Live count of accepted (and not-yet-closed) client sessions.
    #[getter]
    pub fn active_sessions(&self) -> usize {
        self.inner.active_sessions
    }

    /// Cumulative RTP packets sent across all peers + all mounts.
    #[getter]
    pub fn total_rtp_packets_sent(&self) -> u64 {
        self.inner.total_rtp_packets_sent
    }

    /// Cumulative RTP bytes sent across all peers + all mounts.
    #[getter]
    pub fn total_rtp_bytes_sent(&self) -> u64 {
        self.inner.total_rtp_bytes_sent
    }

    /// Number of registered mounts.
    #[getter]
    pub fn mounts(&self) -> usize {
        self.inner.mounts
    }

    fn __repr__(&self) -> String {
        format!(
            "ServerStats(active_sessions={}, mounts={}, total_rtp_packets_sent={}, total_rtp_bytes_sent={})",
            self.inner.active_sessions,
            self.inner.mounts,
            self.inner.total_rtp_packets_sent,
            self.inner.total_rtp_bytes_sent,
        )
    }
}

/// Frozen snapshot of per-mount stats. Returned by
/// [`MountHandle.stats()`][PyMountHandle::stats].
#[pyclass(name = "MountStats", module = "tstrans.rtp", frozen)]
pub struct PyMountStats {
    bytes_pushed: u64,
    packets_pushed: u64,
    peer_count: usize,
    frames_dropped_total: u64,
}

#[pymethods]
impl PyMountStats {
    /// Cumulative TS bytes pushed through this mount's fanout.
    #[getter]
    pub fn bytes_pushed(&self) -> u64 {
        self.bytes_pushed
    }

    /// Cumulative RTP-sized chunks broadcast through this mount.
    #[getter]
    pub fn packets_pushed(&self) -> u64 {
        self.packets_pushed
    }

    /// Live subscriber count on the broadcast channel.
    #[getter]
    pub fn peer_count(&self) -> usize {
        self.peer_count
    }

    /// Sum of per-peer dropped-frame counters reported by lagging
    /// subscribers.
    #[getter]
    pub fn frames_dropped_total(&self) -> u64 {
        self.frames_dropped_total
    }

    fn __repr__(&self) -> String {
        format!(
            "MountStats(bytes_pushed={}, packets_pushed={}, peer_count={}, frames_dropped_total={})",
            self.bytes_pushed, self.packets_pushed, self.peer_count, self.frames_dropped_total,
        )
    }
}

// ---------------------------------------------------------------------------
// PyMountHandle.
// ---------------------------------------------------------------------------

/// Public mount surface returned by [`RtspServer.add_unicast_mount`] /
/// [`RtspServer.add_multicast_mount`]. Cloning is cheap (clones the
/// internal `Arc`); multiple holders can push from different threads.
///
/// All `push_*` methods release the GIL via `py.allow_threads()` so
/// concurrent Python threads can run while the muxer/fanout work
/// proceeds.
#[pyclass(name = "MountHandle", module = "tstrans.rtp", frozen)]
#[derive(Clone)]
pub struct PyMountHandle {
    inner: RustMountHandle,
}

#[pymethods]
impl PyMountHandle {
    // ── Identity / introspection ────────────────────────────────────────────

    /// The mount path registered via `add_unicast_mount("/path", ...)`.
    pub fn mount_path(&self) -> String {
        self.inner.mount_path().to_string()
    }

    /// Live subscriber count on the broadcast channel.
    pub fn peer_count(&self) -> usize {
        self.inner.peer_count()
    }

    /// Discriminant string — `"unicast"` or `"multicast"`. Matches the
    /// kind passed to the originating `add_*_mount` call.
    pub fn mount_kind(&self) -> &'static str {
        match self.inner.mount_kind() {
            RustMountKind::Unicast => "unicast",
            RustMountKind::Multicast { .. } => "multicast",
            _ => "unknown",
        }
    }

    /// Snapshot of cumulative + live mount stats.
    pub fn stats(&self) -> PyMountStats {
        let s = self.inner.stats();
        PyMountStats {
            bytes_pushed: s.bytes_pushed,
            packets_pushed: s.packets_pushed,
            peer_count: s.peer_count,
            frames_dropped_total: s.frames_dropped_total,
        }
    }

    // ── Push surface — single stream variants ──────────────────────────────
    //
    // `pts` is keyword-only on every push method. Each method wraps the Rust call in
    // `py.allow_threads(|| ...)` so other Python threads can run while
    // the muxer + broadcast fanout proceed.

    /// Push one video access unit onto the lone configured video stream.
    /// Accepts any bytes-like input (bytes / bytearray / memoryview /
    /// NumPy uint8). Raises `RtspError(MOUNT)` on muxer rejection or
    /// backpressure.
    #[pyo3(signature = (nal, *, pts, key_frame = false))]
    pub fn push_video(
        &self,
        py: Python<'_>,
        nal: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        key_frame: bool,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, nal)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_video(slice, rust_pts, key_frame));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push one KLV blob onto the lone configured KLV stream.
    /// `metadata_service_id` defaults to 0 (single-service case).
    /// Accepts any bytes-like input.
    #[pyo3(signature = (klv, *, pts, metadata_service_id = 0))]
    pub fn push_klv(
        &self,
        py: Python<'_>,
        klv: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        metadata_service_id: u8,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, klv)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_klv(slice, rust_pts, metadata_service_id));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push one audio frame onto the lone configured audio stream.
    /// Accepts any bytes-like input.
    #[pyo3(signature = (frames, *, pts))]
    pub fn push_audio(
        &self,
        py: Python<'_>,
        frames: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, frames)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_audio(slice, rust_pts));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push one subtitle payload onto the lone configured subtitle stream.
    /// Accepts any bytes-like input.
    #[pyo3(signature = (payload, *, pts))]
    pub fn push_subtitle(
        &self,
        py: Python<'_>,
        payload: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, payload)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_subtitle(slice, rust_pts));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push one data payload onto the lone configured data stream.
    /// Pass-through: lands verbatim as one PES packet on stream_id 0xBD.
    #[pyo3(signature = (data, *, pts))]
    pub fn push_data(
        &self,
        py: Python<'_>,
        data: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_data(slice, rust_pts));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    // ── Push surface — multi-stream variants ───────────────────────────────
    //
    // The `_to` variants take an explicit stream handle (obtained from
    // the matching `*_handle()`/`*_handles()` accessor). Use these when
    // the mount's MuxerConfig declares more than one stream of a given
    // kind — the single-stream methods above return `MOUNT` errors in
    // that case.

    /// Push to a specific video stream handle. Accepts bytes-like input.
    #[pyo3(signature = (handle, nal, *, pts, key_frame = false))]
    pub fn push_video_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyVideoStreamHandle>,
        nal: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        key_frame: bool,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, nal)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.inner
                .push_video_to(handle_inner, slice, rust_pts, key_frame)
        });
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push to a specific KLV stream handle. Accepts bytes-like input.
    #[pyo3(signature = (handle, klv, *, pts, metadata_service_id = 0))]
    pub fn push_klv_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyKlvStreamHandle>,
        klv: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        metadata_service_id: u8,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, klv)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| {
            self.inner
                .push_klv_to(handle_inner, slice, rust_pts, metadata_service_id)
        });
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push to a specific audio stream handle. Accepts bytes-like input.
    #[pyo3(signature = (handle, frames, *, pts))]
    pub fn push_audio_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyAudioStreamHandle>,
        frames: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, frames)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_audio_to(handle_inner, slice, rust_pts));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    /// Push to a specific subtitle stream handle. Accepts bytes-like input.
    #[pyo3(signature = (handle, payload, *, pts))]
    pub fn push_subtitle_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PySubtitleStreamHandle>,
        payload: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, payload)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_subtitle_to(handle_inner, slice, rust_pts));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    #[pyo3(signature = (handle, data, *, pts))]
    pub fn push_data_to(
        &self,
        py: Python<'_>,
        handle: PyRef<'_, PyDataStreamHandle>,
        data: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let rust_pts = py_pts90khz(pts)?;
        let handle_inner = handle.0;
        let coerced = crate::util::coerce_bytes_like(py, data)?;
        let slice = coerced.as_bytes();
        let res = py.allow_threads(|| self.inner.push_data_to(handle_inner, slice, rust_pts));
        res.map_err(|e| raise(py, &RTSP, BindingError::from(e)))
    }

    // ── Stream-handle accessors ────────────────────────────────────────────
    //
    // Return the first configured handle of each kind (single-stream
    // convenience). For multi-stream layouts, callers should use the
    // multi-handle Rust API; this is the Python equivalent of
    // `Muxer.video_stream_handle(0)`.

    /// Get the first configured video stream handle, or `None` if none
    /// declared.
    pub fn video_handle(&self) -> Option<PyVideoStreamHandle> {
        self.inner
            .video_handles()
            .into_iter()
            .next()
            .map(PyVideoStreamHandle)
    }

    /// Get the first configured KLV stream handle, or `None` if none
    /// declared.
    pub fn klv_handle(&self) -> Option<PyKlvStreamHandle> {
        self.inner
            .klv_handles()
            .into_iter()
            .next()
            .map(PyKlvStreamHandle)
    }

    /// Get the first configured audio stream handle, or `None` if none
    /// declared.
    pub fn audio_handle(&self) -> Option<PyAudioStreamHandle> {
        self.inner
            .audio_handles()
            .into_iter()
            .next()
            .map(PyAudioStreamHandle)
    }

    /// Get the first configured subtitle stream handle, or `None` if
    /// none declared.
    pub fn subtitle_handle(&self) -> Option<PySubtitleStreamHandle> {
        self.inner
            .subtitle_handles()
            .into_iter()
            .next()
            .map(PySubtitleStreamHandle)
    }

    /// Get the first configured data stream handle, or `None` if none
    /// declared.
    pub fn data_handle(&self) -> Option<PyDataStreamHandle> {
        self.inner
            .data_handles()
            .into_iter()
            .next()
            .map(PyDataStreamHandle)
    }

    /// All configured video stream handles.
    pub fn video_handles(&self) -> Vec<PyVideoStreamHandle> {
        self.inner
            .video_handles()
            .into_iter()
            .map(PyVideoStreamHandle)
            .collect()
    }

    /// All configured KLV stream handles.
    pub fn klv_handles(&self) -> Vec<PyKlvStreamHandle> {
        self.inner
            .klv_handles()
            .into_iter()
            .map(PyKlvStreamHandle)
            .collect()
    }

    /// All configured audio stream handles.
    pub fn audio_handles(&self) -> Vec<PyAudioStreamHandle> {
        self.inner
            .audio_handles()
            .into_iter()
            .map(PyAudioStreamHandle)
            .collect()
    }

    /// All configured subtitle stream handles.
    pub fn subtitle_handles(&self) -> Vec<PySubtitleStreamHandle> {
        self.inner
            .subtitle_handles()
            .into_iter()
            .map(PySubtitleStreamHandle)
            .collect()
    }

    /// All configured data stream handles.
    pub fn data_handles(&self) -> Vec<PyDataStreamHandle> {
        self.inner
            .data_handles()
            .into_iter()
            .map(PyDataStreamHandle)
            .collect()
    }

    // ── Lifecycle helpers ──────────────────────────────────────────────────

    /// Drain any TS packets queued in the inner muxer and broadcast them
    /// through the mount's fanout channel. Always safe to call.
    pub fn flush(&self, py: Python<'_>) {
        py.allow_threads(|| self.inner.flush());
    }

    /// Reset all flow counters on the mount to zero.
    pub fn reset_stats(&self, py: Python<'_>) {
        py.allow_threads(|| self.inner.reset_stats());
    }

    fn __repr__(&self) -> String {
        format!(
            "MountHandle(path={:?}, kind={:?}, peer_count={})",
            self.inner.mount_path(),
            self.mount_kind(),
            self.inner.peer_count(),
        )
    }
}

// ---------------------------------------------------------------------------
// PyRtspServer.
// ---------------------------------------------------------------------------

/// Sync RTSP server. Construct via [`RtspServer.start(config)`][PyRtspServer::start].
///
/// The underlying `tst_rtp::RtspServer` owns a tokio Runtime that lives
/// for the server's lifetime; `__exit__` (or explicit `.stop()`) sends
/// an RFC 7826 §13.5.1 Notice 5402 ("Server-Initiated TEARDOWN") to
/// each active session before closing.
///
/// Use as a context manager for guaranteed cleanup:
/// ```python
/// with RtspServer.start(cfg) as server:
///     mount = server.add_unicast_mount("/live", program)
///     ...
/// ```
#[pyclass(name = "RtspServer", module = "tstrans.rtp")]
pub struct PyRtspServer {
    /// `Arc` so `cancel_handle()` + `add_mount()` + `__exit__` can all
    /// hold references; the underlying Rust `RtspServer` is the unique
    /// owner of its tokio Runtime (dropped on the last Arc drop).
    inner: Arc<RustRtspServer>,
}

#[pymethods]
impl PyRtspServer {
    /// Build, bind, and start an RTSP server in one shot.
    ///
    /// `config` is a `tstrans.rtp.RtspServerConfig` dataclass instance.
    /// The Rust builder is fluent-constructed from the dataclass fields,
    /// then `build()` + `start()` are called. Errors during any of those
    /// surface as `RtspError(SERVER)` (start/bind failures), `RtspError(TLS)`
    /// (cert/key parse failures), or `RtspError(PROTOCOL)` (URL parse).
    ///
    /// GIL is released for the build + bind + spin-wait. Returns once the
    /// listener is bound (i.e. `local_addr()` is populated).
    #[staticmethod]
    pub fn start(py: Python<'_>, config: &Bound<'_, PyAny>) -> PyResult<Self> {
        let cfg = ServerConfigExtract::from_pyobj(config)?;
        // TLS guard for feature-less builds: raise a clear error up front
        // rather than letting the builder fail on an `rtsps://` bind (or
        // silently ignoring the cert paths).
        #[cfg(not(feature = "tls"))]
        if cfg.tls_cert.is_some() || cfg.tls_key.is_some() {
            return Err(make_rtsp_error(
                py,
                "TLS",
                "TLS (rtsps://) is not enabled in this build of tstrans; \
                 rebuild with the tst-py `tls` feature",
            ));
        }
        // Belt-and-suspenders path validation. tst-rtp's start() now
        // loads + validates the cert/key SYNCHRONOUSLY and fails typed
        // (the old listener-loads-after-start() wart is fixed), so this
        // guard is no longer load-bearing — it's kept for the friendlier
        // Python-side error messages (names the exact path, catches
        // directories/permissions distinctly).
        #[cfg(feature = "tls")]
        for path in [cfg.tls_cert.as_ref(), cfg.tls_key.as_ref()]
            .into_iter()
            .flatten()
        {
            match std::fs::File::open(path).and_then(|f| f.metadata()) {
                Ok(m) if m.is_file() => {}
                Ok(_) => {
                    return Err(raise(
                        py,
                        &RTSP,
                        BindingError::new(
                            BindingErrorKind::RtspTls,
                            format!("TLS path '{path}' is not a regular file"),
                        ),
                    ));
                }
                Err(e) => {
                    return Err(raise(
                        py,
                        &RTSP,
                        BindingError::new(
                            BindingErrorKind::RtspTls,
                            format!("cannot read TLS file '{path}': {e}"),
                        ),
                    ));
                }
            }
        }
        // Construction is fast but does real work (tokio Runtime build +
        // socket bind); release the GIL so other Python threads can run.
        let server = py
            .allow_threads(
                || -> Result<RustRtspServer, tst_rtp::error::RtspServerError> {
                    let mut builder = RtspServerBuilder::new(&cfg.bind_url)?;
                    builder
                        .max_sessions(cfg.max_sessions)
                        .session_timeout(Duration::from_secs(cfg.session_timeout_secs))
                        .fanout_capacity(cfg.fanout_capacity)
                        .graceful_shutdown_drain(Duration::from_millis(
                            cfg.graceful_shutdown_drain_ms,
                        ));
                    // Wire the cert + key paths through before build().
                    // start() loads these synchronously and fails typed
                    // on bad paths (the guard above just gives nicer
                    // Python-side messages).
                    #[cfg(feature = "tls")]
                    if let (Some(cert), Some(key)) = (cfg.tls_cert.as_ref(), cfg.tls_key.as_ref()) {
                        builder.tls_cert(
                            std::path::PathBuf::from(cert),
                            std::path::PathBuf::from(key),
                        );
                    }
                    if let Some(auth) = cfg.auth.as_ref() {
                        match auth.scheme {
                            AuthScheme::Basic => {
                                builder.auth_basic(
                                    &auth.realm,
                                    &auth.username,
                                    SecretString::new(auth.password.clone().into()),
                                );
                            }
                            AuthScheme::DigestMd5 => {
                                builder.auth_digest_md5(
                                    &auth.realm,
                                    &auth.username,
                                    SecretString::new(auth.password.clone().into()),
                                );
                            }
                            AuthScheme::DigestSha256 => {
                                builder.auth_digest_sha256(
                                    &auth.realm,
                                    &auth.username,
                                    SecretString::new(auth.password.clone().into()),
                                );
                            }
                        }
                    }
                    let server = builder.build()?;
                    server.start()?;
                    Ok(server)
                },
            )
            .map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;

        Ok(Self {
            inner: Arc::new(server),
        })
    }

    /// Register a unicast mount under `path`. The returned `MountHandle`
    /// is the push surface for that mount; cloning the handle is cheap
    /// so multiple producer threads can each hold one.
    ///
    /// Errors:
    /// - `RtspError(MOUNT)` for invalid path (empty, missing leading
    ///   slash, URL-reserved characters), duplicate path, or invalid
    ///   `program_config`.
    /// - `RtspError(SERVER)` if the server has been stopped.
    pub fn add_unicast_mount(
        &self,
        py: Python<'_>,
        path: &str,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<PyMountHandle> {
        let muxer_cfg = build_single_program_muxer_config(&program_config)?;
        let server = self.inner.clone();
        let path_owned = path.to_string();
        let res = py
            .allow_threads(move || server.add_mount(&path_owned, muxer_cfg))
            .map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;
        Ok(PyMountHandle { inner: res })
    }

    /// Register a multicast mount. `group` is a literal multicast IP
    /// (`"239.0.0.1"` / `"ff02::1"`); `port` is the destination UDP port.
    /// `ttl` defaults to 1 (link-local); `iface` may be set to a local
    /// IPv4 literal (`"192.168.1.50"`) or IPv6 interface name to pin
    /// outgoing traffic to a specific NIC.
    #[pyo3(signature = (path, group, port, *, ttl = 1, iface = None, program_config))]
    #[allow(clippy::too_many_arguments)] // 8 args is the Python signature.
    pub fn add_multicast_mount(
        &self,
        py: Python<'_>,
        path: &str,
        group: &str,
        port: u16,
        ttl: u8,
        iface: Option<&str>,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<PyMountHandle> {
        let muxer_cfg = build_single_program_muxer_config(&program_config)?;
        // Build the `rtp://<group>:<port>?ttl=N&iface=...` URL the Rust
        // API expects.
        let mut url = format!("rtp://{group}:{port}?ttl={ttl}");
        if let Some(i) = iface {
            url.push_str("&iface=");
            url.push_str(i);
        }
        let server = self.inner.clone();
        let path_owned = path.to_string();
        let res = py
            .allow_threads(move || server.add_multicast_mount(&path_owned, muxer_cfg, &url))
            .map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;
        Ok(PyMountHandle { inner: res })
    }

    /// Snapshot of aggregate server stats.
    pub fn stats(&self) -> PyServerStats {
        PyServerStats {
            inner: self.inner.stats(),
        }
    }

    /// Listener's bound address as `"ip:port"`, populated after `start()`.
    pub fn local_addr(&self) -> Option<String> {
        self.inner.local_addr().map(|a| a.to_string())
    }

    /// Graceful shutdown — fires the Notice 5402 path on each active
    /// session, then waits `drain_ms` for in-flight RTP to drain.
    ///
    /// Idempotent: a second call is a no-op. Calling after the server
    /// was never started raises `RtspError(SERVER)`.
    #[pyo3(signature = (*, drain_ms = 1000))]
    pub fn stop(&self, py: Python<'_>, drain_ms: u64) -> PyResult<()> {
        // The Rust API's drain window is set on the builder; `drain_ms`
        // here is an additional Python-side sleep cap (forward-looking;
        // the current builder default is 100 ms + a fixed 1 s in stop()
        // itself). We pass through to `stop()` and rely on the builder's
        // `graceful_shutdown_drain` for the actual wait window. The
        // `drain_ms` arg is accepted for API stability — future Rust
        // additions will route it through.
        let _ = drain_ms;
        let server = self.inner.clone();
        py.allow_threads(move || server.stop())
            .map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;
        Ok(())
    }

    /// Hard-cancel handle. Cloning is cheap; multiple holders can race
    /// the cancel call.
    pub fn cancel_handle(&self) -> PyRtspServerCancelHandle {
        PyRtspServerCancelHandle {
            inner: self.inner.cancel_handle(),
        }
    }

    // ── Context manager ────────────────────────────────────────────────────

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// `__exit__` calls `stop()` for graceful shutdown. Suppresses no
    /// exceptions (returns `None` / falsy so any in-block exception
    /// re-raises after cleanup).
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        exc_value: Option<&Bound<'_, PyAny>>,
        traceback: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let _ = (exc_type, exc_value, traceback);
        // Best-effort stop: if the server wasn't started (extremely rare
        // in a `with` block) or already shut down, swallow NotStarted.
        let server = self.inner.clone();
        match py.allow_threads(move || server.stop()) {
            Ok(_) => Ok(false),
            Err(tst_rtp::error::RtspServerError::NotStarted) => Ok(false),
            Err(e) => Err(raise(py, &RTSP, BindingError::from(e))),
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "RtspServer(local_addr={:?}, mounts={})",
            self.inner.local_addr().map(|a| a.to_string()),
            self.inner.stats().mounts,
        )
    }
}

// ---------------------------------------------------------------------------
// Internal — extract RtspServerConfig + auth + tls bytes from a Python
// dataclass.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum AuthScheme {
    Basic,
    DigestMd5,
    DigestSha256,
}

#[derive(Debug, Clone)]
struct AuthExtract {
    scheme: AuthScheme,
    realm: String,
    username: String,
    password: String,
}

#[derive(Debug, Clone)]
struct ServerConfigExtract {
    bind_url: String,
    max_sessions: usize,
    session_timeout_secs: u64,
    fanout_capacity: usize,
    graceful_shutdown_drain_ms: u64,
    auth: Option<AuthExtract>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

impl ServerConfigExtract {
    fn from_pyobj(obj: &Bound<'_, PyAny>) -> PyResult<Self> {
        let py = obj.py();
        let bind_addr: String = obj.getattr(intern!(py, "bind_addr"))?.extract()?;
        // The Rust builder takes an `rtsp://` URL with explicit scheme +
        // host:port. The dataclass uses `host:port` (e.g. "0.0.0.0:8554")
        // for ergonomics — prepend `rtsp://` here.
        let bind_url = if bind_addr.starts_with("rtsp://") || bind_addr.starts_with("rtsps://") {
            bind_addr
        } else {
            format!("rtsp://{bind_addr}")
        };
        let max_sessions: usize = obj.getattr(intern!(py, "max_sessions"))?.extract()?;
        let session_timeout_secs: u64 = obj
            .getattr(intern!(py, "session_timeout_secs"))?
            .extract()?;
        let fanout_capacity: usize = obj.getattr(intern!(py, "fanout_capacity"))?.extract()?;
        let graceful_shutdown_drain_ms: u64 = obj
            .getattr(intern!(py, "graceful_shutdown_drain_ms"))?
            .extract()?;

        let auth_obj = obj.getattr(intern!(py, "auth"))?;
        let auth = if auth_obj.is_none() {
            None
        } else {
            Some(extract_auth(&auth_obj)?)
        };

        let tls_cert = extract_optional_string(obj, "tls_cert")?;
        let tls_key = extract_optional_string(obj, "tls_key")?;

        Ok(Self {
            bind_url,
            max_sessions,
            session_timeout_secs,
            fanout_capacity,
            graceful_shutdown_drain_ms,
            auth,
            tls_cert,
            tls_key,
        })
    }
}

fn extract_optional_string(obj: &Bound<'_, PyAny>, attr: &str) -> PyResult<Option<String>> {
    let v = obj.getattr(attr)?;
    if v.is_none() {
        return Ok(None);
    }
    Ok(Some(v.extract()?))
}

fn extract_auth(obj: &Bound<'_, PyAny>) -> PyResult<AuthExtract> {
    use crate::rtp::client::{PyBasicAuth, PyDigestAlgorithm, PyDigestAuth};

    // Prefer Rust-side downcasting over Python-level getattr so we can
    // read the password without exposing it via a Python-visible getter.
    // The PyBasicAuth + PyDigestAuth pyclasses are `frozen`, so
    // `obj.downcast::<...>()` + `.borrow()` is sound and cheap.
    if let Ok(basic) = obj.downcast::<PyBasicAuth>() {
        let g = basic.borrow();
        let realm = g.realm.clone().ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(
                "BasicAuth used for server-side config requires realm=<str>",
            )
        })?;
        return Ok(AuthExtract {
            scheme: AuthScheme::Basic,
            realm,
            username: g.user.clone(),
            password: g.password.clone(),
        });
    }
    if let Ok(digest) = obj.downcast::<PyDigestAuth>() {
        let g = digest.borrow();
        let realm = g.realm.clone().ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(
                "DigestAuth used for server-side config requires realm=<str>",
            )
        })?;
        let scheme = match g.algorithm {
            PyDigestAlgorithm::SHA256 => AuthScheme::DigestSha256,
            PyDigestAlgorithm::MD5 => AuthScheme::DigestMd5,
        };
        return Ok(AuthExtract {
            scheme,
            realm,
            username: g.user.clone(),
            password: g.password.clone(),
        });
    }
    let py = obj.py();
    let cls_name: String = obj.get_type().getattr(intern!(py, "__name__"))?.extract()?;
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "RtspServerConfig.auth must be None, BasicAuth, or DigestAuth; got {cls_name}"
    )))
}

/// Wrap a `PyMuxerProgramConfig` in a single-program `MuxerConfig` and
/// return the inner Rust value. Mirrors what
/// `PyMuxerConfigBuilder.add_program(...).build()` does, condensed for
/// the common single-program server case.
fn build_single_program_muxer_config(
    program: &PyMuxerProgramConfig,
) -> PyResult<tst_core::mpegts::mux::MuxerConfig> {
    let mut builder = tst_core::mpegts::mux::MuxerConfig::builder();
    builder.add_program(program.inner.clone());
    builder
        .build()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid MuxerConfig: {e}")))
}
