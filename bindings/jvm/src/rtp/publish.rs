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

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JObject, JString, JValue};
use jni::signature::RuntimeFieldSignature;
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong};

use tst_core::mpegts::demux::DemuxerConfig;
use tst_rtp::rtsp::server::publish::{
    ClockAlignment as RustClockAlignment, PublishMountHandle as RustPublishMountHandle,
    PublishShape as RustPublishShape, PublisherInfo as RustPublisherInfo,
};

use crate::handle::HandleRegistry;
use crate::mpegts::build_demux_config_from_args;

use super::demux_receiver_handle_from_transport;
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
    env: &mut Env,
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
    env: &mut Env<'local>,
    class: &str,
    member: &str,
) -> jni::errors::Result<JObject<'local>> {
    env.get_static_field(
        FindClass(JNIString::from(format!("org/tstrans/rtp/{class}"))),
        JNIString::from(member),
        RuntimeFieldSignature::from_str(format!("Lorg/tstrans/rtp/{class};"))?.field_signature(),
    )?
    .l()
}

/// The Java `ClockAlignment` constant for `a`. A variant this build does not
/// know throws `RuntimeException` and returns `Err(())`: the exception is
/// already pending (the Rust enum is non-exhaustive).
pub(crate) fn build_clock_alignment<'local>(
    env: &mut Env<'local>,
    a: RustClockAlignment,
) -> Result<JObject<'local>, ()> {
    let name = match a {
        RustClockAlignment::NotApplicable => "NOT_APPLICABLE",
        RustClockAlignment::Pending => "PENDING",
        RustClockAlignment::Provisional => "PROVISIONAL",
        RustClockAlignment::SenderReport => "SENDER_REPORT",
        _ => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                jni_str!("ClockAlignment variant unknown to this build of tstrans"),
            );
            return Err(());
        }
    };
    enum_const(env, "ClockAlignment", name).map_err(|_| ())
}

/// Build `org.tstrans.rtp.PublisherInfo(String, PublishShape, boolean, long, long)`.
fn build_info<'local>(
    env: &mut Env<'local>,
    info: &RustPublisherInfo,
) -> jni::errors::Result<JObject<'local>> {
    let (shape, klv) = match info.shape {
        RustPublishShape::Mp2t => ("MP2T", false),
        RustPublishShape::Elementary { klv } => ("ELEMENTARY", klv),
        _ => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                jni_str!("PublishShape variant unknown to this build of tstrans"),
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
        FindClass(jni_str!("org/tstrans/rtp/PublisherInfo")),
        jni_sig!("(Ljava/lang/String;Lorg/tstrans/rtp/PublishShape;ZJJ)V"),
        &[
            JValue::Object(&peer),
            JValue::Object(&shape),
            JValue::Bool(klv),
            JValue::Long(since_unix_ms),
            JValue::Long(info.generation as i64),
        ],
    )
}

jni_native! {
    /// `PublishMount.nMountPath(handle)` → the mount path.
    fn Java_org_tstrans_rtp_PublishMount_nMountPath<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JString<'local> = JString::null() => {
        let Some(path) = with_publish(env, handle, |m| m.mount_path().to_owned()) else {
            return JString::null();
        };
        env.new_string(path)
            .unwrap_or_else(|_| JString::null())
    }
}

jni_native! {
    /// `PublishMount.nPeerCount(handle)` → live PLAY readers.
    fn Java_org_tstrans_rtp_PublishMount_nPeerCount(env, _class, handle: jlong) -> jlong = 0 => {
        with_publish(env, handle, |m| m.peer_count() as jlong).unwrap_or(0)
    }
}

jni_native! {
    /// `PublishMount.nGeneration(handle)` → publishers that have ended on the mount.
    fn Java_org_tstrans_rtp_PublishMount_nGeneration(env, _class, handle: jlong) -> jlong = 0 => {
        with_publish(env, handle, |m| m.generation() as jlong).unwrap_or(0)
    }
}

jni_native! {
    /// `PublishMount.nPublisher(handle)` → `PublisherInfo`, or null when no
    /// publisher holds the mount.
    fn Java_org_tstrans_rtp_PublishMount_nPublisher<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let Some(Some(info)) = with_publish(env, handle, |m| m.publisher()) else {
            return JObject::null();
        };
        build_info(env, &info).unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `PublishMount.nStats(handle)` → `PublishMountStats`.
    fn Java_org_tstrans_rtp_PublishMount_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let Some(s) = with_publish(env, handle, |m| m.stats()) else {
            return JObject::null();
        };
        crate::generated::stats_rtp::build_publish_mount_stats(env, &s)
            .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `PublishMount.nCancel(handle)` — end the application side only: the
    /// mount's `DemuxReceiver` (taken now or later) reads CLOSED.
    fn Java_org_tstrans_rtp_PublishMount_nCancel(env, _class, handle: jlong) = () => {
        with_publish(env, handle, |m| m.cancel());
    }
}

jni_native! {
    /// `PublishMount.nIntoDemuxReceiver(handle, withConfig, ...demuxer config...)`
    /// → `DemuxReceiver` handle, or 0 with a pending exception. Does not consume
    /// the `PublishMount` wrapper: the take-once rule lives in the mount, so a
    /// second take, through this or any other handle to the mount, is the
    /// `TransportTaken` error (`RtspException` kind `CLOSED`). The config is
    /// validated before the take, so a bad config does not spend it.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_rtp_PublishMount_nIntoDemuxReceiver(
        env,
        _class,
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
    ) -> jlong = 0 => {
        let opts: Option<DemuxerConfig> = if with_config {
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
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.detail));
                0
            }
        }
    }
}

jni_native! {
    /// `PublishMount.nClose(handle)` — free the wrapper. The mount stays in the
    /// server's table until `removeMount` or the server stops.
    fn Java_org_tstrans_rtp_PublishMount_nClose(_env, _class, handle: jlong) = () => {
        let _ = REGISTRY_PUBLISH.close(handle as u64);
    }
}
