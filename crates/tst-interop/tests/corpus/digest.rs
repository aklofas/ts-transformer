use tst_core::klv::checksum::checksum_running_sum_16;
use tst_core::klv::length::{read_ber, read_ber_oid};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_interop::corpus::digest::{
    FileDigest, Incremental, digest_bytes, digest_config, digest_file, elementary_sha256,
    klv_record_digest, load, store,
};
use tst_interop::corpus::judge::{ReceiverOutcome, judge};
use tst_interop::corpus::pace::PcrPacer;
use tst_interop::corpus::pcr::{align_offset, prefix_end, prefix_end_read};
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
    // The strict pass still delivers what it could and records why it
    // stopped, keyed by the error's variant, never its text.
    let kinds = &d.configs["strict_full"].feed_error_kinds;
    assert!(!kinds.is_empty());
    let variants = [
        "Unrecoverable",
        "StrictRejection",
        "MalformedPes",
        "SyncBufExhausted",
    ];
    assert!(
        kinds.keys().all(|k| variants.contains(&k.as_str())),
        "{kinds:?}"
    );
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

#[test]
fn the_streaming_digest_of_a_file_equals_the_in_memory_digest() {
    // Repeated so the file spans several feed chunks.
    let bytes = gen_profile_bytes("baseline", 10.0).repeat(4);
    assert!(bytes.len() > 2 * FEED_CHUNK, "{}", bytes.len());
    let sha = sha_of(&bytes);
    let dir = temp_dir_named("digest-stream");
    let path = dir.join("capture.ts");
    std::fs::write(&path, &bytes).unwrap();
    for prefix in [None, Some(1)] {
        let mem = digest_bytes(&sha, &bytes, prefix).unwrap();
        let known = digest_file(Some(&sha), &path, prefix).unwrap();
        let unknown = digest_file(None, &path, prefix).unwrap();
        assert_eq!(mem, known, "prefix {prefix:?}");
        assert_eq!(mem, unknown, "prefix {prefix:?}");
        assert_eq!(mem.elementary_sha256, known.elementary_sha256);
    }
    let wrong = sha_of(b"other");
    assert!(digest_file(Some(&wrong), &path, None).is_err());
}

fn incremental(bytes: &[u8], piece: usize, prefix: Option<u32>) -> FileDigest {
    let mut inc = Incremental::new(prefix);
    bytes.chunks(piece).for_each(|p| inc.feed(p));
    assert_eq!(inc.bytes_fed(), bytes.len() as u64);
    inc.finish().unwrap()
}

#[test]
fn an_incremental_digest_in_datagram_pieces_equals_the_whole_file_digest() {
    let bytes = gen_profile_bytes("baseline", 4.0);
    let sha = sha_of(&bytes);
    for prefix in [None, Some(1), Some(600)] {
        let want = digest_bytes(&sha, &bytes, prefix).unwrap();
        assert_eq!(incremental(&bytes, 1316, prefix), want, "prefix {prefix:?}");
    }
}

/// A capture past the PMT head, built by repeating a short one (each repeat
/// restarts the PCR, which the span reads as a discontinuity and counts
/// through), and the smallest whole number of seconds whose cut lies past
/// `beyond` bytes.
fn long_capture(len: usize, beyond: usize) -> (Vec<u8>, u32) {
    let one = gen_profile_bytes("baseline", 4.0);
    let mut bytes = Vec::new();
    while bytes.len() < len {
        bytes.extend_from_slice(&one);
    }
    let seconds = (1..)
        .step_by(5)
        .find(|s| prefix_end(&bytes, *s) > beyond)
        .expect("a long enough prefix");
    (bytes, seconds)
}

#[test]
fn an_incremental_prefix_cut_beyond_the_pmt_head_lands_on_the_same_packet() {
    let head = tst_interop::corpus::pcr::PMT_PREFIX;
    let (bytes, seconds) = long_capture(head + 8 * FEED_CHUNK, head + 2 * FEED_CHUNK);
    let sha = sha_of(&bytes);
    let prefix = Some(seconds);
    let want = digest_bytes(&sha, &bytes, prefix).unwrap();
    let end = prefix_end(&bytes, seconds);
    assert!(end > head && end < bytes.len());
    assert_eq!(
        want.configs["default"],
        digest_config("default", &bytes[..end], FEED_CHUNK)
    );
    for piece in [188, 1316, FEED_CHUNK + 7] {
        assert_eq!(incremental(&bytes, piece, prefix), want, "piece {piece}");
    }
}

#[test]
fn the_senders_cut_and_the_sweeps_cut_agree_when_junk_sits_at_a_block_boundary() {
    let head = tst_interop::corpus::pcr::PMT_PREFIX;
    let (mut bytes, _) = long_capture(head + 8 * FEED_CHUNK, 0);
    // A run of non-sync bytes just before a block boundary past the head
    // shifts every later packet off its alignment and forces a resync at the
    // block tail.
    let at = 23 * FEED_CHUNK - 60;
    assert!(at > head);
    bytes.splice(at..at, vec![0x11u8; 37]);
    let seconds = (1..)
        .step_by(5)
        .find(|s| prefix_end(&bytes, *s) > at + FEED_CHUNK)
        .unwrap();
    let sha = sha_of(&bytes);
    let end = prefix_end_read(&mut bytes.as_slice(), seconds).unwrap() as usize;
    assert!(
        end > at && end < bytes.len(),
        "cut {end} of {}",
        bytes.len()
    );
    assert_eq!(end, prefix_end(&bytes, seconds));

    let sweep = digest_bytes(&sha, &bytes, Some(seconds)).unwrap();
    let cut = digest_bytes(&sha_of(&bytes[..end]), &bytes[..end], None).unwrap();
    assert_eq!(sweep.configs, cut.configs);

    let pacer = PcrPacer::new(&bytes, Some(seconds)).unwrap();
    let joined: Vec<u8> = pacer.chunks().flat_map(|c| c.bytes.to_vec()).collect();
    let received = digest_bytes(&sha_of(&joined), &joined, None).unwrap();
    let v = judge("udp", &sweep, Some(&received), &ReceiverOutcome::Completed);
    assert!(v.pass, "{:?}", v.failures);
}

/// An ST 0605 Precision Time Stamp Pack: its UL, BER length 9, the status
/// byte and the big-endian microseconds.
fn st0605_pack_bytes(status: u8, micros: u64) -> Vec<u8> {
    let mut out = tst_core::klv::UniversalLabel::PRECISION_TIMESTAMP_PACK_UL
        .0
        .to_vec();
    out.push(0x09);
    out.push(status);
    out.extend_from_slice(&micros.to_be_bytes());
    out
}

/// A complete item under a UL that is neither an ST 0605 pack nor an
/// ST 0601 set.
pub fn foreign_ul_item() -> Vec<u8> {
    let mut foreign = vec![
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x02, 0x00, 0x00,
        0x00,
    ];
    foreign.extend_from_slice(&[0x01, 0xAA]);
    foreign
}

#[test]
fn a_pack_prefixed_0601_payload_is_one_decoded_record() {
    // A pack immediately followed by a minimal ST 0601 LS.
    let pack = st0605_pack_bytes(0x00, 1_700_000_000_000_000);
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    let rec = klv_record_digest(0x1001, 90_000, &[pack.as_slice(), ls.as_slice()].concat());
    assert!(rec.decoded, "{:?}", rec.unknown_reason);
    assert_eq!(rec.items, ["st0605-pack", "st0601-ls"]);
    assert_eq!(rec.tags, klv_record_digest(0x1001, 90_000, &ls).tags);
    // The canonical hash covers the pack: the LS alone hashes differently.
    assert_ne!(
        rec.canonical_sha256,
        klv_record_digest(0x1001, 90_000, &ls).canonical_sha256
    );
}

#[test]
fn a_standalone_pack_is_a_decoded_record_with_no_tags() {
    let rec = klv_record_digest(0x1001, 0, &st0605_pack_bytes(0x00, 1));
    assert!(rec.decoded, "{:?}", rec.unknown_reason);
    assert_eq!(rec.items, ["st0605-pack"]);
    assert!(rec.tags.is_empty());
}

#[test]
fn an_unregistered_ul_is_still_unknown_and_names_the_item() {
    let rec = klv_record_digest(0x1001, 0, &foreign_ul_item());
    assert!(!rec.decoded);
    let why = rec.unknown_reason.as_deref().unwrap();
    assert!(why.starts_with("item 0 (060e2b34020b0101"), "{why}");
}

#[test]
fn a_payload_that_ends_inside_an_item_is_unknown_not_partially_decoded() {
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    let rec = klv_record_digest(0x1001, 0, &ls[..ls.len() - 3]);
    assert!(!rec.decoded);
    assert!(rec.items.is_empty());
    // A pack whose trailing LS is cut is not partially decoded either.
    let pack = st0605_pack_bytes(0x00, 1);
    let rec = klv_record_digest(0x1001, 0, &[pack.as_slice(), &ls[..ls.len() - 3]].concat());
    assert!(!rec.decoded);
    assert!(rec.items.is_empty());
}

#[test]
fn trailing_bytes_shorter_than_a_label_are_unknown() {
    let pack = st0605_pack_bytes(0x00, 1);
    let rec = klv_record_digest(0x1001, 0, &[pack.as_slice(), &[0x06, 0x0E, 0x2B]].concat());
    assert!(!rec.decoded);
    assert!(rec.items.is_empty());
    // Stuffing after a complete item is malformed, not a cut record.
    assert!(!rec.truncated);
    assert_eq!(
        rec.unknown_reason.as_deref(),
        Some("3 trailing byte(s) shorter than a UL")
    );
}

/// A record cut inside its long-form BER length keeps the reason text the
/// ST 0601 decoder gave it, offsets included, so its digest does not change.
#[test]
fn a_record_cut_inside_its_length_keeps_the_old_reason_text() {
    let mut cut = tst_core::klv::UniversalLabel::ST_0601_LS.0.to_vec();
    cut.extend_from_slice(&[0x82, 0x01]);
    let old = tst_core::klv::st0601::decode(&cut).unwrap_err().to_string();
    let rec = klv_record_digest(1, 0, &cut);
    assert!(!rec.decoded);
    assert!(rec.truncated);
    assert_eq!(rec.unknown_reason.as_deref(), Some(old.as_str()));
    // And a cut body, the common case.
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    let body_cut = &ls[..ls.len() - 3];
    let old = tst_core::klv::st0601::decode(body_cut)
        .unwrap_err()
        .to_string();
    assert_eq!(
        klv_record_digest(1, 0, body_cut).unknown_reason.as_deref(),
        Some(old.as_str())
    );
}

#[test]
fn a_record_cut_mid_body_is_truncated_and_a_foreign_ul_is_not() {
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    assert!(klv_record_digest(1, 0, &ls[..ls.len() - 3]).truncated);
    assert!(!klv_record_digest(1, 0, &foreign_ul_item()).truncated);
    assert!(!klv_record_digest(1, 0, &ls).truncated);
}

/// An ST 0601 LS whose Item 2 (Precision Time Stamp, 8 bytes) carries 3
/// bytes, then a valid Item 1 checksum.
pub fn st0601_with_short_item2() -> Vec<u8> {
    let mut out = tst_core::klv::UniversalLabel::ST_0601_LS.0.to_vec();
    let body = [0x02, 0x03, 0x00, 0x01, 0x02];
    // The body, then Tag 1 with its 2-byte checksum.
    out.push((body.len() + 4) as u8);
    out.extend_from_slice(&body);
    out.extend_from_slice(&[0x01, 0x02]);
    let sum = checksum_running_sum_16(&out);
    out.extend_from_slice(&sum.to_be_bytes());
    out
}

#[test]
fn a_record_with_a_field_error_is_decoded_and_counts_its_errors() {
    // The lenient decoder records the short Item 2 as one field error and
    // keeps the record.
    let rec = klv_record_digest(1, 0, &st0601_with_short_item2());
    assert!(rec.decoded, "{:?}", rec.unknown_reason);
    assert_eq!(rec.field_errors, 1);
    // A clean record has none.
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    assert_eq!(klv_record_digest(1, 0, &ls).field_errors, 0);
}

#[test]
fn field_errors_are_counted_per_pid_in_the_digest() {
    let bytes = crate::invariants::muxed_klv_file(&st0601_with_short_item2(), 5);
    let d = digest_config("default", &bytes, FEED_CHUNK);
    let klv = d
        .pids
        .values()
        .find(|p| p.metadata > 0 && p.kind.starts_with("klv"))
        .unwrap();
    assert_eq!(klv.klv_field_errors, Some(5), "{klv:?}");
    assert_eq!(klv.klv_unknown, 0);
}
