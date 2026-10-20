//! `org.tstrans.webrtc.WhepServer` natives.
//!
//! The server is held behind an `Arc` in a leased [`HandleRegistry`]; every
//! native clones it out under a short lease and runs with no registry lock
//! held. `tst_webrtc::WhepServer` takes `&self` everywhere and guards its own
//! state, and a call can wait (a statistics read waits up to 2 s for the media
//! thread, `stop` up to 2 s per thread), so holding the lease would make a
//! concurrent `close()` wait behind it. `close()` takes the registry's `Arc`
//! and stops the server; a clone still in flight keeps the stopped server
//! alive until that call returns.

use std::sync::{Arc, LazyLock};

use jni::objects::{JObject, JString, JValue};
use jni::sys::{jint, jlong};
use jni::{Env, jni_sig, jni_str};
use tst_pipeline::binding::BindingErrorKind;
use tst_webrtc::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec, WhepServer};

use crate::handle::HandleRegistry;
use crate::jutil::FindClass;
use crate::panic::jni_native;

use super::errors::{server_error, throw_webrtc};

type ServerInner = Arc<WhepServer>;

/// Per-type leased-handle registry for `org.tstrans.webrtc.WhepServer`. No
/// cancel hook: no server call parks on the network, and `stop` is bounded.
static REGISTRY_SERVER: LazyLock<HandleRegistry<ServerInner>> = LazyLock::new(HandleRegistry::new);

/// The server behind `handle`, cloned out of the registry, or `None` after
/// throwing `IllegalStateException` for a closed handle.
fn server(env: &mut Env, handle: jlong) -> Option<ServerInner> {
    let s = REGISTRY_SERVER.with(handle as u64, |s| Arc::clone(s));
    if s.is_none() {
        crate::error::throw_closed(env, "WhepServer");
    }
    s
}

/// A non-null Java string, or `None` with a `RuntimeException` pending.
fn read_string(env: &mut Env, s: &JString) -> Option<String> {
    match s.try_to_string(env) {
        Ok(v) => Some(v),
        Err(e) => {
            throw_webrtc(env, BindingErrorKind::Internal, &e.to_string());
            None
        }
    }
}

jni_native! {
    /// `WhepServer.nBind(bindUrl)` → server handle, or 0 with a pending
    /// `WebrtcException` (`URL` / `TLS` / `IO`).
    fn Java_org_tstrans_webrtc_WhepServer_nBind<'local>(
        env,
        _class,
        bind_url: JString<'local>,
    ) -> jlong = 0 => {
        let Some(url) = read_string(env, &bind_url) else {
            return 0;
        };
        match WhepServer::bind(&url) {
            Ok(s) => REGISTRY_SERVER.insert(Arc::new(s)) as jlong,
            Err(e) => {
                server_error(env, e);
                0
            }
        }
    }
}

/// Builds the mount configuration from the Java enums' ordinals, which follow
/// their declaration order. An ordinal outside an enum is a JAR built from
/// other sources; it is refused as `MOUNT`, the invalid-configuration kind.
fn mount_config(
    env: &mut Env,
    kind: jint,
    video: jint,
    audio: jint,
    klv: jint,
    sei_budget_bytes: jint,
    viewer_queue_packets: jint,
) -> Option<MountConfig> {
    let mut refuse = |what: &str, v: jint| {
        throw_webrtc(
            env,
            BindingErrorKind::WebrtcMount,
            &format!("unknown {what} ordinal {v}"),
        );
        None
    };
    let mut cfg = MountConfig::default();
    cfg.kind = match kind {
        0 => MountKind::Elementary,
        1 => MountKind::Mp2t,
        v => return refuse("MountKind", v),
    };
    cfg.video = match video {
        0 => VideoCodec::H264,
        1 => VideoCodec::H265,
        2 => VideoCodec::H266,
        3 => VideoCodec::Av1,
        v => return refuse("VideoCodec", v),
    };
    cfg.audio = match audio {
        0 => AudioPolicy::Drop,
        1 => AudioPolicy::PassthroughOpus,
        v => return refuse("AudioPolicy", v),
    };
    cfg.klv = match klv {
        0 => KlvCarriage::SeiAndDataChannel,
        1 => KlvCarriage::SeiOnly,
        2 => KlvCarriage::DataChannelOnly,
        v => return refuse("KlvCarriage", v),
    };
    // A negative Java int is no valid count; 0 reaches `validate`, which
    // refuses it with the range in its message.
    cfg.sei_budget_bytes = u32::try_from(sei_budget_bytes).unwrap_or(0);
    cfg.viewer_queue_packets = u32::try_from(viewer_queue_packets).unwrap_or(0);
    Some(cfg)
}

jni_native! {
    /// `WhepServer.nAddMount(handle, path, kind, video, audio, klv, seiBudgetBytes,
    /// viewerQueuePackets)` → mount handle, or 0 with a pending `WebrtcException`
    /// (`MOUNT`, or `CLOSED` after stop).
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_webrtc_WhepServer_nAddMount<'local>(
        env,
        _class,
        handle: jlong,
        path: JString<'local>,
        kind: jint,
        video: jint,
        audio: jint,
        klv: jint,
        sei_budget_bytes: jint,
        viewer_queue_packets: jint,
    ) -> jlong = 0 => {
        let Some(srv) = server(env, handle) else {
            return 0;
        };
        let Some(path) = read_string(env, &path) else {
            return 0;
        };
        let Some(cfg) = mount_config(
            env,
            kind,
            video,
            audio,
            klv,
            sei_budget_bytes,
            viewer_queue_packets,
        ) else {
            return 0;
        };
        match srv.add_mount(&path, cfg) {
            Ok(m) => super::mount::register(m),
            Err(e) => {
                server_error(env, e);
                0
            }
        }
    }
}

jni_native! {
    /// `WhepServer.nRemoveMount(handle, path)`; `MOUNT` when nothing is mounted there.
    fn Java_org_tstrans_webrtc_WhepServer_nRemoveMount<'local>(
        env,
        _class,
        handle: jlong,
        path: JString<'local>,
    ) = () => {
        let Some(srv) = server(env, handle) else {
            return;
        };
        let Some(path) = read_string(env, &path) else {
            return;
        };
        if let Err(e) = srv.remove_mount(&path) {
            server_error(env, e);
        }
    }
}

jni_native! {
    /// `WhepServer.nStats(handle)` → `ServerStats`, or null on a JNI failure.
    fn Java_org_tstrans_webrtc_WhepServer_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        let Some(srv) = server(env, handle) else {
            return JObject::null();
        };
        let s = srv.stats();
        // Every u64 crosses as its bit pattern in a Java long.
        env.new_object(
            FindClass(jni_str!("org/tstrans/webrtc/ServerStats")),
            jni_sig!("(JJJJJJJJJJJ)V"),
            &[
                JValue::Long(s.mounts as i64),
                JValue::Long(s.active_sessions as i64),
                JValue::Long(s.sessions_total as i64),
                JValue::Long(s.sessions_rejected_cap as i64),
                JValue::Long(s.sessions_rejected_codec as i64),
                JValue::Long(s.sessions_rejected_auth as i64),
                JValue::Long(s.sessions_reaped_ice_timeout as i64),
                JValue::Long(s.sessions_reaped_disconnected as i64),
                JValue::Long(s.rtp_packets_sent as i64),
                JValue::Long(s.rtp_bytes_sent as i64),
                JValue::Long(s.datagrams_unrouted as i64),
            ],
        )
        .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `WhepServer.nLocalAddr(handle)` → the signalling address as "ip:port".
    fn Java_org_tstrans_webrtc_WhepServer_nLocalAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JString<'local> = JString::null() => {
        let Some(srv) = server(env, handle) else {
            return JString::null();
        };
        env.new_string(srv.local_addr().to_string())
            .unwrap_or_else(|_| JString::null())
    }
}

jni_native! {
    /// `WhepServer.nMediaAddr(handle)` → the media address as "ip:port".
    fn Java_org_tstrans_webrtc_WhepServer_nMediaAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JString<'local> = JString::null() => {
        let Some(srv) = server(env, handle) else {
            return JString::null();
        };
        env.new_string(srv.media_addr().to_string())
            .unwrap_or_else(|_| JString::null())
    }
}

jni_native! {
    /// `WhepServer.nStop(handle)` — idempotent and bounded.
    fn Java_org_tstrans_webrtc_WhepServer_nStop(env, _class, handle: jlong) = () => {
        if let Some(srv) = server(env, handle) {
            srv.stop();
        }
    }
}

jni_native! {
    /// `WhepServer.nClose(handle)` — stop, then drop the registry's `Arc`.
    /// Idempotent: a second close finds the id gone.
    fn Java_org_tstrans_webrtc_WhepServer_nClose(_env, _class, handle: jlong) = () => {
        if let Some(srv) = REGISTRY_SERVER.close(handle as u64) {
            srv.stop();
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only viewer probe (feature = "jni-test-hooks"): tst-webrtc's own test
// viewer (str0m in frame mode) plays a mount that Java feeds, so the JVM test
// proves media reaches a real WebRTC peer. Never compiled into the shipped JAR.
// ---------------------------------------------------------------------------

#[cfg(feature = "jni-test-hooks")]
#[path = "../../../../crates/tst-webrtc/tests/fixtures/mod.rs"]
#[allow(dead_code, unexpected_cfgs, clippy::expect_used, clippy::unreachable)]
mod whep_fixtures;

jni_native! {
    /// `org.tstrans.internal.WebrtcProbe.nLoopbackViewerProbe(serverHandle, mount)`
    /// → the frames one viewer received: it joins `mount`, waits up to 5 s to
    /// connect and up to 5 s for a first frame, then returns. A failed join
    /// panics, which reaches Java as a `RuntimeException`.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_WebrtcProbe_nLoopbackViewerProbe<'local>(
        env,
        _class,
        server_handle: jlong,
        mount: JString<'local>,
    ) -> jint = 0 => {
        use std::time::Duration;
        let Some(srv) = server(env, server_handle) else {
            return 0;
        };
        let Some(mount) = read_string(env, &mount) else {
            return 0;
        };
        let mut v = whep_fixtures::TestViewer::new();
        whep_fixtures::join(srv.local_addr(), &mount, &mut v, false);
        let connected = v.pump(|v| v.connected, Duration::from_secs(5));
        assert!(connected, "the viewer did not connect: {}", v.trace());
        v.pump(|v| !v.frames.is_empty(), Duration::from_secs(5));
        i32::try_from(v.frames.len()).unwrap_or(i32::MAX)
    }
}
