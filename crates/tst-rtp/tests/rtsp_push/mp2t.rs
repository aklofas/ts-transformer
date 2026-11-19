//! An MPEG-TS push: what `send_ts` sends reaches the publish mount byte
//! for byte, and closing the push frees the mount.

use tst_rtp::PushDescription;

use crate::common::*;
use crate::fixtures::raw_rtsp_publisher::ts_fixture_packets;

#[test]
fn an_mp2t_push_reaches_the_publish_mount_and_close_frees_it() {
    let s = Served::new();
    let mut app = s.app();
    let mut push = s.push(&PushDescription::Mp2t);
    assert!(s.mount.publisher().is_some());
    let ts: Vec<u8> = ts_fixture_packets(210).concat();
    for bundle in ts.chunks(7 * 188) {
        push.send_ts(bundle).unwrap();
    }
    let got = read_ts(&mut app, ts.len());
    assert_same_bytes(&got, &ts, "the mount's stream");
    assert_eq!(s.mount.stats().malformed_packets, 0);

    push.close().unwrap();
    s.until_no_publisher();
}
