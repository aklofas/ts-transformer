//! Publisher direction of [`RtspServer`](crate::rtsp::server::RtspServer):
//! ANNOUNCE / SETUP `mode=record` / RECORD (RFC 2326 §10.3, §10.11, §12.39).
//! A published mount re-serves PLAY readers and hands the application an
//! [`RtpRecvTransport`](crate::transport::RtpRecvTransport).

pub(crate) mod adapter;
pub(crate) mod adapter_es;
pub(crate) mod align;
pub(crate) mod handlers;
pub(crate) mod klv_depacketizer;
pub(crate) mod mount;
pub(crate) mod session;
pub(crate) mod shape;
pub(crate) mod udp_ingest;

// Hidden: `pub` only so the fuzz workspace's `rtp_klv_depacketize`
// target reaches the depacketizer; not part of the supported surface.
#[doc(hidden)]
pub use klv_depacketizer::{KlvDepacketizer, KlvUnit};
pub use mount::{ClockAlignment, PublishMountHandle, PublishMountStats, PublisherInfo};
// Hidden: `pub` only so the fuzz workspace's `sdp_announce_classify`
// target reaches the ANNOUNCE classifier; not part of the supported surface.
#[doc(hidden)]
pub use shape::{AnnounceShape, ShapeReject, classify_announce};

/// Wire shape a publisher announced (§1 classification table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublishShape {
    /// One MPEG-TS-over-RTP track (RFC 2250, PT 33 or a dynamic PT mapped to `MP2T/90000`).
    Mp2t,
    /// Elementary tracks: one H.264 (RFC 6184) video track, optionally one KLV (RFC 6597) track.
    Elementary {
        /// The announce carries a KLV (RFC 6597, `smpte336m`) track beside
        /// the video.
        klv: bool,
    },
}

/// Drain every complete interleaved frame (`$<channel><len_be16><payload>`,
/// RFC 2326 §10.12) from the head of a publisher session's read buffer,
/// handing each frame's channel and payload to `route` in wire order.
///
/// Returns `true` when the head is no longer a `$` frame: `buf` is empty or
/// begins with an RTSP message, which the caller frames next. Returns
/// `false` when the head is a `$` frame whose bytes have not all arrived;
/// the caller reads more before calling again. Bytes after the drained
/// frames are left in `buf` untouched.
//
// Hidden: `pub` only so the fuzz workspace's `rtsp_server_publish_framing`
// target runs the server session loop's own framing step; not part of the
// supported surface.
#[doc(hidden)]
pub fn drain_interleaved_head(buf: &mut Vec<u8>, route: &mut dyn FnMut(u8, &[u8])) -> bool {
    while buf.first() == Some(&b'$') {
        let Some((channel, total_len)) = crate::rtsp::framing::parse_binary_frame_header(buf)
        else {
            return false;
        };
        route(channel, &buf[4..total_len]);
        buf.drain(..total_len);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::drain_interleaved_head;

    #[test]
    fn a_partial_frame_at_the_head_waits_and_drains_nothing() {
        let mut buf = b"$\x00\x00\x10abc".to_vec(); // declares 16 bytes, holds 3
        let mut routed = Vec::new();
        let mut route = |ch: u8, p: &[u8]| routed.push((ch, p.to_vec()));
        assert!(!drain_interleaved_head(&mut buf, &mut route), "needs more");
        assert_eq!(buf, b"$\x00\x00\x10abc", "nothing drained");
        buf.extend_from_slice(&[0u8; 13]);
        assert!(drain_interleaved_head(&mut buf, &mut route));
        // Exactly one frame, and only after the rest arrived.
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].0, 0);
        assert_eq!(routed[0].1.len(), 16);
        assert!(buf.is_empty());
    }

    #[test]
    fn frames_before_an_rtsp_request_are_drained_and_the_request_is_left() {
        let mut buf = b"$\x01\x00\x02ab$\x00\x00\x01cOPTIONS * RTSP/1.0\r\n\r\n".to_vec();
        let mut routed = Vec::new();
        let mut route = |ch: u8, p: &[u8]| routed.push((ch, p.to_vec()));
        assert!(drain_interleaved_head(&mut buf, &mut route));
        assert_eq!(routed, vec![(1u8, b"ab".to_vec()), (0u8, b"c".to_vec())]);
        assert_eq!(buf, b"OPTIONS * RTSP/1.0\r\n\r\n");
    }
}
