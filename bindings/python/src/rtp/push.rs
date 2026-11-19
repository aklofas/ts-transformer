//! `PushDescription`, `RtspPushSession`, `RtspPushStats`: publishing to an
//! RTSP server.
//!
//! `RtspClient.push(config, description)` (in `client.rs`) runs
//! ANNOUNCE / SETUP (`mode=record`, interleaved) / RECORD through
//! `tst_rtp::RtspPush::connect` and returns an `RtspPushSession` that owns
//! the recording push.
//!
//! The session follows `RtspSession`'s layout: the push in a mutex, a clone
//! of its cancel outside it, a `closed` flag. Every call that can block
//! (a data call against a slow server, `pause` / `resume`, the end of the
//! session) takes the mutex only with the GIL released, and builds its
//! exception after the guard has dropped. A data call holds the mutex while
//! it waits for room in the push's queue; `cancel_handle()` never waits
//! for it, and a `close()` that finds such a call in progress fires the
//! cancel first so the waiting call ends. A getter holds the mutex only for
//! a moment: a `close()` that finds only a getter waits for it.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use tst_core::transport::{Transport, TransportCancel};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};
use tst_rtp::error::RtspError as RustRtspError;
use tst_rtp::rtsp::client::RtspCancelHandle as RustRtspCancelHandle;
use tst_rtp::{
    PushDescription as RustPushDescription, PushError, PushStats, RtspPush as RustRtspPush,
    VideoTrack,
};

use crate::mux::{PyMuxerProgramConfig, py_pts90khz};
use crate::raise::{RTP, RTSP, raise};
use crate::rtp::client::PyRtspCancelHandle;
use crate::shells::mux_sender::PyMuxSender;
use crate::shells::sender::PySender;
use crate::util::{CancelSource, allow_threads_parking, coerce_bytes_like};

// ---------------------------------------------------------------------------
// PushDescription
// ---------------------------------------------------------------------------

/// What a push announces: one MPEG-TS track, or one video track (H.264 or
/// H.265) with an optional KLV track. Frozen; built by its static
/// constructors.
#[pyclass(name = "PushDescription", module = "tstrans.rtp", frozen)]
pub struct PyPushDescription {
    pub(crate) inner: RustPushDescription,
}

impl PyPushDescription {
    fn elementary(video: VideoTrack, klv: bool) -> Self {
        Self {
            inner: RustPushDescription::Elementary { video, klv },
        }
    }

    /// Whether the description is the MPEG-TS one.
    pub(crate) fn is_mp2t(&self) -> bool {
        matches!(self.inner, RustPushDescription::Mp2t)
    }
}

#[pymethods]
impl PyPushDescription {
    /// One MPEG-TS track: send with `send_ts`, or convert the session into a
    /// `MuxSender` / `Sender`.
    #[staticmethod]
    fn mp2t() -> Self {
        Self {
            inner: RustPushDescription::Mp2t,
        }
    }

    /// An H.264 track described by `parameter_sets`: at least one SPS and
    /// one PPS, each one NAL unit without a start code (a NAL unit of
    /// another type is ignored). A set lacking one is refused by
    /// `RtspClient.push` with `RtspError(PROTOCOL)`. `klv` announces a KLV
    /// track beside the video.
    #[staticmethod]
    #[pyo3(signature = (parameter_sets, *, klv = false))]
    fn h264(parameter_sets: Vec<Vec<u8>>, klv: bool) -> Self {
        Self::elementary(VideoTrack::H264 { parameter_sets }, klv)
    }

    /// An H.264 track described by every SPS and PPS of `annexb`, one
    /// access unit in Annex B form (a key frame as an encoder emits it).
    /// Raises `ValueError` when it carries no SPS or no PPS.
    #[staticmethod]
    #[pyo3(signature = (annexb, *, klv = false))]
    fn h264_from_key_frame(annexb: &[u8], klv: bool) -> PyResult<Self> {
        let video = VideoTrack::h264_from_key_frame(annexb).map_err(|e| {
            PyValueError::new_err(format!("PushDescription.h264_from_key_frame: {e}"))
        })?;
        Ok(Self::elementary(video, klv))
    }

    /// An H.265 track described by its VPS, SPS and PPS, each one NAL unit
    /// without a start code.
    #[staticmethod]
    #[pyo3(signature = (vps, sps, pps, *, klv = false))]
    fn h265(vps: Vec<u8>, sps: Vec<u8>, pps: Vec<u8>, klv: bool) -> Self {
        Self::elementary(VideoTrack::H265 { vps, sps, pps }, klv)
    }

    /// An H.265 track described by the first VPS, SPS and PPS of `annexb`,
    /// one access unit in Annex B form. Raises `ValueError` when it lacks
    /// one of the three.
    #[staticmethod]
    #[pyo3(signature = (annexb, *, klv = false))]
    fn h265_from_key_frame(annexb: &[u8], klv: bool) -> PyResult<Self> {
        let video = VideoTrack::h265_from_key_frame(annexb).map_err(|e| {
            PyValueError::new_err(format!("PushDescription.h265_from_key_frame: {e}"))
        })?;
        Ok(Self::elementary(video, klv))
    }
}

// ---------------------------------------------------------------------------
// RtspPushStats
// ---------------------------------------------------------------------------

/// Counters of a push, over all its tracks. Frozen snapshot.
#[pyclass(name = "RtspPushStats", module = "tstrans.rtp", frozen)]
pub struct PyRtspPushStats {
    packets_sent: u64,
    bytes_sent: u64,
    sender_reports_sent: u64,
    receiver_reports: u64,
    fraction_lost_q8: u8,
    cumulative_lost: u32,
    frames_dropped_inbound: u64,
    frames_dropped_at_close: u64,
}

impl From<PushStats> for PyRtspPushStats {
    fn from(s: PushStats) -> Self {
        Self {
            packets_sent: s.packets_sent,
            bytes_sent: s.bytes_sent,
            sender_reports_sent: s.sender_reports_sent,
            receiver_reports: s.receiver_reports,
            fraction_lost_q8: s.fraction_lost_q8,
            cumulative_lost: s.cumulative_lost,
            frames_dropped_inbound: s.frames_dropped_inbound,
            frames_dropped_at_close: s.frames_dropped_at_close,
        }
    }
}

#[pymethods]
impl PyRtspPushStats {
    /// RTP packets handed to the client's thread.
    #[getter]
    fn packets_sent(&self) -> u64 {
        self.packets_sent
    }
    /// Bytes of those packets, RTP header included.
    #[getter]
    fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }
    /// RTCP sender reports handed to the client's thread.
    #[getter]
    fn sender_reports_sent(&self) -> u64 {
        self.sender_reports_sent
    }
    /// RTCP receiver reports the server sent.
    #[getter]
    fn receiver_reports(&self) -> u64 {
        self.receiver_reports
    }
    /// The fraction of packets lost, in 1/256, from the last receiver report.
    #[getter]
    fn fraction_lost_q8(&self) -> u8 {
        self.fraction_lost_q8
    }
    /// The packets lost since the start, from the same report.
    #[getter]
    fn cumulative_lost(&self) -> u32 {
        self.cumulative_lost
    }
    /// Frames the server sent on a channel this side sends media on.
    #[getter]
    fn frames_dropped_inbound(&self) -> u64 {
        self.frames_dropped_inbound
    }
    /// Frames still queued, never written, when the session ended.
    #[getter]
    fn frames_dropped_at_close(&self) -> u64 {
        self.frames_dropped_at_close
    }

    fn __repr__(&self) -> String {
        format!(
            "RtspPushStats(packets_sent={}, bytes_sent={}, sender_reports_sent={}, \
             receiver_reports={}, fraction_lost_q8={}, cumulative_lost={}, \
             frames_dropped_inbound={}, frames_dropped_at_close={})",
            self.packets_sent,
            self.bytes_sent,
            self.sender_reports_sent,
            self.receiver_reports,
            self.fraction_lost_q8,
            self.cumulative_lost,
            self.frames_dropped_inbound,
            self.frames_dropped_at_close,
        )
    }
}

// ---------------------------------------------------------------------------
// RtspPushSession
// ---------------------------------------------------------------------------

/// The push and the registration that lets interpreter exit cancel a call
/// parked in it. Dropping it de-registers the push (the registry holds a
/// weak reference).
struct Live {
    push: RustRtspPush,
    _exit: Arc<CancelSource>,
}

/// Why a data call failed: a binding error raised as `RtpError`, a
/// caller's mistake raised as `ValueError`, or a refusal this binding does
/// not know raised as `RuntimeError`. Built into a Python exception only
/// after the session's guard has dropped.
enum DataError {
    Binding(BindingError),
    Value(String),
    Runtime(String),
}

impl DataError {
    fn into_py(self, py: Python<'_>) -> PyErr {
        match self {
            Self::Binding(e) => raise(py, &RTP, e),
            Self::Value(m) => PyValueError::new_err(m),
            Self::Runtime(m) => PyRuntimeError::new_err(m),
        }
    }

    fn from_push(op: &str, e: PushError) -> Self {
        match e {
            PushError::Transport(t) => Self::Binding(BindingError::from(t)),
            // The caller's mistakes: nothing was sent.
            e @ (PushError::NoSuchTrack(_) | PushError::Empty(_)) => {
                Self::Value(format!("RtspPushSession.{op}: {e}"))
            }
            // A refusal added to the Rust push later: not the caller's
            // mistake as far as this binding knows.
            other => Self::Runtime(format!("RtspPushSession.{op}: {other}")),
        }
    }
}

/// One call that can park in the push (a data call, `pause`, `resume`),
/// counted in [`PyRtspPushSession::blocking`] from before it checks
/// `closed` until it has released the mutex.
struct Blocking<'a>(&'a AtomicUsize);

impl Drop for Blocking<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn closed_error(op: &str) -> BindingError {
    BindingError::new(
        BindingErrorKind::Closed,
        format!("RtspPushSession.{op}: the session is closed"),
    )
}

/// A recording RTSP session: what `RtspClient.push` returns.
///
/// Frozen: every method takes `&self`; the push is behind a mutex, so the
/// calls that need it run one at a time and a second thread's call waits,
/// GIL released, for the one in progress.
#[pyclass(name = "RtspPushSession", module = "tstrans.rtp", frozen)]
pub struct PyRtspPushSession {
    /// The push; `None` once `close()` / `teardown()` ended it or a
    /// conversion moved it into a sender shell. A data call holds the guard
    /// while it waits for room in the push's queue.
    live: Mutex<Option<Live>>,
    /// A clone of the push's cancel, outside the mutex, so
    /// `cancel_handle()` and a contended `close()` never wait.
    cancel: RustRtspCancelHandle,
    /// Set when `close()` / `teardown()` begins or a conversion takes the
    /// push: every later call is refused as CLOSED.
    closed: AtomicBool,
    /// Calls that can park in the push, in progress or waiting for the
    /// mutex. A `close()` that finds the mutex held fires the cancel first
    /// only when this is not 0; otherwise the holder is a getter, which
    /// lets go at once. Raised before such a call reads `closed`, and
    /// `close()` sets `closed` before it reads this (both `SeqCst`): a call
    /// that `close()` does not count sees `closed` and is refused.
    blocking: AtomicUsize,
    /// The session id the server named; it cannot change.
    session_id: String,
    /// Whether the push announced the MPEG-TS track.
    mp2t: bool,
}

/// A session collected without `close()`: the push's own drop, with the GIL
/// released, sets its cancel, discards what is queued and writes TEARDOWN
/// without waiting for the reply. Collection must not wait for the queue,
/// so it does not call `RtspPush::close`.
impl Drop for PyRtspPushSession {
    fn drop(&mut self) {
        let live = self
            .live
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(live) = live {
            crate::util::drop_blocking(move || drop(live));
        }
    }
}

impl PyRtspPushSession {
    /// Wrap a recording push. Holds the GIL: the exit registration needs it.
    pub(crate) fn new(push: RustRtspPush, mp2t: bool) -> Self {
        let cancel = push.cancel_handle();
        let exit = CancelSource::new(Arc::new(cancel.clone()));
        Self {
            session_id: push.session_id().to_owned(),
            cancel,
            closed: AtomicBool::new(false),
            blocking: AtomicUsize::new(0),
            mp2t,
            live: Mutex::new(Some(Live { push, _exit: exit })),
        }
    }

    /// Run a data call on the push with the GIL released. Raised in the RTP
    /// domain: a closed session is `RtpError(CLOSED)`.
    fn data(
        &self,
        py: Python<'_>,
        op: &'static str,
        call: impl FnOnce(&mut RustRtspPush) -> Result<(), DataError> + Send,
    ) -> PyResult<()> {
        let blocking = self.enter_blocking(py, &RTP, op)?;
        let result = allow_threads_parking(py, move || {
            // Declared first, dropped last: the count falls only after the
            // guard has released the mutex.
            let _blocking = blocking;
            let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(live) => call(&mut live.push),
                None => Err(DataError::Binding(closed_error(op))),
            }
        });
        result.map_err(|e| e.into_py(py))
    }

    /// Run a control request on the push with the GIL released. Raised in
    /// the RTSP domain: a closed session is `RtspError(CLOSED)`.
    fn control(
        &self,
        py: Python<'_>,
        op: &'static str,
        request: impl FnOnce(&mut RustRtspPush) -> Result<(), RustRtspError> + Send,
    ) -> PyResult<()> {
        let blocking = self.enter_blocking(py, &RTSP, op)?;
        let result = allow_threads_parking(py, move || {
            let _blocking = blocking;
            let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(live) => request(&mut live.push).map_err(BindingError::from),
                None => Err(closed_error(op)),
            }
        });
        result.map_err(|e| raise(py, &RTSP, e))
    }

    /// Count a call that can park, then refuse it as CLOSED in `domain` if
    /// the session is closed (the count is taken back first).
    fn enter_blocking(
        &self,
        py: Python<'_>,
        domain: &crate::raise::Domain,
        op: &'static str,
    ) -> PyResult<Blocking<'_>> {
        self.blocking.fetch_add(1, Ordering::SeqCst);
        let blocking = Blocking(&self.blocking);
        if self.closed.load(Ordering::SeqCst) {
            drop(blocking);
            return Err(raise(py, domain, closed_error(op)));
        }
        Ok(blocking)
    }

    /// Read from the push with the GIL released: the mutex can be held by a
    /// data call parked on another thread, and this waits for it. A closed
    /// session is `RtspError(CLOSED)`.
    fn read<T: Send>(
        &self,
        py: Python<'_>,
        op: &'static str,
        get: impl FnOnce(&RustRtspPush) -> T + Send,
    ) -> PyResult<T> {
        if self.closed.load(Ordering::Acquire) {
            return Err(raise(py, &RTSP, closed_error(op)));
        }
        let value = allow_threads_parking(py, || {
            let guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|live| get(&live.push))
        });
        value.ok_or_else(|| raise(py, &RTSP, closed_error(op)))
    }

    /// End the session: `RtspPush::close` (a last sender report per track,
    /// the queued media written, TEARDOWN under 500 ms, the client's thread
    /// stopped). `None` when the session was already closed, ended or
    /// converted. A call that can park, in progress on another thread, is
    /// cancelled first, so the close never waits behind it for long; a
    /// getter holding the mutex is waited for, and the close stays clean.
    fn end(&self, py: Python<'_>) -> Option<Result<(), RustRtspError>> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return None;
        }
        allow_threads_parking(py, || {
            let taken = match self.live.try_lock() {
                Ok(mut guard) => guard.take(),
                Err(TryLockError::WouldBlock) => {
                    if self.blocking.load(Ordering::SeqCst) > 0 {
                        self.cancel.cancel();
                    }
                    self.live.lock().unwrap_or_else(|e| e.into_inner()).take()
                }
                Err(TryLockError::Poisoned(e)) => e.into_inner().take(),
            };
            // The exit registration stays alive across the close, which can
            // wait for a slow server.
            taken.map(|Live { push, _exit }| push.close())
        })
    }

    /// Take the push for a conversion into a sender shell. Marks the
    /// session closed first, so a concurrent `close()` cannot cancel the
    /// push the shell is about to own.
    fn take_for_conversion(&self, py: Python<'_>, op: &'static str) -> PyResult<RustRtspPush> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Err(raise(py, &RTSP, closed_error(op)));
        }
        let taken = allow_threads_parking(py, || {
            self.live.lock().unwrap_or_else(|e| e.into_inner()).take()
        });
        match taken {
            Some(Live { push, .. }) => Ok(push),
            None => Err(raise(py, &RTSP, closed_error(op))),
        }
    }

    /// Refuse a conversion that would consume nothing useful: a closed
    /// session is `RtspError(CLOSED)`, an elementary push `ValueError`.
    fn refuse_conversion(&self, py: Python<'_>, op: &'static str) -> PyResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(raise(py, &RTSP, closed_error(op)));
        }
        if !self.mp2t {
            return Err(PyValueError::new_err(format!(
                "RtspPushSession.{op}: the push announced elementary streams; \
                 only an MPEG-TS push becomes a sender"
            )));
        }
        Ok(())
    }
}

/// The push as an opened shell transport; its cancel is taken before it is
/// boxed.
fn opened(push: RustRtspPush) -> Opened<Box<dyn Transport>> {
    let cancel: Arc<dyn TransportCancel> = Arc::new(push.cancel_handle());
    Opened::new(Box::new(push), cancel, Domain::Rtp)
}

#[pymethods]
impl PyRtspPushSession {
    /// Send whole TS packets as one RTP packet on an MPEG-TS push: a
    /// multiple of 188 bytes, at most 1316, each packet starting with 0x47.
    fn send_ts(&self, py: Python<'_>, ts: &Bound<'_, PyAny>) -> PyResult<()> {
        let coerced = coerce_bytes_like(py, ts)?;
        let ts = coerced.as_bytes();
        let mp2t = self.mp2t;
        let cancel = &self.cancel;
        self.data(py, "send_ts", |push| {
            // A cancelled push reports the cancel ahead of any refusal, as
            // the Rust push does.
            if !mp2t && !cancel.is_cancelled() {
                return Err(DataError::Value(
                    "RtspPushSession.send_ts: the push announced elementary \
                     streams; send with send_video / send_klv"
                        .to_owned(),
                ));
            }
            push.send_ts(ts)
                .map_err(|e| DataError::Binding(BindingError::from(e)))
        })
    }

    /// Send one video access unit (Annex B) with its presentation time.
    #[pyo3(signature = (annexb, *, pts))]
    fn send_video(
        &self,
        py: Python<'_>,
        annexb: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let pts = py_pts90khz(pts)?;
        let coerced = coerce_bytes_like(py, annexb)?;
        let annexb = coerced.as_bytes();
        self.data(py, "send_video", |push| {
            push.send_video(annexb, pts)
                .map_err(|e| DataError::from_push("send_video", e))
        })
    }

    /// Send one KLV unit with its presentation time.
    #[pyo3(signature = (klv, *, pts))]
    fn send_klv(
        &self,
        py: Python<'_>,
        klv: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let pts = py_pts90khz(pts)?;
        let coerced = coerce_bytes_like(py, klv)?;
        let klv = coerced.as_bytes();
        self.data(py, "send_klv", |push| {
            push.send_klv(klv, pts)
                .map_err(|e| DataError::from_push("send_klv", e))
        })
    }

    /// Pause the recording: the queued media is written, then PAUSE.
    fn pause(&self, py: Python<'_>) -> PyResult<()> {
        self.control(py, "pause", RustRtspPush::pause)
    }

    /// Send RECORD again on a paused push.
    fn resume(&self, py: Python<'_>) -> PyResult<()> {
        self.control(py, "resume", RustRtspPush::resume)
    }

    /// End the session as `close()` does and raise what the TEARDOWN
    /// returned. The session is closed afterwards whatever the outcome.
    fn teardown(&self, py: Python<'_>) -> PyResult<()> {
        match self.end(py) {
            None => Err(raise(py, &RTSP, closed_error("teardown"))),
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(raise(py, &RTSP, BindingError::from(e))),
        }
    }

    /// End the session; silent and idempotent.
    fn close(&self, py: Python<'_>) {
        let _ = self.end(py);
    }

    /// A handle on the push's cancel; answers at once, also while a send
    /// is parked on another thread.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<PyRtspCancelHandle> {
        if self.closed.load(Ordering::Acquire) {
            return Err(raise(py, &RTSP, closed_error("cancel_handle")));
        }
        Ok(PyRtspCancelHandle {
            inner: self.cancel.clone(),
        })
    }

    /// The push's counters now. Waits, GIL released, while a send,
    /// `pause()` or `resume()` on another thread is in progress (a parked
    /// send can stay parked for as long as the server takes a byte now and
    /// then); `cancel_handle()` never waits and is the way to end it.
    fn stats(&self, py: Python<'_>) -> PyResult<PyRtspPushStats> {
        self.read(py, "stats", RustRtspPush::stats)
            .map(PyRtspPushStats::from)
    }

    /// The session id the server named at SETUP.
    #[getter]
    fn session_id(&self) -> &str {
        &self.session_id
    }

    /// False once the session is known dead. Waits as `stats()` does while
    /// a call on another thread is in progress.
    fn is_session_alive(&self, py: Python<'_>) -> PyResult<bool> {
        self.read(py, "is_session_alive", RustRtspPush::is_session_alive)
    }

    /// Move the MPEG-TS push into a `MuxSender` for `program_config`.
    #[allow(clippy::wrong_self_convention)]
    fn into_mux_sender(
        &self,
        py: Python<'_>,
        program_config: PyRef<'_, PyMuxerProgramConfig>,
    ) -> PyResult<PyMuxSender> {
        self.refuse_conversion(py, "into_mux_sender")?;
        // Validate before the take: a refused config consumes nothing.
        let muxer_cfg = PyMuxSender::muxer_config_for(py, &program_config)?;
        let push = self.take_for_conversion(py, "into_mux_sender")?;
        PyMuxSender::from_opened_with_config(py, opened(push), muxer_cfg)
    }

    /// Move the MPEG-TS push into a `Sender`.
    #[allow(clippy::wrong_self_convention)]
    fn into_sender(&self, py: Python<'_>) -> PyResult<PySender> {
        self.refuse_conversion(py, "into_sender")?;
        let push = self.take_for_conversion(py, "into_sender")?;
        PySender::from_opened(py, opened(push))
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// `close()` on context-manager exit: silent; call `teardown()` first
    /// for the TEARDOWN's result.
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: &Bound<'_, PyAny>,
        exc_value: &Bound<'_, PyAny>,
        traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let _ = (exc_type, exc_value, traceback);
        self.close(py);
        Ok(false)
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPushDescription>()?;
    m.add_class::<PyRtspPushStats>()?;
    m.add_class::<PyRtspPushSession>()?;
    Ok(())
}
