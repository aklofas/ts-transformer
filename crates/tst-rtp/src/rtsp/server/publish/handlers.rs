//! ANNOUNCE / SETUP `mode=record` / RECORD request handlers for a
//! published mount.
//!
//! `handle_announce` is dispatched directly from `session.rs`'s
//! `dispatch`; `handle_setup_record` is reached only through
//! [`crate::rtsp::server::handlers::handle_setup`]'s `(MountEntry,
//! is_record)` routing (auth for the SETUP method is already checked
//! there, so this module's SETUP path does not re-check it); `handle_record`
//! is dispatched directly from `dispatch` too. TEARDOWN and PAUSE's
//! publisher arms live in `crate::rtsp::server::handlers` itself (they
//! only need one extra branch each, not a full handler).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;

use crate::rtsp::client::transport_negotiation::{RtspTransportKind, TransportResponse};
use crate::rtsp::message::{RtspRequest, RtspResponse};
use crate::rtsp::server::ServerState;
use crate::rtsp::server::handlers::{
    bind_server_udp_pair, check_auth, control_segment, error_response, extract_mount_path,
    generate_session_id, handle_not_valid_in_state, last_path_segment, server_header,
};
use crate::rtsp::server::mount::MountEntry;
use crate::rtsp::server::session::ServerSessionState;
use crate::sdp::Sdp;

use super::PublishShape;
use super::adapter::{Mp2tAdapter, PublishAdapter};
use super::mount::{PublishMountState, PublisherInfo};
use super::session::{PublishSession, TrackTransport};
use super::shape::{ShapeReject, classify_announce};

/// 200 OK with CSeq + Server headers, and a bare `Session: <id>` when
/// `session_id` is given. ANNOUNCE doesn't allocate a session id (the
/// first SETUP does, via `generate_session_id`), so its 200 passes
/// `None`; RECORD passes the session's existing id.
fn ok_response(req: &RtspRequest, session_id: Option<&str>) -> RtspResponse {
    let mut headers = HashMap::new();
    if let Some(cseq) = req.headers.get("cseq") {
        headers.insert("cseq".into(), cseq.clone());
    }
    headers.insert("server".into(), server_header());
    if let Some(id) = session_id {
        headers.insert("session".into(), id.to_string());
    }
    RtspResponse {
        version: req.version,
        status: 200,
        reason: "OK".into(),
        headers,
        body: Bytes::new(),
    }
}

/// ANNOUNCE handler — RFC 2326 §10.3. Auth-gated.
///
/// Classifies the SDP body (`classify_announce`), claims the target
/// mount's publisher slot, builds the shape's `PublishAdapter`, and
/// installs a fresh `PublishSession` on `session.publish`.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails.
/// - 455 Method Not Valid in This State — this session already has a
///   publisher (one ANNOUNCE per connection; TEARDOWN first to replace it),
///   or already holds reader state from a SETUP (one role per connection).
/// - 400 Bad Request — missing/non-`application/sdp` Content-Type, an
///   empty body, or a structurally unusable SDP (`ShapeReject::BadRequest`).
/// - 415 Unsupported Media Type — a well-formed SDP whose track
///   combination matches no accepted shape (`ShapeReject::Unsupported`),
///   or (today) an `Elementary` shape, which has no H.264(+KLV) adapter
///   yet.
/// - 404 Not Found — mount path not registered.
/// - 461 Unsupported Transport — the mount exists but is a local
///   (muxer-backed) mount, which never accepts a publisher.
/// - 403 Forbidden — the mount already has a publisher.
pub(crate) fn handle_announce(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(c) = check_auth(req, state, session, "ANNOUNCE") {
        return c;
    }
    // One role per connection: no second publisher, and no publisher on
    // a connection that already set up (or is playing) as a reader.
    if session.publish.is_some() || session.transport.is_some() || session.mount_path.is_some() {
        return handle_not_valid_in_state(req);
    }
    let ct = req
        .headers
        .get("content-type")
        .map(|s| s.to_ascii_lowercase());
    if ct.as_deref().map(|c| c.starts_with("application/sdp")) != Some(true) || req.body.is_empty()
    {
        return error_response(req, 400, "Bad Request");
    }
    let sdp = match Sdp::parse(&req.body) {
        Ok(s) => s,
        Err(_) => return error_response(req, 400, "Bad Request"),
    };
    let announced = match classify_announce(&sdp) {
        Ok(a) => a,
        Err(ShapeReject::BadRequest(detail)) => {
            tracing::debug!(
                target: "tst_rtp::server::publish",
                detail = ?detail,
                "ANNOUNCE rejected 400"
            );
            return error_response(req, 400, "Bad Request");
        }
        Err(ShapeReject::Unsupported(detail)) => {
            tracing::debug!(
                target: "tst_rtp::server::publish",
                detail = ?detail,
                "ANNOUNCE rejected 415"
            );
            return error_response(req, 415, "Unsupported Media Type");
        }
    };
    let mount_path = extract_mount_path(&req.uri);
    let mount = {
        let mounts = match state.mounts.lock() {
            Ok(m) => m,
            Err(_) => return error_response(req, 500, "Internal Server Error"),
        };
        match mounts.get(&mount_path) {
            Some(MountEntry::Publish(m)) => m.clone(),
            Some(MountEntry::Local(_)) => {
                return error_response(req, 461, "Unsupported Transport");
            }
            // An on-demand (auto-create-on-ANNOUNCE) branch belongs here.
            None => return error_response(req, 404, "Not Found"),
        }
    };
    // Reject an `Elementary` shape BEFORE claiming the publisher slot —
    // until a real H.264(+KLV) adapter exists, ANNOUNCE can only
    // classify it, never feed it, so there is
    // nothing to hold the slot for. Claiming it first and immediately
    // freeing it would still bump the mount's generation and could 403 a
    // concurrent well-formed ANNOUNCE for no reason.
    let PublishShape::Mp2t = announced.shape else {
        return error_response(req, 415, "Unsupported Media Type");
    };
    let info = PublisherInfo {
        peer: session.peer_addr,
        shape: announced.shape,
        since: SystemTime::now(),
        generation: 0, // try_begin_publisher overwrites this with the mount's real value
    };
    if !mount.try_begin_publisher(info) {
        return error_response(req, 403, "Forbidden");
    }
    let adapter: Box<dyn PublishAdapter> = Box::new(Mp2tAdapter::new(
        mount.clone(),
        announced.tracks[0].payload_type,
    ));
    session.publish = Some(PublishSession::new(mount, announced, adapter));
    ok_response(req, None)
}

/// SETUP (`mode=record`) handler — reached only via
/// [`crate::rtsp::server::handlers::handle_setup`]'s routing match, which
/// has already run `check_auth(req, state, session, "SETUP")` and
/// resolved `mount_path`/`parsed` — this function does not repeat either.
///
/// Rejection codes:
/// - 455 Method Not Valid in This State — no ANNOUNCE has run on this
///   session yet, or the named track is already set up.
/// - 454 Session Not Found — the request carries a `Session:` header
///   that doesn't match the session id this publisher's first SETUP
///   already allocated (a later track's SETUP on the wrong connection).
/// - 404 Not Found — the SETUP URI's control segment doesn't name any
///   track this ANNOUNCE declared.
/// - 461 Unsupported Transport — a unicast UDP SETUP with no
///   `client_port=` (mirrors the reader path's same refusal).
/// - 500 Internal Server Error — UDP bind failure, no free interleaved
///   channel pair, or `local_addr` not yet set.
pub(crate) fn handle_setup_record(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
    parsed: &TransportResponse,
    mount_path: &str,
    routed_mount: &Arc<PublishMountState>,
) -> RtspResponse {
    let Some(publish) = session.publish.as_ref() else {
        return handle_not_valid_in_state(req);
    };
    // The caller (`handle_setup`) resolved `routed_mount` from the
    // request URI's path — ordinarily that's exactly this session's
    // announced mount, but `publisher_mount_path`'s announced-control
    // matching derives the mount path from the URI's trailing segment,
    // which a crafted or buggy request could point at a DIFFERENT
    // registered publish mount while still matching this session's own
    // announced control text. Never let a SETUP silently move this
    // session's publisher onto another mount.
    if !Arc::ptr_eq(routed_mount, &publish.mount) {
        return handle_not_valid_in_state(req);
    }
    // A later track's SETUP on this connection must name the session id
    // the first SETUP already allocated — the Session header is optional
    // on the first SETUP (no id exists yet) but must match once one does.
    // The comparison is on the id token before any `;timeout=` parameter
    // (RFC 2326 §12.37), which a client may echo back.
    if let Some(sid) = session.session_id.as_deref() {
        if let Some(hdr) = req.headers.get("session") {
            if hdr.split(';').next().unwrap_or("").trim() != sid {
                return error_response(req, 454, "Session Not Found");
            }
        }
    }
    // Resolve the track: first by an exact match against whatever the
    // SDP actually announced as `a=control` (handles any convention,
    // e.g. gst-rtsp-server's bare `stream=0`); if that finds nothing
    // (including when the announce had no `a=control` at all), fall
    // back to the fixed-prefix `control_segment` resolution, which also
    // covers the no-control, bare-mount-URI, single-track case the
    // exact match cannot (a track with no announced control never
    // matches an exact-text comparison).
    let idx = last_path_segment(&req.uri)
        .and_then(|seg| publish.track_for_raw_segment(seg))
        .or_else(|| {
            let segment = control_segment(&req.uri);
            session
                .publish
                .as_mut()
                .expect("checked Some above")
                .track_for_control(segment)
        });
    let Some(idx) = idx else {
        return error_response(req, 404, "Not Found");
    };
    // A track is set up once. A second SETUP would orphan the first
    // transport (and, after RECORD, a UDP track's new sockets would never
    // get an ingest task).
    if session.publish.as_ref().expect("checked Some above").tracks[idx]
        .transport
        .is_some()
    {
        return handle_not_valid_in_state(req);
    }

    let (transport_response_header, track_transport) = match parsed.kind {
        RtspTransportKind::Udp => {
            let Some(client_port) = parsed.client_port else {
                return error_response(req, 461, "Unsupported Transport");
            };
            let local_ip = match *state.local_addr.lock().unwrap() {
                Some(addr) => addr.ip(),
                None => return error_response(req, 500, "Internal Server Error"),
            };
            let (rtp_sock, rtcp_sock, server_rtp_port) = match bind_server_udp_pair(local_ip) {
                Ok(t) => t,
                Err(_) => return error_response(req, 500, "Internal Server Error"),
            };
            // Same +1 companion-port guard as the reader path's UDP branch.
            let Some(server_rtcp_port) = server_rtp_port.checked_add(1) else {
                return error_response(req, 500, "Internal Server Error");
            };
            (
                format!(
                    "RTP/AVP;unicast;client_port={}-{};server_port={}-{};mode=record",
                    client_port.0, client_port.1, server_rtp_port, server_rtcp_port,
                ),
                TrackTransport::Udp {
                    rtp: rtp_sock,
                    rtcp: rtcp_sock,
                },
            )
        }
        RtspTransportKind::TcpInterleaved => {
            let Some((base, companion)) = session
                .publish
                .as_ref()
                .expect("checked Some above")
                .interleaved_pair_for(parsed.interleaved)
            else {
                return error_response(req, 500, "Internal Server Error");
            };
            (
                format!("RTP/AVP/TCP;unicast;interleaved={base}-{companion};mode=record"),
                TrackTransport::Interleaved {
                    rtp: base,
                    rtcp: companion,
                },
            )
        }
    };
    session.publish.as_mut().expect("checked Some above").tracks[idx].transport =
        Some(track_transport);

    let session_id = match &session.session_id {
        Some(id) => id.clone(),
        None => {
            let id = generate_session_id();
            session.session_id = Some(id.clone());
            id
        }
    };
    session.mount_path = Some(mount_path.to_string());

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

/// RECORD handler — RFC 2326 §10.11. Auth-gated.
///
/// Spawns one UDP ingest task (`super::udp_ingest::spawn_udp_ingest`)
/// per `TrackTransport::Udp` track that doesn't already have one running —
/// gated per-track by `PublishTrack::udp_spawned`, so a later RECORD on
/// the same session (RFC 2326 §10.11 allows one) never spawns a
/// duplicate. Interleaved tracks spawn nothing; their RTP/RTCP already
/// arrives on the control connection and is dispatched by the session
/// loop's `$`-frame arm.
///
/// Rejection codes:
/// - 401 Unauthorized — auth check fails.
/// - 455 Method Not Valid in This State — no publisher on this session,
///   or its announced tracks aren't all SETUP yet.
pub(crate) fn handle_record(
    req: &RtspRequest,
    state: &Arc<ServerState>,
    session: &mut ServerSessionState,
) -> RtspResponse {
    if let Err(c) = check_auth(req, state, session, "RECORD") {
        return c;
    }
    let Some(publish) = session.publish.as_mut() else {
        return handle_not_valid_in_state(req);
    };
    if !publish.all_tracks_set_up() {
        return handle_not_valid_in_state(req);
    }
    publish.recording = true;
    for idx in 0..publish.tracks.len() {
        let track = &publish.tracks[idx];
        if track.udp_spawned {
            continue;
        }
        let Some(TrackTransport::Udp { rtp, rtcp }) = &track.transport else {
            continue;
        };
        let handle = super::udp_ingest::spawn_udp_ingest(
            idx,
            rtp.clone(),
            rtcp.clone(),
            session.peer_addr.ip(),
            track.announced.payload_type,
            publish.adapter.clone(),
            publish.mount.clone(),
            publish.last_media_ms.clone(),
            publish.udp_cancel.clone(),
        );
        publish.udp_tasks.push(handle);
        publish.tracks[idx].udp_spawned = true;
    }
    ok_response(req, session.session_id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtsp::message::{RtspMethod, RtspRequest};
    use crate::rtsp::server::session::ServerSessionState;
    use crate::rtsp::server::test_state;
    use crate::url::RtspVersion;
    use std::time::Duration;

    const SDP_MP2T: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:streamid=0\r\n";

    fn announce(uri: &str, body: &str) -> RtspRequest {
        let mut r = RtspRequest::new(RtspMethod::Announce, uri, RtspVersion::V1_0);
        r.headers.insert("cseq".into(), "2".into());
        r.headers
            .insert("content-type".into(), "application/sdp".into());
        r.body = bytes::Bytes::from(body.to_string());
        r
    }
    fn req(m: RtspMethod, uri: &str) -> RtspRequest {
        let mut r = RtspRequest::new(m, uri, RtspVersion::V1_0);
        r.headers.insert("cseq".into(), "3".into());
        r
    }
    fn state_with_publish_mount() -> (
        Arc<ServerState>,
        Arc<super::super::mount::PublishMountState>,
    ) {
        let st = test_state();
        let m = super::super::mount::PublishMountState::new("/pub", 8);
        st.mounts
            .lock()
            .unwrap()
            .insert("/pub".into(), MountEntry::Publish(m.clone()));
        (st, m)
    }

    #[test]
    fn announce_mp2t_enters_publisher_role() {
        let (st, m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        let r = handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        assert_eq!(r.status, 200);
        let p = s.publish.as_ref().unwrap();
        assert_eq!(p.shape, PublishShape::Mp2t);
        assert_eq!(p.tracks.len(), 1);
        assert!(m.publisher.lock().unwrap().is_some());
    }

    #[test]
    fn announce_refusals() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        assert_eq!(
            handle_announce(&announce("rtsp://h/nope", SDP_MP2T), &st, &mut s).status,
            404
        );
        let mut bad_ct = announce("rtsp://h/pub", SDP_MP2T);
        bad_ct
            .headers
            .insert("content-type".into(), "text/plain".into());
        assert_eq!(handle_announce(&bad_ct, &st, &mut s).status, 400);
        assert_eq!(
            handle_announce(&announce("rtsp://h/pub", ""), &st, &mut s).status,
            400
        );
        assert_eq!(
            handle_announce(
                &announce(
                    "rtsp://h/pub",
                    "v=0\r\no=- 0 0 IN IP4 1.2.3.4\r\ns=x\r\nt=0 0\r\nm=audio 0 RTP/AVP 97\r\na=rtpmap:97 L16/48000\r\na=control:a\r\n"
                ),
                &st,
                &mut s
            )
            .status,
            415
        );
        // reader (local) mount cannot be announced into
        let local = crate::rtsp::server::mount::MountState::new(
            "/live",
            crate::rtsp::server::mount::MountKind::Unicast,
            crate::rtsp::server::test_muxer_cfg(),
            8,
        )
        .unwrap();
        st.mounts
            .lock()
            .unwrap()
            .insert("/live".into(), MountEntry::Local(local));
        assert_eq!(
            handle_announce(&announce("rtsp://h/live", SDP_MP2T), &st, &mut s).status,
            461
        );
    }

    #[test]
    fn second_publisher_on_a_live_mount_is_403() {
        let (st, _m) = state_with_publish_mount();
        let mut s1 = ServerSessionState::new();
        assert_eq!(
            handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s1).status,
            200
        );
        let mut s2 = ServerSessionState::new();
        assert_eq!(
            handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s2).status,
            403
        );
        assert!(s2.publish.is_none());
        drop(s1); // publisher ends → mount idle
        let mut s3 = ServerSessionState::new();
        assert_eq!(
            handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s3).status,
            200
        );
    }

    #[test]
    fn setup_record_interleaved_then_record() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub/streamid=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
        );
        let r = crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        assert_eq!(r.status, 200, "{:?}", r.headers);
        assert!(
            r.headers["transport"].contains("mode=record"),
            "{:?}",
            r.headers
        );
        assert!(r.headers["session"].contains(";timeout="));
        assert!(s.session_id.is_some());
        // The response header echoes the pair the session stored: an
        // even base and its companion.
        let (rtp, rtcp) = match s.publish.as_ref().unwrap().tracks[0].transport {
            Some(TrackTransport::Interleaved { rtp, rtcp }) => (rtp, rtcp),
            ref other => panic!("expected an Interleaved transport, got {other:?}"),
        };
        assert_eq!(rtcp, rtp + 1, "companion channel must be rtp+1");
        assert_eq!(rtp % 2, 0, "base channel must be even");
        assert!(
            r.headers["transport"].contains(&format!("interleaved={rtp}-{rtcp}")),
            "response Transport header must echo the stored pair: {:?}",
            r.headers
        );
        let mut rec = req(RtspMethod::Record, "rtsp://h/pub");
        rec.headers
            .insert("session".into(), s.session_id.clone().unwrap());
        let r = handle_record(&rec, &st, &mut s);
        assert_eq!(r.status, 200);
        assert!(s.publish.as_ref().unwrap().recording);
    }

    #[test]
    fn state_errors_455_461_454() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        // SETUP mode=record before ANNOUNCE
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub/streamid=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
        );
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s).status,
            455
        );
        // RECORD before SETUP
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        assert_eq!(
            handle_record(&req(RtspMethod::Record, "rtsp://h/pub"), &st, &mut s).status,
            455
        );
        // reader SETUP (no mode=record) on a publish mount
        let mut play_setup = req(RtspMethod::Setup, "rtsp://h/pub");
        play_setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        let mut reader = ServerSessionState::new();
        // readers ARE allowed on publish mounts — this must be 200 (fan-out path)
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&play_setup, &st, &mut reader).status,
            200
        );
        // mode=record SETUP by the publisher session with a wrong Session header
        let mut setup2 = setup.clone();
        setup2
            .headers
            .insert("session".into(), "deadbeefdeadbeef".into());
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s).status,
            200
        );
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&setup2, &st, &mut s).status,
            454
        );
        // unknown control segment
        let mut setup3 = setup.clone();
        setup3.uri = "rtsp://h/pub/streamid=9".into();
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&setup3, &st, &mut s).status,
            404
        );
        // mode=record on a local mount
        let local = crate::rtsp::server::mount::MountState::new(
            "/live",
            crate::rtsp::server::mount::MountKind::Unicast,
            crate::rtsp::server::test_muxer_cfg(),
            8,
        )
        .unwrap();
        st.mounts
            .lock()
            .unwrap()
            .insert("/live".into(), MountEntry::Local(local));
        let mut setup4 = setup.clone();
        setup4.uri = "rtsp://h/live".into();
        let mut s4 = ServerSessionState::new();
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&setup4, &st, &mut s4).status,
            461
        );
    }

    #[test]
    fn setup_record_on_a_different_mount_is_455() {
        let (st, _m) = state_with_publish_mount();
        let m2 = super::super::mount::PublishMountState::new("/pub2", 8);
        st.mounts
            .lock()
            .unwrap()
            .insert("/pub2".into(), MountEntry::Publish(m2));
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        // "streamid=0" is this session's own announced control, so
        // `publisher_mount_path` resolves the mount path by stripping it
        // from the URI — landing on "/pub2", a DIFFERENT publish mount
        // than the one this session announced into. `handle_setup_record`
        // must refuse rather than move the session's publisher there.
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub2/streamid=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
        );
        let r = crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        assert_eq!(r.status, 455);
        assert_eq!(s.mount_path, None);
    }

    #[test]
    fn setup_record_resolves_gstreamer_stream_control() {
        // gst-rtsp-server's convention: `a=control:stream=0`, resolved
        // both by the generic `stream=` prefix now in `extract_mount_path`
        // / `control_segment`, and (for any OTHER control convention) by
        // `track_for_raw_segment`'s exact-match fallback.
        const SDP_STREAM_CONTROL: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:stream=0\r\n";
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_STREAM_CONTROL), &st, &mut s);
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub/stream=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
        );
        let r = crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        assert_eq!(r.status, 200, "{:?}", r.headers);
        assert_eq!(s.mount_path.as_deref(), Some("/pub"));
        assert!(matches!(
            s.publish.as_ref().unwrap().tracks[0].transport,
            Some(TrackTransport::Interleaved { .. })
        ));
    }

    #[test]
    fn teardown_ends_the_publisher_and_frees_the_mount() {
        let (st, m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let r = crate::rtsp::server::handlers::handle_teardown(
            &req(RtspMethod::Teardown, "rtsp://h/pub"),
            &st,
            &mut s,
        );
        assert_eq!(r.status, 200);
        assert!(s.publish.is_none());
        assert!(m.publisher.lock().unwrap().is_none());
        assert_eq!(m.generation.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn pause_is_acknowledged_and_keeps_recording_state() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub/streamid=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
        );
        crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        handle_record(&req(RtspMethod::Record, "rtsp://h/pub"), &st, &mut s);
        let r = crate::rtsp::server::handlers::handle_pause(
            &req(RtspMethod::Pause, "rtsp://h/pub"),
            &st,
            &mut s,
        );
        assert_eq!(r.status, 200);
        assert!(s.publish.is_some());
    }

    #[tokio::test]
    async fn setup_record_udp_binds_a_server_pair() {
        let (st, _m) = state_with_publish_mount();
        *st.local_addr.lock().unwrap() = Some("127.0.0.1:8554".parse().unwrap());
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub/streamid=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/UDP;unicast;client_port=23348-23349;mode=record".into(),
        );
        let r = crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        assert_eq!(r.status, 200);
        let t = &r.headers["transport"];
        assert!(
            t.contains("server_port=")
                && t.contains("client_port=23348-23349")
                && t.contains("mode=record"),
            "{t}"
        );
        assert!(matches!(
            s.publish.as_ref().unwrap().tracks[0].transport,
            Some(TrackTransport::Udp { .. })
        ));
    }

    #[tokio::test]
    async fn record_spawns_one_udp_ingest_task_idempotently_and_teardown_ends_it() {
        let (st, _m) = state_with_publish_mount();
        *st.local_addr.lock().unwrap() = Some("127.0.0.1:8554".parse().unwrap());
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let mut setup = req(RtspMethod::Setup, "rtsp://h/pub/streamid=0");
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/UDP;unicast;client_port=24350-24351;mode=record".into(),
        );
        let r = crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        assert_eq!(r.status, 200, "{:?}", r.headers);

        let mut rec = req(RtspMethod::Record, "rtsp://h/pub");
        rec.headers
            .insert("session".into(), s.session_id.clone().unwrap());
        let r = handle_record(&rec, &st, &mut s);
        assert_eq!(r.status, 200);
        assert_eq!(s.publish.as_ref().unwrap().udp_tasks.len(), 1);

        // RFC 2326 §10.11 allows a later RECORD on the same session —
        // it must not spawn a second ingest task for the same track.
        let r2 = handle_record(&rec, &st, &mut s);
        assert_eq!(r2.status, 200);
        assert_eq!(s.publish.as_ref().unwrap().udp_tasks.len(), 1);

        // Take the handle out before TEARDOWN: `PublishSession::end`
        // cancels `udp_cancel` (which this held task observes through
        // its own `select!`) and then aborts anything still left in
        // `udp_tasks` as a backstop — pulling it out first means this
        // assertion is exercising the graceful cancel path, not the
        // abort backstop.
        let j = s.publish.as_mut().unwrap().udp_tasks.pop().unwrap();

        let r3 = crate::rtsp::server::handlers::handle_teardown(
            &req(RtspMethod::Teardown, "rtsp://h/pub"),
            &st,
            &mut s,
        );
        assert_eq!(r3.status, 200);
        assert!(s.publish.is_none());
        tokio::time::timeout(Duration::from_secs(2), j)
            .await
            .unwrap()
            .unwrap();
    }

    fn setup_record(uri: &str) -> RtspRequest {
        let mut setup = req(RtspMethod::Setup, uri);
        setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record".into(),
        );
        setup
    }

    #[test]
    fn setup_record_resolves_an_absolute_control_url_on_another_host() {
        const SDP_ABS: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:rtsp://example.invalid/pub/streamid=0\r\n";
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        assert_eq!(
            handle_announce(&announce("rtsp://h/pub", SDP_ABS), &st, &mut s).status,
            200
        );
        let r = crate::rtsp::server::handlers::handle_setup(
            &setup_record("rtsp://127.0.0.1/pub/streamid=0"),
            &st,
            &mut s,
        );
        assert_eq!(r.status, 200, "{:?}", r.headers);
        assert_eq!(s.mount_path.as_deref(), Some("/pub"));
    }

    #[test]
    fn setup_record_resolves_an_absolute_control_url_naming_the_mount_itself() {
        // A single-track announce whose control is the aggregate URL: the
        // SETUP names the mount URI, which must not resolve to its parent.
        const SDP_ABS_MOUNT: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=x\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 0 RTP/AVP 33\r\na=control:RTSP://example.invalid:554/pub/\r\n";
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_ABS_MOUNT), &st, &mut s);
        let r = crate::rtsp::server::handlers::handle_setup(
            &setup_record("rtsp://127.0.0.1/pub"),
            &st,
            &mut s,
        );
        assert_eq!(r.status, 200, "{:?}", r.headers);
        assert_eq!(s.mount_path.as_deref(), Some("/pub"));
    }

    #[test]
    fn reader_setup_on_a_publisher_session_is_455() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let mut reader_setup = req(RtspMethod::Setup, "rtsp://h/pub");
        reader_setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        let r = crate::rtsp::server::handlers::handle_setup(&reader_setup, &st, &mut s);
        assert_eq!(r.status, 455);
        assert_eq!(s.session_id, None, "no reader session id minted");
        assert!(s.transport.is_none());
    }

    #[test]
    fn play_on_a_publisher_session_is_455() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let r = crate::rtsp::server::handlers::handle_setup(
            &setup_record("rtsp://h/pub/streamid=0"),
            &st,
            &mut s,
        );
        assert_eq!(r.status, 200);
        let mut play = req(RtspMethod::Play, "rtsp://h/pub");
        play.headers
            .insert("session".into(), s.session_id.clone().unwrap());
        let r = crate::rtsp::server::handlers::handle_play(&play, &st, &mut s);
        assert_eq!(r.status, 455);
        assert!(s.fanout_handle.is_none());
    }

    #[test]
    fn announce_on_a_reader_session_is_455() {
        let (st, m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        let mut reader_setup = req(RtspMethod::Setup, "rtsp://h/pub");
        reader_setup.headers.insert(
            "transport".into(),
            "RTP/AVP/TCP;unicast;interleaved=0-1".into(),
        );
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&reader_setup, &st, &mut s).status,
            200
        );
        let r = handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        assert_eq!(r.status, 455);
        assert!(s.publish.is_none());
        assert!(m.publisher.lock().unwrap().is_none(), "slot not claimed");
    }

    #[test]
    fn session_header_with_timeout_matches_and_a_set_up_track_is_not_set_up_twice() {
        let (st, _m) = state_with_publish_mount();
        let mut s = ServerSessionState::new();
        handle_announce(&announce("rtsp://h/pub", SDP_MP2T), &st, &mut s);
        let setup = setup_record("rtsp://h/pub/streamid=0");
        let r = crate::rtsp::server::handlers::handle_setup(&setup, &st, &mut s);
        assert_eq!(r.status, 200);
        let sid = s.session_id.clone().unwrap();

        // A Session header echoing `;timeout=60` names this session (not
        // 454); the track is already set up, so the second SETUP is 455.
        let mut again = setup.clone();
        again
            .headers
            .insert("session".into(), format!("{sid};timeout=60"));
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&again, &st, &mut s).status,
            455
        );
        // A different id with the same parameter is still 454.
        let mut wrong = setup.clone();
        wrong
            .headers
            .insert("session".into(), "deadbeefdeadbeef;timeout=60".into());
        assert_eq!(
            crate::rtsp::server::handlers::handle_setup(&wrong, &st, &mut s).status,
            454
        );
    }
}
