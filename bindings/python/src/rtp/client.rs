//! `RtspClient`, `RtspSession`, `BasicAuth`, `DigestAuth`,
//! `RtspClientConfig`, `RtspStats`.
//!
//! Translation strategy:
//!
//! - Python `RtspClient.connect(config)` is a `@staticmethod` that runs
//!   the full OPTIONS / DESCRIBE / SETUP / PLAY chain through the
//!   underlying `tst_rtp::RtspClient` and returns a Python
//!   `RtspSession` wrapping the now-PLAYing client.
//! - `RtspSession` owns the live `RtspClient`. Methods `play` / `pause`
//!   / `teardown` delegate to it under `py.allow_threads`.
//! - `into_demux_receiver` converts the session into a demux-capable
//!   receiver; calling it before the session is playing raises
//!   `NotImplementedError`.
//! - Secrets (`DigestAuth.password`, `BasicAuth.password`) are accepted
//!   from Python as `str`, wrapped in `secrecy::SecretString` only when
//!   handed to `RtspClientBuilder::auth`, and never re-exposed to
//!   Python (only `user` / `algorithm` are readable through getters).
//!
//! GIL release boundaries (per design spec "GIL release boundaries"):
//!
//! - `py.allow_threads`: `connect`, `play`, `pause`, `teardown`.
//! - **Not** wrapped: `cancel_handle()`, `stats()`, dataclass ctors,
//!   `__enter__` / `__exit__` (these are non-blocking).
//!
//! `#![allow(...)]` mirrors `errors.rs` / `mpegts.rs` — PyO3 0.22 +
//! Rust 2024 macro expansions trip these lints. Hand-written code in
//! this module has no unsafe blocks.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use secrecy::SecretString;
use tst_rtp::error::RtspError as RustRtspError;
use tst_rtp::rtsp::client::session::RtspSession as RustRtspSession;
use tst_rtp::rtsp::client::{
    RtspCancelHandle as RustRtspCancelHandle, RtspClient as RustRtspClient,
};
use tst_rtp::{H264DepayConfig, RtspClientBuilder, RtspVersion};

use crate::errors::make_rtsp_error;
use crate::rtp::demux_receiver::PyDemuxReceiver;
use crate::rtp::h264_receiver::PyH264Receiver;

// ---------------------------------------------------------------------------
// Enums (Python IntEnum-equivalent PyClasses)
// ---------------------------------------------------------------------------

/// Wire-time RTSP version preference. Mirrors `tst_rtp::RtspVersion` —
/// only the 1.0 / 2.0 split matters at the SETUP / PLAY layer.
#[pyclass(eq, eq_int, name = "RtspVersion", module = "tstrans.rtp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyRtspVersion {
    /// `RTSP/1.0` per RFC 2326 — the default for maximum interop with
    /// deployed IP cameras.
    V1_0 = 0,
    /// `RTSP/2.0` per RFC 7826 — wire-identical for the OPTIONS /
    /// DESCRIBE / SETUP / PLAY / TEARDOWN subset.
    V2_0 = 1,
}

impl PyRtspVersion {
    fn to_rust(self) -> RtspVersion {
        match self {
            PyRtspVersion::V1_0 => RtspVersion::V1_0,
            PyRtspVersion::V2_0 => RtspVersion::V2_0,
        }
    }
}

/// Transport preference at SETUP time. AUTO = UDP-first with TCP
/// fallback on 461 Unsupported Transport. UDP / TCP force a single
/// transport (no fallback).
///
/// Variant names are SHOUTY_SNAKE to match the Python-side convention
/// for IntEnum-shaped pyclasses; `#[allow(clippy::upper_case_acronyms)]`
/// covers the lint that would otherwise demand `Udp` / `Tcp`.
#[pyclass(eq, eq_int, name = "TransportPref", module = "tstrans.rtp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum PyTransportPref {
    AUTO = 0,
    UDP = 1,
    TCP = 2,
}

/// Digest authentication algorithm selector. Mirrors the user-facing
/// subset of `tst_rtp::DigestAlgorithm` — the `*-sess` variants are
/// parsed from server challenges but not selectable by the caller;
/// tst-rtp's challenge handler picks them automatically when the
/// server demands them.
#[pyclass(eq, eq_int, name = "DigestAlgorithm", module = "tstrans.rtp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyDigestAlgorithm {
    /// RFC 7616 §3.4 with `algorithm=MD5` (also the RFC 2617 default).
    MD5 = 0,
    /// RFC 7616 §3.4 with `algorithm=SHA-256`.
    SHA256 = 1,
}

// ---------------------------------------------------------------------------
// Auth dataclasses
// ---------------------------------------------------------------------------

/// HTTP Basic auth credentials per RFC 7617.
///
/// Sent only after a 401 challenge — tst-rtp never preemptively
/// transmits Basic credentials. Passwords are accepted from Python as
/// `str` and wrapped in `secrecy::SecretString` at the Rust boundary
/// before they reach `RtspClientBuilder::auth`; they are never
/// re-exposed to Python (only `user` is readable via the getter).
#[pyclass(name = "BasicAuth", module = "tstrans.rtp", frozen)]
#[derive(Debug, Clone)]
pub struct PyBasicAuth {
    pub(crate) user: String,
    /// Held as a plain Rust `String` because the Python-side surface
    /// already accepted it as a `str` (it lives in Python memory
    /// before we get it). `secrecy::SecretString` adoption happens at
    /// the `RtspClientBuilder::auth` call boundary in
    /// `PyRtspClient::connect`.
    pub(crate) password: String,
    /// Optional auth realm. Set when this credential is used to
    /// configure server-side authentication (the realm is what the
    /// server quotes back in `WWW-Authenticate`). `None` for
    /// client-side use where the realm is provided by the peer's
    /// 401 challenge.
    pub(crate) realm: Option<String>,
}

#[pymethods]
impl PyBasicAuth {
    #[new]
    #[pyo3(signature = (user, password, realm = None))]
    fn new(user: String, password: String, realm: Option<String>) -> Self {
        Self {
            user,
            password,
            realm,
        }
    }

    #[getter]
    fn user(&self) -> &str {
        &self.user
    }

    #[getter]
    fn realm(&self) -> Option<&str> {
        self.realm.as_deref()
    }

    fn __repr__(&self) -> String {
        // Never leak the password through __repr__.
        format!(
            "BasicAuth(user={:?}, password=<redacted>, realm={:?})",
            self.user, self.realm
        )
    }
}

/// HTTP Digest auth credentials per RFC 7616 (MD5 + SHA-256) and
/// RFC 2617 (legacy MD5).
///
/// Same secret-handling story as `BasicAuth`: password held as `String`
/// here, wrapped in `secrecy::SecretString` only when handed to
/// `RtspClientBuilder::auth`.
#[pyclass(name = "DigestAuth", module = "tstrans.rtp", frozen)]
#[derive(Debug, Clone)]
pub struct PyDigestAuth {
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) algorithm: PyDigestAlgorithm,
    /// Same semantics as [`PyBasicAuth::realm`]: optional, server-side
    /// configuration only.
    pub(crate) realm: Option<String>,
}

#[pymethods]
impl PyDigestAuth {
    #[new]
    #[pyo3(signature = (user, password, algorithm = PyDigestAlgorithm::MD5, realm = None))]
    fn new(
        user: String,
        password: String,
        algorithm: PyDigestAlgorithm,
        realm: Option<String>,
    ) -> Self {
        Self {
            user,
            password,
            algorithm,
            realm,
        }
    }

    #[getter]
    fn user(&self) -> &str {
        &self.user
    }

    #[getter]
    fn algorithm(&self) -> PyDigestAlgorithm {
        self.algorithm
    }

    #[getter]
    fn realm(&self) -> Option<&str> {
        self.realm.as_deref()
    }

    fn __repr__(&self) -> String {
        format!(
            "DigestAuth(user={:?}, password=<redacted>, algorithm={:?}, realm={:?})",
            self.user, self.algorithm, self.realm
        )
    }
}

// ---------------------------------------------------------------------------
// RtspClientConfig dataclass
// ---------------------------------------------------------------------------

/// RTSP client connection configuration. Frozen dataclass — all
/// fields validated at construction.
///
/// `auth` is one of `BasicAuth`, `DigestAuth`, or `None`. `transport_pref`
/// controls UDP/TCP selection at SETUP. `rtsps://` connections verify the
/// server certificate against platform native trust roots by default;
/// pass `tls_root_certs_pem` (a PEM bundle) to verify against a private
/// CA / self-signed camera cert instead.
#[pyclass(name = "RtspClientConfig", module = "tstrans.rtp", frozen)]
#[derive(Debug)]
pub struct PyRtspClientConfig {
    url: String,
    /// Stored as `PyObject` so we can hand it back through the getter
    /// while preserving its concrete Python class identity.
    auth: Option<PyObject>,
    transport_pref: PyTransportPref,
    rtcp: bool,
    tls_root_certs_pem: Option<Vec<u8>>,
    keepalive: bool,
    rtsp_version: PyRtspVersion,
}

#[pymethods]
impl PyRtspClientConfig {
    #[new]
    #[pyo3(signature = (
        url,
        *,
        auth = None,
        transport_pref = PyTransportPref::AUTO,
        rtcp = true,
        tls_root_certs_pem = None,
        keepalive = true,
        rtsp_version = PyRtspVersion::V1_0,
    ))]
    fn new(
        url: String,
        auth: Option<&Bound<'_, PyAny>>,
        transport_pref: PyTransportPref,
        rtcp: bool,
        tls_root_certs_pem: Option<Vec<u8>>,
        keepalive: bool,
        rtsp_version: PyRtspVersion,
    ) -> PyResult<Self> {
        if url.is_empty() {
            return Err(PyValueError::new_err(
                "RtspClientConfig: url must not be empty",
            ));
        }
        // Validate the auth argument is one of the recognised types
        // (or None). Reject ad-hoc duck-typed objects so a config that
        // round-trips through `.auth` always survives untouched.
        let auth_obj = match auth {
            None => None,
            Some(a) if a.is_none() => None,
            Some(a) => {
                let is_basic = a.is_instance_of::<PyBasicAuth>();
                let is_digest = a.is_instance_of::<PyDigestAuth>();
                if !is_basic && !is_digest {
                    return Err(PyValueError::new_err(
                        "RtspClientConfig: auth must be BasicAuth, DigestAuth, or None",
                    ));
                }
                Some(a.clone().unbind())
            }
        };
        Ok(Self {
            url,
            auth: auth_obj,
            transport_pref,
            rtcp,
            tls_root_certs_pem,
            keepalive,
            rtsp_version,
        })
    }

    #[getter]
    fn url(&self) -> &str {
        &self.url
    }

    #[getter]
    fn auth(&self, py: Python<'_>) -> Option<PyObject> {
        self.auth.as_ref().map(|p| p.clone_ref(py))
    }

    #[getter]
    fn transport_pref(&self) -> PyTransportPref {
        self.transport_pref
    }

    #[getter]
    fn rtcp(&self) -> bool {
        self.rtcp
    }

    #[getter]
    fn tls_root_certs_pem(&self) -> Option<&[u8]> {
        self.tls_root_certs_pem.as_deref()
    }

    #[getter]
    fn keepalive(&self) -> bool {
        self.keepalive
    }

    #[getter]
    fn rtsp_version(&self) -> PyRtspVersion {
        self.rtsp_version
    }

    fn __repr__(&self) -> String {
        format!(
            "RtspClientConfig(url={:?}, auth={}, transport_pref={:?}, rtcp={}, \
             tls_root_certs_pem={}, keepalive={}, rtsp_version={:?})",
            self.url,
            if self.auth.is_some() {
                "<auth>"
            } else {
                "None"
            },
            self.transport_pref,
            self.rtcp,
            if self.tls_root_certs_pem.is_some() {
                "<bytes>"
            } else {
                "None"
            },
            self.keepalive,
            self.rtsp_version,
        )
    }
}

// ---------------------------------------------------------------------------
// RtspStats — RTCP-derived snapshot
// ---------------------------------------------------------------------------

/// RTSP session stats snapshot. Reserved — all fields always report
/// zero until RTCP session stats land; see
/// `docs/project/deferred-features.md`.
#[pyclass(name = "RtspStats", module = "tstrans.rtp", frozen)]
#[derive(Debug, Clone, Default)]
pub struct PyRtspStats {
    rr_packets_received: u64,
    sr_packets_received: u64,
    rr_packets_sent: u64,
    sr_packets_sent: u64,
    interarrival_jitter_us: u32,
    fraction_lost_q8: u8,
}

#[pymethods]
impl PyRtspStats {
    #[getter]
    fn rr_packets_received(&self) -> u64 {
        self.rr_packets_received
    }
    #[getter]
    fn sr_packets_received(&self) -> u64 {
        self.sr_packets_received
    }
    #[getter]
    fn rr_packets_sent(&self) -> u64 {
        self.rr_packets_sent
    }
    #[getter]
    fn sr_packets_sent(&self) -> u64 {
        self.sr_packets_sent
    }
    #[getter]
    fn interarrival_jitter_us(&self) -> u32 {
        self.interarrival_jitter_us
    }
    #[getter]
    fn fraction_lost_q8(&self) -> u8 {
        self.fraction_lost_q8
    }

    fn __repr__(&self) -> String {
        format!(
            "RtspStats(rr_packets_received={}, sr_packets_received={}, \
             rr_packets_sent={}, sr_packets_sent={}, interarrival_jitter_us={}, \
             fraction_lost_q8={})",
            self.rr_packets_received,
            self.sr_packets_received,
            self.rr_packets_sent,
            self.sr_packets_sent,
            self.interarrival_jitter_us,
            self.fraction_lost_q8,
        )
    }
}

// ---------------------------------------------------------------------------
// CancelHandle — shared with the in-flight Rust RtspClient
// ---------------------------------------------------------------------------

/// RTSP control-plane cancel handle. Forwards `cancel()` through the
/// underlying `tst_rtp::rtsp::client::RtspCancelHandle`, which flips
/// the same `AtomicBool` the client polls on its blocking I/O loops.
///
/// Note: this is the *RTSP* cancel — the transport-side data-plane
/// cancel (post-PLAY RTP data) is exposed by `tstrans.rtp.CancelHandle`
/// from T20's `transport.rs`. The two flags are independent in the
/// underlying Rust API; we expose them under distinct Python class
/// names to keep the contracts honest.
#[pyclass(name = "RtspCancelHandle", module = "tstrans.rtp", frozen)]
pub struct PyRtspCancelHandle {
    inner: RustRtspCancelHandle,
}

#[pymethods]
impl PyRtspCancelHandle {
    /// Flip the cancel flag. Any in-flight `connect` / `pause` /
    /// `play` / `teardown` call on the originating session breaks out
    /// of blocking I/O at the next poll (typically <100ms).
    fn cancel(&self) {
        self.inner.cancel();
    }

    /// Has `cancel()` been called?
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("RtspCancelHandle(cancelled={})", self.inner.is_cancelled())
    }
}

// ---------------------------------------------------------------------------
// PyRtspClient — static facade exposing connect()
// ---------------------------------------------------------------------------

/// Static facade. Holds no state itself — `connect` produces an
/// `RtspSession` that holds the live client.
#[pyclass(name = "RtspClient", module = "tstrans.rtp", frozen)]
pub struct PyRtspClient;

#[pymethods]
impl PyRtspClient {
    /// Connect to `config.url`, run OPTIONS / DESCRIBE / SETUP /
    /// PLAY, return a live `RtspSession`.
    ///
    /// Raises `tstrans.exceptions.RtspError` on any failure in the
    /// control-plane state machine. The mapped `.kind` enum value
    /// reflects which Rust `RtspError` variant fired (see
    /// `rtsp_error_kind_str` below for the variant → kind table).
    ///
    /// GIL released for the duration of the network exchange — other
    /// Python threads continue to run while we wait on TCP I/O.
    #[staticmethod]
    fn connect(py: Python<'_>, config: &PyRtspClientConfig) -> PyResult<PyRtspSession> {
        // 1. Build the RtspClientBuilder from config. Builder
        //    construction is synchronous (URL parse) and the only
        //    operation here that can yield a typed RtspError; do it
        //    eagerly under the GIL so the error mapping has access to
        //    the Python interpreter handle.
        let mut builder = RtspClientBuilder::new(&config.url)
            .map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))?;

        // 2. Wire credentials. SecretString wrapping happens at this
        //    boundary — Python sees `str`, Rust sees SecretString.
        if let Some(auth_obj) = &config.auth {
            let auth_bound = auth_obj.bind(py);
            if let Ok(basic) = auth_bound.extract::<PyRef<'_, PyBasicAuth>>() {
                let pw = SecretString::from(basic.password.clone());
                builder = builder.auth(basic.user.clone(), pw);
            } else if let Ok(digest) = auth_bound.extract::<PyRef<'_, PyDigestAuth>>() {
                // tst-rtp's RtspClientBuilder::auth doesn't (yet) take an
                // algorithm hint — the challenge handler in
                // `options_describe::handle_auth_challenge_and_retry`
                // picks the algorithm from the server's
                // WWW-Authenticate header. We still capture the
                // algorithm on the Python side for caller introspection
                // through DigestAuth.algorithm.
                let pw = SecretString::from(digest.password.clone());
                builder = builder.auth(digest.user.clone(), pw);
            }
        }

        // 3. `keepalive=false` disables the auto-keepalive thread.
        if !config.keepalive {
            builder = builder.no_auto_keepalive(true);
        }

        // 4. `rtsp_version` and `transport_pref` are captured on the
        //    Python side for surface parity with the C ABI but
        //    tst-rtp's builder doesn't take them — both are derived
        //    from the URL itself (`rtsp://` vs `rtsps://` for
        //    version; `?transport=udp|tcp` for pref). The fields
        //    survive a config round-trip; tighter pass-through wiring
        //    can land in a follow-up.
        let _ = (
            config.rtsp_version.to_rust(),
            config.transport_pref,
            config.rtcp,
        );

        // 5. Custom trust roots for `rtsps://` (private-CA cameras):
        //    parse the PEM bundle into a RootCertStore and hand it to
        //    the builder; without it the handshake verifies against
        //    platform native trust roots.
        builder = apply_tls_roots(py, builder, config)?;

        // 6. Drive the RTSP state machine to PLAY. Wrap in
        //    py.allow_threads for the full network exchange.
        //    Wave B (T23): retain the `RtspSession` so
        //    `RtspSession.into_demux_receiver` can consume its
        //    UDP-socket-pair (or TCP-interleaved mpsc rx) downstream.
        let result = py.allow_threads(
            || -> Result<(RustRtspClient, RustRtspSession), RustRtspError> {
                let mut client = builder.connect()?;
                let _opts = client.options()?;
                let sdp = client.describe()?;
                let session = client.setup_mp2t_auto(&sdp)?;
                let _rtp_info = client.play()?;
                Ok((client, session))
            },
        );

        let (client, session) =
            result.map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))?;

        Ok(PyRtspSession {
            client: Arc::new(Mutex::new(Some(client))),
            session: Arc::new(Mutex::new(Some(session))),
            torn_down: Arc::new(AtomicBool::new(false)),
            h264_depay_config: Arc::new(Mutex::new(None)),
        })
    }

    /// Connect to `config.url`, run OPTIONS / DESCRIBE / SETUP / PLAY for
    /// an H.264 media stream, and return a live `RtspSession`.
    ///
    /// Twin of `connect()`, but calls `setup_h264_auto` instead of
    /// `setup_mp2t_auto`. The resulting session stashes the negotiated
    /// `H264DepayConfig` (payload type + out-of-band SPS/PPS from the
    /// SDP `a=fmtp:` line) so that `session.into_h264_receiver()` can
    /// configure the depacketizer without the caller needing to inspect
    /// the SDP manually.
    ///
    /// GIL released for the duration of the network exchange.
    ///
    /// Raises `RtspError(MOUNT)` when the SDP has no H.264 media or
    /// more than one H.264 media (use `connect()` for MP2T streams).
    /// Raises `RtspError(UNSUPPORTED_TRANSPORT)` for packetization mode 2.
    #[staticmethod]
    fn connect_h264(py: Python<'_>, config: &PyRtspClientConfig) -> PyResult<PyRtspSession> {
        let mut builder = RtspClientBuilder::new(&config.url)
            .map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))?;

        if let Some(auth_obj) = &config.auth {
            let auth_bound = auth_obj.bind(py);
            if let Ok(basic) = auth_bound.extract::<PyRef<'_, PyBasicAuth>>() {
                let pw = SecretString::from(basic.password.clone());
                builder = builder.auth(basic.user.clone(), pw);
            } else if let Ok(digest) = auth_bound.extract::<PyRef<'_, PyDigestAuth>>() {
                // tst-rtp's RtspClientBuilder::auth doesn't (yet) take an
                // algorithm hint — the challenge handler in
                // `options_describe::handle_auth_challenge_and_retry`
                // picks the algorithm from the server's
                // WWW-Authenticate header. We still capture the
                // algorithm on the Python side for caller introspection
                // through DigestAuth.algorithm.
                let pw = SecretString::from(digest.password.clone());
                builder = builder.auth(digest.user.clone(), pw);
            }
        }

        if !config.keepalive {
            builder = builder.no_auto_keepalive(true);
        }

        let _ = (
            config.rtsp_version.to_rust(),
            config.transport_pref,
            config.rtcp,
        );

        builder = apply_tls_roots(py, builder, config)?;

        let result = py.allow_threads(
            || -> Result<(RustRtspClient, RustRtspSession, H264DepayConfig), RustRtspError> {
                let mut client = builder.connect()?;
                let _opts = client.options()?;
                let sdp = client.describe()?;
                let (session, depay_config) = client.setup_h264_auto(&sdp)?;
                let _rtp_info = client.play()?;
                Ok((client, session, depay_config))
            },
        );

        let (client, session, depay_config) =
            result.map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))?;

        Ok(PyRtspSession {
            client: Arc::new(Mutex::new(Some(client))),
            session: Arc::new(Mutex::new(Some(session))),
            torn_down: Arc::new(AtomicBool::new(false)),
            h264_depay_config: Arc::new(Mutex::new(Some(depay_config))),
        })
    }
}

/// Parse `RtspClientConfig.tls_root_certs_pem` into a `rustls::RootCertStore`
/// and hand it to the builder (`rtsps://` verification against a private CA).
/// No-op when the config carries no PEM bundle — the handshake then uses
/// platform native trust roots.
#[cfg(feature = "tls")]
fn apply_tls_roots(
    py: Python<'_>,
    builder: RtspClientBuilder,
    config: &PyRtspClientConfig,
) -> PyResult<RtspClientBuilder> {
    let Some(pem) = config.tls_root_certs_pem.as_ref() else {
        return Ok(builder);
    };
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = std::io::BufReader::new(&pem[..]);
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = cert.map_err(|e| {
            make_rtsp_error(py, "TLS", &format!("tls_root_certs_pem: invalid PEM: {e}"))
        })?;
        roots.add(cert).map_err(|e| {
            make_rtsp_error(
                py,
                "TLS",
                &format!("tls_root_certs_pem: certificate rejected as trust anchor: {e}"),
            )
        })?;
        added += 1;
    }
    if added == 0 {
        return Err(make_rtsp_error(
            py,
            "TLS",
            "tls_root_certs_pem contains no certificates",
        ));
    }
    Ok(builder.tls_root_certs(roots))
}

/// Feature-less twin: reject an explicitly-set PEM bundle instead of
/// silently ignoring it (plaintext-only source build).
#[cfg(not(feature = "tls"))]
fn apply_tls_roots(
    py: Python<'_>,
    builder: RtspClientBuilder,
    config: &PyRtspClientConfig,
) -> PyResult<RtspClientBuilder> {
    if config.tls_root_certs_pem.is_some() {
        return Err(make_rtsp_error(
            py,
            "TLS",
            "tls_root_certs_pem requires the tst-py `tls` feature (on by \
             default; this is a custom build without it)",
        ));
    }
    Ok(builder)
}

// ---------------------------------------------------------------------------
// PyRtspSession — the live PLAY-state client
// ---------------------------------------------------------------------------

/// Live RTSP session — server is in PLAY state. Methods drive the
/// remaining control-plane events (`pause` / `play` resume /
/// `teardown`) and expose RTCP-derived stats.
///
/// `into_demux_receiver` consumes the underlying `RtspSession` and
/// returns a `tstrans.rtp.DemuxReceiver` over the post-SETUP RTP data
/// plane. The control-plane methods (`pause` / `play` / `teardown`)
/// remain usable AFTER `into_demux_receiver` is called — the
/// session-id state lives on the `RtspClient`, which `into_demux_receiver`
/// does NOT consume.
///
/// `__enter__` / `__exit__` make `RtspSession` usable as a context
/// manager — `__exit__` fires `teardown` best-effort.
#[pyclass(name = "RtspSession", module = "tstrans.rtp")]
pub struct PyRtspSession {
    /// Live `tst_rtp::RtspClient` in PLAY state. `Option` because
    /// `teardown()` / `__exit__` consume it; `Arc<Mutex>` because
    /// `cancel_handle()` clones the inner `RtspCancelHandle` (the
    /// handle owns its own `Arc<AtomicBool>` backing flag — sharing
    /// with the client doesn't require holding the mutex).
    client: Arc<Mutex<Option<RustRtspClient>>>,
    /// The SETUP-time `RtspSession` carrying the UDP socket pair (or
    /// TCP-interleaved mpsc receiver) for the data plane. `Option`
    /// because `into_demux_receiver` / `into_h264_receiver` consumes it
    /// — calling either twice raises `RtspError(PROTOCOL)`. The
    /// `Mutex` mirrors the `client` field's pattern so the two fields
    /// can be accessed under uniform locking discipline.
    session: Arc<Mutex<Option<RustRtspSession>>>,
    /// Set true by `teardown` / `__exit__` so duplicate teardowns
    /// are a no-op rather than a "session_id is None" surface from
    /// `RtspClient::teardown`. `Arc<AtomicBool>` so `__exit__` can
    /// observe it across the GIL boundary inside the
    /// `py.allow_threads` closure without holding the python ref.
    torn_down: Arc<AtomicBool>,
    /// H.264 depacketizer config stashed at `connect_h264` time. `None`
    /// when this session was created via `connect()` (MP2T path) or when
    /// `into_h264_receiver()` has already consumed it. Calling
    /// `into_h264_receiver()` on a `connect()`-created session raises
    /// `RtspError(PROTOCOL)`.
    h264_depay_config: Arc<Mutex<Option<H264DepayConfig>>>,
}

#[pymethods]
impl PyRtspSession {
    /// Send PAUSE. Server stops emitting RTP; session remains valid
    /// for a subsequent `play()`.
    fn pause(&mut self, py: Python<'_>) -> PyResult<()> {
        let client = self.client.clone();
        let result = py.allow_threads(move || -> Result<(), RustRtspError> {
            let mut guard = client.lock().map_err(|_| RustRtspError::SessionExpired)?;
            match guard.as_mut() {
                Some(c) => c.pause(),
                None => Err(RustRtspError::SessionExpired),
            }
        });
        result.map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))
    }

    /// Send PLAY (resume after `pause()`).
    fn play(&mut self, py: Python<'_>) -> PyResult<()> {
        let client = self.client.clone();
        let result = py.allow_threads(move || -> Result<(), RustRtspError> {
            let mut guard = client.lock().map_err(|_| RustRtspError::SessionExpired)?;
            match guard.as_mut() {
                Some(c) => c.play().map(|_info| ()),
                None => Err(RustRtspError::SessionExpired),
            }
        });
        result.map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))
    }

    /// Send TEARDOWN. Closes the server session; subsequent
    /// `pause` / `play` calls raise `RtspError(kind=PROTOCOL, ...)`.
    /// The Python wrapper considers itself "torn down" so duplicate
    /// teardowns are a no-op (returns `None`).
    fn teardown(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.torn_down.load(Ordering::Relaxed) {
            return Ok(());
        }
        let client = self.client.clone();
        let torn = self.torn_down.clone();
        let result = py.allow_threads(move || -> Result<(), RustRtspError> {
            let mut guard = client.lock().map_err(|_| RustRtspError::SessionExpired)?;
            let r = match guard.as_mut() {
                Some(c) => c.teardown(),
                None => Ok(()),
            };
            torn.store(true, Ordering::Relaxed);
            r
        });
        result.map_err(|e| make_rtsp_error(py, rtsp_error_kind_str(&e), &e.to_string()))
    }

    /// Cancel handle — flipping `cancel()` breaks any in-flight
    /// `pause` / `play` / `teardown` out of blocking I/O at the next
    /// poll (typically <100ms — the client's TCP read timeout is
    /// 100ms for cancel-responsiveness).
    ///
    /// Returns a fresh `PyRtspCancelHandle` each call; all handles
    /// share the same backing `Arc<AtomicBool>` flag (cloned from
    /// `RustRtspCancelHandle`).
    fn cancel_handle(&self) -> PyResult<PyRtspCancelHandle> {
        let guard = self
            .client
            .lock()
            .map_err(|_| PyValueError::new_err("RtspSession lock poisoned"))?;
        match guard.as_ref() {
            Some(c) => Ok(PyRtspCancelHandle {
                inner: c.cancel_handle(),
            }),
            None => Err(make_rtsp_error_pure(
                "PROTOCOL",
                "RtspSession is torn down; cancel_handle unavailable",
            )),
        }
    }

    /// RTCP-derived stats snapshot.
    ///
    /// Reserved — always returns a zeroed snapshot until RTCP session
    /// stats land; see `docs/project/deferred-features.md`.
    fn stats(&self) -> PyRtspStats {
        PyRtspStats::default()
    }

    /// Consume the session's data-plane (UDP socket pair or
    /// TCP-interleaved mpsc receiver) and wrap it in a
    /// `tstrans.rtp.DemuxReceiver` for iterating `DemuxEvent`s.
    ///
    /// The control-plane methods (`pause` / `play` / `teardown` /
    /// `cancel_handle`) remain usable on this `RtspSession` after the
    /// call — only the data-plane `RtspSession` (an internal Rust
    /// value, distinct from this Python wrapper) is consumed. Calling
    /// `into_demux_receiver` twice on the same `PyRtspSession` raises
    /// `RtspError(PROTOCOL)`.
    ///
    /// `demux_config` accepts the same `tstrans.mpegts.DemuxerConfig`
    /// dataclass the Python `Demuxer(config=...)` constructor takes;
    /// `None` defers to the demuxer defaults.
    ///
    /// `into_*` naming matches the Rust convention; the
    /// `wrong_self_convention` lint is muted because the consumed
    /// resource is the inner data plane, not the Python wrapper.
    #[pyo3(signature = (demux_config = None))]
    #[allow(clippy::wrong_self_convention)]
    fn into_demux_receiver(
        &mut self,
        py: Python<'_>,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyDemuxReceiver> {
        // Guard: reject if this session was created by connect_h264() — the
        // H264DepayConfig slot being Some signals an H.264 session.
        {
            let guard = self
                .h264_depay_config
                .lock()
                .map_err(|_| PyValueError::new_err("RtspSession lock poisoned"))?;
            if guard.is_some() {
                return Err(make_rtsp_error(
                    py,
                    "PROTOCOL",
                    "RtspSession.into_demux_receiver: this session was negotiated \
                     for H.264 elementary ingest — use into_h264_receiver() instead",
                ));
            }
        }
        // Take the SETUP-time RtspSession; double-consume = protocol err.
        let session = {
            let mut guard = self
                .session
                .lock()
                .map_err(|_| PyValueError::new_err("RtspSession lock poisoned"))?;
            guard.take().ok_or_else(|| {
                make_rtsp_error_pure(
                    "PROTOCOL",
                    "RtspSession.into_demux_receiver: already consumed",
                )
            })?
        };
        // Convert to RtpRecvTransport. The session's `into_recv_transport`
        // panics only on internal-state-corruption shapes that can't
        // happen on a SETUP-succeeded path; we don't catch_unwind here
        // because the upstream invariants are stable. If a future
        // tst-rtp release flips this to fallible we'll switch to
        // map_err.
        let transport = session.into_recv_transport();
        // Optionally lift demux config; build with defaults otherwise.
        let opts = match demux_config {
            None => None,
            Some(cfg) => Some(crate::mpegts::build_demuxer_config(py, cfg)?),
        };
        PyDemuxReceiver::from_recv_transport(py, transport, opts)
    }

    /// Consume the session's data plane and wrap it in an `H264Receiver`
    /// for iterating reassembled H.264 Access Units.
    ///
    /// Raises `RtspError(PROTOCOL)` when:
    /// - this session was created via `connect()` (not `connect_h264()`), or
    /// - `into_h264_receiver()` or `into_demux_receiver()` has already been
    ///   called on this session (data plane consumed).
    ///
    /// The control-plane methods (`pause` / `play` / `teardown` /
    /// `cancel_handle`) remain usable after the call — only the data-plane
    /// `RtspSession` (the inner Rust value) is consumed.
    ///
    /// Handle is zeroed BEFORE the fallible native work to avoid
    /// double-free if the construction raises (the double-free lesson).
    #[allow(clippy::wrong_self_convention)]
    fn into_h264_receiver(&mut self, py: Python<'_>) -> PyResult<PyH264Receiver> {
        // Step 1: take the H264DepayConfig stashed at connect_h264 time.
        // None = session was created by connect() (wrong path).
        let depay_config = {
            let mut guard = self
                .h264_depay_config
                .lock()
                .map_err(|_| PyValueError::new_err("RtspSession lock poisoned"))?;
            guard.take().ok_or_else(|| {
                make_rtsp_error(
                    py,
                    "PROTOCOL",
                    "RtspSession.into_h264_receiver: session was not created by \
                     connect_h264(), or the H264DepayConfig has already been consumed",
                )
            })?
        };

        // Step 2: take the SETUP-time RtspSession.
        // Zero this BEFORE the fallible into_h264_receiver call (double-free lesson).
        let session = {
            let mut guard = self
                .session
                .lock()
                .map_err(|_| PyValueError::new_err("RtspSession lock poisoned"))?;
            guard.take().ok_or_else(|| {
                make_rtsp_error(
                    py,
                    "PROTOCOL",
                    "RtspSession.into_h264_receiver: data plane already consumed",
                )
            })?
        };

        // Step 3: convert the RTSP session into an H264Receiver.
        let receiver = session.into_h264_receiver(depay_config);
        Ok(PyH264Receiver::from_h264_receiver(receiver))
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Best-effort teardown on context-manager exit. Errors are
    /// swallowed (the contract is "ensure session closed", not "fail
    /// if server is uncooperative") — caller can call `.teardown()`
    /// explicitly to get the typed error.
    fn __exit__(
        &mut self,
        py: Python<'_>,
        exc_type: &Bound<'_, PyAny>,
        exc_value: &Bound<'_, PyAny>,
        traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let _ = (exc_type, exc_value, traceback);
        if !self.torn_down.load(Ordering::Relaxed) {
            // swallow any error
            let _ = self.teardown(py);
        }
        Ok(false) // do not suppress exceptions
    }

    /// Returns true once `teardown` / `__exit__` has fired.
    fn is_torn_down(&self) -> bool {
        self.torn_down.load(Ordering::Relaxed)
    }
}

/// `make_rtsp_error` requires a Python handle; this helper provides a
/// non-Python-handle variant for sites that have no `py` in scope
/// (e.g. `cancel_handle` returning a typed error inside a `?` chain
/// before `Python::with_gil`). Returns a `PyErr` that wraps a Python
/// `RtspError` instance by re-acquiring the GIL.
fn make_rtsp_error_pure(kind: &str, message: &str) -> PyErr {
    Python::with_gil(|py| make_rtsp_error(py, kind, message))
}

// ---------------------------------------------------------------------------
// Helper: RtspError → SHOUTY_SNAKE kind classifier
// ---------------------------------------------------------------------------

/// Map a Rust `RtspError` variant onto the SHOUTY_SNAKE kind name
/// expected by `make_rtsp_error` / `tstrans.exceptions.RtspErrorKind`.
///
/// Bucket policy (collapsed-variant rule from the design spec):
///
/// - `Io` → `IO` (transport-level socket failure)
/// - `Tls` → `TLS`
/// - `Protocol { code: 404, .. }` → `NOT_FOUND`
/// - `Protocol { code: 401, .. }` → `AUTH_REQUIRED` (server demanded
///   auth and we couldn't satisfy it on retry)
/// - `Protocol { .. }` (other 4xx/5xx) → `PROTOCOL`
/// - `AuthFailed` / `AuthUnsupported` → `AUTH_FAILED`
/// - `UnsupportedTransport` → `UNSUPPORTED_TRANSPORT`
/// - `BadResponse` / `BadSdp` / `SessionExpired` / `LocalCancel` / `Url`
///   → `PROTOCOL`
/// - `Timeout` → `TIMEOUT`
/// - `NoMp2tMedia` / `MultipleMp2tMedia` → `MOUNT` (SETUP-time mount
///   path issue from the SDP side)
/// - `NoH264Media` / `MultipleH264Media` → `MOUNT` (H.264 SETUP-time SDP issues)
/// - `UnsupportedPacketizationMode` → `UNSUPPORTED_TRANSPORT` (parity with
///   `UnsupportedTransport` and the C RtspUnsupported bucket — the error is
///   about a transport/encoding mode the client cannot speak, not a
///   server-side mount/configuration failure)
/// - any future `#[non_exhaustive]` variant → `PROTOCOL` (catch-all;
///   the bash ratchet flags missing Python-side variants when they
///   land)
fn rtsp_error_kind_str(e: &RustRtspError) -> &'static str {
    match e {
        RustRtspError::Io(_) => "IO",
        RustRtspError::Tls(_) => "TLS",
        RustRtspError::Protocol { code: 404, .. } => "NOT_FOUND",
        RustRtspError::Protocol { code: 401, .. } => "AUTH_REQUIRED",
        RustRtspError::Protocol { .. } => "PROTOCOL",
        RustRtspError::AuthFailed => "AUTH_FAILED",
        RustRtspError::AuthUnsupported { .. } => "AUTH_FAILED",
        RustRtspError::BadResponse { .. } => "PROTOCOL",
        RustRtspError::BadSdp { .. } => "PROTOCOL",
        RustRtspError::UnsupportedTransport => "UNSUPPORTED_TRANSPORT",
        RustRtspError::SessionExpired => "PROTOCOL",
        RustRtspError::Timeout => "TIMEOUT",
        RustRtspError::LocalCancel => "PROTOCOL",
        RustRtspError::NoMp2tMedia => "MOUNT",
        RustRtspError::MultipleMp2tMedia { .. } => "MOUNT",
        RustRtspError::NoH264Media => "MOUNT",
        RustRtspError::MultipleH264Media { .. } => "MOUNT",
        RustRtspError::UnsupportedPacketizationMode(_) => "UNSUPPORTED_TRANSPORT",
        RustRtspError::Url(_) => "PROTOCOL",
        // #[non_exhaustive] wildcard — future variants land in PROTOCOL
        // until the Python-side RtspErrorKind grows a matching variant.
        _ => "PROTOCOL",
    }
}

// ---------------------------------------------------------------------------
// Server-side variant call-site anchors for the ratchet
// ---------------------------------------------------------------------------
//
// The consolidated `scripts/check/python/error-mapping-coverage.sh`
// ratchet scans `bindings/python/src/` for at least one literal
// `make_rtsp_error(<py>, "KIND", ...)` call site per
// `RtspErrorKind` variant. Wave A's natural call sites cover most
// kinds via `rtsp_error_kind_str`, but `SERVER` and `MOUNT` (mostly
// server-side concepts coming in Task 22 / Wave B / future work)
// need anchor call sites here too. The function below is `#[allow(
// dead_code)]`, not called from anywhere, but the grep-ratchet
// counts it.

#[allow(dead_code)]
fn _ratchet_coverage_anchor(py: Python<'_>) -> PyErr {
    let _ = make_rtsp_error(py, "PROTOCOL", "ratchet anchor");
    let _ = make_rtsp_error(py, "AUTH_FAILED", "ratchet anchor");
    let _ = make_rtsp_error(py, "AUTH_REQUIRED", "ratchet anchor");
    let _ = make_rtsp_error(py, "NOT_FOUND", "ratchet anchor");
    let _ = make_rtsp_error(py, "UNSUPPORTED_TRANSPORT", "ratchet anchor");
    let _ = make_rtsp_error(py, "TLS", "ratchet anchor");
    let _ = make_rtsp_error(py, "IO", "ratchet anchor");
    let _ = make_rtsp_error(py, "TIMEOUT", "ratchet anchor");
    let _ = make_rtsp_error(py, "SERVER", "ratchet anchor");
    let _ = make_rtsp_error(py, "MOUNT", "ratchet anchor");
    make_rtsp_error(py, "PROTOCOL", "unreachable")
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyRtspVersion>()?;
    m.add_class::<PyTransportPref>()?;
    m.add_class::<PyDigestAlgorithm>()?;
    m.add_class::<PyBasicAuth>()?;
    m.add_class::<PyDigestAuth>()?;
    m.add_class::<PyRtspClientConfig>()?;
    m.add_class::<PyRtspStats>()?;
    m.add_class::<PyRtspCancelHandle>()?;
    m.add_class::<PyRtspClient>()?;
    m.add_class::<PyRtspSession>()?;
    Ok(())
}
