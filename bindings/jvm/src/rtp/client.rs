//! `org.tstrans.rtp` RTSP client JNI surface — `RtspClient`, `RtspSession`,
//! `RtspCancelHandle`, and the auth/config/stats value types' native backing.
//! Ports tst-py's `bindings/python/src/rtp/client.rs`. Natives added in Tasks 4-5.
//! Task 15 adds `nConnectH264` (RtspClient) + `nIntoH264Receiver` (RtspSession).

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jint, jlong};

use secrecy::SecretString;
use tst_core::mpegts::demux::DemuxerConfig;
use tst_rtp::H264DepayConfig;
use tst_rtp::RtspClientBuilder;
use tst_rtp::error::RtspError;
use tst_rtp::rtsp::client::RtspCancelHandle as RustRtspCancel;
use tst_rtp::rtsp::client::RtspClient as RustRtspClient;
use tst_rtp::rtsp::client::session::RtspSession as RustRtspSession;

use crate::handle::{HandleRegistry, TryWith};
use crate::mpegts::build_demux_config_from_args;

use super::demux_receiver::demux_receiver_handle_from_transport;
use super::errors::{rtsp_error_to_jvm, throw_rtsp};

/// Parse `RtspClientConfig.tlsRootCertsPem` (a PEM bundle) into a
/// `rustls::RootCertStore` and hand it to the builder — `rtsps://`
/// verification against a private CA. Port of tst-py's `apply_tls_roots`
/// (bindings/python/src/rtp/client.rs) with identical error semantics:
/// invalid PEM / rejected anchor / zero certs all throw kind TLS. A null
/// Java array is a no-op (platform native trust roots). Returns None with
/// a pending exception on failure.
fn apply_tls_roots(
    env: &mut JNIEnv,
    builder: RtspClientBuilder,
    pem: &JByteArray<'_>,
) -> Option<RtspClientBuilder> {
    if pem.is_null() {
        return Some(builder);
    }
    let bytes = match env.convert_byte_array(pem) {
        Ok(b) => b,
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            return None;
        }
    };
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = std::io::BufReader::new(&bytes[..]);
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = match cert {
            Ok(c) => c,
            Err(e) => {
                throw_rtsp(env, "TLS", &format!("tlsRootCertsPem: invalid PEM: {e}"));
                return None;
            }
        };
        if let Err(e) = roots.add(cert) {
            throw_rtsp(
                env,
                "TLS",
                &format!("tlsRootCertsPem: certificate rejected as trust anchor: {e}"),
            );
            return None;
        }
        added += 1;
    }
    if added == 0 {
        throw_rtsp(env, "TLS", "tlsRootCertsPem contains no certificates");
        return None;
    }
    Some(builder.tls_root_certs(roots))
}

/// Boxed behind `org.tstrans.rtp.RtspCancelHandle.handle`. Wraps tst-rtp's
/// self-contained `RtspCancelHandle` (owns its own `Arc<AtomicBool>` flag).
pub(super) struct JniRtspCancel {
    pub(super) inner: RustRtspCancel,
}

/// Per-type leased-handle registry for `org.tstrans.rtp.RtspCancelHandle`. A
/// cancel target — register with `insert` (cancel = None).
static REGISTRY_CANCEL: LazyLock<HandleRegistry<JniRtspCancel>> =
    LazyLock::new(HandleRegistry::new);

/// Per-type leased-handle registry for `org.tstrans.rtp.RtspSession`. No registry
/// cancel hook (teardown is the session's own `torn_down` flag); the cross-thread
/// stop routes through `RtspCancelHandle`, not `close()`.
static REGISTRY_SESSION: LazyLock<HandleRegistry<JniRtspSession>> =
    LazyLock::new(HandleRegistry::new);

impl JniRtspCancel {
    pub(super) fn into_handle(self) -> jlong {
        REGISTRY_CANCEL.insert(self) as jlong
    }
}

/// Flip the cancel flag. Wakes a parked connect/pause/play/teardown at the next
/// ~100 ms poll. Guards a closed (zero) handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspCancelHandle_nCancel(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        if REGISTRY_CANCEL
            .with(handle as u64, |c| c.inner.cancel())
            .is_none()
        {
            crate::error::throw_closed(env, "RtspCancelHandle");
        }
    })
}

/// Report whether the backing flag has been flipped. Guards a closed handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspCancelHandle_nIsCancelled(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |env| {
        match REGISTRY_CANCEL.try_with(handle as u64, |c| u8::from(c.inner.is_cancelled())) {
            TryWith::Ran(v) => v,
            _ => {
                crate::error::throw_closed(env, "RtspCancelHandle");
                0
            }
        }
    })
}

/// Free the boxed cancel handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspCancelHandle_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent drop.
        let _ = REGISTRY_CANCEL.close(handle as u64);
    })
}

/// Native backing for `org.tstrans.rtp.RtspSession`. Faithful port of tst-py's
/// `PyRtspSession`: `client` + `session` behind `Arc<Mutex<Option<..>>>` (so the
/// control natives clone the Arc and lock, never holding a `&mut` to the box;
/// `cancel_handle` clones a self-contained handle out from under the lock);
/// `torn_down` so a duplicate teardown is a no-op.
///
/// `h264_depay_config` is stashed only when created via `nConnectH264` (the H.264
/// path — it carries the SDP-negotiated payload type and out-of-band SPS/PPS).
/// `nIntoH264Receiver` takes it + the data-plane session together; `None` when
/// the session was created via plain `nConnect` (MP2T path) or after consumption.
struct JniRtspSession {
    client: Arc<Mutex<Option<RustRtspClient>>>,
    session: Arc<Mutex<Option<RustRtspSession>>>,
    torn_down: Arc<AtomicBool>,
    /// `Some(_)` only for `nConnectH264`-created sessions (Task 15). `None` for
    /// plain `nConnect` sessions and after `nIntoH264Receiver` has consumed it.
    h264_depay_config: Arc<Mutex<Option<H264DepayConfig>>>,
}

/// `RtspClient.nConnect(url, authUser, authPassword, keepalive, tlsRootCertsPem)`
/// — run OPTIONS/DESCRIBE/SETUP/PLAY, return a `Box<JniRtspSession>` handle, or 0
/// with a pending `RtspException` on failure. Ports `PyRtspClient::connect`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspClient_nConnect(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
    auth_user: JString<'_>,
    auth_password: JString<'_>,
    keepalive: jboolean,
    tls_root_certs_pem: JByteArray<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        // 1. Builder construction (URL parse) — the only eager typed RtspError.
        let mut builder = match RtspClientBuilder::new(&url_str) {
            Ok(b) => b,
            Err(e) => {
                rtsp_error_to_jvm(env, &e);
                return 0;
            }
        };

        // 2. Wire credentials when present. SecretString wrapping happens here.
        //    A null Java String arrives as a null JString (is_null()). The algorithm
        //    is NOT passed: tst-rtp's challenge handler picks it from the server's
        //    WWW-Authenticate header (matches tst-py).
        if !auth_user.is_null() {
            let user: String = match env.get_string(&auth_user) {
                Ok(s) => s.into(),
                Err(e) => {
                    let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                    return 0;
                }
            };
            let pw: String = if auth_password.is_null() {
                String::new()
            } else {
                match env.get_string(&auth_password) {
                    Ok(s) => s.into(),
                    Err(e) => {
                        let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                        return 0;
                    }
                }
            };
            builder = builder.auth(user, SecretString::from(pw));
        }

        // 3. keepalive=false disables the auto-keepalive thread.
        if keepalive == 0 {
            builder = builder.no_auto_keepalive(true);
        }

        // 4. Custom-CA trust roots, if supplied. Must land before any connect I/O.
        let builder = match apply_tls_roots(env, builder, &tls_root_certs_pem) {
            Some(b) => b,
            None => return 0, // exception pending
        };

        // 5. Drive the control-plane to PLAY (network I/O). Ports tst-py's
        //    allow_threads closure — JNI has no GIL to release.
        let result: Result<(RustRtspClient, RustRtspSession), RtspError> = (|| {
            let mut client = builder.connect()?;
            let _opts = client.options()?;
            let sdp = client.describe()?;
            let session = client.setup_mp2t_auto(&sdp)?;
            let _info = client.play()?;
            Ok((client, session))
        })();

        let (client, session) = match result {
            Ok(pair) => pair,
            Err(e) => {
                rtsp_error_to_jvm(env, &e);
                return 0;
            }
        };

        REGISTRY_SESSION.insert(JniRtspSession {
            client: Arc::new(Mutex::new(Some(client))),
            session: Arc::new(Mutex::new(Some(session))),
            torn_down: Arc::new(AtomicBool::new(false)),
            h264_depay_config: Arc::new(Mutex::new(None)), // MP2T path — no H.264 config
        }) as jlong
    })
}

/// `RtspClient.nConnectH264(url, authUser, authPassword, keepalive, tlsRootCertsPem)`
/// — run OPTIONS/DESCRIBE/SETUP for H.264 media/PLAY; return a `JniRtspSession`
/// handle with the session slot holding an `H264DepayConfig`. Ports
/// `PyRtspClient::connect_h264`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspClient_nConnectH264(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    url: JString<'_>,
    auth_user: JString<'_>,
    auth_password: JString<'_>,
    keepalive: jboolean,
    tls_root_certs_pem: JByteArray<'_>,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        let url_str: String = match env.get_string(&url) {
            Ok(s) => s.into(),
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                return 0;
            }
        };

        let mut builder = match RtspClientBuilder::new(&url_str) {
            Ok(b) => b,
            Err(e) => {
                rtsp_error_to_jvm(env, &e);
                return 0;
            }
        };

        if !auth_user.is_null() {
            let user: String = match env.get_string(&auth_user) {
                Ok(s) => s.into(),
                Err(e) => {
                    let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                    return 0;
                }
            };
            let pw: String = if auth_password.is_null() {
                String::new()
            } else {
                match env.get_string(&auth_password) {
                    Ok(s) => s.into(),
                    Err(e) => {
                        let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
                        return 0;
                    }
                }
            };
            builder = builder.auth(user, SecretString::from(pw));
        }

        if keepalive == 0 {
            builder = builder.no_auto_keepalive(true);
        }

        // Custom-CA trust roots, if supplied. Must land before any connect I/O.
        let builder = match apply_tls_roots(env, builder, &tls_root_certs_pem) {
            Some(b) => b,
            None => return 0, // exception pending
        };

        // Drive OPTIONS / DESCRIBE / SETUP (H.264 path) / PLAY.
        let result: Result<(RustRtspClient, RustRtspSession, H264DepayConfig), RtspError> =
            (|| {
                let mut client = builder.connect()?;
                let _opts = client.options()?;
                let sdp = client.describe()?;
                let (session, depay_config) = client.setup_h264_auto(&sdp)?;
                let _info = client.play()?;
                Ok((client, session, depay_config))
            })();

        let (client, session, depay_config) = match result {
            Ok(triple) => triple,
            Err(e) => {
                rtsp_error_to_jvm(env, &e);
                return 0;
            }
        };

        // Stash the H264DepayConfig in the session slot alongside the data-plane
        // RtspSession. `nIntoH264Receiver` takes both atomically.
        REGISTRY_SESSION.insert(JniRtspSession {
            client: Arc::new(Mutex::new(Some(client))),
            session: Arc::new(Mutex::new(Some(session))),
            torn_down: Arc::new(AtomicBool::new(false)),
            h264_depay_config: Arc::new(Mutex::new(Some(depay_config))),
        }) as jlong
    })
}

/// `RtspSession.nPause` — clone the client Arc, lock, `pause()`. Ports
/// `PyRtspSession::pause`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nPause(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        session_control(env, handle, |c| c.pause());
    })
}

/// `RtspSession.nPlay`. Ports `PyRtspSession::play`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nPlay(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        session_control(env, handle, |c| c.play().map(|_info| ()));
    })
}

/// `RtspSession.nTeardown` — no-op if already torn down, else `teardown()` and set
/// the flag. Ports `PyRtspSession::teardown`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nTeardown(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |env| {
        // Lease the session and clone the `client`/`torn_down` Arcs out. The registry
        // lease replaces the round-1 `&*ptr` deref (which raced `nClose`'s free);
        // `close` now atomically takes the resource so a fresh entry here either sees
        // it (Some) or finds it gone (None → IllegalStateException).
        let Some((client, torn)) =
            REGISTRY_SESSION.with(handle as u64, |s| (s.client.clone(), s.torn_down.clone()))
        else {
            crate::error::throw_closed(env, "RtspSession");
            return;
        };
        if torn.load(Ordering::Relaxed) {
            return;
        }
        let res = (|| -> Result<(), RtspError> {
            let mut guard = client.lock().map_err(|_| RtspError::SessionExpired)?;
            let r = match guard.as_mut() {
                Some(c) => c.teardown(),
                None => Ok(()),
            };
            torn.store(true, Ordering::Relaxed);
            r
        })();
        if let Err(e) = res {
            rtsp_error_to_jvm(env, &e);
        }
    })
}

/// Shared control-call helper for pause/play. Clones the client Arc, locks, and
/// runs `op`; maps any RtspError.
fn session_control(
    env: &mut JNIEnv,
    handle: jlong,
    op: impl FnOnce(&mut RustRtspClient) -> Result<(), RtspError>,
) {
    let Some(client) = REGISTRY_SESSION.with(handle as u64, |s| s.client.clone()) else {
        crate::error::throw_closed(env, "RtspSession");
        return;
    };
    let res = (|| -> Result<(), RtspError> {
        let mut guard = client.lock().map_err(|_| RtspError::SessionExpired)?;
        match guard.as_mut() {
            Some(c) => op(c),
            None => Err(RtspError::SessionExpired),
        }
    })();
    if let Err(e) = res {
        rtsp_error_to_jvm(env, &e);
    }
}

/// `RtspSession.nCancelHandle` — clone a self-contained cancel handle out of the
/// client. Returns 0 (no throw) when torn down (the Java side converts that to
/// IllegalStateException). Ports `PyRtspSession::cancel_handle`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nCancelHandle(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Lease the session and clone the `client` Arc out. `None` (absent/closed) →
        // 0 (the Java side converts that to IllegalStateException), matching the
        // torn-down → 0 contract.
        let Some(client) = REGISTRY_SESSION.with(handle as u64, |s| s.client.clone()) else {
            return 0;
        };
        let guard = match client.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        match guard.as_ref() {
            Some(c) => JniRtspCancel {
                inner: c.cancel_handle(),
            }
            .into_handle(),
            None => 0,
        }
    })
}

/// `RtspSession.nIntoDemuxReceiver` — take the SETUP-time RtspSession, convert to
/// an RtpRecvTransport, build a wave-B DemuxReceiver handle. Double-consume →
/// RtspException(PROTOCOL) + 0. Ports `PyRtspSession::into_demux_receiver`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nIntoDemuxReceiver(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    with_config: jboolean,
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
        // Lease the session and clone the data-plane `session` Arc out. `None`
        // (absent/closed) → IllegalStateException.
        let Some(session_slot) = REGISTRY_SESSION.with(handle as u64, |s| s.session.clone()) else {
            crate::error::throw_closed(env, "RtspSession");
            return 0;
        };
        // Take the data-plane RtspSession; double-consume = protocol error.
        let session = {
            let mut guard = match session_slot.lock() {
                Ok(g) => g,
                Err(_) => {
                    throw_rtsp(env, "PROTOCOL", "RtspSession lock poisoned");
                    return 0;
                }
            };
            match guard.take() {
                Some(sess) => sess,
                None => {
                    throw_rtsp(
                        env,
                        "PROTOCOL",
                        "RtspSession.intoDemuxReceiver: already consumed",
                    );
                    return 0;
                }
            }
        };
        let transport = session.into_recv_transport();
        let opts: Option<DemuxerConfig> = if with_config != 0 {
            let Some(cfg) = build_demux_config_from_args(
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
            Some(cfg)
        } else {
            None
        };
        demux_receiver_handle_from_transport(transport, opts)
    })
}

/// Best-effort TEARDOWN + `torn_down` latch for a session slot already taken
/// out of `REGISTRY_SESSION`. Errors swallowed (the tst-py `__exit__` contract:
/// "ensure closed", not "fail if the server is uncooperative"). Used by the
/// consuming `nIntoH264Receiver` failure paths.
fn teardown_best_effort(slot: &JniRtspSession) {
    if !slot.torn_down.load(Ordering::Relaxed) {
        if let Ok(mut guard) = slot.client.lock() {
            if let Some(c) = guard.as_mut() {
                let _ = c.teardown();
            }
        }
        slot.torn_down.store(true, Ordering::Relaxed);
    }
}

/// `RtspSession.nIntoH264Receiver` — CONSUMING native (the Java caller zeroed
/// its handle via `consumeHandle()` BEFORE this call — NativeHandle contract
/// item 3, same shape as srt `Socket.nIntoSender`). Takes the whole
/// `JniRtspSession` slot out of the registry (the Java wrapper will never call
/// `nClose` again, so a leased lookup here would leak the slot forever):
///
/// - success: converts the data plane into an `H264Receiver` and moves the
///   control plane (`RtspClient` + `torn_down` flag) into the receiver's slot
///   so the RTSP control connection + keepalive stay alive while AUs flow;
///   `H264Receiver.nClose` then performs the best-effort TEARDOWN.
/// - failure (plain-`nConnect` session / data plane already consumed) →
///   `RtspException(PROTOCOL)` and the session tears down best-effort — the
///   session is consumed either way (mirrors `Socket`'s "consumed even if the
///   config is rejected" semantics).
///
/// Ports `PyRtspSession::into_h264_receiver` (Python keeps its session object
/// alive instead; the consumption asymmetry is a deliberate JVM adjudication).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nIntoH264Receiver(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    crate::panic::jni_catch(&mut env, 0, |env| {
        // Step 1: take the session out of its registry (atomic; idempotent).
        // `None` = already closed/consumed → the Java caller zeroed its field,
        // so this is a stale call racing a concurrent close/consume.
        let Some(slot) = REGISTRY_SESSION.close(handle as u64) else {
            crate::error::throw_closed(env, "RtspSession");
            return 0;
        };

        // Step 2: take the H264DepayConfig. None = wrong path (plain connect) or
        // already consumed. The session is consumed even on failure: tear it down
        // best-effort before throwing (dropping `slot` then closes the control
        // connection).
        let depay_config = {
            let taken = match slot.h264_depay_config.lock() {
                Ok(mut g) => g.take(),
                Err(_) => {
                    throw_rtsp(env, "PROTOCOL", "RtspSession H264DepayConfig lock poisoned");
                    return 0;
                }
            };
            match taken {
                Some(cfg) => cfg,
                None => {
                    teardown_best_effort(&slot);
                    throw_rtsp(
                        env,
                        "PROTOCOL",
                        "RtspSession.intoH264Receiver: session was not created by \
                         connectH264(), or the H264DepayConfig has already been consumed",
                    );
                    return 0;
                }
            }
        };

        // Step 3: take the data-plane RtspSession. Double-consume = protocol error.
        // This is zeroed BEFORE the fallible `into_h264_receiver` call
        // (double-free lesson — mirrors Python's `guard.take()` before conversion).
        let session = {
            let taken = match slot.session.lock() {
                Ok(mut g) => g.take(),
                Err(_) => {
                    throw_rtsp(env, "PROTOCOL", "RtspSession data-plane lock poisoned");
                    return 0;
                }
            };
            match taken {
                Some(sess) => sess,
                None => {
                    teardown_best_effort(&slot);
                    throw_rtsp(
                        env,
                        "PROTOCOL",
                        "RtspSession.intoH264Receiver: data plane already consumed",
                    );
                    return 0;
                }
            }
        };

        // Step 4: convert — infallible on valid SETUP-succeeded paths.
        let receiver = session.into_h264_receiver(depay_config);

        // Step 5: register, moving the control plane into the receiver's slot so
        // the control connection + keepalive outlive this (consumed) session.
        super::h264_receiver::h264_receiver_handle_from_rtsp_session(
            receiver,
            super::h264_receiver::JniRtspControl {
                client: slot.client,
                torn_down: slot.torn_down,
            },
        )
    })
}

/// `RtspSession.nIsTornDown`. Ports `PyRtspSession::is_torn_down`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nIsTornDown(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    crate::panic::jni_catch(&mut env, 0, |_env| {
        // Absent/closed reads as torn down (1). A live session reads its atomic flag.
        REGISTRY_SESSION
            .with(handle as u64, |s| {
                u8::from(s.torn_down.load(Ordering::Relaxed))
            })
            .unwrap_or(1)
    })
}

/// `RtspSession.nClose` — best-effort teardown (swallow errors, like tst-py's
/// `__exit__`), then free the box.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_rtp_RtspSession_nClose(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        // Atomic + idempotent: only the winning close gets the session back for
        // best-effort teardown (errors swallowed, like tst-py's `__exit__`). A second
        // close finds the id gone → no-op. No registry cancel hook (the cross-thread
        // stop routes through RtspCancelHandle, not close).
        if let Some(b) = REGISTRY_SESSION.close(handle as u64) {
            if !b.torn_down.load(Ordering::Relaxed) {
                if let Ok(mut guard) = b.client.lock() {
                    if let Some(c) = guard.as_mut() {
                        let _ = c.teardown(); // best-effort
                    }
                }
                b.torn_down.store(true, Ordering::Relaxed);
            }
            drop(b);
        }
    })
}
