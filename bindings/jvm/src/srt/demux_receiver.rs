//! JNI surface for `org.tstrans.srt.DemuxReceiver` — the single-call convenience
//! wrapper that owns a `Demuxer` + `SrtTransport`.
//!
//! Wraps `tst_pipeline::DemuxReceiver<tst_srt::SrtTransport>`: bind a libsrt
//! listener-mode receiver on a URL, accept one peer, demux the resulting
//! MPEG-TS stream, and iterate over `DemuxEvent` instances. Ports tst-py's
//! `bindings/python/src/srt/demux_receiver.rs`, but with the JVM threading model
//! instead of the GIL.
//!
//! Threading (spec §3.4): single-threaded per the `Receiver` model — `inner` is
//! accessed `&mut` per call with no mutex (the Java `DemuxReceiver` is not
//! thread-safe and is NOT `synchronized`). The sanctioned cross-thread stop is
//! `cancelHandle().cancel()`, which wakes a parked `recv_event`.
//!
//! `add_byte_sink` discipline (spec §6, the load-bearing piece): the registered
//! `Box<dyn FnMut(&[u8]) + Send>` runs on the receiver's own thread inside
//! `recv_event`, attaches to the JVM, and upcalls a `Consumer<byte[]>`. It holds
//! NO Java monitor and NO Rust lock across the upcall — the JVM analogue of
//! tst-py's allow-threads-before-lock fix, trivial here because there is no GIL
//! and `DemuxReceiver.next()`/`addByteSink()` are not `synchronized`. A callback
//! exception is captured first-wins into `sink_error` (as a `GlobalRef`) and
//! re-thrown from the next `nNext` after the `&mut inner` borrow ends.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};

use jni::JNIEnv;
use jni::objects::{GlobalRef, JClass, JObject, JString, JThrowable, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};

use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
};
use tst_srt::{Listener, ListenerConfig, Socket, SrtTransport, SrtUrl, url::Mode};

use crate::handle::HandleRegistry;
use crate::jutil::{build_socket_stats, join_host_port};
use crate::mpegts::{
    build_demux_config_from_args, build_muxer_stats, convert_event, throw_demux_error,
};

use super::JniCancel;
use super::errors::{accept_error, bind_error, throw_srt, transport_error, url_error};
use super::mux_sender::build_transport_stats;

/// Native backing for `org.tstrans.srt.DemuxReceiver`. Single-threaded per the
/// `Receiver` model (spec §3.4) — `inner` is accessed under the registry's
/// resource lock per call. Only `sink_error` is shared: the `FnMut`+`Send`
/// byte-sink closure needs owned `Send` capture, and it stashes the first
/// byte-sink exception here as a `GlobalRef` for fail-loud re-raise from `nNext`.
struct JniDemuxReceiver {
    inner: RustDemuxReceiver<SrtTransport>,
    sink_error: Arc<Mutex<Option<GlobalRef>>>,
}

/// Per-type leased-handle registry for `org.tstrans.srt.DemuxReceiver`. No cancel
/// hook (srt model — the public cancel handle + connection-close end iteration).
static REGISTRY: LazyLock<HandleRegistry<JniDemuxReceiver>> = LazyLock::new(HandleRegistry::new);

/// Map a `DemuxReceiverError` raised by `recv_event` onto a thrown Java
/// exception. Transport-side errors map to `SrtException` (via the shared
/// `transport_error` helper); demux-side errors map to `DemuxException` (via the
/// shared `throw_demux_error`). Mirrors tst-py's `demux_recv_error_to_pyerr`.
pub(crate) fn throw_demux_recv_error(env: &mut JNIEnv, e: &DemuxReceiverError) {
    match &e.source {
        DemuxReceiverErrorSource::Transport(t) => transport_error(env, t),
        DemuxReceiverErrorSource::Demux(d) => throw_demux_error(env, d),
        // `DemuxReceiverErrorSource` is non-exhaustive; route any future variant
        // through `SrtException(IO)` with the Display message preserved.
        _ => throw_srt(env, "IO", &e.to_string()),
    }
}

/// Shared construction body for `nFromUrl` / `nFromUrlWithConfig`: parse the URL,
/// reject non-listener mode, bind + one-shot accept, wrap the transport in a
/// `DemuxReceiver` (with or without explicit demux options). Returns the boxed
/// handle as `jlong`, or `0` with a pending exception on any failure.
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

    let parsed = match SrtUrl::parse(&url_str) {
        Ok(p) => p,
        Err(e) => {
            url_error(env, &e);
            return 0;
        }
    };

    if parsed.mode != Mode::Listener {
        let msg = format!(
            "DemuxReceiver.fromUrl requires mode=listener; got mode={:?}",
            parsed.mode
        );
        throw_srt(env, "CONFIG_INVALID", &msg);
        return 0;
    }

    let mut cfg = ListenerConfig::default();
    parsed.overlay.apply_to_listener(&mut cfg);

    let addr = if parsed.host.is_empty() {
        format!("0.0.0.0:{}", parsed.port)
    } else {
        join_host_port(&parsed.host, parsed.port)
    };

    let mut listener = match Listener::bind_with(&cfg, addr.as_str()) {
        Ok(l) => l,
        Err(e) => {
            bind_error(env, &e);
            return 0;
        }
    };

    let (socket, _peer) = match listener.accept() {
        Ok(pair) => pair,
        Err(e) => {
            accept_error(env, &e);
            return 0;
        }
    };

    register(make_receiver(socket, opts))
}

/// Build a `JniDemuxReceiver` from an already-connected `Socket` (with or without
/// explicit demux options). `DemuxReceiver::new` / `with_demux_options` are
/// infallible post-consume.
fn make_receiver(
    socket: Socket,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> JniDemuxReceiver {
    let transport = SrtTransport::new(socket);
    let inner = match opts {
        None => RustDemuxReceiver::new(transport),
        Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
    };
    JniDemuxReceiver {
        inner,
        sink_error: Arc::new(Mutex::new(None)),
    }
}

/// Register the receiver, capturing its cancel target BEFORE the box goes into
/// the registry so `nCancelHandle` never needs the resource lock a parked `nNext`
/// holds. A fresh `SrtTransport` always has a cancel handle.
///
/// Cancel-on-close: `nClose` fires `target` before taking the resource lock, so
/// a `next()` parked on another thread ends promptly (with `SrtException(BROKEN)`
/// — the plain cancel closes the socket under the parked recv) instead of
/// holding `close()` hostage. The contract tst-py's `DemuxReceiver.close()` and
/// the C ABI's `tst_demux_receiver_close` already have; no end-reason cell on
/// the plain shell.
fn register(jdr: JniDemuxReceiver) -> jlong {
    let target = jdr
        .inner
        .cancel_handle()
        .expect("a fresh SrtTransport always returns Some(cancel_handle)");
    REGISTRY.insert_cancel_on_close(jdr, target, None) as jlong
}

/// `DemuxReceiver.nFromUrl(url)` — bind a listener-mode SRT receiver, accept one
/// peer, and return a `Box<JniDemuxReceiver>` handle (default demux options).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nFromUrl<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| build_from_url(env, &url, None))
}

/// `DemuxReceiver.nFromUrlWithConfig(url, ...)` — same as `nFromUrl` but with an
/// explicit `DemuxerConfig` (the 9 marshalled primitives; see
/// `crate::mpegts::build_demux_config_from_args`).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nFromUrlWithConfig<'local>(
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

/// `nNext(handle)` — block until the next `DemuxEvent`, returning it as a Java
/// object; Java `null` on clean EOF (transport closed, demuxer drained → the
/// Java iterator's `hasNext` returns false). Throws `SrtException` /
/// `DemuxException` on a recv-side error, or re-raises a captured byte-sink
/// exception fail-loud.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nNext<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // recv_event runs INSIDE the registry lease (under the resource lock). The
        // byte sinks fire during this call and may stash an exception; we clone the
        // `sink_error` Arc out so we can drain it AFTER the lease releases.
        let Some((res, sink_error)) = REGISTRY.with_poisoning(handle as u64, |jdr| {
            (jdr.inner.recv_event(), jdr.sink_error.clone())
        }) else {
            crate::error::throw_closed(env, "DemuxReceiver");
            return JObject::null().into_raw();
        };

        // Fail-loud: surface any byte-sink exception captured during this
        // `recv_event` BEFORE inspecting `res`, re-raising the first-wins Throwable
        // and stopping iteration. `take()` so a resumed iteration after a caught
        // error isn't permanently poisoned.
        let captured = sink_error.lock().ok().and_then(|mut s| s.take());
        if let Some(global) = captured {
            // Derive a local ref that outlives the GlobalRef, then throw it.
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
                    // Event-conversion JNI failure (mirrors mpegts::nNextEvent's
                    // Err(()) arm). `throw_demux` guards against clobbering a
                    // pending exception; the INTERNAL literal stays ratchet-visible.
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

/// `nAddByteSink(handle, consumer)` — register a fan-out `Consumer<byte[]>` that
/// fires once per 188-byte TS packet before demux. See the module doc / spec §6
/// for the monitor discipline: the closure attaches to the JVM and upcalls with
/// NO Rust lock and NO Java monitor held.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nAddByteSink<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    consumer: JObject<'local>,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
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
        let registered = REGISTRY.with_poisoning(handle as u64, |jdr| {
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
        if registered.is_none() {
            crate::error::throw_closed(env, "DemuxReceiver");
        }
    })
}

/// `nCancelHandle(handle)` — return a shareable cancel handle that wakes a thread
/// parked in `nNext`. Lock-free: the target was captured at registration, so this
/// returns promptly even while another thread is parked in `nNext` on the same
/// handle. Throws `IllegalStateException` on a closed handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY.cancel_target(handle as u64) {
            Some(inner) => JniCancel {
                inner,
                flag: AtomicBool::new(false),
            }
            .into_handle(),
            None => {
                crate::error::throw_closed(env, "DemuxReceiver");
                0
            }
        }
    })
}

/// `nSocketStats(handle)` — scheme-neutral 16-field wire stats. Returns null on a
/// JNI builder error (non-fatal; mirrors the stats-builder convention).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nSocketStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(stats) = REGISTRY.with(handle as u64, |jdr| {
            jdr.inner.socket_stats().unwrap_or_default()
        }) else {
            crate::error::throw_closed(env, "DemuxReceiver");
            return JObject::null();
        };
        match build_socket_stats(env, "org/tstrans/srt/SocketStats", &stats) {
            Ok(obj) => obj,
            Err(_) => JObject::null(),
        }
    })
}

/// `nStats(handle)` — `(SocketStats, MuxerStats)` projection mirroring tst-py's
/// `DemuxReceiver.stats`. The socket stats carry only the pipeline-tracked
/// byte/packet counters (a partial `SocketStats`); the demux side is reshaped as
/// a `MuxerStats` projection so callers read the same `TransportStats` shape on
/// both `MuxSender` + `DemuxReceiver`. Returns null on a JNI builder error.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nStats<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> JObject<'local> {
    crate::panic::jni_catch(&mut env, JObject::null(), |env| {
        let Some(combined) = REGISTRY.with(handle as u64, |jdr| jdr.inner.stats()) else {
            crate::error::throw_closed(env, "DemuxReceiver");
            return JObject::null();
        };

        // SocketStats from the wire counters tracked at the pipeline layer (full
        // SocketStats via the transport accessor isn't surfaced through the shell).
        let mut sock = tst_core::transport::SocketStats::default();
        sock.bytes_received = combined.bytes_received;
        sock.packets_received = combined.packets_received;

        let sock_obj = match build_socket_stats(env, "org/tstrans/srt/SocketStats", &sock) {
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
        match build_transport_stats(env, &sock_obj, &mux_obj) {
            Ok(o) => o,
            Err(_) => JObject::null(),
        }
    })
}

/// `nLastSeenMicros(handle, pid)` — Unix-epoch microsecond timestamp the
/// stream identified by `pid` last carried a demuxed item through this
/// receiver (last emitted event); `-1` if `pid` was never seen — including
/// an unrecognized PID (no range check beyond the native `u16` truncating
/// cast, same as `pmtPid`/`pcrPid` elsewhere in this binding) — or a
/// timestamp predating the Unix epoch. Boxed to `Long` (`null` for `-1`) at
/// the Java layer. Same registry-lock discipline as `nStats`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nLastSeenMicros(
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

/// `nClose(handle)` — close the underlying transport and drop the box. No-op on a
/// zero handle so a double `close()` is safe.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent. No cancel hook (srt model — public cancel handle wakes
        // a parked recv); a close racing a parked recv blocks on the resource lock
        // until cancelled, never UAFs.
        if let Some(mut jdr) = REGISTRY.close(handle as u64) {
            jdr.inner.close();
        }
    })
}

/// `nIsAlive(handle)` — whether the receiver owns a live transport. Uses the
/// non-blocking `try_with` so a concurrent parked `nNext` reports "alive" rather
/// than blocking on the resource lock (mirrors rtp `DemuxReceiver::nIsAlive`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_DemuxReceiver_nIsAlive(
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

// ---------------------------------------------------------------------------
// Socket — nIntoDemuxReceiver (CONSUMES the Box<Socket>)
// ---------------------------------------------------------------------------

/// `Socket.nIntoDemuxReceiver(handle)` — consume a `Box<Socket>` and produce a
/// `Box<JniDemuxReceiver>` (default demux options). `DemuxReceiver::new` is
/// infallible post-consume, so this cannot throw after consuming.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nIntoDemuxReceiver(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let Some(socket) = super::lowlevel::REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        register(make_receiver(socket, None))
    })
}

/// `Socket.nIntoDemuxReceiverWithConfig(handle, ...)` — same as
/// `nIntoDemuxReceiver` but with an explicit `DemuxerConfig`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nIntoDemuxReceiverWithConfig(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
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
        let Some(socket) = super::lowlevel::REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
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
        register(make_receiver(socket, Some(opts)))
    })
}
