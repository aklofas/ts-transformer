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
fn a_flat_rss_passes_the_gate() {
    let j = judged(0.0);
    assert!(j.gated_by_path["udp"]);
    assert!(j.gate_by_path["udp"].starts_with("PASS "), "{j:?}");
    assert!(j.failed.is_empty(), "{j:?}");
}

#[test]
fn a_rising_rss_fails_the_gate() {
    let j = judged(1000.0);
    assert!(j.gated_by_path["udp"]);
    assert!(j.gate_by_path["udp"].starts_with("FAIL "), "{j:?}");
    assert_eq!(j.failed, ["udp"]);
}

#[test]
fn a_path_with_too_few_samples_is_recorded_as_not_gated() {
    // tcp's two samples rise by 50 MB, but no slope rule judges two points.
    let j = judged(0.0);
    assert!(!j.gated_by_path["tcp"]);
    assert!(j.gate_by_path["tcp"].starts_with("not gated: "), "{j:?}");
    assert!(!j.failed.contains(&"tcp".to_string()), "{j:?}");
}

#[test]
fn the_udp_buffer_holds_two_of_the_larger_of_the_access_unit_and_the_paced_chunk() {
    use tst_interop::corpus::receive::udp_rcvbuf_for;
    // 184 000 bytes of access unit are 1 000 packets: 188 000 on the wire.
    assert_eq!(udp_rcvbuf_for(184_000, 0, Some(212_992)), Some(376_000));
    // A paced chunk larger than the access unit sets the burst.
    assert_eq!(
        udp_rcvbuf_for(184_000, 300_000, Some(212_992)),
        Some(600_000)
    );
    // A default that holds two bursts needs nothing; nor does an unknown size.
    assert_eq!(udp_rcvbuf_for(1_000, 50_000, Some(212_992)), None);
    assert_eq!(udp_rcvbuf_for(0, 0, None), None);
    // An unreadable default still sets the buffer.
    assert_eq!(udp_rcvbuf_for(0, 50_000, None), Some(100_000));
}
