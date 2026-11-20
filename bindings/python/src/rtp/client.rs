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
//!   / `teardown` delegate to it under `py.detach`; `close` (and the
//!   `with` exit) takes it out and ends it with `RtspClient::close`.
//! - `into_demux_receiver` converts the session into a demux-capable
//!   receiver; calling it before the session is playing raises
//!   `NotImplementedError`.
//! - Secrets (`DigestAuth.password`, `BasicAuth.password`) are accepted
//!   from Python as `str`, wrapped in `secrecy::SecretString` only when
//!   handed to `RtspClientBuilder::auth`, and never re-exposed to
//!   Python (only `user` / `algorithm` are readable through getters).
//!
//! GIL release boundaries:
//!
//! - `py.detach`: `connect`, `play`, `pause`, `teardown`, `close` (and
//!   `__exit__`, which calls it).
//! - **Not** wrapped: `cancel_handle()`, `stats()`, dataclass ctors,
//!   `__enter__` (these are non-blocking).
//!
//! `#![allow(...)]` mirrors `errors.rs` / `mpegts.rs` — PyO3's macro
//! expansions under Rust 2024 trip these lints. Hand-written code in
//! this module has no unsafe blocks.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use secrecy::SecretString;
use tst_core::url::redact_url;
use tst_rtp::error::RtspError as RustRtspError;
use tst_rtp::rtsp::client::session::RtspSession as RustRtspSession;
use tst_rtp::rtsp::client::{
    RtspCancelHandle as RustRtspCancelHandle, RtspClient as RustRtspClient,
};
use tst_rtp::{
    AuthSchemes, H264DepayConfig, RtspClientBuilder, RtspPush as RustRtspPush, RtspTransportPref,
    RtspVersion,
};

use crate::raise::{RTSP, raise};
use crate::rtp::h264_receiver::PyH264Receiver;
use crate::rtp::push::{PyPushDescription, PyRtspPushSession};
use crate::shells::demux_receiver::PyDemuxReceiver;
use tst_pipeline::binding::{BindingError, BindingErrorKind};

// ---------------------------------------------------------------------------
// Enums (Python IntEnum-equivalent PyClasses)
// ---------------------------------------------------------------------------

/// Wire-time RTSP version preference. Mirrors `tst_rtp::RtspVersion` —
/// only the 1.0 / 2.0 split matters at the SETUP / PLAY layer.
#[pyclass(
    from_py_object,
    eq,
    eq_int,
    name = "RtspVersion",
    module = "tstrans.rtp"
)]
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
#[pyclass(
    from_py_object,
    eq,
    eq_int,
    name = "TransportPref",
    module = "tstrans.rtp"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum PyTransportPref {
    AUTO = 0,
    UDP = 1,
    TCP = 2,
}

impl PyTransportPref {
    fn to_rust(self) -> RtspTransportPref {
        match self {
            PyTransportPref::AUTO => RtspTransportPref::PreferUdp,
            PyTransportPref::UDP => RtspTransportPref::ForceUdp,
            PyTransportPref::TCP => RtspTransportPref::ForceTcp,
        }
    }
}

/// Digest authentication algorithm selector. Mirrors the user-facing
/// subset of the digest algorithms tst-rtp's client answers — the
/// `*-sess` variants are parsed from server challenges but not
/// selectable by the caller; tst-rtp's challenge handler picks them
/// automatically when the server demands them.
#[pyclass(
    from_py_object,
    eq,
    eq_int,
    name = "DigestAlgorithm",
    module = "tstrans.rtp"
)]
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
#[derive(Clone)]
pub struct PyBasicAuth {
    pub(crate) user: String,
    /// Held as a plain Rust `String` because the Python-side surface
    /// already accepted it as a `str` (it lives in Python memory
    /// before we get it). `secrecy::SecretString` adoption happens at
    /// the `RtspClientBuilder::auth` call boundary in
    /// `builder_from_config`.
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
///
/// As a client credential it never falls back to Basic: a server that
/// offers no Digest challenge fails the request instead of receiving the
/// password in the clear. Use `BasicAuth` to accept either scheme.
#[pyclass(name = "DigestAuth", module = "tstrans.rtp", frozen)]
#[derive(Clone)]
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
/// controls UDP/TCP selection at SETUP and `rtsp_version` the request-line
/// version; a non-default value overrides the URL's `?transport=` /
/// `?rtsp_version=`, the defaults leave the URL in charge. `rtsps://`
/// connections verify the server certificate against platform native trust roots by default;
/// pass `tls_root_certs_pem` (a PEM bundle) to verify against a private
/// CA / self-signed camera cert instead.
///
/// `keepalive_interval_ms` pins the keepalive cadence (default: half the
/// session timeout the server advertises); `connect_timeout_ms` bounds the
/// TCP connect and the TLS handshake together (default 10 s);
/// `request_timeout_ms` bounds each request's write and reply (default
/// 10 s); `user_agent` replaces the `User-Agent` header. `None` keeps the
/// default; zero or below, and an interval with `keepalive=False`, raise
/// `ValueError`.
#[pyclass(name = "RtspClientConfig", module = "tstrans.rtp", frozen)]
pub struct PyRtspClientConfig {
    url: String,
    /// Stored as `Py<PyAny>` so we can hand it back through the getter
    /// while preserving its concrete Python class identity.
    auth: Option<Py<PyAny>>,
    transport_pref: PyTransportPref,
    tls_root_certs_pem: Option<Vec<u8>>,
    keepalive: bool,
    rtsp_version: PyRtspVersion,
    keepalive_interval_ms: Option<u64>,
    connect_timeout_ms: Option<u64>,
    request_timeout_ms: Option<u64>,
    user_agent: Option<String>,
}

/// A millisecond knob: `None`, or a positive count.
fn positive_ms(name: &str, v: Option<i64>) -> PyResult<Option<u64>> {
    match v {
        None => Ok(None),
        Some(ms) if ms > 0 => Ok(Some(ms as u64)),
        Some(ms) => Err(PyValueError::new_err(format!(
            "RtspClientConfig: {name} must be positive, got {ms}"
        ))),
    }
}

fn ms_repr(v: Option<u64>) -> String {
    v.map_or_else(|| "None".to_owned(), |ms| ms.to_string())
}

#[pymethods]
impl PyRtspClientConfig {
    #[new]
    #[pyo3(signature = (
        url,
        *,
        auth = None,
        transport_pref = PyTransportPref::AUTO,
        tls_root_certs_pem = None,
        keepalive = true,
        rtsp_version = PyRtspVersion::V1_0,
        keepalive_interval_ms = None,
        connect_timeout_ms = None,
        request_timeout_ms = None,
        user_agent = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        url: String,
        auth: Option<&Bound<'_, PyAny>>,
        transport_pref: PyTransportPref,
        tls_root_certs_pem: Option<Vec<u8>>,
        keepalive: bool,
        rtsp_version: PyRtspVersion,
        keepalive_interval_ms: Option<i64>,
        connect_timeout_ms: Option<i64>,
        request_timeout_ms: Option<i64>,
        user_agent: Option<String>,
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
        let keepalive_interval_ms = positive_ms("keepalive_interval_ms", keepalive_interval_ms)?;
        if keepalive_interval_ms.is_some() && !keepalive {
            return Err(PyValueError::new_err(
                "RtspClientConfig: keepalive_interval_ms needs keepalive=True",
            ));
        }
        Ok(Self {
            url,
            auth: auth_obj,
            transport_pref,
            tls_root_certs_pem,
            keepalive,
            rtsp_version,
            keepalive_interval_ms,
            connect_timeout_ms: positive_ms("connect_timeout_ms", connect_timeout_ms)?,
            request_timeout_ms: positive_ms("request_timeout_ms", request_timeout_ms)?,
            user_agent,
        })
    }

    #[getter]
    fn url(&self) -> &str {
        &self.url
    }

    #[getter]
    fn auth(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.auth.as_ref().map(|p| p.clone_ref(py))
    }

    #[getter]
    fn transport_pref(&self) -> PyTransportPref {
        self.transport_pref
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

    #[getter]
    fn keepalive_interval_ms(&self) -> Option<u64> {
        self.keepalive_interval_ms
    }

    #[getter]
    fn connect_timeout_ms(&self) -> Option<u64> {
        self.connect_timeout_ms
    }

    #[getter]
    fn request_timeout_ms(&self) -> Option<u64> {
        self.request_timeout_ms
    }

    #[getter]
    fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    fn __repr__(&self) -> String {
        format!(
            "RtspClientConfig(url={:?}, auth={}, transport_pref={:?}, \
             tls_root_certs_pem={}, keepalive={}, rtsp_version={:?}, \
             keepalive_interval_ms={}, connect_timeout_ms={}, request_timeout_ms={}, \
             user_agent={:?})",
            redact_url(&self.url),
            if self.auth.is_some() {
                "<auth>"
            } else {
                "None"
            },
            self.transport_pref,
            if self.tls_root_certs_pem.is_some() {
                "<bytes>"
            } else {
                "None"
            },
            self.keepalive,
            self.rtsp_version,
            ms_repr(self.keepalive_interval_ms),
            ms_repr(self.connect_timeout_ms),
            ms_repr(self.request_timeout_ms),
            self.user_agent,
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
             interarrival_jitter_us={}, fraction_lost_q8={})",
            self.rr_packets_received,
            self.sr_packets_received,
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
/// cancel (post-PLAY RTP data) is exposed by `tstrans.shells.CancelHandle`
/// from `transport.rs`. The two flags are independent in the
/// underlying Rust API; we expose them under distinct Python class
/// names to keep the contracts honest.
#[pyclass(name = "RtspCancelHandle", module = "tstrans.rtp", frozen)]
pub struct PyRtspCancelHandle {
    pub(crate) inner: RustRtspCancelHandle,
}

#[pymethods]
impl PyRtspCancelHandle {
    /// Flip the cancel flag. An in-flight `pause` / `play` / `teardown`
    /// on the originating session ends with `RtspError(CLOSED)`: a request
    /// waiting for its reply notices the flag within 100 ms; a request
    /// whose bytes the server is not taking notices it only when its write
    /// completes, its deadline passes or 5 s without progress end the
    /// control connection.
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

/// Apply the config's `transport_pref` / `rtsp_version`. The defaults
/// (`AUTO`, `V1_0`) leave the URL's `?transport=` / `?rtsp_version=` in
/// charge; any other value overrides the URL — the C builder's rule, where
/// `tst_rtsp_client_builder_transport_pref` overrides only when called.
fn apply_transport_and_version(
    mut builder: RtspClientBuilder,
    config: &PyRtspClientConfig,
) -> RtspClientBuilder {
    if config.transport_pref != PyTransportPref::AUTO {
        builder = builder.transport_preference(config.transport_pref.to_rust());
    }
    if config.rtsp_version != PyRtspVersion::V1_0 {
        builder = builder.rtsp_version(config.rtsp_version.to_rust());
    }
    builder
}

/// The `RtspClientBuilder` a config describes: the URL (a parse error is
/// `RtspError`), the credentials, `keepalive` and its interval, the
/// connect and request timeouts, the user agent, `transport_pref` /
/// `rtsp_version` and the TLS trust roots. Every factory of `RtspClient`
/// starts from it.
fn builder_from_config(py: Python<'_>, config: &PyRtspClientConfig) -> PyResult<RtspClientBuilder> {
    // URL parse, under the GIL so the error maps through `raise`.
    let mut builder =
        RtspClientBuilder::new(&config.url).map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;

    // Credentials. SecretString wrapping happens at this boundary: Python
    // sees `str`, Rust sees SecretString.
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
            builder = builder
                .auth(digest.user.clone(), pw)
                .auth_schemes(AuthSchemes::DigestOnly);
        }
    }

    // `keepalive=false` disables the automatic keepalive pings.
    if !config.keepalive {
        builder = builder.no_auto_keepalive(true);
    }
    if let Some(ms) = config.keepalive_interval_ms {
        builder = builder.keepalive_interval(Duration::from_millis(ms));
    }
    if let Some(ms) = config.connect_timeout_ms {
        builder = builder.connect_timeout(Duration::from_millis(ms));
    }
    if let Some(ms) = config.request_timeout_ms {
        builder = builder.request_timeout(Some(Duration::from_millis(ms)));
    }
    if let Some(ua) = &config.user_agent {
        builder = builder.user_agent(ua.clone());
    }

    // `transport_pref` / `rtsp_version` (non-default values override the
    // URL query).
    builder = apply_transport_and_version(builder, config);

    // Custom trust roots for `rtsps://` (private-CA cameras): without them
    // the handshake verifies against platform native trust roots.
    apply_tls_roots(py, builder, config)
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
    /// control-plane state machine. The `.kind` reflects which Rust
    /// `RtspError` variant fired — the table is
    /// `impl From<RtspError> for BindingError` in
    /// `crates/tst-rtp/src/binding_kind.rs`, resolved on
    /// `RtspErrorKind` by `crate::raise`.
    ///
    /// GIL released for the duration of the network exchange — other
    /// Python threads continue to run while we wait on TCP I/O.
    #[staticmethod]
    fn connect(py: Python<'_>, config: &PyRtspClientConfig) -> PyResult<PyRtspSession> {
        let builder = builder_from_config(py, config)?;

        // Drive the RTSP state machine to PLAY. Wrap in py.detach for the
        // full network exchange. Retain the `RtspSession` so
        // `RtspSession.into_demux_receiver` can consume its
        // UDP-socket-pair (or TCP-interleaved mpsc rx) downstream.
        let result = crate::util::allow_threads_parking(
            py,
            || -> Result<(RustRtspClient, RustRtspSession), RustRtspError> {
                let mut client = builder.connect()?;
                let _opts = client.options()?;
                let sdp = client.describe()?;
                let session = client.setup_mp2t_auto(&sdp)?;
                let _rtp_info = client.play()?;
                Ok((client, session))
            },
        );

        let (client, session) = result.map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;

        Ok(PyRtspSession::new(client, session, None))
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
    /// Raises `RtspError(NOT_FOUND)` when the SDP has no H.264 media or
    /// more than one H.264 media (use `connect()` for MP2T streams).
    /// Raises `RtspError(UNSUPPORTED_TRANSPORT)` for packetization mode 2.
    #[staticmethod]
    fn connect_h264(py: Python<'_>, config: &PyRtspClientConfig) -> PyResult<PyRtspSession> {
        let builder = builder_from_config(py, config)?;

        let result = crate::util::allow_threads_parking(
            py,
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
            result.map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;

        Ok(PyRtspSession::new(client, session, Some(depay_config)))
    }

    /// Connect to `config.url` and publish to it: ANNOUNCE `description`,
    /// SETUP every track (`mode=record`, interleaved on the control
    /// connection) and RECORD. Returns the recording `RtspPushSession`.
    ///
    /// Raises `RtspError` when any of the four steps fails; the connection
    /// is closed then (after a TEARDOWN once a session is open).
    /// `transport_pref=TransportPref.UDP`, or a URL carrying
    /// `?transport=udp`, raises `UNSUPPORTED_TRANSPORT` before any SETUP is
    /// written (publishing over UDP is not implemented), and
    /// `rtsp_version=V2_0` raises `PROTOCOL` before the ANNOUNCE is written
    /// (RTSP 2.0 has no ANNOUNCE). GIL released for the network exchange.
    ///
    /// `keepalive=True` (the default) sends keepalive pings at half the
    /// session timeout the server advertises; the config has no interval.
    /// The pings do not keep alive a push that goes silent for less than
    /// that: a server may end a publisher it reads nothing from sooner
    /// (mediamtx 1.21.4 after 10 s, while it advertises 55 s), so keep
    /// media flowing.
    #[staticmethod]
    fn push(
        py: Python<'_>,
        config: &PyRtspClientConfig,
        description: PyRef<'_, PyPushDescription>,
    ) -> PyResult<PyRtspPushSession> {
        let builder = builder_from_config(py, config)?;
        let desc = description.inner.clone();
        let mp2t = description.is_mp2t();
        let push =
            crate::util::allow_threads_parking(py, move || RustRtspPush::connect(builder, &desc))
                .map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;
        Ok(PyRtspPushSession::new(push, mp2t))
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
    use tst_core::net::tls::TlsLoadError as E;
    let roots = tst_core::net::tls::load_root_store(pem).map_err(|e| {
        let detail = match e {
            E::Pem { source, .. } => format!("tls_root_certs_pem: invalid PEM: {source}"),
            E::NoCertificates { .. } => "tls_root_certs_pem contains no certificates".to_string(),
            E::TrustAnchor(err) => {
                format!("tls_root_certs_pem: certificate rejected as trust anchor: {err}")
            }
            other => format!("tls_root_certs_pem: {other}"),
        };
        raise(
            py,
            &RTSP,
            BindingError::new(BindingErrorKind::RtspTls, detail),
        )
    })?;
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
        return Err(raise(
            py,
            &RTSP,
            BindingError::new(
                BindingErrorKind::RtspTls,
                "tls_root_certs_pem requires the tst-py `tls` feature (on by \
                 default; this is a custom build without it)",
            ),
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
/// returns a `tstrans.shells.DemuxReceiver` over the post-SETUP RTP data
/// plane. The control-plane methods (`pause` / `play` / `teardown`)
/// remain usable AFTER `into_demux_receiver` is called — the
/// session-id state lives on the `RtspClient`, which `into_demux_receiver`
/// does NOT consume.
///
/// `__enter__` / `__exit__` make `RtspSession` usable as a context
/// manager — `__exit__` calls `close`.
///
/// Frozen: every method takes `&self` and all state is behind a mutex or
/// an atomic, so a second thread's call is never refused by a borrow
/// flag while a control call is parked.
#[pyclass(name = "RtspSession", module = "tstrans.rtp", frozen)]
pub struct PyRtspSession {
    /// Live `tst_rtp::RtspClient` in PLAY state. `Option` because
    /// `close()` / `__exit__` take it out to end it. `pause` / `play` /
    /// `teardown` hold the mutex across their network exchange. A receiver
    /// taken by `into_*` holds a clone of the `Arc`, so the client ends at
    /// `close()`, or when the last of the session and its receiver goes.
    client: Arc<Mutex<Option<RustRtspClient>>>,
    /// A clone of the client's cancel, kept outside the `client` mutex so
    /// `cancel_handle()` and a contended `close()` never wait behind a
    /// parked control call.
    rtsp_cancel: RustRtspCancelHandle,
    /// Set when `close()` begins: `cancel_handle()` and the data-plane
    /// takes are refused from then.
    closed: AtomicBool,
    /// The SETUP-time data plane: the `RtspSession` carrying the UDP socket
    /// pair (or TCP-interleaved mpsc receiver), with the H.264
    /// depacketizer config of a `connect_h264` session. `None` once
    /// `into_demux_receiver` / `into_h264_receiver` took it: any later take
    /// raises `RtspError(CLOSED)`. Which receiver may take it is decided
    /// and the take done under this one lock. A refusal builds its
    /// exception only after the guard drops: the exception's `__init__` is
    /// Python and can hand the GIL to a thread waiting for the same lock.
    data_plane: Mutex<Option<DataPlane>>,
    /// Set true by `teardown` / `close` so duplicate teardowns
    /// are a no-op rather than a "session_id is None" surface from
    /// `RtspClient::teardown`. `Arc<AtomicBool>` so the `py.detach`
    /// closures can set it without holding the python ref.
    torn_down: Arc<AtomicBool>,
}

/// What a session's data plane carries: MPEG-TS from `connect()`, or H.264
/// with its depacketizer config from `connect_h264()`.
enum DataPlane {
    Mp2t(RustRtspSession),
    H264(RustRtspSession, H264DepayConfig),
}

/// `into_demux_receiver` on a `connect_h264` session.
fn h264_session_error() -> BindingError {
    BindingError::new(
        BindingErrorKind::RtspProtocol,
        "RtspSession.into_demux_receiver: this session was negotiated \
         for H.264 elementary ingest — use into_h264_receiver() instead",
    )
}

/// The error for a session whose lock a panic poisoned: unusable from then
/// on, as a closed session is.
fn poisoned() -> BindingError {
    BindingError::new(
        BindingErrorKind::Closed,
        "RtspSession: the session's lock was poisoned by a panic in an earlier call",
    )
}

/// A session collected without `close()`: the client's own drop sends a
/// best-effort TEARDOWN with the GIL released, then tells the client's
/// thread to stop without waiting for it; that thread closes the
/// connection on its own. Collection must not wait, so it does not call
/// `RtspClient::close`.
impl Drop for PyRtspSession {
    fn drop(&mut self) {
        // Only this session's share of the client: a receiver taken from it
        // keeps the client, and the last holder's drop ends it.
        let client = std::mem::take(&mut self.client);
        let session = self
            .data_plane
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        crate::util::drop_blocking(move || {
            drop(client);
            drop(session);
        });
    }
}

#[pymethods]
impl PyRtspSession {
    /// Send PAUSE. Server stops emitting RTP; session remains valid
    /// for a subsequent `play()`. Raises `RtspError(CLOSED)` after
    /// `close()`.
    fn pause(&self, py: Python<'_>) -> PyResult<()> {
        self.control(py, "RtspSession.pause: the session is closed", |c| {
            c.pause()
        })
    }

    /// Send PLAY (resume after `pause()`). Raises `RtspError(CLOSED)`
    /// after `close()`.
    fn play(&self, py: Python<'_>) -> PyResult<()> {
        self.control(py, "RtspSession.play: the session is closed", |c| {
            c.play().map(|_info| ())
        })
    }

    /// Send TEARDOWN and report the server's answer; the client is kept
    /// (its thread and the control connection live on until `close()`,
    /// the `with` exit or collection). Subsequent `pause` / `play` calls
    /// raise `RtspError(kind=PROTOCOL, ...)`. The Python wrapper considers
    /// itself "torn down" so duplicate teardowns, and a teardown after
    /// `close()`, are a no-op (returns `None`).
    fn teardown(&self, py: Python<'_>) -> PyResult<()> {
        if self.torn_down.load(Ordering::Relaxed) {
            return Ok(());
        }
        let client = self.client.clone();
        let torn = self.torn_down.clone();
        let result = crate::util::allow_threads_parking(py, move || -> Result<(), BindingError> {
            let mut guard = client.lock().map_err(|_| poisoned())?;
            let r = match guard.as_mut() {
                Some(c) => c.teardown(),
                None => Ok(()),
            };
            torn.store(true, Ordering::Relaxed);
            r.map_err(BindingError::from)
        });
        result.map_err(|e| raise(py, &RTSP, e))
    }

    /// End the session and the client: TEARDOWN with one 500 ms bound
    /// over writing it and its reply, then the client's thread is stopped
    /// and waited for up to 2 s; the control connection is closed when
    /// this returns (unless that wait ran out). Silent: returns `None`
    /// whatever the server answered (`teardown()` is the call that
    /// reports). A call made while another `close()` is running returns at
    /// once without waiting for it; any later call is a no-op.
    ///
    /// A control call in progress on another thread is cancelled first (it
    /// raises `RtspError(CLOSED)`); the TEARDOWN then goes out on the
    /// cancelled client without awaiting the reply, so the session ends as
    /// a cancel (a live receiver ends `RtpError(CLOSED)`), not as a clean
    /// teardown. Afterwards `pause` / `play` / `cancel_handle` and the
    /// data-plane takes (`into_demux_receiver` / `into_h264_receiver`)
    /// raise `RtspError(CLOSED)`, `teardown` returns `None`, and a cancel
    /// handle taken earlier reads `is_cancelled() == True`.
    fn close(&self, py: Python<'_>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = self.client.clone();
        let cancel = self.rtsp_cancel.clone();
        let torn = self.torn_down.clone();
        crate::util::allow_threads_parking(py, move || {
            let taken = match client.try_lock() {
                Ok(mut guard) => guard.take(),
                Err(TryLockError::WouldBlock) => {
                    cancel.cancel();
                    client.lock().unwrap_or_else(|e| e.into_inner()).take()
                }
                Err(TryLockError::Poisoned(e)) => e.into_inner().take(),
            };
            if let Some(c) = taken {
                let _ = c.close();
            }
            torn.store(true, Ordering::Relaxed);
        });
    }

    /// Cancel handle — its `cancel()` ends an in-flight `pause` / `play` /
    /// `teardown` (see `RtspCancelHandle.cancel` for how soon). Answers at
    /// once, also while another thread's control
    /// call is parked. The control calls run one at a time: a second
    /// `pause` / `play` / `teardown` waits, GIL released, until the call in
    /// progress returns or is cancelled.
    ///
    /// Returns a fresh `PyRtspCancelHandle` each call; all handles
    /// share the client's backing flag. Returned after `teardown()` too;
    /// raises `RtspError(CLOSED)` once `close()` has begun.
    fn cancel_handle(&self, py: Python<'_>) -> PyResult<PyRtspCancelHandle> {
        if self.closed.load(Ordering::Acquire) {
            return Err(raise(
                py,
                &RTSP,
                BindingError::new(
                    BindingErrorKind::Closed,
                    "RtspSession is closed; cancel_handle unavailable",
                ),
            ));
        }
        Ok(PyRtspCancelHandle {
            inner: self.rtsp_cancel.clone(),
        })
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
    /// `tstrans.shells.DemuxReceiver` for iterating `DemuxEvent`s.
    ///
    /// The control-plane methods (`pause` / `play` / `teardown` /
    /// `cancel_handle`) remain usable on this `RtspSession` after the
    /// call — only the data-plane `RtspSession` (an internal Rust
    /// value, distinct from this Python wrapper) is consumed. Calling
    /// `into_demux_receiver` twice on the same `PyRtspSession`, or once
    /// `close()` has begun, raises `RtspError(CLOSED)` (the data plane is
    /// a consumable handle). The receiver holds the session's client, so
    /// collecting this session object does not end it; `close()` here does.
    ///
    /// `demux_config` accepts the same `tstrans.mpegts.DemuxerConfig`
    /// dataclass the Python `Demuxer(config=...)` constructor takes;
    /// `None` defers to the demuxer defaults. A rejected config raises and
    /// leaves the source unconsumed.
    ///
    /// `into_*` naming matches the Rust convention; the
    /// `wrong_self_convention` lint is muted because the consumed
    /// resource is the inner data plane, not the Python wrapper.
    #[pyo3(signature = (demux_config = None))]
    #[allow(clippy::wrong_self_convention)]
    fn into_demux_receiver(
        &self,
        py: Python<'_>,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyDemuxReceiver> {
        // A closed session, a consumed data plane and an H.264 session
        // refuse before the config is read, so a bad config never masks
        // them; the take below decides again under its own lock.
        self.refuse_if_closed("RtspSession.into_demux_receiver: the session is closed")?;
        self.check_data_plane(
            |plane| match plane {
                DataPlane::Mp2t(_) => None,
                DataPlane::H264(..) => Some(h264_session_error()),
            },
            "RtspSession.into_demux_receiver: already consumed",
        )
        .map_err(|e| raise(py, &RTSP, e))?;
        // Validate before the take: a refused config must not consume the
        // data plane.
        let opts = match demux_config {
            None => None,
            Some(cfg) => Some(crate::mpegts::build_demuxer_config(py, cfg)?),
        };
        let session = self
            .take_data_plane(
                |plane| match plane {
                    DataPlane::Mp2t(session) => Ok(session),
                    DataPlane::H264(..) => Err((plane, h264_session_error())),
                },
                "RtspSession.into_demux_receiver: already consumed",
            )
            .map_err(|e| raise(py, &RTSP, e))?;
        // Convert to RtpRecvTransport. The session's `into_recv_transport`
        // panics only on internal-state-corruption shapes that can't
        // happen on a SETUP-succeeded path; we don't catch_unwind here
        // because the upstream invariants are stable. If a future
        // tst-rtp release flips this to fallible we'll switch to
        // map_err.
        let transport = session.into_recv_transport();
        let rtp_stats = Some(transport.stats_handle());
        let opened = crate::shells::open::OpenedRecv {
            opened: crate::rtp::transport::opened_recv(transport),
            rtp_stats,
        };
        let client: crate::shells::demux_receiver::KeepAlive = self.client.clone();
        PyDemuxReceiver::from_recv(py, opened, opts).map(|rx| rx.keeping_alive(client))
    }

    /// Consume the session's data plane and wrap it in an `H264Receiver`
    /// for iterating reassembled H.264 Access Units.
    ///
    /// Raises `RtspError(CLOSED)` when the data plane has already been
    /// consumed (by either `into_*` method) or `close()` has begun, and
    /// `RtspError(PROTOCOL)` when
    /// a fresh session was created via `connect()` (not `connect_h264()`) —
    /// that refusal leaves the data plane in place.
    ///
    /// The control-plane methods (`pause` / `play` / `teardown` /
    /// `cancel_handle`) remain usable after the call — only the data-plane
    /// `RtspSession` (the inner Rust value) is consumed. The receiver holds
    /// the session's client, so collecting this session object does not end
    /// it; `close()` here does.
    ///
    /// Handle is zeroed BEFORE the fallible native work (prevents a double
    /// free if the construction raises).
    #[allow(clippy::wrong_self_convention)]
    fn into_h264_receiver(&self, py: Python<'_>) -> PyResult<PyH264Receiver> {
        // A closed session or a consumed data plane is CLOSED, before the
        // wrong-constructor check could say PROTOCOL; a refused plane stays.
        self.refuse_if_closed("RtspSession.into_h264_receiver: the session is closed")?;
        let (session, depay_config) = self
            .take_data_plane(
                |plane| match plane {
                    DataPlane::H264(session, cfg) => Ok((session, cfg)),
                    DataPlane::Mp2t(_) => Err((
                        plane,
                        BindingError::new(
                            BindingErrorKind::RtspProtocol,
                            "RtspSession.into_h264_receiver: session was not created by \
                             connect_h264()",
                        ),
                    )),
                },
                "RtspSession.into_h264_receiver: data plane already consumed",
            )
            .map_err(|e| raise(py, &RTSP, e))?;

        let receiver = session.into_h264_receiver(depay_config);
        Ok(PyH264Receiver::from_h264_receiver(
            receiver,
            Some(self.client.clone()),
        ))
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// `close()` on context-manager exit: silent (the contract is
    /// "ensure session closed", not "fail if server is uncooperative") —
    /// call `.teardown()` first to get the typed error.
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: &Bound<'_, PyAny>,
        exc_value: &Bound<'_, PyAny>,
        traceback: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let _ = (exc_type, exc_value, traceback);
        self.close(py);
        Ok(false) // do not suppress exceptions
    }

    /// Returns true once `teardown` / `close` / `__exit__` has fired.
    fn is_torn_down(&self) -> bool {
        self.torn_down.load(Ordering::Relaxed)
    }
}

impl PyRtspSession {
    fn new(
        client: RustRtspClient,
        session: RustRtspSession,
        h264_depay_config: Option<H264DepayConfig>,
    ) -> Self {
        let data_plane = match h264_depay_config {
            Some(cfg) => DataPlane::H264(session, cfg),
            None => DataPlane::Mp2t(session),
        };
        Self {
            rtsp_cancel: client.cancel_handle(),
            client: Arc::new(Mutex::new(Some(client))),
            closed: AtomicBool::new(false),
            data_plane: Mutex::new(Some(data_plane)),
            torn_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run a control request on the client with the GIL released; a
    /// closed session (the client taken) is `RtspError(CLOSED)`.
    fn control(
        &self,
        py: Python<'_>,
        closed: &'static str,
        request: impl FnOnce(&mut RustRtspClient) -> Result<(), RustRtspError> + Send,
    ) -> PyResult<()> {
        let client = self.client.clone();
        let result = crate::util::allow_threads_parking(py, move || -> Result<(), BindingError> {
            let mut guard = client.lock().map_err(|_| poisoned())?;
            match guard.as_mut() {
                Some(c) => request(c).map_err(BindingError::from),
                None => Err(BindingError::new(BindingErrorKind::Closed, closed)),
            }
        });
        result.map_err(|e| raise(py, &RTSP, e))
    }

    /// `RtspError(CLOSED)` once `close()` has begun: a receiver taken after
    /// it would end at once (TCP-interleaved) or wait for packets that
    /// cannot come (UDP). Reads only the flag, so no guard is alive when
    /// the exception is built.
    fn refuse_if_closed(&self, message: &str) -> PyResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(rtsp_err_no_gil(BindingErrorKind::Closed, message));
        }
        Ok(())
    }

    /// Refuse a take without taking anything: CLOSED when the data plane
    /// is gone, else what `refusal` says of it.
    fn check_data_plane(
        &self,
        refusal: impl FnOnce(&DataPlane) -> Option<BindingError>,
        consumed: &str,
    ) -> Result<(), BindingError> {
        let guard = self.data_plane.lock().map_err(|_| poisoned())?;
        match guard.as_ref() {
            None => Err(BindingError::new(BindingErrorKind::Closed, consumed)),
            Some(plane) => refusal(plane).map_or(Ok(()), Err),
        }
    }

    /// Take the data plane when `wanted` accepts it, deciding and taking
    /// under one lock; a refused plane stays. `Err` carries the refusal,
    /// built into an exception by the caller once the guard is gone.
    fn take_data_plane<T>(
        &self,
        wanted: impl FnOnce(DataPlane) -> Result<T, (DataPlane, BindingError)>,
        consumed: &str,
    ) -> Result<T, BindingError> {
        let mut guard = self.data_plane.lock().map_err(|_| poisoned())?;
        match guard.take() {
            None => Err(BindingError::new(BindingErrorKind::Closed, consumed)),
            Some(plane) => wanted(plane).map_err(|(plane, e)| {
                *guard = Some(plane);
                e
            }),
        }
    }
}

/// `raise` needs a `Python` token; this helper serves the sites that have
/// none in scope (e.g. `refuse_if_closed` returning a typed error inside
/// a `?` chain before `Python::attach`) by re-acquiring the GIL.
fn rtsp_err_no_gil(kind: BindingErrorKind, message: &str) -> PyErr {
    Python::attach(|py| raise(py, &RTSP, BindingError::new(kind, message)))
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
    #[cfg(feature = "py-test-hooks")]
    m.add_function(wrap_pyfunction!(poison_for_test, m)?)?;
    Ok(())
}

/// `_rtsp_session_poison_for_test(session)`: panic while holding each of
/// the session's locks, as a bug in a call would, so both are poisoned.
/// Test-only (`py-test-hooks`), never in a published wheel.
///
/// The client lock is taken with the GIL released: `pause` / `play` /
/// `teardown` hold it across their network exchange without the GIL.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_rtsp_session_poison_for_test")]
fn poison_for_test(py: Python<'_>, session: &Bound<'_, PyRtspSession>) {
    let s = session.get();
    crate::util::allow_threads_parking(py, || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = s.client.lock();
            panic!("_rtsp_session_poison_for_test");
        }));
    });
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = s.data_plane.lock();
        panic!("_rtsp_session_poison_for_test");
    }));
}

// Hand-written so a `{:?}` never prints the password.
impl std::fmt::Debug for PyBasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyBasicAuth")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("realm", &self.realm)
            .finish()
    }
}

// Hand-written so a `{:?}` never prints the password.
impl std::fmt::Debug for PyDigestAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyDigestAuth")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("algorithm", &self.algorithm)
            .field("realm", &self.realm)
            .finish()
    }
}

// Hand-written so a `{:?}` never prints a URL password or secret value.
impl std::fmt::Debug for PyRtspClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyRtspClientConfig")
            .field("url", &redact_url(&self.url))
            .field("auth", &self.auth)
            .field("transport_pref", &self.transport_pref)
            .field(
                "tls_root_certs_pem_len",
                &self.tls_root_certs_pem.as_ref().map(Vec::len),
            )
            .field("keepalive", &self.keepalive)
            .field("rtsp_version", &self.rtsp_version)
            .finish()
    }
}
