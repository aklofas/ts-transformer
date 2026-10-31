//! `org.tstrans.rtp` + `org.tstrans` (RTSP) Rust→Java error mapping — typed
//! entry points over [`crate::error::throw_binding`]; the kind table is
//! `tst_pipeline::binding::BindingErrorKind` and the Java members carry its
//! `name()`s (verified at load by `NativeLoader.nVerifyKinds`).
//!
//! This file holds no mapping tables of its own: the rtp and
//! rtsp buckets are the shared `From` impls in `crates/tst-rtp/src/binding_kind.rs`.

use jni::Env;
use tst_core::transport::TransportError;
use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_rtp::error::{MountError, RtspServerError};
use tst_rtp::{MountRuleError, RtspError};

use crate::error::{Domain, throw_binding};

/// `org.tstrans.RtpException(Kind.<kind.name()>, message)`.
pub(crate) fn throw_rtp(env: &mut Env, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Rtp, &BindingError::new(kind, message));
}

/// `send_bytes` / `recv_bytes` / shell recv errors — the shared `From<TransportError>`:
/// `ExplicitClose → CLOSED` ("cancelled from another thread"),
/// `Backpressure → BACKPRESSURE` (the `?recv_timeout=` deadline),
/// `TooLarge → TOO_LARGE`, `Broken → BROKEN`, `Closed → CLOSED`.
pub(crate) fn transport_error(env: &mut Env, e: &TransportError) {
    throw_binding(env, Domain::Rtp, &BindingError::from(e.clone()));
}

/// `RtpUrl::parse` failures
/// (`RtpUrlError`) — there is no bare `From<RtpUrlError>` (the type is shared with
/// `rtsp://` parsing), so the bucket is fixed here: `URL` (was `TRANSPORT`).
pub(crate) fn rtp_url_error(env: &mut Env, e: &tst_rtp::RtpUrlError) {
    throw_rtp(env, BindingErrorKind::RtpUrl, &e.to_string());
}

/// `RtpTransport::connect_with` / `RtpRecvTransport::listen_with` / `H264Receiver::listen`
/// failures — the shared `From<tst_rtp::RtpConnectError>`: one member per variant
/// (`PAYLOAD_TYPE_PARAM` / `MISSING_PAYLOAD_TYPE_PARAM` / `URL` /
/// `HOST_NOT_LITERAL` / `IO` / `IFACE_UNSUPPORTED`; all were `TRANSPORT`).
pub(crate) fn connect_error(env: &mut Env, e: tst_rtp::RtpConnectError) {
    throw_binding(env, Domain::Rtp, &BindingError::from(e));
}

/// `org.tstrans.RtspException(Kind.<kind.name()>, message)`.
pub(crate) fn throw_rtsp(env: &mut Env, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Rtsp, &BindingError::new(kind, message));
}

/// RTSP client errors — the shared `From<RtspError>`. Two buckets move vs the table
/// this file carried before 0.7.0: `AuthUnsupported` → `AUTH_REQUIRED` (was
/// `AUTH_FAILED`) and the four SDP-media errors (`NoMp2tMedia` /
/// `MultipleMp2tMedia` / `NoH264Media` / `MultipleH264Media`) → `NOT_FOUND`
/// (was `MOUNT`). Everything else keeps its bucket.
pub(crate) fn rtsp_error_to_jvm(env: &mut Env, e: RtspError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}

/// RTSP server errors — the shared `From<RtspServerError>` (buckets unchanged).
pub(crate) fn server_error_to_jvm(env: &mut Env, e: RtspServerError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}

/// Mount push errors — the shared `From<MountError>` (`Mux(_)` | `Closed` → `MOUNT`,
/// as before). NOTE: this DIFFERS from the `MuxSender`, whose `Mux(...)` is a
/// `MuxException` — `MountHandle` pushes are `MOUNT`.
pub(crate) fn mount_error_to_jvm(env: &mut Env, e: MountError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}

/// Mount-rule edit errors — the shared `From<MountRuleError>`: an invalid path or
/// empty credentials → `MOUNT`, a stopped server → `SERVER`.
pub(crate) fn mount_rule_error_to_jvm(env: &mut Env, e: MountRuleError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}
