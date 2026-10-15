//! RFC 7826 §14 interleaved-frame rule shared by the client pump
//! (`rtsp::client::interleaved_pump`) and the server session loop
//! (`rtsp::server::session`, publisher direction). The RTSP-message boundary
//! rule is `rtsp::message::rtsp_frame_decision`. Pure functions: no socket,
//! thread or channel.

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
}
