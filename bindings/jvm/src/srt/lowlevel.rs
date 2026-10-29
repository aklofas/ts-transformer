//! JNI exports for `org.tstrans.srt.Builder`, `org.tstrans.srt.Socket`, and
//! `org.tstrans.srt.Listener` — the low-level SRT primitives.
//!
//! Handle lifecycle:
//! - `nConnect` / `nListen` register via `REGISTRY.insert[_with_cancel]`.
//! - Non-consuming methods lease via `REGISTRY.with`.
//! - `nInto*` CONSUME the `Socket` via `REGISTRY.close` and return a new
//!   `org.tstrans.shells` handle. The Java caller zeros its own
//!   socket handle on success — the `handle = 0` assignment is the next statement
//!   after the native call.
//! - `nClose` takes + tears down via `REGISTRY.close`.
//!
//! Errors are thrown through `throw_srt` with a `BindingErrorKind`; the kind
//! tables behind them are checked by `scripts/check/repo/kind-equivalence.sh`.

#![allow(clippy::too_many_arguments)]

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use jni::Env;
use jni::jni_sig;
use jni::jni_str;
use jni::objects::{JBooleanArray, JByteArray, JIntArray, JObject, JString, JValue};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong, jobject};
use tst_core::transport::{RecvTransport, Transport, TransportCancel};
use tst_pipeline::binding::{BindingErrorKind, Domain, Opened, Owned};
use tst_srt::{
    SrtListener, SrtListenerConfig, SrtSocket, SrtSocketConfig, SrtTransport, SrtUrl,
    options::{Congestion, MaxBandwidth, Passphrase, StreamId},
    url::Mode,
};

use super::errors::{srt_error, throw_srt, url_error};
use crate::handle::{HandleRegistry, OwnedRegistry};
use crate::jutil::{checked_u16, join_host_port};
use crate::mpegts::build_demux_config_from_args;
use crate::mpegts::muxer::build_muxer_config_from_arrays;
use crate::shells;

/// Per-type leased-handle registry for `org.tstrans.srt.Socket` (an `SrtSocket`).
/// `nConnect`/`nAccept` insert; per-call methods lease; `nClose` and the
/// `nInto*` consumers `close` (taking the resource for teardown / hand-off).
pub(crate) static REGISTRY_SOCKET: LazyLock<HandleRegistry<SrtSocket>> =
    LazyLock::new(HandleRegistry::new);

/// Construction-constant view of a `Listener`: the bound address is
/// captured at registration so `localAddr()` never takes the slot a parked
/// `accept` holds — otherwise `Listener.localAddr()` blocks behind
/// `accept(null)` on another thread.
pub(crate) struct ListenerSnapshot {
    pub local_addr: Result<std::net::SocketAddr, String>,
}

/// Per-type `Owned`-backed registry for `org.tstrans.srt.Listener`.
static REGISTRY_LISTENER: LazyLock<OwnedRegistry<SrtListener, ListenerSnapshot>> =
    LazyLock::new(OwnedRegistry::new);

// ---------------------------------------------------------------------------
// Listener registration
// ---------------------------------------------------------------------------
//
// MEMORY-SAFETY / LIFETIME CONTRACT. The `Owned`-backed registry is the
// process-global synchronisation point. `nAccept` runs `accept()` INSIDE
// `Owned::with_mut`, i.e. holding the entry's slot, so every `Listener` field
// read happens in the critical section. `nClose` routes through
// `OwnedRegistry::close`, which cancels FIRST (closing the SRTSOCKET, waking a
// parked accept WITHOUT touching the `Listener` allocation) and only THEN takes
// the value out of the slot — blocking until the woken accept released it.
// `accept()` is single-owner (single-iterator) per the Java contract.

/// Register a `Listener` as an `Owned` entry: its independent `SrtCancelHandle`
/// is the cancel target (fired by `close`, and handed out by `nCancelHandle`
/// while an `accept` is parked), and the bound address is snapshotted so
/// `nLocalAddr` never touches the slot.
fn register_listener(listener: SrtListener) -> u64 {
    let cancel: Arc<dyn TransportCancel> = Arc::new(listener.cancel_handle());
    let snapshot = ListenerSnapshot {
        local_addr: listener.local_addr().map_err(|e| e.to_string()),
    };
    REGISTRY_LISTENER.insert(Owned::new(listener, cancel, snapshot))
}

// ---------------------------------------------------------------------------
// Helper: build a HostPort JObject from a std::net::SocketAddr
// ---------------------------------------------------------------------------

fn build_host_port<'local>(
    env: &mut Env<'local>,
    addr: std::net::SocketAddr,
) -> jni::errors::Result<JObject<'local>> {
    // Ensure capacity for: HostPort ctor arg (String) + local refs.
    env.ensure_local_capacity(8)?;
    let host_str = addr.ip().to_string();
    let host = env.new_string(&host_str)?;
    env.new_object(
        FindClass(jni_str!("org/tstrans/srt/HostPort")),
        jni_sig!("(Ljava/lang/String;I)V"),
        &[JValue::Object(&host), JValue::Int(addr.port() as i32)],
    )
}

// ---------------------------------------------------------------------------
// Helper: unbox nullable Integer / Long JObject args
// ---------------------------------------------------------------------------

/// Unbox a nullable `java.lang.Integer` JNI argument. Returns `None` for null.
fn unbox_nullable_int(env: &mut Env, obj: &JObject) -> jni::errors::Result<Option<i32>> {
    if obj.is_null() {
        return Ok(None);
    }
    let v = env
        .call_method(obj, jni_str!("intValue"), jni_sig!("()I"), &[])?
        .i()?;
    Ok(Some(v))
}

/// Unbox a nullable `java.lang.Long` JNI argument. Returns `None` for null.
fn unbox_nullable_long(env: &mut Env, obj: &JObject) -> jni::errors::Result<Option<i64>> {
    if obj.is_null() {
        return Ok(None);
    }
    let v = env
        .call_method(obj, jni_str!("longValue"), jni_sig!("()J"), &[])?
        .j()?;
    Ok(Some(v))
}

// ---------------------------------------------------------------------------
// Helper: build SrtSocketConfig from knob args
// ---------------------------------------------------------------------------
//
// Mode ordinals (the Java Builder.Mode enum, passed as mode.ordinal()):
//   URL_CHOICE=0, CALLER=1, LISTENER=2, RENDEZVOUS=3.
// nConnect rejects LISTENER(2) and RENDEZVOUS(3); nListen rejects CALLER(1)
// and RENDEZVOUS(3). URL_CHOICE(0) defers to the URL's ?mode= parameter.

/// Apply the nullable knob args into a `SrtSocketConfig`. Returns `Err` if any
/// validated knob (passphrase, stream_id, congestion, mss, payload_size) fails.
/// Does NOT apply the URL's keys — the caller does that after this returns.
#[allow(clippy::cast_sign_loss)]
fn build_socket_config(
    env: &mut Env,
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
) -> jni::errors::Result<SrtSocketConfig> {
    let mut cfg = SrtSocketConfig::default();

    // latency_ms — applied to both SRTO_LATENCY on socket configs
    if let Some(ms) = unbox_nullable_int(env, latency_ms)? {
        cfg.latency = Some(Duration::from_millis(ms as u64));
    }

    // passphrase
    if !passphrase.is_null() {
        let s: String = passphrase.try_to_string(env)?;
        match Passphrase::new(s) {
            Ok(pp) => cfg.passphrase = Some(pp),
            Err(e) => {
                throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    // stream_id (caller-side only; not in SrtListenerConfig)
    if !stream_id.is_null() {
        let s: String = stream_id.try_to_string(env)?;
        match StreamId::new(s) {
            Ok(id) => cfg.stream_id = Some(id),
            Err(e) => {
                throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    // congestion
    if !congestion.is_null() {
        let s: String = congestion.try_to_string(env)?;
        match Congestion::from_str_strict(&s) {
            Ok(c) => cfg.congestion = Some(c),
            Err(e) => {
                throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
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

    // peer_latency_ms (caller-only; not in SrtListenerConfig)
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

/// Apply the nullable knob args into a `SrtListenerConfig`. Shares most knobs with
/// `build_socket_config` but omits `stream_id`, `peer_latency_ms`, and
/// `connect_timeout_ms` (not present on `SrtListenerConfig`).
#[allow(clippy::cast_sign_loss)]
fn build_listener_config(
    env: &mut Env,
    latency_ms: &JObject,
    passphrase: &JString,
    congestion: &JString,
    recv_timeout_ms: &JObject,
    send_timeout_ms: &JObject,
    recv_latency_ms: &JObject,
    max_bandwidth_bps: &JObject,
    mss: &JObject,
    payload_size: &JObject,
) -> jni::errors::Result<SrtListenerConfig> {
    let mut cfg = SrtListenerConfig::default();

    if let Some(ms) = unbox_nullable_int(env, latency_ms)? {
        cfg.socket.latency = Some(Duration::from_millis(ms as u64));
    }

    if !passphrase.is_null() {
        let s: String = passphrase.try_to_string(env)?;
        match Passphrase::new(s) {
            Ok(pp) => cfg.socket.passphrase = Some(pp),
            Err(e) => {
                throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    if !congestion.is_null() {
        let s: String = congestion.try_to_string(env)?;
        match Congestion::from_str_strict(&s) {
            Ok(c) => cfg.socket.congestion = Some(c),
            Err(e) => {
                throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
                return Err(jni::errors::Error::JavaException);
            }
        }
    }

    if let Some(ms) = unbox_nullable_int(env, recv_timeout_ms)? {
        cfg.socket.recv_timeout = Some(Duration::from_millis(ms as u64));
    }

    if let Some(ms) = unbox_nullable_int(env, send_timeout_ms)? {
        cfg.socket.send_timeout = Some(Duration::from_millis(ms as u64));
    }

    if let Some(ms) = unbox_nullable_int(env, recv_latency_ms)? {
        cfg.socket.recv_latency = Some(Duration::from_millis(ms as u64));
    }

    if let Some(bps) = unbox_nullable_long(env, max_bandwidth_bps)? {
        cfg.socket.max_bandwidth = Some(MaxBandwidth::Limited(bps as u64));
    }

    if let Some(v) = unbox_nullable_int(env, mss)? {
        cfg.socket.mss = Some(checked_u16(env, v as i64, "mss")?);
    }

    if let Some(v) = unbox_nullable_int(env, payload_size)? {
        cfg.socket.payload_size = Some(checked_u16(env, v as i64, "payloadSize")?);
    }

    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Builder — nConnect
// ---------------------------------------------------------------------------

jni_native! {
    /// Open a caller-mode SRT socket. Returns `Box<Socket>` as `jlong`.
    ///
    /// Mode ordinal mapping (Builder.Mode enum):
    /// - 0 = URL_CHOICE: defer to the URL's ?mode= (default is caller)
    /// - 1 = CALLER: assert URL mode must be caller
    /// - 2 = LISTENER: reject from nConnect (wrong finalizer)
    /// - 3 = RENDEZVOUS: reject (not supported)
    fn Java_org_tstrans_srt_Builder_nConnect<'local>(
        env,
        _class,
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
    ) -> jlong = 0 => {
        // Ordinal 3 = RENDEZVOUS, ordinal 2 = LISTENER (wrong for connect).
        // Order pinned by BuilderModeOrdinalTest.
        if mode == 3 {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "rendezvous mode is not yet supported by tst-srt",
            );
            return 0;
        }
        if mode == 2 {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "Builder.connect() requires caller mode (mode is LISTENER)",
            );
            return 0;
        }

        let url_str: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
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
            throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
            return 0;
        }

        // The URL's keys AFTER kwargs — URL wins on conflict.
        cfg.merge_from(&parsed.config);

        let addr = join_host_port(&parsed.host, parsed.port);
        let socket = match SrtSocket::connect_with(&cfg, addr.as_str()) {
            Ok(s) => s,
            Err(e) => {
                srt_error(env, e);
                return 0;
            }
        };

        REGISTRY_SOCKET.insert(socket) as jlong
    }
}

// ---------------------------------------------------------------------------
// Builder — nListen
// ---------------------------------------------------------------------------

jni_native! {
    /// Bind a listener-mode SRT socket. Returns `Box<Listener>` as `jlong`.
    ///
    /// Mode ordinal mapping (same as nConnect):
    /// - 0 = URL_CHOICE: defer to URL (must have ?mode=listener)
    /// - 1 = CALLER: reject from nListen (wrong finalizer)
    /// - 2 = LISTENER: assert URL mode must be listener
    /// - 3 = RENDEZVOUS: reject (not supported)
    fn Java_org_tstrans_srt_Builder_nListen<'local>(
        env,
        _class,
        url: JString<'local>,
        mode: jni::sys::jint,
        latency_ms: JObject<'local>,
        passphrase: JString<'local>,
        stream_id: JString<'local>, // accepted but ignored for a listener (a listener does not apply stream_id)
        congestion: JString<'local>,
        connect_timeout_ms: JObject<'local>, // accepted but ignored (a listener does not apply connect_timeout)
        recv_timeout_ms: JObject<'local>,
        send_timeout_ms: JObject<'local>,
        peer_latency_ms: JObject<'local>, // accepted but ignored (a listener does not apply peer_latency)
        recv_latency_ms: JObject<'local>,
        max_bandwidth_bps: JObject<'local>,
        mss: JObject<'local>,
        payload_size: JObject<'local>,
    ) -> jlong = 0 => {
        // Order pinned by BuilderModeOrdinalTest.
        if mode == 3 {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "rendezvous mode is not yet supported by tst-srt",
            );
            return 0;
        }
        if mode == 1 {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                "Builder.listen() requires listener mode (mode is CALLER)",
            );
            return 0;
        }

        let url_str: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
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

        // Silently ignore: stream_id, connect_timeout_ms and peer_latency_ms
        // (a listener does not apply them).
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
            throw_srt(env, BindingErrorKind::ConfigInvalid, &msg);
            return 0;
        }

        // The URL's keys AFTER kwargs.
        cfg.socket.merge_from(&parsed.config);

        let addr = if parsed.host.is_empty() {
            format!("0.0.0.0:{}", parsed.port)
        } else {
            join_host_port(&parsed.host, parsed.port)
        };

        let listener = match SrtListener::bind_with(&cfg, addr.as_str()) {
            Ok(l) => l,
            Err(e) => {
                srt_error(env, e);
                return 0;
            }
        };

        register_listener(listener) as jlong
    }
}

// ---------------------------------------------------------------------------
// Socket — nInto{Sender,Receiver,MuxSender,DemuxReceiver}
// ---------------------------------------------------------------------------
//
// Each CONSUMES the `Socket` via `REGISTRY_SOCKET.close` and returns the key of
// a new `org.tstrans.shells` handle over it; the Java caller zeroes its own
// socket handle unconditionally after the call (the socket is consumed even
// when a config error follows).

/// The socket as an opened send transport. The cancel target is read BEFORE the
/// socket moves into the transport (obtain-before-move); `Owned` keeps it
/// outside the slot, so `nCancelHandle` answers while a send is parked and
/// `close()` cancels first.
fn opened_send(socket: SrtSocket) -> Opened<Box<dyn Transport>> {
    let cancel: Arc<dyn TransportCancel> = Arc::new(socket.cancel_handle());
    Opened::new(Box::new(SrtTransport::new(socket)), cancel, Domain::Srt)
}

/// The socket as an opened receive transport; see [`opened_send`].
fn opened_recv(socket: SrtSocket) -> Opened<Box<dyn RecvTransport>> {
    let cancel: Arc<dyn TransportCancel> = Arc::new(socket.cancel_handle());
    Opened::new(Box::new(SrtTransport::new(socket)), cancel, Domain::Srt)
}

jni_native! {
    /// `Socket.nIntoSender(handle)`.
    fn Java_org_tstrans_srt_Socket_nIntoSender(env, _class, handle: jlong) -> jlong = 0 => {
        // `None` = already closed/consumed: a stale call.
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        shells::sender::register(opened_send(socket))
    }
}

jni_native! {
    /// `Socket.nIntoReceiver(handle)`.
    fn Java_org_tstrans_srt_Socket_nIntoReceiver(env, _class, handle: jlong) -> jlong = 0 => {
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        shells::receiver::register(opened_recv(socket))
    }
}

jni_native! {
    /// `Socket.nIntoMuxSender(handle, ...programConfig...)`.
    fn Java_org_tstrans_srt_Socket_nIntoMuxSender<'local>(
        env,
        _class,
        handle: jlong,
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
    ) -> jlong = 0 => {
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
            // Refused before the take: the socket stays registered with its
            // caller, so a corrected retry can still convert it.
            Err(()) => return 0,
        };
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        shells::mux_sender::register(env, opened_send(socket), cfg)
    }
}

jni_native! {
    /// `Socket.nIntoDemuxReceiver(handle)` — default demux options.
    fn Java_org_tstrans_srt_Socket_nIntoDemuxReceiver(env, _class, handle: jlong) -> jlong = 0 => {
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        shells::demux_receiver::register(opened_recv(socket), None)
    }
}

jni_native! {
    /// `Socket.nIntoDemuxReceiverWithConfig(handle, ...)` — explicit `DemuxerConfig`.
    fn Java_org_tstrans_srt_Socket_nIntoDemuxReceiverWithConfig(
        env,
        _class,
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
            // Refused before the take (see nIntoMuxSender).
            return 0;
        };
        let Some(socket) = REGISTRY_SOCKET.close(handle as u64) else {
            crate::error::throw_closed(env, "Socket");
            return 0;
        };
        shells::demux_receiver::register(opened_recv(socket), Some(opts))
    }
}

// ---------------------------------------------------------------------------
// Socket — nLocalAddr / nPeerAddr / nStreamId / nClose
// ---------------------------------------------------------------------------

jni_native! {
    fn Java_org_tstrans_srt_Socket_nLocalAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
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
                srt_error(env, e);
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_Socket_nPeerAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
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
                srt_error(env, e);
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_Socket_nStreamId<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
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
    }
}

jni_native! {
    fn Java_org_tstrans_srt_Socket_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: only the winning close gets the Socket back.
        if let Some(socket) = REGISTRY_SOCKET.close(handle as u64) {
            socket.close();
        }
    }
}

// ---------------------------------------------------------------------------
// Listener — nAccept
// ---------------------------------------------------------------------------

jni_native! {
    fn Java_org_tstrans_srt_Listener_nAccept(
        env,
        _class,
        handle: jlong,
        timeout_ms: jlong,
    ) -> jlong = 0 => {
        // Run accept INSIDE the registry lease — the closure holds the entry's
        // resource lock, so a racing `nClose` (which fires the cancel hook before
        // taking the lock) wakes a parked accept rather than freeing under it.
        // `None` = absent/closed/taken → throw and bail.
        let result = REGISTRY_LISTENER.with_mut(handle as u64, |listener| {
            if timeout_ms < 0 {
                listener.accept()
            } else {
                listener.accept_timeout(Duration::from_millis(timeout_ms as u64))
            }
        });
        match result {
            Ok(Ok((socket, _peer))) => REGISTRY_SOCKET.insert(socket) as jlong,
            Ok(Err(e)) => {
                srt_error(env, e);
                0
            }
            Err(state) => {
                crate::error::throw_handle_state(env, "Listener", &state);
                0
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Listener — nCancelHandle
// ---------------------------------------------------------------------------

jni_native! {
    fn Java_org_tstrans_srt_Listener_nCancelHandle(_env, _class, handle: jlong) -> jlong = 0 => {
        // The listener's independent `SrtCancelHandle`, captured at
        // registration and read WITHOUT the resource lock — a parked `accept`
        // holds that lock, and this is exactly the call that must wake it.
        // `cancel()` closes the SRTSOCKET WITHOUT freeing the Listener — the
        // sanctioned cross-thread wake. Mirrors tst-py's PyCancelHandle.
        REGISTRY_LISTENER
            .cancel_view(handle as u64)
            .map_or(0, crate::shells::cancel::cancel_view_handle)
    }
}

// ---------------------------------------------------------------------------
// Listener — nLocalAddr / nClose
// ---------------------------------------------------------------------------

jni_native! {
    fn Java_org_tstrans_srt_Listener_nLocalAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jobject = std::ptr::null_mut() => {
        // Lock-free: the bound address was snapshotted at registration, so this
        // answers while `accept` is parked on the slot — a getter must never
        // block behind a parked call.
        match REGISTRY_LISTENER.snapshot(handle as u64, |s| s.local_addr.clone()) {
            Some(Ok(addr)) => match build_host_port(env, addr) {
                Ok(obj) => obj.into_raw(),
                Err(_) => std::ptr::null_mut(),
            },
            Some(Err(msg)) => {
                throw_srt(env, BindingErrorKind::SrtIo, &msg);
                std::ptr::null_mut()
            }
            None => {
                crate::error::throw_closed(env, "Listener");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_Listener_nClose(_env, _class, handle: jlong) = () => {
        // `OwnedRegistry::close` cancels FIRST (waking any parked accept via the
        // independent SRTSOCKET cancel WITHOUT touching the Listener allocation),
        // THEN takes the Listener out of the slot — blocking until the woken
        // accept released it. So the free below is sound against a parked accept.
        // Atomic + idempotent: a double close finds the id gone → no-op.
        if let Some(listener) = REGISTRY_LISTENER.close(handle as u64) {
            listener.close();
        }
    }
}
