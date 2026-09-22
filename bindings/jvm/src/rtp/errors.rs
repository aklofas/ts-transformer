//! `org.tstrans.rtp` + `org.tstrans` (RTSP) Rust→Java error mapping — typed
//! entry points over [`crate::error::throw_binding`]; the kind table is
//! `tst_pipeline::binding::BindingErrorKind` and the Java members carry its
//! `name()`s (verified at load by `NativeLoader.nVerifyKinds`).
//!
//! Since Arc 2 WP-B3 this file holds no mapping tables of its own: the rtp and
//! rtsp buckets are A2's `From` impls in `crates/tst-rtp/src/binding_kind.rs`.

use jni::JNIEnv;
use tst_core::transport::TransportError;
use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_rtp::RtspError;
use tst_rtp::error::{MountError, RtspServerError};

use crate::error::{Domain, throw_binding};

/// `org.tstrans.RtpException(Kind.<kind.name()>, message)`.
pub(crate) fn throw_rtp(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Rtp, &BindingError::new(kind, message));
}

/// `send_bytes` / `recv_bytes` / shell recv errors — A2's `From<TransportError>`:
/// `ExplicitClose → CLOSED` ("cancelled from another thread"; was `CANCELLED`),
/// `Backpressure → BACKPRESSURE` (the `?recv_timeout=` deadline; was `TIMEOUT`),
/// `TooLarge → TOO_LARGE` (was `MALFORMED_PACKET`), `Broken → BROKEN`,
/// `Closed → CLOSED` (both were `TRANSPORT`).
pub(crate) fn transport_error(env: &mut JNIEnv, e: &TransportError) {
    throw_binding(env, Domain::Rtp, &BindingError::from(e.clone()));
}

/// `RtpSocketBuilder::from_url` / `RtpRecvSocketBuilder::from_url` failures
/// (`RtpUrlError`) — A2 has no bare `From<RtpUrlError>` (the type is shared with
/// `rtsp://` parsing), so the bucket is fixed here: `URL` (was `TRANSPORT`).
pub(crate) fn rtp_url_error(env: &mut JNIEnv, e: &tst_rtp::RtpUrlError) {
    throw_rtp(env, BindingErrorKind::RtpUrl, &e.to_string());
}

/// `RtpSocketBuilder::build` / `RtpRecvSocketBuilder::build` / `H264Receiver::listen`
/// failures — A2's `From<tst_rtp::ConnectError>`: one member per variant
/// (`PAYLOAD_TYPE_PARAM` / `MISSING_PAYLOAD_TYPE_PARAM` / `URL` /
/// `HOST_NOT_LITERAL` / `IO` / `IFACE_UNSUPPORTED`; all were `TRANSPORT`).
pub(crate) fn connect_error(env: &mut JNIEnv, e: tst_rtp::ConnectError) {
    throw_binding(env, Domain::Rtp, &BindingError::from(e));
}

/// `org.tstrans.RtspException(Kind.<kind.name()>, message)`.
pub(crate) fn throw_rtsp(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Rtsp, &BindingError::new(kind, message));
}

/// RTSP client errors — A2's `From<RtspError>`. Two buckets move vs the table
/// this file carried before 0.7.0: `AuthUnsupported` → `AUTH_REQUIRED` (was
/// `AUTH_FAILED`) and the four SDP-media errors (`NoMp2tMedia` /
/// `MultipleMp2tMedia` / `NoH264Media` / `MultipleH264Media`) → `NOT_FOUND`
/// (was `MOUNT`). Everything else keeps its bucket.
pub(crate) fn rtsp_error_to_jvm(env: &mut JNIEnv, e: RtspError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}

/// RTSP server errors — A2's `From<RtspServerError>` (buckets unchanged).
pub(crate) fn server_error_to_jvm(env: &mut JNIEnv, e: RtspServerError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}

/// Mount push errors — A2's `From<MountError>` (`Mux(_)` | `Closed` → `MOUNT`,
/// as before). NOTE: this DIFFERS from the `MuxSender`, whose `Mux(...)` is a
/// `MuxException` — `MountHandle` pushes are `MOUNT`.
pub(crate) fn mount_error_to_jvm(env: &mut JNIEnv, e: MountError) {
    throw_binding(env, Domain::Rtsp, &BindingError::from(e));
}
