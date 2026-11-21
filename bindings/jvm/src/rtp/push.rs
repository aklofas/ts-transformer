//! `org.tstrans.rtp` RTSP publish surface: `RtspClient.nPush`, `PushDescription`'s
//! key-frame readers and `RtspPushSession`.
//!
//! The session's push sits in a [`ParkingSlot`], which holds the close rule: every
//! data and control call runs holding the slot (a data call parks there while the
//! server is slow); a close fires the push's cancel first only when a call that can
//! park on the network holds it; a query holding it for a moment, and a call only
//! waiting for it, are not cancelled, and the waiting call then runs nothing. A
//! conversion waits behind a query and refuses behind a call that can park. A panic
//! inside a call poisons the slot: every later call that takes the session throws
//! `RtspException(CLOSED)`, and a close still ends the push.
//!
//! The counters, the end reason and the close outcome are read through the push's
//! `PushStatsHandle`, never the slot, so they answer while a send is parked. The Java
//! object holds the handle under a key of its own (`nStatsKey`), kept past a close or a
//! conversion until its `close()`, so `stats()` answers after both.

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::sync::{Arc, LazyLock};

use jni::objects::{JBooleanArray, JByteArray, JIntArray, JObject, JObjectArray, JString, JValue};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong, jobject, jobjectArray, jstring};
use jni::{Env, jni_sig, jni_str};

use tst_core::mpegts::common::Pts90khz;
use tst_core::transport::{Transport, TransportCancel, TransportError};
use tst_pipeline::binding::parking::{ParkingSlot, Peek, SlotRefused};
use tst_pipeline::binding::{BindingErrorKind, Domain, Opened};
use tst_rtp::rtsp::client::RtspCancelHandle as RustRtspCancel;
use tst_rtp::{
    PushCloseOutcome, PushDescription, PushError, PushStatsHandle, RtspPush, VideoTrack,
};

use crate::handle::HandleRegistry;
use crate::mpegts::muxer::build_muxer_config_from_arrays;
use crate::shells;
use crate::shells::end_reason::build_close_snapshot;

use super::client::{JniRtspCancel, client_builder};
use super::errors::{
    rtsp_error_to_jvm, throw_rtp, throw_rtsp, throw_runtime_unless_pending, transport_error,
};

/// `PushDescription.KIND_*`: the description's kind as the Java side passes it.
const KIND_MP2T: jint = 0;
const KIND_H264: jint = 1;
const KIND_H265: jint = 2;

const WHAT: &str = "RtspPushSession";

/// What a call reports when a close or a conversion on another thread took the push
/// while the call waited for it.
const TAKEN: &str = "the session was closed during the call";

/// What every call that takes the session reports once a panic poisoned it.
const POISONED: &str =
    "RtspPushSession: the session's lock was poisoned by a panic in an earlier call";

/// What a conversion reports when a send, `pause` or `resume` on another thread holds
/// the push.
const IN_PROGRESS: &str = "a send, pause or resume on another thread is in progress on this \
     RtspPushSession; nothing was converted";

/// Native backing for `org.tstrans.rtp.RtspPushSession`.
struct JniRtspPush {
    slot: Arc<ParkingSlot<RtspPush>>,
    /// Read at RECORD; a clone goes to each `nStatsKey`.
    stats: PushStatsHandle,
    /// The push's cancel, read at RECORD and kept outside the slot, which a parked
    /// send holds.
    cancel: RustRtspCancel,
    /// The session id, read at RECORD: it never changes.
    session_id: String,
    /// Announced as MPEG-TS: the only shape `sendTs` and the conversions take.
    mp2t: bool,
}

/// Per-type leased-handle registry for `org.tstrans.rtp.RtspPushSession`. No cancel
/// hook: the slot fires the cancel itself (see the module doc).
static REGISTRY_PUSH: LazyLock<HandleRegistry<JniRtspPush>> = LazyLock::new(HandleRegistry::new);

/// The counters' and end reason's reader of each Java session object, under the key
/// `nStatsKey` gave it; freed by `nFreeStats` from the object's `close()`.
static REGISTRY_PUSH_STATS: LazyLock<HandleRegistry<PushStatsHandle>> =
    LazyLock::new(HandleRegistry::new);

fn throw_illegal_argument(env: &mut Env, msg: &str) {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalArgumentException")),
        JNIString::from(msg),
    );
}

fn throw_illegal_state(env: &mut Env, msg: &str) {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalStateException")),
        JNIString::from(msg),
    );
}

fn throw_runtime(env: &mut Env, msg: String) {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/RuntimeException")),
        JNIString::from(msg),
    );
}

/// A copy of a Java `byte[]`; `None` with a `RuntimeException` pending when JNI
/// cannot read it.
fn read_bytes(env: &mut Env, arr: &JByteArray<'_>) -> Option<Vec<u8>> {
    match env.convert_byte_array(arr) {
        Ok(b) => Some(b),
        Err(e) => {
            throw_runtime_unless_pending(env, e.to_string());
            None
        }
    }
}

/// Every element of a Java `byte[][]`, copied; a null array is empty. `None` with a
/// pending exception for a null element or an array JNI cannot read.
fn read_byte_arrays(env: &mut Env, arr: &JObjectArray<'_>) -> Option<Vec<Vec<u8>>> {
    if arr.is_null() {
        return Some(Vec::new());
    }
    let len = match arr.len(env) {
        Ok(n) => n,
        Err(e) => {
            throw_runtime_unless_pending(env, e.to_string());
            return None;
        }
    };
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        // One frame per element so the array locals do not pile up.
        let bytes = env.with_local_frame(4, |env| -> jni::errors::Result<Option<Vec<u8>>> {
            let elem = arr.get_element(env, i)?;
            if elem.is_null() {
                return Ok(None);
            }
            let elem = env.cast_local::<JByteArray>(elem)?;
            Ok(Some(env.convert_byte_array(&elem)?))
        });
        match bytes {
            Ok(Some(b)) => out.push(b),
            Ok(None) => {
                let _ = env.throw_new(
                    FindClass(jni_str!("java/lang/NullPointerException")),
                    JNIString::from("parameter set"),
                );
                return None;
            }
            Err(e) => {
                throw_runtime_unless_pending(env, e.to_string());
                return None;
            }
        }
    }
    Some(out)
}

/// A Java `byte[][]` holding `sets`, or null with a pending exception.
fn byte_arrays(env: &mut Env, sets: &[&[u8]]) -> jobjectArray {
    let build = |env: &mut Env| -> jni::errors::Result<jobjectArray> {
        let out = env.new_object_array(
            sets.len() as i32,
            FindClass(jni_str!("[B")),
            JObject::null(),
        )?;
        for (i, set) in sets.iter().enumerate() {
            env.with_local_frame(4, |inner| -> jni::errors::Result<()> {
                let arr = inner.byte_array_from_slice(set)?;
                out.set_element(inner, i, &arr)
            })?;
        }
        Ok(out.into_raw())
    };
    match build(env) {
        Ok(a) => a,
        Err(e) => {
            throw_runtime_unless_pending(env, e.to_string());
            std::ptr::null_mut()
        }
    }
}

jni_native! {
    /// `PushDescription.nH264ParameterSets(annexB)`: every SPS and PPS of the access
    /// unit, or `IllegalArgumentException` when it carries no SPS or no PPS.
    fn Java_org_tstrans_rtp_PushDescription_nH264ParameterSets<'local>(
        env,
        _class,
        annexb: JByteArray<'local>,
    ) -> jobjectArray = std::ptr::null_mut() => {
        let Some(bytes) = read_bytes(env, &annexb) else {
            return std::ptr::null_mut();
        };
        match VideoTrack::h264_from_key_frame(&bytes) {
            Ok(VideoTrack::H264 { parameter_sets }) => {
                let sets: Vec<&[u8]> = parameter_sets.iter().map(Vec::as_slice).collect();
                byte_arrays(env, &sets)
            }
            Ok(_) => {
                throw_runtime(env, "h264_from_key_frame returned another codec".to_string());
                std::ptr::null_mut()
            }
            Err(e) => {
                throw_illegal_argument(env, &e.to_string());
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `PushDescription.nH265ParameterSets(annexB)`: the first VPS, SPS and PPS of the
    /// access unit, or `IllegalArgumentException` when it lacks one.
    fn Java_org_tstrans_rtp_PushDescription_nH265ParameterSets<'local>(
        env,
        _class,
        annexb: JByteArray<'local>,
    ) -> jobjectArray = std::ptr::null_mut() => {
        let Some(bytes) = read_bytes(env, &annexb) else {
            return std::ptr::null_mut();
        };
        match VideoTrack::h265_from_key_frame(&bytes) {
            Ok(VideoTrack::H265 { vps, sps, pps }) => byte_arrays(env, &[&vps, &sps, &pps]),
            Ok(_) => {
                throw_runtime(env, "h265_from_key_frame returned another codec".to_string());
                std::ptr::null_mut()
            }
            Err(e) => {
                throw_illegal_argument(env, &e.to_string());
                std::ptr::null_mut()
            }
        }
    }
}

/// The description `kind`, `klv` and `sets` name; `None` with an
/// `IllegalArgumentException` pending for a kind or a set count the Java side never
/// passes.
fn description(
    env: &mut Env,
    kind: jint,
    klv: bool,
    sets: Vec<Vec<u8>>,
) -> Option<PushDescription> {
    let video = match kind {
        KIND_MP2T => return Some(PushDescription::Mp2t),
        KIND_H264 => VideoTrack::H264 {
            parameter_sets: sets,
        },
        KIND_H265 => {
            let Ok::<[Vec<u8>; 3], _>([vps, sps, pps]) = sets.try_into() else {
                throw_illegal_argument(env, "an H.265 description holds a VPS, an SPS and a PPS");
                return None;
            };
            VideoTrack::H265 { vps, sps, pps }
        }
        other => {
            throw_illegal_argument(env, &format!("unknown push description kind {other}"));
            return None;
        }
    };
    Some(PushDescription::Elementary { video, klv })
}

jni_native! {
    /// `RtspClient.nPush(url, authUser, authPassword, digestOnly, keepalive,
    /// tlsRootCertsPem, keepaliveIntervalMs, connectTimeoutMs, requestTimeoutMs,
    /// userAgent, kind, klv, parameterSets)`: connect, ANNOUNCE, SETUP every
    /// track for recording and RECORD (`RtspPush::connect`); the session's key, or 0
    /// with a pending `RtspException`.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_rtp_RtspClient_nPush<'local>(
        env,
        _class,
        url: JString<'local>,
        auth_user: JString<'local>,
        auth_password: JString<'local>,
        digest_only: jboolean,
        keepalive: jboolean,
        tls_root_certs_pem: JByteArray<'local>,
        keepalive_interval_ms: jlong,
        connect_timeout_ms: jlong,
        request_timeout_ms: jlong,
        user_agent: JString<'local>,
        kind: jint,
        klv: jboolean,
        parameter_sets: JObjectArray<'local>,
    ) -> jlong = 0 => {
        let Some(sets) = read_byte_arrays(env, &parameter_sets) else {
            return 0;
        };
        let Some(desc) = description(env, kind, klv, sets) else {
            return 0;
        };
        let settings = super::client::ClientSettings {
            keepalive_interval_ms,
            connect_timeout_ms,
            request_timeout_ms,
            user_agent: &user_agent,
        };
        let Some(builder) = client_builder(
            env,
            &url,
            &auth_user,
            &auth_password,
            digest_only,
            keepalive,
            &settings,
            &tls_root_certs_pem,
        ) else {
            return 0; // exception pending
        };
        let push = match RtspPush::connect(builder, &desc) {
            Ok(p) => p,
            Err(e) => {
                rtsp_error_to_jvm(env, e);
                return 0;
            }
        };
        let cancel = push.cancel_handle();
        let session_id = push.session_id().to_string();
        let stats = push.stats_handle();
        let slot_cancel: Arc<dyn TransportCancel> = Arc::new(cancel.clone());
        let slot = ParkingSlot::new(push, slot_cancel);
        #[cfg(feature = "jni-test-hooks")]
        let slot = slot.with_observer(probe_observe);
        REGISTRY_PUSH.insert(JniRtspPush {
            slot: Arc::new(slot),
            stats,
            cancel,
            session_id,
            mp2t: matches!(desc, PushDescription::Mp2t),
        }) as jlong
    }
}

/// The session's slot and its shape under a lease; `None` with an
/// `IllegalStateException` pending for a closed or consumed session.
fn lease(env: &mut Env, handle: jlong) -> Option<(Arc<ParkingSlot<RtspPush>>, bool)> {
    let leased = REGISTRY_PUSH.with(handle as u64, |s| (s.slot.clone(), s.mp2t));
    if leased.is_none() {
        crate::error::throw_closed(env, WHAT);
    }
    leased
}

/// Throw what a call the slot refused reports: `CLOSED` in the call's domain (RTP for
/// a data call) for a session a close or a conversion took, `RtspException(CLOSED)`
/// for a poisoned one whatever the call.
fn throw_refused(env: &mut Env, refused: SlotRefused, data_call: bool) {
    match refused {
        SlotRefused::Poisoned => throw_rtsp(env, BindingErrorKind::Closed, POISONED),
        _ if data_call => throw_rtp(env, BindingErrorKind::Closed, TAKEN),
        _ => throw_rtsp(env, BindingErrorKind::Closed, TAKEN),
    }
}

/// Throw what a failed `send_video` / `send_klv` reports: the transport's error in
/// the RTP domain, `IllegalArgumentException` for a call the description cannot
/// take (nothing was sent).
fn throw_push_error(env: &mut Env, e: PushError) {
    match e {
        PushError::Transport(t) => transport_error(env, &t),
        e @ (PushError::NoSuchTrack(_) | PushError::Empty(_)) => {
            throw_illegal_argument(env, &e.to_string());
        }
        // A variant this binding does not know is not caller misuse.
        other => throw_runtime(env, other.to_string()),
    }
}

jni_native! {
    /// `RtspPushSession.nSendTs(handle, ts)`. On a push announced with a video track
    /// every payload is refused (`Broken` from the push, with nothing sent and the
    /// session open): that is `IllegalArgumentException` here. A latched cancel is
    /// still `CLOSED`, as the push checks it first.
    fn Java_org_tstrans_rtp_RtspPushSession_nSendTs<'local>(
        env,
        _class,
        handle: jlong,
        ts: JByteArray<'local>,
    ) = () => {
        let Some((slot, mp2t)) = lease(env, handle) else { return };
        let Some(bytes) = read_bytes(env, &ts) else { return };
        match slot.run_parking(|p| p.send_ts(&bytes)) {
            Err(r) => throw_refused(env, r, true),
            Ok(Ok(())) => {}
            Ok(Err(TransportError::Broken { .. })) if !mp2t => throw_illegal_argument(
                env,
                "this session announced a video track and takes no MPEG-TS",
            ),
            Ok(Err(e)) => transport_error(env, &e),
        }
    }
}

jni_native! {
    /// `RtspPushSession.nSendVideo(handle, annexB, pts)`.
    fn Java_org_tstrans_rtp_RtspPushSession_nSendVideo<'local>(
        env,
        _class,
        handle: jlong,
        annexb: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some((slot, _)) = lease(env, handle) else { return };
        let Some(bytes) = read_bytes(env, &annexb) else { return };
        match slot.run_parking(|p| p.send_video(&bytes, Pts90khz::new(pts))) {
            Err(r) => throw_refused(env, r, true),
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_push_error(env, e),
        }
    }
}

jni_native! {
    /// `RtspPushSession.nSendKlv(handle, klv, pts)`.
    fn Java_org_tstrans_rtp_RtspPushSession_nSendKlv<'local>(
        env,
        _class,
        handle: jlong,
        klv: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some((slot, _)) = lease(env, handle) else { return };
        let Some(bytes) = read_bytes(env, &klv) else { return };
        match slot.run_parking(|p| p.send_klv(&bytes, Pts90khz::new(pts))) {
            Err(r) => throw_refused(env, r, true),
            Ok(Ok(())) => {}
            Ok(Err(e)) => throw_push_error(env, e),
        }
    }
}

/// `pause` / `resume`: the RTSP domain; a push a close took is `CLOSED`.
fn control(
    env: &mut Env,
    handle: jlong,
    op: impl FnOnce(&mut RtspPush) -> Result<(), tst_rtp::RtspError>,
) {
    let Some((slot, _)) = lease(env, handle) else {
        return;
    };
    match slot.run_parking(op) {
        Err(r) => throw_refused(env, r, false),
        Ok(Ok(())) => {}
        Ok(Err(e)) => rtsp_error_to_jvm(env, e),
    }
}

jni_native! {
    /// `RtspPushSession.nPause(handle)`.
    fn Java_org_tstrans_rtp_RtspPushSession_nPause(env, _class, handle: jlong) = () => {
        control(env, handle, RtspPush::pause);
    }
}

jni_native! {
    /// `RtspPushSession.nResume(handle)`.
    fn Java_org_tstrans_rtp_RtspPushSession_nResume(env, _class, handle: jlong) = () => {
        control(env, handle, RtspPush::resume);
    }
}

/// Take the push out of a session the registry has handed over, for its close
/// ([`ParkingSlot::take_for_close`]: a call that can park is cancelled first, a query
/// is waited for, a poisoned slot still gives the push up). `None` when a conversion
/// took it first.
fn take_for_close(entry: &JniRtspPush, _handle: jlong) -> Option<RtspPush> {
    let push = entry.slot.take_for_close();
    #[cfg(feature = "jni-test-hooks")]
    if push.is_none() {
        return probe_take_marked(_handle);
    }
    push
}

jni_native! {
    /// `RtspPushSession.nTeardown(handle)`: CONSUMING (the Java side zeroed its key
    /// first). Closes the push as `nClose` does and throws the TEARDOWN's error.
    /// `false` when a conversion had taken the push, also when it has claimed the key
    /// as well (only a conversion removes the entry while the Java side holds the key):
    /// nothing was torn down, and the Java side reports that as `CLOSED`.
    fn Java_org_tstrans_rtp_RtspPushSession_nTeardown(env, _class, handle: jlong) -> jboolean = false => {
        let Some(entry) = REGISTRY_PUSH.close(handle as u64) else {
            return false;
        };
        let Some(push) = take_for_close(&entry, handle) else {
            return false;
        };
        if let Err(e) = push.close() {
            rtsp_error_to_jvm(env, e);
        }
        true
    }
}

jni_native! {
    /// `RtspPushSession.nClose(handle)`: `RtspPush::close` (the queued media written,
    /// TEARDOWN under 500 ms, the client's thread stopped), silent. Idempotent: only
    /// the close that wins the registry entry runs it. `true` when this call ended the
    /// push, `false` when a conversion had taken it.
    fn Java_org_tstrans_rtp_RtspPushSession_nClose(_env, _class, handle: jlong) -> jboolean = false => {
        if let Some(entry) = REGISTRY_PUSH.close(handle as u64)
            && let Some(push) = take_for_close(&entry, handle)
        {
            let _ = push.close();
            return true;
        }
        false
    }
}

jni_native! {
    /// `RtspPushSession.nCancelHandle(handle)`: a handle on the stored cancel, read
    /// under the lease only, so it answers while a call holds the push. 0 (the Java
    /// side throws `IllegalStateException`) for a closed session.
    fn Java_org_tstrans_rtp_RtspPushSession_nCancelHandle(_env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY_PUSH.with(handle as u64, |s| s.cancel.clone()) {
            Some(inner) => JniRtspCancel { inner }.into_handle(),
            None => 0,
        }
    }
}

jni_native! {
    /// `RtspPushSession.nStatsKey(handle)`: a key of its own on the push's counters
    /// and end reason, which `nStatsOf` / `nEndOf` read until `nFreeStats`; 0 for an
    /// absent session.
    fn Java_org_tstrans_rtp_RtspPushSession_nStatsKey(_env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY_PUSH.with(handle as u64, |s| s.stats.clone()) {
            Some(stats) => REGISTRY_PUSH_STATS.insert(stats) as jlong,
            None => 0,
        }
    }
}

jni_native! {
    /// `RtspPushSession.nStatsOf(statsKey)`: the counters now, read through the handle,
    /// never the slot; null for a freed key.
    fn Java_org_tstrans_rtp_RtspPushSession_nStatsOf(env, _class, key: jlong) -> jobject = std::ptr::null_mut() => {
        let Some(handle) = REGISTRY_PUSH_STATS.with(key as u64, |h| h.clone()) else {
            return std::ptr::null_mut();
        };
        let stats = handle.stats();
        match crate::generated::stats_rtp::build_push_stats(env, &stats) {
            Ok(o) => o.into_raw(),
            Err(e) => {
                throw_runtime_unless_pending(env, e.to_string());
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `RtspPushSession.nEndOf(statsKey)`: why the session ended, as an
    /// `EndReasonSnapshot` (reason null while it has not); null for a freed key.
    fn Java_org_tstrans_rtp_RtspPushSession_nEndOf(env, _class, key: jlong) -> jobject = std::ptr::null_mut() => {
        let Some(handle) = REGISTRY_PUSH_STATS.with(key as u64, |h| h.clone()) else {
            return std::ptr::null_mut();
        };
        let reason = handle.end_reason();
        match build_close_snapshot(env, reason) {
            Ok(o) => o.into_raw(),
            Err(e) => {
                throw_runtime_unless_pending(env, e.to_string());
                std::ptr::null_mut()
            }
        }
    }
}

/// `PushCloseOutcome.Kind.code()`, the status (-1 for none) and the detail ("" for
/// none) of `outcome`, as the Java constructor takes them. A variant added later
/// than this binding reads as `FAILED` with its debug text.
fn close_outcome_parts(outcome: &PushCloseOutcome) -> (jint, jint, String) {
    match outcome {
        PushCloseOutcome::Clean => (0, -1, String::new()),
        PushCloseOutcome::Cancelled => (1, -1, String::new()),
        PushCloseOutcome::MediaLost => (2, -1, String::new()),
        PushCloseOutcome::TeardownRefused { code, reason } => {
            (3, jint::from(*code), reason.clone())
        }
        PushCloseOutcome::TeardownTimedOut => (4, -1, String::new()),
        PushCloseOutcome::Failed { detail } => (5, -1, detail.clone()),
        other => (5, -1, format!("{other:?}")),
    }
}

jni_native! {
    /// `RtspPushSession.nCloseOutcomeOf(statsKey)`: how the push's first close ended,
    /// as an `org.tstrans.rtp.PushCloseOutcome`; null while no close has run, and for a
    /// freed key. Read through the handle, never the slot.
    fn Java_org_tstrans_rtp_RtspPushSession_nCloseOutcomeOf(env, _class, key: jlong) -> jobject = std::ptr::null_mut() => {
        let Some(handle) = REGISTRY_PUSH_STATS.with(key as u64, |h| h.clone()) else {
            return std::ptr::null_mut();
        };
        let Some(outcome) = handle.close_outcome() else {
            return std::ptr::null_mut();
        };
        let (kind, status, detail) = close_outcome_parts(&outcome);
        let built = env.new_string(detail).and_then(|detail| {
            env.new_object(
                FindClass(jni_str!("org/tstrans/rtp/PushCloseOutcome")),
                jni_sig!("(IILjava/lang/String;)V"),
                &[
                    JValue::Int(kind),
                    JValue::Int(status),
                    JValue::Object(&detail),
                ],
            )
        });
        match built {
            Ok(o) => o.into_raw(),
            Err(e) => {
                throw_runtime_unless_pending(env, e.to_string());
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `RtspPushSession.nFreeStats(statsKey)`: free the reader `nStatsKey` made.
    fn Java_org_tstrans_rtp_RtspPushSession_nFreeStats(_env, _class, key: jlong) = () => {
        let _ = REGISTRY_PUSH_STATS.close(key as u64);
    }
}

jni_native! {
    /// `RtspPushSession.nSessionId(handle)`: read at RECORD, so it answers at once.
    fn Java_org_tstrans_rtp_RtspPushSession_nSessionId(env, _class, handle: jlong) -> jstring = std::ptr::null_mut() => {
        let Some(id) = REGISTRY_PUSH.with(handle as u64, |s| s.session_id.clone()) else {
            crate::error::throw_closed(env, WHAT);
            return std::ptr::null_mut();
        };
        match env.new_string(id) {
            Ok(s) => s.into_raw(),
            Err(e) => {
                throw_runtime_unless_pending(env, e.to_string());
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `RtspPushSession.nIsSessionAlive(handle)`: never waits and never throws. A call
    /// holding the push counts as live (the shells' `isAlive` rule) unless a close has
    /// begun; a closed, converted, poisoned or unknown session is `false`.
    fn Java_org_tstrans_rtp_RtspPushSession_nIsSessionAlive(_env, _class, handle: jlong) -> jboolean = false => {
        let Some(slot) = REGISTRY_PUSH.with(handle as u64, |s| s.slot.clone()) else {
            return false;
        };
        match slot.try_peek(RtspPush::is_session_alive) {
            Peek::Value(alive) => alive,
            Peek::Held => true,
            _ => false,
        }
    }
}

/// Refuse a conversion of a push announced with a video track before anything is
/// taken; `false` with an exception pending (`IllegalStateException` for a closed
/// session).
fn convertible(env: &mut Env, handle: jlong) -> bool {
    match REGISTRY_PUSH.with(handle as u64, |s| s.mp2t) {
        Some(true) => true,
        Some(false) => {
            throw_illegal_argument(
                env,
                "only a session announced as MPEG-TS converts to a sender shell",
            );
            false
        }
        None => {
            crate::error::throw_closed(env, WHAT);
            false
        }
    }
}

/// Take the push for a conversion, as an opened send transport
/// ([`ParkingSlot::take_for_conversion`]): never waits for a call that can park, so
/// behind a send, `pause` or `resume` it takes nothing and leaves an
/// `IllegalStateException` saying so (the session stays usable); a query holding the
/// push for a moment is waited for. `None` with an `IllegalStateException` pending
/// also when a close won the session or has begun, and with `RtspException(CLOSED)`
/// for a poisoned one.
fn take_opened(env: &mut Env, handle: jlong) -> Option<Opened<Box<dyn Transport>>> {
    let Some(slot) = REGISTRY_PUSH.with(handle as u64, |s| s.slot.clone()) else {
        crate::error::throw_closed(env, WHAT);
        return None;
    };
    let push = match slot.take_for_conversion() {
        Ok(push) => push,
        Err(SlotRefused::Busy) => {
            throw_illegal_state(env, IN_PROGRESS);
            return None;
        }
        Err(SlotRefused::Poisoned) => {
            throw_rtsp(env, BindingErrorKind::Closed, POISONED);
            return None;
        }
        Err(_) => {
            crate::error::throw_closed(env, WHAT);
            return None;
        }
    };
    // The slot's closed flag, not the registry key, decided: a close that won the
    // entry first took nothing from the slot, so the push is converted either way.
    let _ = REGISTRY_PUSH.close(handle as u64);
    let cancel: Arc<dyn TransportCancel> = Arc::new(push.cancel_handle());
    Some(Opened::new(Box::new(push), cancel, Domain::Rtp))
}

jni_native! {
    /// `RtspPushSession.nIntoSender(handle)`: refuses an elementary push before the
    /// take (the Java side claims its key only after this returns).
    fn Java_org_tstrans_rtp_RtspPushSession_nIntoSender(env, _class, handle: jlong) -> jlong = 0 => {
        if !convertible(env, handle) {
            return 0;
        }
        match take_opened(env, handle) {
            Some(opened) => shells::sender::register(opened),
            None => 0,
        }
    }
}

jni_native! {
    /// `RtspPushSession.nIntoMuxSender(handle, ...programConfig...)`: refuses an
    /// elementary push and builds the muxer config before the take, so a refusal
    /// leaves the session with its caller (the Java side claims its key only after
    /// this returns).
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_rtp_RtspPushSession_nIntoMuxSender<'local>(
        env,
        _class,
        handle: jlong,
        program_number: jint,
        pmt_pid: jint,
        pcr_pid: jint,
        pcr_interval_ms: jint,
        pcr_lead_ms: jint,
        psi_interval_ms: jint,
        buffer_packets: jint,
        av1_carriage: jint,
        stream_pids: JIntArray<'local>,
        stream_kinds: JIntArray<'local>,
        stream_codecs: JIntArray<'local>,
        stream_type_codes: JIntArray<'local>,
        stream_carries_pts: JBooleanArray<'local>,
        data_desc_bytes: JByteArray<'local>,
        data_desc_lens: JIntArray<'local>,
    ) -> jlong = 0 => {
        if !convertible(env, handle) {
            return 0;
        }
        let Ok(cfg) = build_muxer_config_from_arrays(
            env,
            program_number,
            pmt_pid,
            pcr_pid,
            pcr_interval_ms,
            pcr_lead_ms,
            psi_interval_ms,
            buffer_packets,
            av1_carriage,
            &stream_pids,
            &stream_kinds,
            &stream_codecs,
            &stream_type_codes,
            &stream_carries_pts,
            &data_desc_bytes,
            &data_desc_lens,
        ) else {
            return 0; // MuxException pending; the session stays registered
        };
        match take_opened(env, handle) {
            Some(opened) => shells::mux_sender::register(env, opened, cfg),
            None => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only probes (feature = "jni-test-hooks"), never in the shipped JAR: hold a
// real session's slot the way a query call does until the test releases it, count
// the rounds a close or a conversion waits for it, begin a close without finishing
// it, and poison the slot, so a test can order a close against other calls without a
// timing race.
// ---------------------------------------------------------------------------

#[cfg(feature = "jni-test-hooks")]
static PROBE_READER_HOLDS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "jni-test-hooks")]
static PROBE_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The rounds every close or conversion has waited, without a cancel or a refusal,
/// for a query call holding a push.
#[cfg(feature = "jni-test-hooks")]
static PROBE_WAIT_ROUNDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The closes `nMarkClosing` began, by session key: each takes the push once the
/// slot is free, and the session's own close finishes it.
#[cfg(feature = "jni-test-hooks")]
static PROBE_MARKED: LazyLock<
    std::sync::Mutex<std::collections::HashMap<jlong, std::thread::JoinHandle<Option<RtspPush>>>>,
> = LazyLock::new(Default::default);

/// The longest a probe holds the slot unreleased: a test that never releases ends
/// instead of hanging the suite.
#[cfg(feature = "jni-test-hooks")]
const PROBE_HOLD_CAP: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(feature = "jni-test-hooks")]
fn probe_observe(event: tst_pipeline::binding::parking::ParkingEvent) {
    use tst_pipeline::binding::parking::ParkingEvent;
    if matches!(
        event,
        ParkingEvent::CloseRetry | ParkingEvent::ConversionRetry
    ) {
        PROBE_WAIT_ROUNDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The push a close `nMarkClosing` began on `handle` took, once it has.
#[cfg(feature = "jni-test-hooks")]
fn probe_take_marked(handle: jlong) -> Option<RtspPush> {
    let marked = PROBE_MARKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&handle)?;
    marked.join().ok().flatten()
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nHoldAsReader(handle)`: hold the push's slot
    /// through the query path until `nRelease()` (at most 30 s).
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nHoldAsReader(env, _class, handle: jlong) = () => {
        use std::sync::atomic::Ordering;
        PROBE_RELEASE.store(false, Ordering::SeqCst);
        let Some((slot, _)) = lease(env, handle) else { return };
        let held = slot.run(|_| {
            PROBE_READER_HOLDS.store(true, Ordering::SeqCst);
            let start = std::time::Instant::now();
            while !PROBE_RELEASE.load(Ordering::SeqCst) && start.elapsed() < PROBE_HOLD_CAP {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            PROBE_READER_HOLDS.store(false, Ordering::SeqCst);
        });
        if let Err(r) = held {
            throw_refused(env, r, false);
        }
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nRelease()`: end the hold of `nHoldAsReader`.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nRelease(_env, _class) = () => {
        PROBE_RELEASE.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nReaderHolds()`.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nReaderHolds(_env, _class) -> jboolean = false => {
        PROBE_READER_HOLDS.load(std::sync::atomic::Ordering::SeqCst)
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nWaitRounds()`: the rounds every close or
    /// conversion has waited, without a cancel or a refusal, for a query call holding
    /// a push.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nWaitRounds(_env, _class) -> jlong = 0 => {
        PROBE_WAIT_ROUNDS.load(std::sync::atomic::Ordering::SeqCst) as jlong
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nMarkClosing(handle)`: begin a close of the
    /// session on another thread and return once it has begun (the slot is marked
    /// closed); that close takes the push when the slot is free and the session's own
    /// `close()` finishes it. The next call that takes the session meets a close that
    /// has begun.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nMarkClosing(env, _class, handle: jlong) = () => {
        let Some((slot, _)) = lease(env, handle) else { return };
        let closing = Arc::clone(&slot);
        let taker = std::thread::spawn(move || closing.take_for_close());
        PROBE_MARKED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle, taker);
        while !slot.is_closed() {
            std::thread::yield_now();
        }
    }
}

/// The push `nTakeAsConversion` took, until `nCloseTaken`.
#[cfg(feature = "jni-test-hooks")]
static PROBE_TAKEN: std::sync::Mutex<Option<RtspPush>> = std::sync::Mutex::new(None);

jni_native! {
    /// `org.tstrans.internal.PushProbe.nTakeAsConversion(handle)`: take the push from
    /// the session's slot as a conversion does and keep it, leaving the session's key
    /// in place: where a conversion stands between its take and its claim of the key.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nTakeAsConversion(env, _class, handle: jlong) = () => {
        let Some((slot, _)) = lease(env, handle) else { return };
        match slot.take_for_conversion() {
            Ok(push) => {
                *PROBE_TAKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(push);
            }
            Err(r) => throw_refused(env, r, false),
        }
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nRemoveKey(handle)`: remove the session's
    /// registry entry, as a conversion does once it has taken the push.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nRemoveKey(_env, _class, handle: jlong) = () => {
        drop(REGISTRY_PUSH.close(handle as u64));
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nCloseTaken()`: close the push
    /// `nTakeAsConversion` took, as its new owner would; quiet when there is none.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nCloseTaken(_env, _class) = () => {
        let taken = PROBE_TAKEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(push) = taken {
            let _ = push.close();
        }
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nPoison(handle)`: panic inside a query holding
    /// the session's slot, as a bug in a call would, so the slot is poisoned.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nPoison(env, _class, handle: jlong) = () => {
        let Some((slot, _)) = lease(env, handle) else { return };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = slot.run(|_| panic!("a panic inside a push session call, for a test"));
        }));
    }
}
