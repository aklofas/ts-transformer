//! An H.264 push without KLV: every access unit the push sends comes out of
//! the mount's stream with its slices and its PTS step, the first one
//! behind the parameter sets of the description.

use crate::common::*;
use crate::fixtures::publish_mount::video_samples;

#[test]
fn h264_alone_arrives_with_every_access_unit() {
    const AUS: usize = 30;
    let s = Served::ending_on_exit();
    let app = s.app();
    let mut push = s.push(&elementary(false));
    let sent: Vec<Vec<u8>> = (0..AUS).map(access_unit).collect();
    for (i, au) in sent.iter().enumerate() {
        push.send_video(au, pts(9_000 + i as i64 * AU_STEP))
            .unwrap();
    }
    push.close().unwrap();

    let samples = video_samples(&demux_to_end(app));
    assert_eq!(samples.len(), AUS, "video samples demuxed");
    let first_pts = samples[0].0;
    for (i, ((pts, raw, random_access), au)) in samples.iter().zip(&sent).enumerate() {
        assert_eq!(slices(raw), slices(au), "the slices of access unit {i}");
        assert_eq!(
            pts - first_pts,
            i as i64 * AU_STEP,
            "PTS of access unit {i}"
        );
        assert_eq!(
            *random_access,
            i == 0,
            "random access flag of access unit {i}"
        );
    }
    // The server puts the description's parameter sets in front of the
    // key frame; the push sent them only in the description.
    let key_frame = &samples[0].1;
    assert!(contains(key_frame, &sps()), "the key frame lacks the SPS");
    assert!(contains(key_frame, &pps()), "the key frame lacks the PPS");
    let stats = s.mount.stats();
    assert_eq!(stats.aus_emitted, AUS as u64);
    assert_eq!(stats.aus_dropped, 0);
    assert_eq!(stats.malformed_packets, 0);
}
