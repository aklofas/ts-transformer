//! JNI surface for `org.tstrans.hls.MuxPublisher` — wraps
//! `tst_pipeline::MuxPublisher<tst_hls::HlsPublisher>` (a `Muxer` + the HLS
//! sink in one shell). Ports tst-py's `bindings/python/src/hls/mux_publisher.rs`.
//!
//! `nWithConfigHls` builds the `MuxerConfig` through the shared parallel-array
//! helper `MuxSender.nFromUrl` uses FIRST, then CONSUMES the `HlsPublisher`
//! handle (taken through `publisher::REGISTRY.close`, the same path `finish`
//! uses) — that `close` is the single atomic claim, on both the Rust and the
//! Java side: the Java caller passes its handle live and only zeroes it once
//! this call has returned a nonzero shell handle, so a rejected config never
//! consumes the publisher. `nFinishIntoPublisher` consumes the shell and
//! re-registers the inner publisher as a fresh `HlsPublisher` handle. Sends
//! take `&self` (the shell serialises internally), so they lease with the
//! non-poisoning `with`.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::{JBooleanArray, JByteArray, JClass, JIntArray, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_core::mpegts::common::Pts90khz;
use tst_hls::{HlsError, HlsPublisher};
use tst_pipeline::{MuxPublisher as RustMuxPublisher, MuxPublisherError, MuxPublisherStats};

use crate::error::throw_closed;
use crate::handle::HandleRegistry;
use crate::jutil::{checked_u8, read_bytes};
use crate::mpegts::muxer::build_muxer_config_from_arrays;

use super::errors::mux_publisher_error;
use super::publisher::{REGISTRY as REGISTRY_HLS, build_publisher_stats, register as register_hls};

type Inner = RustMuxPublisher<HlsPublisher>;

static REGISTRY: LazyLock<HandleRegistry<Inner>> = LazyLock::new(HandleRegistry::new);

fn with_shell(
    env: &mut JNIEnv,
    handle: jlong,
    op: impl FnOnce(&Inner) -> Result<(), MuxPublisherError<HlsError>>,
) {
    match REGISTRY.with(handle as u64, |inner| op(inner)) {
        Some(Ok(())) => {}
        Some(Err(e)) => mux_publisher_error(env, e),
        None => throw_closed(env, "MuxPublisher"),
    }
}

fn build_mux_publisher_stats<'local>(
    env: &mut JNIEnv<'local>,
    s: &MuxPublisherStats,
) -> jni::errors::Result<jni::objects::JObject<'local>> {
    env.ensure_local_capacity(4)?;
    env.new_object(
        "org/tstrans/hls/MuxPublisherStats",
        "(JJJ)V",
        &[
            JValue::Long(s.bytes_pushed as i64),
            JValue::Long(s.drain_calls as i64),
            JValue::Long(s.cut_calls as i64),
        ],
    )
}

/// `nWithConfigHls(publisherHandle, ...programConfig...)` — consume the
/// publisher, build the shell. The Java side passes the LIVE handle and
/// zeroes it only after this returns nonzero, so the `REGISTRY_HLS.close`
/// below is the single atomic claim; a registry miss here means a
/// concurrent `close()`/`finish()`/`withConfigHls` on the same publisher won
/// the race.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nWithConfigHls<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    publisher_handle: jlong,
    program_number: jint,
    pmt_pid: jint,
    pcr_pid: jint,
    pcr_interval_ms: jint,
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
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Config FIRST: a rejected config returns before the take, so the
        // publisher stays usable (the Java side now also reads the handle
        // live and claims it only after this call succeeds — see
        // MuxPublisher.withConfigHls).
        let cfg = match build_muxer_config_from_arrays(
            env,
            program_number,
            pmt_pid,
            pcr_pid,
            pcr_interval_ms,
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
        ) {
            Ok(c) => c,
            Err(()) => return 0,
        };
        let Some(publisher) = REGISTRY_HLS.close(publisher_handle as u64) else {
            throw_closed(env, "HlsPublisher");
            return 0;
        };
        match RustMuxPublisher::with_config(publisher, cfg) {
            Ok(shell) => REGISTRY.insert(shell) as jlong,
            Err(e) => {
                mux_publisher_error(env, e);
                0
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nSendVideo<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    nal: JByteArray<'local>,
    pts: jlong,
    key_frame: jboolean,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_shell(env, handle, |s| {
            s.send_video(&buf, Pts90khz::new(pts), key_frame != 0)
        });
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nSendKlv<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    klv: JByteArray<'local>,
    pts: jlong,
    stream_index: jint,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Ok(idx) = checked_u8(env, i64::from(stream_index), "streamIndex") else {
            return;
        };
        let Some(buf) = read_bytes(env, &klv) else {
            return;
        };
        with_shell(env, handle, |s| s.send_klv(&buf, Pts90khz::new(pts), idx));
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nSendAudio<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    frames: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_shell(env, handle, |s| s.send_audio(&buf, Pts90khz::new(pts)));
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nSendSubtitle<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    payload: JByteArray<'local>,
    pts: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_shell(env, handle, |s| s.send_subtitle(&buf, Pts90khz::new(pts)));
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nCutSegment<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        with_shell(env, handle, |s| s.cut_segment());
    })
}

/// `nFinishIntoPublisher(handle)` — consume the shell; the inner publisher
/// becomes a fresh `HlsPublisher` handle (its key is returned).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nFinishIntoPublisher<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| match REGISTRY.close(handle as u64) {
        Some(shell) => match shell.finish() {
            Ok(publisher) => register_hls(publisher),
            Err(e) => {
                mux_publisher_error(env, e);
                0
            }
        },
        None => {
            throw_closed(env, "MuxPublisher");
            0
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |s| s.stats()) {
            Some(st) => build_mux_publisher_stats(env, &st)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "MuxPublisher");
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nPublisherStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |s| s.publisher_stats()) {
            Some(st) => build_publisher_stats(env, &st)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "MuxPublisher");
                std::ptr::null_mut()
            }
        }
    })
}

/// `nClose(handle)` — quiet: finish the shell AND the inner publisher, errors
/// dropped, idempotent.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_MuxPublisher_nClose<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        if let Some(shell) = REGISTRY.close(handle as u64) {
            if let Ok(publisher) = shell.finish() {
                let _ = tst_core::publisher::Publisher::finish(publisher);
            }
        }
    })
}
