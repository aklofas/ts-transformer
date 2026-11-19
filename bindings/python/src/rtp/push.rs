//! `PushDescription`, `RtspPushSession`, `RtspPushStats`: publishing to an
//! RTSP server.
//!
//! `RtspClient.push(config, description)` (in `client.rs`) runs
//! ANNOUNCE / SETUP (`mode=record`, interleaved) / RECORD through
//! `tst_rtp::RtspPush::connect` and returns an `RtspPushSession` that owns
//! the recording push.
//!
//! The session follows `RtspSession`'s layout: the push in a mutex, a clone
//! of its cancel outside it, a `closed` flag. Every call that takes the
//! mutex takes it with the GIL released and builds its exception after the
//! guard has dropped; `is_session_alive()` only tries it. A data call holds
//! the mutex while it waits for room in the push's queue; `cancel_handle()`
//! never waits for it.
//!
//! The close rule: a `close()` / `teardown()` fires the cancel first only
//! for a call that HOLDS the mutex and can park (a data call, `pause`,
//! `resume`; marked in `parking_holder` while it runs), so that call ends.
//! A call only waiting for the mutex is no reason to cancel: it gets the
//! mutex after the close began, runs nothing and raises CLOSED. A getter
//! holds the mutex only for a moment, and the close waits for it.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// another type is ignored), each any bytes-like object. A set lacking
    /// one is refused by `RtspClient.push` with `RtspError(PROTOCOL)`.
    /// `klv` announces a KLV track beside the video.
    #[staticmethod]
    #[pyo3(signature = (parameter_sets, *, klv = false))]
    fn h264(py: Python<'_>, parameter_sets: Vec<Bound<'_, PyAny>>, klv: bool) -> PyResult<Self> {
        let parameter_sets = parameter_sets
            .iter()
            .map(|set| owned_bytes(py, set))
            .collect::<PyResult<_>>()?;
        Ok(Self::elementary(VideoTrack::H264 { parameter_sets }, klv))
    }

    /// An H.264 track described by every SPS and PPS of `annexb`, one
    /// access unit in Annex B form (a key frame as an encoder emits it).
    /// Raises `ValueError` when it carries no SPS or no PPS.
    #[staticmethod]
    #[pyo3(signature = (annexb, *, klv = false))]
    fn h264_from_key_frame(py: Python<'_>, annexb: &Bound<'_, PyAny>, klv: bool) -> PyResult<Self> {
        let coerced = coerce_bytes_like(py, annexb)?;
        let video = VideoTrack::h264_from_key_frame(coerced.as_bytes()).map_err(|e| {
            PyValueError::new_err(format!("PushDescription.h264_from_key_frame: {e}"))
        })?;
        Ok(Self::elementary(video, klv))
    }

    /// An H.265 track described by its VPS, SPS and PPS, each one NAL unit
    /// without a start code, each any bytes-like object.
    #[staticmethod]
    #[pyo3(signature = (vps, sps, pps, *, klv = false))]
    fn h265(
        py: Python<'_>,
        vps: &Bound<'_, PyAny>,
        sps: &Bound<'_, PyAny>,
        pps: &Bound<'_, PyAny>,
        klv: bool,
    ) -> PyResult<Self> {
        let track = VideoTrack::H265 {
            vps: owned_bytes(py, vps)?,
            sps: owned_bytes(py, sps)?,
            pps: owned_bytes(py, pps)?,
        };
        Ok(Self::elementary(track, klv))
    }

    /// An H.265 track described by the first VPS, SPS and PPS of `annexb`,
    /// one access unit in Annex B form. Raises `ValueError` when it lacks
    /// one of the three.
    #[staticmethod]
    #[pyo3(signature = (annexb, *, klv = false))]
    fn h265_from_key_frame(py: Python<'_>, annexb: &Bound<'_, PyAny>, klv: bool) -> PyResult<Self> {
        let coerced = coerce_bytes_like(py, annexb)?;
        let video = VideoTrack::h265_from_key_frame(coerced.as_bytes()).map_err(|e| {
            PyValueError::new_err(format!("PushDescription.h265_from_key_frame: {e}"))
        })?;
        Ok(Self::elementary(video, klv))
    }
}

/// A copy of a bytes-like argument, as the send methods take one.
fn owned_bytes(py: Python<'_>, arg: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    Ok(coerce_bytes_like(py, arg)?.as_bytes().to_vec())
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

/// Lowers [`PyRtspPushSession::parking_holder`] when a call that can park
/// ends, however it ends. Declared after the mutex guard, so it drops
/// first: the marker is down before the mutex is released.
struct ParkingHolder<'a>(&'a AtomicBool);

impl Drop for ParkingHolder<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
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
/// GIL released, for the one in progress. `is_session_alive()`,
/// `cancel_handle()` and the conversions never wait. A `close()` or
/// `teardown()` fires the cancel first only when a send, `pause()` or
/// `resume()` holds the session; one still waiting for the session runs
/// nothing after the close began and raises `CLOSED`.
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
    /// push: every later call is refused as CLOSED, and a call that can
    /// park and gets the mutex after it runs nothing.
    closed: AtomicBool,
    /// Set while a data call, `pause` or `resume` holds the mutex and runs:
    /// raised after it has the mutex and has found `closed` clear, lowered
    /// before it lets go. A `close()` that finds the mutex held fires the
    /// cancel first only while this is set; getters and calls only waiting
    /// for the mutex never set it.
    parking_holder: AtomicBool,
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
            parking_holder: AtomicBool::new(false),
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
        self.refuse_if_closed(py, &RTP, op)?;
        match allow_threads_parking(py, || self.run_parking(call)) {
            Some(result) => result.map_err(|e| e.into_py(py)),
            None => Err(raise(py, &RTP, closed_error(op))),
        }
    }

    /// Run a control request on the push with the GIL released. Raised in
    /// the RTSP domain: a closed session is `RtspError(CLOSED)`.
    fn control(
        &self,
        py: Python<'_>,
        op: &'static str,
        request: impl FnOnce(&mut RustRtspPush) -> Result<(), RustRtspError> + Send,
    ) -> PyResult<()> {
        self.refuse_if_closed(py, &RTSP, op)?;
        let result = allow_threads_parking(py, || {
            self.run_parking(|push| request(push).map_err(BindingError::from))
        });
        match result {
            Some(result) => result.map_err(|e| raise(py, &RTSP, e)),
            None => Err(raise(py, &RTSP, closed_error(op))),
        }
    }

    /// Refuse a call as CLOSED in `domain`, with the GIL held, when the
    /// session is already closed.
    fn refuse_if_closed(
        &self,
        py: Python<'_>,
        domain: &crate::raise::Domain,
        op: &'static str,
    ) -> PyResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(raise(py, domain, closed_error(op)));
        }
        Ok(())
    }

    /// Run `call` on the push holding the mutex, for a call that can park;
    /// with the GIL released. `None`, nothing run, when a close, a teardown
    /// or a conversion began before this call got the mutex. While `call`
    /// runs, `parking_holder` is set, so a close cancels it.
    fn run_parking<R>(&self, call: impl FnOnce(&mut RustRtspPush) -> R) -> Option<R> {
        #[cfg(feature = "py-test-hooks")]
        probe::PARKING_WAITERS.fetch_add(1, Ordering::SeqCst);
        // A poisoned mutex (a panic in an earlier call, which that call
        // raised) is used as it is: the session stays usable, and a close
        // still ends the push.
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "py-test-hooks")]
        probe::PARKING_WAITERS.fetch_sub(1, Ordering::SeqCst);
        // A close stores `closed` and then tries the mutex; this call has
        // the mutex and then reads `closed` (both `SeqCst`). A call that
        // reads `false` holds the mutex, so the close sees it held and
        // re-reads the marker until it is set.
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        self.parking_holder.store(true, Ordering::SeqCst);
        let _holding = ParkingHolder(&self.parking_holder);
        guard.as_mut().map(|live| call(&mut live.push))
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
    /// converted. A call that holds the mutex and can park (a send against
    /// a slow server, `pause` / `resume`) is cancelled first, so the close
    /// waits only for it to notice. Anything else holding the mutex (a
    /// getter, a call that has just found `closed` set) is waited for
    /// without a cancel, and the close stays clean. A call only waiting for
    /// the mutex is not cancelled: it gets the mutex after this and runs
    /// nothing.
    fn end(&self, py: Python<'_>) -> Option<Result<(), RustRtspError>> {
        // A call that can park and gets the mutex from here on runs nothing.
        if self.closed.swap(true, Ordering::SeqCst) {
            return None;
        }
        allow_threads_parking(py, || {
            // A held mutex without the marker is a getter or a call that
            // found `closed` set, both of which let go at once: retry,
            // re-reading the marker every round, so a call that starts to
            // run meanwhile is still cancelled.
            let taken = loop {
                match self.live.try_lock() {
                    Ok(mut guard) => break guard.take(),
                    Err(TryLockError::WouldBlock) => {
                        #[cfg(feature = "py-test-hooks")]
                        probe::CLOSE_ROUNDS.fetch_add(1, Ordering::SeqCst);
                        if self.parking_holder.load(Ordering::SeqCst) {
                            self.cancel.cancel();
                            break self.live.lock().unwrap_or_else(|e| e.into_inner()).take();
                        }
                        std::thread::park_timeout(std::time::Duration::from_millis(1));
                    }
                    Err(TryLockError::Poisoned(e)) => break e.into_inner().take(),
                }
            };
            // The exit registration stays alive across the close, which can
            // wait for a slow server.
            taken.map(|Live { push, _exit }| push.close())
        })
    }

    /// Take the push for a conversion into a sender shell, never waiting:
    /// while another thread's call holds the session it raises
    /// `RuntimeError` and consumes nothing. With the mutex held it marks
    /// the session closed before the take, so a call that gets the mutex
    /// after it runs nothing and a concurrent `close()` does not cancel the
    /// push the shell is about to own.
    fn take_for_conversion(&self, py: Python<'_>, op: &'static str) -> PyResult<RustRtspPush> {
        enum Refused {
            Closed,
            Busy,
        }
        let taken = allow_threads_parking(py, || {
            let mut guard = match self.live.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                // A close that began holds no lock until the holder lets go:
                // report it as the close it is.
                Err(TryLockError::WouldBlock) if self.closed.load(Ordering::SeqCst) => {
                    return Err(Refused::Closed);
                }
                Err(TryLockError::WouldBlock) => return Err(Refused::Busy),
            };
            if self.closed.swap(true, Ordering::SeqCst) {
                return Err(Refused::Closed);
            }
            guard.take().ok_or(Refused::Closed)
        });
        match taken {
            Ok(Live { push, .. }) => Ok(push),
            Err(Refused::Closed) => Err(raise(py, &RTSP, closed_error(op))),
            Err(Refused::Busy) => Err(PyRuntimeError::new_err(format!(
                "RtspPushSession.{op}: another thread's call on the session is in \
                 progress; nothing was consumed"
            ))),
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
    /// `pause()` or `resume()` on another thread is in progress; a parked
    /// send can stay parked for as long as the server takes a byte now and
    /// then. `cancel_handle()` never waits, and firing it is how to end a
    /// parked send.
    fn stats(&self, py: Python<'_>) -> PyResult<PyRtspPushStats> {
        self.read(py, "stats", RustRtspPush::stats)
            .map(PyRtspPushStats::from)
    }

    /// The session id the server named at SETUP.
    #[getter]
    fn session_id(&self) -> &str {
        &self.session_id
    }

    /// False once the session is known dead (the control connection
    /// ended, or a keepalive ping was answered `454`) and once it is closed
    /// or converted. Never waits and never raises: a send, `pause()` or
    /// `resume()` in progress on another thread counts as live.
    fn is_session_alive(&self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        // Only tried, with the GIL held: a `try_lock` never waits for a
        // holder, so it never waits for one that released the GIL.
        match self.live.try_lock() {
            Ok(guard) => guard.as_ref().is_some_and(|l| l.push.is_session_alive()),
            Err(TryLockError::Poisoned(e)) => e
                .into_inner()
                .as_ref()
                .is_some_and(|l| l.push.is_session_alive()),
            Err(TryLockError::WouldBlock) => !self.closed.load(Ordering::SeqCst),
        }
    }

    /// Move an MPEG-TS push into a `MuxSender`; the session is closed
    /// afterwards. Raises `ValueError` on an elementary push,
    /// `MuxError(CONFIG_INVALID)` for a refused config and `RuntimeError`
    /// while another thread's call holds the session, each leaving the
    /// session as it was. The sender's `close()` cancels first; `finish()`
    /// cancels nothing.
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

    /// Move an MPEG-TS push into a `Sender`; the session is closed
    /// afterwards. Raises `ValueError` on an elementary push and
    /// `RuntimeError` while another thread's call holds the session, both
    /// leaving the session as it was. The sender's `close()` cancels
    /// first, and a `Sender` has no lossless end: for one, send with
    /// `send_ts` and call `teardown()`.
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
    #[cfg(feature = "py-test-hooks")]
    probe::register(m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test-only probes (feature = "py-test-hooks"), never in a published wheel and
// not re-exported by `tstrans.rtp`: hold a real session's lock the way a query
// call does until the test releases it, and count what a test must wait for
// (a send waiting for the lock; a close's rounds against a held lock), so a
// test can order a close against other calls without a timing race.
// ---------------------------------------------------------------------------

#[cfg(feature = "py-test-hooks")]
mod probe {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use pyo3::prelude::*;

    use super::PyRtspPushSession;

    /// Data calls, `pause` and `resume` waiting for the session's lock.
    pub(super) static PARKING_WAITERS: AtomicUsize = AtomicUsize::new(0);
    /// Rounds in which a close found the session's lock held.
    pub(super) static CLOSE_ROUNDS: AtomicUsize = AtomicUsize::new(0);
    static READER_HOLDS: AtomicBool = AtomicBool::new(false);
    static RELEASE: AtomicBool = AtomicBool::new(false);

    /// The longest a probe holds the lock unreleased: a test that never
    /// releases ends instead of hanging the suite.
    const HOLD_CAP: Duration = Duration::from_secs(30);

    /// `_push_hold_as_reader_for_test(session)`: hold the session's lock
    /// through the query path (as `stats()` does) until
    /// `_push_release_for_test()`, at most 30 s; resets the close-round
    /// count first. Raises `RtspError(CLOSED)` for a closed session.
    #[pyfunction]
    #[pyo3(name = "_push_hold_as_reader_for_test")]
    fn hold_as_reader(py: Python<'_>, session: &Bound<'_, PyRtspPushSession>) -> PyResult<()> {
        RELEASE.store(false, Ordering::SeqCst);
        CLOSE_ROUNDS.store(0, Ordering::SeqCst);
        session
            .get()
            .read(py, "_push_hold_as_reader_for_test", |_| {
                READER_HOLDS.store(true, Ordering::SeqCst);
                let start = Instant::now();
                while !RELEASE.load(Ordering::SeqCst) && start.elapsed() < HOLD_CAP {
                    std::thread::sleep(Duration::from_millis(1));
                }
                READER_HOLDS.store(false, Ordering::SeqCst);
            })
    }

    /// `_push_release_for_test()`: end the hold.
    #[pyfunction]
    #[pyo3(name = "_push_release_for_test")]
    fn release() {
        RELEASE.store(true, Ordering::SeqCst);
    }

    /// `_push_reader_holds_for_test()`: whether a hold has the lock now.
    #[pyfunction]
    #[pyo3(name = "_push_reader_holds_for_test")]
    fn reader_holds() -> bool {
        READER_HOLDS.load(Ordering::SeqCst)
    }

    /// `_push_parking_waiters_for_test()`.
    #[pyfunction]
    #[pyo3(name = "_push_parking_waiters_for_test")]
    fn parking_waiters() -> usize {
        PARKING_WAITERS.load(Ordering::SeqCst)
    }

    /// `_push_close_rounds_for_test()`.
    #[pyfunction]
    #[pyo3(name = "_push_close_rounds_for_test")]
    fn close_rounds() -> usize {
        CLOSE_ROUNDS.load(Ordering::SeqCst)
    }

    pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_function(wrap_pyfunction!(hold_as_reader, m)?)?;
        m.add_function(wrap_pyfunction!(release, m)?)?;
        m.add_function(wrap_pyfunction!(reader_holds, m)?)?;
        m.add_function(wrap_pyfunction!(parking_waiters, m)?)?;
        m.add_function(wrap_pyfunction!(close_rounds, m)?)?;
        Ok(())
    }
}
