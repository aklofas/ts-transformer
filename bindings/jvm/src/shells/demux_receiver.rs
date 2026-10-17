//! JNI exports for `org.tstrans.shells.DemuxReceiver`: any transport in,
//! `DemuxEvent`s out.
//!
//! Wraps `tst_pipeline::DemuxReceiver<Box<dyn RecvTransport>>`, opened from a
//! URL by `nOpen` / `nOpenWithConfig`, and iterated for `DemuxEvent` objects.
//! The transport's error domain rides in the snapshot (readable after
//! `close()`), and its end-reason cell is attached to the `Owned`.
//!
//! Threading: single-threaded per the `Receiver` model: the Java
//! `DemuxReceiver` is not thread-safe and not `synchronized`. The sanctioned
//! cross-thread stop is `cancelHandle().cancel()` (or `close()`, which cancels
//! first), which wakes a parked `recv_event`.
//!
//! `add_byte_sink` discipline (the load-bearing piece): the registered
//! `Box<dyn FnMut(&[u8]) + Send>` runs on the receiver's own thread inside
//! `recv_event`, attaches to the JVM, and upcalls a `Consumer<byte[]>`. It
//! holds NO Java monitor across the upcall, but it DOES run under this
//! handle's slot: `nNext` holds the slot for the whole `recv_event` call
//! (`REGISTRY.with_mut`) and the sink fires while it is held. A sink that
//! re-enters the same `DemuxReceiver` through any native that takes the slot
//! (`next()`, `stats()`, `socketStats()`, `lastSeenMicros()`, `addByteSink()`)
//! therefore self-deadlocks; the never-re-enter rule in the user guide is
//! load-bearing. `cancelHandle()` / `isAlive()` / `endReason()` stay lock-free
//! and `close()` cancels before it takes the slot, so OTHER threads can still
//! stop a receiver whose sink is running. A callback exception is captured
//! first-wins into `sink_error` (as a `GlobalRef`) and re-thrown from the next
//! `nNext` after the slot is released.

use crate::panic::jni_native;
use std::sync::{Arc, LazyLock, Mutex};

use jni::JNIEnv;
use jni::objects::{GlobalRef, JObject, JString, JThrowable, JValue};
use jni::sys::{jboolean, jint, jlong, jobject, jstring};

use tst_core::transport::RecvTransport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};

use super::cancel::cancel_view_handle;
use super::end_reason::{build_close_snapshot, detail, wire_value};
use super::mux_sender::build_transport_stats;
use super::open::{open_recv, read_url};
use super::{domain_or_closed, throw_state};
use crate::error::{throw_binding, throw_handle_state};
use crate::generated::stats_mpegts::build_muxer_stats;
use crate::generated::stats_shells::build_socket_stats;
use crate::handle::OwnedRegistry;
use crate::mpegts::{
    build_demux_config_from_args, convert_event, shell_muxer_stats, throw_demux_error,
};

/// Native backing for `org.tstrans.shells.DemuxReceiver`. Only `sink_error` is
/// shared: the `FnMut`+`Send` byte-sink closure needs owned `Send` capture, and
/// it stashes the first byte-sink exception here as a `GlobalRef` for
/// fail-loud re-raise from `nNext`.
struct JniDemuxReceiver {
    inner: RustDemuxReceiver<Box<dyn RecvTransport>>,
    sink_error: Arc<Mutex<Option<GlobalRef>>>,
}

static REGISTRY: LazyLock<OwnedRegistry<JniDemuxReceiver, Snapshot<()>>> =
    LazyLock::new(OwnedRegistry::new);

/// Throw a `recv_event` failure: demux-sourced ones keep `DemuxException`;
/// transport-sourced ones are thrown in the shell's `domain`. The peer-EOS rule:
/// a receiver shell's `TransportError::Closed` is `EndOfStream`, which an
/// iterator reports as the clean end of iteration, never as an exception, so
/// nothing is thrown and `nNext` returns null.
fn throw_demux_recv_error(env: &mut JNIEnv, domain: Domain, e: DemuxReceiverError) {
    match e.source {
        DemuxReceiverErrorSource::Demux(d) => throw_demux_error(env, &d),
        _ => {
            let be = BindingError::from(e);
            if be.kind != BindingErrorKind::EndOfStream {
                throw_binding(env, domain, &be);
            }
        }
    }
}

/// Shared body of `nOpen` / `nOpenWithConfig`: open the transport named by the
/// URL's scheme and wrap it in a `DemuxReceiver` (with or without explicit
/// demux options). Returns the key, or `0` with an exception pending.
fn open_with(
    env: &mut JNIEnv,
    url: &JString,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> jlong {
    let Some(url) = read_url(env, url) else {
        return 0;
    };
    let Some(opened) = open_recv(env, &url) else {
        return 0;
    };
    register(opened, opts)
}

/// Wrap an opened receive transport in a `DemuxReceiver` (with or without
/// explicit demux options) and register it. Shared by `open_with` and by
/// `srt.Socket.intoDemuxReceiver`.
pub(crate) fn register(
    opened: Opened<Box<dyn RecvTransport>>,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> jlong {
    let Opened {
        transport,
        cancel,
        domain,
        ..
    } = opened;
    // Taken before the transport moves into the shell.
    let end_reason = transport.end_reason_handle();
    let inner = match opts {
        None => RustDemuxReceiver::new(transport),
        Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
    };
    let jdr = JniDemuxReceiver {
        inner,
        sink_error: Arc::new(Mutex::new(None)),
    };
    let mut owned = Owned::new(
        jdr,
        cancel,
        Snapshot {
            domain,
            handles: (),
        },
    );
    if let Some(h) = end_reason {
        owned = owned.with_end_reason(h);
    }
    REGISTRY.insert(owned) as jlong
}

jni_native! {
    /// `DemuxReceiver.nOpen(url)` — default demux options. For `srt://` the URL
    /// must carry `?mode=listener`; the open accepts one peer (not cancellable).
    fn Java_org_tstrans_shells_DemuxReceiver_nOpen<'local>(
        env,
        _class,
        url: JString<'local>,
    ) -> jlong = 0 => {
        open_with(env, &url, None)
    }
}

jni_native! {
    /// `DemuxReceiver.nOpenWithConfig(url, ...)` — same with an explicit
    /// `DemuxerConfig` (the 10 marshalled primitives; see
    /// `crate::mpegts::build_demux_config_from_args`).
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_shells_DemuxReceiver_nOpenWithConfig<'local>(
        env,
        _class,
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
    ) -> jlong = 0 => {
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
        open_with(env, &url, Some(opts))
    }
}

jni_native! {
    /// `nNext(handle)` — block until the next `DemuxEvent`, returning it as a Java
    /// object; Java `null` on a clean end (the peer closed, or the demuxer
    /// drained), which ends the Java iterator. Throws the transport's exception /
    /// `DemuxException` on a recv-side error, or re-raises a captured byte-sink
    /// exception fail-loud.
    fn Java_org_tstrans_shells_DemuxReceiver_nNext<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "DemuxReceiver") else {
            return JObject::null().into_raw();
        };
        // recv_event runs INSIDE the slot. The byte sinks fire during this call
        // and may stash an exception; the `sink_error` Arc is cloned out so it
        // can be drained AFTER the slot is released.
        let (res, sink_error) = match REGISTRY.with_mut(handle as u64, |jdr| {
            (jdr.inner.recv_event(), jdr.sink_error.clone())
        }) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "DemuxReceiver", &state);
                return JObject::null().into_raw();
            }
        };

        // Fail-loud: surface any byte-sink exception captured during this
        // `recv_event` BEFORE inspecting `res`, re-raising the first-wins
        // Throwable and stopping iteration. `take()` so a resumed iteration
        // after a caught error is not permanently poisoned.
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
                // All current `DemuxEvent` variants map to a record; retained as a
                // forward-compat guard (see mpegts::nNextEvent).
                Ok(None) => JObject::null().into_raw(),
                Err(()) => {
                    // Event-conversion JNI failure. `throw_demux` guards against
                    // clobbering a pending exception; it throws RuntimeException:
                    // demux has no INTERNAL kind.
                    crate::error::throw_demux(
                        env,
                        BindingErrorKind::Internal,
                        "event conversion failed",
                    );
                    JObject::null().into_raw()
                }
            },
            Err(e) => {
                throw_demux_recv_error(env, domain, e);
                JObject::null().into_raw()
            }
        }
    }
}

jni_native! {
    /// `nAddByteSink(handle, consumer)` — register a fan-out `Consumer<byte[]>` that
    /// fires once per 188-byte TS packet before demux. See the module doc
    /// for the monitor discipline: the closure attaches to the JVM and upcalls with
    /// NO Rust lock and NO Java monitor held.
    fn Java_org_tstrans_shells_DemuxReceiver_nAddByteSink<'local>(
        env,
        _class,
        handle: jlong,
        consumer: JObject<'local>,
    ) = () => {
        // Capture owned `Send` state for the closure: a cached `JavaVM` (to attach
        // the receiver thread per packet), a `GlobalRef` to the consumer (local refs
        // can't cross the call). These touch `env`, so build them BEFORE leasing.
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

        // Register the sink under the registry lease. The shared sink-error slot is
        // cloned from the leased receiver.
        let registered = REGISTRY.with_mut(handle as u64, |jdr| {
            let slot = jdr.sink_error.clone();
            jdr.inner.add_byte_sink(Box::new(move |pkt: &[u8]| {
                // Runs on the receiver's own thread INSIDE recv_event. NO Java monitor
                // and NO Rust lock is held across this upcall (DemuxReceiver Java
                // methods are NOT synchronized and there is no inner mutex) — the JVM
                // analogue of tst-py's allow-threads-before-lock fix, trivial here
                // because there is no GIL. Touches ONLY `consumer` + `slot`, never the
                // receiver's `inner`.
                let Ok(mut env) = vm.attach_current_thread() else {
                    return;
                };
                let arr = match env.byte_array_from_slice(pkt) {
                    Ok(a) => a,
                    // OOM-only: clear the pending JavaException before bailing so the
                    // next packet's fanout doesn't run JNI calls under a stale exception.
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
        if let Err(state) = registered {
            throw_handle_state(env, "DemuxReceiver", &state);
        }
    }
}

jni_native! {
    /// `nLastSeenMicros(handle, pid)` — Unix-epoch microsecond timestamp the
    /// stream identified by `pid` last carried a demuxed item through this
    /// receiver (last emitted event); `-1` if `pid` was never seen — including
    /// an unrecognized PID (no range check beyond the native `u16` truncating
    /// cast, same as `pmtPid`/`pcrPid` elsewhere in this binding) — or a
    /// timestamp predating the Unix epoch. Boxed to `Long` (`null` for `-1`) at
    /// the Java layer. Same registry-lock discipline as `nStats`.
    fn Java_org_tstrans_shells_DemuxReceiver_nLastSeenMicros(
        env,
        _class,
        handle: jlong,
        pid: jint,
    ) -> jlong = -1 => {
        // per-PID timestamp lives in the demuxer stats inside the shell; blocking caveat documented with the watchdog recipe
        let last_seen = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jdr| {
                jdr.inner
                    .stats()
                    .per_stream
                    .get(&(pid as u16))
                    .and_then(|s| s.last_seen)
            },
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "DemuxReceiver", &state);
                return -1;
            }
        };
        last_seen
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros() as i64)
            .unwrap_or(-1)
    }
}

jni_native! {
    /// `nCancelHandle(handle)` — a shareable cancel handle that wakes a thread
    /// parked in `nNext`. Lock-free: answers while `nNext` is parked.
    fn Java_org_tstrans_shells_DemuxReceiver_nCancelHandle(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        match REGISTRY.cancel_view(handle as u64) {
            Some(view) => cancel_view_handle(view),
            None => {
                crate::error::throw_closed(env, "DemuxReceiver");
                0
            }
        }
    }
}

jni_native! {
    /// `nSocketStats(handle)` — scheme-neutral 16-field wire stats. Null on a JNI
    /// builder error (non-fatal).
    fn Java_org_tstrans_shells_DemuxReceiver_nSocketStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jdr| jdr.inner.socket_stats().unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "DemuxReceiver", &state);
                return JObject::null();
            }
        };
        build_socket_stats(env, &stats)
            .unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `nStats(handle)` — `(SocketStats, MuxerStats)` projection. The socket stats
    /// carry only the pipeline-tracked byte/packet counters (a partial
    /// `SocketStats`); the demux side is reshaped as a `MuxerStats` projection so
    /// callers read the same `TransportStats` shape on `MuxSender` and
    /// `DemuxReceiver`. Null on a JNI builder error.
    fn Java_org_tstrans_shells_DemuxReceiver_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let combined = match REGISTRY.with_ref_blocking(
            handle as u64,
            BlockingReason::NoAnswerWithoutSlot,
            |jdr| jdr.inner.stats(),
        ) {
            Ok(v) => v,
            Err(state) => {
                throw_handle_state(env, "DemuxReceiver", &state);
                return JObject::null();
            }
        };

        let mut sock = tst_core::transport::SocketStats::default();
        sock.bytes_received = combined.bytes_received;
        sock.packets_received = combined.packets_received;

        let sock_obj = match build_socket_stats(env, &sock) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        let mux_obj = match build_muxer_stats(
            env,
            &shell_muxer_stats(
                combined.packets_received,
                combined.bytes_received,
                combined.program_maps_seen,
            ),
        ) {
            Ok(o) => o,
            Err(_) => return JObject::null(),
        };
        build_transport_stats(env, &sock_obj, &mux_obj).unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `nEndReason(handle)` — why the receive session ended, as a wire value
    /// (`-1` while it has not ended or the transport records none). Lock-free.
    fn Java_org_tstrans_shells_DemuxReceiver_nEndReason(
        _env,
        _class,
        handle: jlong,
    ) -> jint = -1 => {
        wire_value(REGISTRY.end_reason(handle as u64).as_ref())
    }
}

jni_native! {
    /// `nEndDetail(handle)` — free-text detail for `nEndReason`; null for a variant
    /// without one.
    fn Java_org_tstrans_shells_DemuxReceiver_nEndDetail(
        env,
        _class,
        handle: jlong,
    ) -> jstring = std::ptr::null_mut() => {
        match REGISTRY.end_reason(handle as u64).as_ref().and_then(detail) {
            Some(d) => env
                .new_string(d)
                .map_or(std::ptr::null_mut(), |s| s.into_raw()),
            None => std::ptr::null_mut(),
        }
    }
}

jni_native! {
    /// `nClose(handle)` — cancel first, then take the receiver and close it.
    /// Returns the end-reason snapshot (`EndReason$Snapshot`), null when the
    /// close lost the race (a double `close()` is safe).
    fn Java_org_tstrans_shells_DemuxReceiver_nClose<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        match REGISTRY.close_with_end_reason(handle as u64, |mut jdr| jdr.inner.close()) {
            Some(reason) => build_close_snapshot(env, reason).unwrap_or_else(|_| JObject::null()),
            None => JObject::null(),
        }
    }
}

jni_native! {
    /// `nIsAlive(handle)` — whether the receiver owns a live transport.
    /// Non-blocking: a parked `nNext` holds the slot, and reads as alive.
    fn Java_org_tstrans_shells_DemuxReceiver_nIsAlive(
        _env,
        _class,
        handle: jlong,
    ) -> jboolean = 0 => {
        match REGISTRY.try_with_ref(handle as u64, |jdr| u8::from(jdr.inner.is_alive())) {
            Ok(Some(v)) => v,
            // A parked next() holds the slot: the receiver is live.
            Ok(None) => 1,
            // Taken/absent: closed.
            Err(_) => 0,
        }
    }
}
