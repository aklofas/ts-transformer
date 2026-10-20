use tst_core::klv::checksum::checksum_running_sum_16;
use tst_core::klv::length::{read_ber, read_ber_oid};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_interop::corpus::RunDir;
use tst_interop::corpus::digest::{FileDigest, digest_bytes, klv_record_digest, load, store};
use tst_interop::fixtures::{KlvSet, klv_record_for};
use tst_test_helpers::rawts::{self, classify_packet};

use crate::{gen_profile_bytes, sha_of, temp_dir_named};

fn samples(d: &FileDigest) -> u64 {
    d.configs["default"].pids.values().map(|p| p.samples).sum()
}

#[test]
fn digest_is_deterministic_and_has_three_configs() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let sha = sha_of(&bytes);
    let a = digest_bytes(&sha, &bytes, None).unwrap();
    let b = digest_bytes(&sha, &bytes, None).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        a.configs.keys().cloned().collect::<Vec<_>>(),
        ["default", "strict_full", "unwrap"]
    );
    assert!(a.configs["default"].klv_records > 0);
    assert_eq!(a.configs["default"].klv_unknown, 0);
    assert_eq!(a.bytes, bytes.len() as u64);
}

/// The record's top-level tag/value items, checksum last, reversed and
/// re-emitted with the length and checksum recomputed.
fn reordered(ls: &[u8]) -> Vec<u8> {
    let (len, rest) = read_ber(&ls[16..]).unwrap();
    let body = &rest[..len];
    let mut items: Vec<&[u8]> = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let (_, after_tag) = read_ber_oid(&body[at..]).unwrap();
        let tag_len = body.len() - at - after_tag.len();
        let (vlen, after_len) = read_ber(after_tag).unwrap();
        let len_len = after_tag.len() - after_len.len();
        let end = at + tag_len + len_len + vlen;
        items.push(&body[at..end]);
        at = end;
    }
    let checksum = items.pop().unwrap();
    assert_eq!(checksum[0], 1, "the checksum item is last");
    items.reverse();
    let mut out = ls[..16].to_vec();
    let inner: usize = items.iter().map(|i| i.len()).sum::<usize>() + 4;
    assert!(inner < 128, "short-form BER length keeps the helper small");
    out.push(inner as u8);
    for i in &items {
        out.extend_from_slice(i);
    }
    out.extend_from_slice(&[1, 2]);
    let sum = checksum_running_sum_16(&out);
    out.extend_from_slice(&sum.to_be_bytes());
    out
}

/// A stream carrying `ls` as its only KLV record.
fn one_record_stream(ls: &[u8]) -> Vec<u8> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_klv(0x101, KlvStreamType::PrivateData, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let klv = mux.klv_handles()[0];
    mux.push_klv_to(klv, ls, Pts90khz::new(90_000), 0).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn a_reordered_klv_record_has_the_same_canonical_hash() {
    let ls = klv_record_for(KlvSet::Compact, 0, 3).unwrap();
    let flipped = reordered(&ls);
    assert_ne!(ls, flipped, "the tag order really changed");
    let a = klv_record_digest(0x101, 0, &ls);
    let b = klv_record_digest(0x101, 0, &flipped);
    assert!(
        a.decoded && b.decoded,
        "{:?} {:?}",
        a.unknown_reason,
        b.unknown_reason
    );
    assert_eq!(a.canonical_sha256, b.canonical_sha256);
    assert_eq!(a.tags, b.tags);

    // The same through a stream: the raw payload hashes differ, the KLV hash does not.
    let (sa, sb) = (one_record_stream(&ls), one_record_stream(&flipped));
    let da = digest_bytes(&sha_of(&sa), &sa, None).unwrap();
    let db = digest_bytes(&sha_of(&sb), &sb, None).unwrap();
    let (ca, cb) = (&da.configs["default"], &db.configs["default"]);
    assert_eq!((ca.klv_records, ca.klv_decoded), (1, 1));
    assert_eq!(ca.klv_records_sha256, cb.klv_records_sha256);
    assert_ne!(
        ca.pids[&0x101].payload_sha256,
        cb.pids[&0x101].payload_sha256
    );
}

#[test]
fn an_undecodable_klv_payload_is_hashed_as_it_is() {
    let junk = [0u8; 40];
    let d = klv_record_digest(0x101, 7, &junk);
    assert!(!d.decoded);
    assert!(d.unknown_reason.is_some());
    assert_eq!(d.canonical_sha256, sha_of(&junk));
    assert_eq!((d.pid, d.pts), (0x101, 7));
}

#[test]
fn a_prefix_digest_sees_fewer_samples_than_the_whole_file() {
    let bytes = gen_profile_bytes("baseline", 4.0);
    let whole = digest_bytes(&sha_of(&bytes), &bytes, None).unwrap();
    let cut = digest_bytes(&sha_of(&bytes), &bytes, Some(1)).unwrap();
    assert!(samples(&cut) > 0);
    assert!(samples(&cut) < samples(&whole));
    assert_eq!(cut.prefix_seconds, Some(1));
    assert_eq!(cut.bytes, bytes.len() as u64);
    // A prefix longer than the capture is the whole capture.
    let long = digest_bytes(&sha_of(&bytes), &bytes, Some(600)).unwrap();
    assert_eq!(long.configs, whole.configs);
}

#[test]
fn strict_full_feed_errors_are_counted_not_fatal() {
    let mut bytes = gen_profile_bytes("baseline", 4.0);
    let pcr_pid = rawts::parsed(&bytes).pcr_pid.unwrap();
    let mut seen = 0;
    for pkt in bytes.chunks_exact_mut(188) {
        let info = classify_packet((&*pkt).try_into().unwrap()).unwrap();
        if info.pid == pcr_pid && info.pcr_base.is_some() {
            seen += 1;
            if seen > 1 {
                pkt[6..12].fill(0);
            }
        }
    }
    assert!(seen > 2);
    let d = digest_bytes(&sha_of(&bytes), &bytes, None).unwrap();
    assert!(d.configs["strict_full"].feed_errors > 0);
    assert_eq!(d.configs["default"].feed_errors, 0);
    // The strict pass still delivers what it could and records why it stopped.
    assert!(!d.configs["strict_full"].feed_error_kinds.is_empty());
}

#[test]
fn a_capture_that_does_not_start_on_a_packet_still_digests() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let shifted = &bytes[100..];
    let whole = digest_bytes(&sha_of(&bytes), &bytes, None).unwrap();
    let cut = digest_bytes(&sha_of(shifted), shifted, None).unwrap();
    let (w, c) = (&whole.configs["default"], &cut.configs["default"]);
    assert_eq!(c.feed_errors, 0);
    // The first 100 bytes are the head of packet 0, the PAT. Until the next
    // PAT the demuxer has no PMT, so it loses the first few access units and
    // the KLV record riding with them (3 video samples and 1 record here). Nothing else
    // changes: same PMT count, same end of stream, no timing issues.
    assert!((1..=5).contains(&(samples(&whole) - samples(&cut))));
    assert!((1..=2).contains(&(w.klv_records - c.klv_records)));
    assert_eq!(c.klv_unknown, 0);
    assert_eq!(w.stats, c.stats);
    for (pid, p) in &c.pids {
        assert_eq!(p.pts_last, w.pids[pid].pts_last);
        assert_eq!(p.nonconformant, w.pids[pid].nonconformant);
    }
    // A prefix cut works on it too.
    assert!(digest_bytes(&sha_of(shifted), shifted, Some(1)).is_ok());
}

#[test]
fn a_digest_round_trips_through_the_run_dir() {
    let bytes = gen_profile_bytes("baseline", 1.0);
    let sha = sha_of(&bytes);
    let d = digest_bytes(&sha, &bytes, None).unwrap();
    let run = RunDir {
        root: temp_dir_named("digest-roundtrip"),
    };
    store(&run, &d).unwrap();
    assert!(
        run.root
            .join("digests")
            .join(format!("{sha}.json"))
            .is_file()
    );
    assert_eq!(load(&run, &sha).unwrap(), d);
    assert!(load(&run, &sha_of(b"other")).is_err());
}
