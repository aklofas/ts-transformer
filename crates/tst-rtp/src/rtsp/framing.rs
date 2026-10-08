//! RFC 7826 §14 interleaved-frame and RTSP-message boundary rules shared by
//! the client pump (`rtsp::client::interleaved_pump`) and the server session
//! loop (`rtsp::server::session`, publisher direction). Pure functions: no
//! socket, thread or channel.

use crate::rtsp::message::content_length_from_header_text;

/// Look for a complete `$<channel><len_be16><payload>` binary
/// interleaved frame (RFC 7826 §14) at the start of `buf`.
///
/// Returns `None` if `buf` doesn't start with `$` or doesn't yet hold
/// the full `4 + length` bytes (the pump reads more and retries). On
/// success, returns `(channel, total_len)` — `buf[4..total_len]` is the
/// frame's payload and `buf[..total_len]` is the whole frame to drain.
//
// `#[doc(hidden)] pub` so `tst-rtp-fuzz`'s `rtsp_client_pump_framing`
// target can drive this exact parsing rule without a live socket/thread
// — see that target for the harness. Not part of the crate's stable
// public API.
#[doc(hidden)]
pub fn parse_binary_frame_header(buf: &[u8]) -> Option<(u8, usize)> {
    if buf.first() != Some(&b'$') || buf.len() < 4 {
        return None;
    }
    let channel = buf[1];
    let length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let total_len = 4 + length;
    if buf.len() < total_len {
        return None;
    }
    Some((channel, total_len))
}

/// Outcome of scanning for a complete RTSP message (headers terminated
/// by `CRLFCRLF` + `Content-Length` body) at the start of `buf`. See
/// [`scan_rtsp_message_boundary`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtspFrameBoundary {
    /// `buf` doesn't yet hold a complete message.
    Incomplete,
    /// The header block (up to and including `CRLFCRLF`) is not valid
    /// UTF-8. Treated as a resync point rather than a fatal error —
    /// skip `skip` bytes and keep reading.
    NonUtf8Headers { skip: usize },
    /// `Content-Length` is missing/unparseable/oversized/duplicated.
    BadContentLength { detail: &'static str },
    /// `end + 4 + content_length` overflowed `usize`. Unreachable in
    /// practice (both operands are capped well below `usize::MAX` by
    /// `pump_accumulation_exceeded` / `content_length_from_header_text`)
    /// but checked for defense in depth.
    LengthOverflow,
    /// A complete message occupies `buf[..len]`.
    Complete { len: usize },
}

/// Scan `buf` for the next complete RTSP message boundary.
//
// `#[doc(hidden)] pub` — see [`parse_binary_frame_header`]'s doc for why.
#[doc(hidden)]
pub fn scan_rtsp_message_boundary(buf: &[u8]) -> RtspFrameBoundary {
    let end = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(e) => e,
        None => return RtspFrameBoundary::Incomplete,
    };
    let header_text = match std::str::from_utf8(&buf[..end]) {
        Ok(s) => s,
        Err(_) => return RtspFrameBoundary::NonUtf8Headers { skip: end + 4 },
    };
    let content_length = match content_length_from_header_text(header_text) {
        Ok(n) => n,
        Err(detail) => return RtspFrameBoundary::BadContentLength { detail },
    };
    let len = match end
        .checked_add(4)
        .and_then(|e| e.checked_add(content_length))
    {
        Some(m) => m,
        None => return RtspFrameBoundary::LengthOverflow,
    };
    if buf.len() < len {
        return RtspFrameBoundary::Incomplete;
    }
    RtspFrameBoundary::Complete { len }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_frame_header_needs_four_bytes_and_full_body() {
        assert_eq!(parse_binary_frame_header(b"$\x01\x00"), None);
        assert_eq!(parse_binary_frame_header(b"$\x01\x00\x02a"), None);
        assert_eq!(parse_binary_frame_header(b"$\x01\x00\x02ab"), Some((1, 6)));
        assert_eq!(parse_binary_frame_header(b"OPTIONS"), None);
    }

    #[test]
    fn rtsp_boundary_complete_with_body() {
        let msg = b"RTSP/1.0 200 OK\r\nContent-Length: 3\r\n\r\nabc";
        assert_eq!(
            scan_rtsp_message_boundary(msg),
            RtspFrameBoundary::Complete { len: msg.len() }
        );
        assert_eq!(
            scan_rtsp_message_boundary(&msg[..msg.len() - 1]),
            RtspFrameBoundary::Incomplete
        );
    }
}
