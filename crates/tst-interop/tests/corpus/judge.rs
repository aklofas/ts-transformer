use tst_interop::corpus::digest::{FileDigest, digest_bytes};
use tst_interop::corpus::judge::{ReceiverOutcome, judge};

use crate::{gen_profile_bytes, sha_of};

fn digest() -> FileDigest {
    let bytes = gen_profile_bytes("baseline", 2.0);
    digest_bytes(&sha_of(&bytes), &bytes, None).unwrap()
}

fn first_pid(d: &FileDigest) -> u16 {
    *d.configs["default"]
        .pids
        .iter()
        .find(|(_, p)| p.samples > 1)
        .unwrap()
        .0
}

#[test]
fn equal_digests_pass_on_every_path() {
    let d = digest();
    for path in ["srt", "rist", "udp", "tcp", "rtsp-publish"] {
        let v = judge(path, &d, Some(&d), &ReceiverOutcome::Completed);
        assert!(v.pass, "{path}: {:?}", v.failures);
        assert!(v.failures.is_empty());
        assert_eq!(v.path, path);
        assert_eq!(v.sha256, d.sha256);
    }
}

#[test]
fn a_missing_sample_fails_naming_the_pid() {
    let d = digest();
    let pid = first_pid(&d);
    let mut got = d.clone();
    got.configs
        .get_mut("default")
        .unwrap()
        .pids
        .get_mut(&pid)
        .unwrap()
        .samples -= 1;
    let v = judge("srt", &d, Some(&got), &ReceiverOutcome::Completed);
    assert!(!v.pass);
    assert_eq!(
        v.failures,
        [format!(
            "delivery incomplete: 1 sample(s) short on pid {pid}"
        )]
    );

    let mut got = d.clone();
    got.configs
        .get_mut("default")
        .unwrap()
        .pids
        .get_mut(&pid)
        .unwrap()
        .discontinuities += 1;
    let v = judge("udp", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, [format!("pid {pid}: discontinuities differs")]);

    let mut got = d.clone();
    got.configs.get_mut("default").unwrap().klv_records_sha256 = sha_of(b"other");
    let v = judge("tcp", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, ["klv records differ"]);
}

#[test]
fn rtsp_publish_compares_the_elementary_hash_and_klv_count_only() {
    let d = digest();
    let mut got = d.clone();
    let pid = first_pid(&d);
    got.configs
        .get_mut("default")
        .unwrap()
        .pids
        .get_mut(&pid)
        .unwrap()
        .samples = 1;
    got.configs.get_mut("default").unwrap().klv_records_sha256 = sha_of(b"retimed");
    assert!(judge("rtsp-publish", &d, Some(&got), &ReceiverOutcome::Completed).pass);

    got.elementary_sha256 = sha_of(b"other");
    let v = judge("rtsp-publish", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, ["elementary streams differ"]);

    let mut got = d.clone();
    got.configs.get_mut("default").unwrap().klv_records += 1;
    let v = judge("rtsp-publish", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, ["klv record count differs"]);
}

#[test]
fn a_demux_error_fails_on_every_path() {
    let d = digest();
    let mut got = d.clone();
    got.configs.get_mut("default").unwrap().feed_errors = 2;
    for path in ["srt", "rtsp-publish"] {
        let v = judge(path, &d, Some(&got), &ReceiverOutcome::Completed);
        assert!(
            v.failures.contains(&"receiver demux error".to_string()),
            "{path}"
        );
    }
}

#[test]
fn a_receiver_that_did_not_complete_fails_with_one_failure() {
    let d = digest();
    let cases = [
        (ReceiverOutcome::Stalled, "receiver stalled"),
        (ReceiverOutcome::Exited, "receiver exited"),
        (
            ReceiverOutcome::Error("boom".into()),
            "receiver error: boom",
        ),
    ];
    for (o, text) in cases {
        let v = judge("srt", &d, Some(&d), &o);
        assert!(!v.pass);
        assert_eq!(v.failures, [text]);
    }
    let v = judge("srt", &d, None, &ReceiverOutcome::Completed);
    assert_eq!(v.failures, ["no received digest"]);
}

#[test]
fn a_missing_or_extra_pid_and_a_missing_config_fail() {
    let d = digest();
    let pid = first_pid(&d);
    let mut got = d.clone();
    let moved = got
        .configs
        .get_mut("default")
        .unwrap()
        .pids
        .remove(&pid)
        .unwrap();
    let v = judge("srt", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, [format!("pid {pid}: missing")]);

    let mut got = d.clone();
    got.configs
        .get_mut("default")
        .unwrap()
        .pids
        .insert(0x1FF1, moved);
    let v = judge("srt", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, ["pid 8177: unexpected"]);

    let mut got = d.clone();
    got.configs.remove("default");
    let v = judge("srt", &d, Some(&got), &ReceiverOutcome::Completed);
    assert_eq!(v.failures, ["digest has no default configuration"]);
}

/// Each field of a PID digest, changed in turn, fails the judge naming it.
#[test]
fn every_pid_digest_field_is_judged() {
    use tst_interop::corpus::digest::PidDigest;

    let d = digest();
    let pid = first_pid(&d);
    type Mutation = (&'static str, fn(&mut PidDigest));
    let mutations: Vec<Mutation> = vec![
        ("kind", |p| p.kind.push('x')),
        ("metadata", |p| p.metadata += 1),
        ("discontinuities", |p| p.discontinuities += 1),
        ("pes_oversize", |p| p.pes_oversize += 1),
        ("nonconformant", |p| {
            p.nonconformant.insert("Probe".into(), 1);
        }),
        ("pts_first", |p| {
            p.pts_first = Some(p.pts_first.unwrap_or(0) + 1)
        }),
        ("pts_last", |p| {
            p.pts_last = Some(p.pts_last.unwrap_or(0) + 1)
        }),
        ("pts_absent", |p| p.pts_absent += 1),
        ("pts_backward_breaks", |p| p.pts_backward_breaks += 1),
        ("klv_unknown", |p| p.klv_unknown += 1),
        ("klv_unknown_reason", |p| {
            p.klv_unknown_reason = Some("why".into())
        }),
        ("unknown_samples", |p| p.unknown_samples += 1),
        ("payload_sha256", |p| p.payload_sha256 = sha_of(b"other")),
    ];
    for (name, mutate) in mutations {
        let mut got = d.clone();
        mutate(
            got.configs
                .get_mut("default")
                .unwrap()
                .pids
                .get_mut(&pid)
                .unwrap(),
        );
        let v = judge("srt", &d, Some(&got), &ReceiverOutcome::Completed);
        assert_eq!(v.failures, [format!("pid {pid}: {name} differs")], "{name}");
    }
}

/// A capture whose PES lost their PTS differs from the intact one, and the
/// judge names the absent-PTS count of the KLV PID.
#[test]
fn a_receiver_that_loses_timestamps_fails_the_judge() {
    use tst_interop::corpus::digest::digest_bytes;

    let intact = crate::invariants::klv_every_tenth_of_a_second();
    let (stripped, _) = crate::invariants::klv_with_stripped_pts();
    let want = digest_bytes(&sha_of(&intact), &intact, None).unwrap();
    let got = digest_bytes(&sha_of(&stripped), &stripped, None).unwrap();
    let v = judge("udp", &want, Some(&got), &ReceiverOutcome::Completed);
    assert!(!v.pass);
    assert_eq!(
        v.failures.first().map(String::as_str),
        Some("pid 257: pts_absent differs"),
        "{:?}",
        v.failures
    );
}
