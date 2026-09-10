//! JNI surface for `org.tstrans.rtp.DemuxReceiver` — the single-call convenience
//! wrapper that owns a `Demuxer` + an RTP `RtpRecvTransport`.
//!
//! Wraps `tst_pipeline::DemuxReceiver<tst_rtp::RtpRecvTransport>`: bind a UDP RTP
//! receiver to a URL, demux the resulting MPEG-TS stream, and iterate over
//! `DemuxEvent` instances. Ports tst-py's
//! `bindings/python/src/rtp/demux_receiver.rs`.
//!
//! Concurrency (the headline rtp divergence from the srt JVM DemuxReceiver):
//! `inner` is held under `Arc<Mutex<Option<...>>>` and `cancel` is held OUTSIDE
//! the mutex. `nNext` locks `inner` inside the call, runs `recv_event`, drops the
//! guard, then drains `sink_error`. `nClose` fires `cancel` FIRST (waking a parked
//! `recv_event` within ~100 ms WITHOUT taking the lock), then locks + takes +
//! drops `inner`. This makes `close()` a safe cross-thread stop for a parked
//! iteration — necessary because the rtp convenience wrapper exposes NO public
//! cancel handle (matching tst-py) and RTP/UDP has no connection-close signal to
//! end iteration. The srt JVM DemuxReceiver's no-mutex model is deliberately NOT
//! reused (it relied on a public cancel handle + SRT connection-close).
//!
//! `add_byte_sink` discipline: the registered `Box<dyn FnMut(&[u8]) + Send>` runs
//! on the receiver's own thread inside `recv_event`, attaches to the JVM, and
//! upcalls a `Consumer<byte[]>`. It holds NO Java monitor across the upcall and
//! touches ONLY the captured `consumer` + `sink_error` — never `inner` (so it
//! cannot deadlock against the `inner` lock the parked `nNext` holds: a callback
//! that re-entered the receiver is documented as forbidden, and a concurrent
//! `close()` fires `cancel` before acquiring the lock). A callback exception is
//! captured first-wins into `sink_error` (as a `GlobalRef`) and re-thrown from the
//! next `nNext` after the `inner` guard is dropped.

use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

use jni::JNIEnv;
use jni::objects::{GlobalRef, JClass, JObject, JString, JThrowable, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_core::mpegts::demux::DemuxEvent;
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};
use tst_rtp::builder::RtpRecvSocketBuilder;
use tst_rtp::{RtpRecvTransport, StreamEndReasonHandle};

use crate::handle::HandleRegistry;
use crate::jutil::build_socket_stats;
use crate::mpegts::{
    build_demux_config_from_args, build_muxer_stats, convert_event, throw_demux_error,
};

use super::errors::{connect_error_to_rtp, throw_rtp, transport_error_to_rtp};
use super::mux_sender::build_rtp_transport_stats;

/// Native backing for `org.tstrans.rtp.DemuxReceiver`. The registry's
/// `Mutex<Option<Self>>` holds `inner` (replacing the round-1 bespoke
/// `Arc<Mutex<Option<…>>>`), and the registry's cancel hook (registered with a
/// clone of `cancel`) wakes a parked `recv_event` before `close` takes the
/// resource. `sink_error` is the fail-loud byte-sink re-raise slot, shared with
/// the byte-sink closure (which runs on the receiver thread inside recv_event).
struct JniRtpDemuxReceiver {
    inner: RustDemuxReceiver<RtpRecvTransport>,
    /// Pulled from the underlying `RtpRecvTransport::end_reason_handle()`
    /// BEFORE it moves into `RustDemuxReceiver::new`/`with_demux_options`
    /// below (the shell, generic over `RecvTransport`, has no
    /// `end_reason_handle()` delegate of its own) — same capture-before-move
    /// shape as `end_reason` on `transport::JniRtpReceiver`. See that
    /// module's `end_reason` doc for why `nClose` (not the getters) builds
    /// the post-close snapshot.
    end_reason: StreamEndReasonHandle,
    sink_error: Arc<Mutex<Option<GlobalRef>>>,
}

/// Per-type leased-handle registry for `org.tstrans.rtp.DemuxReceiver`. Registers
/// a cancel hook so a cross-thread `close()` wakes a parked `recv_event` (the
/// headline rtp divergence — there is no public cancel handle).
static REGISTRY: LazyLock<HandleRegistry<JniRtpDemuxReceiver>> = LazyLock::new(HandleRegistry::new);

/// Map a `DemuxReceiverError` raised by `recv_event` onto a thrown Java exception.
/// Transport-side → `RtpException`; demux-side → `DemuxException`. Mirrors tst-py's
/// `demux_recv_error_to_pyerr`.
fn throw_demux_recv_error(env: &mut JNIEnv, e: &DemuxReceiverError) {
    match &e.source {
        DemuxReceiverErrorSource::Transport(t) => transport_error_to_rtp(env, t),
        DemuxReceiverErrorSource::Demux(d) => throw_demux_error(env, d),
        // `DemuxReceiverErrorSource` is non-exhaustive; route any future variant
        // through `RtpException(TRANSPORT)` with the Display message preserved.
        _ => throw_rtp(env, "TRANSPORT", &e.to_string()),
    }
}

/// Build a `JniRtpDemuxReceiver` from an `rtp://` URL (with or without explicit
/// demux options). Returns the boxed handle as `jlong`, or `0` with a pending
/// exception on any failure.
fn build_from_url(
    env: &mut JNIEnv,
    url: &JString,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> jlong {
    let url_str: String = match env.get_string(url) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            return 0;
        }
    };

    let builder = match RtpRecvSocketBuilder::from_url(&url_str) {
        Ok(b) => b,
        Err(e) => {
            throw_rtp(env, "TRANSPORT", &e.to_string());
            return 0;
        }
    };
    let transport = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            connect_error_to_rtp(env, &e);
            return 0;
        }
    };

    demux_receiver_handle_from_transport(transport, opts)
}

/// Build a `JniRtpDemuxReceiver` handle from an already-constructed
/// `RtpRecvTransport` (used by the RTSP client's `intoDemuxReceiver` — it hands
/// off the SETUP-time recv transport). Mirrors tst-py's
/// `PyDemuxReceiver::from_recv_transport[_with_config]`. Infallible (box alloc).
pub(crate) fn demux_receiver_handle_from_transport(
    transport: RtpRecvTransport,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> jlong {
    // Pulled BEFORE `transport` moves into the pipeline shell below — see
    // the `end_reason` field doc for why. `transport` here already carries
    // the owning RtspClient's shared end-reason slot when this is reached
    // via an RTSP-session conversion (PR-A wired that into the transport
    // before handing it off), so this handle observes reasons recorded by
    // the RTSP keepalive/pump threads too, not just this receiver's own
    // close — no extra plumbing needed for that path.
    let end_reason = transport.end_reason_handle();
    let receiver = match opts {
        None => RustDemuxReceiver::new(transport),
        Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
    };
    // The cancel handle drives the registry's cancel hook (wakes a parked recv on
    // close); it is NOT stored in the struct.
    let cancel = receiver
        .cancel_handle()
        .expect("RtpRecvTransport always returns Some(cancel_handle)");
    let jdr = JniRtpDemuxReceiver {
        inner: receiver,
        end_reason,
        sink_error: Arc::new(Mutex::new(None)),
    };
    REGISTRY.insert_with_cancel(jdr, Some(Box::new(move || cancel.cancel()))) as jlong
}

/// `DemuxReceiver.nFromUrl(url)` — default demux options.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nFromUrl<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| build_from_url(env, &url, None))
}

/// `DemuxReceiver.nFromUrlWithConfig(url, ...)` — explicit `DemuxerConfig`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nFromUrlWithConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
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
        let Some(opts) = build_demux_config_from_args(
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
        build_from_url(env, &url, Some(opts))
    })
}

/// `nNext(handle)` — block until the next `DemuxEvent`. Java `null` on clean end
/// (receiver closed / drained → iterator `hasNext` returns false). Throws
/// `RtpException` / `DemuxException` on a recv-side error, or re-raises a captured
/// byte-sink exception fail-loud.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nNext<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // recv_event runs INSIDE the registry lease — the closure holds the entry's
        // resource lock for the duration of the (possibly parked) recv. A concurrent
        // `close()` fires the cancel hook (waking the parked recv) BEFORE it takes the
        // resource lock, so close is a safe cross-thread stop. `None` = the resource
        // was already taken by close → clean end of iteration. We clone `sink_error`
        // out so it is drained AFTER the lease releases.
        let leased: Option<(Result<Option<DemuxEvent>, DemuxReceiverError>, _)> = REGISTRY
            .with_poisoning(handle as u64, |jdr| {
                (jdr.inner.recv_event(), jdr.sink_error.clone())
            });
        let Some((res, sink_error)) = leased else {
            // Closed/absent: a closed-during-iteration is the clean end of iteration
            // (the iterator's hasNext returns false); an outright-absent handle is the
            // same observable null. Match the round-1 contract: no throw on a closed
            // receiver mid-iteration.
            return JObject::null().into_raw();
        };

        // Fail-loud: surface any byte-sink exception captured during this recv_event
        // BEFORE inspecting `res`. `take()` so a resumed iteration isn't poisoned.
        let captured = sink_error.lock().ok().and_then(|mut s| s.take());
        if let Some(global) = captured {
            if let Ok(local) = env.new_local_ref(&global) {
                let _ = env.throw(JThrowable::from(local));
            }
            return JObject::null().into_raw();
        }

        match res {
            Ok(None) => JObject::null().into_raw(),
            Ok(Some(ev)) => match convert_event(env, &ev) {
                Ok(Some(obj)) => obj.into_raw(),
                // Forward-compat guard (all current variants build a record).
                Ok(None) => JObject::null().into_raw(),
                Err(()) => {
                    crate::error::throw_demux(env, "INTERNAL", "event conversion failed");
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                throw_demux_recv_error(env, &e);
                JObject::null().into_raw()
            }
        }
    })
}

/// `nAddByteSink(handle, consumer)` — register a per-188-byte `Consumer<byte[]>`.
/// The closure attaches to the JVM and upcalls with NO Java monitor held; it
/// touches only `consumer` + `sink_error`. See the module doc.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nAddByteSink<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    consumer: JObject<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        // Build the vm + consumer global ref BEFORE leasing (these touch `env`).
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return;
            }
        };
        let consumer = match env.new_global_ref(&consumer) {
            Ok(g) => g,
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return;
            }
        };

        // Register the sink under the registry lease. If a concurrent `nNext` holds
        // the resource lock, this blocks until it yields (registration is append-only,
        // no re-entry into the JVM here).
        let registered = REGISTRY.with_poisoning(handle as u64, |jdr| {
            let slot = jdr.sink_error.clone();
            jdr.inner.add_byte_sink(Box::new(move |pkt: &[u8]| {
                // Runs on the receiver's own thread inside recv_event. NO Java monitor is
                // held across this upcall; touches ONLY `consumer` + `slot`, never `inner`.
                let Ok(mut env) = vm.attach_current_thread() else {
                    return;
                };
                let arr = match env.byte_array_from_slice(pkt) {
                    Ok(a) => a,
                    // OOM-only: clear the pending exception before bailing so the next
                    // packet's fanout doesn't run JNI calls under a stale exception.
                    Err(_) => {
                        let _ = env.exception_clear();
                        return;
                    }
                };
                let _ = env.call_method(
                    &consumer,
                    "accept",
                    "(Ljava/lang/Object;)V",
                    &[JValue::Object(&arr.into())],
                );
                // Fail-loud: capture the first callback exception as a GlobalRef; later
                // per-packet errors are dropped. `nNext` drains + re-throws it.
                if env.exception_check().unwrap_or(false) {
                    if let Ok(exc) = env.exception_occurred() {
                        let _ = env.exception_clear();
                        if let Ok(mut s) = slot.lock() {
                            if s.is_none() {
                                if let Ok(g) = env.new_global_ref(&exc) {
                                    *s = Some(g);
                                }
                            }
                        }
                    }
                }
            }));
        });
        if registered.is_none() {
            crate::error::throw_closed(env, "DemuxReceiver");
        }
    })
}

/// `nStats(handle)` — `(SocketStats, MuxerStats)` projection mirroring tst-py's
/// `DemuxReceiver.stats`. Returns null on a JNI builder error (non-fatal).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(combined) = REGISTRY.with(handle as u64, |jdr| jdr.inner.stats()) else {
            crate::error::throw_closed(env, "DemuxReceiver");
            return JObject::null();
        };

        // Partial SocketStats from the pipeline-tracked recv counters (full
        // SocketStats via the transport accessor isn't surfaced through the shell).
        // `SocketStats` is non-exhaustive; populate via mut spread.
        let mut sock = tst_core::transport::SocketStats::default();
        sock.bytes_received = combined.bytes_received;
        sock.packets_received = combined.packets_received;

        let sock_obj = match build_socket_stats(env, "org/tstrans/rtp/SocketStats", &sock) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        // Re-shape the demux side as a MuxerStats projection (mirrors tst-py).
        let mux_obj = match build_muxer_stats(
            env,
            combined.packets_received as i64,
            combined.bytes_received as i64,
            combined.program_maps_seen as i64,
        ) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        match build_rtp_transport_stats(env, &sock_obj, &mux_obj) {
            Ok(o) => o,
            Err(_) => JObject::null(),
        }
    })
}

/// `nLastSeenMicros(handle, pid)` — Unix-epoch microsecond timestamp the stream
/// identified by `pid` last carried a demuxed item through this receiver
/// (last emitted event); `-1` if `pid` was never seen — including an
/// unrecognized PID (no range check beyond the native `u16` truncating cast,
/// same as `pmtPid`/`pcrPid` elsewhere in this binding) — or a timestamp
/// predating the Unix epoch. Boxed to `Long` (`null` for `-1`) at the Java
/// layer. Same registry-lock discipline as `nStats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nLastSeenMicros(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    pid: jint,
) -> jlong {
    crate::panic::jni_catch(&mut env, -1, |env| {
        let Some(last_seen) = REGISTRY.with(handle as u64, |jdr| {
            jdr.inner
                .stats()
                .per_stream
                .get(&(pid as u16))
                .and_then(|s| s.last_seen)
        }) else {
            crate::error::throw_closed(env, "DemuxReceiver");
            return -1;
        };
        last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as i64)
            .unwrap_or(-1)
    })
}

/// `nEndReason(handle)` — why the receive session ended, as the wire-pinned
/// ordinal (see `end_reason`'s module doc); `-1` if it hasn't ended yet, or
/// on a closed/absent handle (the closed case never reaches this native —
/// `DemuxReceiver.endReason()` reads the Java-side snapshot once
/// `peekHandle()` is 0).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nEndReason(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jint {
    crate::panic::jni_catch(&mut env, -1, |_env| {
        REGISTRY
            .with(handle as u64, |jdr| {
                super::end_reason::end_reason_ordinal(jdr.end_reason.get().as_ref())
            })
            .unwrap_or(-1)
    })
}

/// `nEndDetail(handle)` — free-text detail for `nEndReason`; `null` for a
/// detail-less reason, "hasn't ended yet", or a closed/absent handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nEndDetail<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let detail = REGISTRY
            .with(handle as u64, |jdr| {
                jdr.end_reason
                    .get()
                    .and_then(|r| super::end_reason::end_reason_detail(&r).map(str::to_owned))
            })
            .flatten();
        match detail {
            Some(d) => match env.new_string(&d) {
                Ok(s) => s.into(),
                Err(_) => JObject::null(),
            },
            None => JObject::null(),
        }
    })
}

/// `nClose(handle)` — cancel-first, then take + drop the inner receiver and free
/// the box. Cancelling before locking wakes a parked `nNext` so this never
/// deadlocks against it. No-op on a zero handle.
///
/// Returns the close-time `EndReasonSnapshot` (see `end_reason`'s module
/// doc) — computed here, from the resource this call already exclusively
/// owns, since the registry entry (and any further `nEndReason`/
/// `nEndDetail` call on this handle) is gone once this function returns.
/// `null` only on a JNI allocation failure building the snapshot
/// (`nativeClose` null-checks before touching it).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jobject {
    // `REGISTRY.close` fires the cancel hook FIRST (waking any parked recv WITHOUT
    // taking the resource lock), THEN takes + tears down the receiver under the
    // lock — blocking briefly until the woken recv releases it. Atomic +
    // idempotent: a double close finds the id gone → no-op.
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let reason = if let Some(mut jdr) = REGISTRY.close(handle as u64) {
            jdr.inner.close();
            jdr.end_reason.get()
        } else {
            None
        };
        super::end_reason::build_close_snapshot(env, reason)
            .map(|obj| obj.into_raw())
            .unwrap_or(std::ptr::null_mut())
    })
}

/// `nIsAlive(handle)` — whether the receiver owns a live transport. Uses the
/// non-blocking `try_with` so a concurrent parked `nNext` reports "alive" rather
/// than blocking on the resource lock.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_DemuxReceiver_nIsAlive(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        match REGISTRY.try_with(handle as u64, |jdr| u8::from(jdr.inner.is_alive())) {
            crate::handle::TryWith::Ran(v) => v,
            // Locked by a parked recv → the receiver is live.
            crate::handle::TryWith::Locked => 1,
            // Taken/absent → closed.
            crate::handle::TryWith::Taken => 0,
        }
    })
}
