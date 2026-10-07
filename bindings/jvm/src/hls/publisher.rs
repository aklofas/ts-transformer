//! JNI surface for `org.tstrans.hls.HlsPublisher` — wraps `tst_hls::HlsPublisher`.
//!
//! Ports tst-py's `bindings/python/src/hls/publisher.rs`. The handle is a key into
//! a plain `HandleRegistry` (no cancel hook: a push is bounded disk I/O). The
//! mutating natives lease with `with_poisoning` (a panic mid-push tears the
//! resource down, mirroring the C `with_inner_mut` policy); the getters with
//! `with`. `nFinish` / `nFinishServing` take the resource out via
//! `REGISTRY.close` and consume it — the Java side has already zeroed its
//! handle (`consumeHandle()`), so a second call misses the table and throws
//! `IllegalStateException`.
//!
//! `nLocalAddr` is called ONCE by the Java constructor and cached: the bound
//! address cannot change, and a cached value answers while another thread is
//! inside a push (the blocking-slot-readers rule, without an `OwnedRegistry`).

use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jobject, jstring};

use tst_core::publisher::{Publisher, PublisherStats};
use tst_hls::{HlsError, HlsMode, HlsPublisher, HlsPublisherBuilder, HlsStats};

use crate::error::throw_closed;
use crate::handle::HandleRegistry;
use crate::jutil::read_bytes;

use super::errors::hls_error;

pub(crate) static REGISTRY: LazyLock<HandleRegistry<HlsPublisher>> =
    LazyLock::new(HandleRegistry::new);

/// Register an owned publisher; returns its opaque handle. Shared with
/// `MuxPublisher.nFinishIntoPublisher`.
pub(crate) fn register(p: HlsPublisher) -> jlong {
    REGISTRY.insert(p) as jlong
}

/// Lease + run a fallible op. Closed/absent → `IllegalStateException`;
/// `Err(HlsError)` → `HlsException`. Returns `default` on either failure.
fn with_pub<R>(
    env: &mut JNIEnv,
    handle: jlong,
    default: R,
    op: impl FnOnce(&mut HlsPublisher) -> Result<R, HlsError>,
) -> R {
    match REGISTRY.with_poisoning(handle as u64, op) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            hls_error(env, e);
            default
        }
        None => {
            throw_closed(env, "HlsPublisher");
            default
        }
    }
}

/// A nullable Java string. `None` = JNI failure (exception pending).
fn opt_string(env: &mut JNIEnv, s: &JString) -> Option<Option<String>> {
    if s.is_null() {
        return Some(None);
    }
    match env.get_string(s) {
        Ok(js) => Some(Some(js.into())),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            None
        }
    }
}

/// `org.tstrans.hls.PublisherStats(JJJJ)` — optional durations as `-1`.
pub(crate) fn build_publisher_stats<'local>(
    env: &mut JNIEnv<'local>,
    s: &PublisherStats,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    let opt = |d: Option<Duration>| d.map(|d| d.as_micros() as i64).unwrap_or(-1);
    env.new_object(
        "org/tstrans/hls/PublisherStats",
        "(JJJJ)V",
        &[
            JValue::Long(s.segments_written as i64),
            JValue::Long(s.bytes_written as i64),
            JValue::Long(opt(s.current_segment_age)),
            JValue::Long(opt(s.last_segment_duration)),
        ],
    )
}

/// `org.tstrans.hls.HlsStats(JJJJ)`.
fn build_hls_stats<'local>(
    env: &mut JNIEnv<'local>,
    s: &HlsStats,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    env.new_object(
        "org/tstrans/hls/HlsStats",
        "(JJJJ)V",
        &[
            JValue::Long(s.segments_written as i64),
            JValue::Long(s.bytes_pushed_total as i64),
            JValue::Long(s.open_segment_bytes as i64),
            JValue::Long(s.forced_cuts as i64),
        ],
    )
}

/// `nBuild(url, bind, outputDir, segmentDurationMs, maxSegmentDurationMs,
/// playlistWindow, mode, authUser, authPass, tlsCert, tlsKey)`.
/// Sentinels: `null` string = unset; `0` ms = unset; `-1` window/mode = unset.
/// Order = tst-py: `from_url` seeds, every set field overlays.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nBuild<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    bind: JString<'local>,
    output_dir: JString<'local>,
    segment_duration_ms: jlong,
    max_segment_duration_ms: jlong,
    playlist_window: jint,
    mode: jint,
    auth_user: JString<'local>,
    auth_pass: JString<'local>,
    tls_cert: JString<'local>,
    tls_key: JString<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let Some(url) = opt_string(env, &url) else {
            return 0;
        };
        let Some(bind) = opt_string(env, &bind) else {
            return 0;
        };
        let Some(output_dir) = opt_string(env, &output_dir) else {
            return 0;
        };
        let Some(auth_user) = opt_string(env, &auth_user) else {
            return 0;
        };
        let Some(auth_pass) = opt_string(env, &auth_pass) else {
            return 0;
        };
        let Some(tls_cert) = opt_string(env, &tls_cert) else {
            return 0;
        };
        let Some(tls_key) = opt_string(env, &tls_key) else {
            return 0;
        };

        let mut b = match url {
            Some(u) => match HlsPublisherBuilder::from_url(&u) {
                Ok(b) => b,
                Err(e) => {
                    hls_error(env, e);
                    return 0;
                }
            },
            None => HlsPublisherBuilder::new(),
        };
        if let Some(addr) = bind {
            match addr.parse::<SocketAddr>() {
                Ok(a) => b = b.bind(a),
                Err(e) => {
                    let _ = env.throw_new(
                        "java/lang/IllegalArgumentException",
                        format!("invalid bind address {addr:?}: {e}"),
                    );
                    return 0;
                }
            }
        }
        if let Some(d) = output_dir {
            b = b.output_dir(d);
        }
        if segment_duration_ms > 0 {
            b = b.segment_duration(Duration::from_millis(segment_duration_ms as u64));
        }
        if max_segment_duration_ms > 0 {
            b = b.max_segment_duration(Duration::from_millis(max_segment_duration_ms as u64));
        }
        if playlist_window >= 0 {
            b = b.playlist_window(playlist_window as usize);
        }
        match mode {
            -1 => {}
            0 => b = b.mode(HlsMode::Live),
            1 => b = b.mode(HlsMode::Event),
            2 => b = b.mode(HlsMode::Vod),
            other => {
                let _ = env.throw_new(
                    "java/lang/IllegalArgumentException",
                    format!("invalid HlsMode ordinal {other}"),
                );
                return 0;
            }
        }
        if let (Some(u), Some(p)) = (auth_user, auth_pass) {
            b = b.basic_auth(u, p);
        }
        if let (Some(c), Some(k)) = (tls_cert, tls_key) {
            b = b.enable_tls(c, k);
        }
        match b.build() {
            Ok(p) => register(p),
            Err(e) => {
                hls_error(env, e);
                0
            }
        }
    })
}

/// `nLocalAddr(handle)` — `"ip:port"` or `null` when built without a server.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nLocalAddr<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |p| p.local_addr().map(|a| a.to_string())) {
            Some(Some(addr)) => env
                .new_string(addr)
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            Some(None) => std::ptr::null_mut(),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nPushTs<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    ts_bytes: JByteArray<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &ts_bytes) else {
            return;
        };
        with_pub(env, handle, (), |p| Publisher::push_ts(p, &buf));
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nCutSegment<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        with_pub(env, handle, (), Publisher::cut_segment);
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nCutSegmentWithDuration<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    media_duration_us: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        if media_duration_us < 0 {
            let _ = env.throw_new(
                "java/lang/IllegalArgumentException",
                "mediaDurationUs must be >= 0",
            );
            return;
        }
        let d = Duration::from_micros(media_duration_us as u64);
        with_pub(env, handle, (), |p| {
            Publisher::cut_segment_with_duration(p, d)
        });
    })
}

/// `nFinish(handle)` — take + consume. The Java side has zeroed its handle
/// already; a miss here is the "already consumed" case.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nFinish<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| match REGISTRY.close(handle as u64) {
        Some(p) => {
            if let Err(e) = Publisher::finish(p) {
                hls_error(env, e);
            }
        }
        None => throw_closed(env, "HlsPublisher"),
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |p| Publisher::stats(p)) {
            Some(s) => build_publisher_stats(env, &s)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nHlsStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |p| p.hls_stats()) {
            Some(s) => build_hls_stats(env, &s)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nRenderPlaylist<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    is_event: jboolean,
) -> jstring {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |p| p.render_playlist(is_event != 0)) {
            Some(text) => env
                .new_string(text)
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    })
}

/// `nClose(handle)` — quiet: take + finish, errors dropped, idempotent.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nClose<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        if let Some(p) = REGISTRY.close(handle as u64) {
            let _ = Publisher::finish(p);
        }
    })
}

/// `nFinishServing(handle)` — take + consume into an `HlsServerHandle`;
/// returns the new server handle key, or `0` with a pending exception.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsPublisher_nFinishServing<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| match REGISTRY.close(handle as u64) {
        Some(p) => match p.finish_serving() {
            Ok(h) => super::server_handle::register(h),
            Err(e) => {
                hls_error(env, e);
                0
            }
        },
        None => {
            throw_closed(env, "HlsPublisher");
            0
        }
    })
}
