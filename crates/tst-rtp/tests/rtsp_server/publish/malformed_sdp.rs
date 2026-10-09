//! A malformed SDP body in ANNOUNCE is a 400 and the session survives:
//! the same connection answers OPTIONS and then publishes with a valid SDP.

use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp_publisher::*;

/// The smallest input known to make an SDP parser assert internally (a
/// version line, a non-UTF-8 line, then an empty-type `=` line).
const MALFORMED_SDP: &[u8] = b"v=0\n\xff\n\0=";

#[test]
fn malformed_announce_sdp_is_400_and_the_session_survives() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let _mount = server.add_publish_mount("/pub").unwrap();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce_bytes("/pub", MALFORMED_SDP), 400);
    // Same connection: the session task is still serving requests.
    assert_eq!(p.options("/pub"), 200);
    assert_eq!(p.announce("/pub", SDP_MP2T), 200);
    let (status, _) = p.setup_interleaved("/pub", "streamid=0");
    assert_eq!(status, 200);
    assert_eq!(p.record("/pub"), 200);

    drop(p);
    server.stop().ok();
}
