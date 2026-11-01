//! WHEP signalling over HTTP/1.1: request-head parsing, routing and
//! response rendering. No I/O happens here; the accept thread feeds the
//! bytes it has read and writes the bytes [`render`] returns.
//!
//! The parser accepts only what a WHEP player sends: a request line, header
//! lines terminated by CRLF, and a body framed by `Content-Length`. Chunked
//! bodies, folded header lines and bare CR or LF characters are refused,
//! so a request has exactly one reading.

/// The largest request head accepted, including its terminating blank line.
pub const MAX_HEAD: usize = 16 * 1024;

/// The largest request body accepted. An SDP offer from a browser is a few
/// kilobytes.
pub const MAX_BODY: usize = 64 * 1024;

/// The methods a mount endpoint (`/<mount>`) takes, as a 405's `Allow`.
pub const ENDPOINT_ALLOW: &str = "POST, OPTIONS";

/// The methods a session resource (`/<mount>/sessions/<id>`) takes, as a
/// 405's `Allow`.
pub const SESSION_ALLOW: &str = "DELETE, OPTIONS";

/// The CORS method list on every response. WHEP draft-ietf-wish-whep-03
/// §4.4.1 makes trickle ICE and ICE restart optional and this server
/// supports neither, so PATCH is absent: a client that sees it switches to
/// trickle and then needs an ETag the 201 never carries.
pub const ALLOW_METHODS: &str = "POST, DELETE, OPTIONS";

/// The request method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `POST`: an SDP offer that creates a session.
    Post,
    /// `DELETE`: ends a session.
    Delete,
    /// `OPTIONS`: a CORS preflight.
    Options,
    /// Any other method token, `PATCH` included.
    Other,
}

/// A parsed request head.
///
/// `Debug` redacts the bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// The method.
    pub method: Method,
    /// The request target without its query string (`/cam1`, or `*`).
    pub path: String,
    /// The `Content-Type` value, if present.
    pub content_type: Option<String>,
    /// The `Content-Length` value; zero when the header is absent.
    pub content_length: usize,
    /// The token of an `Authorization: Bearer <token>` header.
    pub bearer: Option<String>,
    /// The `Origin` value, if present.
    pub origin: Option<String>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("content_type", &self.content_type)
            .field("content_length", &self.content_length)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .field("origin", &self.origin)
            .finish()
    }
}

/// The outcome of [`parse_head`].
#[derive(Debug, PartialEq, Eq)]
pub enum HeadParse {
    /// The blank line ending the head has not arrived yet; read more.
    Incomplete,
    /// The head is longer than [`MAX_HEAD`] (answered with 431 upstream).
    TooLarge,
    /// The head is not a request this server reads, or declares a body
    /// longer than [`MAX_BODY`] (answered with 400 upstream).
    Malformed,
    /// A complete head. The body starts at `head_len`.
    Done {
        /// The parsed request.
        request: Request,
        /// The head's length in bytes, including the blank line.
        head_len: usize,
    },
}

/// Parses the request head at the start of `buf`.
///
/// Header names match case-insensitively. A `Content-Length` above
/// [`MAX_BODY`], a repeated `Content-Length` and any `Transfer-Encoding`
/// make the request [`HeadParse::Malformed`].
pub fn parse_head(buf: &[u8]) -> HeadParse {
    let window = &buf[..buf.len().min(MAX_HEAD)];
    let Some(end) = window.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD {
            HeadParse::TooLarge
        } else {
            HeadParse::Incomplete
        };
    };
    match parse_request(&buf[..end]) {
        Some(request) => HeadParse::Done {
            request,
            head_len: end + 4,
        },
        None => HeadParse::Malformed,
    }
}

/// Parses the head without its terminating blank line; `None` is malformed.
fn parse_request(head: &[u8]) -> Option<Request> {
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");
    let (method, path) = parse_request_line(lines.next()?)?;
    let mut request = Request {
        method,
        path,
        content_type: None,
        content_length: 0,
        bearer: None,
        origin: None,
    };
    let mut saw_length = false;
    for line in lines {
        // Bare CR/LF and folded continuation lines have no single reading.
        if line.contains(['\r', '\n']) || line.starts_with([' ', '\t']) {
            return None;
        }
        let (name, value) = line.split_once(':')?;
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return None;
        }
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
            return None;
        }
        if name.eq_ignore_ascii_case("content-length") {
            if saw_length || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            saw_length = true;
            let length: usize = value.parse().ok()?;
            if length > MAX_BODY {
                return None;
            }
            request.content_length = length;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return None;
        } else if name.eq_ignore_ascii_case("content-type") {
            request.content_type = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("origin") {
            request.origin = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("authorization") {
            request.bearer = value
                .split_once(' ')
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, token)| token.trim_matches([' ', '\t']).to_string())
                .filter(|token| !token.is_empty());
        }
    }
    Some(request)
}

/// `METHOD SP target SP HTTP/1.x`, with an origin-form target or `*`.
fn parse_request_line(line: &str) -> Option<(Method, String)> {
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(is_token_byte)
        || !(version == "HTTP/1.1" || version == "HTTP/1.0")
    {
        return None;
    }
    if !(target == "*" || target.starts_with('/')) || !target.bytes().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    let method = match method {
        "POST" => Method::Post,
        "DELETE" => Method::Delete,
        "OPTIONS" => Method::Options,
        _ => Method::Other,
    };
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    Some((method, path.to_string()))
}

/// RFC 9110 `tchar`.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Where a request goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `POST /<mount>`: create a session on the mount.
    CreateSession {
        /// The mount path, with its leading `/` (`/cam1`).
        mount: String,
    },
    /// `DELETE /<mount>/sessions/<id>`: end a session.
    DeleteSession {
        /// The mount path, with its leading `/`.
        mount: String,
        /// The session id from the resource URL.
        id: String,
    },
    /// `OPTIONS` on any target: a CORS preflight.
    Preflight,
    /// A method the target does not take, answered with 405 and `allow`
    /// as its `Allow` (RFC 9110 §15.5.6).
    MethodNotAllowed {
        /// The target's own methods: [`SESSION_ALLOW`] or [`ENDPOINT_ALLOW`].
        allow: &'static str,
    },
    /// A target that names no mount or session resource (answered with 404).
    NotFound,
}

/// Routes a parsed request. A trailing `/` is ignored and empty path
/// segments name nothing; whether the mount or session exists is the
/// server's question, not the router's. A method the target does not take
/// is refused by the target's shape alone, before any lookup or bearer
/// check: `add_mount` refuses a path with a session resource's shape.
pub fn route(req: &Request) -> Route {
    let segments = path_segments(&req.path);
    let session = segments.as_deref().and_then(session_target);
    match req.method {
        Method::Options => Route::Preflight,
        Method::Post => match (segments, session) {
            (Some(_), Some(_)) => Route::MethodNotAllowed {
                allow: SESSION_ALLOW,
            },
            (Some(segments), None) => Route::CreateSession {
                mount: format!("/{}", segments.join("/")),
            },
            (None, _) => Route::NotFound,
        },
        Method::Delete => match (segments, session) {
            (_, Some((mount, id))) => Route::DeleteSession { mount, id },
            (Some(_), None) => Route::MethodNotAllowed {
                allow: ENDPOINT_ALLOW,
            },
            (None, None) => Route::NotFound,
        },
        Method::Other => Route::MethodNotAllowed {
            allow: if session.is_some() {
                SESSION_ALLOW
            } else {
                ENDPOINT_ALLOW
            },
        },
    }
}

/// The mount path and session id of `/<mount>/sessions/<id>`.
fn session_target(segments: &[&str]) -> Option<(String, String)> {
    match segments {
        [mount @ .., "sessions", id] if !mount.is_empty() => {
            Some((format!("/{}", mount.join("/")), (*id).to_string()))
        }
        _ => None,
    }
}

/// Whether `path` has the shape of a session resource,
/// `/<mount>/sessions/<id>`.
pub(crate) fn is_session_target(path: &str) -> bool {
    path_segments(path)
        .as_deref()
        .and_then(session_target)
        .is_some()
}

/// The path's segments without the leading `/` or a trailing one; `None`
/// when the path is not origin-form or has an empty segment.
fn path_segments(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let segments: Vec<&str> = rest.split('/').collect();
    (!segments.iter().any(|s| s.is_empty())).then_some(segments)
}

/// A response before rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The reason phrase.
    pub reason: &'static str,
    /// Extra headers (`Content-Type`, `Location`, `ETag`, ...).
    pub headers: Vec<(&'static str, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// Renders `resp` as HTTP/1.1 bytes. Adds the CORS headers for
/// `cors_origin`, `Content-Length` and `Connection: close` (every exchange
/// uses its own connection).
pub fn render(resp: &Response, cors_origin: &str) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = String::with_capacity(256);
    // Writing into a String cannot fail.
    let _ = write!(head, "HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
    for (name, value) in &resp.headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    let _ = write!(
        head,
        "Access-Control-Allow-Origin: {cors_origin}\r\n\
         Access-Control-Allow-Methods: {ALLOW_METHODS}\r\n\
         Access-Control-Allow-Headers: Authorization, Content-Type, If-Match\r\n\
         Access-Control-Expose-Headers: Location\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        resp.body.len()
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(&resp.body);
    out
}

/// A response with `status`, its standard reason phrase, no extra headers
/// and `body`.
pub fn resp(status: u16, body: impl Into<Vec<u8>>) -> Response {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    Response {
        status,
        reason,
        headers: Vec::new(),
        body: body.into(),
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_bearer_token() {
        let head = b"POST /cam1 HTTP/1.1\r\nAuthorization: Bearer s3cr3tT0ken\r\n\r\n";
        let HeadParse::Done { request, .. } = parse_head(head) else {
            panic!("head did not parse");
        };
        assert_eq!(request.bearer.as_deref(), Some("s3cr3tT0ken"));
        let shown = format!("{request:?}");
        assert!(!shown.contains("s3cr3tT0ken"), "{shown}");
        assert!(
            shown.contains("/cam1") && shown.contains("<redacted>"),
            "{shown}"
        );
    }
}
