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
//! inside a push (the snapshot-read rule, without an `OwnedRegistry`).

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JByteArray, JObject, JString, JValue};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong, jobject, jstring};

use tst_core::publisher::{Publisher, PublisherStats};
use tst_core::url::redact_url;
use tst_hls::{HlsError, HlsMode, HlsPublisher, HlsPublisherBuilder};
use tst_pipeline::binding::{BindingError, BindingErrorKind};

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
    env: &mut Env,
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
fn opt_string(env: &mut Env, s: &JString) -> Option<Option<String>> {
    if s.is_null() {
        return Some(None);
    }
    match s.try_to_string(env) {
        Ok(js) => Some(Some(js)),
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(e.to_string()),
            );
            None
        }
    }
}

/// `org.tstrans.hls.PublisherStats(JJJJ)` — optional durations as `-1`.
pub(crate) fn build_publisher_stats<'local>(
    env: &mut Env<'local>,
    s: &PublisherStats,
) -> jni::errors::Result<JObject<'local>> {
    env.ensure_local_capacity(4)?;
    let opt = |d: Option<Duration>| d.map(|d| d.as_micros() as i64).unwrap_or(-1);
    env.new_object(
        FindClass(jni_str!("org/tstrans/hls/PublisherStats")),
        jni_sig!("(JJJJ)V"),
        &[
            JValue::Long(s.segments_written as i64),
            JValue::Long(s.bytes_written as i64),
            JValue::Long(opt(s.current_segment_age)),
            JValue::Long(opt(s.last_segment_duration)),
        ],
    )
}

jni_native! {
    /// `nBuild(url, bind, outputDir, segmentDurationMs, maxSegmentDurationMs,
    /// playlistWindow, mode, authUser, authPass, tlsCert, tlsKey)`.
    /// Sentinels: `null` string = unset; `0` ms = unset; `-1` window/mode = unset.
    /// Order = tst-py: `from_url` seeds, every set field overlays.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_hls_HlsPublisher_nBuild<'local>(
        env,
        _class,
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
    ) -> jlong = 0 => {
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
                    let message = redact_url(&e.to_string());
                    hls_error(env, BindingError::new(BindingErrorKind::HlsUrl, message));
                    return 0;
                }
            },
            None => HlsPublisherBuilder::new(),
        };
        if let Some(addr) = bind {
            match addr.parse::<SocketAddr>() {
                Ok(a) => {
                    b.bind(a);
                }
                Err(e) => {
                    let _ = env.throw_new(
                        FindClass(jni_str!("java/lang/IllegalArgumentException")),
                        JNIString::from(format!("invalid bind address {:?}: {e}", redact_url(&addr))),
                    );
                    return 0;
                }
            }
        }
        if let Some(d) = output_dir {
            b.output_dir(d);
        }
        if segment_duration_ms > 0 {
            b.segment_duration(Duration::from_millis(segment_duration_ms as u64));
        }
        if max_segment_duration_ms > 0 {
            b.max_segment_duration(Duration::from_millis(max_segment_duration_ms as u64));
        }
        if playlist_window >= 0 {
            b.playlist_window(playlist_window as usize);
        }
        // Order pinned by OrdinalContractTest.
        match mode {
            -1 => {}
            0 => {
                b.mode(HlsMode::Live);
            }
            1 => {
                b.mode(HlsMode::Event);
            }
            2 => {
                b.mode(HlsMode::Vod);
            }
            other => {
                let _ = env.throw_new(
                    FindClass(jni_str!("java/lang/IllegalArgumentException")),
                    JNIString::from(format!("invalid HlsMode ordinal {other}")),
                );
                return 0;
            }
        }
        if let (Some(u), Some(p)) = (auth_user, auth_pass) {
            b.basic_auth(u, p);
        }
        if let (Some(c), Some(k)) = (tls_cert, tls_key) {
            b.enable_tls(c, k);
        }
        match b.build() {
            Ok(p) => register(p),
            Err(e) => {
                hls_error(env, e);
                0
            }
        }
    }
}

jni_native! {
    /// `nLocalAddr(handle)` — `"ip:port"` or `null` when built without a server.
    fn Java_org_tstrans_hls_HlsPublisher_nLocalAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jstring = std::ptr::null_mut() => {
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
    }
}

jni_native! {
    fn Java_org_tstrans_hls_HlsPublisher_nPushTs<'local>(
        env,
        _class,
        handle: jlong,
        ts_bytes: JByteArray<'local>,
    ) = () => {
        let Some(buf) = read_bytes(env, &ts_bytes) else {
            return;
        };
        with_pub(env, handle, (), |p| Publisher::push_ts(p, &buf));
    }
}

jni_native! {
    fn Java_org_tstrans_hls_HlsPublisher_nCutSegment<'local>(env, _class, handle: jlong) = () => {
        with_pub(env, handle, (), Publisher::cut_segment);
    }
}

jni_native! {
    fn Java_org_tstrans_hls_HlsPublisher_nCutSegmentWithDuration<'local>(
        env,
        _class,
        handle: jlong,
        media_duration_us: jlong,
    ) = () => {
        if media_duration_us < 0 {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/IllegalArgumentException")),
                jni_str!("mediaDurationUs must be >= 0"),
            );
            return;
        }
        let d = Duration::from_micros(media_duration_us as u64);
        with_pub(env, handle, (), |p| {
            Publisher::cut_segment_with_duration(p, d)
        });
    }
}

jni_native! {
    /// `nFinish(handle)` — take + consume. The Java side has zeroed its handle
    /// already; a miss here is the "already consumed" case.
    fn Java_org_tstrans_hls_HlsPublisher_nFinish<'local>(env, _class, handle: jlong) = () => {
        match REGISTRY.close(handle as u64) {
            Some(p) => {
                if let Err(e) = Publisher::finish(p) {
                    hls_error(env, e);
                }
            }
            None => throw_closed(env, "HlsPublisher"),
        }
    }
}

jni_native! {
    fn Java_org_tstrans_hls_HlsPublisher_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        match REGISTRY.with(handle as u64, |p| Publisher::stats(p)) {
            Some(s) => build_publisher_stats(env, &s)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_hls_HlsPublisher_nHlsStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        match REGISTRY.with(handle as u64, |p| p.hls_stats()) {
            Some(s) => crate::generated::stats_hls::build_hls_stats(env, &s)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_hls_HlsPublisher_nRenderPlaylist<'local>(
        env,
        _class,
        handle: jlong,
        is_event: jboolean,
    ) -> jstring = std::ptr::null_mut() => {
        match REGISTRY.with(handle as u64, |p| p.render_playlist(is_event)) {
            Some(text) => env
                .new_string(text)
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsPublisher");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `nClose(handle)` — quiet: take + finish, errors dropped, idempotent.
    fn Java_org_tstrans_hls_HlsPublisher_nClose<'local>(_env, _class, handle: jlong) = () => {
        if let Some(p) = REGISTRY.close(handle as u64) {
            let _ = Publisher::finish(p);
        }
    }
}

jni_native! {
    /// `nFinishServing(handle)` — take + consume into an `HlsServerHandle`;
    /// returns the new server handle key, or `0` with a pending exception.
    fn Java_org_tstrans_hls_HlsPublisher_nFinishServing<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        match REGISTRY.close(handle as u64) {
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
        }
    }
}
