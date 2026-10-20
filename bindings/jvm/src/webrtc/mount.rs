//! `org.tstrans.webrtc.MountHandle` natives.

use std::sync::LazyLock;

use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jboolean, jint, jlong};
use jni::{Env, jni_sig, jni_str};
use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::{IceState, MountHandle, SessionStats};

use crate::handle::HandleRegistry;
use crate::jutil::{FindClass, checked_u8, read_bytes};
use crate::panic::jni_native;

use super::errors::push_error;

/// Per-type leased-handle registry for `org.tstrans.webrtc.MountHandle`. No
/// cancel hook (pushes never block); `close` frees only this wrapper, the
/// mount stays on the server.
static REGISTRY_MOUNT: LazyLock<HandleRegistry<MountHandle>> = LazyLock::new(HandleRegistry::new);

/// Registers a new mount handle and returns its Java handle.
pub(super) fn register(m: MountHandle) -> jlong {
    REGISTRY_MOUNT.insert(m) as jlong
}

/// Runs `f` on the mount under its lease, or throws `IllegalStateException`
/// for a closed handle. Every `MountHandle` method takes `&self`.
fn with_mount<R>(env: &mut Env, handle: jlong, f: impl FnOnce(&MountHandle) -> R) -> Option<R> {
    let r = REGISTRY_MOUNT.with(handle as u64, |m| f(m));
    if r.is_none() {
        crate::error::throw_closed(env, "MountHandle");
    }
    r
}

/// [`with_mount`] for the pushes: a panic mid-push poisons the entry, so a
/// later call throws `IllegalStateException` instead of using a mount whose
/// push state may be torn.
fn with_mount_poisoning<R>(
    env: &mut Env,
    handle: jlong,
    f: impl FnOnce(&MountHandle) -> R,
) -> Option<R> {
    let r = REGISTRY_MOUNT.with_poisoning(handle as u64, |m| f(m));
    if r.is_none() {
        crate::error::throw_closed(env, "MountHandle");
    }
    r
}

jni_native! {
    /// `MountHandle.nPushVideo(handle, au, pts, keyFrame)`.
    fn Java_org_tstrans_webrtc_MountHandle_nPushVideo<'local>(
        env,
        _class,
        handle: jlong,
        au: JByteArray<'local>,
        pts: jlong,
        key_frame: jboolean,
    ) = () => {
        let Some(buf) = read_bytes(env, &au) else {
            return;
        };
        let Some(res) = with_mount_poisoning(env, handle, |m| {
            m.push_video(&buf, Pts90khz::new(pts), key_frame)
        }) else {
            return;
        };
        if let Err(e) = res {
            push_error(env, e);
        }
    }
}

jni_native! {
    /// `MountHandle.nPushKlv(handle, record, pts, metadataServiceId)`.
    fn Java_org_tstrans_webrtc_MountHandle_nPushKlv<'local>(
        env,
        _class,
        handle: jlong,
        record: JByteArray<'local>,
        pts: jlong,
        metadata_service_id: jint,
    ) = () => {
        let Ok(service_id) = checked_u8(env, i64::from(metadata_service_id), "metadataServiceId")
        else {
            return; // IllegalArgumentException pending
        };
        let Some(buf) = read_bytes(env, &record) else {
            return;
        };
        let Some(res) = with_mount_poisoning(env, handle, |m| {
            m.push_klv(&buf, Pts90khz::new(pts), service_id)
        }) else {
            return;
        };
        if let Err(e) = res {
            push_error(env, e);
        }
    }
}

jni_native! {
    /// `MountHandle.nPushAudio(handle, frames, pts)`.
    fn Java_org_tstrans_webrtc_MountHandle_nPushAudio<'local>(
        env,
        _class,
        handle: jlong,
        frames: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        let Some(res) =
            with_mount_poisoning(env, handle, |m| m.push_audio(&buf, Pts90khz::new(pts)))
        else {
            return;
        };
        if let Err(e) = res {
            push_error(env, e);
        }
    }
}

jni_native! {
    /// `MountHandle.nPushTs(handle, bytes)`.
    fn Java_org_tstrans_webrtc_MountHandle_nPushTs<'local>(
        env,
        _class,
        handle: jlong,
        bytes: JByteArray<'local>,
    ) = () => {
        let Some(buf) = read_bytes(env, &bytes) else {
            return;
        };
        let Some(res) = with_mount_poisoning(env, handle, |m| m.push_ts(&buf)) else {
            return;
        };
        if let Err(e) = res {
            push_error(env, e);
        }
    }
}

jni_native! {
    /// `MountHandle.nStats(handle)` → `MountStats`, or null on a JNI failure.
    fn Java_org_tstrans_webrtc_MountHandle_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let Some(s) = with_mount(env, handle, MountHandle::stats) else {
            return JObject::null();
        };
        // Every u64 crosses as its bit pattern in a Java long.
        env.new_object(
            FindClass(jni_str!("org/tstrans/webrtc/MountStats")),
            jni_sig!("(JJJJJJJJJJJJJJJJJJJ)V"),
            &[
                JValue::Long(s.frames_pushed as i64),
                JValue::Long(s.keyframes_pushed as i64),
                JValue::Long(s.keyframe_flag_mismatch as i64),
                JValue::Long(s.viewers as i64),
                JValue::Long(s.viewers_waiting_keyframe as i64),
                JValue::Long(s.klv_records_pushed as i64),
                JValue::Long(s.klv_records_dropped_unbound as i64),
                JValue::Long(s.klv_sei_inserted as i64),
                JValue::Long(s.klv_sei_dropped_oversize as i64),
                JValue::Long(s.klv_dc_messages_sent as i64),
                JValue::Long(s.klv_dc_dropped_oversize as i64),
                JValue::Long(s.klv_dc_dropped_viewer as i64),
                JValue::Long(s.audio_frames_pushed as i64),
                JValue::Long(s.audio_frames_dropped as i64),
                JValue::Long(s.plis_received as i64),
                JValue::Long(s.nacks_received as i64),
                JValue::Long(s.viewer_packets_dropped_queue_full as i64),
                JValue::Long(s.rtp_clock_rebases as i64),
                JValue::Long(s.demux_nonconformant as i64),
            ],
        )
        .unwrap_or_else(|_| JObject::null())
    }
}

/// The ICE state as the lower-case word `SessionStats.iceState` documents.
fn ice_state_name(s: IceState) -> &'static str {
    match s {
        IceState::New => "new",
        IceState::Checking => "checking",
        IceState::Connected => "connected",
        IceState::Disconnected => "disconnected",
        IceState::Failed => "failed",
        IceState::Closed => "closed",
        _ => "unknown",
    }
}

/// One `org.tstrans.webrtc.SessionStats`.
fn build_session<'l>(env: &mut Env<'l>, s: &SessionStats) -> jni::errors::Result<JObject<'l>> {
    let id = env.new_string(&s.id)?;
    let path = env.new_string(&s.mount_path)?;
    let remote: JObject = match s.remote_addr {
        Some(a) => env.new_string(a.to_string())?.into(),
        None => JObject::null(),
    };
    let ice = env.new_string(ice_state_name(s.ice_state))?;
    // -1 before the session connected, as the record documents.
    let connected_ms = s
        .connected_for
        .map_or(-1, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    env.new_object(
        FindClass(jni_str!("org/tstrans/webrtc/SessionStats")),
        jni_sig!(
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;JJJJJJJJ)V"
        ),
        &[
            JValue::Object(&id),
            JValue::Object(&path),
            JValue::Object(&remote),
            JValue::Object(&ice),
            JValue::Long(connected_ms),
            JValue::Long(s.rtp_packets as i64),
            JValue::Long(s.rtp_bytes as i64),
            JValue::Long(s.nacks as i64),
            JValue::Long(s.plis as i64),
            JValue::Long(s.dc_messages as i64),
            JValue::Long(s.packets_dropped_queue_full as i64),
            JValue::Long(s.dc_messages_dropped as i64),
        ],
    )
}

jni_native! {
    /// `MountHandle.nSessions(handle)` → `java.util.ArrayList<SessionStats>`, or
    /// null on a JNI failure.
    ///
    /// `sessions()` asks the media thread and can wait up to 2 s for its reply,
    /// so it runs on a clone taken under a short lease (the handle is one
    /// `Arc`), never under the lease itself: holding the entry's lock that
    /// long would make pushes and `close()` on this handle wait behind it.
    /// The other natives here do not wait (`stats` reads atomics, pushes
    /// only `try_send`), so they keep the lease.
    fn Java_org_tstrans_webrtc_MountHandle_nSessions<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let Some(mount) = with_mount(env, handle, MountHandle::clone) else {
            return JObject::null();
        };
        let sessions = mount.sessions();
        drop(mount);
        let built = (|| -> jni::errors::Result<JObject<'local>> {
            let list = env.new_object(
                FindClass(jni_str!("java/util/ArrayList")),
                jni_sig!("(I)V"),
                &[JValue::Int(i32::try_from(sessions.len()).unwrap_or(i32::MAX))],
            )?;
            for s in &sessions {
                // A frame per entry keeps the local references bounded.
                env.with_local_frame(8, |inner| {
                    let obj = build_session(inner, s)?;
                    inner.call_method(
                        &list,
                        jni_str!("add"),
                        jni_sig!("(Ljava/lang/Object;)Z"),
                        &[JValue::Object(&obj)],
                    )?;
                    Ok::<(), jni::errors::Error>(())
                })?;
            }
            Ok(list)
        })();
        built.unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `MountHandle.nMountPath(handle)` → the mount's path.
    fn Java_org_tstrans_webrtc_MountHandle_nMountPath<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jni::objects::JString<'local> = jni::objects::JString::null() => {
        let Some(path) = with_mount(env, handle, |m| m.mount_path().to_owned()) else {
            return jni::objects::JString::null();
        };
        env.new_string(path)
            .unwrap_or_else(|_| jni::objects::JString::null())
    }
}

jni_native! {
    /// `MountHandle.nClose(handle)` — free the wrapper; the mount stays on the server.
    fn Java_org_tstrans_webrtc_MountHandle_nClose(_env, _class, handle: jlong) = () => {
        let _ = REGISTRY_MOUNT.close(handle as u64);
    }
}
