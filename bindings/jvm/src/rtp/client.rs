//! `org.tstrans.rtp` RTSP client JNI surface — `RtspClient`, `RtspSession`,
//! `RtspCancelHandle`, and the auth/config/stats value types' native backing.
//! Ports tst-py's `bindings/python/src/rtp/client.rs`, including
//! `nConnectH264` (RtspClient) + `nIntoH264Receiver` (RtspSession).

use crate::jutil::FindClass;
use crate::panic::jni_native;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

use jni::Env;
use jni::jni_str;
use jni::objects::{JByteArray, JString};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong};

use secrecy::SecretString;
use tst_core::mpegts::demux::DemuxerConfig;
use tst_pipeline::binding::BindingErrorKind;
use tst_rtp::AuthSchemes;
use tst_rtp::H264DepayConfig;
use tst_rtp::RtspClientBuilder;
use tst_rtp::error::RtspError;
use tst_rtp::rtsp::client::RtspCancelHandle as RustRtspCancel;
use tst_rtp::rtsp::client::RtspClient as RustRtspClient;
use tst_rtp::rtsp::client::session::RtspSession as RustRtspSession;

use crate::handle::{HandleRegistry, TryWith};
use crate::mpegts::build_demux_config_from_args;

use super::demux_receiver_handle_from_transport;
use super::errors::{rtsp_error_to_jvm, throw_rtsp};

/// Parse `RtspClientConfig.tlsRootCertsPem` (a PEM bundle) into a
/// `rustls::RootCertStore` and hand it to the builder — `rtsps://`
/// verification against a private CA. Port of tst-py's `apply_tls_roots`
/// (bindings/python/src/rtp/client.rs) with identical error semantics:
/// invalid PEM / rejected anchor / zero certs all throw kind TLS. A null
/// Java array is a no-op (platform native trust roots). Returns None with
/// a pending exception on failure.
fn apply_tls_roots(
    env: &mut Env,
    builder: RtspClientBuilder,
    pem: &JByteArray<'_>,
) -> Option<RtspClientBuilder> {
    if pem.is_null() {
        return Some(builder);
    }
    let bytes = match env.convert_byte_array(pem) {
        Ok(b) => b,
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(e.to_string()),
            );
            return None;
        }
    };
    use tst_core::net::tls::TlsLoadError as E;
    match tst_core::net::tls::load_root_store(&bytes) {
        Ok(roots) => Some(builder.tls_root_certs(roots)),
        Err(e) => {
            let detail = match e {
                E::Pem { source, .. } => format!("tlsRootCertsPem: invalid PEM: {source}"),
                E::NoCertificates { .. } => "tlsRootCertsPem contains no certificates".to_string(),
                E::TrustAnchor(err) => {
                    format!("tlsRootCertsPem: certificate rejected as trust anchor: {err}")
                }
                other => format!("tlsRootCertsPem: {other}"),
            };
            throw_rtsp(env, BindingErrorKind::RtspTls, &detail);
            None
        }
    }
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
/// cancel hook: `nClose` fires the session's stored cancel itself, and only when
/// a control call holds the client (see [`close_client`]).
static REGISTRY_SESSION: LazyLock<HandleRegistry<JniRtspSession>> =
    LazyLock::new(HandleRegistry::new);

impl JniRtspCancel {
    pub(super) fn into_handle(self) -> jlong {
        REGISTRY_CANCEL.insert(self) as jlong
    }
}

jni_native! {
    /// Flip the cancel flag. Wakes a parked pause/play/teardown at the next
    /// ~100 ms poll. Guards a closed (zero) handle.
    fn Java_org_tstrans_rtp_RtspCancelHandle_nCancel(env, _class, handle: jlong) = () => {
        if REGISTRY_CANCEL
            .with(handle as u64, |c| c.inner.cancel())
            .is_none()
        {
            crate::error::throw_closed(env, "RtspCancelHandle");
        }
    }
}

jni_native! {
    /// Report whether the backing flag has been flipped. Guards a closed handle.
    fn Java_org_tstrans_rtp_RtspCancelHandle_nIsCancelled(
        env,
        _class,
        handle: jlong,
    ) -> jboolean = false => {
        match REGISTRY_CANCEL.try_with(handle as u64, |c| c.inner.is_cancelled()) {
            TryWith::Ran(v) => v,
            _ => {
                crate::error::throw_closed(env, "RtspCancelHandle");
                false
            }
        }
    }
}

jni_native! {
    /// Free the boxed cancel handle.
    fn Java_org_tstrans_rtp_RtspCancelHandle_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent drop.
        let _ = REGISTRY_CANCEL.close(handle as u64);
    }
}

/// Native backing for `org.tstrans.rtp.RtspSession`. Faithful port of tst-py's
/// `PyRtspSession`: `client` + `session` behind `Arc<Mutex<Option<..>>>` (so the
/// control natives clone the Arc and lock, never holding a `&mut` to the box);
/// `torn_down` so a duplicate teardown is a no-op. The client is `None` once a
/// close has taken it.
///
/// `h264_depay_config` is stashed only when created via `nConnectH264` (the H.264
/// path — it carries the SDP-negotiated payload type and out-of-band SPS/PPS).
/// `nIntoH264Receiver` takes it + the data-plane session together; `None` when
/// the session was created via plain `nConnect` (MP2T path) or after consumption.
struct JniRtspSession {
    client: Arc<Mutex<Option<RustRtspClient>>>,
    /// A clone of the client's cancel, taken at connect and kept outside the
    /// client mutex, which a parked control call holds for up to the request
    /// timeout: `nCancelHandle` and the close read it without that wait.
    rtsp_cancel: RustRtspCancel,
    session: Arc<Mutex<Option<RustRtspSession>>>,
    torn_down: Arc<AtomicBool>,
    /// `Some(_)` only for `nConnectH264`-created sessions. `None` for
    /// plain `nConnect` sessions and after `nIntoH264Receiver` has consumed it.
    h264_depay_config: Arc<Mutex<Option<H264DepayConfig>>>,
}

jni_native! {
    /// `RtspClient.nConnect(url, authUser, authPassword, digestOnly, keepalive, tlsRootCertsPem)`
    /// — run OPTIONS/DESCRIBE/SETUP/PLAY, return a `Box<JniRtspSession>` handle, or 0
    /// with a pending `RtspException` on failure. Ports `PyRtspClient::connect`.
    fn Java_org_tstrans_rtp_RtspClient_nConnect(
        env,
        _class,
        url: JString<'_>,
        auth_user: JString<'_>,
        auth_password: JString<'_>,
        digest_only: jboolean,
        keepalive: jboolean,
        tls_root_certs_pem: JByteArray<'_>,
    ) -> jlong = 0 => {
        let url_str: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return 0;
            }
        };

        // 1. Builder construction (URL parse) — the only eager typed RtspError.
        let mut builder = match RtspClientBuilder::new(&url_str) {
            Ok(b) => b,
            Err(e) => {
                rtsp_error_to_jvm(env, e);
                return 0;
            }
        };

        // 2. Wire credentials when present. SecretString wrapping happens here.
        //    A null Java String arrives as a null JString (is_null()). The algorithm
        //    is NOT passed: tst-rtp's challenge handler picks it from the server's
        //    WWW-Authenticate header (matches tst-py).
        if !auth_user.is_null() {
            let user: String = match auth_user.try_to_string(env) {
                Ok(s) => s,
                Err(e) => {
                    let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                    return 0;
                }
            };
            let pw: String = if auth_password.is_null() {
                String::new()
            } else {
                match auth_password.try_to_string(env) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                        return 0;
                    }
                }
            };
            builder = builder.auth(user, SecretString::from(pw));
            if digest_only {
                builder = builder.auth_schemes(AuthSchemes::DigestOnly);
            }
        }

        // 3. keepalive=false disables the automatic keepalive pings.
        if !keepalive {
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
                rtsp_error_to_jvm(env, e);
                return 0;
            }
        };

        REGISTRY_SESSION.insert(JniRtspSession {
            rtsp_cancel: client.cancel_handle(),
            client: Arc::new(Mutex::new(Some(client))),
            session: Arc::new(Mutex::new(Some(session))),
            torn_down: Arc::new(AtomicBool::new(false)),
            h264_depay_config: Arc::new(Mutex::new(None)), // MP2T path — no H.264 config
        }) as jlong
    }
}

jni_native! {
    /// `RtspClient.nConnectH264(url, authUser, authPassword, digestOnly, keepalive, tlsRootCertsPem)`
    /// — run OPTIONS/DESCRIBE/SETUP for H.264 media/PLAY; return a `JniRtspSession`
    /// handle with the session slot holding an `H264DepayConfig`. Ports
    /// `PyRtspClient::connect_h264`.
    fn Java_org_tstrans_rtp_RtspClient_nConnectH264(
        env,
        _class,
        url: JString<'_>,
        auth_user: JString<'_>,
        auth_password: JString<'_>,
        digest_only: jboolean,
        keepalive: jboolean,
        tls_root_certs_pem: JByteArray<'_>,
    ) -> jlong = 0 => {
        let url_str: String = match url.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return 0;
            }
        };

        let mut builder = match RtspClientBuilder::new(&url_str) {
            Ok(b) => b,
            Err(e) => {
                rtsp_error_to_jvm(env, e);
                return 0;
            }
        };

        if !auth_user.is_null() {
            let user: String = match auth_user.try_to_string(env) {
                Ok(s) => s,
                Err(e) => {
                    let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                    return 0;
                }
            };
            let pw: String = if auth_password.is_null() {
                String::new()
            } else {
                match auth_password.try_to_string(env) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                        return 0;
                    }
                }
            };
            builder = builder.auth(user, SecretString::from(pw));
            if digest_only {
                builder = builder.auth_schemes(AuthSchemes::DigestOnly);
            }
        }

        if !keepalive {
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
                rtsp_error_to_jvm(env, e);
                return 0;
            }
        };

        // Stash the H264DepayConfig in the session slot alongside the data-plane
        // RtspSession. `nIntoH264Receiver` takes both atomically.
        REGISTRY_SESSION.insert(JniRtspSession {
            rtsp_cancel: client.cancel_handle(),
            client: Arc::new(Mutex::new(Some(client))),
            session: Arc::new(Mutex::new(Some(session))),
            torn_down: Arc::new(AtomicBool::new(false)),
            h264_depay_config: Arc::new(Mutex::new(Some(depay_config))),
        }) as jlong
    }
}

jni_native! {
    /// `RtspSession.nPause` — clone the client Arc, lock, `pause()`. Ports
    /// `PyRtspSession::pause`.
    fn Java_org_tstrans_rtp_RtspSession_nPause(env, _class, handle: jlong) = () => {
        session_control(env, handle, |c| c.pause());
    }
}

jni_native! {
    /// `RtspSession.nPlay`. Ports `PyRtspSession::play`.
    fn Java_org_tstrans_rtp_RtspSession_nPlay(env, _class, handle: jlong) = () => {
        session_control(env, handle, |c| c.play().map(|_info| ()));
    }
}

jni_native! {
    /// `RtspSession.nTeardown` — no-op if already torn down, else `teardown()` (the
    /// request timeout bounds the reply wait) and set the flag; the client is kept
    /// for the close. Ports `PyRtspSession::teardown`.
    fn Java_org_tstrans_rtp_RtspSession_nTeardown(env, _class, handle: jlong) = () => {
        // Lease the session and clone the `client`/`torn_down` Arcs out. The registry
        // lease keeps a racing close from freeing the client during the call;
        // `close` atomically takes the resource so a fresh entry here either sees
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
                // A concurrent close took and ended the client: the session is torn
                // down, so this returns normally (pause/play throw CLOSED there; a
                // teardown has nothing left to do, and the close's result is silent).
                None => Ok(()),
            };
            torn.store(true, Ordering::Relaxed);
            r
        })();
        if let Err(e) = res {
            rtsp_error_to_jvm(env, e);
        }
    }
}

/// Shared control-call helper for pause/play. Clones the client Arc, locks, and
/// runs `op`; maps any RtspError. A client a concurrent close took while this
/// call waited for the lock is `CLOSED`, the kind the close gives a call it
/// cancels.
fn session_control(
    env: &mut Env,
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
            None => Err(RtspError::LocalCancel),
        }
    })();
    if let Err(e) = res {
        rtsp_error_to_jvm(env, e);
    }
}

jni_native! {
    /// `RtspSession.nCancelHandle` — a handle on the session's stored cancel, read
    /// under the registry lease only, so it answers while a control call holds the
    /// client. A handle after `teardown()` too; 0 (the Java side throws
    /// IllegalStateException) only for a closed session. Ports
    /// `PyRtspSession::cancel_handle`.
    fn Java_org_tstrans_rtp_RtspSession_nCancelHandle(_env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY_SESSION.with(handle as u64, |s| s.rtsp_cancel.clone()) {
            Some(inner) => JniRtspCancel { inner }.into_handle(),
            None => 0,
        }
    }
}

jni_native! {
    /// `RtspSession.nIntoDemuxReceiver` — take the SETUP-time RtspSession, convert to
    /// an RtpRecvTransport, build a DemuxReceiver handle. Double-consume →
    /// RtspException(CLOSED) + 0. Ports `PyRtspSession::into_demux_receiver`.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_rtp_RtspSession_nIntoDemuxReceiver(
        env,
        _class,
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
    ) -> jlong = 0 => {
        // Lease the session and clone the data-plane `session` Arc out. `None`
        // (absent/closed) → IllegalStateException.
        let Some(session_slot) = REGISTRY_SESSION.with(handle as u64, |s| s.session.clone()) else {
            crate::error::throw_closed(env, "RtspSession");
            return 0;
        };
        // Take the data-plane RtspSession; double-consume = CLOSED.
        let session = {
            let mut guard = match session_slot.lock() {
                Ok(g) => g,
                Err(_) => {
                    throw_rtsp(
                        env,
                        BindingErrorKind::RtspProtocol,
                        "RtspSession lock poisoned",
                    );
                    return 0;
                }
            };
            match guard.take() {
                Some(sess) => sess,
                None => {
                    throw_rtsp(
                        env,
                        BindingErrorKind::Closed,
                        "RtspSession.intoDemuxReceiver: already consumed",
                    );
                    return 0;
                }
            }
        };
        let transport = session.into_recv_transport();
        let opts: Option<DemuxerConfig> = if with_config {
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
        match demux_receiver_handle_from_transport(transport, opts) {
            Ok(h) => h,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.detail));
                0
            }
        }
    }
}

/// End a session's client with `RtspClient::close()`: TEARDOWN when a session is
/// open, under one 500 ms bound with the reply awaited, then up to 2 s for the
/// client's thread, which closes the control connection. Runs whether or not
/// `teardown()` ran before (then it writes nothing and only waits for the
/// thread). The result is discarded: every close is silent, `teardown()` is the
/// call that reports. The client is taken out of its mutex and closed outside it.
///
/// A control call parked on another thread holds the mutex for up to the request
/// timeout: the stored cancel fires first (a call waiting for its reply ends
/// `CLOSED` at its next ~100 ms poll), then the close runs on the cancelled client,
/// so its TEARDOWN's reply is not awaited. An idle close does not cancel first, so an accepted
/// TEARDOWN still ends a live interleaved receiver with end of stream.
pub(super) fn close_client(
    client: &Mutex<Option<RustRtspClient>>,
    cancel: &RustRtspCancel,
    torn_down: &AtomicBool,
) {
    let taken = match client.try_lock() {
        Ok(mut guard) => guard.take(),
        Err(TryLockError::WouldBlock) => {
            cancel.cancel();
            client.lock().unwrap_or_else(PoisonError::into_inner).take()
        }
        Err(TryLockError::Poisoned(p)) => p.into_inner().take(),
    };
    if let Some(c) = taken {
        let _ = c.close();
    }
    torn_down.store(true, Ordering::Relaxed);
}

jni_native! {
    /// `RtspSession.nIntoH264Receiver` — CONSUMING native (the Java caller zeroed
    /// its handle via `consumeHandle()` BEFORE this call — NativeHandle contract
    /// item 3, same shape as srt `Socket.nIntoSender`). Takes the whole
    /// `JniRtspSession` slot out of the registry (the Java wrapper will never call
    /// `nClose` again, so a leased lookup here would leak the slot forever):
    ///
    /// - success: converts the data plane into an `H264Receiver` and moves the
    ///   control plane (`RtspClient` + `torn_down` flag) into the receiver's slot
    ///   so the RTSP control connection + keepalive stay alive while AUs flow;
    ///   `H264Receiver.nClose` then closes the client ([`close_client`]).
    /// - failure → the client is closed ([`close_client`]) and the session is
    ///   consumed either way
    ///   (mirrors `Socket`'s "consumed even if the config is rejected"
    ///   semantics): a data plane already consumed by `nIntoDemuxReceiver` is
    ///   `RtspException(CLOSED)` — checked FIRST, so it is CLOSED on both session
    ///   types — and a fresh plain-`nConnect` session is
    ///   `RtspException(PROTOCOL)`.
    ///
    /// Ports `PyRtspSession::into_h264_receiver` (Python keeps its session object
    /// alive instead; the consumption asymmetry is a deliberate JVM adjudication).
    fn Java_org_tstrans_rtp_RtspSession_nIntoH264Receiver(
        env,
        _class,
        handle: jlong,
    ) -> jlong = 0 => {
        // Step 1: take the session out of its registry (atomic; idempotent).
        // `None` = already closed/consumed → the Java caller zeroed its field,
        // so this is a stale call racing a concurrent close/consume.
        let Some(slot) = REGISTRY_SESSION.close(handle as u64) else {
            crate::error::throw_closed(env, "RtspSession");
            return 0;
        };

        // Step 2: take the data-plane RtspSession. None = already consumed by
        // `nIntoDemuxReceiver` → CLOSED. Checked BEFORE the H.264 config so a
        // consumed MP2T session is not misreported as PROTOCOL.
        // Taken here, before the fallible `into_h264_receiver` call
        // (prevents a double free — mirrors Python's `guard.take()` before
        // conversion); the whole slot is consumed on every outcome anyway.
        let session = {
            let taken = match slot.session.lock() {
                Ok(mut g) => g.take(),
                Err(_) => {
                    throw_rtsp(
                        env,
                        BindingErrorKind::RtspProtocol,
                        "RtspSession data-plane lock poisoned",
                    );
                    return 0;
                }
            };
            match taken {
                Some(sess) => sess,
                None => {
                    close_client(&slot.client, &slot.rtsp_cancel, &slot.torn_down);
                    throw_rtsp(
                        env,
                        BindingErrorKind::Closed,
                        "RtspSession.intoH264Receiver: data plane already consumed",
                    );
                    return 0;
                }
            }
        };

        // Step 3: take the H264DepayConfig. None = wrong path (plain connect).
        // The session is consumed even on failure: close its client before
        // throwing.
        let depay_config = {
            let taken = match slot.h264_depay_config.lock() {
                Ok(mut g) => g.take(),
                Err(_) => {
                    throw_rtsp(
                        env,
                        BindingErrorKind::RtspProtocol,
                        "RtspSession H264DepayConfig lock poisoned",
                    );
                    return 0;
                }
            };
            match taken {
                Some(cfg) => cfg,
                None => {
                    close_client(&slot.client, &slot.rtsp_cancel, &slot.torn_down);
                    throw_rtsp(
                        env,
                        BindingErrorKind::RtspProtocol,
                        "RtspSession.intoH264Receiver: session was not created by \
                         connectH264()",
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
                rtsp_cancel: slot.rtsp_cancel,
                torn_down: slot.torn_down,
            },
        )
    }
}

jni_native! {
    /// `RtspSession.nIsTornDown`. Ports `PyRtspSession::is_torn_down`.
    fn Java_org_tstrans_rtp_RtspSession_nIsTornDown(
        _env,
        _class,
        handle: jlong,
    ) -> jboolean = false => {
        // Absent/closed reads as torn down (true). A live session reads its atomic flag.
        REGISTRY_SESSION
            .with(handle as u64, |s| {
                s.torn_down.load(Ordering::Relaxed)
            })
            .unwrap_or(true)
    }
}

jni_native! {
    /// `RtspSession.nClose` — close the client ([`close_client`]: silent, bounded),
    /// then free the box.
    fn Java_org_tstrans_rtp_RtspSession_nClose(_env, _class, handle: jlong) = () => {
        // Atomic + idempotent: only the winning close gets the session back; a
        // second close finds the id gone → no-op.
        if let Some(b) = REGISTRY_SESSION.close(handle as u64) {
            close_client(&b.client, &b.rtsp_cancel, &b.torn_down);
        }
    }
}
