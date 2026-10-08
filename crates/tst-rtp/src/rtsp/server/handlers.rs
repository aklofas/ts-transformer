//! RTSP request handlers — dispatched from `session.rs`'s per-session
//! state machine. Shared helpers (`server_header`, `error_response`,
//! `challenge_response`), OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE,
//! TEARDOWN, and GET_PARAMETER are all fully implemented. ANNOUNCE and
//! RECORD (publisher direction) are implemented in
//! `super::publish::handlers`; `handle_setup` routes a `mode=record`
//! SETUP there too, and `handle_teardown`/`handle_pause` have publisher
//! arms below.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::Bytes;

use crate::rtsp::message::{RtspRequest, RtspResponse};
use crate::rtsp::server::ServerState;
use crate::rtsp::server::auth::{AuthVerifyError, build_challenge_header, verify_authorization};
use crate::rtsp::server::mount::MountEntry;
use crate::rtsp::server::session::ServerSessionState;

/// `Server:` header value for outbound responses.
pub(crate) fn server_header() -> String {
    format!("tst-rtp/{}", env!("CARGO_PKG_VERSION"))
}

/// Build a 4xx/5xx error response with copied CSeq + Server header.
pub(crate) fn error_response(req: &RtspRequest, status: u16, reason: &str) -> RtspResponse {
    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    RtspResponse {
        version: req.version,
        status,
        reason: reason.into(),
        headers,
        body: Bytes::new(),
    }
}

/// 455 for a method that is valid only in a state this session is not in.
pub(crate) fn handle_not_valid_in_state(req: &RtspRequest) -> RtspResponse {
    error_response(req, 455, "Method Not Valid in This State")
}

/// Build a 401 Unauthorized response with a fresh `WWW-Authenticate`
/// challenge for the configured auth scheme. Mutates `session.auth_nonce`
/// if `stale = true` (rotates the nonce for a stale-challenge retry per
/// RFC 7616 §3.5). Also resets `session.auth_nc_hwm` to 0 when the
/// nonce is rotated so nc tracking starts fresh for the new nonce.
pub(crate) fn challenge_response(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
    stale: bool,
) -> RtspResponse {
    let mut response = error_response(req, 401, "Unauthorized");
    if let Some(auth_cfg) = state.builder.auth.as_ref() {
        if stale {
            // Rotate the nonce so the client's next attempt uses a
            // fresh one. Reset the nc hwm so replay tracking begins
            // fresh under the new nonce.
            session.auth_nonce = crate::rtsp::server::auth::generate_nonce();
            session.auth_nc_hwm = 0;
        }
        let challenge = build_challenge_header(auth_cfg, &session.auth_nonce);
        response
            .headers
            .insert("www-authenticate".into(), challenge);
    }
    response
}

/// Verify auth for an authenticated request. Returns `Ok(())` if auth
/// is either not required or successfully verified; returns the
/// challenge response (401) if auth is required and failed.
pub(crate) fn check_auth(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
    method_str: &str,
) -> Result<(), RtspResponse> {
    let Some(auth_cfg) = state.builder.auth.as_ref() else {
        return Ok(()); // No auth configured — allow.
    };
    let auth_header = req.headers.get("authorization").map(String::as_str);
    match verify_authorization(
        auth_header,
        method_str,
        &req.uri,
        auth_cfg,
        &session.auth_nonce,
        &mut session.auth_nc_hwm,
    ) {
        Ok(()) => Ok(()),
        Err(AuthVerifyError::StaleNonce) => {
            // Surface stale=true so the client knows to retry with the
            // new nonce instead of treating it as a credentials error.
            Err(challenge_response(req, state, session, true))
        }
        Err(_) => Err(challenge_response(req, state, session, false)),
    }
}

/// OPTIONS handler — RFC 7826 §10.1 / RFC 2326 §10.1.
///
/// Returns the list of supported methods in the `Public:` header.
/// OPTIONS itself never requires auth (it's the connectivity probe).
#[allow(unused_variables)]
pub(crate) fn handle_options(req: &RtspRequest, state: &Arc<ServerState>) -> RtspResponse {
    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    headers.insert(
        "public".into(),
        "OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, RECORD, PAUSE, TEARDOWN, GET_PARAMETER".into(),
    );
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

/// DESCRIBE handler — RFC 7826 §10.2.
///
/// 1. Auth check (401 if missing/wrong/stale).
/// 2. Extract mount path from request URI; look up in state.mounts.
/// 3. 404 if mount not registered.
/// 4. 200 OK with `Content-Type: application/sdp` + SDP body from
///    `Sdp::build_for_mount`.
pub(crate) fn handle_describe(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(challenge) = check_auth(req, state, session, "DESCRIBE") {
        return challenge;
    }

    // Extract mount path. URI may be a full rtsp://host/path or a
    // path-only "/path" form (some cameras send relative URIs).
    let mount_path = extract_mount_path(&req.uri);
    let mounts = match state.mounts.lock() {
        Ok(m) => m,
        Err(_) => return error_response(req, 500, "Internal Server Error"),
    };
    let mount = match mounts.get(&mount_path) {
        Some(m) => m.clone(),
        None => {
            // Mount not found: return 404.
            return error_response(req, 404, "Not Found");
        }
    };
    drop(mounts);

    let kind_is_multicast = mount.is_multicast();
    let multicast_addr = mount.multicast_group();

    let local_addr = match *state.local_addr.lock().unwrap() {
        Some(addr) => addr,
        None => return error_response(req, 500, "Internal Server Error"),
    };
    let connection_addr = if kind_is_multicast {
        multicast_addr.unwrap_or(local_addr)
    } else {
        local_addr
    };
    let body = crate::sdp::Sdp::build_for_mount(&mount_path, connection_addr, kind_is_multicast);

    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    headers.insert("content-type".into(), "application/sdp".into());
    headers.insert("content-length".into(), body.len().to_string());
    // RFC 7826 §D: Content-Base anchors relative a=control: URIs in the SDP
    // body so third-party clients (VLC, ffplay) resolve SETUP URLs correctly.
    // The value is the request URI with a trailing slash appended.
    let content_base = {
        let uri = req.uri.trim_end_matches('/');
        format!("{uri}/")
    };
    headers.insert("content-base".into(), content_base);
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body,
    }
}

/// Extract the path component from a request URI. Handles both full
/// `rtsp://host:port/path` and path-only `/path` forms.
///
/// Per-media control URLs like `a=control:trackID=0` from the SDP cause
/// `RtspClient::setup_mp2t_auto` to append `/trackID=N` to the SETUP
/// URI. We strip the trailing per-media segment if it's recognized
/// (RFC 7826 §C.1.1 trackID convention) — otherwise the mount lookup
/// misses the registered base path.
///
/// A trailing `/` is also stripped (except on the bare root `"/"`) so
/// that `rtsp://host/live/` and `rtsp://host/live` both resolve to the
/// same mount registered at `"/live"`. Without this, a client that
/// appends a trailing slash to the DESCRIBE URI would get a 404 for a
/// perfectly valid mount. DESCRIBE, SETUP, and PLAY all go through this
/// function, so the normalization is consistent across the whole session.
pub(crate) fn extract_mount_path(uri: &str) -> String {
    normalize_mount_path(uri, |seg| {
        seg.starts_with("trackID=") || seg.starts_with("streamid=") || seg.starts_with("stream=")
    })
}

/// Shared normalization for [`extract_mount_path`] and the publisher
/// SETUP path's announced-`a=control`-aware mount-path derivation (see
/// [`crate::rtsp::server::publish::handlers::handle_setup_record`]):
/// strip scheme + authority, strip a trailing query string, then strip
/// the last `/`-delimited path segment when `strip_last` says it's a
/// per-media control segment (not part of the registered mount path)
/// rather than part of the mount path itself, then normalize away a
/// single trailing slash.
fn normalize_mount_path(uri: &str, strip_last: impl FnOnce(&str) -> bool) -> String {
    // Strip scheme + authority if present.
    let path_start = if let Some(after_scheme) = uri.strip_prefix("rtsp://") {
        after_scheme
            .find('/')
            .map(|idx| &after_scheme[idx..])
            .unwrap_or("/")
    } else if let Some(after_scheme) = uri.strip_prefix("rtsps://") {
        after_scheme
            .find('/')
            .map(|idx| &after_scheme[idx..])
            .unwrap_or("/")
    } else {
        uri
    };
    // Strip any trailing query string; v1 matches the bare path component
    // of the mount as registered.
    let path = path_start.split('?').next().unwrap_or(path_start);
    // Strip a trailing per-media control segment, per `strip_last`. SDP
    // `a=control:trackID=N` (the canonical form per RFC 7826 §C.1.1), or
    // `a=control:stream=N` (gst-rtsp-server's convention), causes a SETUP
    // URI of `<base>/trackID=N` or `<base>/stream=N`. SETUP matches
    // against the base mount path; the control segment is per-media and
    // not part of the registered mount.
    let path = if let Some(last_slash) = path.rfind('/') {
        let last_seg = &path[last_slash + 1..];
        if strip_last(last_seg) {
            // Keep the leading slash if it's the only one (root mount)
            // by ensuring we don't strip to empty.
            if last_slash == 0 {
                "/"
            } else {
                &path[..last_slash]
            }
        } else {
            path
        }
    } else {
        path
    };
    // Normalize: strip a single trailing '/' unless the path is just "/".
    // This makes "/live/" and "/live" resolve to the same registered mount.
    let path = if path.len() > 1 && path.ends_with('/') {
        &path[..path.len() - 1]
    } else {
        path
    };
    path.to_string()
}

/// The raw last `/`-delimited path segment of a request URI (scheme and
/// authority, if present, are irrelevant — `rsplit('/')` finds the same
/// trailing chunk either way), with no prefix check at all. `None` for
/// an empty segment (e.g. a URI ending in `/`). Shared by
/// [`control_segment`] (which additionally requires a recognized
/// prefix) and the publisher SETUP path's exact-match-against-announced-
/// `a=control` resolution, which needs the raw text regardless of
/// whether it looks like `trackID=`/`streamid=`/`stream=`.
pub(crate) fn last_path_segment(uri: &str) -> Option<&str> {
    let path = uri.split('?').next().unwrap_or(uri);
    let last = path.rsplit('/').next()?;
    (!last.is_empty()).then_some(last)
}

/// The trailing per-media control segment of a SETUP URI, if any
/// (`streamid=0`, `trackID=1`, `stream=0`). Used by the publisher SETUP
/// path to resolve which announced track a given SETUP targets when the
/// exact-match-against-announced-control resolution
/// (`PublishSession::track_for_raw_segment`) doesn't apply —
/// [`extract_mount_path`] strips the same recognized segment when it
/// locates the mount itself, so the two must agree on what counts as
/// one. This function only recognizes the three fixed prefixes above;
/// an announced control using any other convention is resolved via the
/// exact-match path instead; this one never inspects the announced
/// value itself, so it has no notion of an absolute `a=control` URL.
pub(crate) fn control_segment(uri: &str) -> Option<&str> {
    let last = last_path_segment(uri)?;
    (last.starts_with("trackID=") || last.starts_with("streamid=") || last.starts_with("stream="))
        .then_some(last)
}

/// For a session that has already announced a publisher on this
/// connection, try resolving `uri`'s mount path by matching its raw
/// trailing path segment against this session's announced `a=control`
/// values with an exact string match — handles any control-naming
/// convention (gst-rtsp-server's `rtspclientsink` announces bare
/// `a=control:stream=0`; other tools may use yet other text) that
/// [`extract_mount_path`]'s fixed `trackID=`/`streamid=`/`stream=`
/// prefix list doesn't recognize. `None` when this session isn't a
/// publisher yet, or when nothing it announced matches the URI's
/// trailing segment — the caller falls back to [`extract_mount_path`]
/// in both cases.
pub(crate) fn publisher_mount_path(uri: &str, session: &ServerSessionState) -> Option<String> {
    let publish = session.publish.as_ref()?;
    let last = last_path_segment(uri)?;
    publish.track_for_raw_segment(last)?;
    Some(normalize_mount_path(uri, |seg| seg == last))
}

/// Allocate a fresh even/odd TCP-interleaved channel pair from the
/// process-global allocator shared by the reader (this module's
/// `handle_setup`) and publisher (`publish::handlers::handle_setup_record`)
/// SETUP paths. A single shared counter across both directions is safe
/// because channels are scoped per RTSP session on the wire — each
/// client's TCP connection has its own interleaved namespace (see the
/// allocator's original call site) — so there is no cross-session
/// collision to avoid, only one allocator to avoid duplicating.
///
/// `None` on exhaustion (the companion channel would overflow `u8`);
/// callers map that to 500 — a server-side allocator exhaustion, not a
/// client error.
pub(crate) fn next_interleaved_pair() -> Option<(u8, u8)> {
    static NEXT_CHANNEL: AtomicU8 = AtomicU8::new(0);
    let base = NEXT_CHANNEL.fetch_add(2, Ordering::Relaxed);
    base.checked_add(1).map(|companion| (base, companion))
}

/// SETUP handler — RFC 7826 §10.4 / RFC 2326 §12.10. Auth-gated.
/// Allocates per-session transport (UDP socket pair or interleaved
/// channel pair); returns 200 with Session + Transport response.
///
/// A `mode=record` SETUP against a publish mount is routed to
/// [`super::publish::handlers::handle_setup_record`] instead — see the
/// `(MountEntry, is_record)` match right after the Transport header is
/// parsed, below.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails (via `check_auth`).
/// - 404 Not Found — mount path not registered.
/// - 400 Bad Request — Transport header missing or malformed.
/// - 461 Unsupported Transport — TCP-interleaved against a multicast
///   mount (RFC 7826 §13.3), or `mode=record` against a local
///   (muxer-backed) mount — only a publish mount accepts a publisher.
/// - 500 Internal Server Error — UDP socket bind failure, interleaved
///   channel allocator exhaustion, or poisoned mutex.
///
/// On 200, mutates `session` with: `session_id`, `mount_path`,
/// `transport`, and either `udp_sockets` (unicast UDP) or
/// `interleaved_channels` (TCP-interleaved). Multicast SETUP responses
/// reuse the per-mount sender task so no per-session socket pair
/// is allocated; the response just points the client at the multicast
/// group.
pub(crate) fn handle_setup(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(challenge) = check_auth(req, state, session, "SETUP") {
        return challenge;
    }

    // Look up the mount. A session that has already announced a
    // publisher gets first crack at resolving the path via its own
    // announced `a=control` values (handles non-standard control
    // conventions like gst-rtsp-server's bare `stream=N`); anything else
    // (a pre-ANNOUNCE SETUP, a reader, or a segment that doesn't match
    // what this session announced) falls back to the fixed-prefix form.
    let mount_path =
        publisher_mount_path(&req.uri, session).unwrap_or_else(|| extract_mount_path(&req.uri));
    let mounts = match state.mounts.lock() {
        Ok(m) => m,
        Err(_) => return error_response(req, 500, "Internal Server Error"),
    };
    let mount = match mounts.get(&mount_path) {
        Some(m) => m.clone(),
        None => return error_response(req, 404, "Not Found"),
    };
    drop(mounts);

    // Parse the Transport request header. The wire response stays a bare
    // 400 either way (a client gains nothing from the distinction); the
    // debug lines exist so a server operator can tell a missing header
    // from a malformed one — the parse layer's own debug line (see
    // `parse_u16_pair`) names which specific check failed.
    let transport_str = match req.headers.get("transport") {
        Some(t) => t.as_str(),
        None => {
            tracing::debug!(
                target: "tst_rtp::server",
                "SETUP rejected 400: no Transport header"
            );
            return error_response(req, 400, "Bad Request");
        }
    };
    let parsed =
        match crate::rtsp::client::transport_negotiation::parse_transport_response(transport_str) {
            Ok(p) => p,
            Err(_) => {
                // Debug-capture (`?`): the header is untrusted wire input;
                // escape control characters instead of logging them raw.
                tracing::debug!(
                    target: "tst_rtp::server",
                    transport = ?transport_str,
                    "SETUP rejected 400: malformed Transport header"
                );
                return error_response(req, 400, "Bad Request");
            }
        };

    // Route a publisher's SETUP (`mode=record`) to the publish handlers;
    // a reader's SETUP (no `mode=`, or any value other than "record")
    // falls through to the existing unicast/multicast allocation below —
    // `MountEntry`'s accessors (`fanout`/`is_multicast`/...) already
    // treat a publish mount and a local mount uniformly for readers, so
    // a publish-mount reader needs no special casing past this match.
    let is_record = parsed.mode.as_deref() == Some("record");
    match (&mount, is_record) {
        (MountEntry::Publish(m), true) => {
            return crate::rtsp::server::publish::handlers::handle_setup_record(
                req,
                state,
                session,
                &parsed,
                &mount_path,
                m,
            );
        }
        // A local (muxer-backed) mount never accepts a publisher.
        (MountEntry::Local(_), true) => {
            return error_response(req, 461, "Unsupported Transport");
        }
        (MountEntry::Publish(_), false) | (MountEntry::Local(_), false) => {}
    }

    // Per RFC 7826 §13.3: TCP-interleaved is incompatible with multicast.
    let is_multicast = mount.is_multicast();
    if is_multicast
        && matches!(
            parsed.kind,
            crate::rtsp::client::transport_negotiation::RtspTransportKind::TcpInterleaved
        )
    {
        return error_response(req, 461, "Unsupported Transport");
    }

    // Allocate transport-specific server state + build the response
    // header.
    use crate::rtsp::client::transport_negotiation::RtspTransportKind;
    let session_id = generate_session_id();
    let transport_response_header: String;
    match parsed.kind {
        RtspTransportKind::Udp => {
            if is_multicast {
                // Multicast SETUP: server points the client at the
                // group; the per-mount multicast sender task is
                // already publishing there.
                let (group, ttl) = match (mount.multicast_group(), mount.multicast_ttl()) {
                    (Some(group), Some(ttl)) => (group, ttl),
                    _ => unreachable!("multicast path gated above"),
                };
                // Guard: group port 65535 has no valid RTCP companion
                // port (65536 overflows u16). Emit a single-port range
                // in that case rather than wrapping to 0. Mirrors the
                // guard at handlers.rs bind_server_udp_pair (rtp_port ==
                // u16::MAX) and the transport.rs companion-port fallbacks.
                let port_range = match group.port().checked_add(1) {
                    Some(rtcp_port) => format!("{}-{}", group.port(), rtcp_port),
                    None => group.port().to_string(),
                };
                transport_response_header = format!(
                    "RTP/AVP;multicast;destination={};port={};ttl={}",
                    group.ip(),
                    port_range,
                    ttl,
                );
            } else {
                // Unicast UDP REQUIRES a valid client_port pair — the
                // server fans RTP out to client_port.0 (RTP) and RTCP to
                // client_port.1. A missing client_port can't be silently
                // defaulted to 0-0 (that would point the fan-out at
                // privileged port 0). Reject with 461 Unsupported
                // Transport. (Malformed/reversed/out-of-range pairs are
                // already rejected as 400 by parse_transport_response
                // above; this guards the *absent* case.)
                let client_port = match parsed.client_port {
                    Some(cp) => cp,
                    None => return error_response(req, 461, "Unsupported Transport"),
                };
                // Unicast UDP: bind a server-side RTP+RTCP port pair.
                let local_ip = match *state.local_addr.lock().unwrap() {
                    Some(addr) => addr.ip(),
                    None => return error_response(req, 500, "Internal Server Error"),
                };
                let (rtp_sock, rtcp_sock, server_rtp_port) = match bind_server_udp_pair(local_ip) {
                    Ok(t) => t,
                    Err(_) => return error_response(req, 500, "Internal Server Error"),
                };
                // bind_server_udp_pair already guarantees server_rtp_port
                // != u16::MAX, so the companion can't overflow; use
                // checked_add defensively (the +1 companion-port bug
                // class) and fail closed if that invariant ever changes.
                let server_rtcp_port = match server_rtp_port.checked_add(1) {
                    Some(p) => p,
                    None => return error_response(req, 500, "Internal Server Error"),
                };
                transport_response_header = format!(
                    "RTP/AVP;unicast;client_port={}-{};server_port={}-{}",
                    client_port.0, client_port.1, server_rtp_port, server_rtcp_port,
                );
                session.udp_sockets = Some((rtp_sock, rtcp_sock));
            }
        }
        RtspTransportKind::TcpInterleaved => {
            // Allocate a fresh even/odd channel pair from the shared
            // allocator (also used by the publisher SETUP path) — see
            // `next_interleaved_pair`'s doc for why one process-global
            // counter is safe across both directions. 500 on exhaustion:
            // a server-side allocator problem, not a client error.
            let Some((base, companion)) = next_interleaved_pair() else {
                return error_response(req, 500, "Internal Server Error");
            };
            session.interleaved_channels = Some((base, companion));
            transport_response_header =
                format!("RTP/AVP/TCP;unicast;interleaved={base}-{companion}");
        }
    }

    session.session_id = Some(session_id.clone());
    session.mount_path = Some(mount_path);
    session.transport = Some(parsed);

    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    headers.insert(
        "session".into(),
        format!(
            "{};timeout={}",
            session_id,
            state.builder.session_timeout.as_secs()
        ),
    );
    headers.insert("transport".into(), transport_response_header);
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

/// Generate a 16-hex-char session ID. Per RFC 7826 §17.3.2, session IDs
/// must be at least 8 characters; 16 hex is generous and avoids
/// collisions across concurrent SETUPs.
pub(crate) fn generate_session_id() -> String {
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_err() {
        // Fallback: timestamp-based. getrandom only fails on platforms
        // without an OS RNG, which we don't target.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        buf.copy_from_slice(&nanos.to_be_bytes());
    }
    let mut s = String::with_capacity(16);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Bind a fresh server-side UDP RTP+RTCP socket pair on `bind_ip`.
///
/// RTCP MUST live on RTP+1 per RFC 3550 §11; we let the kernel pick the
/// RTP port (passing port 0) and retry up to 16 times if the
/// neighboring odd port is already in use.
///
/// Returns `(rtp_socket, rtcp_socket, rtp_port)`. Each socket is wrapped
/// as `Arc<tokio::net::UdpSocket>` so it can be cloned into the
/// per-peer fan-out task.
pub(crate) fn bind_server_udp_pair(
    bind_ip: std::net::IpAddr,
) -> Result<
    (
        std::sync::Arc<tokio::net::UdpSocket>,
        std::sync::Arc<tokio::net::UdpSocket>,
        u16,
    ),
    std::io::Error,
> {
    for _attempt in 0..16 {
        let rtp_addr = SocketAddr::new(bind_ip, 0);
        let rtp_std = StdUdpSocket::bind(rtp_addr)?;
        rtp_std.set_nonblocking(true)?;
        let rtp_port = rtp_std.local_addr()?.port();
        // Avoid the (extremely rare) case where the kernel hands us a
        // port pair like (65535, 65536) where the +1 would wrap.
        let Some(rtcp_port) = rtp_port.checked_add(1) else {
            drop(rtp_std);
            continue;
        };
        let rtcp_addr = SocketAddr::new(bind_ip, rtcp_port);
        match StdUdpSocket::bind(rtcp_addr) {
            Ok(rtcp_std) => {
                rtcp_std.set_nonblocking(true)?;
                let rtp = tokio::net::UdpSocket::from_std(rtp_std)?;
                let rtcp = tokio::net::UdpSocket::from_std(rtcp_std)?;
                return Ok((
                    std::sync::Arc::new(rtp),
                    std::sync::Arc::new(rtcp),
                    rtp_port,
                ));
            }
            Err(_) => {
                // Neighboring RTCP slot taken; retry with a fresh RTP
                // port.
                drop(rtp_std);
                continue;
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "could not find a free RTP+RTCP port pair",
    ))
}

/// PLAY handler — RFC 7826 §10.5 / RFC 2326 §11.2.
///
/// Subscribes the session to the mount's broadcast fanout channel and
/// spawns the per-peer fanout task with the SETUP-allocated transport.
/// Invokes `spawn_peer_fanout` to wire the fan-out to the per-peer task.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails.
/// - 454 Session Not Found — `session.session_id` is None (PLAY before
///   SETUP), or the SETUP-allocated transport is missing for a unicast
///   mount.
/// - 404 Not Found — mount disappeared between SETUP and PLAY (rare).
/// - 500 Internal Server Error — mounts mutex poisoned, or required
///   UDP socket pair missing despite the transport being UDP.
///
/// On 200: returns Session + RTP-Info headers. For multicast mounts the
/// per-mount sender drives sends, so PLAY just confirms;
/// no per-peer task is spawned. For TCP-interleaved unicast the
/// per-session `OwnedWriteHalf` (populated by `handle_connection_inner`
/// after the TCP split) is cloned into `PeerTransport::Interleaved` so
/// the per-peer fanout task can write RFC 7826 §14 `$<channel><len>`
/// frames on the same TCP as the RTSP control responses; the
/// `AsyncMutex` around the write half serializes the two writers.
/// Returns the UDP target address for a unicast PLAY: preserves the
/// client's IP from the TCP control connection and substitutes the
/// RTP port that the client advertised in its SETUP `client_port` field.
///
/// Separating this from `handle_play` makes it directly unit-testable
/// without setting up a full `ServerState`.
pub(crate) fn compute_udp_play_target(peer: SocketAddr, client_rtp_port: u16) -> SocketAddr {
    SocketAddr::new(peer.ip(), client_rtp_port)
}
pub(crate) fn handle_play(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(challenge) = check_auth(req, state, session, "PLAY") {
        return challenge;
    }
    let Some(session_id) = session.session_id.clone() else {
        return error_response(req, 454, "Session Not Found");
    };
    let Some(mount_path) = session.mount_path.clone() else {
        return error_response(req, 454, "Session Not Found");
    };
    let mounts = match state.mounts.lock() {
        Ok(m) => m,
        Err(_) => return error_response(req, 500, "Internal Server Error"),
    };
    let mount = match mounts.get(&mount_path) {
        Some(m) => m.clone(),
        None => return error_response(req, 404, "Not Found"),
    };
    drop(mounts);

    // Multicast mounts: the per-mount sender already drives
    // sends to the group. PLAY just confirms; no per-peer task spawn.
    // seq/rtptime are not meaningful for a multicast mount (the server
    // has been sending since the mount was created); report zeros per
    // the shared-stream convention.
    let is_multicast = mount.is_multicast();
    if is_multicast {
        return play_response_ok(req, &session_id, 0, 0);
    }

    // Unicast PLAY: subscribe + spawn per-peer fanout.
    let Some(transport) = session.transport.clone() else {
        return error_response(req, 454, "Session Not Found");
    };
    use crate::rtsp::client::transport_negotiation::RtspTransportKind;
    let drop_counter =
        crate::rtsp::server::fanout::PeerDropCounter::with_mount_total(mount.frames_dropped());
    let rx = mount.fanout().subscribe();
    let peer_transport = match transport.kind {
        RtspTransportKind::Udp => {
            let Some((rtp_sock, _rtcp_sock)) = session.udp_sockets.clone() else {
                return error_response(req, 500, "Internal Server Error");
            };
            // SETUP (handle_setup) already rejected a UDP unicast without a
            // client_port (461). Defense-in-depth: never fall back to a
            // bogus port-0 fan-out target — fail closed if it's somehow
            // absent at PLAY time.
            let Some(client_port) = transport.client_port else {
                return error_response(req, 500, "Internal Server Error");
            };
            let peer_addr = compute_udp_play_target(session.peer_addr, client_port.0);
            crate::rtsp::server::fanout::PeerTransport::Udp {
                socket: rtp_sock,
                peer_addr,
            }
        }
        RtspTransportKind::TcpInterleaved => {
            // The per-session TCP was split in `handle_connection_inner`
            // and the write half stashed on `session.tcp_write`. Clone
            // the `Arc` so both the session's response writer and the
            // fanout task can share the underlying writer via the
            // `AsyncMutex` (serializes RTSP responses vs §14 interleaved
            // RTP frames). Plain `rtsp://` sessions always populate
            // `tcp_write`; `rtsps://` (TLS) sessions do NOT — the §14
            // interleaved fanout writer is typed to the plain TCP
            // `OwnedWriteHalf`, so TCP-interleaved PLAY over a TLS control
            // channel is not supported and returns 461 (rtsps:// control
            // plane + UDP-transport PLAY both work). Synthetic unit-test
            // sessions also leave it `None` but assert at SETUP.
            let Some(writer) = session.tcp_write.clone() else {
                return error_response(req, 461, "Unsupported Transport");
            };
            let (rtp_channel, _rtcp_channel) = session
                .interleaved_channels
                .expect("SETUP populated interleaved_channels for TcpInterleaved transport");
            crate::rtsp::server::fanout::PeerTransport::Interleaved {
                writer,
                rtp_channel,
            }
        }
    };
    // A second PLAY without PAUSE (RFC 7826 §13.4 allows it) replaces the
    // fanout; a detached old task would keep streaming to the same peer
    // with its own sequence space. Retire via
    // the token the fanout checks between frames — never `abort()`, which
    // could cut an interleaved frame mid-write and let the PLAY response
    // land inside it (see `ServerSessionState::retire_fanout`). The old
    // task exits at its next frame boundary; the session's Drop is the
    // backstop for a peer that never reads.
    if session.fanout_handle.is_some() {
        session.retire_fanout();
        session.peer_cancel = tokio_util::sync::CancellationToken::new();
    }
    // Snapshot the initial RTP seq and clock timestamp before spawning.
    // These values are handed to both spawn_peer_fanout (which uses them
    // to seed the first packet's header fields) and play_response_ok
    // (which embeds them in the PLAY RTP-Info header per RFC 7826 §18.45),
    // so the client's jitter buffer is seeded with the actual first-packet
    // coordinates.
    let initial_seq = crate::rtsp::server::rand_seq();
    let clock = crate::clock::RtpClock::new(0);
    let initial_rtptime = clock.now_ticks();
    let join = crate::rtsp::server::fanout::spawn_peer_fanout(
        rx,
        peer_transport,
        session.peer_cancel.clone(),
        state.cancel_token.clone(),
        crate::rtsp::server::rand_ssrc(),
        initial_seq,
        clock,
        drop_counter.clone(),
    );
    session.fanout_handle = Some(join);
    session.peer_drop_counter = Some(drop_counter);

    play_response_ok(req, &session_id, initial_seq, initial_rtptime)
}

/// Build a 200 OK PLAY response with Session + RTP-Info headers.
/// RTP-Info per RFC 7826 §18.45 — the `url` tag is required; `seq` and
/// `rtptime` are anchors clients can use for jitter-buffer initialization.
/// `initial_seq` and `initial_rtptime` must match the values handed to
/// `spawn_peer_fanout` so the client's jitter buffer is seeded with the
/// actual first-packet coordinates, not zeros.
///
/// `initial_seq` is exact — the fanout's first packet carries exactly this
/// sequence number. `initial_rtptime` is a snapshot taken just before the
/// fanout spawns: the first packet's RTP timestamp is this value plus the
/// scheduling delay until the first send (bounded by one event-loop cycle),
/// which is within RFC 7826 §18.45's intent of describing the first packet.
fn play_response_ok(
    req: &RtspRequest,
    session_id: &str,
    initial_seq: u16,
    initial_rtptime: u32,
) -> RtspResponse {
    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    headers.insert("session".into(), session_id.into());
    headers.insert(
        "rtp-info".into(),
        format!(
            "url={};seq={};rtptime={}",
            req.uri, initial_seq, initial_rtptime
        ),
    );
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

/// PAUSE handler — RFC 7826 §10.6 / RFC 2326 §11.3.
///
/// Cancels the per-peer fanout task (RTP stops flowing) while keeping
/// the rest of the session state (session_id, mount_path, allocated
/// transport sockets) intact. A subsequent PLAY can re-subscribe + spawn
/// a fresh fanout task; the `peer_cancel` token is replaced after
/// cancellation since `CancellationToken` doesn't auto-reset.
///
/// A publisher session (`session.publish.is_some()`) answers 200 with no
/// fanout retire — PAUSE on the publisher direction has no RFC 7826
/// analogue (RECORD has no PAUSE semantics defined); we just acknowledge
/// it and keep recording state untouched, matching how `handle_setup`'s
/// publisher branch and `handle_record` otherwise ignore the reader-only
/// fanout machinery entirely.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails.
/// - 454 Session Not Found — PAUSE before SETUP.
pub(crate) fn handle_pause(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(challenge) = check_auth(req, state, session, "PAUSE") {
        return challenge;
    }
    let Some(session_id) = session.session_id.clone() else {
        return error_response(req, 454, "Session Not Found");
    };
    if session.publish.is_some() {
        let mut headers = HashMap::new();
        if let Some(cseq) = req.headers.get("cseq") {
            headers.insert("cseq".into(), cseq.clone());
        }
        headers.insert("server".into(), server_header());
        headers.insert("session".into(), session_id);
        return RtspResponse {
            version: req.version,
            status: 200,
            reason: "OK".into(),
            headers,
            body: Bytes::new(),
        };
    }
    // Retire the current fanout task. The session's `peer_cancel` was
    // passed into `spawn_peer_fanout` at PLAY; cancelling it exits the
    // task at its next frame boundary. Replace with a fresh token so a
    // future PLAY can spawn anew. The handle is retired, not `abort()`ed:
    // an abort could cut an interleaved frame mid-`write_all` and the
    // 200 OK would then be written into the hole (see
    // `ServerSessionState::retire_fanout`); we don't `await` it either —
    // the dispatcher loop never blocks on the task's drain. A peer that
    // never reads defers that boundary exit indefinitely (the task stays
    // parked in its frame write, holding the writer); the session's Drop
    // aborts retired fanouts and is the backstop for that case.
    session.retire_fanout();
    session.peer_cancel = tokio_util::sync::CancellationToken::new();

    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    headers.insert("session".into(), session_id);
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

/// TEARDOWN handler — RFC 7826 §10.7 / RFC 2326 §11.4.
///
/// Cancels the fanout task and clears all session state. The per-session
/// dispatcher in `session.rs` observes the 200 OK + TEARDOWN method
/// and closes the TCP cleanly. Always returns 200 OK after auth so the
/// client gets a clean ack — even if no SETUP ever happened, TEARDOWN is
/// idempotent in the sense that "session is gone" is the same observable
/// either way.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails.
pub(crate) fn handle_teardown(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(challenge) = check_auth(req, state, session, "TEARDOWN") {
        return challenge;
    }
    let session_id = session.session_id.clone().unwrap_or_default();
    // End the publisher before touching the reader-fanout fields below —
    // `PublishSession::end` flushes the adapter, cancels any UDP ingest
    // tasks, and frees the mount's publisher slot (bumping its
    // generation) so a fresh ANNOUNCE can claim it right away.
    if let Some(mut p) = session.publish.take() {
        p.end();
    }
    // Retire, don't `abort()` — the 200 OK is written on the same
    // connection before FIN and must not land inside a half-written
    // interleaved frame (see `ServerSessionState::retire_fanout`).
    session.retire_fanout();
    // Clear all session state; the session task closes the TCP after
    // observing the 200 OK + TEARDOWN method.
    session.session_id = None;
    session.mount_path = None;
    session.transport = None;
    session.udp_sockets = None;
    session.interleaved_channels = None;
    session.peer_drop_counter = None;

    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    if !session_id.is_empty() {
        headers.insert("session".into(), session_id);
    }
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

/// GET_PARAMETER handler — v1 echoes 200 OK with cseq + session header
/// (used by client-initiated keepalive pings).
#[allow(unused_variables)]
pub(crate) fn handle_get_parameter(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    if let Some(sid) = req.headers.get("session") {
        headers.insert("session".into(), sid.clone());
    }
    headers.insert("server".into(), server_header());
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::message::RtspMethod;
    use crate::url::RtspVersion;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use tokio_util::sync::CancellationToken;

    fn make_state() -> Arc<ServerState> {
        let builder = crate::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        Arc::new(ServerState {
            builder,
            cancel_token: CancellationToken::new(),
            hard_cancel: crate::cancel::RtspServerCancelHandle::new(),
            mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_sessions: AtomicUsize::new(0),
            total_rtp_packets_sent: AtomicU64::new(0),
            total_rtp_bytes_sent: AtomicU64::new(0),
            started: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            local_addr: std::sync::Mutex::new(Some("127.0.0.1:8554".parse().unwrap())),
            sessions: std::sync::Mutex::new(Vec::new()),
            notice_cseq: AtomicU64::new(1_000_000),
            #[cfg(feature = "rtsp-server-tls")]
            tls_config: std::sync::Mutex::new(None),
            startup_tx: std::sync::Mutex::new(None),
        })
    }

    fn make_req(method: RtspMethod, uri: &str) -> RtspRequest {
        let mut headers = HashMap::new();
        headers.insert("cseq".into(), "1".into());
        RtspRequest {
            method,
            uri: uri.into(),
            version: RtspVersion::V1_0,
            headers,
            body: Bytes::new(),
        }
    }

    #[test]
    fn options_response_lists_public_methods() {
        let state = make_state();
        let req = make_req(RtspMethod::Options, "rtsp://127.0.0.1:8554/*");
        let resp = handle_options(&req, &state);
        assert_eq!(resp.status, 200);
        let public = resp.headers.get("public").unwrap();
        for m in [
            "OPTIONS",
            "DESCRIBE",
            "ANNOUNCE",
            "SETUP",
            "PLAY",
            "RECORD",
            "PAUSE",
            "TEARDOWN",
            "GET_PARAMETER",
        ] {
            assert!(public.contains(m), "missing method {m} in Public header");
        }
    }

    #[test]
    fn options_response_includes_cseq_and_server() {
        let state = make_state();
        let req = make_req(RtspMethod::Options, "rtsp://127.0.0.1:8554/");
        let resp = handle_options(&req, &state);
        assert_eq!(resp.headers.get("cseq").map(String::as_str), Some("1"));
        assert!(
            resp.headers
                .get("server")
                .map(|s| s.starts_with("tst-rtp/"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn describe_unknown_mount_returns_404() {
        let state = make_state();
        let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/nope");
        let mut session = ServerSessionState::new();
        let resp = handle_describe(&req, &state, &mut session);
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn describe_with_auth_required_returns_401_when_no_authorization() {
        use secrecy::SecretString;
        let mut builder = crate::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.auth_digest_md5("tst", "admin", SecretString::new("secret".into()));
        let state = Arc::new(ServerState {
            builder,
            cancel_token: CancellationToken::new(),
            hard_cancel: crate::cancel::RtspServerCancelHandle::new(),
            mounts: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_sessions: AtomicUsize::new(0),
            total_rtp_packets_sent: AtomicU64::new(0),
            total_rtp_bytes_sent: AtomicU64::new(0),
            started: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            local_addr: std::sync::Mutex::new(Some("127.0.0.1:8554".parse().unwrap())),
            sessions: std::sync::Mutex::new(Vec::new()),
            notice_cseq: AtomicU64::new(1_000_000),
            #[cfg(feature = "rtsp-server-tls")]
            tls_config: std::sync::Mutex::new(None),
            startup_tx: std::sync::Mutex::new(None),
        });
        let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = handle_describe(&req, &state, &mut session);
        assert_eq!(resp.status, 401);
        let www_auth = resp.headers.get("www-authenticate").unwrap();
        assert!(www_auth.starts_with("Digest"));
        assert!(www_auth.contains("realm=\"tst\""));
    }

    #[test]
    fn extract_mount_path_strips_scheme_and_authority() {
        assert_eq!(extract_mount_path("rtsp://host:8554/live"), "/live");
        assert_eq!(extract_mount_path("rtsps://host/live"), "/live");
        assert_eq!(extract_mount_path("/live"), "/live");
        assert_eq!(extract_mount_path("rtsp://host:8554/live?x=1"), "/live");
        // No path → root.
        assert_eq!(extract_mount_path("rtsp://host:8554"), "/");
        // Trailing slash is normalized away (except root).
        assert_eq!(extract_mount_path("rtsp://host:8554/live/"), "/live");
        assert_eq!(extract_mount_path("/live/"), "/live");
        // Root path ("/") keeps its slash.
        assert_eq!(extract_mount_path("rtsp://host:8554/"), "/");
        assert_eq!(extract_mount_path("/"), "/");
    }

    #[test]
    fn extract_mount_path_strips_trackid_segment() {
        // RtspClient::setup_mp2t_auto appends /trackID=0 from the SDP
        // a=control: attribute, so the SETUP URI is /live/trackID=0 but
        // only /live is registered as a mount. The fix strips the per-media
        // control segment.
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/trackID=0"),
            "/live"
        );
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/trackID=1"),
            "/live"
        );
        // Also strip the alternate streamid= form (older cameras).
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/streamid=0"),
            "/live"
        );
        // Non-track suffixes are kept (multi-segment mount paths).
        assert_eq!(
            extract_mount_path("rtsp://host:8554/live/audio"),
            "/live/audio"
        );
        // Root mount with trackID — keep root slash.
        assert_eq!(extract_mount_path("rtsp://host:8554/trackID=0"), "/");
    }

    #[test]
    fn error_response_includes_cseq_and_server() {
        let req = make_req(RtspMethod::Options, "/");
        let resp = error_response(&req, 404, "Not Found");
        assert_eq!(resp.status, 404);
        assert_eq!(resp.reason, "Not Found");
        assert_eq!(resp.headers.get("cseq").map(String::as_str), Some("1"));
        assert!(resp.headers.contains_key("server"));
    }

    // ── DESCRIBE Content-Base test ───────────────────────────────────────

    #[test]
    fn describe_response_includes_content_base_with_trailing_slash() {
        let state = make_state_with_mount();
        let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = handle_describe(&req, &state, &mut session);
        assert_eq!(resp.status, 200);
        // RFC 7826 §D: DESCRIBE response MUST include Content-Base so
        // third-party clients resolve relative a=control: URIs correctly.
        assert_eq!(
            resp.headers.get("content-base").map(String::as_str),
            Some("rtsp://127.0.0.1:8554/live/"),
        );
    }

    /// Content-Base idempotency: a request URI that already ends with `/`
    /// must resolve to the same mount as the slash-free form, and the
    /// response Content-Base must have exactly one trailing slash — not `//`.
    ///
    /// `extract_mount_path` normalizes trailing slashes, so both
    /// `rtsp://…/live` and `rtsp://…/live/` map to the same mount registered
    /// at `"/live"`.
    #[test]
    fn describe_content_base_no_double_slash_when_uri_already_has_trailing_slash() {
        // Mount is registered at "/live" (no trailing slash).
        // Both "/live" and "/live/" URIs must find it after normalization.
        let state = make_state_with_mount();

        // Non-trailing-slash form.
        let req_no_slash = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = handle_describe(&req_no_slash, &state, &mut session);
        assert_eq!(resp.status, 200, "no-slash form must find mount");
        assert_eq!(
            resp.headers.get("content-base").map(String::as_str),
            Some("rtsp://127.0.0.1:8554/live/"),
            "Content-Base must append exactly one trailing slash"
        );

        // Trailing-slash form: same mount, same Content-Base (not "…/live//").
        let req_slash = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live/");
        let mut session2 = ServerSessionState::new();
        let resp2 = handle_describe(&req_slash, &state, &mut session2);
        assert_eq!(
            resp2.status, 200,
            "trailing-slash form must find the same mount"
        );
        assert_eq!(
            resp2.headers.get("content-base").map(String::as_str),
            Some("rtsp://127.0.0.1:8554/live/"),
            "Content-Base must have exactly one trailing slash, not '//'"
        );
    }

    // ── SETUP handler tests ───────────────────────────────────────────────

    use crate::rtsp::server::mount::{MountEntry, MountKind, MountState};
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

    fn make_muxer_cfg() -> MuxerConfig {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    fn make_state_with_mount() -> Arc<ServerState> {
        let state = make_state();
        let mount_state = MountState::new("/live", MountKind::Unicast, make_muxer_cfg(), 256)
            .expect("mount state constructs");
        state
            .mounts
            .lock()
            .unwrap()
            .insert("/live".into(), MountEntry::Local(mount_state));
        state
    }

    fn make_state_with_multicast_mount() -> Arc<ServerState> {
        make_state_with_multicast_mount_at_port(5004)
    }

    fn make_state_with_multicast_mount_at_port(group_port: u16) -> Arc<ServerState> {
        let state = make_state();
        let mount_state = MountState::new(
            "/mc",
            MountKind::Multicast {
                group: format!("239.0.0.1:{group_port}").parse().unwrap(),
                ttl: 4,
                iface: None,
            },
            make_muxer_cfg(),
            256,
        )
        .expect("multicast mount state constructs");
        state
            .mounts
            .lock()
            .unwrap()
            .insert("/mc".into(), MountEntry::Local(mount_state));
        state
    }

    #[test]
    fn setup_with_no_transport_header_returns_400() {
        let state = make_state_with_mount();
        let req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        // No Transport header.
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 400);
    }

    /// `tokio::net::UdpSocket::from_std` requires a tokio reactor in
    /// scope, so the UDP-path SETUP test runs under a #[tokio::test].
    #[tokio::test]
    async fn setup_with_udp_transport_returns_200_with_server_port() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;unicast;client_port=5004-5005".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 200, "got: {} {}", resp.status, resp.reason);
        assert!(resp.headers.contains_key("session"));
        let transport_resp = resp.headers.get("transport").unwrap();
        assert!(
            transport_resp.contains("server_port="),
            "transport missing server_port: {transport_resp}"
        );
        assert!(
            transport_resp.contains("client_port=5004-5005"),
            "transport missing client_port echo: {transport_resp}"
        );
        assert!(session.session_id.is_some());
        assert!(session.udp_sockets.is_some());
        assert!(session.interleaved_channels.is_none());
        assert_eq!(session.mount_path.as_deref(), Some("/live"));
    }

    /// A second PLAY without PAUSE replaces the fanout instead of
    /// detaching the old task, which would keep streaming to the same peer
    /// with its own sequence space.
    #[tokio::test]
    async fn play_twice_replaces_the_fanout_instead_of_detaching_it() {
        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        let mut setup = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP;unicast;client_port=5004-5005".into(),
        );
        assert_eq!(handle_setup(&setup, &state, &mut session).status, 200);
        let play = make_req(RtspMethod::Play, "rtsp://127.0.0.1:8554/live");
        assert_eq!(handle_play(&play, &state, &mut session).status, 200);
        assert_eq!(handle_play(&play, &state, &mut session).status, 200);
        let fanout = state
            .mounts
            .lock()
            .unwrap()
            .get("/live")
            .unwrap()
            .fanout()
            .clone();
        // The cancelled task drops its broadcast receiver when it exits at
        // its next frame boundary (its next poll here: nothing is in flight).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while fanout.receiver_count() != 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "second PLAY left {} fanout receivers",
                fanout.receiver_count()
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let bye = make_req(RtspMethod::Teardown, "rtsp://127.0.0.1:8554/live");
        assert_eq!(handle_teardown(&bye, &state, &mut session).status, 200);
    }

    /// A second PLAY on a TCP-interleaved session must not cut the RTP
    /// frame the fanout is writing: the old task finishes its frame (or
    /// never starts one) before the PLAY response goes out on the same
    /// connection.
    ///
    /// The old fanout is modelled by a task parked inside a `write_all`
    /// under the shared writer, having put the `$`-channel-length header
    /// and the first payload byte on the wire. Without the fix the
    /// handler `abort()`ed that task, the mutex was released with 15
    /// payload bytes still owed, and the `RTSP/1.0 200 OK` text landed
    /// where the interleaved client expected RTP payload — a framing
    /// desync on a connection the server keeps serving.
    #[tokio::test]
    async fn play_replacement_preserves_an_in_progress_interleaved_frame() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        use tokio::sync::{Mutex, oneshot};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut client = client.unwrap();
        let (server, _) = accepted.unwrap();
        let (_read, write) = server.into_split();
        let writer = std::sync::Arc::new(Mutex::new(write));

        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        session.tcp_write = Some(writer.clone());
        let mut setup = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        assert_eq!(handle_setup(&setup, &state, &mut session).status, 200);
        let channel = session.interleaved_channels.unwrap().0;

        let (ready_tx, ready_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let old_writer = writer.clone();
        session.fanout_handle = Some(tokio::spawn(async move {
            let mut g = old_writer.lock().await;
            // A 16-byte payload, suspended after its first byte.
            g.write_all(&[b'$', channel, 0, 16, 0x55]).await.unwrap();
            let _ = ready_tx.send(());
            let _ = resume_rx.await;
            g.write_all(&[0x55; 15]).await.unwrap();
        }));
        ready_rx.await.unwrap();

        let play = make_req(RtspMethod::Play, "rtsp://127.0.0.1:8554/live");
        let response = handle_play(&play, &state, &mut session);
        assert_eq!(response.status, 200);
        let _ = resume_tx.send(());

        let control = tokio::spawn(async move {
            let mut g = writer.lock().await;
            g.write_all(&response.encode()).await.unwrap();
            g.shutdown().await.unwrap();
        });
        let mut wire = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_to_end(&mut wire),
        )
        .await
        .unwrap()
        .unwrap();
        control.await.unwrap();
        assert!(wire.len() >= 20);
        assert_eq!(
            &wire[5..20],
            &[0x55; 15],
            "control response replaced the unfinished RTP payload"
        );
    }

    /// Model of a fanout parked inside an interleaved frame write on a
    /// peer that never reads: it holds the session's shared writer and
    /// never reaches its cancel check. Resolves `ready` once it holds the
    /// lock; sets `ended` when the task is dropped (aborted).
    fn parked_fanout(
        writer: std::sync::Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
        ready: tokio::sync::oneshot::Sender<()>,
        ended: std::sync::Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        struct Flag(std::sync::Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        tokio::spawn(async move {
            let _flag = Flag(ended);
            let _g = writer.lock().await;
            let _ = ready.send(());
            std::future::pending::<()>().await;
        })
    }

    /// Set up an interleaved session whose fanout is `parked_fanout`;
    /// returns the session, the shared writer, and the parked task's
    /// `ended` flag. The client end is returned too so the TCP stays open.
    async fn session_with_parked_fanout() -> (
        Arc<ServerState>,
        ServerSessionState,
        std::sync::Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
        std::sync::Arc<AtomicBool>,
        tokio::net::TcpStream,
    ) {
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let (server, _) = accepted.unwrap();
        let (_read, write) = server.into_split();
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(write));

        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        session.tcp_write = Some(writer.clone());
        let mut setup = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        assert_eq!(handle_setup(&setup, &state, &mut session).status, 200);

        let ended = std::sync::Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        session.fanout_handle = Some(parked_fanout(writer.clone(), ready_tx, ended.clone()));
        ready_rx.await.unwrap();
        (state, session, writer, ended, client.unwrap())
    }

    /// Wait (bounded) for the parked task to be ended and the writer to
    /// become lockable again — both must follow from dropping the session.
    async fn assert_parked_fanout_ended(
        writer: &tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>,
        ended: &AtomicBool,
        what: &str,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !ended.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fanout retired by {what} outlived the session's Drop"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let _reacquired = tokio::time::timeout(std::time::Duration::from_secs(2), writer.lock())
            .await
            .unwrap_or_else(|_| {
                panic!("the writer is still held after the {what}-retired fanout ended")
            });
    }

    /// A fanout retired by PAUSE exits at its next frame boundary, which a
    /// never-reading peer defers indefinitely; the session's Drop must end
    /// it anyway (the terminal path, FIN follows) so it never outlives the
    /// session nor keeps the writer locked.
    #[tokio::test]
    async fn dropping_a_session_ends_a_fanout_retired_by_pause() {
        let (state, mut session, writer, ended, _client) = session_with_parked_fanout().await;
        let pause = make_req(RtspMethod::Pause, "rtsp://127.0.0.1:8554/live");
        assert_eq!(handle_pause(&pause, &state, &mut session).status, 200);
        assert!(
            !ended.load(std::sync::atomic::Ordering::Acquire),
            "PAUSE must not abort a fanout mid-frame"
        );
        drop(session);
        assert_parked_fanout_ended(&writer, &ended, "PAUSE").await;
    }

    /// Same contract for the fanout a second PLAY replaces.
    #[tokio::test]
    async fn dropping_a_session_ends_a_fanout_retired_by_a_second_play() {
        let (state, mut session, writer, ended, _client) = session_with_parked_fanout().await;
        let play = make_req(RtspMethod::Play, "rtsp://127.0.0.1:8554/live");
        assert_eq!(handle_play(&play, &state, &mut session).status, 200);
        assert!(
            !ended.load(std::sync::atomic::Ordering::Acquire),
            "a second PLAY must not abort the old fanout mid-frame"
        );
        drop(session);
        assert_parked_fanout_ended(&writer, &ended, "second PLAY").await;
    }

    #[test]
    fn setup_with_tcp_interleaved_returns_200_with_channel_pair() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 200, "got: {} {}", resp.status, resp.reason);
        let transport_resp = resp.headers.get("transport").unwrap();
        assert!(
            transport_resp.contains("interleaved="),
            "transport missing interleaved=: {transport_resp}"
        );
        assert!(session.interleaved_channels.is_some());
        assert!(session.udp_sockets.is_none());
    }

    #[test]
    fn setup_against_multicast_mount_rejects_tcp_with_461() {
        let state = make_state_with_multicast_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/mc");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 461);
    }

    #[test]
    fn setup_unknown_mount_returns_404() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/nope");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;unicast;client_port=5004-5005".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 404);
    }

    /// Multicast SETUP returns the multicast Transport response shape
    /// (destination + port + ttl) without binding a per-session UDP
    /// pair.
    #[test]
    fn setup_against_multicast_mount_returns_multicast_transport() {
        let state = make_state_with_multicast_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/mc");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;multicast;client_port=5004-5005".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 200, "got: {} {}", resp.status, resp.reason);
        let transport_resp = resp.headers.get("transport").unwrap();
        assert!(transport_resp.contains("multicast"));
        assert!(transport_resp.contains("destination=239.0.0.1"));
        assert!(transport_resp.contains("port=5004-5005"));
        assert!(transport_resp.contains("ttl=4"));
        // Multicast SETUP doesn't bind per-session UDP sockets — the
        // per-mount sender drives the actual sends.
        assert!(session.udp_sockets.is_none());
        assert!(session.session_id.is_some());
    }

    /// Guard: a multicast group port of 65535 must not panic (no u16
    /// overflow building the RTCP companion port in the Transport
    /// header) and must emit a single-port range, not a wrap-to-0.
    #[test]
    fn setup_multicast_group_port_65535_does_not_panic() {
        let state = make_state_with_multicast_mount_at_port(u16::MAX);
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/mc");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;multicast;client_port=5004-5005".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 200, "got: {} {}", resp.status, resp.reason);
        let transport_resp = resp.headers.get("transport").unwrap();
        // Single-port range (no 65536 companion); never port=65535-0.
        assert!(
            transport_resp.contains("port=65535;"),
            "got: {transport_resp}"
        );
    }

    // ── Adversarial SETUP Transport parsing (unauthenticated path) ──────

    /// A reversed client_port pair (hi-lo) must be rejected with 400, not
    /// silently accepted as a bogus range.
    #[test]
    fn setup_with_reversed_client_port_returns_400() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;unicast;client_port=5005-5004".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 400);
    }

    /// An invalid (non-numeric) client_port must be rejected with 400, not
    /// silently mapped to 0.
    #[test]
    fn setup_with_invalid_client_port_returns_400() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;unicast;client_port=abc-def".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 400);
    }

    /// A UDP unicast SETUP that omits client_port must be rejected (461
    /// Unsupported Transport) — the server must NOT bind a socket pair and
    /// echo a bogus client_port=0-0.
    #[tokio::test]
    async fn setup_udp_unicast_without_client_port_returns_461() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        // UDP unicast with NO client_port param.
        req.headers
            .insert("transport".into(), "RTP/AVP;unicast".into());
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 461);
        assert!(session.udp_sockets.is_none());
        assert!(session.session_id.is_none());
    }

    /// client_port=65535 has no valid RTCP companion (65536 overflows
    /// u16). The single-value form (`client_port=65535`) must be rejected
    /// with 400 rather than fabricating a wrapped companion.
    #[test]
    fn setup_with_single_max_client_port_returns_400() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;unicast;client_port=65535".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 400);
    }

    /// Mixed-case Transport parameter keys (`Client_Port=`, `UNICAST`)
    /// must parse correctly per RFC 7826 (case-insensitive param names).
    #[tokio::test]
    async fn setup_with_mixed_case_keys_returns_200() {
        let state = make_state_with_mount();
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP;UNICAST;Client_Port=5004-5005".into(),
        );
        let mut session = ServerSessionState::new();
        let resp = handle_setup(&req, &state, &mut session);
        assert_eq!(resp.status, 200, "got: {} {}", resp.status, resp.reason);
        let transport_resp = resp.headers.get("transport").unwrap();
        assert!(
            transport_resp.contains("client_port=5004-5005"),
            "transport missing client_port echo: {transport_resp}"
        );
    }

    #[test]
    fn generate_session_id_is_16_hex_chars() {
        let id = generate_session_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_session_id_changes_each_call() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b);
    }

    // ── PLAY / PAUSE / TEARDOWN handler tests ────────────────────────────

    #[test]
    fn play_before_setup_returns_454() {
        let state = make_state_with_mount();
        let req = make_req(RtspMethod::Play, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = handle_play(&req, &state, &mut session);
        assert_eq!(resp.status, 454);
    }

    #[test]
    fn pause_before_setup_returns_454() {
        let state = make_state_with_mount();
        let req = make_req(RtspMethod::Pause, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = handle_pause(&req, &state, &mut session);
        assert_eq!(resp.status, 454);
    }

    /// TEARDOWN clears session state and returns 200 OK even after only
    /// SETUP (no PLAY). No fanout handle was ever set, so the cleanup
    /// path takes the `Option::take` → `None` branch.
    #[test]
    fn teardown_after_setup_returns_200_and_clears_state() {
        let state = make_state_with_mount();
        let req = make_req(RtspMethod::Teardown, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        session.session_id = Some("abc123".into());
        session.mount_path = Some("/live".into());
        let resp = handle_teardown(&req, &state, &mut session);
        assert_eq!(resp.status, 200);
        assert!(session.session_id.is_none());
        assert!(session.mount_path.is_none());
        // Session header is echoed when a session was active.
        assert_eq!(
            resp.headers.get("session").map(String::as_str),
            Some("abc123")
        );
    }

    /// play_response_ok must embed the caller-supplied seq and rtptime values,
    /// not hardcoded zeros.  This is the server side of RFC 7826 §18.45: the
    /// PLAY response RTP-Info header must describe the actual first packet.
    #[test]
    fn play_response_ok_embeds_initial_seq_and_rtptime() {
        let req = make_req(RtspMethod::Play, "rtsp://127.0.0.1:8554/live");
        let resp = play_response_ok(&req, "sid123", 0xBEEF, 12345);
        assert_eq!(resp.status, 200);
        let rtp_info = resp
            .headers
            .get("rtp-info")
            .expect("rtp-info header present");
        // 0xBEEF = 48879 decimal.
        assert!(
            rtp_info.contains("seq=48879"),
            "rtp-info must contain actual seq=48879: {rtp_info}"
        );
        assert!(
            rtp_info.contains("rtptime=12345"),
            "rtp-info must contain actual rtptime=12345: {rtp_info}"
        );
        // Guard: a zero-regression would be caught here.
        assert!(
            !rtp_info.contains("seq=0;rtptime=0"),
            "rtp-info must not hardcode zeros: {rtp_info}"
        );
    }

    /// `compute_udp_play_target` must take the IP from the TCP peer address
    /// and combine it with the RTP port the client advertised in SETUP.
    /// This is the core correctness property: a remote client at 10.0.0.5
    /// whose SETUP said `client_port=5004-5005` must receive RTP at
    /// 10.0.0.5:5004, not 127.0.0.1:5004.
    #[test]
    fn compute_udp_play_target_uses_real_peer_ip() {
        let peer: std::net::SocketAddr = "10.0.0.5:40000".parse().unwrap();
        let target = compute_udp_play_target(peer, 5004);
        assert_eq!(
            target,
            "10.0.0.5:5004".parse::<std::net::SocketAddr>().unwrap()
        );
    }

    /// Loopback peer: the peer IP is 127.0.0.1, so the target must also
    /// be 127.0.0.1 (existing integration-test shape is unaffected).
    #[test]
    fn compute_udp_play_target_preserves_loopback() {
        let peer: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let target = compute_udp_play_target(peer, 5004);
        assert_eq!(
            target,
            "127.0.0.1:5004".parse::<std::net::SocketAddr>().unwrap()
        );
    }
}
