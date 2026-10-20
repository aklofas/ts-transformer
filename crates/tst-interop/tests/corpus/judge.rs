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
