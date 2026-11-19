//! `org.tstrans.rtp` RTSP publish surface: `RtspClient.nPush`, `PushDescription`'s
//! key-frame readers and `RtspPushSession`.
//!
//! The session mirrors the play session's layout (`JniRtspSession` in
//! `client.rs`): the `RtspPush` behind a mutex, which every data, control and query
//! call locks for its whole run (a data call parks there while the server is slow),
//! and a clone of the push's cancel kept outside that mutex, so `nCancelHandle` and a
//! close answer while a send is parked. The registry has no cancel hook: `nClose`
//! fires the stored cancel itself, and only when a call that can park on the network
//! holds or awaits the push ([`take_for_close`]); a query call holding it for a
//! moment is waited for.

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, TryLockError};

use jni::objects::{JBooleanArray, JByteArray, JIntArray, JObject, JObjectArray, JString, JValue};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong, jobject, jobjectArray, jstring};
use jni::{Env, jni_sig, jni_str};

use tst_core::mpegts::common::Pts90khz;
use tst_core::transport::{Transport, TransportCancel, TransportError};
use tst_pipeline::binding::{BindingErrorKind, Domain, Opened};
use tst_rtp::rtsp::client::RtspCancelHandle as RustRtspCancel;
use tst_rtp::{PushDescription, PushError, PushStats, RtspPush, VideoTrack};

use crate::handle::HandleRegistry;
use crate::mpegts::muxer::build_muxer_config_from_arrays;
use crate::shells;

use super::client::{JniRtspCancel, client_builder};
use super::errors::{rtsp_error_to_jvm, throw_rtp, throw_rtsp, transport_error};

/// `PushDescription.KIND_*`: the description's kind as the Java side passes it.
const KIND_MP2T: jint = 0;
const KIND_H264: jint = 1;
const KIND_H265: jint = 2;

const WHAT: &str = "RtspPushSession";

/// What a call reports when a close on another thread took the push while the call
/// waited for it.
const TAKEN: &str = "the session was closed during the call";

/// What the calls on one session share: the push and what a close reads to decide
/// whether to cancel the call holding it.
struct Shared {
    /// `None` once a close or a conversion has taken the push.
    push: Mutex<Option<RtspPush>>,
    /// Calls that can park on the network (a send, `pause`, `resume`) running or
    /// waiting for `push`. Query calls are not counted.
    blocking: AtomicUsize,
    /// Set by a close before it reads `blocking`: a blocking call that gets `push`
    /// afterwards runs nothing, so the close never waits for a call it did not see.
    closing: AtomicBool,
}

/// Native backing for `org.tstrans.rtp.RtspPushSession`.
struct JniRtspPush {
    shared: Arc<Shared>,
    /// The push's cancel, read at RECORD and kept outside `push`'s mutex, which a
    /// parked send holds.
    cancel: RustRtspCancel,
    /// The session id, read at RECORD: it never changes.
    session_id: String,
    /// Announced as MPEG-TS: the only shape `sendTs` and the conversions take.
    mp2t: bool,
}

/// Per-type leased-handle registry for `org.tstrans.rtp.RtspPushSession`. No cancel
/// hook (see the module doc).
static REGISTRY_PUSH: LazyLock<HandleRegistry<JniRtspPush>> = LazyLock::new(HandleRegistry::new);

fn throw_illegal_argument(env: &mut Env, msg: &str) {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalArgumentException")),
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
            throw_runtime(env, e.to_string());
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
            throw_runtime(env, e.to_string());
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
                throw_runtime(env, e.to_string());
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
            if !env.exception_check() {
                throw_runtime(env, e.to_string());
            }
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
    /// tlsRootCertsPem, kind, klv, parameterSets)`: connect, ANNOUNCE, SETUP every
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
        let Some(builder) = client_builder(
            env,
            &url,
            &auth_user,
            &auth_password,
            digest_only,
            keepalive,
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
        REGISTRY_PUSH.insert(JniRtspPush {
            cancel: push.cancel_handle(),
            session_id: push.session_id().to_string(),
            mp2t: matches!(desc, PushDescription::Mp2t),
            shared: Arc::new(Shared {
                push: Mutex::new(Some(push)),
                blocking: AtomicUsize::new(0),
                closing: AtomicBool::new(false),
            }),
        }) as jlong
    }
}

/// The session's shared state and its shape under a lease; `None` with an
/// `IllegalStateException` pending for a closed or consumed session.
fn lease(env: &mut Env, handle: jlong) -> Option<(Arc<Shared>, bool)> {
    let leased = REGISTRY_PUSH.with(handle as u64, |s| (s.shared.clone(), s.mp2t));
    if leased.is_none() {
        crate::error::throw_closed(env, WHAT);
    }
    leased
}

/// Run the query `op` on the push, holding its mutex for the whole call; `None`
/// when a close took the push while this call waited for it. A poisoned mutex (a
/// panic in an earlier call, already thrown as a `RuntimeException`) is used as it
/// is.
fn run<R>(shared: &Shared, op: impl FnOnce(&mut RtspPush) -> R) -> Option<R> {
    let mut guard = shared.push.lock().unwrap_or_else(PoisonError::into_inner);
    guard.as_mut().map(op)
}

/// Lowers [`Shared::blocking`] when a blocking call ends, however it ends.
struct BlockingCall<'a>(&'a AtomicUsize);

impl Drop for BlockingCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// [`run`] for a call that can park on the network: counted in
/// [`Shared::blocking`] from before it waits for the mutex, so a close cancels it,
/// and `None` (nothing run) when it gets the mutex after a close began.
fn run_blocking<R>(shared: &Shared, op: impl FnOnce(&mut RtspPush) -> R) -> Option<R> {
    shared.blocking.fetch_add(1, Ordering::SeqCst);
    let _counted = BlockingCall(&shared.blocking);
    let mut guard = shared.push.lock().unwrap_or_else(PoisonError::into_inner);
    if shared.closing.load(Ordering::SeqCst) {
        return None;
    }
    guard.as_mut().map(op)
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
        match run_blocking(&slot, |p| p.send_ts(&bytes)) {
            None => throw_rtp(env, BindingErrorKind::Closed, TAKEN),
            Some(Ok(())) => {}
            Some(Err(TransportError::Broken { .. })) if !mp2t => throw_illegal_argument(
                env,
                "this session announced a video track and takes no MPEG-TS",
            ),
            Some(Err(e)) => transport_error(env, &e),
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
        match run_blocking(&slot, |p| p.send_video(&bytes, Pts90khz::new(pts))) {
            None => throw_rtp(env, BindingErrorKind::Closed, TAKEN),
            Some(Ok(())) => {}
            Some(Err(e)) => throw_push_error(env, e),
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
        match run_blocking(&slot, |p| p.send_klv(&bytes, Pts90khz::new(pts))) {
            None => throw_rtp(env, BindingErrorKind::Closed, TAKEN),
            Some(Ok(())) => {}
            Some(Err(e)) => throw_push_error(env, e),
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
    match run_blocking(&slot, op) {
        None => throw_rtsp(env, BindingErrorKind::Closed, TAKEN),
        Some(Ok(())) => {}
        Some(Err(e)) => rtsp_error_to_jvm(env, e),
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

/// Take the push out of a session the registry has handed over, for its close. A
/// call that can park on the network and holds or awaits the push (a send on a
/// slow server, a pause draining the queue) is cancelled first, so the close waits
/// only for it to notice (about 100 ms). A query call holding the push for a moment
/// is waited for, not cancelled: like an idle push, the session's queued media is
/// then written and an accepted TEARDOWN ends it cleanly.
fn take_for_close(entry: &JniRtspPush) -> Option<RtspPush> {
    let shared = &entry.shared;
    // Before `blocking` is read: a blocking call counted after that read finds
    // `closing` once it has the mutex and runs nothing.
    shared.closing.store(true, Ordering::SeqCst);
    match shared.push.try_lock() {
        Ok(mut guard) => guard.take(),
        Err(TryLockError::WouldBlock) => {
            if shared.blocking.load(Ordering::SeqCst) > 0 {
                entry.cancel.cancel();
            }
            shared
                .push
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        }
        Err(TryLockError::Poisoned(p)) => p.into_inner().take(),
    }
}

jni_native! {
    /// `RtspPushSession.nTeardown(handle)`: CONSUMING (the Java side zeroed its key
    /// first). Closes the push as `nClose` does and throws the TEARDOWN's error.
    fn Java_org_tstrans_rtp_RtspPushSession_nTeardown(env, _class, handle: jlong) = () => {
        let Some(entry) = REGISTRY_PUSH.close(handle as u64) else {
            crate::error::throw_closed(env, WHAT);
            return;
        };
        if let Some(push) = take_for_close(&entry)
            && let Err(e) = push.close()
        {
            rtsp_error_to_jvm(env, e);
        }
    }
}

jni_native! {
    /// `RtspPushSession.nClose(handle)`: `RtspPush::close` (the queued media written,
    /// TEARDOWN under 500 ms, the client's thread stopped), silent. Idempotent: only
    /// the close that wins the registry entry runs it.
    fn Java_org_tstrans_rtp_RtspPushSession_nClose(_env, _class, handle: jlong) = () => {
        if let Some(entry) = REGISTRY_PUSH.close(handle as u64)
            && let Some(push) = take_for_close(&entry)
        {
            let _ = push.close();
        }
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

/// One `org.tstrans.rtp.RtspPushStats`.
fn build_stats<'l>(env: &mut Env<'l>, s: &PushStats) -> jni::errors::Result<JObject<'l>> {
    env.new_object(
        FindClass(jni_str!("org/tstrans/rtp/RtspPushStats")),
        jni_sig!("(JJJJIJJJ)V"),
        &[
            JValue::Long(s.packets_sent as i64),
            JValue::Long(s.bytes_sent as i64),
            JValue::Long(s.sender_reports_sent as i64),
            JValue::Long(s.receiver_reports as i64),
            JValue::Int(jint::from(s.fraction_lost_q8)),
            JValue::Long(i64::from(s.cumulative_lost)),
            JValue::Long(s.frames_dropped_inbound as i64),
            JValue::Long(s.frames_dropped_at_close as i64),
        ],
    )
}

jni_native! {
    /// `RtspPushSession.nStats(handle)`: waits for a call that holds the push.
    fn Java_org_tstrans_rtp_RtspPushSession_nStats(env, _class, handle: jlong) -> jobject = std::ptr::null_mut() => {
        let Some((slot, _)) = lease(env, handle) else { return std::ptr::null_mut() };
        let Some(stats) = run(&slot, |p| p.stats()) else {
            crate::error::throw_closed(env, WHAT);
            return std::ptr::null_mut();
        };
        match build_stats(env, &stats) {
            Ok(o) => o.into_raw(),
            Err(e) => {
                if !env.exception_check() {
                    throw_runtime(env, e.to_string());
                }
                std::ptr::null_mut()
            }
        }
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
                if !env.exception_check() {
                    throw_runtime(env, e.to_string());
                }
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `RtspPushSession.nIsSessionAlive(handle)`: waits for a call that holds the push.
    fn Java_org_tstrans_rtp_RtspPushSession_nIsSessionAlive(env, _class, handle: jlong) -> jboolean = false => {
        let Some((slot, _)) = lease(env, handle) else { return false };
        match run(&slot, |p| p.is_session_alive()) {
            Some(alive) => alive,
            None => {
                crate::error::throw_closed(env, WHAT);
                false
            }
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

/// Take the whole session out of its registry for a conversion: the push as an
/// opened send transport. Waits for a call that holds the push; does not cancel it.
/// `None` with an `IllegalStateException` pending when a close won the session.
fn take_opened(env: &mut Env, handle: jlong) -> Option<Opened<Box<dyn Transport>>> {
    let push = REGISTRY_PUSH.close(handle as u64).and_then(|entry| {
        entry
            .shared
            .push
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    });
    let Some(push) = push else {
        crate::error::throw_closed(env, WHAT);
        return None;
    };
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
// real session's lock the way a query call does, so a test can close the session
// while a non-blocking holder has it.
// ---------------------------------------------------------------------------

#[cfg(feature = "jni-test-hooks")]
static PROBE_READER_HOLDS: AtomicBool = AtomicBool::new(false);

jni_native! {
    /// `org.tstrans.internal.PushProbe.nHoldAsReader(handle, ms)`: hold the push's
    /// lock through the query path for `ms` milliseconds.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nHoldAsReader(env, _class, handle: jlong, ms: jlong) = () => {
        let Some((slot, _)) = lease(env, handle) else { return };
        let held = run(&slot, |_| {
            PROBE_READER_HOLDS.store(true, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64));
            PROBE_READER_HOLDS.store(false, Ordering::SeqCst);
        });
        if held.is_none() {
            crate::error::throw_closed(env, WHAT);
        }
    }
}

jni_native! {
    /// `org.tstrans.internal.PushProbe.nReaderHolds()`.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_PushProbe_nReaderHolds(_env, _class) -> jboolean = false => {
        PROBE_READER_HOLDS.load(Ordering::SeqCst)
    }
}
