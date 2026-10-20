use tst_core::klv::checksum::checksum_running_sum_16;
use tst_core::klv::length::{read_ber, read_ber_oid};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_interop::corpus::digest::{
    FileDigest, digest_bytes, digest_config, elementary_sha256, klv_record_digest, load, store,
};
use tst_interop::corpus::pcr::{align_offset, prefix_end};
use tst_interop::corpus::{FEED_CHUNK, RunDir};
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

/// Sets the PCR base of a PCR-bearing packet, keeping the extension.
fn set_pcr_base(pkt: &mut [u8], base: u64) {
    pkt[6] = (base >> 25) as u8;
    pkt[7] = (base >> 17) as u8;
    pkt[8] = (base >> 9) as u8;
    pkt[9] = (base >> 1) as u8;
    pkt[10] = (pkt[10] & 0x7F) | ((base & 1) as u8) << 7;
}

/// Calls `f(packet index, packet)` for every PCR packet on the PCR PID.
fn for_each_pcr_packet(bytes: &mut [u8], mut f: impl FnMut(usize, &mut [u8], u64)) {
    let pcr_pid = rawts::parsed(bytes).pcr_pid.unwrap();
    let mut n = 0;
    for pkt in bytes.chunks_exact_mut(188) {
        let info = classify_packet((&*pkt).try_into().unwrap()).unwrap();
        if info.pid == pcr_pid
            && let Some(base) = info.pcr_base
        {
            f(n, pkt, base);
            n += 1;
        }
    }
}

#[test]
fn rejections_inside_one_chunk_are_all_counted_whatever_the_chunking() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    // Zero two PCRs well apart so each rejection sits in the same chunk.
    for_each_pcr_packet(&mut bytes, |n, pkt, _| {
        if n == 3 || n == 8 {
            pkt[6..12].fill(0);
        }
    });
    assert!(
        bytes.len() < FEED_CHUNK,
        "one chunk holds the whole capture"
    );
    let one = digest_config("strict_full", &bytes, FEED_CHUNK);
    let pieces = digest_config("strict_full", &bytes, 188);
    assert!(one.feed_errors >= 2, "{}", one.feed_errors);
    assert_eq!(one, pieces);
    // The tail after the last rejection is still demuxed.
    let default = digest_config("default", &bytes, FEED_CHUNK);
    let last = |d: &tst_interop::corpus::digest::ConfigDigest| {
        d.pids.values().filter_map(|p| p.pts_last).max()
    };
    assert_eq!(last(&one), last(&default));
    for key in ["default", "unwrap"] {
        assert_eq!(
            digest_config(key, &bytes, FEED_CHUNK),
            digest_config(key, &bytes, 188 * 7)
        );
    }
}

/// A one-video, one-KLV stream on the given PIDs.
fn remuxed(pmt: u16, video: u16, klv: u16) -> Vec<u8> {
    let p = tst_interop::profiles::by_name("baseline").unwrap();
    let mut prog = MuxerProgramConfigBuilder::new(1, pmt);
    prog.add_video(video, VideoCodec::H264);
    prog.add_klv(klv, KlvStreamType::PrivateData, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let (vh, kh) = (mux.video_handles()[0], mux.klv_handles()[0]);
    let mut out = Vec::new();
    let mut buf = [0u8; 1316];
    for i in 0..30u32 {
        let pts = Pts90khz::new(i64::from(i) * 3000);
        let (au, key) = tst_interop::fixtures::video_au_sized(
            p.video,
            i,
            tst_interop::fixtures::AuSizeMode::Compact,
        );
        mux.push_video_to(vh, &au, pts, key).unwrap();
        let rec = klv_record_for(KlvSet::Compact, 0, i).unwrap();
        mux.push_klv_to(kh, &rec, pts, 0).unwrap();
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }
    out
}

#[test]
fn the_elementary_hash_ignores_pids_and_timing_only_pids() {
    let a = remuxed(0x1000, 0x100, 0x101);
    let b = remuxed(0x0500, 0x0200, 0x0201);
    let da = digest_bytes(&sha_of(&a), &a, None).unwrap();
    let db = digest_bytes(&sha_of(&b), &b, None).unwrap();
    assert_ne!(
        da.configs["default"].pids.keys().collect::<Vec<_>>(),
        db.configs["default"].pids.keys().collect::<Vec<_>>()
    );
    assert!(da.configs["default"].klv_records > 0);
    assert_eq!(da.elementary_sha256, db.elementary_sha256);

    // A PID that only raised timing issues does not enter the hash.
    let mut with_timing_pid = da.configs["default"].clone();
    let mut ghost = with_timing_pid.pids.values().next().unwrap().clone();
    (ghost.samples, ghost.metadata) = (0, 0);
    ghost.kind = "unknown".into();
    ghost.nonconformant.insert("PcrAnomaly".into(), 9);
    with_timing_pid.pids.insert(0x1FF0, ghost);
    assert_eq!(elementary_sha256(&with_timing_pid), da.elementary_sha256);
}

#[test]
fn nonconformant_keys_are_variant_names() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    for_each_pcr_packet(&mut bytes, |n, pkt, _| {
        if n == 3 {
            pkt[6..12].fill(0);
        }
    });
    let d = digest_bytes(&sha_of(&bytes), &bytes, None).unwrap();
    let keys: Vec<&String> = d
        .configs
        .values()
        .flat_map(|c| c.pids.values())
        .flat_map(|p| p.nonconformant.keys())
        .collect();
    assert!(!keys.is_empty(), "a zeroed PCR raises issues");
    assert!(
        keys.iter()
            .all(|k| k.chars().all(|c| c.is_ascii_alphanumeric())),
        "{keys:?}"
    );
}

#[test]
fn a_small_backwards_pcr_step_does_not_end_the_prefix_early() {
    let clean = gen_profile_bytes("baseline", 4.0);
    let mut glitched = clean.clone();
    let total = {
        let mut n = 0;
        for_each_pcr_packet(&mut glitched, |_, _, _| n += 1);
        n
    };
    // First half one second ahead: a backwards step of about a second at the middle.
    for_each_pcr_packet(&mut glitched, |n, pkt, base| {
        if n < total / 2 {
            set_pcr_base(pkt, base + 90_000);
        }
    });
    let at_clean = prefix_end(&clean, 3);
    let at_glitched = prefix_end(&glitched, 3);
    assert!(at_clean < clean.len());
    assert!(at_glitched > at_clean, "{at_glitched} vs {at_clean}");
    assert_eq!(at_clean % 188, 0);
}

#[test]
fn sync_needs_two_confirming_strides() {
    let mut buf = vec![0u8; 1000];
    // 0x47 at 3 and 191 but not at 379: not a packet start.
    buf[3] = 0x47;
    buf[191] = 0x47;
    let real = gen_profile_bytes("baseline", 1.0);
    buf.extend_from_slice(&real);
    assert_eq!(align_offset(&buf), 1000);
    assert_eq!(align_offset(&real), 0);
}
