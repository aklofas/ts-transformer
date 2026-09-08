//! JNI exports for `org.tstrans.srt.Builder`, `org.tstrans.srt.Socket`, and
//! `org.tstrans.srt.Listener` — the low-level SRT primitives.
//!
//! Handle lifecycle:
//! - `nConnect` / `nListen` register via `REGISTRY.insert[_with_cancel]`.
//! - Non-consuming methods lease via `REGISTRY.with`.
//! - `nIntoSender` / `nIntoReceiver` CONSUME the `Socket` via `REGISTRY.close`
//!   and return a new `Sender` / `Receiver` handle. The Java caller zeros its own
//!   socket handle on success — the `handle = 0` assignment is the next statement
//!   after the native call.
//! - `nClose` takes + tears down via `REGISTRY.close`.
//!
//! Error mapping keeps each KIND literal on the same line as `throw_srt` so the
//! T4 grep ratchet can verify coverage.

#![allow(clippy::too_many_arguments)]

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::{jlong, jobject};
use tst_core::transport::TransportCancel;
use tst_pipeline::{Receiver as PlReceiver, ReceiverConfig, Sender as PlSender, SenderConfig};
use tst_srt::{
    Listener as SrtListener, ListenerConfig, Socket as SrtSocket, SocketConfig, SrtTransport,
    SrtUrl,
    options::{Congestion, MaxBandwidth, Passphrase, StreamId},
    url::Mode,
};

use super::JniCancel;
use super::errors::{accept_error, bind_error, connect_error, io_error, throw_srt, url_error};
use crate::handle::HandleRegistry;
use crate::jutil::{checked_u16, join_host_port};

/// Per-type leased-handle registry for `org.tstrans.srt.Socket` (an `SrtSocket`).
/// `nConnect`/`nAccept` insert; per-call methods lease; `nClose` and the
/// `nInto*` consumers `close` (taking the resource for teardown / hand-off).
pub(crate) static REGISTRY_SOCKET: LazyLock<HandleRegistry<SrtSocket>> =
    LazyLock::new(HandleRegistry::new);

/// Per-type registry for `org.tstrans.srt.Listener` (a `JniListener`). Registered
/// with a cancel hook that wakes a parked `accept`.
static REGISTRY_LISTENER: LazyLock<HandleRegistry<SrtListener>> =
    LazyLock::new(HandleRegistry::new);

// ---------------------------------------------------------------------------
// LowLevelSrtCancel adapter
// ---------------------------------------------------------------------------
//
// `Listener::cancel_handle()` returns a concrete `tst_core::SrtCancelHandle`,
// not an `Arc<dyn TransportCancel>`. We need a thin adapter to fit it into
// `JniCancel::inner: Arc<dyn TransportCancel + Send + Sync>`. This mirrors
// tst-py's `LowLevelSrtCancel` in `bindings/python/src/srt/transport.rs`.

struct LowLevelSrtCancel(tst_core::SrtCancelHandle);

impl TransportCancel for LowLevelSrtCancel {
    fn cancel(&self) {
        self.0.cancel();
    }
}

// ---------------------------------------------------------------------------
// Listener registration
// ---------------------------------------------------------------------------
//
// MEMORY-SAFETY / LIFETIME CONTRACT. The leased `HandleRegistry` is the
// process-global synchronisation point. `nAccept` leases the entry and runs
// `accept()` INSIDE the entry's resource lock, so every `Listener` field read
// happens in the critical section. `nClose` routes through `REGISTRY.close`,
// which fires the entry's cancel hook FIRST (closing the SRTSOCKET, waking a
// parked accept WITHOUT touching the `Listener` allocation) and only THEN takes
// the resource under the same lock — blocking until the woken accept released
// it. The cancel hook below is the outside-the-mutex wake the round-1 bespoke
// `Arc<Mutex<Option<…>>>` shim provided; the registry's generic primitive
// replaces it. `accept()` is single-owner (single-iterator) per the Java
// contract.

/// Register a `Listener`, wiring the cancel hook from its independent
/// `SrtCancelHandle` adapter (held by the registry entry, fired by `close`). The
/// same adapter is the entry's lock-free cancel target, so `nCancelHandle`
/// returns while an `accept` is parked on the resource lock.
fn register_listener(listener: SrtListener) -> u64 {
    let cancel: Arc<dyn TransportCancel + Send + Sync> =
        Arc::new(LowLevelSrtCancel(listener.cancel_handle()));
    let hook = Arc::clone(&cancel);
    REGISTRY_LISTENER.insert_full(
        listener,
        Some(Box::new(move || hook.cancel())),
        Some(cancel),
    )
}

// ---------------------------------------------------------------------------
// Helper: build a HostPort JObject from a std::net::SocketAddr
// ---------------------------------------------------------------------------

fn build_host_port<'local>(
    env: &mut JNIEnv<'local>,
    addr: std::net::SocketAddr,
) -> jni::errors::Result<JObject<'local>> {
    // Ensure capacity for: HostPort ctor arg (String) + local refs.
    env.ensure_local_capacity(8)?;
    let host_str = addr.ip().to_string();
    let host = env.new_string(&host_str)?;
    env.new_object(
        "org/tstrans/srt/HostPort",
        "(Ljava/lang/String;I)V",
        &[JValue::Object(&host), JValue::Int(addr.port() as i32)],
    )
}

// ---------------------------------------------------------------------------
// Helper: unbox nullable Integer / Long JObject args
// ---------------------------------------------------------------------------

/// Unbox a nullable `java.lang.Integer` JNI argument. Returns `None` for null.
fn unbox_nullable_int(env: &mut JNIEnv, obj: &JObject) -> jni::errors::Result<Option<i32>> {
    if obj.is_null() {
        return Ok(None);
    }
    let v = env.call_method(obj, "intValue", "()I", &[])?.i()?;
    Ok(Some(v))
}

/// Unbox a nullable `java.lang.Long` JNI argument. Returns `None` for null.
fn unbox_nullable_long(env: &mut JNIEnv, obj: &JObject) -> jni::errors::Result<Option<i64>> {
    if obj.is_null() {
        return Ok(None);
    }
    let v = env.call_method(obj, "longValue", "()J", &[])?.j()?;
    Ok(Some(v))
}

// ---------------------------------------------------------------------------
// Helper: build SocketConfig from knob args
// ---------------------------------------------------------------------------
//
// Mode ordinals (the Java Builder.Mode enum, passed as mode.ordinal()):
//   URL_CHOICE=0, CALLER=1, LISTENER=2, RENDEZVOUS=3.
// nConnect rejects LISTENER(2) and RENDEZVOUS(3); nListen rejects CALLER(1)
// and RENDEZVOUS(3). URL_CHOICE(0) defers to the URL's ?mode= parameter.

/// Apply the nullable knob args into a `SocketConfig`. Returns `Err` if any
/// validated knob (passphrase, stream_id, congestion, mss, payload_size) fails.
/// Does NOT apply the URL overlay — the caller does that after this returns.
#[allow(clippy::cast_sign_loss)]
fn build_socket_config(
    env: &mut JNIEnv,
    latency_ms: &JObject,
    passphrase: &JString,
    stream_id: &JString,
    congestion: &JString,
    connect_timeout_ms: &JObject,
    recv_timeout_ms: &JObject,
    send_timeout_ms: &JObject,
    peer_latency_ms: &JObject,
    recv_latency_ms: &JObject,
    max_bandwidth_bps: &JObject,
    mss: &JObject,
    payload_size: &JObject,
) -> jni::errors::Result<SocketConfig> {
    let mut cfg = SocketConfig::default();

    // latency_ms — applied to both SRTO_LATENCY on socket configs
    if let Some(ms) = unbox_nullable_int(env, latency_ms)? {
        cfg.latency = Some(Duration::from_millis(ms as u64));
    }

    // passphrase
    if !passphrase.is_null() {
        let s: String = env.get_string(passphrase).map(Into::into)?;
        match Passphrase::new(s) {
            Ok(pp) => cfg.passphrase = Some(pp),
            Err(e) => {
                throw_srt(env, "CONFIG_INVALID", &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    // stream_id (caller-side only; not in ListenerConfig)
    if !stream_id.is_null() {
        let s: String = env.get_string(stream_id).map(Into::into)?;
        match StreamId::new(s) {
            Ok(id) => cfg.stream_id = Some(id),
            Err(e) => {
                throw_srt(env, "CONFIG_INVALID", &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    // congestion
    if !congestion.is_null() {
        let s: String = env.get_string(congestion).map(Into::into)?;
        match Congestion::from_str_strict(&s) {
            Ok(c) => cfg.congestion = Some(c),
            Err(e) => {
                throw_srt(env, "CONFIG_INVALID", &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    // connect_timeout_ms
    if let Some(ms) = unbox_nullable_int(env, connect_timeout_ms)? {
        cfg.connect_timeout = Some(Duration::from_millis(ms as u64));
    }

    // recv_timeout_ms
    if let Some(ms) = unbox_nullable_int(env, recv_timeout_ms)? {
        cfg.recv_timeout = Some(Duration::from_millis(ms as u64));
    }

    // send_timeout_ms
    if let Some(ms) = unbox_nullable_int(env, send_timeout_ms)? {
        cfg.send_timeout = Some(Duration::from_millis(ms as u64));
    }

    // peer_latency_ms (caller-only; not in ListenerConfig)
    if let Some(ms) = unbox_nullable_int(env, peer_latency_ms)? {
        cfg.peer_latency = Some(Duration::from_millis(ms as u64));
    }

    // recv_latency_ms
    if let Some(ms) = unbox_nullable_int(env, recv_latency_ms)? {
        cfg.recv_latency = Some(Duration::from_millis(ms as u64));
    }

    // max_bandwidth_bps
    if let Some(bps) = unbox_nullable_long(env, max_bandwidth_bps)? {
        cfg.max_bandwidth = Some(MaxBandwidth::Limited(bps as u64));
    }

    // mss — u16; checked_u16 throws IllegalArgumentException on overflow
    if let Some(v) = unbox_nullable_int(env, mss)? {
        cfg.mss = Some(checked_u16(env, v as i64, "mss")?);
    }

    // payload_size — u16
    if let Some(v) = unbox_nullable_int(env, payload_size)? {
        cfg.payload_size = Some(checked_u16(env, v as i64, "payloadSize")?);
    }

    Ok(cfg)
}

/// Apply the nullable knob args into a `ListenerConfig`. Shares most knobs with
/// `build_socket_config` but omits `stream_id`, `peer_latency_ms`, and
/// `connect_timeout_ms` (not present on `ListenerConfig`).
#[allow(clippy::cast_sign_loss)]
fn build_listener_config(
    env: &mut JNIEnv,
    latency_ms: &JObject,
    passphrase: &JString,
    congestion: &JString,
    recv_timeout_ms: &JObject,
    send_timeout_ms: &JObject,
    recv_latency_ms: &JObject,
    max_bandwidth_bps: &JObject,
    mss: &JObject,
    payload_size: &JObject,
) -> jni::errors::Result<ListenerConfig> {
    let mut cfg = ListenerConfig::default();

    if let Some(ms) = unbox_nullable_int(env, latency_ms)? {
        cfg.latency = Some(Duration::from_millis(ms as u64));
    }

    if !passphrase.is_null() {
        let s: String = env.get_string(passphrase).map(Into::into)?;
        match Passphrase::new(s) {
            Ok(pp) => cfg.passphrase = Some(pp),
            Err(e) => {
                throw_srt(env, "CONFIG_INVALID", &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    if !congestion.is_null() {
        let s: String = env.get_string(congestion).map(Into::into)?;
        match Congestion::from_str_strict(&s) {
            Ok(c) => cfg.congestion = Some(c),
            Err(e) => {
                throw_srt(env, "CONFIG_INVALID", &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    if let Some(ms) = unbox_nullable_int(env, recv_timeout_ms)? {
        cfg.recv_timeout = Some(Duration::from_millis(ms as u64));
    }

    if let Some(ms) = unbox_nullable_int(env, send_timeout_ms)? {
        cfg.send_timeout = Some(Duration::from_millis(ms as u64));
    }

    if let Some(ms) = unbox_nullable_int(env, recv_latency_ms)? {
        cfg.recv_latency = Some(Duration::from_millis(ms as u64));
    }

    if let Some(bps) = unbox_nullable_long(env, max_bandwidth_bps)? {
        cfg.max_bandwidth = Some(MaxBandwidth::Limited(bps as u64));
    }

    if let Some(v) = unbox_nullable_int(env, mss)? {
        cfg.mss = Some(checked_u16(env, v as i64, "mss")?);
    }

    if let Some(v) = unbox_nullable_int(env, payload_size)? {
        cfg.payload_size = Some(checked_u16(env, v as i64, "payloadSize")?);
    }

    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Builder — nConnect
// ---------------------------------------------------------------------------

/// Open a caller-mode SRT socket. Returns `Box<Socket>` as `jlong`.
///
/// Mode ordinal mapping (Builder.Mode enum):
/// - 0 = URL_CHOICE: defer to the URL's ?mode= (default is caller)
/// - 1 = CALLER: assert URL mode must be caller
/// - 2 = LISTENER: reject from nConnect (wrong finalizer)
/// - 3 = RENDEZVOUS: reject (not supported)
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Builder_nConnect<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    mode: jni::sys::jint,
    latency_ms: JObject<'local>,
    passphrase: JString<'local>,
    stream_id: JString<'local>,
    congestion: JString<'local>,
    connect_timeout_ms: JObject<'local>,
    recv_timeout_ms: JObject<'local>,
    send_timeout_ms: JObject<'local>,
    peer_latency_ms: JObject<'local>,
    recv_latency_ms: JObject<'local>,
    max_bandwidth_bps: JObject<'local>,
    mss: JObject<'local>,
    payload_size: JObject<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Ordinal 3 = RENDEZVOUS, ordinal 2 = LISTENER (wrong for connect).
        if mode == 3 {
            throw_srt(
                env,
                "CONFIG_INVALID",
                "rendezvous mode is not yet supported by tst-srt",
            );
            return 0;
        }
        if mode == 2 {
            throw_srt(
                env,
                "CONFIG_INVALID",
                "Builder.connect() requires caller mode (mode is LISTENER)",
            );
            return 0;
        }

        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        // knob overflow checks happen BEFORE the network call (checked_u16 in build_socket_config)
        let mut cfg = match build_socket_config(
            env,
            &latency_ms,
            &passphrase,
            &stream_id,
            &congestion,
            &connect_timeout_ms,
            &recv_timeout_ms,
            &send_timeout_ms,
            &peer_latency_ms,
            &recv_latency_ms,
            &max_bandwidth_bps,
            &mss,
            &payload_size,
        ) {
            Ok(c) => c,
            Err(_) => return 0, // exception already thrown (IllegalArgumentException or SrtException)
        };

        let parsed = match SrtUrl::parse(&url_str) {
            Ok(p) => p,
            Err(e) => {
                url_error(env, &e);
                return 0;
            }
        };

        // Validate that the URL mode is caller (or default) — mode=1 (CALLER) asserts this.
        // mode=0 (URL_CHOICE) also requires URL to be caller-default.
        if parsed.mode != Mode::Caller {
            let msg = format!(
                "Builder.connect() requires URL mode=caller (default); got mode={:?}",
                parsed.mode
            );
            throw_srt(env, "CONFIG_INVALID", &msg);
            return 0;
        }

        // URL overlay AFTER kwargs — URL wins on conflict (Q4-A).
        parsed.overlay.apply_to_socket(&mut cfg);

        let addr = join_host_port(&parsed.host, parsed.port);
        let socket = match SrtSocket::connect_with(&cfg, addr.as_str()) {
            Ok(s) => s,
            Err(e) => {
                connect_error(env, &e);
                return 0;
            }
        };

        REGISTRY_SOCKET.insert(socket) as jlong
    })
}

// ---------------------------------------------------------------------------
// Builder — nListen
// ---------------------------------------------------------------------------

/// Bind a listener-mode SRT socket. Returns `Box<Listener>` as `jlong`.
///
/// Mode ordinal mapping (same as nConnect):
/// - 0 = URL_CHOICE: defer to URL (must have ?mode=listener)
/// - 1 = CALLER: reject from nListen (wrong finalizer)
/// - 2 = LISTENER: assert URL mode must be listener
/// - 3 = RENDEZVOUS: reject (not supported)
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Builder_nListen<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    mode: jni::sys::jint,
    latency_ms: JObject<'local>,
    passphrase: JString<'local>,
    stream_id: JString<'local>, // accepted but ignored for listener (no ListenerConfig.stream_id)
    congestion: JString<'local>,
    connect_timeout_ms: JObject<'local>, // accepted but ignored (no ListenerConfig.connect_timeout)
    recv_timeout_ms: JObject<'local>,
    send_timeout_ms: JObject<'local>,
    peer_latency_ms: JObject<'local>, // accepted but ignored (no ListenerConfig.peer_latency)
    recv_latency_ms: JObject<'local>,
    max_bandwidth_bps: JObject<'local>,
    mss: JObject<'local>,
    payload_size: JObject<'local>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        if mode == 3 {
            throw_srt(
                env,
                "CONFIG_INVALID",
                "rendezvous mode is not yet supported by tst-srt",
            );
            return 0;
        }
        if mode == 1 {
            throw_srt(
                env,
                "CONFIG_INVALID",
                "Builder.listen() requires listener mode (mode is CALLER)",
            );
            return 0;
        }

        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        // knob overflow checks happen BEFORE the network call
        let mut cfg = match build_listener_config(
            env,
            &latency_ms,
            &passphrase,
            &congestion,
            &recv_timeout_ms,
            &send_timeout_ms,
            &recv_latency_ms,
            &max_bandwidth_bps,
            &mss,
            &payload_size,
        ) {
            Ok(c) => c,
            Err(_) => return 0,
        };

        // Silently ignore: stream_id (no ListenerConfig field), connect_timeout_ms
        // (no ListenerConfig field), peer_latency_ms (no ListenerConfig field).
        // This matches tst-py: apply_stream_id goes to socket_cfg only; peer_latency
        // goes to socket_cfg only; connect_timeout goes to socket_cfg only.
        let _ = (&stream_id, &connect_timeout_ms, &peer_latency_ms); // suppress unused warnings

        let parsed = match SrtUrl::parse(&url_str) {
            Ok(p) => p,
            Err(e) => {
                url_error(env, &e);
                return 0;
            }
        };

        if parsed.mode != Mode::Listener {
            let msg = format!(
                "Builder.listen() requires URL ?mode=listener; got mode={:?}",
                parsed.mode
            );
            throw_srt(env, "CONFIG_INVALID", &msg);
            return 0;
        }

        // URL overlay AFTER kwargs.
        parsed.overlay.apply_to_listener(&mut cfg);

        let addr = if parsed.host.is_empty() {
            format!("0.0.0.0:{}", parsed.port)
        } else {
            join_host_port(&parsed.host, parsed.port)
        };

        let listener = match SrtListener::bind_with(&cfg, addr.as_str()) {
            Ok(l) => l,
            Err(e) => {
                bind_error(env, &e);
                return 0;
            }
        };

        register_listener(listener) as jlong
    })
}

// ---------------------------------------------------------------------------
// Socket — nIntoSender
// ---------------------------------------------------------------------------

/// Consume a `Box<Socket>` and produce a `Box<PlSender<SrtTransport>>`.
/// The Java caller MUST zero its socket handle field after this returns.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nIntoSender(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Take the Socket out of its registry (atomic; idempotent). `None` = already
        // closed/consumed → the Java caller zeroed its field, so this is a stale call.
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        let transport = SrtTransport::new(socket);
        let sender = PlSender::new(transport, SenderConfig::default());
        super::transport::register_sender(sender)
    })
}

// ---------------------------------------------------------------------------
// Socket — nIntoReceiver
// ---------------------------------------------------------------------------

/// Consume a `Box<Socket>` and produce a `Box<PlReceiver<SrtTransport>>`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nIntoReceiver(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        let transport = SrtTransport::new(socket);
        let receiver = PlReceiver::new(transport, ReceiverConfig::default());
        super::transport::register_receiver(receiver)
    })
}

// ---------------------------------------------------------------------------
// Socket — nLocalAddr / nPeerAddr / nStreamId / nClose
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nLocalAddr<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let addr = match REGISTRY_SOCKET.with(handle as u64, |s| s.local_addr()) {
            Some(r) => r,
            None => {
                crate::error::throw_closed(env, "Socket");
                return std::ptr::null_mut();
            }
        };
        match addr {
            Ok(addr) => match build_host_port(env, addr) {
                Ok(obj) => obj.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(e) => {
                io_error(env, &e);
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nPeerAddr<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let addr = match REGISTRY_SOCKET.with(handle as u64, |s| s.peer_addr()) {
            Some(r) => r,
            None => {
                crate::error::throw_closed(env, "Socket");
                return std::ptr::null_mut();
            }
        };
        match addr {
            Ok(addr) => match build_host_port(env, addr) {
                Ok(obj) => obj.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(e) => {
                io_error(env, &e);
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nStreamId<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        // Closed handle or absent stream-id both yield null (no throw, matching the
        // original contract).
        let id = REGISTRY_SOCKET
            .with(handle as u64, |s| s.stream_id().map(str::to_owned))
            .flatten();
        match id {
            Some(id) => match env.new_string(id) {
                Ok(s) => s.into_raw() as jobject,
                Err(_) => std::ptr::null_mut(),
            },
            None => std::ptr::null_mut(),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Socket_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: only the winning close gets the Socket back.
        if let Some(socket) = REGISTRY_SOCKET.close(handle as u64) {
            let _ = socket.close();
        }
    })
}

// ---------------------------------------------------------------------------
// Listener — nAccept
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Listener_nAccept(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    timeout_ms: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Run accept INSIDE the registry lease — the closure holds the entry's
        // resource lock, so a racing `nClose` (which fires the cancel hook before
        // taking the lock) wakes a parked accept rather than freeing under it.
        // `None` = absent/closed/taken → throw and bail.
        let result = REGISTRY_LISTENER.with(handle as u64, |listener| {
            if timeout_ms < 0 {
                listener.accept()
            } else {
                listener.accept_timeout(Duration::from_millis(timeout_ms as u64))
            }
        });
        match result {
            Some(Ok((socket, _peer))) => REGISTRY_SOCKET.insert(socket) as jlong,
            Some(Err(e)) => {
                accept_error(env, &e);
                0
            }
            None => {
                crate::error::throw_closed(env, "Listener");
                0
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Listener — nCancelHandle
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Listener_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // The listener's independent `SrtCancelHandle` adapter, captured at
        // registration and read WITHOUT the resource lock — a parked `accept`
        // holds that lock, and this is exactly the call that must wake it.
        // `cancel()` closes the SRTSOCKET WITHOUT freeing the Listener — the
        // sanctioned cross-thread wake. Mirrors tst-py's PyCancelHandle.
        let cancel = REGISTRY_LISTENER.cancel_target(handle as u64);
        match cancel {
            Some(inner) => JniCancel {
                inner,
                flag: AtomicBool::new(false),
            }
            .into_handle(),
            None => 0,
        }
    })
}

// ---------------------------------------------------------------------------
// Listener — nLocalAddr / nClose
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Listener_nLocalAddr<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jobject {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        let addr_result =
            match REGISTRY_LISTENER.with(handle as u64, |listener| listener.local_addr()) {
                Some(r) => r,
                None => {
                    crate::error::throw_closed(env, "Listener");
                    return std::ptr::null_mut();
                }
            };
        match addr_result {
            Ok(addr) => match build_host_port(env, addr) {
                Ok(obj) => obj.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Err(e) => {
                io_error(env, &e);
                std::ptr::null_mut()
            }
        }
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_srt_Listener_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // `REGISTRY.close` fires the cancel hook FIRST (waking any parked accept via
        // the independent SRTSOCKET cancel WITHOUT touching the Listener allocation),
        // THEN takes the Listener under the resource lock — blocking until the woken
        // accept released it. So the free below is sound against a parked accept.
        // Atomic + idempotent: a double close finds the id gone → no-op.
        if let Some(listener) = REGISTRY_LISTENER.close(handle as u64) {
            let _ = listener.close();
        }
    })
}
