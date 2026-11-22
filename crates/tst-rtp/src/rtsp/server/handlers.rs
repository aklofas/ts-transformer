//! RTSP request handlers — dispatched from `session.rs`'s per-session
//! state machine. Shared helpers (`server_header`, `error_response`,
//! `challenge_response`), OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE,
//! TEARDOWN, and GET_PARAMETER are all fully implemented. ANNOUNCE and
//! RECORD (publisher direction) are implemented in
//! `super::publish::handlers`; `handle_setup` routes a `mode=record`
//! SETUP there too, and `handle_teardown`/`handle_pause` have publisher
//! arms below.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::Bytes;

use crate::rtsp::message::{RtspMethod, RtspRequest, RtspResponse};
use crate::rtsp::server::ServerState;
use crate::rtsp::server::auth::{
    AuthVerifyError, Credential, VerifyContext, build_challenge, verify_credentials_with,
};
use crate::rtsp::server::builder::AuthScheme;
use crate::rtsp::server::mount::MountEntry;
use crate::rtsp::server::mount_rules::{Direction, MountPolicy};
use crate::rtsp::server::path::{extract_mount_path, publisher_mount_path};
use crate::rtsp::server::session::{
    Playback, ReaderSession, ReaderTransport, Role, ServerSessionState,
};

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

/// A 200 OK with the request's CSeq and our `Server:` header; the caller
/// adds the method's own headers and body. (`error_response` builds any
/// status — its name stays for its error callers.)
pub(crate) fn ok_response(req: &RtspRequest) -> RtspResponse {
    error_response(req, 200, "OK")
}

/// 455 for a method that is valid only in a state this session is not in.
pub(crate) fn handle_not_valid_in_state(req: &RtspRequest) -> RtspResponse {
    error_response(req, 455, "Method Not Valid in This State")
}

/// Build a 401 Unauthorized response with a fresh `WWW-Authenticate`
/// challenge in `scheme` and `realm`, the pair the request was checked
/// under, so a `Require` path is challenged even on a server without a
/// builder credential. Mutates `session.auth_nonce` if `stale = true`
/// (rotates the nonce for a stale-challenge retry per RFC 7616 §3.5). Also
/// resets `session.auth_nc_hwm` to 0 when the nonce is rotated so nc
/// tracking starts fresh for the new nonce.
pub(crate) fn challenge_response(
    req: &RtspRequest,
    session: &mut ServerSessionState,
    stale: bool,
    scheme: AuthScheme,
    realm: &str,
) -> RtspResponse {
    let mut response = error_response(req, 401, "Unauthorized");
    if stale {
        // Rotate the nonce so the client's next attempt uses a
        // fresh one. Reset the nc hwm so replay tracking begins
        // fresh under the new nonce.
        session.auth_nonce = crate::rtsp::server::auth::generate_nonce();
        session.auth_nc_hwm = 0;
    }
    let challenge = build_challenge(scheme, realm, &session.auth_nonce);
    response
        .headers
        .insert("www-authenticate".into(), challenge);
    response
}

/// The mount path the rules key on, and the direction, for this request on
/// this session.
///
/// ANNOUNCE and DESCRIBE name their own path: they key on the request URI,
/// ANNOUNCE publishing and DESCRIBE reading. Every other request on a
/// session that holds a role keys on the role's mount path (so a
/// publisher's `SETUP .../amber/video` still keys on `/amber`) and takes
/// the role's direction: a publisher's SETUP, PLAY, PAUSE, RECORD and
/// TEARDOWN publish, a reader's read. A reader's SETUP is the exception to
/// the key: it may move the reader to another mount, so it keys on the path
/// `handle_setup` resolves from the URI. On an `Idle` session they key on
/// the URI and read.
pub(crate) fn rule_key_and_direction(
    req: &RtspRequest,
    session: &ServerSessionState,
) -> (String, Direction) {
    let uri_keyed = || {
        let dir = if req.method == RtspMethod::Announce {
            Direction::Publish
        } else {
            Direction::Read
        };
        (extract_mount_path(&req.uri), dir)
    };
    if matches!(req.method, RtspMethod::Announce | RtspMethod::Describe) {
        return uri_keyed();
    }
    match &session.role {
        Role::Idle => uri_keyed(),
        // A reader that has not played yet may SETUP another mount, and
        // `handle_setup` re-points it there: key that SETUP on the path the
        // handler resolves, not the mount the reader is leaving.
        Role::Reader(_) if req.method == RtspMethod::Setup => (
            publisher_mount_path(&req.uri, session).unwrap_or_else(|| extract_mount_path(&req.uri)),
            Direction::Read,
        ),
        Role::Reader(r) => (r.mount_path.clone(), Direction::Read),
        // The mount this session's ANNOUNCE claimed; a publisher SETUP
        // never moves it to another mount.
        Role::Publisher(p) => (p.mount.path.clone(), Direction::Publish),
    }
}

/// Whether a refusal of this request, keyed on `path`, ends the session's
/// role: only when the key is the role's own mount. ANNOUNCE and DESCRIBE
/// name their own path and an `Idle` session has no role; a reader's SETUP
/// keys on the mount it names, which is its own only when it equals the
/// reader's mount (`/open/trackID=1` from a reader of `/open` is); every
/// other request of a session with a role keys on that role's mount.
fn keyed_on_own_role(req: &RtspRequest, session: &ServerSessionState, path: &str) -> bool {
    if matches!(req.method, RtspMethod::Announce | RtspMethod::Describe) {
        return false;
    }
    match &session.role {
        Role::Idle => false,
        Role::Reader(r) if req.method == RtspMethod::Setup => r.mount_path == path,
        Role::Reader(_) | Role::Publisher(_) => true,
    }
}

/// The method's text for the Digest check when `method` is admitted
/// through the mount rules; `None` for OPTIONS and GET_PARAMETER, which
/// never are. No wildcard arm: a new method does not compile until it is
/// classified here.
pub(crate) fn gated_method(method: RtspMethod) -> Option<&'static str> {
    match method {
        RtspMethod::Describe => Some("DESCRIBE"),
        RtspMethod::Announce => Some("ANNOUNCE"),
        RtspMethod::Setup => Some("SETUP"),
        RtspMethod::Play => Some("PLAY"),
        RtspMethod::Record => Some("RECORD"),
        RtspMethod::Pause => Some("PAUSE"),
        RtspMethod::Teardown => Some("TEARDOWN"),
        RtspMethod::Options | RtspMethod::GetParameter => None,
    }
}

/// Admit a gated request against the mount rules, before its handler runs.
/// Returns `Ok(())` if the request may proceed; otherwise the response to
/// send: the `Refuse` status (no challenge; it is not a 401, so it never
/// counts toward the three-401 close) or a 401 challenge. `method_str` is
/// [`gated_method`]'s text for the request.
pub(crate) fn authorize_request(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
    method_str: &str,
) -> Result<(), RtspResponse> {
    let (path, dir) = rule_key_and_direction(req, session);
    let auth_header = req.headers.get("authorization").map(String::as_str);
    let credential_checked: bool;
    // One reading of the table: the policy, and the challenge (scheme and
    // realm) that the check and its 401 use, the server's own credential
    // included.
    let decision = state.mount_rules.decision(&path, dir);
    let (scheme, realm) = (decision.scheme, decision.realm.as_str());
    let ctx = VerifyContext {
        scheme,
        realm,
        accept_digest_without_qop: state.builder.accept_digest_without_qop,
    };
    let verified = match &decision.policy {
        MountPolicy::Refuse(status) => {
            // A refusal keyed on the session's own role ends that role: a
            // publisher refused at RECORD or TEARDOWN stops. One that
            // concerns another mount (a reader's SETUP elsewhere) leaves
            // the role and its stream alone.
            if keyed_on_own_role(req, session, &path) {
                // Unlike TEARDOWN, a refusal keeps the connection open, so
                // it may set up a new role.
                session.end_role_keeping_the_connection();
            }
            return Err(error_response(req, status.code(), status.reason()));
        }
        MountPolicy::Open => return Ok(()),
        MountPolicy::Inherit => {
            let Some(auth_cfg) = state.builder.auth.as_ref() else {
                return Ok(()); // No auth configured — allow.
            };
            credential_checked = true;
            verify_credentials_with(
                auth_header,
                method_str,
                &req.uri,
                &ctx,
                &[Credential {
                    username: &auth_cfg.username,
                    password: &auth_cfg.password,
                }],
                &session.auth_nonce,
                &mut session.auth_nc_hwm,
            )
        }
        MountPolicy::Require(creds) => {
            credential_checked = true;
            let creds: Vec<Credential<'_>> = creds.iter().map(Credential::from).collect();
            verify_credentials_with(
                auth_header,
                method_str,
                &req.uri,
                &ctx,
                &creds,
                &session.auth_nonce,
                &mut session.auth_nc_hwm,
            )
        }
    };
    #[cfg(test)]
    if verified.is_err() {
        state.mount_rules.pause_before_challenge();
    }
    match verified {
        Ok(()) => {
            // Tells the session loop this request passed a credential check,
            // the only kind of success that may reset the 401 counter.
            session.request_authenticated = credential_checked;
            Ok(())
        }
        Err(AuthVerifyError::StaleNonce) => {
            // A 401 with a fresh nonce (the old one is rotated out). The
            // challenge carries no `stale=` parameter, so the client cannot
            // tell this 401 from a refused credential.
            Err(challenge_response(req, session, true, scheme, realm))
        }
        Err(_) => {
            // A credential that was sent and refused counts on the rule; the
            // first challenge every client gets (no header) and a stale
            // nonce (re-challenged, not a guess) do not.
            if auth_header.is_some() {
                decision.count_refused_credential(dir);
            }
            Err(challenge_response(req, session, false, scheme, realm))
        }
    }
}

/// OPTIONS handler — RFC 7826 §10.1 / RFC 2326 §10.1.
///
/// Returns the list of supported methods in the `Public:` header.
/// OPTIONS itself never requires auth (it's the connectivity probe).
#[allow(unused_variables)]
pub(crate) fn handle_options(req: &RtspRequest, state: &Arc<ServerState>) -> RtspResponse {
    let mut resp = ok_response(req);
    resp.headers.insert(
        "public".into(),
        "OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, RECORD, PAUSE, TEARDOWN, GET_PARAMETER".into(),
    );
    resp
}

/// DESCRIBE handler — RFC 7826 §10.2.
///
/// 1. Admitted before this runs ([`authorize_request`]: 401 if
///    missing/wrong/stale).
/// 2. Extract mount path from request URI; look up in state.mounts.
/// 3. 404 if mount not registered.
/// 4. 200 OK with `Content-Type: application/sdp` + SDP body from
///    `Sdp::build_for_mount`.
pub(crate) fn handle_describe(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    _session: &mut ServerSessionState,
) -> RtspResponse {
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

    let mut resp = ok_response(req);
    resp.headers
        .insert("content-type".into(), "application/sdp".into());
    resp.headers
        .insert("content-length".into(), body.len().to_string());
    // RFC 7826 §D: Content-Base anchors relative a=control: URIs in the SDP
    // body so third-party clients (VLC, ffplay) resolve SETUP URLs correctly.
    // The value is the request URI with a trailing slash appended.
    let content_base = {
        let uri = req.uri.trim_end_matches('/');
        format!("{uri}/")
    };
    resp.headers.insert("content-base".into(), content_base);
    resp.body = body;
    resp
}

/// Allocate a fresh even/odd TCP-interleaved channel pair for a reader
/// SETUP from a process-global counter. Channels are scoped per RTSP
/// connection on the wire, so a counter shared across sessions causes no
/// collision. (A publisher SETUP instead honours the publisher's requested
/// pair, per session: `PublishSession::interleaved_pair_for`.)
///
/// The base is always even (the counter steps by 2 from 0 and wraps
/// modulo 256), so the companion never overflows and the `None` arm is
/// unreachable today; callers keep mapping it to 500 so a future change
/// to the stepping fails closed.
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
/// - 401 Unauthorized — auth check fails (via `authorize_request`).
/// - 404 Not Found — mount path not registered, or a publisher session's
///   SETUP outside the mount it announced (answered like a missing mount).
/// - 400 Bad Request — Transport header missing or malformed.
/// - 455 Method Not Valid in This State — a reader-direction SETUP (no
///   `mode=record`) on a session that announced a publisher; one
///   connection holds one role.
/// - 461 Unsupported Transport — a transport that is not TCP-interleaved
///   on a server that does not allow media over UDP
///   ([`RtspServerBuilder::allow_udp_media`](crate::RtspServerBuilder::allow_udp_media);
///   by default an `rtsps://` server), reader or publisher;
///   TCP-interleaved against a multicast mount (RFC 7826 §13.3); or
///   `mode=record` against a local (muxer-backed) mount — only a publish
///   mount accepts a publisher.
/// - 453 Not Enough Bandwidth — a reader's SETUP while every reader slot
///   is held ([`RtspServerBuilder::max_readers`](crate::RtspServerBuilder::max_readers));
///   the connection is closed after the reply. Checked after every status
///   above and before the transport is allocated, so a request refused by
///   any status above takes no slot and a reader over the cap binds no UDP
///   port pair.
/// - 500 Internal Server Error — UDP socket bind failure, interleaved
///   channel allocator exhaustion, or poisoned mutex. A slot taken for the
///   request is freed again.
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
    // Look up the mount. A session that has already announced a
    // publisher gets first crack at resolving the path via its own
    // announced `a=control` values (handles non-standard control
    // conventions like gst-rtsp-server's bare `stream=N`); anything else
    // (a pre-ANNOUNCE SETUP, a reader, or a segment that doesn't match
    // what this session announced) falls back to the fixed-prefix form.
    let mount_path =
        publisher_mount_path(&req.uri, session).unwrap_or_else(|| extract_mount_path(&req.uri));
    // A publisher holds one mount. A SETUP naming another is answered as a
    // missing mount before any lookup, so it cannot reveal that mount (or
    // its type) past a `Refuse(NotFound)` rule.
    if session
        .publisher()
        .is_some_and(|p| p.mount.path != mount_path)
    {
        return error_response(req, 404, "Not Found");
    }
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
    let parsed = match crate::rtsp::transport_negotiation::parse_transport_response(transport_str) {
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

    // A server that does not allow media over UDP (by default an `rtsps://`
    // one, whose media would otherwise leave the TLS connection) refuses
    // every transport that is not interleaved, reader or publisher. Here,
    // after admission and the lookup, it reveals nothing about the path.
    if !state.builder.udp_media_allowed()
        && parsed.kind != crate::rtsp::transport_negotiation::RtspTransportKind::TcpInterleaved
    {
        tracing::debug!(
            target: "tst_rtp::server",
            transport = ?transport_str,
            "SETUP rejected 461: this server does not allow media over UDP"
        );
        return error_response(req, 461, "Unsupported Transport");
    }

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
    // One role per session: a connection that announced a publisher
    // cannot also become a reader (the session registry, and through it
    // `stop()`'s notice, names one mount per session). A reader that is
    // playing or paused cannot be re-pointed at another mount either: the
    // running fanout would keep serving the old one.
    match &session.role {
        Role::Publisher(_)
        | Role::Reader(ReaderSession {
            playback: Playback::Playing { .. } | Playback::Paused,
            ..
        }) => return handle_not_valid_in_state(req),
        Role::Idle | Role::Reader(_) => {}
    }

    // Per RFC 7826 §13.3: TCP-interleaved is incompatible with multicast.
    let is_multicast = mount.is_multicast();
    if is_multicast
        && matches!(
            parsed.kind,
            crate::rtsp::transport_negotiation::RtspTransportKind::TcpInterleaved
        )
    {
        return error_response(req, 461, "Unsupported Transport");
    }

    use crate::rtsp::transport_negotiation::RtspTransportKind;
    // Unicast UDP REQUIRES a valid client_port pair — the server fans RTP
    // out to client_port.0 (RTP) and RTCP to client_port.1. A missing
    // client_port can't be silently defaulted to 0-0 (that would point the
    // fan-out at privileged port 0). Reject with 461 Unsupported
    // Transport. (Malformed/reversed/out-of-range pairs are already
    // rejected as 400 by parse_transport_response above; this guards the
    // *absent* case.)
    if parsed.kind == RtspTransportKind::Udp && !is_multicast && parsed.client_port.is_none() {
        return error_response(req, 461, "Unsupported Transport");
    }

    // A reader holds one of the server's reader slots until its role ends.
    // The slot is taken after every refusal of the request itself and
    // before any transport is allocated, so a reader over the cap binds no
    // UDP port pair; a later failure drops the guard and frees the slot. A
    // reader set up again keeps the one it holds, so re-pointing it never
    // needs a second slot.
    let new_slot = if session.reader_slot.is_some() {
        None
    } else {
        match crate::rtsp::server::session::ReaderSlot::reserve(state) {
            Some(slot) => Some(slot),
            None => {
                tracing::warn!(
                    target: "tst_rtp::server",
                    peer = %session.peer_addr,
                    max_readers = state.builder.max_readers.unwrap_or_default(),
                    "max readers reached; refusing a reader SETUP and closing the connection"
                );
                session.close_after_reply = true;
                return error_response(req, 453, "Not Enough Bandwidth");
            }
        }
    };

    // Allocate transport-specific server state + build the response
    // header.
    let session_id = generate_session_id();
    let transport_response_header: String;
    let reader_transport: ReaderTransport;
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
                // guard in transport_negotiation::bind_udp_pair (rtp_port ==
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
                reader_transport = ReaderTransport::Multicast;
            } else {
                let Some(client_port) = parsed.client_port else {
                    unreachable!("an absent client_port is refused above")
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
                reader_transport = ReaderTransport::Udp {
                    sockets: (rtp_sock, rtcp_sock),
                    negotiated: parsed.clone(),
                };
            }
        }
        RtspTransportKind::TcpInterleaved => {
            // Allocate a fresh even/odd channel pair — see
            // `next_interleaved_pair`'s doc for why one process-global
            // counter is safe. 500 on exhaustion: a server-side allocator
            // problem, not a client error.
            let Some((base, companion)) = next_interleaved_pair() else {
                return error_response(req, 500, "Internal Server Error");
            };
            reader_transport = ReaderTransport::Interleaved {
                channels: (base, companion),
            };
            transport_response_header =
                format!("RTP/AVP/TCP;unicast;interleaved={base}-{companion}");
        }
    }

    if let Some(slot) = new_slot {
        session.reader_slot = Some(slot);
    }
    session.set_role(Role::Reader(ReaderSession {
        session_id: session_id.clone(),
        mount_path,
        mount: mount.identity(),
        transport: reader_transport,
        playback: Playback::Ready,
    }));

    let mut resp = ok_response(req);
    resp.headers.insert(
        "session".into(),
        format!(
            "{};timeout={}",
            session_id,
            state.builder.session_timeout.as_secs()
        ),
    );
    resp.headers
        .insert("transport".into(), transport_response_header);
    resp
}

/// Generate a 16-hex-char session ID. Per RFC 7826 §17.3.2, session IDs
/// must be at least 8 characters; 16 hex is generous and avoids
/// collisions across concurrent SETUPs.
pub(crate) fn generate_session_id() -> String {
    let mut buf = [0u8; 8];
    if getrandom::fill(&mut buf).is_err() {
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

/// Bind a fresh server-side UDP RTP+RTCP socket pair on `bind_ip` through
/// the shared allocator (kernel-picked RTP port, RTCP on RTP+1).
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
    let (rtp_std, rtcp_std, rtp_port) =
        crate::rtsp::transport_negotiation::bind_udp_pair(bind_ip, 0)
            .map_err(std::io::Error::other)?;
    rtp_std.set_nonblocking(true)?;
    rtcp_std.set_nonblocking(true)?;
    let rtp = tokio::net::UdpSocket::from_std(rtp_std)?;
    let rtcp = tokio::net::UdpSocket::from_std(rtcp_std)?;
    Ok((
        std::sync::Arc::new(rtp),
        std::sync::Arc::new(rtcp),
        rtp_port,
    ))
}

/// Returns the UDP target address for a unicast PLAY: preserves the
/// client's IP from the TCP control connection and substitutes the
/// RTP port that the client advertised in its SETUP `client_port` field.
///
/// Separating this from `handle_play` makes it directly unit-testable
/// without setting up a full `ServerState`.
pub(crate) fn compute_udp_play_target(peer: SocketAddr, client_rtp_port: u16) -> SocketAddr {
    SocketAddr::new(peer.ip(), client_rtp_port)
}

/// PLAY handler — RFC 7826 §10.5 / RFC 2326 §11.2.
///
/// Subscribes the session to the mount's broadcast fanout channel and
/// spawns the per-peer fanout task with the SETUP-allocated transport.
/// Invokes `spawn_peer_fanout` to wire the fan-out to the per-peer task.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails.
/// - 454 Session Not Found — the session is `Idle` (PLAY before SETUP).
/// - 455 Method Not Valid in This State — the session announced a
///   publisher (one role per connection).
/// - 404 Not Found — the reader's mount was removed after its SETUP, or
///   the path now holds another mount; the reader's role ends with it
///   (its reader slot is freed) and the connection stays open.
/// - 500 Internal Server Error — mounts mutex poisoned, or required
///   UDP socket pair missing despite the transport being UDP.
///
/// On 200: returns Session + RTP-Info headers. For multicast mounts the
/// per-mount sender drives sends, so PLAY just confirms;
/// no per-peer task is spawned. For TCP-interleaved unicast the
/// per-session `SessionWriter` (populated by `serve_connection`, plain TCP
/// or TLS) is cloned into `PeerTransport::Interleaved` so
/// the per-peer fanout task can write RFC 7826 §14 `$<channel><len>`
/// frames on the same TCP as the RTSP control responses; the
/// `AsyncMutex` around the writer serializes the two writers.
pub(crate) fn handle_play(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    let (session_id, mount_path, own_mount, transport) = match &session.role {
        // A publisher session never plays (one role per session).
        Role::Publisher(_) => return handle_not_valid_in_state(req),
        Role::Idle => return error_response(req, 454, "Session Not Found"),
        Role::Reader(r) => (
            r.session_id.clone(),
            r.mount_path.clone(),
            r.mount.clone(),
            r.transport.clone(),
        ),
    };
    let mounts = match state.mounts.lock() {
        Ok(m) => m,
        Err(_) => return error_response(req, 500, "Internal Server Error"),
    };
    let mount = match mounts.get(&mount_path) {
        Some(m) if m.is(&own_mount) => m.clone(),
        // The reader's own mount was removed (a SETUP answered while
        // `remove_mount` ran is not among the sessions it ends), and the
        // path may hold a mount this reader never set up: a refusal on the
        // reader's own mount, so it ends the role and frees the reader slot.
        _ => {
            drop(mounts);
            session.end_role_keeping_the_connection();
            return error_response(req, 404, "Not Found");
        }
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
    let drop_counter =
        crate::rtsp::server::fanout::PeerDropCounter::with_mount_total(mount.frames_dropped());
    let rx = mount.fanout().subscribe();
    let peer_transport = match transport {
        ReaderTransport::Udp {
            sockets,
            negotiated,
        } => {
            let (rtp_sock, _rtcp_sock) = sockets;
            // SETUP (handle_setup) already rejected a UDP unicast without a
            // client_port (461). Defense-in-depth: never fall back to a
            // bogus port-0 fan-out target — fail closed if it's somehow
            // absent at PLAY time.
            let Some(client_port) = negotiated.client_port else {
                return error_response(req, 500, "Internal Server Error");
            };
            let peer_addr = compute_udp_play_target(session.peer_addr, client_port.0);
            crate::rtsp::server::fanout::PeerTransport::Udp {
                socket: rtp_sock,
                peer_addr,
            }
        }
        ReaderTransport::Interleaved { channels } => {
            // The connection handler stored the session's writer (plain TCP
            // or TLS); the fanout task shares it with the response writer.
            // Only a unit-test session built without a connection has none.
            let Some(writer) = session.writer.clone() else {
                return error_response(req, 500, "Internal Server Error");
            };
            crate::rtsp::server::fanout::PeerTransport::Interleaved {
                writer,
                rtp_channel: channels.0,
                probe: state.interleaved_writes.clone(),
            }
        }
        // The multicast mount returned above.
        ReaderTransport::Multicast => return play_response_ok(req, &session_id, 0, 0),
    };
    // A second PLAY without PAUSE (RFC 7826 §13.4 allows it) replaces the
    // fanout; a detached old task would keep streaming to the same peer
    // with its own sequence space. Retire via
    // the token the fanout checks between frames — never `abort()`, which
    // could cut an interleaved frame mid-write and let the PLAY response
    // land inside it (see `ServerSessionState::retire_fanout`). The old
    // task exits at its next frame boundary; the session's Drop is the
    // backstop for a peer that never reads.
    if session.is_playing() {
        session.retire_fanout_and_rearm();
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
        Arc::clone(&state.rtp_sent),
    );
    if let Role::Reader(r) = &mut session.role {
        r.playback = Playback::Playing {
            fanout: join,
            drop_counter,
        };
    }

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
    let mut resp = ok_response(req);
    resp.headers.insert("session".into(), session_id.into());
    resp.headers.insert(
        "rtp-info".into(),
        format!(
            "url={};seq={};rtptime={}",
            req.uri, initial_seq, initial_rtptime
        ),
    );
    resp
}

/// PAUSE handler — RFC 7826 §10.6 / RFC 2326 §11.3.
///
/// Cancels the per-peer fanout task (RTP stops flowing) while keeping
/// the rest of the session state (session_id, mount_path, allocated
/// transport sockets) intact. A subsequent PLAY can re-subscribe + spawn
/// a fresh fanout task; the `peer_cancel` token is replaced after
/// cancellation since `CancellationToken` doesn't auto-reset.
///
/// A publisher session (`Role::Publisher`) answers 200 with no
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
    _state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    let Some(session_id) = session.session_id().map(str::to_owned) else {
        return error_response(req, 454, "Session Not Found");
    };
    if matches!(session.role, Role::Publisher(_)) {
        let mut resp = ok_response(req);
        resp.headers.insert("session".into(), session_id);
        return resp;
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
    session.retire_fanout_and_rearm();
    if let Role::Reader(r) = &mut session.role {
        r.playback = Playback::Paused;
    }

    let mut resp = ok_response(req);
    resp.headers.insert("session".into(), session_id);
    resp
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
    _state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    let session_id = session.session_id().unwrap_or_default().to_owned();
    // Retire the reader's fanout and end a publisher — `PublishSession::end`
    // flushes the adapter, cancels any UDP ingest tasks, and frees the
    // mount's publisher slot (bumping its generation) so a fresh ANNOUNCE can
    // claim it right away. The fanout is retired, not `abort()`ed: the 200 OK
    // is written on the same connection before FIN and must not land inside a
    // half-written interleaved frame (see `ServerSessionState::retire_fanout`).
    // The session task closes the TCP after observing the 200 OK + TEARDOWN.
    session.end_role();

    let mut resp = ok_response(req);
    if !session_id.is_empty() {
        resp.headers.insert("session".into(), session_id);
    }
    resp
}

/// GET_PARAMETER handler — v1 echoes 200 OK with cseq + session header
/// (used by client-initiated keepalive pings).
#[allow(unused_variables)]
pub(crate) fn handle_get_parameter(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    let mut resp = ok_response(req);
    if let Some(sid) = req.headers.get("session") {
        resp.headers.insert("session".into(), sid.clone());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::message::RtspMethod;
    use crate::url::RtspVersion;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

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
        let state = crate::rtsp::server::test_state();
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
        let state = crate::rtsp::server::test_state();
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
        let state = crate::rtsp::server::test_state();
        let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/nope");
        let mut session = ServerSessionState::new();
        let resp = handle_describe(&req, &state, &mut session);
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn describe_with_auth_required_returns_401_when_no_authorization() {
        use secrecy::SecretString;
        let mut builder =
            crate::rtsp::server::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.auth_digest_md5("tst", "admin", SecretString::new("secret".into()));
        let state = crate::rtsp::server::test_state_with(builder);
        let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = crate::rtsp::server::session::dispatch(&req, &state, &mut session);
        assert_eq!(resp.status, 401);
        let www_auth = resp.headers.get("www-authenticate").unwrap();
        assert!(www_auth.starts_with("Digest"));
        assert!(www_auth.contains("realm=\"tst\""));
    }

    #[test]
    fn quoted_realm_401_encodes_one_escaped_www_authenticate_line() {
        use secrecy::SecretString;
        let mut builder =
            crate::rtsp::server::builder::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.auth_digest_md5("ts \"q\"", "admin", SecretString::new("secret".into()));
        let state = crate::rtsp::server::test_state_with(builder);
        let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        let resp = crate::rtsp::server::session::dispatch(&req, &state, &mut session);
        assert_eq!(resp.status, 401);
        let wire = String::from_utf8(resp.encode().to_vec()).unwrap();
        let lines: Vec<&str> = wire
            .lines()
            .filter(|l| l.to_ascii_lowercase().starts_with("www-authenticate:"))
            .collect();
        assert_eq!(lines.len(), 1, "{wire}");
        assert!(lines[0].contains(r#"realm="ts \"q\"""#), "{}", lines[0]);
    }

    #[test]
    fn a_401_carries_the_challenge_its_request_was_checked_under() {
        use crate::rtsp::server::mount_rules::{MountCredential, MountPolicy, MountRule};
        use secrecy::SecretString;
        let state = crate::rtsp::server::test_state();
        state
            .mount_rules
            .set(
                "/live",
                MountRule::both(MountPolicy::Require(vec![MountCredential::new(
                    "viewer",
                    SecretString::new("pw".into()),
                )])),
            )
            .unwrap();
        let (reached, go_on) = state.mount_rules.arm_challenge_pause();
        let request = {
            let state = state.clone();
            std::thread::spawn(move || {
                let req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live");
                let mut session = ServerSessionState::new();
                crate::rtsp::server::session::dispatch(&req, &state, &mut session)
            })
        };
        reached
            .recv_timeout(tst_test_helpers::timing::scaled(
                std::time::Duration::from_secs(10),
            ))
            .expect("the request reached the point between its check and its 401");
        // The operator changes the challenge after the request was checked
        // under the table's Digest pair and before its 401 is built.
        state
            .mount_rules
            .set_challenge(AuthScheme::Basic, "lab")
            .unwrap();
        go_on.send(()).unwrap();
        let resp = request.join().unwrap();
        assert_eq!(resp.status, 401);
        let www = &resp.headers["www-authenticate"];
        assert!(
            www.starts_with("Digest") && www.contains(r#"realm="tstrans""#),
            "the 401 offered a challenge its request was not checked under: {www}"
        );
    }

    /// A Digest MD5 answer for `user:password` to the table's default
    /// challenge (realm `tstrans`) under `nonce`.
    fn digest_answer(user: &str, password: &str, method: &str, uri: &str, nonce: &str) -> String {
        use crate::rtsp::digest::{Algo, ha1, response};
        use secrecy::SecretString;
        let ha1 = ha1(
            Algo::Md5,
            user,
            "tstrans",
            &SecretString::new(password.into()),
        );
        let resp = response(
            Algo::Md5,
            &ha1,
            method,
            uri,
            nonce,
            "00000001",
            "cafe",
            "auth",
        );
        format!(
            "Digest username=\"{user}\", realm=\"tstrans\", nonce=\"{nonce}\", uri=\"{uri}\", \
             response=\"{resp}\", algorithm=MD5, qop=auth, nc=00000001, cnonce=\"cafe\""
        )
    }

    #[test]
    fn a_refused_credential_counts_on_its_rule_and_a_bare_challenge_does_not() {
        use crate::rtsp::server::mount_rules::{
            MountCredential, MountPolicy, MountRule, MountRuleAuthFailures, RefuseStatus,
        };
        use secrecy::SecretString;
        let state = crate::rtsp::server::test_state();
        let rules = &state.mount_rules;
        rules
            .set(
                "/live",
                MountRule::both(MountPolicy::Require(vec![MountCredential::new(
                    "viewer",
                    SecretString::new("pw".into()),
                )])),
            )
            .unwrap();
        rules
            .set(
                "/refused",
                MountRule::both(MountPolicy::Refuse(RefuseStatus::Forbidden)),
            )
            .unwrap();
        let uri = "rtsp://127.0.0.1:8554/live";
        // (what, method, the Authorization header given the session's nonce,
        // the status, the counts of /live after it).
        type Header = fn(&str) -> Option<String>;
        type Row = (&'static str, RtspMethod, Header, u16, (u64, u64));
        let rows: [Row; 9] = [
            ("no header", RtspMethod::Describe, |_| None, 401, (0, 0)),
            (
                "a stale nonce",
                RtspMethod::Describe,
                |_| {
                    Some(digest_answer(
                        "viewer",
                        "pw",
                        "DESCRIBE",
                        "rtsp://127.0.0.1:8554/live",
                        "not-this-sessions",
                    ))
                },
                401,
                (0, 0),
            ),
            (
                "a wrong password",
                RtspMethod::Describe,
                |n| {
                    Some(digest_answer(
                        "viewer",
                        "nope",
                        "DESCRIBE",
                        "rtsp://127.0.0.1:8554/live",
                        n,
                    ))
                },
                401,
                (0, 1),
            ),
            (
                "an unknown user",
                RtspMethod::Describe,
                |n| {
                    Some(digest_answer(
                        "nobody",
                        "pw",
                        "DESCRIBE",
                        "rtsp://127.0.0.1:8554/live",
                        n,
                    ))
                },
                401,
                (0, 2),
            ),
            (
                // The verifier reads a Digest header without this
                // connection's nonce as stale, whatever else it holds.
                "a Digest header without a nonce",
                RtspMethod::Describe,
                |_| Some("Digest garbage".into()),
                401,
                (0, 2),
            ),
            (
                "a malformed header",
                RtspMethod::Describe,
                |n| {
                    Some(format!(
                        "Digest username=\"viewer\", realm=\"tstrans\", nonce=\"{n}\""
                    ))
                },
                401,
                (0, 3),
            ),
            (
                "the wrong scheme",
                RtspMethod::Describe,
                |_| Some("Basic dmlld2VyOnB3".into()),
                401,
                (0, 4),
            ),
            (
                "a wrong password on ANNOUNCE",
                RtspMethod::Announce,
                |n| {
                    Some(digest_answer(
                        "viewer",
                        "nope",
                        "ANNOUNCE",
                        "rtsp://127.0.0.1:8554/live",
                        n,
                    ))
                },
                401,
                (1, 4),
            ),
            (
                "the right credential",
                RtspMethod::Describe,
                |n| {
                    Some(digest_answer(
                        "viewer",
                        "pw",
                        "DESCRIBE",
                        "rtsp://127.0.0.1:8554/live",
                        n,
                    ))
                },
                // Admitted: the handler answers for a path with no mount.
                404,
                (1, 4),
            ),
        ];
        for (what, method, header, status, (publish, read)) in rows {
            let mut session = ServerSessionState::new();
            let mut req = make_req(method, uri);
            if let Some(h) = header(&session.auth_nonce) {
                req.headers.insert("authorization".into(), h);
            }
            let resp = crate::rtsp::server::session::dispatch(&req, &state, &mut session);
            assert_eq!(resp.status, status, "{what}");
            let want = MountRuleAuthFailures { publish, read };
            assert_eq!(rules.auth_failures("/live"), Some(want), "{what}");
        }
        let mut session = ServerSessionState::new();
        let mut req = make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/refused");
        req.headers
            .insert("authorization".into(), "Basic dmlld2VyOnB3".into());
        let resp = crate::rtsp::server::session::dispatch(&req, &state, &mut session);
        assert_eq!(resp.status, 403, "a Refuse rule");
        assert_eq!(
            rules.auth_failures("/refused"),
            Some(MountRuleAuthFailures::default())
        );
        assert_eq!(
            rules.default_auth_failures(),
            MountRuleAuthFailures::default()
        );

        // An honest client's answer computed for the realm in force before
        // `set_challenge` changed it is a refused credential too.
        rules.set_challenge(AuthScheme::DigestMd5, "other").unwrap();
        let mut session = ServerSessionState::new();
        let mut req = make_req(RtspMethod::Describe, uri);
        let answer = digest_answer("viewer", "pw", "DESCRIBE", uri, &session.auth_nonce);
        req.headers.insert("authorization".into(), answer);
        let resp = crate::rtsp::server::session::dispatch(&req, &state, &mut session);
        assert_eq!(resp.status, 401, "an answer for the old realm");
        assert_eq!(
            rules.auth_failures("/live"),
            Some(MountRuleAuthFailures {
                publish: 1,
                read: 5
            }),
            "an answer for the old realm"
        );
    }

    #[test]
    fn refused_credentials_on_invented_paths_allocate_nothing() {
        use crate::rtsp::server::mount_rules::{MountCredential, MountPolicy, MountRule};
        use secrecy::SecretString;
        let state = crate::rtsp::server::test_state();
        let rules = &state.mount_rules;
        rules
            .set_default(MountRule::both(MountPolicy::Require(vec![
                MountCredential::new("viewer", SecretString::new("pw".into())),
            ])))
            .unwrap();
        for i in 0..1000 {
            let mut session = ServerSessionState::new();
            let mut req = make_req(RtspMethod::Describe, &format!("rtsp://127.0.0.1:8554/p{i}"));
            req.headers
                .insert("authorization".into(), "Basic Zm9vOmJhcg==".into());
            let resp = crate::rtsp::server::session::dispatch(&req, &state, &mut session);
            assert_eq!(resp.status, 401);
        }
        assert_eq!(rules.len(), 0);
        assert_eq!(rules.auth_failures("/p7"), None);
        assert_eq!(rules.default_auth_failures().read, 1000);
        assert_eq!(rules.default_auth_failures().publish, 0);
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

    /// Every 200 OK carries exactly the request's CSeq, our Server header and
    /// the method's own headers, says "OK", and echoes the request version.
    #[test]
    fn ok_responses_carry_exactly_their_headers() {
        use crate::rtsp::message::RtspResponse;
        fn keys(r: &RtspResponse) -> Vec<&str> {
            let mut k: Vec<&str> = r.headers.keys().map(String::as_str).collect();
            k.sort_unstable();
            k
        }
        let state = crate::rtsp::server::test_state();
        let mut session = ServerSessionState::new();

        let opt = handle_options(&make_req(RtspMethod::Options, "/"), &state);
        assert_eq!(keys(&opt), ["cseq", "public", "server"]);

        let mut gp_req = make_req(RtspMethod::GetParameter, "/");
        gp_req.headers.insert("session".into(), "abc".into());
        let gp = handle_get_parameter(&gp_req, &state, &mut session);
        assert_eq!(keys(&gp), ["cseq", "server", "session"]);
        assert_eq!(gp.headers["session"], "abc");

        let td = handle_teardown(&make_req(RtspMethod::Teardown, "/"), &state, &mut session);
        assert_eq!(keys(&td), ["cseq", "server"]);

        let with_mount = make_state_with_mount();
        let d = handle_describe(
            &make_req(RtspMethod::Describe, "rtsp://127.0.0.1:8554/live"),
            &with_mount,
            &mut ServerSessionState::new(),
        );
        assert_eq!(
            keys(&d),
            [
                "content-base",
                "content-length",
                "content-type",
                "cseq",
                "server"
            ]
        );
        assert_eq!(d.headers["content-length"], d.body.len().to_string());

        let play = play_response_ok(&make_req(RtspMethod::Play, "rtsp://h/live"), "sid", 7, 9);
        assert_eq!(keys(&play), ["cseq", "rtp-info", "server", "session"]);
        assert_eq!(play.headers["session"], "sid");
        assert_eq!(
            play.headers["rtp-info"],
            "url=rtsp://h/live;seq=7;rtptime=9"
        );

        for r in [&opt, &gp, &td, &d, &play] {
            assert_eq!((r.status, r.reason.as_str()), (200, "OK"));
            assert_eq!(r.version, RtspVersion::V1_0);
            assert_eq!(r.headers["cseq"], "1");
            assert_eq!(r.headers["server"], server_header());
        }
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
    use crate::rtsp::server::test_muxer_cfg;

    fn make_state_with_mount() -> Arc<ServerState> {
        let state = crate::rtsp::server::test_state();
        let mount_state = MountState::new(
            "/live",
            MountKind::Unicast,
            test_muxer_cfg(),
            256,
            state.ended(),
        )
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
        let state = crate::rtsp::server::test_state();
        let mount_state = MountState::new(
            "/mc",
            MountKind::Multicast {
                group: format!("239.0.0.1:{group_port}").parse().unwrap(),
                ttl: 4,
                iface: None,
            },
            test_muxer_cfg(),
            256,
            state.ended(),
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
        assert!(session.session_id().is_some());
        assert!(matches!(
            &session.role,
            Role::Reader(ReaderSession {
                transport: ReaderTransport::Udp { .. },
                ..
            })
        ));
        assert_eq!(session.mount_path(), Some("/live"));
    }

    /// An `rtsps://` server by default, and a plain one told so, answer a
    /// SETUP that is not TCP-interleaved with 461 and set up nothing.
    #[test]
    fn setup_over_udp_is_461_where_udp_media_is_off() {
        use crate::rtsp::server::builder::RtspServerBuilder;
        let tls = RtspServerBuilder::new("rtsps://127.0.0.1:0").unwrap();
        let mut plain = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        plain.allow_udp_media(false);
        for builder in [tls, plain] {
            let state = crate::rtsp::server::test_state_with(builder);
            let mount = MountState::new(
                "/live",
                MountKind::Unicast,
                test_muxer_cfg(),
                256,
                state.ended(),
            )
            .expect("mount state constructs");
            state
                .mounts
                .lock()
                .unwrap()
                .insert("/live".into(), MountEntry::Local(mount));
            let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
            req.headers.insert(
                "transport".into(),
                "RTP/AVP;unicast;client_port=5004-5005".into(),
            );
            let mut session = ServerSessionState::new();
            let resp = handle_setup(&req, &state, &mut session);
            assert_eq!(
                (resp.status, resp.reason.as_str()),
                (461, "Unsupported Transport")
            );
            assert!(!resp.headers.contains_key("transport"));
            assert!(matches!(session.role, Role::Idle));
        }
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
        use tokio::sync::oneshot;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut client = client.unwrap();
        let (server, _) = accepted.unwrap();
        let (_read, write) = server.into_split();
        let writer = crate::rtsp::server::session::session_writer(write);

        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        session.writer = Some(writer.clone());
        let mut setup = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        assert_eq!(handle_setup(&setup, &state, &mut session).status, 200);
        let Role::Reader(ReaderSession {
            transport: ReaderTransport::Interleaved { channels },
            ..
        }) = &session.role
        else {
            panic!("SETUP made an interleaved reader");
        };
        let channel = channels.0;

        let (ready_tx, ready_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let old_writer = writer.clone();
        session.install_test_fanout(tokio::spawn(async move {
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
        writer: crate::rtsp::server::session::SessionWriter,
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
        crate::rtsp::server::session::SessionWriter,
        std::sync::Arc<AtomicBool>,
        tokio::net::TcpStream,
    ) {
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let (server, _) = accepted.unwrap();
        let (_read, write) = server.into_split();
        let writer = crate::rtsp::server::session::session_writer(write);

        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        session.writer = Some(writer.clone());
        let mut setup = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        assert_eq!(handle_setup(&setup, &state, &mut session).status, 200);

        let ended = std::sync::Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        session.install_test_fanout(parked_fanout(writer.clone(), ready_tx, ended.clone()));
        ready_rx.await.unwrap();
        (state, session, writer, ended, client.unwrap())
    }

    /// Wait (bounded) for the parked task to be ended and the writer to
    /// become lockable again — both must follow from dropping the session.
    async fn assert_parked_fanout_ended(
        writer: &crate::rtsp::server::session::SessionWriter,
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
        assert!(matches!(
            &session.role,
            Role::Reader(ReaderSession {
                transport: ReaderTransport::Interleaved { .. },
                ..
            })
        ));
    }

    fn interleaved_setup_request() -> RtspRequest {
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        req
    }

    /// A reader that is playing or paused cannot be set up again: the
    /// running fanout would keep serving the old setup. Refused with 455,
    /// and the reader stays as it was.
    #[tokio::test]
    async fn setup_after_play_or_pause_on_a_reader_is_455() {
        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        assert_eq!(
            handle_setup(&interleaved_setup_request(), &state, &mut session).status,
            200
        );
        let session_id = session.session_id().unwrap().to_string();
        session.install_test_fanout(tokio::spawn(std::future::pending::<()>()));

        let resp = handle_setup(&interleaved_setup_request(), &state, &mut session);
        assert_eq!(resp.status, 455, "SETUP while playing");
        assert_eq!(session.session_id(), Some(session_id.as_str()));
        assert!(session.is_playing());

        session.retire_fanout();
        let resp = handle_setup(&interleaved_setup_request(), &state, &mut session);
        assert_eq!(resp.status, 455, "SETUP while paused");
        assert_eq!(session.session_id(), Some(session_id.as_str()));
    }

    /// A second SETUP on a reader that has not played yet is accepted and
    /// replaces the first: new session id, role still a ready reader.
    #[test]
    fn setup_after_a_first_setup_on_a_ready_reader_replaces_it() {
        let state = make_state_with_mount();
        let mut session = ServerSessionState::new();
        assert_eq!(
            handle_setup(&interleaved_setup_request(), &state, &mut session).status,
            200
        );
        let first = session.session_id().unwrap().to_string();

        let resp = handle_setup(&interleaved_setup_request(), &state, &mut session);
        assert_eq!(resp.status, 200, "got: {} {}", resp.status, resp.reason);
        assert_ne!(session.session_id().unwrap(), first);
        assert!(matches!(
            &session.role,
            Role::Reader(ReaderSession {
                playback: Playback::Ready,
                ..
            })
        ));
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
        assert!(matches!(
            &session.role,
            Role::Reader(ReaderSession {
                transport: ReaderTransport::Multicast,
                ..
            })
        ));
        assert!(session.session_id().is_some());
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
        assert!(matches!(session.role, Role::Idle));
    }

    /// The reader slot is taken after the refusals of the request itself:
    /// on a server with no reader slots a UDP SETUP without a client_port
    /// still reads 461 and keeps its connection.
    #[test]
    fn a_setup_refused_for_its_transport_reads_that_refusal_at_the_reader_cap() {
        let mut builder = crate::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.max_readers(0);
        let state = crate::rtsp::server::test_state_with(builder);
        add_live_mount(&state);
        let mut req = make_req(RtspMethod::Setup, "rtsp://127.0.0.1:8554/live");
        req.headers
            .insert("transport".into(), "RTP/AVP;unicast".into());
        let mut session = ServerSessionState::new();
        assert_eq!(handle_setup(&req, &state, &mut session).status, 461);
        assert!(!session.close_after_reply);

        let resp = handle_setup(&interleaved_setup_request(), &state, &mut session);
        assert_eq!(resp.status, 453, "the cap itself is still enforced");
        assert!(session.close_after_reply);
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

    /// A unicast mount at `/live` on a server with room for one reader.
    fn make_state_with_one_reader_slot() -> Arc<ServerState> {
        let mut builder = crate::RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        builder.max_readers(1);
        let state = crate::rtsp::server::test_state_with(builder);
        add_live_mount(&state);
        state
    }

    fn add_live_mount(state: &Arc<ServerState>) {
        let mount_state = MountState::new(
            "/live",
            MountKind::Unicast,
            test_muxer_cfg(),
            256,
            state.ended(),
        )
        .expect("mount state constructs");
        state
            .mounts
            .lock()
            .unwrap()
            .insert("/live".into(), MountEntry::Local(mount_state));
    }

    /// Take `/live` out of the table as `RtspServer::remove_mount` does.
    fn remove_live_mount(state: &Arc<ServerState>) {
        let entry = state.mounts.lock().unwrap().remove("/live").unwrap();
        let MountEntry::Local(m) = entry else {
            unreachable!("a local mount")
        };
        m.removed.cancel();
    }

    /// The rest of a test whose reader `a` lost its own mount: its PLAY
    /// reads 404 and ends its role, so its slot is free for a second reader
    /// under a cap of one, and its next PLAY reads like one from a session
    /// that never set up.
    fn assert_a_play_on_a_lost_mount_ends_the_role(
        state: &Arc<ServerState>,
        a: &mut ServerSessionState,
    ) {
        let play = make_req(RtspMethod::Play, "rtsp://127.0.0.1:8554/live");
        assert_eq!(handle_play(&play, state, a).status, 404);
        assert!(matches!(a.role, Role::Idle), "the reader's role ended");
        assert!(a.reader_slot.is_none());
        assert_eq!(state.active_readers.load(Ordering::Relaxed), 0);

        if !state.mounts.lock().unwrap().contains_key("/live") {
            add_live_mount(state);
        }
        let mut b = ServerSessionState::new();
        let resp = handle_setup(&interleaved_setup_request(), state, &mut b);
        assert_eq!(resp.status, 200, "a second reader gets the freed slot");

        assert_eq!(handle_play(&play, state, a).status, 454);
    }

    /// A reader whose SETUP completed while its mount was being removed is
    /// not among the sessions the removal ends; its PLAY finds the mount
    /// gone and ends its role.
    #[test]
    fn a_play_that_finds_its_own_mount_removed_ends_the_role_and_frees_the_slot() {
        let state = make_state_with_one_reader_slot();
        let mut a = ServerSessionState::new();
        assert_eq!(
            handle_setup(&interleaved_setup_request(), &state, &mut a).status,
            200
        );
        assert_eq!(state.active_readers.load(Ordering::Relaxed), 1);
        remove_live_mount(&state);
        assert_a_play_on_a_lost_mount_ends_the_role(&state, &mut a);
    }

    /// A mount added again at the reader's path is not the one it set up:
    /// its PLAY does not play the new mount, it ends the role as above.
    #[test]
    fn a_play_after_its_mount_was_replaced_at_the_same_path_ends_the_role() {
        let state = make_state_with_one_reader_slot();
        let mut a = ServerSessionState::new();
        assert_eq!(
            handle_setup(&interleaved_setup_request(), &state, &mut a).status,
            200
        );
        remove_live_mount(&state);
        add_live_mount(&state);
        assert_a_play_on_a_lost_mount_ends_the_role(&state, &mut a);
    }

    /// TEARDOWN clears session state and returns 200 OK even after only
    /// SETUP (no PLAY). No fanout handle was ever set, so the cleanup
    /// path takes the `Option::take` → `None` branch.
    #[test]
    fn teardown_after_setup_returns_200_and_clears_state() {
        let state = make_state_with_mount();
        let req = make_req(RtspMethod::Teardown, "rtsp://127.0.0.1:8554/live");
        let mut session = ServerSessionState::new();
        session.set_role(Role::Reader(ReaderSession {
            session_id: "abc123".into(),
            mount_path: "/live".into(),
            mount: crate::rtsp::server::mount::MountIdentity::none(),
            transport: ReaderTransport::Multicast,
            playback: Playback::Ready,
        }));
        let resp = handle_teardown(&req, &state, &mut session);
        assert_eq!(resp.status, 200);
        assert!(matches!(session.role, Role::Idle));
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
