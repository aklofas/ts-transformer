//! The application transport a publish mount hands out meets the shared
//! receive-side contract (`tst_core::transport::conformance`).

use std::cell::{Cell, RefCell};

use tst_core::transport::conformance::{BrokenSource, assert_recv_contract};
use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp_publisher::*;

/// One server, one fresh mount per row (the transport is take-once per
/// mount). `feed` (used by `empty_recv_is_noop`) drives a REAL publisher
/// into the newest mount — `emit` is crate-private, and an ANNOUNCE/RECORD
/// over interleaved is the honest way in. The publishers are kept alive in
/// `publishers` until the test ends so none of them ends mid-row.
///
/// `BrokenSource::NotProducible`: a publisher ending (TEARDOWN, dropped
/// connection) does NOT end this transport by design — the mount outlives
/// its publishers (see `lifecycle::publisher_teardown_leaves_app_transport_open`)
/// — so `not_alive_after_broken` and `peer_eof_is_not_a_cancel` have no
/// peer-side break to observe and print their skip lines.
#[test]
fn publish_transport_meets_the_recv_contract() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let n = Cell::new(0usize);
    let latest = RefCell::new(String::new());
    let publishers = RefCell::new(Vec::new());

    let factory = || {
        n.set(n.get() + 1);
        let path = format!("/k{}", n.get());
        let t = server
            .add_publish_mount(&path)
            .unwrap()
            .into_recv_transport()
            .unwrap();
        *latest.borrow_mut() = path;
        t
    };
    let feed = |payload: &[u8]| {
        let path = latest.borrow().clone();
        let mut p = RawPublisher::connect(port);
        assert_eq!(p.announce(&path, SDP_MP2T), 200);
        let (status, ch) = p.setup_interleaved(&path, "streamid=0");
        assert_eq!(status, 200);
        assert_eq!(p.record(&path), 200);
        p.send_frame(ch.unwrap(), &rtp_wrap(0, 0, 0xC0F, 33, payload));
        publishers.borrow_mut().push(p);
    };
    assert_recv_contract(factory, feed, BrokenSource::NotProducible);
    drop(publishers);
    server.stop().ok();
}
