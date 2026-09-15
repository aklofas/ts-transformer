//! Hand-rolled RTSP wire-format parse/serialize, scoped to the subset we
//! use: OPTIONS / DESCRIBE / SETUP / PLAY / PAUSE / TEARDOWN.
//!
//! The parser is intentionally minimal — it handles the response shape that
//! real RTSP cameras send, without pulling in a general-purpose RTSP library.
//! If real-camera quirks surface that require broader spec coverage, extend
//! [`RtspResponse::parse`] in place.

use std::collections::HashMap;

use bytes::Bytes;

use crate::error::RtspError;
use crate::url::RtspVersion;

/// Maximum allowed RTSP request body (Content-Length) in bytes.
///
/// RTSP request bodies are small in normal use (SDP offers for ANNOUNCE,
/// GET_PARAMETER bodies, etc.). Capping at 1 MiB is generous for any
/// legitimate client while preventing a single unauthenticated connection
/// from declaring a multi-GB body and driving the process to OOM.
pub(crate) const MAX_RTSP_BODY_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum bytes an in-progress RTSP message (status/request line + headers +
/// body) may accumulate in a pre-parse read buffer before it is rejected.
///
/// This is the single coherent cap for *accumulation* shared by every code
/// path that buffers raw RTSP text off a socket — the server session loop, the
/// server interleaved pump, the client `send_and_read` non-pump loop, and the
/// client interleaved pump. It guards the pre-parse path against a peer that
/// declares a huge `Content-Length` (or never sends a `CRLFCRLF` terminator)
/// and dribbles bytes, forcing the buffer to grow unbounded.
///
/// 64 KiB is generous for any legitimate RTSP message (typical OPTIONS /
/// DESCRIBE / SETUP responses are well under 2 KiB; even large SDP bodies are
/// rarely more than a few KiB). Note this accumulation cap is intentionally
/// smaller than the parser's [`MAX_RTSP_BODY_BYTES`] (1 MiB) body cap: the
/// body cap bounds a single already-complete message's declared body, while
/// this caps the live read buffer so a slow-dribble peer can't grow it
/// without bound.
pub(crate) const MAX_RTSP_MESSAGE_BYTES: usize = 64 * 1024; // 64 KiB

/// Strictly interpret a `Content-Length` header value (already trimmed by the
/// header parser) into a body length, rejecting hostile inputs.
///
/// - absent → `Ok(0)` (no body)
/// - present but non-`1*DIGIT` (e.g. `nope`, `+5`, `5x`) → `Err`
/// - present and overflows `usize` → `Err`
/// - present and `> MAX_RTSP_BODY_BYTES` → `Err`
///
/// Returning a silent `0` for a malformed/oversized value would let the
/// remaining declared-body bytes desync the framing (request smuggling), so
/// every non-clean value is an error. Per RFC 7826 Content-Length is
/// `1*DIGIT`, so we reject any non-ASCII-digit byte (Rust's `usize::from_str`
/// would otherwise accept a leading `+`).
pub(crate) fn parse_content_length(value: Option<&str>) -> Result<usize, &'static str> {
    let Some(raw) = value else {
        return Ok(0);
    };
    let digits = raw.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("unparseable Content-Length");
    }
    let n: usize = digits.parse().map_err(|_| "unparseable Content-Length")?;
    if n > MAX_RTSP_BODY_BYTES {
        return Err("Content-Length exceeds maximum");
    }
    Ok(n)
}

/// Split raw RTSP header text (everything before the terminating CRLFCRLF) into
/// header lines on `\r\n` exactly as the [`HashMap`] parsers
/// ([`RtspResponse::parse`]/[`RtspRequest::parse`]) do.
///
/// All three Content-Length code paths MUST tokenize identically: splitting on
/// bare `\n` (e.g. `str::lines`) where the HashMap parsers split on `\r\n`
/// creates a parser-differential (the scanner sees a `Content-Length` the
/// HashMap parser doesn't), the seed of a request-smuggle.
fn header_lines(header_text: &str) -> impl Iterator<Item = &str> {
    header_text.split("\r\n")
}

/// Scan raw RTSP header text (everything before the terminating CRLFCRLF) for
/// the `Content-Length` value, applying the same strict rules as
/// [`parse_content_length`] plus duplicate-header rejection.
///
/// Used by the interleaved framing pumps, which scan header lines directly
/// rather than building a [`HashMap`]. Tokenizes via [`header_lines`] so it
/// agrees byte-for-byte with the HashMap parsers' view. Returns the strict
/// body length on success, or an error detail string on a
/// malformed/oversized/duplicate Content-Length.
pub(crate) fn content_length_from_header_text(header_text: &str) -> Result<usize, &'static str> {
    let mut found: Option<&str> = None;
    for line in header_lines(header_text) {
        // Case-insensitive "content-length:" name without allocating per line.
        let Some(colon) = line.find(':') else {
            continue;
        };
        if line[..colon].trim().eq_ignore_ascii_case("content-length") {
            if found.is_some() {
                return Err("duplicate Content-Length header");
            }
            found = Some(line[colon + 1..].trim());
        }
    }
    parse_content_length(found)
}

/// Pre-parse accumulation guard for an interleaved pump buffer, which holds at
/// most one incomplete leading frame (the pump drains every complete frame each
/// iteration). Returns `true` when that leading frame has provably overrun its
/// legal bound and the pump must close:
///
/// - **Binary `$`-frame** (`buf[0] == b'$'`): inherently bounded — the 2-byte
///   u16 length permits at most a 65535-byte payload, so a full frame is
///   ≤ 65539 B. Such a frame can never overrun; return `false` (the
///   `buf.len() < 4 + length` check in the pump awaits the rest).
/// - **RTSP text** (any other leading byte): apply the same Phase-1 header cap
///   as [`rtsp_frame_decision`] / the client `send_and_read` loop — once the
///   un-terminated headers exceed [`MAX_RTSP_MESSAGE_BYTES`] (64 KiB), or a
///   terminated message declares a bad/over-cap `Content-Length`, the peer is
///   hostile. A legitimate body up to [`MAX_RTSP_BODY_BYTES`] (1 MiB) is *not*
///   rejected (the pump awaits it). This keeps the unterminated-header DoS bound
///   at a tight 64 KiB — coherent with B2 — rather than the loose
///   header+body sum.
pub(crate) fn pump_accumulation_exceeded(buf: &[u8]) -> bool {
    if buf.first() == Some(&b'$') {
        return false; // u16-bounded binary frame
    }
    matches!(
        rtsp_frame_decision(buf),
        RtspFraming::HeadersTooLong | RtspFraming::BadContentLength(_)
    )
}

/// Two-phase framing decision for a read buffer that begins with an RTSP message
/// (status/request line + headers + optional body). This is the single shared
/// implementation of the body-aware cap policy used by the client
/// `send_and_read` non-pump loop and the server session request loop, so the
/// two agree byte-for-byte:
///
/// - **Phase 1** — no `CRLFCRLF` terminator yet: the headers are still
///   accumulating. If they have already exceeded [`MAX_RTSP_MESSAGE_BYTES`]
///   (64 KiB) without terminating, the peer is malformed/adversarial →
///   [`RtspFraming::HeadersTooLong`]. Otherwise [`RtspFraming::NeedMore`].
/// - **Phase 2** — terminator seen: parse the (already-bounded) `Content-Length`
///   via the strict shared scanner. A malformed/duplicate/over-cap
///   (> [`MAX_RTSP_BODY_BYTES`]) value is fatal →
///   [`RtspFraming::BadContentLength`]. Otherwise the exact end is
///   `header_end + 4 + content_length` (≤ 64 KiB + 1 MiB); if the buffer is
///   short → [`RtspFraming::NeedMore`], else [`RtspFraming::Complete`].
pub(crate) enum RtspFraming {
    /// Within bounds but incomplete — keep reading.
    NeedMore,
    /// Pre-terminator headers exceeded [`MAX_RTSP_MESSAGE_BYTES`] — reject.
    HeadersTooLong,
    /// A malformed/duplicate/over-cap `Content-Length` — reject.
    BadContentLength(&'static str),
    /// A full message is present; `total_len` bytes (header + CRLFCRLF + body)
    /// belong to it.
    Complete { total_len: usize },
}

/// Apply the body-aware two-phase cap policy to a buffer that begins with an
/// RTSP message. See [`RtspFraming`] for the phase semantics. Non-UTF8 header
/// bytes are reported as a `BadContentLength` (they can't carry a valid
/// `Content-Length` and must not be read toward EOF).
pub(crate) fn rtsp_frame_decision(buf: &[u8]) -> RtspFraming {
    let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        // Phase 1: still accumulating headers.
        if buf.len() > MAX_RTSP_MESSAGE_BYTES {
            return RtspFraming::HeadersTooLong;
        }
        return RtspFraming::NeedMore;
    };
    // Phase 2: parse the declared body length via the strict shared scanner.
    let Ok(header_text) = core::str::from_utf8(&buf[..header_end]) else {
        return RtspFraming::BadContentLength("non-UTF8 RTSP headers");
    };
    let content_length = match content_length_from_header_text(header_text) {
        Ok(n) => n,
        Err(detail) => return RtspFraming::BadContentLength(detail),
    };
    // Exact end = header + CRLFCRLF + declared body. Checked so a near-usize::MAX
    // header_end can't wrap (content_length ≤ 1 MiB).
    let Some(total_len) = header_end
        .checked_add(4)
        .and_then(|e| e.checked_add(content_length))
    else {
        return RtspFraming::BadContentLength("RTSP body offset overflow");
    };
    if buf.len() < total_len {
        return RtspFraming::NeedMore;
    }
    RtspFraming::Complete { total_len }
}

/// Insert a parsed header. A repeated name follows RFC 7230 §3.2.2 list
/// semantics — the values are joined with `", "` — so a `WWW-Authenticate:`
/// a camera sends as two lines (Digest + Basic, RFC 7235 §4.1) reaches
/// `parse_challenges` as one multi-challenge string instead of last-wins
/// (which kept whichever scheme came LAST and downgraded Digest to Basic).
///
/// Three headers are single-valued and a repeat is rejected outright:
/// `Content-Length` (a classic request-smuggling vector a last-wins map
/// would hide), `CSeq` (RFC 7826 §18.20) and `Session` (§18.49).
fn insert_header_strict(
    headers: &mut HashMap<String, String>,
    name: String,
    value: String,
) -> Result<(), &'static str> {
    use std::collections::hash_map::Entry;
    match headers.entry(name) {
        Entry::Vacant(slot) => {
            slot.insert(value);
        }
        Entry::Occupied(mut slot) => match slot.key().as_str() {
            "content-length" => return Err("duplicate Content-Length header"),
            "cseq" => return Err("duplicate CSeq header"),
            "session" => return Err("duplicate Session header"),
            _ => {
                let joined = slot.get_mut();
                joined.push_str(", ");
                joined.push_str(&value);
            }
        },
    }
    Ok(())
}

/// Reject a header NAME or VALUE that carries a byte which would let it
/// break out of its single header line — closing the RTSP header/request
/// injection vector (a caller-supplied User-Agent, an Authorization built
/// from credentials, or a custom header containing `\r\n` would otherwise be
/// serialized verbatim and smuggle an attacker-chosen header or whole
/// request onto the wire).
///
/// Per RFC 7826 §5.1 a header field-value is visible ASCII plus SP/HT on a
/// single CRLF-terminated line. We hard-reject (never silently strip —
/// stripping hides the attack) any:
/// - CR (`\r`) or LF (`\n`) — the line terminators that enable injection;
/// - NUL (`\0`) — truncation hazard for C-string intermediaries;
/// - any other ASCII control byte (`< 0x20`, plus DEL `0x7f`).
///
/// SP (0x20) and HT (0x09) are intentionally *not* control bytes here only
/// in values; in names we also forbid them (a header name has no spaces).
/// We treat HT conservatively as forbidden in both: none of the headers we
/// emit (CSeq, User-Agent, Session, Authorization, Transport, …) legitimately
/// contain a tab, and allowing it widens the wire surface for no benefit.
pub(crate) fn validate_header_field(s: &str, detail: &'static str) -> Result<(), RtspError> {
    if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(RtspError::InvalidHeader { detail });
    }
    Ok(())
}

/// An RTSP request we construct on the client side.
///
/// Wraps a method + URI + version + headers + body and serializes to the
/// canonical CRLF-terminated wire format via [`RtspRequest::encode`]. The
/// headers we always set on production traffic (CSeq, User-Agent,
/// Session-if-present, Authorization-if-present) are added by the caller
/// via [`RtspRequest::header`] rather than baked in here, so this type
/// stays a pure data carrier.
#[derive(Debug, Clone)]
pub struct RtspRequest {
    pub method: RtspMethod,
    pub uri: String,
    pub version: RtspVersion,
    pub headers: HashMap<String, String>,
    pub body: Bytes,
}

/// An RTSP method we send. Bound to the subset we implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspMethod {
    Options,
    Describe,
    Setup,
    Play,
    Pause,
    Teardown,
    GetParameter,
}

/// An RTSP response we receive on the client side.
#[derive(Debug, Clone)]
pub struct RtspResponse {
    pub version: RtspVersion,
    pub status: u16,
    pub reason: String,
    pub headers: HashMap<String, String>,
    pub body: Bytes,
}

impl RtspRequest {
    pub fn new(method: RtspMethod, uri: impl Into<String>, version: RtspVersion) -> Self {
        Self {
            method,
            uri: uri.into(),
            version,
            headers: HashMap::new(),
            body: Bytes::new(),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.insert(name.to_ascii_lowercase(), value.into());
        self
    }

    pub fn body(mut self, body: Bytes) -> Self {
        self.body = body;
        self
    }

    /// Validate every header name and value, then serialize to wire bytes.
    ///
    /// This is the injection-safe serialization path used by all real send
    /// sites in the client: it rejects any request-line URI or header
    /// name/value carrying CR, LF, NUL, or another ASCII control byte (see
    /// [`validate_header_field`]) before producing a single byte, so a
    /// malicious User-Agent / Authorization (built from caller credentials) /
    /// custom header / request URI can never smuggle a second header or a
    /// whole request onto the wire.
    ///
    /// The request URI is validated too — not just headers — because the
    /// SETUP/PLAY request-line URI can derive from a server-provided SDP
    /// control URL (`a=control:` in the DESCRIBE response). Under Theme B's
    /// hostile-server model a CRLF there would split the request line exactly
    /// like a header CRLF. This serializer is THE injection-safe path and is
    /// complete on its own — it does not rely on an upstream URL parser for
    /// the security property.
    ///
    /// # Errors
    ///
    /// - [`RtspError::InvalidHeader`] if the request URI, any header name, or
    ///   any header value contains a forbidden control byte. No bytes are
    ///   written in that case.
    pub(crate) fn encode_checked(&self) -> Result<Bytes, RtspError> {
        validate_header_field(&self.uri, "request URI contains a control byte")?;
        for (k, v) in &self.headers {
            validate_header_field(k, "header name contains a control byte")?;
            validate_header_field(v, "header value contains a control byte")?;
        }
        Ok(self.encode())
    }

    /// Serialize to bytes ready to write to the TCP stream.
    ///
    /// This is the raw, infallible serializer; it does *not* validate header
    /// names/values. Production send paths use the crate-internal
    /// `encode_checked`, which validates against header injection first. Prefer
    /// that on any path where a header value derives from caller/credential input.
    pub fn encode(&self) -> Bytes {
        let method_str = match self.method {
            RtspMethod::Options => "OPTIONS",
            RtspMethod::Describe => "DESCRIBE",
            RtspMethod::Setup => "SETUP",
            RtspMethod::Play => "PLAY",
            RtspMethod::Pause => "PAUSE",
            RtspMethod::Teardown => "TEARDOWN",
            RtspMethod::GetParameter => "GET_PARAMETER",
        };
        let mut out = Vec::with_capacity(256 + self.body.len());
        out.extend_from_slice(method_str.as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.uri.as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.version.wire_str().as_bytes());
        out.extend_from_slice(b"\r\n");
        for (k, v) in &self.headers {
            // Capitalize first letter of each dash-separated word for canonicality.
            for (i, segment) in k.split('-').enumerate() {
                if i > 0 {
                    out.push(b'-');
                }
                let mut chars = segment.chars();
                if let Some(c) = chars.next() {
                    for ch in c.to_uppercase() {
                        out.extend_from_slice(ch.to_string().as_bytes());
                    }
                    for ch in chars {
                        out.extend_from_slice(ch.to_string().as_bytes());
                    }
                }
            }
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        if !self.body.is_empty() && !self.headers.contains_key("content-length") {
            out.extend_from_slice(b"Content-Length: ");
            out.extend_from_slice(self.body.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        Bytes::from(out)
    }
}

impl RtspResponse {
    /// Parse an RTSP response from a byte slice. Returns the parsed
    /// response plus the number of bytes consumed, or
    /// [`RtspError::BadResponse`] if the bytes don't form a complete
    /// well-formed response.
    ///
    /// Hand-rolled parser, scoped to the OPTIONS / DESCRIBE / SETUP /
    /// PLAY / PAUSE / TEARDOWN subset. If real-camera quirks surface,
    /// extend this parser.
    pub fn parse(input: &[u8]) -> Result<(Self, usize), RtspError> {
        // Find end of status line + headers (CRLFCRLF).
        let header_end =
            input
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .ok_or(RtspError::BadResponse {
                    detail: "no header terminator",
                })?;
        let header_text =
            std::str::from_utf8(&input[..header_end]).map_err(|_| RtspError::BadResponse {
                detail: "non-UTF8 headers",
            })?;
        let mut lines = header_lines(header_text);
        let status_line = lines.next().ok_or(RtspError::BadResponse {
            detail: "empty status line",
        })?;

        let mut parts = status_line.splitn(3, ' ');
        let version_str = parts.next().ok_or(RtspError::BadResponse {
            detail: "missing version",
        })?;
        let code_str = parts.next().ok_or(RtspError::BadResponse {
            detail: "missing status code",
        })?;
        let reason = parts.next().unwrap_or("").to_string();
        let version = match version_str {
            "RTSP/1.0" => RtspVersion::V1_0,
            "RTSP/2.0" => RtspVersion::V2_0,
            _ => {
                return Err(RtspError::BadResponse {
                    detail: "unrecognized RTSP version",
                });
            }
        };
        let status = code_str
            .parse::<u16>()
            .map_err(|_| RtspError::BadResponse {
                detail: "non-numeric status code",
            })?;

        let mut headers: HashMap<String, String> = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let colon = line.find(':').ok_or(RtspError::BadResponse {
                detail: "missing header colon",
            })?;
            let k = line[..colon].trim().to_ascii_lowercase();
            let v = line[colon + 1..].trim().to_string();
            insert_header_strict(&mut headers, k, v)
                .map_err(|detail| RtspError::BadResponse { detail })?;
        }

        let content_length =
            parse_content_length(headers.get("content-length").map(String::as_str))
                .map_err(|detail| RtspError::BadResponse { detail })?;
        let body_start = header_end + 4;
        let body_end = body_start
            .checked_add(content_length)
            .ok_or(RtspError::BadResponse {
                detail: "Content-Length body offset overflow",
            })?;
        if input.len() < body_end {
            return Err(RtspError::BadResponse {
                detail: "truncated body (Content-Length larger than available)",
            });
        }
        let body = Bytes::copy_from_slice(&input[body_start..body_end]);
        Ok((
            Self {
                version,
                status,
                reason,
                headers,
                body,
            },
            body_end,
        ))
    }

    pub fn cseq(&self) -> Option<u32> {
        self.headers.get("cseq")?.trim().parse().ok()
    }

    pub fn session_id(&self) -> Option<&str> {
        let raw = self.headers.get("session")?;
        // Format: 12345678;timeout=60 — strip the ;timeout= suffix.
        Some(raw.split(';').next().unwrap().trim())
    }

    pub fn session_timeout_secs(&self) -> Option<u64> {
        let raw = self.headers.get("session")?;
        for part in raw.split(';').skip(1) {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("timeout=") {
                return v.parse().ok();
            }
        }
        None
    }

    /// Serialize the response to wire bytes ready to write to the TCP
    /// stream. Mirrors [`RtspRequest::encode`] on the server side.
    ///
    /// Emits the status line, then headers (one per CRLF line, with the
    /// header name title-cased dash-segment-wise for cosmetic
    /// compatibility — RFC 7826 §5.1.1 declares headers case-insensitive
    /// but some IP cameras do not honor that), then a blank line, then
    /// the body (if any). Content-Length is *not* auto-injected — Task
    /// 10's DESCRIBE handler sets it explicitly when emitting an SDP
    /// body.
    pub fn encode(&self) -> Bytes {
        let mut buf = Vec::with_capacity(256 + self.body.len());
        // Status line.
        buf.extend_from_slice(self.version.wire_str().as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(self.status.to_string().as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(self.reason.as_bytes());
        buf.extend_from_slice(b"\r\n");
        // Headers.
        for (k, v) in &self.headers {
            for (i, segment) in k.split('-').enumerate() {
                if i > 0 {
                    buf.push(b'-');
                }
                let mut chars = segment.chars();
                if let Some(c) = chars.next() {
                    for ch in c.to_uppercase() {
                        buf.extend_from_slice(ch.to_string().as_bytes());
                    }
                    for ch in chars {
                        buf.extend_from_slice(ch.to_string().as_bytes());
                    }
                }
            }
            buf.extend_from_slice(b": ");
            buf.extend_from_slice(v.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(b"\r\n");
        if !self.body.is_empty() {
            buf.extend_from_slice(&self.body);
        }
        Bytes::from(buf)
    }
}

impl RtspRequest {
    /// Parse an RTSP request from wire bytes. Returns the parsed request
    /// plus the number of bytes consumed (including any Content-Length
    /// body). Mirrors [`RtspResponse::parse`] on the server side.
    ///
    /// # Errors
    /// - [`RtspError::BadResponse`] (reused for request-side malformed
    ///   text; we don't have a separate `BadRequest` variant in v1).
    pub fn parse(input: &[u8]) -> Result<(Self, usize), RtspError> {
        // 1. Find CRLFCRLF terminating headers.
        let header_end =
            input
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .ok_or(RtspError::BadResponse {
                    detail: "no CRLFCRLF terminating request headers",
                })?;
        let header_bytes = &input[..header_end];
        let header_text =
            std::str::from_utf8(header_bytes).map_err(|_| RtspError::BadResponse {
                detail: "non-UTF8 RTSP request",
            })?;

        // 2. Parse request line: "METHOD URI VERSION".
        let mut lines = header_lines(header_text);
        let request_line = lines.next().ok_or(RtspError::BadResponse {
            detail: "empty request",
        })?;
        let mut parts = request_line.splitn(3, ' ');
        let method_str = parts.next().ok_or(RtspError::BadResponse {
            detail: "no method in request line",
        })?;
        let uri = parts
            .next()
            .ok_or(RtspError::BadResponse {
                detail: "no URI in request line",
            })?
            .to_string();
        let version_str = parts.next().ok_or(RtspError::BadResponse {
            detail: "no version in request line",
        })?;

        let method = match method_str {
            "OPTIONS" => RtspMethod::Options,
            "DESCRIBE" => RtspMethod::Describe,
            "SETUP" => RtspMethod::Setup,
            "PLAY" => RtspMethod::Play,
            "PAUSE" => RtspMethod::Pause,
            "TEARDOWN" => RtspMethod::Teardown,
            "GET_PARAMETER" => RtspMethod::GetParameter,
            _ => {
                return Err(RtspError::BadResponse {
                    detail: "unsupported RTSP method",
                });
            }
        };
        let version = match version_str {
            "RTSP/1.0" => RtspVersion::V1_0,
            "RTSP/2.0" => RtspVersion::V2_0,
            _ => {
                return Err(RtspError::BadResponse {
                    detail: "unsupported RTSP version",
                });
            }
        };

        // 3. Parse headers.
        let mut headers: HashMap<String, String> = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let colon = line.find(':').ok_or(RtspError::BadResponse {
                detail: "malformed header line",
            })?;
            let name = line[..colon].trim().to_ascii_lowercase();
            let value = line[colon + 1..].trim().to_string();
            insert_header_strict(&mut headers, name, value)
                .map_err(|detail| RtspError::BadResponse { detail })?;
        }

        // 4. Read body per Content-Length, if any.
        //
        // Hard cap: RTSP requests from push-side clients have at most a small
        // SDP/SDP-offer body. Anything larger is either malformed or an attempt
        // to drive unbounded memory use (OOM DoS). An unparseable, oversized,
        // or duplicate Content-Length is rejected at parse time (not silently
        // coerced to 0, which would desync the framing) so the session loop can
        // send 413/400 and close without buffering.
        let content_length =
            parse_content_length(headers.get("content-length").map(String::as_str))
                .map_err(|detail| RtspError::BadResponse { detail })?;
        let body_start = header_end + 4;
        // Checked addition guards against a body offset wrapping usize.
        let body_end = body_start
            .checked_add(content_length)
            .ok_or(RtspError::BadResponse {
                detail: "Content-Length body offset overflow",
            })?;
        if input.len() < body_end {
            return Err(RtspError::BadResponse {
                detail: "truncated body",
            });
        }
        let body = Bytes::copy_from_slice(&input[body_start..body_end]);

        Ok((
            Self {
                method,
                uri,
                version,
                headers,
                body,
            },
            body_end,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_options_request() {
        let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam/h264", RtspVersion::V1_0)
            .header("cseq", "1")
            .header("user-agent", "tst-rtp/0.1");
        let bytes = req.encode();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("OPTIONS rtsp://cam/h264 RTSP/1.0\r\n"));
        assert!(s.contains("Cseq: 1\r\n") || s.contains("CSeq: 1\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parse_200_options_response() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n";
        let (resp, consumed) = RtspResponse::parse(raw).unwrap();
        assert_eq!(consumed, raw.len());
        assert_eq!(resp.status, 200);
        assert_eq!(resp.reason, "OK");
        assert_eq!(resp.version, RtspVersion::V1_0);
        assert_eq!(resp.cseq(), Some(1));
        assert_eq!(
            resp.headers.get("public").unwrap(),
            "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN"
        );
    }

    #[test]
    fn parse_session_id_and_timeout() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 12345678;timeout=60\r\n\r\n";
        let (resp, _) = RtspResponse::parse(raw).unwrap();
        assert_eq!(resp.session_id(), Some("12345678"));
        assert_eq!(resp.session_timeout_secs(), Some(60));
    }

    #[test]
    fn parse_response_with_body() {
        let body = b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=test\r\n";
        let mut raw = format!(
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        let (resp, consumed) = RtspResponse::parse(&raw).unwrap();
        assert_eq!(consumed, raw.len());
        assert_eq!(resp.body.as_ref(), body);
    }

    #[test]
    fn parse_truncated_body_errors() {
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: 100\r\n\r\nshort";
        let e = RtspResponse::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn parse_rejects_unknown_version() {
        let raw = b"RTSP/3.0 200 OK\r\nCSeq: 1\r\n\r\n";
        let e = RtspResponse::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    // --- B1: strict Content-Length (adversarial) ---

    #[test]
    fn parse_rejects_unparseable_content_length() {
        // A non-numeric Content-Length must NOT silently become a 0-length
        // body (request-smuggling / desync); it must error.
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: nope\r\n\r\n";
        let e = RtspResponse::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn parse_rejects_oversized_content_length() {
        // u64::MAX overflows usize parse on 32-bit and exceeds the cap on
        // 64-bit; either way must error, never wrap to 0.
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n";
        let e = RtspResponse::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn parse_rejects_content_length_over_cap() {
        let raw = format!(
            "RTSP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RTSP_BODY_BYTES + 1
        )
        .into_bytes();
        let e = RtspResponse::parse(&raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn parse_rejects_duplicate_content_length() {
        // Two Content-Length headers — classic smuggling vector. Must error,
        // not last-wins into a HashMap.
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: 5\r\nContent-Length: 0\r\n\r\nHELLO";
        let e = RtspResponse::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    /// CORR-05: a 401 whose `WWW-Authenticate` arrives as two header lines
    /// (Digest first, Basic second — RFC 7235 §4.1) must not collapse to
    /// the LAST line. Before the fix the `HashMap` insert was last-wins:
    /// only the Basic line survived and `build_authorization` put
    /// `user:password` on the wire base64-encoded.
    #[test]
    fn two_line_www_authenticate_keeps_digest() {
        use crate::rtsp::auth::build_authorization;
        use secrecy::SecretString;

        let raw = b"RTSP/1.0 401 Unauthorized\r\n\
                    CSeq: 2\r\n\
                    WWW-Authenticate: Digest realm=\"r\", nonce=\"n\"\r\n\
                    WWW-Authenticate: Basic realm=\"r\"\r\n\
                    \r\n";
        let (resp, _) = RtspResponse::parse(raw).unwrap();
        let www = resp
            .headers
            .get("www-authenticate")
            .expect("header present");
        assert_eq!(www, "Digest realm=\"r\", nonce=\"n\", Basic realm=\"r\"");
        let header = build_authorization(
            RtspMethod::Describe,
            "rtsp://cam/x",
            www,
            "u",
            &SecretString::new("p".into()),
            1,
        )
        .unwrap();
        assert!(
            header.starts_with("Digest "),
            "expected Digest, got {header:?}"
        );
    }

    /// The joined value must still yield Digest when Basic is listed FIRST:
    /// `parse_challenges` has to recognise `Digest realm=…` as a new scheme,
    /// not as a parameter named `Digest realm` of the Basic challenge.
    #[test]
    fn two_line_www_authenticate_basic_first_still_prefers_digest() {
        use crate::rtsp::auth::build_authorization;
        use secrecy::SecretString;

        let raw = b"RTSP/1.0 401 Unauthorized\r\n\
                    CSeq: 2\r\n\
                    WWW-Authenticate: Basic realm=\"r\"\r\n\
                    WWW-Authenticate: Digest realm=\"r\", nonce=\"n\"\r\n\
                    \r\n";
        let (resp, _) = RtspResponse::parse(raw).unwrap();
        let www = resp.headers.get("www-authenticate").unwrap();
        let header = build_authorization(
            RtspMethod::Describe,
            "rtsp://cam/x",
            www,
            "u",
            &SecretString::new("p".into()),
            1,
        )
        .unwrap();
        assert!(
            header.starts_with("Digest "),
            "expected Digest, got {header:?}"
        );
    }

    /// `Session` is single-valued (RFC 7826 §18.49): a repeated `Session:`
    /// would otherwise join into an id no server issued.
    #[test]
    fn parse_rejects_duplicate_session() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: abc\r\nSession: def\r\n\r\n";
        let e = RtspResponse::parse(raw).unwrap_err();
        assert!(
            matches!(
                e,
                RtspError::BadResponse {
                    detail: "duplicate Session header"
                }
            ),
            "got {e:?}"
        );
    }

    // The client interleaved pump's cap-check (`pump_accumulation_exceeded`
    // -> `rtsp_frame_decision`) and its boundary scanner
    // (`scan_rtsp_message_boundary` in `rtsp::client::interleaved_pump`)
    // both share `content_length_from_header_text`. Lock its behavior
    // across every branch here.
    #[test]
    fn content_length_from_header_text_strict() {
        assert_eq!(content_length_from_header_text("CSeq: 1").unwrap(), 0);
        assert_eq!(
            content_length_from_header_text("Content-Length: 5").unwrap(),
            5
        );
        // Case-insensitive header name.
        assert_eq!(
            content_length_from_header_text("content-length: 7").unwrap(),
            7
        );
        assert!(content_length_from_header_text("Content-Length: nope").is_err());
        assert!(content_length_from_header_text("Content-Length: 18446744073709551615").is_err());
        assert!(
            content_length_from_header_text(&format!(
                "Content-Length: {}",
                MAX_RTSP_BODY_BYTES + 1
            ))
            .is_err()
        );
        assert!(content_length_from_header_text("Content-Length: 5\r\nContent-Length: 0").is_err());
    }

    /// B1 review Minor #1: the scanner must tokenize on `\r\n` exactly like the
    /// HashMap parsers — a bare-`\n`-delimited `Content-Length` is NOT a header
    /// line, so it must NOT be picked up (otherwise scanner-vs-HashMap-parser
    /// disagree → parser-differential smuggle seed).
    #[test]
    fn content_length_scanner_ignores_bare_lf_delimited_header() {
        // "X: a\nContent-Length: 5" is a SINGLE \r\n-line whose value happens to
        // contain a `\n`; the embedded "Content-Length: 5" is part of the X
        // value, not its own header. The HashMap parser sees header `x` only.
        let header_text = "X: a\nContent-Length: 5";
        assert_eq!(
            content_length_from_header_text(header_text).unwrap(),
            0,
            "bare-LF-delimited Content-Length must not be treated as a header"
        );
        // Confirm the HashMap parser agrees (no content-length header present).
        let raw = b"RTSP/1.0 200 OK\r\nX: a\nContent-Length: 5\r\n\r\n";
        let (resp, _) = RtspResponse::parse(raw).unwrap();
        assert!(!resp.headers.contains_key("content-length"));
    }

    /// B1 review Minor #2: Content-Length is RFC 7826 `1*DIGIT`; a leading `+`
    /// (which `usize::from_str` would accept) must be rejected.
    #[test]
    fn content_length_rejects_leading_plus() {
        assert!(parse_content_length(Some("+5")).is_err());
        assert!(content_length_from_header_text("Content-Length: +5").is_err());
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: +5\r\n\r\n";
        assert!(RtspResponse::parse(raw).is_err());
        let req = b"SETUP rtsp://x/y RTSP/1.0\r\nCSeq: 2\r\nContent-Length: +5\r\n\r\n";
        assert!(RtspRequest::parse(req).is_err());
    }

    // --- B6: reject CR/LF/NUL/control bytes in header names and values
    // (RTSP header/request injection — the "User-Agent CRLF injection") ---

    /// A header VALUE carrying a CRLF + an injected header must be rejected
    /// by the checked encode path, NOT serialized verbatim (which would
    /// smuggle an attacker-chosen `Authorization:` line onto the wire).
    #[test]
    fn encode_checked_rejects_crlf_in_value() {
        let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam/h264", RtspVersion::V1_0)
            .header("cseq", "1")
            .header("user-agent", "ok\r\nAuthorization: Basic injected");
        let e = req.encode_checked().unwrap_err();
        assert!(matches!(e, RtspError::InvalidHeader { .. }));
    }

    /// A bare CR or bare LF (not a full CRLF) is equally an injection — a
    /// lone `\n` ends a header line on most servers. Reject both.
    #[test]
    fn encode_checked_rejects_bare_cr_and_lf() {
        for bad in ["ok\rx", "ok\nx"] {
            let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam", RtspVersion::V1_0)
                .header("user-agent", bad);
            assert!(
                matches!(req.encode_checked(), Err(RtspError::InvalidHeader { .. })),
                "bare control byte in {bad:?} must be rejected"
            );
        }
    }

    /// A NUL or other ASCII control byte in a value is rejected (defense in
    /// depth; some intermediaries truncate at NUL).
    #[test]
    fn encode_checked_rejects_nul_and_control_in_value() {
        for bad in ["ok\0x", "ok\x07x", "ok\tx"] {
            let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam", RtspVersion::V1_0)
                .header("user-agent", bad);
            assert!(
                matches!(req.encode_checked(), Err(RtspError::InvalidHeader { .. })),
                "control byte in {bad:?} must be rejected"
            );
        }
    }

    /// The injection can also ride on the Authorization header (built from
    /// caller-supplied username/realm/password) — same central check covers it.
    #[test]
    fn encode_checked_rejects_crlf_in_authorization_value() {
        let req = RtspRequest::new(RtspMethod::Describe, "rtsp://cam", RtspVersion::V1_0)
            .header("cseq", "2")
            .header("authorization", "Basic abc\r\nX-Evil: 1");
        assert!(matches!(
            req.encode_checked(),
            Err(RtspError::InvalidHeader { .. })
        ));
    }

    /// A control byte in the header NAME is rejected too (a custom-header
    /// caller could otherwise inject via the name side).
    #[test]
    fn encode_checked_rejects_control_in_name() {
        let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam", RtspVersion::V1_0)
            .header("x-bad\r\nevil", "1");
        assert!(matches!(
            req.encode_checked(),
            Err(RtspError::InvalidHeader { .. })
        ));
    }

    /// Raw-wire assertion: the encoded bytes of a request whose User-Agent
    /// carries an injected CRLF must NOT contain the attacker's header. The
    /// checked path errors (nothing reaches the wire); confirm explicitly.
    #[test]
    fn encode_checked_no_injected_crlf_on_wire() {
        let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam", RtspVersion::V1_0)
            .header("cseq", "1")
            .header("user-agent", "ok\r\nAuthorization: Basic injected");
        // The checked encoder must refuse to produce any bytes.
        assert!(req.encode_checked().is_err());
    }

    /// The request-line URI is validated too: a CR/LF in the URI splits the
    /// request line exactly like a header CRLF. Under Theme B's hostile-server
    /// model the SETUP/PLAY URI can derive from a server-provided SDP control
    /// URL (`a=control:`), so a malicious server could inject here. The checked
    /// encoder must reject it before producing any bytes.
    #[test]
    fn encode_checked_rejects_crlf_in_request_uri() {
        let req = RtspRequest::new(
            RtspMethod::Setup,
            "rtsp://cam/track1\r\nEvil: injected",
            RtspVersion::V1_0,
        )
        .header("cseq", "2");
        let e = req.encode_checked().unwrap_err();
        assert!(matches!(e, RtspError::InvalidHeader { .. }));
        // Raw-wire: the checked encoder produces NO bytes — no split request line.
        assert!(req.encode_checked().is_err());
    }

    /// Realistic vector: a server-provided SDP control URL carrying a CRLF,
    /// after the client substitutes it as the SETUP request-line URI, is
    /// rejected before any SETUP byte is sent. This mirrors the substitution
    /// `RtspSession::setup` performs (an absolute `rtsp://` control URL is used
    /// verbatim as the request URI).
    #[test]
    fn encode_checked_rejects_malicious_sdp_control_url() {
        // Stand in for `media.control = Some("rtsp://cam/evil\r\n...")` flowing
        // into `setup_uri` and then into the SETUP request line.
        let malicious_control_url = "rtsp://cam/track1\r\nAuthorization: Basic injected";
        let req = RtspRequest::new(
            RtspMethod::Setup,
            malicious_control_url.to_string(),
            RtspVersion::V1_0,
        )
        .header("cseq", "3")
        .header("user-agent", "tst-rtp/0.1");
        assert!(matches!(
            req.encode_checked(),
            Err(RtspError::InvalidHeader { .. })
        ));
    }

    /// A clean request still encodes fine through the checked path (no false
    /// positives on normal traffic with SP/HT-free single-line values).
    #[test]
    fn encode_checked_accepts_clean_request() {
        let req = RtspRequest::new(RtspMethod::Options, "rtsp://cam/h264", RtspVersion::V1_0)
            .header("cseq", "1")
            .header("user-agent", "tst-rtp/0.1")
            .header("transport", "RTP/AVP/TCP;unicast;interleaved=0-1");
        let bytes = req.encode_checked().expect("clean request must encode");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("User-Agent: tst-rtp/0.1\r\n"));
    }
}

#[cfg(test)]
mod request_parse_tests {
    use super::*;

    #[test]
    fn parse_options_request() {
        let raw = b"OPTIONS rtsp://server/live RTSP/1.0\r\n\
                    CSeq: 1\r\n\
                    User-Agent: test\r\n\
                    \r\n";
        let (req, n) = RtspRequest::parse(raw).unwrap();
        assert_eq!(req.method, RtspMethod::Options);
        assert_eq!(req.uri, "rtsp://server/live");
        assert_eq!(req.headers.get("cseq").map(String::as_str), Some("1"));
        assert_eq!(n, raw.len());
    }

    #[test]
    fn parse_describe_request_no_body() {
        let raw = b"DESCRIBE rtsp://x/y RTSP/2.0\r\nCSeq: 5\r\n\r\n";
        let (req, n) = RtspRequest::parse(raw).unwrap();
        assert_eq!(req.method, RtspMethod::Describe);
        assert_eq!(req.version, RtspVersion::V2_0);
        assert_eq!(n, raw.len());
    }

    #[test]
    fn parse_setup_with_transport_header() {
        let raw = b"SETUP rtsp://x/y RTSP/1.0\r\n\
                    CSeq: 3\r\n\
                    Transport: RTP/AVP;unicast;client_port=5004-5005\r\n\
                    \r\n";
        let (req, _) = RtspRequest::parse(raw).unwrap();
        assert_eq!(req.method, RtspMethod::Setup);
        let t = req.headers.get("transport").unwrap();
        assert!(t.contains("client_port=5004-5005"));
    }

    #[test]
    fn parse_unsupported_method_errors() {
        let raw = b"FOOBAR rtsp://x RTSP/1.0\r\n\r\n";
        let e = RtspRequest::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn parse_truncated_no_crlfcrlf_errors() {
        let raw = b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n";
        let e = RtspRequest::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn parse_request_with_body() {
        let body = b"v=0\r\ns=test\r\n";
        let mut raw = format!(
            "SETUP rtsp://x/y RTSP/1.0\r\nCSeq: 2\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        let (req, n) = RtspRequest::parse(&raw).unwrap();
        assert_eq!(req.method, RtspMethod::Setup);
        assert_eq!(req.body.as_ref(), body);
        assert_eq!(n, raw.len());
    }

    #[test]
    fn encode_response_round_trips_minimal() {
        let mut headers = HashMap::new();
        headers.insert("cseq".into(), "1".into());
        let resp = RtspResponse {
            version: RtspVersion::V1_0,
            status: 200,
            reason: "OK".into(),
            headers,
            body: Bytes::new(),
        };
        let wire = resp.encode();
        let text = std::str::from_utf8(&wire).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(text.contains("Cseq: 1\r\n") || text.contains("CSeq: 1\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn encode_response_with_body_appends_body() {
        let body = b"v=0\r\ns=test\r\n";
        let mut headers = HashMap::new();
        headers.insert("cseq".into(), "2".into());
        headers.insert("content-type".into(), "application/sdp".into());
        headers.insert("content-length".into(), body.len().to_string());
        let resp = RtspResponse {
            version: RtspVersion::V1_0,
            status: 200,
            reason: "OK".into(),
            headers,
            body: Bytes::copy_from_slice(body),
        };
        let wire = resp.encode();
        // Should end with the body bytes (not CRLFCRLF).
        assert!(wire.ends_with(body));
        // And the CRLFCRLF header terminator should be present somewhere inside.
        assert!(wire.windows(4).any(|w| w == b"\r\n\r\n"));
    }

    // --- B1: strict Content-Length (adversarial) ---

    #[test]
    fn request_rejects_unparseable_content_length() {
        let raw = b"SETUP rtsp://x/y RTSP/1.0\r\nCSeq: 2\r\nContent-Length: nope\r\n\r\n";
        let e = RtspRequest::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn request_rejects_oversized_content_length() {
        let raw =
            b"SETUP rtsp://x/y RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 18446744073709551615\r\n\r\n";
        let e = RtspRequest::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn request_rejects_content_length_over_cap() {
        let raw = format!(
            "SETUP rtsp://x/y RTSP/1.0\r\nCSeq: 2\r\nContent-Length: {}\r\n\r\n",
            MAX_RTSP_BODY_BYTES + 1
        )
        .into_bytes();
        let e = RtspRequest::parse(&raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    #[test]
    fn request_rejects_duplicate_content_length() {
        let raw =
            b"SETUP rtsp://x/y RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 5\r\nContent-Length: 0\r\n\r\nHELLO";
        let e = RtspRequest::parse(raw).unwrap_err();
        assert!(matches!(e, RtspError::BadResponse { .. }));
    }

    /// CORR-05, request side: a repeated list header is joined, not last-wins.
    #[test]
    fn request_joins_repeated_list_header() {
        let raw = b"OPTIONS rtsp://x/y RTSP/1.0\r\nCSeq: 1\r\nRequire: a\r\nRequire: b\r\n\r\n";
        let (req, _) = RtspRequest::parse(raw).unwrap();
        assert_eq!(req.headers.get("require").map(String::as_str), Some("a, b"));
    }

    /// `CSeq` is single-valued (RFC 7826 §18.20): a repeat is an error, never
    /// a joined `"1, 2"` that would be echoed into the response.
    #[test]
    fn request_rejects_duplicate_cseq() {
        let raw = b"OPTIONS rtsp://x/y RTSP/1.0\r\nCSeq: 1\r\nCSeq: 2\r\n\r\n";
        let e = RtspRequest::parse(raw).unwrap_err();
        assert!(
            matches!(
                e,
                RtspError::BadResponse {
                    detail: "duplicate CSeq header"
                }
            ),
            "got {e:?}"
        );
    }
}
