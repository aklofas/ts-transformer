//! WHEP request-head parsing, routing and response rendering.

use tst_webrtc::server::http::*;

#[test]
fn post_offer_parses_and_routes() {
    let raw = b"POST /cam1 HTTP/1.1\r\nHost: x\r\ncontent-type: application/sdp\r\nContent-Length: 5\r\nAuthorization: Bearer s3cret\r\nOrigin: https://ops\r\n\r\nv=0\r\n";
    let HeadParse::Done { request, head_len } = parse_head(raw) else {
        panic!()
    };
    assert_eq!(head_len, raw.len() - 5);
    assert_eq!(request.method, Method::Post);
    assert_eq!(request.bearer.as_deref(), Some("s3cret"));
    assert!(matches!(route(&request), Route::CreateSession { mount } if mount == "/cam1"));
}

#[test]
fn delete_routes_with_session_id() {
    let raw = b"DELETE /cam1/sessions/0123abcd HTTP/1.1\r\n\r\n";
    let HeadParse::Done { request, .. } = parse_head(raw) else {
        panic!()
    };
    assert!(
        matches!(route(&request), Route::DeleteSession { mount, id } if mount == "/cam1" && id == "0123abcd")
    );
}

#[test]
fn incomplete_too_large_malformed() {
    assert_eq!(
        parse_head(b"POST /x HTTP/1.1\r\nContent-Length: 1\r\n"),
        HeadParse::Incomplete
    );
    assert_eq!(parse_head(&vec![b'A'; MAX_HEAD + 1]), HeadParse::TooLarge);
    assert_eq!(
        parse_head(b"POST /x HTTP/1.1\r\nContent-Length: 999999\r\n\r\n"),
        HeadParse::Malformed
    );
    assert_eq!(parse_head(b"GARBAGE\r\n\r\n"), HeadParse::Malformed);
}

/// Exact response head. WHEP draft-ietf-wish-whep-03 §4.4.1: trickle ICE
/// and ICE restart are optional, and this server supports neither, so its
/// CORS method list (Fetch, "CORS protocol") names only what it takes.
/// A client that sees PATCH switches to trickle and then demands an ETag.
#[test]
fn render_golden_bytes() {
    assert_eq!(
        String::from_utf8(render(&resp(201, "v=0"), "*")).unwrap(),
        "HTTP/1.1 201 Created\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, DELETE, OPTIONS\r\n\
         Access-Control-Allow-Headers: Authorization, Content-Type, If-Match\r\n\
         Access-Control-Expose-Headers: Location\r\n\
         Content-Length: 3\r\n\
         Connection: close\r\n\r\nv=0"
    );
}

#[test]
fn preflight_routes_on_any_target() {
    let HeadParse::Done { request, .. } = parse_head(b"OPTIONS /cam1 HTTP/1.1\r\n\r\n") else {
        panic!()
    };
    assert!(matches!(route(&request), Route::Preflight));
}

/// RFC 9110 §15.5.6: a 405 lists the target resource's own methods.
#[test]
fn patch_and_other_methods_route_to_405_with_the_targets_allow() {
    for (raw, allow) in [
        (
            &b"PATCH /cam1/sessions/1 HTTP/1.1\r\n\r\n"[..],
            SESSION_ALLOW,
        ),
        (b"GET /cam1/sessions/1 HTTP/1.1\r\n\r\n", SESSION_ALLOW),
        (b"PATCH /cam1 HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"PATCH /sessions/1 HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"PATCH /cam1/sessions HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"PATCH * HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"GET /cam1 HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"POST /cam1/sessions/1 HTTP/1.1\r\n\r\n", SESSION_ALLOW),
        (b"DELETE /cam1 HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"DELETE /sessions/7 HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
        (b"DELETE /cam1/sessions HTTP/1.1\r\n\r\n", ENDPOINT_ALLOW),
    ] {
        let HeadParse::Done { request, .. } = parse_head(raw) else {
            panic!()
        };
        assert!(
            matches!(route(&request), Route::MethodNotAllowed { allow: a } if a == allow),
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn routing_edges() {
    let routed = |raw: &[u8]| {
        let HeadParse::Done { request, .. } = parse_head(raw) else {
            panic!("{}", String::from_utf8_lossy(raw))
        };
        route(&request)
    };
    // A trailing slash and a query string do not change the target.
    assert_eq!(
        routed(b"POST /a/b/?x=1 HTTP/1.1\r\n\r\n"),
        Route::CreateSession {
            mount: "/a/b".into()
        }
    );
    assert_eq!(
        routed(b"DELETE /a/b/sessions/7/ HTTP/1.1\r\n\r\n"),
        Route::DeleteSession {
            mount: "/a/b".into(),
            id: "7".into()
        }
    );
    for raw in [
        &b"POST / HTTP/1.1\r\n\r\n"[..],
        b"POST //cam1 HTTP/1.1\r\n\r\n",
        b"DELETE //cam1 HTTP/1.1\r\n\r\n",
        b"POST * HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(
            routed(raw),
            Route::NotFound,
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
    assert_eq!(
        routed(b"GET /cam1 HTTP/1.1\r\n\r\n"),
        Route::MethodNotAllowed {
            allow: ENDPOINT_ALLOW
        }
    );
    assert_eq!(routed(b"OPTIONS * HTTP/1.1\r\n\r\n"), Route::Preflight);
}

#[test]
fn ambiguous_heads_are_malformed() {
    for raw in [
        &b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
        b"POST /x HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
        b"POST /x HTTP/1.1\r\nContent-Length: +1\r\n\r\n",
        b"POST /x HTTP/1.1\r\nHost: a\r\n folded\r\n\r\n",
        b"POST /x HTTP/1.1\r\nHost: a\nX: b\r\n\r\n",
        b"POST /x HTTP/1.1\r\nBad Name: a\r\n\r\n",
        b"POST /x HTTP/2\r\n\r\n",
        b"POST x HTTP/1.1\r\n\r\n",
        b"POST  /x HTTP/1.1\r\n\r\n",
        b"\xff /x HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(
            parse_head(raw),
            HeadParse::Malformed,
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
    let HeadParse::Done { request, .. } = parse_head(
        b"POST /x HTTP/1.1\r\nauthorization: bearer  t0k \r\nCONTENT-LENGTH: 65536\r\n\r\n",
    ) else {
        panic!()
    };
    assert_eq!(request.bearer.as_deref(), Some("t0k"));
    assert_eq!(request.content_length, MAX_BODY);
    let HeadParse::Done { request, .. } =
        parse_head(b"POST /x HTTP/1.1\r\nAuthorization: Basic dTpw\r\n\r\n")
    else {
        panic!()
    };
    assert_eq!(request.bearer, None);
}
