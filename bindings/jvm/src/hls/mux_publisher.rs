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
//!
//! `nStats` reads the shell's statistics handle, kept beside the registry
//! under the shell's key, so it never takes the entry's lock, which a push
//! in progress holds. `nPublisherStats` still takes it (the publisher's own
//! record lives behind that lock).

use crate::panic::jni_native;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use jni::Env;
use jni::objects::{JBooleanArray, JByteArray, JIntArray};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_core::mpegts::common::Pts90khz;
use tst_hls::{HlsError, HlsPublisher};
use tst_pipeline::{MuxPublisher as RustMuxPublisher, MuxPublisherError, MuxPublisherStatsHandle};

use crate::error::throw_closed;
use crate::handle::HandleRegistry;
use crate::jutil::{checked_u8, read_bytes};
use crate::mpegts::muxer::{build_misp, build_muxer_config_from_arrays};

use super::errors::mux_publisher_error;
use super::publisher::{REGISTRY as REGISTRY_HLS, build_publisher_stats, register as register_hls};

type Inner = RustMuxPublisher<HlsPublisher>;

static REGISTRY: LazyLock<HandleRegistry<Inner>> = LazyLock::new(HandleRegistry::new);

/// The statistics handle of each live shell, under the shell's `REGISTRY` key:
/// inserted with the shell, removed before the shell is taken by a close or a
/// `finishIntoPublisher`. Held only for a lookup, never across a push.
static STATS: LazyLock<Mutex<HashMap<u64, MuxPublisherStatsHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn stats_table() -> std::sync::MutexGuard<'static, HashMap<u64, MuxPublisherStatsHandle>> {
    STATS.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_shell(
    env: &mut Env,
    handle: jlong,
    op: impl FnOnce(&Inner) -> Result<(), MuxPublisherError<HlsError>>,
) {
    match REGISTRY.with(handle as u64, |inner| op(inner)) {
        Some(Ok(())) => {}
        Some(Err(e)) => mux_publisher_error(env, e),
        None => throw_closed(env, "MuxPublisher"),
    }
}

jni_native! {
    /// `nWithConfigHls(publisherHandle, ...programConfig...)` — consume the
    /// publisher, build the shell. The Java side passes the LIVE handle and
    /// zeroes it only after this returns nonzero, so the `REGISTRY_HLS.close`
    /// below is the single atomic claim; a registry miss here means a
    /// concurrent `close()`/`finish()`/`withConfigHls` on the same publisher won
    /// the race.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_hls_MuxPublisher_nWithConfigHls<'local>(
        env,
        _class,
        publisher_handle: jlong,
        program_number: jint,
        pmt_pid: jint,
        pcr_pid: jint,
        pcr_interval_ms: jint,
        pcr_lead_ms: jint,
        psi_interval_ms: jint,
        buffer_packets: jint,
        av1_carriage: jint,
        transport_stream_id: jint,
        psi_version_base: jint,
        stream_pids: JIntArray<'local>,
        stream_kinds: JIntArray<'local>,
        stream_codecs: JIntArray<'local>,
        stream_type_codes: JIntArray<'local>,
        stream_carries_pts: JBooleanArray<'local>,
        data_desc_bytes: JByteArray<'local>,
        data_desc_lens: JIntArray<'local>,
    ) -> jlong = 0 => {
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
            pcr_lead_ms,
            psi_interval_ms,
            buffer_packets,
            av1_carriage,
            transport_stream_id,
            psi_version_base,
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
            Ok(shell) => {
                let stats = shell.stats_handle();
                let id = REGISTRY.insert(shell);
                stats_table().insert(id, stats);
                id as jlong
            }
            Err(e) => {
                mux_publisher_error(env, e);
                0
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nSendVideo<'local>(
        env,
        _class,
        handle: jlong,
        nal: JByteArray<'local>,
        pts: jlong,
        key_frame: jboolean,
    ) = () => {
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_shell(env, handle, |s| {
            s.push_video(&buf, Pts90khz::new(pts), key_frame)
        });
    }
}

jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nSendVideoWithDts<'local>(
        env,
        _class,
        handle: jlong,
        nal: JByteArray<'local>,
        pts: jlong,
        dts: jlong,
        key_frame: jboolean,
    ) = () => {
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_shell(env, handle, |s| {
            s.push_video_with_dts(&buf, Pts90khz::new(pts), Pts90khz::new(dts), key_frame)
        });
    }
}

// `stream_index` is the metadata service id written into the Metadata AU cell
// for synchronous carriage; it is ignored for PrivateData and does not select
// a stream.
jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nSendKlv<'local>(
        env,
        _class,
        handle: jlong,
        klv: JByteArray<'local>,
        pts: jlong,
        stream_index: jint,
    ) = () => {
        let Ok(idx) = checked_u8(env, i64::from(stream_index), "streamIndex") else {
            return;
        };
        let Some(buf) = read_bytes(env, &klv) else {
            return;
        };
        with_shell(env, handle, |s| s.push_klv(&buf, Pts90khz::new(pts), idx));
    }
}

jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nSendAudio<'local>(
        env,
        _class,
        handle: jlong,
        frames: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &frames) else {
            return;
        };
        with_shell(env, handle, |s| s.push_audio(&buf, Pts90khz::new(pts)));
    }
}

jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nSendSubtitle<'local>(
        env,
        _class,
        handle: jlong,
        payload: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &payload) else {
            return;
        };
        with_shell(env, handle, |s| s.push_subtitle(&buf, Pts90khz::new(pts)));
    }
}

jni_native! {
    /// `nSendVideoMisp(handle, nal, pts, keyFrame, mispKind, timeStatus, value)`:
    /// the single video stream's push with a MISP SEI; the scalars are the
    /// `MispTimestamp` fields `Muxer.pushVideoMispTo` passes.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_hls_MuxPublisher_nSendVideoMisp<'local>(
        env,
        _class,
        handle: jlong,
        nal: JByteArray<'local>,
        pts: jlong,
        key_frame: jboolean,
        misp_kind: jint,
        time_status: jint,
        value: jlong,
    ) = () => {
        let Some(misp) = build_misp(env, misp_kind, time_status, value) else {
            return;
        };
        let Some(buf) = read_bytes(env, &nal) else {
            return;
        };
        with_shell(env, handle, |s| {
            s.push_video_misp(&buf, Pts90khz::new(pts), key_frame, &misp)
        });
    }
}

jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nSendData<'local>(
        env,
        _class,
        handle: jlong,
        data: JByteArray<'local>,
        pts: jlong,
    ) = () => {
        let Some(buf) = read_bytes(env, &data) else {
            return;
        };
        with_shell(env, handle, |s| s.push_data(&buf, Pts90khz::new(pts)));
    }
}

jni_native! {
    fn Java_org_tstrans_hls_MuxPublisher_nCutSegment<'local>(env, _class, handle: jlong) = () => {
        with_shell(env, handle, |s| s.cut_segment());
    }
}

jni_native! {
    /// `nFinishIntoPublisher(handle)` — consume the shell; the inner publisher
    /// becomes a fresh `HlsPublisher` handle (its key is returned).
    fn Java_org_tstrans_hls_MuxPublisher_nFinishIntoPublisher<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        stats_table().remove(&(handle as u64));
        match REGISTRY.close(handle as u64) {
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
        }
    }
}

jni_native! {
    /// `nStats(handle)` — the shell's counters, read through its statistics
    /// handle: never waits for a push in progress.
    fn Java_org_tstrans_hls_MuxPublisher_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        let stats = stats_table().get(&(handle as u64)).map(MuxPublisherStatsHandle::stats);
        match stats {
            Some(st) => crate::generated::stats_hls::build_mux_publisher_stats(env, &st)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "MuxPublisher");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `nPublisherStats(handle)` — the publisher's own record; waits for a
    /// push in progress, which holds the shell.
    fn Java_org_tstrans_hls_MuxPublisher_nPublisherStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        match REGISTRY.with(handle as u64, |s| s.publisher_stats()) {
            Some(st) => build_publisher_stats(env, &st)
                .map(|o| o.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "MuxPublisher");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `nClose(handle)` — quiet: finish the shell AND the inner publisher, errors
    /// dropped, idempotent.
    fn Java_org_tstrans_hls_MuxPublisher_nClose<'local>(_env, _class, handle: jlong) = () => {
        stats_table().remove(&(handle as u64));
        if let Some(shell) = REGISTRY.close(handle as u64)
            && let Ok(publisher) = shell.finish() {
                let _ = tst_core::publisher::Publisher::finish(publisher);
            }
    }
}
