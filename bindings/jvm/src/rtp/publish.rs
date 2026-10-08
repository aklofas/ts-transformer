//! `org.tstrans.rtp` RTSP publisher role JNI surface — `PublishMount`,
//! `PublishMountStats`, `PublisherInfo`, `PublishShape`, `ClockAlignment`.
//! Ports tst-py's `bindings/python/src/rtp/publish.rs`.
//!
//! A `PublishMount` comes from `RtspServer.addPublishMount(path)` or, for a
//! mount an ANNOUNCE created on demand, from `RtspServer.nextPublisher(ms)`
//! (both natives live in `server.rs` and register the handle here). The
//! wrapped `tst_rtp` `PublishMountHandle` is `Clone` (an `Arc` inside), and
//! every method takes `&self` and returns without parking, so the registry
//! needs no cancel hook: `close` frees the wrapper only, and the mount stays
//! in the server's table until `RtspServer.removeMount` or the server stops.

use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong};

use tst_core::mpegts::demux::DemuxerConfig;
use tst_rtp::rtsp::server::publish::{
    ClockAlignment as RustClockAlignment, PublishMountHandle as RustPublishMountHandle,
    PublishMountStats as RustPublishMountStats, PublishShape as RustPublishShape,
    PublisherInfo as RustPublisherInfo,
};

use crate::handle::HandleRegistry;
use crate::mpegts::build_demux_config_from_args;

use super::demux_receiver::demux_receiver_handle_from_transport;
use super::errors::server_error_to_jvm;

/// Per-type leased-handle registry for `org.tstrans.rtp.PublishMount`. No
/// cancel hook: no `PublishMount` native parks.
static REGISTRY_PUBLISH: LazyLock<HandleRegistry<RustPublishMountHandle>> =
    LazyLock::new(HandleRegistry::new);

/// Register a publish-mount handle and return its `PublishMount` key.
pub(super) fn publish_mount_handle(h: RustPublishMountHandle) -> jlong {
    REGISTRY_PUBLISH.insert(h) as jlong
}

/// Lease the mount for `handle` and run `f` on it. `None` (absent/closed) →
/// throw `IllegalStateException`.
fn with_publish<R>(
    env: &mut JNIEnv,
    handle: jlong,
    f: impl FnOnce(&RustPublishMountHandle) -> R,
) -> Option<R> {
    match REGISTRY_PUBLISH.with(handle as u64, |m| f(m)) {
        Some(r) => Some(r),
        None => {
            crate::error::throw_closed(env, "PublishMount");
            None
        }
    }
}

/// `org.tstrans.rtp.<class>.<member>` enum constant, looked up by name.
fn enum_const<'local>(
    env: &mut JNIEnv<'local>,
    class: &str,
    member: &str,
) -> jni::errors::Result<JObject<'local>> {
    env.get_static_field(
        format!("org/tstrans/rtp/{class}"),
        member,
        format!("Lorg/tstrans/rtp/{class};"),
    )?
    .l()
}

/// The `ClockAlignment` member name; `None` for a variant this build does not
/// know (the Rust enum is non-exhaustive).
fn alignment_name(a: RustClockAlignment) -> Option<&'static str> {
    Some(match a {
        RustClockAlignment::NotApplicable => "NOT_APPLICABLE",
        RustClockAlignment::Pending => "PENDING",
        RustClockAlignment::Provisional => "PROVISIONAL",
        RustClockAlignment::SenderReport => "SENDER_REPORT",
        _ => return None,
    })
}

/// Build `org.tstrans.rtp.PublishMountStats`: the Rust fields in Rust order.
fn build_stats<'local>(
    env: &mut JNIEnv<'local>,
    s: &RustPublishMountStats,
) -> jni::errors::Result<JObject<'local>> {
    let Some(name) = alignment_name(s.alignment) else {
        let _ = env.throw_new(
            "java/lang/RuntimeException",
            "ClockAlignment variant unknown to this build of tstrans",
        );
        return Ok(JObject::null());
    };
    let alignment = enum_const(env, "ClockAlignment", name)?;
    let l = |v: u64| JValue::Long(v as i64);
    env.new_object(
        "org/tstrans/rtp/PublishMountStats",
        "(JJJJJJJJJJJJLorg/tstrans/rtp/ClockAlignment;JJJJ)V",
        &[
            l(s.rtp_packets_received),
            l(s.bytes_received),
            l(s.malformed_packets),
            l(s.source_rejected),
            l(s.frames_emitted),
            l(s.frames_dropped_app),
            l(s.frames_dropped_readers),
            l(s.aus_emitted),
            l(s.aus_dropped),
            l(s.aus_reordered),
            l(s.klv_units_emitted),
            l(s.klv_units_dropped),
            JValue::Object(&alignment),
            l(s.alignment_steps),
            l(s.ssrc_changes),
            l(s.generation),
            l(s.peer_count as u64),
        ],
    )
}

/// Build `org.tstrans.rtp.PublisherInfo(String, PublishShape, boolean, long, long)`.
fn build_info<'local>(
    env: &mut JNIEnv<'local>,
    info: &RustPublisherInfo,
) -> jni::errors::Result<JObject<'local>> {
    let (shape, klv) = match info.shape {
        RustPublishShape::Mp2t => ("MP2T", false),
        RustPublishShape::Elementary { klv } => ("ELEMENTARY", klv),
        _ => {
            let _ = env.throw_new(
                "java/lang/RuntimeException",
                "PublishShape variant unknown to this build of tstrans",
            );
            return Ok(JObject::null());
        }
    };
    // A clock set before 1970 reads 0 rather than failing the snapshot.
    let since_unix_ms = info
        .since
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let peer = env.new_string(info.peer.to_string())?;
    let shape = enum_const(env, "PublishShape", shape)?;
    env.new_object(
        "org/tstrans/rtp/PublisherInfo",
        "(Ljava/lang/String;Lorg/tstrans/rtp/PublishShape;ZJJ)V",
        &[
            JValue::Object(&peer),
            JValue::Object(&shape),
            JValue::Bool(u8::from(klv)),
            JValue::Long(since_unix_ms),
            JValue::Long(info.generation as i64),
        ],
    )
}

/// `PublishMount.nMountPath(handle)` → the mount path.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nMountPath<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JString<'local> {
    crate::panic::jni_catch(&mut env, JObject::null().into(), |env| {
        let Some(path) = with_publish(env, handle, |m| m.mount_path().to_owned()) else {
            return JObject::null().into();
        };
        env.new_string(path)
            .unwrap_or_else(|_| JObject::null().into())
    })
}

/// `PublishMount.nPeerCount(handle)` → live PLAY readers.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nPeerCount(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        with_publish(env, handle, |m| m.peer_count() as jlong).unwrap_or(0)
    })
}

/// `PublishMount.nGeneration(handle)` → publishers that have ended on the mount.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nGeneration(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        with_publish(env, handle, |m| m.generation() as jlong).unwrap_or(0)
    })
}

/// `PublishMount.nPublisher(handle)` → `PublisherInfo`, or null when no
/// publisher holds the mount.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nPublisher<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(Some(info)) = with_publish(env, handle, |m| m.publisher()) else {
            return JObject::null();
        };
        build_info(env, &info).unwrap_or_else(|_| JObject::null())
    })
}

/// `PublishMount.nStats(handle)` → `PublishMountStats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(s) = with_publish(env, handle, |m| m.stats()) else {
            return JObject::null();
        };
        build_stats(env, &s).unwrap_or_else(|_| JObject::null())
    })
}

/// `PublishMount.nCancel(handle)` — end the application side only: the
/// mount's `DemuxReceiver` (taken now or later) reads CLOSED.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nCancel(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        with_publish(env, handle, |m| m.cancel());
    })
}

/// `PublishMount.nIntoDemuxReceiver(handle, withConfig, ...demuxer config...)`
/// → `DemuxReceiver` handle, or 0 with a pending exception. Does not consume
/// the `PublishMount` wrapper: the take-once rule lives in the mount, so a
/// second take, through this or any other handle to the mount, is the
/// `TransportTaken` error (`RtspException` kind `CLOSED`). The config is
/// validated before the take, so a bad config does not spend it.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nIntoDemuxReceiver(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    with_config: jboolean,
    strict: jint,
    pes_cap_per_pid: jlong,
    pes_cap_total: jlong,
    cfi: jboolean,
    av1: jint,
    au_cell_cap: jlong,
    lenient_psi: jboolean,
    sync_buf_cap: jlong,
    unwrap_timestamps: jboolean,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let opts: Option<DemuxerConfig> = if with_config != 0 {
            let Some(cfg) = build_demux_config_from_args(
                env,
                strict,
                pes_cap_per_pid,
                pes_cap_total,
                cfi,
                av1,
                au_cell_cap,
                lenient_psi,
                sync_buf_cap,
                unwrap_timestamps,
            ) else {
                return 0;
            };
            Some(cfg)
        } else {
            None
        };
        let Some(mount) = with_publish(env, handle, RustPublishMountHandle::clone) else {
            return 0;
        };
        let transport = match mount.into_recv_transport() {
            Ok(t) => t,
            Err(e) => {
                server_error_to_jvm(env, e);
                return 0;
            }
        };
        match demux_receiver_handle_from_transport(transport, opts) {
            Ok(h) => h,
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.detail);
                0
            }
        }
    })
}

/// `PublishMount.nClose(handle)` — free the wrapper. The mount stays in the
/// server's table until `removeMount` or the server stops.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_PublishMount_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        let _ = REGISTRY_PUBLISH.close(handle as u64);
    })
}
