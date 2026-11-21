//! `PushDescription`, `RtspPushSession`, `RtspPushStats`: publishing to an
//! RTSP server.
//!
//! `RtspClient.push(config, description)` (in `client.rs`) runs
//! ANNOUNCE / SETUP (`mode=record`, interleaved) / RECORD through
//! `tst_rtp::RtspPush::connect` and returns an `RtspPushSession` that owns
//! the recording push.
//!
//! The session holds the push in a `ParkingSlot`, which keeps the close
//! rule: a `close()` / `teardown()` fires the cancel first only for a call
//! that HOLDS the session and can park (a data call, `pause`, `resume`), so
//! that call ends; a call only waiting for the session runs nothing after
//! the close began and raises CLOSED; a conversion waits behind a query and
//! refuses only behind a call that can park; a panic in a call poisons the
//! session: every call that takes it is refused from then on, while
//! `close()` and `teardown()` still end the push and the reads below still
//! answer. Every call that takes
//! the slot takes it with the GIL released and builds its exception after
//! it let go; `is_session_alive()` only peeks.
//!
//! `stats()`, `end_reason()`, `end_detail()` and `close_outcome()` read the
//! push's `PushStatsHandle`, taken at build, so they never wait for a send
//! and answer after `close()`, `teardown()` and a conversion.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use tst_core::transport::{Transport, TransportCancel};
use tst_pipeline::binding::parking::{ParkingSlot, Peek, SlotRefused};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};
use tst_rtp::error::RtspError as RustRtspError;
use tst_rtp::rtsp::client::RtspCancelHandle as RustRtspCancelHandle;
use tst_rtp::{
    PushCloseOutcome, PushDescription as RustPushDescription, PushError, PushStatsHandle,
    RtspPush as RustRtspPush, VideoTrack,
};

use crate::generated::stats_rtp::RtspPushStats;

use crate::mux::{PyMuxerProgramConfig, py_pts90khz};
use crate::raise::{RTP, RTSP, raise};
use crate::rtp::client::PyRtspCancelHandle;
use crate::shells::end_reason::{end_detail, end_reason_to_py};
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

/// A copy of a bytes-like argument. A `bytes` object passed where the list
/// of parameter sets goes arrives here as its ints, which the helper
/// refuses with `TypeError`.
fn owned_bytes(py: Python<'_>, arg: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    Ok(coerce_bytes_like(py, arg)?.as_bytes().to_vec())
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

fn closed_error(op: &str) -> BindingError {
    BindingError::new(
        BindingErrorKind::Closed,
        format!("RtspPushSession.{op}: the session is closed"),
    )
}

fn poisoned_error(op: &str) -> BindingError {
    BindingError::new(
        BindingErrorKind::Closed,
        format!(
            "RtspPushSession.{op}: the session's lock was poisoned by a panic in an earlier call"
        ),
    )
}

/// A recording RTSP session: what `RtspClient.push` returns.
///
/// Frozen: every method takes `&self`; the push is in a `ParkingSlot`, so
/// the calls that need it run one at a time and a second thread's call
/// waits, GIL released, for the one in progress. `stats()`,
/// `end_reason()`, `end_detail()`, `is_session_alive()` and
/// `cancel_handle()` never wait, and the conversions never wait for a send,
/// `pause()` or `resume()` (they raise `RuntimeError`). A `close()` or
/// `teardown()` fires the cancel first only when a send, `pause()` or
/// `resume()` holds the session; one still waiting for the session runs
/// nothing after the close began and raises `CLOSED`.
#[pyclass(name = "RtspPushSession", module = "tstrans.rtp", frozen)]
pub struct PyRtspPushSession {
    /// The push; empty once `close()` / `teardown()` ended it or a
    /// conversion moved it into a sender shell. A data call holds the slot
    /// while it waits for room in the push's queue.
    slot: ParkingSlot<Live>,
    /// A clone of the push's cancel, outside the slot, so `cancel_handle()`
    /// never waits.
    cancel: RustRtspCancelHandle,
    /// The push's counters and end, readable without the slot and after
    /// the push is gone.
    stats: PushStatsHandle,
    /// The session id the server named; it cannot change.
    session_id: String,
    /// Whether the push announced the MPEG-TS track.
    mp2t: bool,
}

/// Whether a call on another thread holds the session. Test-only.
#[cfg(feature = "py-test-hooks")]
impl PyRtspPushSession {
    pub(crate) fn slot_held(&self) -> bool {
        matches!(self.slot.try_peek(|_| ()), Peek::Held)
    }
}

/// A session collected without `close()`: the push's own drop, with the GIL
/// released, sets its cancel, discards what is queued and writes TEARDOWN
/// without waiting for the reply. Collection must not wait for the queue,
/// so it does not call `RtspPush::close`.
impl Drop for PyRtspPushSession {
    fn drop(&mut self) {
        if let Some(live) = self.slot.take_for_close() {
            crate::util::drop_blocking(move || drop(live));
        }
    }
}

impl PyRtspPushSession {
    /// Wrap a recording push. Holds the GIL: the exit registration needs it.
    pub(crate) fn new(push: RustRtspPush, mp2t: bool) -> Self {
        let cancel = push.cancel_handle();
        let exit = CancelSource::new(Arc::new(cancel.clone()));
        let slot_cancel: Arc<dyn TransportCancel> = Arc::new(cancel.clone());
        let session_id = push.session_id().to_owned();
        let stats = push.stats_handle();
        let slot = ParkingSlot::new(Live { push, _exit: exit }, slot_cancel);
        #[cfg(feature = "py-test-hooks")]
        let slot = slot.with_observer(probe::observe);
        Self {
            slot,
            cancel,
            stats,
            session_id,
            mp2t,
        }
    }

    /// The exception for a call the slot refused, raised in `domain` (a
    /// poisoned session is `RtspError(CLOSED)` whatever the call).
    fn refused(
        &self,
        py: Python<'_>,
        domain: &crate::raise::Domain,
        op: &'static str,
        why: SlotRefused,
    ) -> PyErr {
        match why {
            SlotRefused::Poisoned => raise(py, &RTSP, poisoned_error(op)),
            SlotRefused::Busy => PyRuntimeError::new_err(format!(
                "RtspPushSession.{op}: a send, pause() or resume() on another thread \
                 is in progress; nothing was consumed"
            )),
            _ => raise(py, domain, closed_error(op)),
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
        match allow_threads_parking(py, || self.slot.run_parking(|live| call(&mut live.push))) {
            Ok(result) => result.map_err(|e| e.into_py(py)),
            Err(why) => Err(self.refused(py, &RTP, op, why)),
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
            self.slot
                .run_parking(|live| request(&mut live.push).map_err(BindingError::from))
        });
        match result {
            Ok(result) => result.map_err(|e| raise(py, &RTSP, e)),
            Err(why) => Err(self.refused(py, &RTSP, op, why)),
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
        if self.slot.is_closed() {
            return Err(raise(py, domain, closed_error(op)));
        }
        Ok(())
    }

    /// End the session: `RtspPush::close` (a last sender report per track,
    /// the queued media written, TEARDOWN under 500 ms, the client's thread
    /// stopped). `None` when the session was already closed, ended or
    /// converted. The slot cancels a call that holds the session and can
    /// park, waits for a query without a cancel, and lets a call only
    /// waiting for the session run nothing afterwards; a poisoned session
    /// still gives its push up.
    fn end(&self, py: Python<'_>) -> Option<Result<(), RustRtspError>> {
        allow_threads_parking(py, || {
            // The exit registration stays alive across the close, which can
            // wait for a slow server.
            self.slot
                .take_for_close()
                .map(|Live { push, _exit }| push.close())
        })
    }

    /// Take the push for a conversion into a sender shell. Never waits for
    /// a call that may park: while a send, `pause` or `resume` holds the
    /// session it raises `RuntimeError` and consumes nothing. A query
    /// holding the session is waited for. A refused conversion leaves the
    /// session as it was, a call that gets the session after it runs
    /// nothing, and a concurrent `close()` does not cancel the push the
    /// shell is about to own.
    fn take_for_conversion(&self, py: Python<'_>, op: &'static str) -> PyResult<RustRtspPush> {
        match allow_threads_parking(py, || self.slot.take_for_conversion()) {
            Ok(Live { push, .. }) => Ok(push),
            Err(why) => Err(self.refused(py, &RTSP, op, why)),
        }
    }

    /// Refuse a conversion that would consume nothing useful: a closed
    /// session is `RtspError(CLOSED)`, an elementary push `ValueError`.
    fn refuse_conversion(&self, py: Python<'_>, op: &'static str) -> PyResult<()> {
        if self.slot.is_closed() {
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

    /// End the session as `close()` does and raise what it reports, in this
    /// order: `RtspError(CLOSED)` on a cancelled push (and on a session
    /// already closed); otherwise `PROTOCOL` with status 454 when queued
    /// media was lost to an expired session; otherwise the TEARDOWN's own
    /// result: `PROTOCOL` (or `NOT_FOUND` for a 404) when the server
    /// refused it, `TIMEOUT` when no reply came within the 500 ms and `IO`
    /// when the connection had ended. Returns `None` only when the queued
    /// media was written and the server accepted the TEARDOWN. The session
    /// is closed after it, whatever the outcome.
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
        match self.slot.try_peek(|_| ()) {
            Peek::Value(()) | Peek::Held => Ok(PyRtspCancelHandle {
                inner: self.cancel.clone(),
            }),
            // A peek reads a poisoned session as closed; the slot's own
            // flag tells the two apart.
            _ if self.slot.is_closed() => Err(raise(py, &RTSP, closed_error("cancel_handle"))),
            _ => Err(raise(py, &RTSP, poisoned_error("cancel_handle"))),
        }
    }

    /// The push's counters now. Never waits: read without the session, so
    /// a send parked on another thread does not hold it up; after
    /// `close()`, `teardown()` or a conversion, the last values (a converted
    /// push's sends included). `frames_dropped_at_close` counts media a
    /// close discarded; `teardown()` is the call that reports such a loss.
    fn stats(&self) -> RtspPushStats {
        RtspPushStats::from(&self.stats.stats())
    }

    /// Why the push ended, as a `tstrans.shells.EndReason`, or `None` while
    /// it has not. Never waits; kept after `close()` and after a
    /// conversion (it follows the sender that owns the push).
    fn end_reason(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        match self.stats.end_reason() {
            Some(r) => end_reason_to_py(py, &r),
            None => Ok(None),
        }
    }

    /// Free-text detail for `end_reason()`: the `msg` carried by
    /// `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`, else `None`.
    fn end_detail(&self) -> Option<String> {
        let reason = self.stats.end_reason()?;
        end_detail(&reason).map(str::to_owned)
    }

    /// How the push's close ended, as a `PushCloseOutcome`, or `None` until
    /// a close has finished: this session's `close()` or `teardown()`, or
    /// the close a sender shell that took the push runs when it is
    /// finished or closed; from then on the first close's outcome. Never
    /// waits: read without the session, so it answers `None` while a send
    /// is parked or a close is still running, and it answers after
    /// `close()` and after a conversion. After a sender's `finish()` it
    /// says how the TEARDOWN went; a sender's `close()` cancels first, so
    /// it reads `CANCELLED`.
    fn close_outcome(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        match self.stats.close_outcome() {
            Some(outcome) => close_outcome_to_py(py, &outcome).map(Some),
            None => Ok(None),
        }
    }

    /// The session id the server named at SETUP.
    #[getter]
    fn session_id(&self) -> &str {
        &self.session_id
    }

    /// False once the session is known dead (the control connection
    /// ended, or a keepalive ping was answered `454`), once it is closed
    /// or converted, and once a panic poisoned it. Never waits and never
    /// raises: a send, `pause()` or `resume()` in progress on another
    /// thread counts as live.
    fn is_session_alive(&self) -> bool {
        // Only peeked, with the GIL held: a peek never waits for a holder,
        // so it never waits for one that released the GIL.
        match self.slot.try_peek(|l| l.push.is_session_alive()) {
            Peek::Value(alive) => alive,
            Peek::Held => true,
            _ => false,
        }
    }

    /// Move an MPEG-TS push into a `MuxSender`; the session is closed
    /// afterwards. Raises `ValueError` on an elementary push,
    /// `MuxError(CONFIG_INVALID)` for a refused config and `RuntimeError`
    /// while a send, `pause()` or `resume()` on another thread holds the
    /// session, each leaving the session as it was; an `is_session_alive()`
    /// peeking at it is waited for. The sender's `close()` cancels first;
    /// `finish()` cancels nothing and ends without loss. `stats()`,
    /// `end_reason()` and `close_outcome()` keep answering on this session,
    /// for the push the sender now owns; after the sender's `finish()`,
    /// `close_outcome()` says how the TEARDOWN went.
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
    /// `RuntimeError` while a send, `pause()` or `resume()` on another
    /// thread holds the session, both leaving the session as it was; an
    /// `is_session_alive()` peeking at it is waited for. The sender's
    /// `close()` cancels first; its `finish()` cancels nothing, writes
    /// what the sender and the push hold, then sends TEARDOWN: the end
    /// without loss. `stats()`, `end_reason()` and `close_outcome()` keep
    /// answering on this session, for the push the sender now owns; after
    /// the sender's `finish()`, `close_outcome()` says how the TEARDOWN
    /// went.
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

/// `outcome` as a `tstrans.rtp.PushCloseOutcome`. `PushCloseOutcome` is
/// `#[non_exhaustive]`: a case this binding does not know yet reads
/// `FAILED` with its debug text as the detail.
fn close_outcome_to_py(py: Python<'_>, outcome: &PushCloseOutcome) -> PyResult<Py<PyAny>> {
    let (kind, status, detail) = match outcome {
        PushCloseOutcome::Clean => ("CLEAN", None, String::new()),
        PushCloseOutcome::Cancelled => ("CANCELLED", None, String::new()),
        PushCloseOutcome::MediaLost => ("MEDIA_LOST", None, String::new()),
        PushCloseOutcome::TeardownRefused { code, reason } => {
            ("TEARDOWN_REFUSED", Some(*code), reason.clone())
        }
        PushCloseOutcome::TeardownTimedOut => ("TEARDOWN_TIMED_OUT", None, String::new()),
        PushCloseOutcome::Failed { detail } => ("FAILED", None, detail.clone()),
        other => ("FAILED", None, format!("{other:?}")),
    };
    let rtp = py.import(intern!(py, "tstrans.rtp"))?;
    let kind = rtp.getattr(intern!(py, "PushCloseKind"))?.getattr(kind)?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("kind", kind)?;
    kwargs.set_item("status", status)?;
    kwargs.set_item("detail", detail)?;
    Ok(rtp
        .getattr(intern!(py, "PushCloseOutcome"))?
        .call((), Some(&kwargs))?
        .unbind())
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPushDescription>()?;
    m.add_class::<PyRtspPushSession>()?;
    #[cfg(feature = "py-test-hooks")]
    probe::register(m)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Test-only probes (feature = "py-test-hooks"), never in a published wheel and
// not re-exported by `tstrans.rtp`: hold a real session's lock the way a query
// call does until the test releases it, poison it, and count what a test must
// wait for (a send waiting for the lock; a close's or a conversion's rounds
// against a held lock), so a test can order a close or a conversion against
// other calls without a timing race.
// ---------------------------------------------------------------------------

#[cfg(feature = "py-test-hooks")]
mod probe {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use pyo3::prelude::*;
    use tst_pipeline::binding::parking::ParkingEvent;

    use super::PyRtspPushSession;
    use crate::raise::RTSP;
    use crate::util::allow_threads_parking;

    /// Data calls, `pause` and `resume` waiting for the session's lock.
    static PARKING_WAITERS: AtomicUsize = AtomicUsize::new(0);
    /// Rounds in which a close found the session's lock held by a query.
    static CLOSE_ROUNDS: AtomicUsize = AtomicUsize::new(0);
    /// Rounds in which a conversion found the session's lock held by a query.
    static CONVERSION_ROUNDS: AtomicUsize = AtomicUsize::new(0);
    static READER_HOLDS: AtomicBool = AtomicBool::new(false);
    static RELEASE: AtomicBool = AtomicBool::new(false);

    /// The longest a probe holds the lock unreleased: a test that never
    /// releases ends instead of hanging the suite.
    const HOLD_CAP: Duration = Duration::from_secs(30);

    /// The session slot's observer.
    pub(super) fn observe(event: ParkingEvent) {
        match event {
            ParkingEvent::ParkingEntered => {
                PARKING_WAITERS.fetch_add(1, Ordering::SeqCst);
            }
            ParkingEvent::ParkingLeft => {
                PARKING_WAITERS.fetch_sub(1, Ordering::SeqCst);
            }
            ParkingEvent::CloseRetry => {
                CLOSE_ROUNDS.fetch_add(1, Ordering::SeqCst);
            }
            ParkingEvent::ConversionRetry => {
                CONVERSION_ROUNDS.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    /// `_push_hold_as_reader_for_test(session)`: hold the session's lock
    /// as a query does until `_push_release_for_test()`, at most 30 s;
    /// resets the close and conversion round counts first. Raises
    /// `RtspError(CLOSED)` for a closed session.
    #[pyfunction]
    #[pyo3(name = "_push_hold_as_reader_for_test")]
    fn hold_as_reader(py: Python<'_>, session: &Bound<'_, PyRtspPushSession>) -> PyResult<()> {
        RELEASE.store(false, Ordering::SeqCst);
        CLOSE_ROUNDS.store(0, Ordering::SeqCst);
        CONVERSION_ROUNDS.store(0, Ordering::SeqCst);
        let s = session.get();
        allow_threads_parking(py, || {
            s.slot.run(|_| {
                READER_HOLDS.store(true, Ordering::SeqCst);
                let start = Instant::now();
                while !RELEASE.load(Ordering::SeqCst) && start.elapsed() < HOLD_CAP {
                    std::thread::sleep(Duration::from_millis(1));
                }
                READER_HOLDS.store(false, Ordering::SeqCst);
            })
        })
        .map_err(|why| s.refused(py, &RTSP, "_push_hold_as_reader_for_test", why))
    }

    /// `_push_poison_for_test(session)`: panic inside a query on the
    /// session, as a bug in a call would, so its lock is poisoned.
    #[pyfunction]
    #[pyo3(name = "_push_poison_for_test")]
    fn poison(py: Python<'_>, session: &Bound<'_, PyRtspPushSession>) {
        let s = session.get();
        let _ = allow_threads_parking(py, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s.slot.run(|_| panic!("_push_poison_for_test"))
            }))
        });
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

    /// `_push_conversion_rounds_for_test()`.
    #[pyfunction]
    #[pyo3(name = "_push_conversion_rounds_for_test")]
    fn conversion_rounds() -> usize {
        CONVERSION_ROUNDS.load(Ordering::SeqCst)
    }

    pub(super) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_function(wrap_pyfunction!(hold_as_reader, m)?)?;
        m.add_function(wrap_pyfunction!(poison, m)?)?;
        m.add_function(wrap_pyfunction!(release, m)?)?;
        m.add_function(wrap_pyfunction!(reader_holds, m)?)?;
        m.add_function(wrap_pyfunction!(parking_waiters, m)?)?;
        m.add_function(wrap_pyfunction!(close_rounds, m)?)?;
        m.add_function(wrap_pyfunction!(conversion_rounds, m)?)?;
        Ok(())
    }
}
