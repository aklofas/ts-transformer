use tst_interop::corpus::replay::judge_rss;
use tst_interop::report::stress::parse_rss_csv;

/// An `rss.csv` of `udp`'s receiver sampled every 10 s for two hours,
/// growing by `kb_per_hour`, and two samples of `tcp`'s.
fn csv(kb_per_hour: f64) -> String {
    let mut s = String::from("elapsed_s,leg,process,pid,rss_kb\n");
    for k in 0..=720 {
        let t = f64::from(k) * 10.0;
        let kb = 50_000.0 + kb_per_hour * t / 3600.0;
        s.push_str(&format!("{t:.3},udp,recv,100,{}\n", kb.round() as u64));
    }
    s.push_str("0.000,tcp,recv,101,40000\n10.000,tcp,recv,101,90000\n");
    s
}

fn judged(kb_per_hour: f64) -> tst_interop::corpus::replay::RssJudgement {
    let samples: Vec<(String, f64, u64)> = parse_rss_csv(&csv(kb_per_hour))
        .unwrap()
        .into_iter()
        .map(|r| (r.leg, r.elapsed_s, r.rss_kb.unwrap()))
        .collect();
    judge_rss(&samples, &["udp".to_string(), "tcp".to_string()], 10.0)
}

#[test]
fn a_flat_rss_is_noted_within_the_soak_threshold() {
    let j = judged(0.0);
    assert!(j.judged_by_path["udp"]);
    let note = &j.note_by_path["udp"];
    assert!(note.contains("within the soak's threshold 200"), "{j:?}");
    assert!(note.ends_with("(recorded, not gated)"), "{j:?}");
}

#[test]
fn a_rising_rss_is_noted_above_the_soak_threshold_and_nothing_fails() {
    // The judgement carries no pass or fail: the replay's verdict comes
    // from the per-file judgements, the soak gates memory.
    let j = judged(1000.0);
    assert!(j.judged_by_path["udp"]);
    let note = &j.note_by_path["udp"];
    assert!(note.contains("above the soak's threshold 200"), "{j:?}");
    assert!(note.ends_with("(recorded, not gated)"), "{j:?}");
}

#[test]
fn a_path_with_too_few_samples_is_noted_as_such() {
    // tcp's two samples rise by 50 MB, but no slope rule judges two points.
    let j = judged(0.0);
    assert!(!j.judged_by_path["tcp"]);
    assert!(
        j.note_by_path["tcp"].starts_with("too few samples: "),
        "{j:?}"
    );
    assert!(!j.note_by_path["tcp"].contains("above"), "{j:?}");
}

#[test]
fn the_udp_buffer_holds_bursts_of_the_larger_of_the_access_unit_and_the_paced_chunk() {
    use tst_interop::corpus::receive::{
        UDP_RCVBUF_BURST_FACTOR as F, UDP_RCVBUF_FLOOR, udp_rcvbuf_for, udp_rcvbuf_need,
    };
    // Linux stores twice the value set; macOS stores it exactly and charges
    // each datagram's mbuf against it.
    #[cfg(target_os = "linux")]
    assert_eq!((F, UDP_RCVBUF_FLOOR), (2, 256 * 1024));
    #[cfg(not(target_os = "linux"))]
    assert_eq!((F, UDP_RCVBUF_FLOOR), (4, 1024 * 1024));
    // 184 000 bytes of access unit are 1 000 packets: 188 000 on the wire.
    assert_eq!(udp_rcvbuf_need(184_000, 0), 188_000 * F);
    // A paced chunk larger than the access unit sets the burst.
    assert_eq!(udp_rcvbuf_need(184_000, 300_000), 300_000 * F);
    assert_eq!(udp_rcvbuf_need(0, 0), 0);
    // The request never falls below the floor, whatever the platform's
    // default, and is the need above it.
    assert_eq!(udp_rcvbuf_for(0, 0), UDP_RCVBUF_FLOOR);
    assert_eq!(udp_rcvbuf_for(1_000, 50_000), UDP_RCVBUF_FLOOR);
    assert_eq!(udp_rcvbuf_for(184_000, 600_000), 600_000 * F);
}
