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
//! `recv_event`, attaches to the JVM, and upcalls a `Consumer<byte[]>`. No
//! Java monitor is held across the upcall, but the receiver's slot is:
//! `nNext` holds it for the whole `recv_event` call (`REGISTRY.with_mut`),
//! so a native that takes the slot (`next()`, `stats()`, `socketStats()`,
//! `lastSeenMicros()`, `addByteSink()`) called from the sink would relock it
//! on the same thread. [`IN_SINK`] marks every receiver whose sink is running
//! on this thread, and those natives throw `IllegalStateException` for a
//! marked receiver instead. `close()` from the sink is deferred
//! (`nDeferClose`): it only cancels; `nNext` then reports end of stream and
//! the Java side completes the close. `cancelHandle()` / `isAlive()` /
//! `endReason()` stay lock-free and `close()` cancels before it takes the
//! slot, so other threads can still stop a receiver whose sink is running.
//! A callback exception ends the receive: the sink stores it (as a
//! `Global`) in [`SinkState`], cancels the receiver so the in-flight
//! `recv_event` returns, and never upcalls again; `nNext` rethrows it once
//! the slot is released.

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{Global, JObject, JString, JThrowable, JValue};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong, jobject, jstring};

use tst_core::transport::RecvTransport;
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, HandleState, Opened, Owned};
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

/// Native backing for `org.tstrans.shells.DemuxReceiver`.
struct JniDemuxReceiver {
    inner: RustDemuxReceiver<Box<dyn RecvTransport>>,
}

/// Byte-sink state shared by the sink closures and `nNext`. It lives in the
/// snapshot, outside the slot, because the closures run while `nNext` holds
/// the slot.
#[derive(Default)]
struct SinkState {
    /// The exception that ended the receive, until `nNext` rethrows it.
    error: Mutex<Option<Global<JThrowable<'static>>>>,
    /// Latched by the first failing callback: no sink is called again and
    /// the receiver stays ended.
    failed: AtomicBool,
    /// Set by a `close()` from this receiver's own sink; the `nNext` it
    /// runs under, or the next one, ends the iteration so Java can finish
    /// the close. A plain close clears it.
    close_pending: AtomicBool,
}

thread_local! {
    /// The keys of every receiver whose byte sink is running on this
    /// thread, innermost last. A stack, not a single key: A's sink may pull
    /// B, and B's sink may then call into A, whose slot is still held.
    static IN_SINK: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Pops this sink's key on scope exit, unwind included.
struct InSinkGuard;

impl InSinkGuard {
    fn enter(key: u64) -> Self {
        IN_SINK.with(|s| s.borrow_mut().push(key));
        Self
    }
}

impl Drop for InSinkGuard {
    fn drop(&mut self) {
        IN_SINK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// `true` when called on this thread from inside a byte sink of the
/// receiver `handle`, however many receivers deep.
fn in_own_sink(handle: jlong) -> bool {
    IN_SINK.with(|s| s.borrow().contains(&(handle as u64)))
}

/// Throw `IllegalStateException` and return `true` when `method` is called
/// from the receiver's own sink, where taking the slot would deadlock.
fn refuse_reentry(env: &mut Env, handle: jlong, method: &str) -> bool {
    if !in_own_sink(handle) {
        return false;
    }
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalStateException")),
        JNIString::from(format!(
            "DemuxReceiver.{method}() called from its own byte sink; only close(), \
             cancelHandle(), isAlive() and endReason() are allowed there"
        )),
    );
    true
}

static REGISTRY: LazyLock<OwnedRegistry<JniDemuxReceiver, Snapshot<Arc<SinkState>>>> =
    LazyLock::new(OwnedRegistry::new);

/// Throw a `recv_event` failure: demux-sourced ones keep `DemuxException`;
/// transport-sourced ones are thrown in the shell's `domain`. The peer-EOS rule:
/// a receiver shell's `TransportError::Closed` is `EndOfStream`, which an
/// iterator reports as the clean end of iteration, never as an exception, so
/// nothing is thrown and `nNext` returns null.
fn throw_demux_recv_error(env: &mut Env, domain: Domain, e: DemuxReceiverError) {
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
    env: &mut Env,
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
    let jdr = JniDemuxReceiver { inner };
    let mut owned = Owned::new(
        jdr,
        cancel,
        Snapshot {
            domain,
            handles: Arc::new(SinkState::default()),
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
        if refuse_reentry(env, handle, "recvEvent") {
            return JObject::null().into_raw();
        }
        let Some(sinks) = REGISTRY.snapshot(handle as u64, |s| s.handles.clone()) else {
            crate::error::throw_closed(env, "DemuxReceiver");
            return JObject::null().into_raw();
        };
        // The previous pull delivered its event after its sink deferred a
        // close: end the iteration now, and Java finishes the close.
        if sinks.close_pending.load(Ordering::Acquire) {
            return JObject::null().into_raw();
        }
        // A sink failure, rethrown by an earlier call, ended the receive for
        // good: what the demuxer still queues came from packets after it.
        if sinks.failed.load(Ordering::Acquire) {
            throw_state(env, domain, "DemuxReceiver", &HandleState::Closed);
            return JObject::null().into_raw();
        }
        // recv_event runs INSIDE the slot; the byte sinks fire during it.
        let res = match REGISTRY.with_mut(handle as u64, |jdr| jdr.inner.recv_event()) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "DemuxReceiver", &state);
                return JObject::null().into_raw();
            }
        };

        // A sink exception wins over `res`: the sink already cancelled the
        // receiver, and an event this `recv_event` returned is discarded.
        let captured = sinks.error.lock().ok().and_then(|mut s| s.take());
        if let Some(global) = captured {
            if let Ok(local) = env.new_local_ref(&global) {
                let _ = env.throw(local);
            }
            return JObject::null().into_raw();
        }

        // A close deferred by a sink during this pull: deliver the event the
        // pull produced, if any (the next call ends the iteration); with
        // none, end it now, whatever the cancel made `recv_event` return.
        if sinks.close_pending.load(Ordering::Acquire) && !matches!(res, Ok(Some(_))) {
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
    /// for the discipline: the closure upcalls under the receiver's slot,
    /// with no Java monitor held, and a callback exception cancels the receiver.
    fn Java_org_tstrans_shells_DemuxReceiver_nAddByteSink<'local>(
        env,
        _class,
        handle: jlong,
        consumer: JObject<'local>,
    ) = () => {
        if refuse_reentry(env, handle, "addByteSink") {
            return;
        }
        // Capture owned `Send` state for the closure: a cached `JavaVM` (to attach
        // the receiver thread per packet), a `Global` to the consumer (local refs
        // can't cross the call). These touch `env`, so build them BEFORE leasing.
        let vm = match env.get_java_vm() {
            Ok(vm) => vm,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return;
            }
        };
        let consumer = match env.new_global_ref(&consumer) {
            Ok(g) => g,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return;
            }
        };

        // The shared sink state and the receiver's own cancel are read off
        // the entry without the slot.
        let (Some(sinks), Some(cancel)) = (
            REGISTRY.snapshot(handle as u64, |s| s.handles.clone()),
            REGISTRY.cancel_view(handle as u64),
        ) else {
            throw_handle_state(env, "DemuxReceiver", &HandleState::Closed);
            return;
        };
        let cancel = cancel.0;
        let registered = REGISTRY.with_mut(handle as u64, |jdr| {
            jdr.inner.add_byte_sink(Box::new(move |pkt: &[u8]| {
                // Runs on the receiver's own thread inside recv_event, under
                // the receiver's slot and with no Java monitor held. Touches
                // only `consumer`, `sinks` and `cancel`, never the receiver.
                if sinks.failed.load(Ordering::Acquire) {
                    return;
                }
                // Marks this receiver for the callback's duration, so a call
                // back into it is refused or deferred instead of relocking.
                let _in_sink = InSinkGuard::enter(handle as u64);
                // Scoped: the thread is detached again when the closure ends
                // unless it was already attached, as with the 0.21 guard. It
                // also pushes a local frame per call (each packet's byte[]
                // local is freed on return) and, under the default
                // PreReThrowPostCatch policy, stashes a pending exception
                // before the callback and re-throws it after; do not swap it
                // for a permanent attach.
                let _ = vm.attach_current_thread_for_scope(|env| -> jni::errors::Result<()> {
                    let arr = match env.byte_array_from_slice(pkt) {
                        Ok(a) => a,
                        // OOM-only: clear the pending JavaException before bailing so the
                        // next packet's fanout doesn't run JNI calls under a stale exception.
                        Err(_) => {
                            env.exception_clear();
                            return Ok(());
                        }
                    };
                    let _ = env.call_method(
                        &consumer,
                        jni_str!("accept"),
                        jni_sig!("(Ljava/lang/Object;)V"),
                        &[JValue::Object(&arr.into())],
                    );
                    // The first callback exception ends the receive: store it
                    // for `nNext`, then cancel so the in-flight recv_event
                    // returns instead of waiting for an event.
                    if env.exception_check()
                        && let Some(exc) = env.exception_occurred() {
                            env.exception_clear();
                            if !sinks.failed.swap(true, Ordering::AcqRel) {
                                if let Ok(mut s) = sinks.error.lock()
                                    && let Ok(g) = env.new_global_ref(&exc) {
                                        *s = Some(g);
                                    }
                                cancel.cancel();
                            }
                        }
                    Ok(())
                });
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
        if refuse_reentry(env, handle, "lastSeenMicros") {
            return -1;
        }
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
        if refuse_reentry(env, handle, "socketStats") {
            return JObject::null();
        }
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
        if refuse_reentry(env, handle, "stats") {
            return JObject::null();
        }
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
    /// `nDeferClose(handle)` — from inside this receiver's own byte sink,
    /// whose `nNext` holds the slot: cancel, mark the close pending and
    /// return `true`; the iteration then ends and Java completes the close.
    /// Elsewhere it does nothing and returns `false`. Never takes the slot.
    fn Java_org_tstrans_shells_DemuxReceiver_nDeferClose(
        _env,
        _class,
        handle: jlong,
    ) -> jboolean = false => {
        if !in_own_sink(handle) {
            return false;
        }
        if let Some(cancel) = REGISTRY.cancel_view(handle as u64) {
            cancel.0.cancel();
        }
        if let Some(sinks) = REGISTRY.snapshot(handle as u64, |s| s.handles.clone()) {
            sinks.close_pending.store(true, Ordering::Release);
        }
        true
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
        // Cleared before and after: while this close waits for the slot, the
        // in-flight pull's sink may defer a close of its own, and the
        // completed close wins over it.
        let sinks = REGISTRY.snapshot(handle as u64, |s| s.handles.clone());
        let clear = || {
            if let Some(s) = &sinks {
                s.close_pending.store(false, Ordering::Release);
            }
        };
        clear();
        let closed = REGISTRY.close_with_end_reason(handle as u64, |mut jdr| jdr.inner.close());
        clear();
        match closed {
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
    ) -> jboolean = false => {
        match REGISTRY.try_with_ref(handle as u64, |jdr| jdr.inner.is_alive()) {
            Ok(Some(v)) => v,
            // A parked next() holds the slot: the receiver is live.
            Ok(None) => true,
            // Taken/absent: closed.
            Err(_) => false,
        }
    }
}
