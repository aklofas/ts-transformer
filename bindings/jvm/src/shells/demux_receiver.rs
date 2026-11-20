//! JNI exports for `org.tstrans.shells.DemuxReceiver`: any transport in,
//! `DemuxEvent`s out.
//!
//! Wraps `tst_pipeline::DemuxReceiver<Box<dyn RecvTransport>>`, opened from a
//! URL by `nOpen` / `nOpenWithConfig`, and iterated for `DemuxEvent` objects.
//! The transport's error domain rides in the snapshot (readable after
//! `close()`), and its end-reason cell is attached to the `Owned`.
//!
//! Threading: single-threaded per the `Receiver` model: iteration on the Java
//! `DemuxReceiver` is not synchronized; only `close()` is (so concurrent
//! closes finish together). The sanctioned cross-thread stop is
//! `cancelHandle().cancel()` (or `close()`, which cancels first), which
//! wakes a parked `recv_event`.
//!
//! Slot discipline: `nNext` holds the receiver's slot for the whole
//! `recv_event` call (`REGISTRY.with_mut`). `cancelHandle()` / `isAlive()` /
//! `endReason()` stay lock-free and `close()` cancels before it takes the
//! slot, so other threads can still stop a receiver whose `nNext` is parked.

use crate::panic::jni_native;
use std::sync::{Arc, LazyLock};

use jni::Env;
use jni::objects::{JLongArray, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jobject, jstring};

use tst_core::transport::{RecvTransport, SourceHandle, TransportCancel};
use tst_pipeline::binding::view::{BlockingReason, Snapshot};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened, Owned};
use tst_pipeline::{
    DemuxReceiver as RustDemuxReceiver, DemuxReceiverError, DemuxReceiverErrorSource,
    TsBytesDemuxedHandle,
};

use tst_rtp::RtpStatsHandle;

use super::cancel::cancel_view_handle;
use super::end_reason::{build_close_snapshot, detail, wire_value};
use super::open::{open_recv, read_url};
use super::open_cancel::object_of;
use super::{domain_or_closed, throw_state};
use crate::error::{throw_binding, throw_handle_state};
use crate::generated::stats_rtp::build_rtp_stats;
use crate::generated::stats_shells::{build_demux_receiver_stats, build_socket_stats};
use crate::handle::OwnedRegistry;
use crate::jutil::socket_addr_array;
use crate::mpegts::{build_demux_config_from_args, convert_event, throw_demux_error};

/// Native backing for `org.tstrans.shells.DemuxReceiver`.
struct JniDemuxReceiver {
    inner: RustDemuxReceiver<Box<dyn RecvTransport>>,
}

/// Latches the receiver's own cancel when its publish mount was cancelled, so
/// the gate refuses with CLOSED before the shell returns queued events. A
/// mount cancel racing this call may still lose to it, like any concurrent
/// cancel.
fn observe_mount_cancel(handle: jlong) {
    let cancelled = REGISTRY
        .snapshot(handle as u64, |s| {
            s.handles
                .mount_cancel
                .as_ref()
                .is_some_and(|c| c.is_cancelled())
        })
        .unwrap_or(false);
    if cancelled && let Some(view) = REGISTRY.cancel_view(handle as u64) {
        view.0.cancel();
    }
}

/// The construction-time constants of a receiver, read without the slot.
struct RxSnap {
    /// Reader for the demuxer's fed-byte counter; outlives the shell.
    ts_bytes_demuxed: TsBytesDemuxedHandle,
    /// The application-cancel latch of the publish mount this receiver was
    /// taken from; `None` on every other receiver. The shared binding layer
    /// reads only a handle's own latch, so this layer carries the mount's
    /// cancel across: a mount cancel must beat events already queued, like
    /// the receiver's own cancel.
    mount_cancel: Option<Arc<dyn TransportCancel>>,
    /// The address the transport bound at open, for the transports that report one.
    local_addr: Option<std::net::SocketAddr>,
    /// Where the transport's datagrams come from, for the transports that record it.
    source: Option<SourceHandle>,
    /// The RTP counters of an `rtp://` or RTSP transport, taken before it was boxed.
    rtp_stats: Option<RtpStatsHandle>,
}

static REGISTRY: LazyLock<OwnedRegistry<JniDemuxReceiver, Snapshot<RxSnap>>> =
    LazyLock::new(OwnedRegistry::new);

/// A counter value as a `jlong`, saturating.
pub(crate) fn counter_to_jlong(v: u64) -> jlong {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Write `v` into `out[0]`; a null or empty array is left alone.
pub(crate) fn write_counter_out(env: &mut Env, out: &JLongArray, v: u64) {
    if out.is_null() || !out.len(env).is_ok_and(|n| n >= 1) {
        return;
    }
    let _ = out.set_region(env, 0, &[counter_to_jlong(v)]);
}

/// The RTP counters as a Java `RtpStats`; null with nothing pending when JNI
/// cannot build it (the getter then keeps its last value).
pub(crate) fn rtp_stats_object(env: &mut Env, h: &RtpStatsHandle) -> jobject {
    build_rtp_stats(env, &h.stats()).map_or(std::ptr::null_mut(), JObject::into_raw)
}

/// Write the RTP counters into `out[0..2]` (malformed, source-rejected); a
/// null or short array is left alone.
pub(crate) fn write_rtp_stats_out(env: &mut Env, out: &JLongArray, h: &RtpStatsHandle) {
    if out.is_null() || !out.len(env).is_ok_and(|n| n >= 2) {
        return;
    }
    let s = h.stats();
    let _ = out.set_region(
        env,
        0,
        &[
            counter_to_jlong(s.malformed_packets),
            counter_to_jlong(s.source_rejected),
        ],
    );
}

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
    cancel: &JObject,
) -> jlong {
    let Ok(oc) = object_of(env, cancel) else {
        return 0;
    };
    let Some(url) = read_url(env, url) else {
        return 0;
    };
    let Some((opened, rtp_stats)) = open_recv(env, &url, oc.as_ref()) else {
        return 0;
    };
    register_with(opened, opts, None, rtp_stats)
}

/// Wrap an opened receive transport in a `DemuxReceiver` (with or without
/// explicit demux options) and register it. Shared by `open_with` and by
/// `srt.Socket.intoDemuxReceiver`.
pub(crate) fn register(
    opened: Opened<Box<dyn RecvTransport>>,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> jlong {
    register_with(opened, opts, None, None)
}

/// [`register`] for an RTP transport whose counters `rtp_stats` reads.
pub(crate) fn register_rtp(
    opened: Opened<Box<dyn RecvTransport>>,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    rtp_stats: RtpStatsHandle,
) -> jlong {
    register_with(opened, opts, None, Some(rtp_stats))
}

/// [`register_rtp`] for the transport of a publish mount, whose cancel latch
/// the mount's `cancel()` sets.
pub(crate) fn register_publish_mount(
    opened: Opened<Box<dyn RecvTransport>>,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    rtp_stats: RtpStatsHandle,
) -> jlong {
    let mount_cancel = Arc::clone(&opened.cancel);
    register_with(opened, opts, Some(mount_cancel), Some(rtp_stats))
}

fn register_with(
    opened: Opened<Box<dyn RecvTransport>>,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
    mount_cancel: Option<Arc<dyn TransportCancel>>,
    rtp_stats: Option<RtpStatsHandle>,
) -> jlong {
    let Opened {
        transport,
        cancel,
        domain,
        local_addr,
        ..
    } = opened;
    // Taken before the transport moves into the shell.
    let end_reason = transport.end_reason_handle();
    let source = transport.source_handle();
    let inner = match opts {
        None => RustDemuxReceiver::new(transport),
        Some(opts) => RustDemuxReceiver::with_demux_options(transport, opts),
    };
    let ts_bytes_demuxed = inner.ts_bytes_demuxed_handle();
    let jdr = JniDemuxReceiver { inner };
    let mut owned = Owned::new(
        jdr,
        cancel,
        Snapshot {
            domain,
            handles: RxSnap {
                ts_bytes_demuxed,
                mount_cancel,
                local_addr,
                source,
                rtp_stats,
            },
        },
    );
    if let Some(h) = end_reason {
        owned = owned.with_end_reason(h);
    }
    REGISTRY.insert(owned) as jlong
}

jni_native! {
    /// `DemuxReceiver.nOpen(url, cancel)` — default demux options. For `srt://`
    /// the URL must carry `?mode=listener`; the open accepts one peer, and a
    /// non-null `OpenCancel` ends that accept (`CLOSED`).
    fn Java_org_tstrans_shells_DemuxReceiver_nOpen<'local>(
        env,
        _class,
        url: JString<'local>,
        cancel: JObject<'local>,
    ) -> jlong = 0 => {
        open_with(env, &url, None, &cancel)
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
        cancel: JObject<'local>,
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
        open_with(env, &url, Some(opts), &cancel)
    }
}

jni_native! {
    /// `nNext(handle)` — block until the next `DemuxEvent`, returning it as a Java
    /// object; Java `null` on a clean end (the peer closed, or the demuxer
    /// drained), which ends the Java iterator. Throws the transport's exception /
    /// `DemuxException` on a recv-side error.
    fn Java_org_tstrans_shells_DemuxReceiver_nNext<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "DemuxReceiver") else {
            return JObject::null().into_raw();
        };
        observe_mount_cancel(handle);
        // recv_event runs INSIDE the slot.
        let res = match REGISTRY.with_mut_unless_cancelled(handle as u64, |jdr| jdr.inner.recv_event()) {
            Ok(v) => v,
            Err(state) => {
                throw_state(env, domain, "DemuxReceiver", &state);
                return JObject::null().into_raw();
            }
        };

        match res {
            Ok(None) => JObject::null().into_raw(),
            Ok(Some(ev)) => match convert_event(env, &ev) {
                Ok(obj) => obj.into_raw(),
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
    /// `nGateCached(handle)` — the cancel gate of `nNext` for an event the
    /// Java iterator already holds from `hasNext()`: after
    /// `cancelHandle().cancel()` it throws what `nNext` throws, so a cached
    /// event is buffered output like any other.
    fn Java_org_tstrans_shells_DemuxReceiver_nGateCached(
        env,
        _class,
        handle: jlong,
    ) = () => {
        let Some(domain) = domain_or_closed(env, &REGISTRY, handle, "DemuxReceiver") else {
            return;
        };
        observe_mount_cancel(handle);
        if let Err(state) = REGISTRY.with_mut_unless_cancelled(handle as u64, |_| ()) {
            throw_state(env, domain, "DemuxReceiver", &state);
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
    /// `nStats(handle)` — the six receive counters as a `DemuxReceiverStats`.
    /// Null on a JNI builder error.
    fn Java_org_tstrans_shells_DemuxReceiver_nStats<'local>(
        env,
        _class,
        handle: jlong,
    ) -> JObject<'local> = JObject::null() => {
        // counters live inside the shell and have no value to report while a parked call holds it
        let stats = match REGISTRY.with_ref_blocking(
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
        build_demux_receiver_stats(env, &stats).unwrap_or_else(|_| JObject::null())
    }
}

jni_native! {
    /// `nRtpStats(handle)` — the RTP counters of an `rtp://` or RTSP receiver;
    /// null for every other transport and for an absent key (the Java side
    /// keeps the last value). Reads the snapshot, never the slot.
    fn Java_org_tstrans_shells_DemuxReceiver_nRtpStats(env, _class, handle: jlong) -> jobject = std::ptr::null_mut() => {
        match REGISTRY.snapshot(handle as u64, |s| s.handles.rtp_stats.clone()) {
            Some(Some(h)) => rtp_stats_object(env, &h),
            _ => std::ptr::null_mut(),
        }
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
    /// `nClose(handle, tsBytesOut, rtpOut)` — cancel first, then take the receiver and close it.
    /// Returns the end-reason snapshot (`EndReasonSnapshot`), null when the
    /// close lost the race (a double `close()` is safe). Writes the fed-byte
    /// count, read after the registry take, into `tsBytesOut[0]`, and an RTP
    /// receiver's counters, read then too, into `rtpOut[0..2]`.
    fn Java_org_tstrans_shells_DemuxReceiver_nClose<'local>(
        env,
        _class,
        handle: jlong,
        ts_bytes_out: JLongArray<'local>,
        rtp_out: JLongArray<'local>,
    ) -> JObject<'local> = JObject::null() => {
        let handles = REGISTRY.snapshot(handle as u64, |s| {
            (s.handles.ts_bytes_demuxed.clone(), s.handles.rtp_stats.clone())
        });
        let closed = REGISTRY.close_with_end_reason(handle as u64, |mut jdr| jdr.inner.close());
        // The take is done: the shell has stopped feeding the demuxer.
        if closed.is_some()
            && let Some((counter, rtp_stats)) = &handles
        {
            write_counter_out(env, &ts_bytes_out, counter.get());
            if let Some(h) = rtp_stats {
                write_rtp_stats_out(env, &rtp_out, h);
            }
        }
        match closed {
            Some(reason) => build_close_snapshot(env, reason).unwrap_or_else(|_| JObject::null()),
            None => JObject::null(),
        }
    }
}

jni_native! {
    /// `nTsBytesDemuxed(handle)` — aligned TS bytes the demuxer was fed; `-1`
    /// when the entry is gone. Reads the registry snapshot, never the slot.
    fn Java_org_tstrans_shells_DemuxReceiver_nTsBytesDemuxed(
        _env,
        _class,
        handle: jlong,
    ) -> jlong = -1 => {
        REGISTRY
            .snapshot(handle as u64, |s| s.handles.ts_bytes_demuxed.get())
            .map_or(-1, counter_to_jlong)
    }
}

jni_native! {
    /// `nLocalPort(handle)` — the port the transport bound at open, `-1` for a
    /// shell that has none; an absent key throws `IllegalStateException`. Reads
    /// the registry snapshot, never the slot.
    fn Java_org_tstrans_shells_DemuxReceiver_nLocalPort(env, _class, handle: jlong) -> jint = -1 => {
        match REGISTRY.snapshot(handle as u64, |s| s.handles.local_addr) {
            Some(addr) => addr.map_or(-1, |a| jint::from(a.port())),
            None => {
                crate::error::throw_closed(env, "DemuxReceiver");
                -1
            }
        }
    }
}

jni_native! {
    /// `nLocalAddr(handle)` — the address the transport bound at open, as
    /// `NativeAddr` bytes; null for a shell that has none. Reads the snapshot.
    fn Java_org_tstrans_shells_DemuxReceiver_nLocalAddr(env, _class, handle: jlong) -> jbyteArray = std::ptr::null_mut() => {
        match REGISTRY.snapshot(handle as u64, |s| s.handles.local_addr) {
            Some(Some(addr)) => socket_addr_array(env, addr),
            Some(None) => std::ptr::null_mut(),
            None => {
                crate::error::throw_closed(env, "DemuxReceiver");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `nPeerAddr(handle)` — the sender of the datagram or block delivered last,
    /// as `NativeAddr` bytes; null before the first one and for a transport that
    /// keeps no source record. Reads the snapshot's shared record, never the slot.
    fn Java_org_tstrans_shells_DemuxReceiver_nPeerAddr(env, _class, handle: jlong) -> jbyteArray = std::ptr::null_mut() => {
        match REGISTRY.snapshot(handle as u64, |s| s.handles.source.as_ref().and_then(SourceHandle::peer_addr)) {
            Some(Some(addr)) => socket_addr_array(env, addr),
            Some(None) => std::ptr::null_mut(),
            None => {
                crate::error::throw_closed(env, "DemuxReceiver");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `nSourceRejected(handle)` — datagrams or blocks the source pin turned
    /// away; 0 for a transport that keeps no source record. Reads the snapshot.
    fn Java_org_tstrans_shells_DemuxReceiver_nSourceRejected(env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY.snapshot(handle as u64, |s| s.handles.source.as_ref().map_or(0, SourceHandle::rejected)) {
            Some(n) => counter_to_jlong(n),
            None => {
                crate::error::throw_closed(env, "DemuxReceiver");
                0
            }
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
