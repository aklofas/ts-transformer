use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_interop::corpus::FEED_CHUNK;
use tst_interop::corpus::annotations::Annotations;
use tst_interop::corpus::digest::digest_config;
use tst_interop::corpus::invariants::{InvariantFailure, check, walk};
use tst_interop::fixtures::{AuSizeMode, KlvSet, klv_record_for, video_au_sized};

use crate::gen_profile_bytes;

const KLV_PID: u16 = 0x101;

fn failures(bytes: &[u8], ann: &Annotations) -> Vec<InvariantFailure> {
    check(
        &digest_config("default", bytes, FEED_CHUNK),
        &walk(bytes),
        ann,
    )
}

fn rules(f: &[InvariantFailure]) -> Vec<&str> {
    f.iter().map(|x| x.rule.as_str()).collect()
}

/// Thirty video AUs on 0x100; each is followed by a KLV payload on 0x101
/// when `klv` is given (`Some(None)` pushes a real ST 0601 record).
fn muxed(klv: Option<Option<&[u8]>>) -> Vec<u8> {
    let p = tst_interop::profiles::by_name("baseline").unwrap();
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let (vh, kh) = (mux.video_handles()[0], mux.klv_handles()[0]);
    let mut out = Vec::new();
    let mut buf = [0u8; 1316];
    for i in 0..30u32 {
        let pts = Pts90khz::new(i64::from(i) * 3000);
        let (au, key) = video_au_sized(p.video, i, AuSizeMode::Compact);
        mux.push_video_to(vh, &au, pts, key).unwrap();
        match klv {
            None => {}
            Some(Some(raw)) => mux.push_klv_to(kh, raw, pts, 0).unwrap(),
            Some(None) => {
                let rec = klv_record_for(KlvSet::Compact, 0, i).unwrap();
                mux.push_klv_to(kh, &rec, pts, 0).unwrap();
            }
        }
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

fn pid_at(bytes: &[u8], o: usize) -> u16 {
    (u16::from(bytes[o + 1] & 0x1F) << 8) | u16::from(bytes[o + 2])
}

/// Offset of the first packet at or after `from` that carries an adaptation
/// field with a flags byte and a payload; `pid` restricts it to one PID.
fn with_adaptation(bytes: &[u8], from: usize, pid: Option<u16>) -> usize {
    (from..bytes.len())
        .step_by(188)
        .find(|&o| {
            (bytes[o + 3] >> 4) & 3 == 3
                && bytes[o + 4] >= 1
                && pid.is_none_or(|p| pid_at(bytes, o) == p)
        })
        .expect("a packet with an adaptation field and a payload")
}

fn middle_packet(bytes: &[u8]) -> usize {
    with_adaptation(bytes, (bytes.len() / 188 / 2) * 188, None)
}

fn demuxer_discontinuities(bytes: &[u8]) -> (u64, u64) {
    let mut d = Demuxer::new();
    let _ = d.feed(bytes);
    d.flush();
    let mut seen = 0;
    while let Some(ev) = d.next_event() {
        if matches!(ev, DemuxEvent::Discontinuity { .. }) {
            seen += 1;
        }
    }
    (d.stats().discontinuities, seen)
}

#[test]
fn a_clean_generated_stream_has_no_failures() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let f = failures(&bytes, &Annotations::default());
    assert!(f.is_empty(), "{f:?}");
    let w = walk(&bytes);
    assert_eq!(w.packets as usize, bytes.len() / 188);
}

#[test]
fn a_klv_pid_with_undecodable_records_fails_unless_annotated() {
    // A SMPTE universal label routes the PES to the KLV path, but the body
    // is not an ST 0601 record. A payload without the label never reaches a
    // KLV record: the demuxer hands it out as an unknown sample.
    let ul_only = b"\x06\x0e\x2b\x34\x02\x0b\x01\x01\x0e\x01\x03\x01\x01\x00\x00\x00\x02\xff\xff";
    let bytes = muxed(Some(Some(ul_only)));
    let f = failures(&bytes, &Annotations::default());
    assert_eq!(rules(&f), ["klv-unknown"], "{f:?}");
    assert_eq!(f[0].pid, Some(KLV_PID));
    assert!(!f[0].detail.is_empty());
    let ann = Annotations {
        non_misb_private_pids: vec![KLV_PID],
        ..Default::default()
    };
    assert!(failures(&bytes, &ann).is_empty());
}

#[test]
fn a_declared_klv_pid_with_no_records_passes() {
    let bytes = muxed(None);
    let d = digest_config("default", &bytes, FEED_CHUNK);
    assert_eq!(d.klv_records, 0);
    assert!(d.pids.get(&KLV_PID).is_none_or(|p| p.metadata == 0));
    assert!(failures(&bytes, &Annotations::default()).is_empty());
}

#[test]
fn a_pts_jump_backwards_fails_unless_known() {
    let first = gen_profile_bytes("baseline", 2.0);
    let mut bytes = first.clone();
    bytes.extend_from_slice(&first);
    let f = failures(&bytes, &Annotations::default());
    assert!(rules(&f).contains(&"pts-order"), "{f:?}");
    let ann = Annotations {
        known_discontinuity: true,
        ..Default::default()
    };
    let f = failures(&bytes, &ann);
    assert!(f.is_empty(), "{f:?}");
}

#[test]
fn a_dropped_packet_is_consistent_between_walk_and_demuxer() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    let at = middle_packet(&bytes);
    bytes.drain(at..at + 188);
    assert_eq!(walk(&bytes).cc_errors, 1);
    assert_eq!(demuxer_discontinuities(&bytes), (1, 1));
    let f = failures(&bytes, &Annotations::default());
    assert!(!rules(&f).contains(&"accounting"), "{f:?}");
}

#[test]
fn a_duplicated_packet_counts_nothing_in_either() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    let at = middle_packet(&bytes);
    let dup = bytes[at..at + 188].to_vec();
    bytes.splice(at..at, dup);
    assert_eq!(walk(&bytes).cc_errors, 0);
    assert_eq!(demuxer_discontinuities(&bytes), (0, 0));
    assert!(failures(&bytes, &Annotations::default()).is_empty());
}

#[test]
fn a_third_identical_packet_counts_one_in_both() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    let at = middle_packet(&bytes);
    let dup = bytes[at..at + 188].to_vec();
    bytes.splice(at..at, [dup.clone(), dup].concat());
    assert_eq!(walk(&bytes).cc_errors, 1);
    assert_eq!(demuxer_discontinuities(&bytes), (1, 1));
}

#[test]
fn the_discontinuity_indicator_after_a_gap_is_one_event_in_both() {
    let mut bytes = gen_profile_bytes("baseline", 2.0);
    let at = middle_packet(&bytes);
    let pid = pid_at(&bytes, at);
    bytes.drain(at..at + 188);
    // The next packet on that PID raises its discontinuity indicator.
    let next = with_adaptation(&bytes, at, Some(pid));
    bytes[next + 5] |= 0x80;
    // The gap is not a jump any more; the indicator itself is the one event
    // the demuxer records, and the walk counts it the same way.
    assert_eq!(walk(&bytes).cc_errors, 1);
    assert_eq!(demuxer_discontinuities(&bytes), (1, 1));
    assert!(!rules(&failures(&bytes, &Annotations::default())).contains(&"accounting"));
}

#[test]
fn a_payload_without_a_klv_label_fails_unless_annotated() {
    // No AU-cell header and no SMPTE label: the demuxer hands it out as an
    // unclassified sample on the KLV PID, never as a KLV record.
    let bytes = muxed(Some(Some(b"not klv")));
    let f = failures(&bytes, &Annotations::default());
    assert_eq!(rules(&f), ["klv-unknown"], "{f:?}");
    assert_eq!(f[0].pid, Some(KLV_PID));
    assert!(f[0].detail.contains("unknown sample"), "{f:?}");
    let ann = Annotations {
        non_misb_private_pids: vec![KLV_PID],
        ..Default::default()
    };
    assert!(failures(&bytes, &ann).is_empty());
}

// ---- hand-built PSI, to drive the walk and the demuxer through the same rules

const PMT_PID: u16 = 0x1000;

fn with_crc(mut section: Vec<u8>) -> Vec<u8> {
    let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn long_section(table: u8, id: u16, version: u8, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4;
    let mut s = vec![table, 0xB0 | (len >> 8) as u8, len as u8];
    s.extend_from_slice(&id.to_be_bytes());
    s.extend_from_slice(&[0xC1 | (version << 1), 0, 0]);
    s.extend_from_slice(body);
    with_crc(s)
}

fn pat(version: u8) -> Vec<u8> {
    let body = [0, 1, 0xE0 | (PMT_PID >> 8) as u8, PMT_PID as u8];
    long_section(0, 1, version, &body)
}

/// A PMT for program 1: `streams` are `(stream_type, pid)`, each with a
/// private descriptor `descriptor_len` bytes long.
fn pmt(version: u8, streams: &[(u8, u16)], descriptor_len: usize) -> Vec<u8> {
    let mut body = vec![0xE1, 0x00, 0xF0, 0x00];
    for (ty, pid) in streams {
        let info = if descriptor_len == 0 {
            0
        } else {
            descriptor_len + 2
        };
        body.extend_from_slice(&[*ty, 0xE0 | (pid >> 8) as u8, *pid as u8]);
        body.extend_from_slice(&[0xF0 | (info >> 8) as u8, info as u8]);
        if descriptor_len > 0 {
            body.extend_from_slice(&[0xC0, descriptor_len as u8]);
            body.extend(std::iter::repeat_n(0u8, descriptor_len));
        }
    }
    long_section(2, 1, version, &body)
}

fn packet(pid: u16, cc: u8, pusi: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0xFF; 188];
    p[0] = 0x47;
    p[1] = (u8::from(pusi) << 6) | (pid >> 8) as u8;
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 0x0F);
    p[4..4 + payload.len()].copy_from_slice(payload);
    p
}

/// A section as consecutive packets from counter `cc`.
fn section_packets(pid: u16, cc: &mut u8, section: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8];
    data.extend_from_slice(section);
    let mut out = Vec::new();
    for (i, chunk) in data.chunks(184).enumerate() {
        out.extend(packet(pid, *cc, i == 0, chunk));
        *cc = (*cc + 1) & 0x0F;
    }
    out
}

struct Psi {
    pat_cc: u8,
    pmt_cc: u8,
}

impl Psi {
    fn new() -> Self {
        Self {
            pat_cc: 0,
            pmt_cc: 0,
        }
    }
    fn pat(&mut self, version: u8) -> Vec<u8> {
        section_packets(0, &mut self.pat_cc, &pat(version))
    }
    fn pmt(&mut self, section: &[u8]) -> Vec<u8> {
        section_packets(PMT_PID, &mut self.pmt_cc, section)
    }
}

fn es(pid: u16, ccs: &[u8]) -> Vec<u8> {
    ccs.iter()
        .flat_map(|c| packet(pid, *c, false, &[0xAA]))
        .collect()
}

fn both(bytes: &[u8]) -> (u64, u64) {
    (walk(bytes).cc_errors, demuxer_discontinuities(bytes).0)
}

#[test]
fn a_pmt_longer_than_one_packet_declares_every_stream() {
    let streams: Vec<(u8, u16)> = (0..12).map(|i| (0x1B, 0x100 + i)).collect();
    let section = pmt(0, &streams, 40);
    assert!(section.len() > 2 * 184, "the PMT spans several packets");
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    bytes.extend(psi.pmt(&section));
    // The last-listed PID skips counter 2.
    bytes.extend(es(0x10B, &[0, 1, 3]));
    assert_eq!(both(&bytes), (1, 1));
}

#[test]
fn a_pmt_with_a_bad_checksum_leaves_the_earlier_declaration_in_force() {
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    bytes.extend(psi.pmt(&pmt(0, &[(0x1B, 0x100)], 0)));
    let mut bad = pmt(1, &[(0x1B, 0x200)], 0);
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    bytes.extend(psi.pmt(&bad));
    bytes.extend(es(0x100, &[0, 1, 3]));
    bytes.extend(es(0x200, &[0, 1, 3]));
    assert_eq!(both(&bytes), (1, 1));
}

#[test]
fn a_pmt_for_another_program_is_not_adopted() {
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    // The PAT says program 1 lives on this PID; this PMT says program 2.
    let body = [0xE1, 0x00, 0xF0, 0x00, 0x1B, 0xE2, 0x00, 0xF0, 0x00];
    bytes.extend(psi.pmt(&long_section(2, 2, 0, &body)));
    bytes.extend(es(0x200, &[0, 1, 3]));
    assert_eq!(both(&bytes), (0, 0));
}

#[test]
fn a_pid_retired_by_a_new_pmt_starts_over_when_it_returns() {
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    bytes.extend(psi.pmt(&pmt(0, &[(0x1B, 0x100), (0x1B, 0x200)], 0)));
    bytes.extend(es(0x200, &[0]));
    bytes.extend(psi.pmt(&pmt(1, &[(0x1B, 0x100)], 0)));
    bytes.extend(psi.pmt(&pmt(2, &[(0x1B, 0x100), (0x1B, 0x200)], 0)));
    // Counter 9 after 0 would be a jump had the state survived the retirement.
    bytes.extend(es(0x200, &[9, 10]));
    assert_eq!(both(&bytes), (0, 0));
}

/// Thirty async KLV records on 0x101, one per 0.1 s (3 s in all), beside
/// a video AU each on 0x100.
pub(crate) fn klv_every_tenth_of_a_second() -> Vec<u8> {
    let p = tst_interop::profiles::by_name("baseline").unwrap();
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_klv(KLV_PID, KlvStreamType::PrivateData, true);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let (vh, kh) = (mux.video_handles()[0], mux.klv_handles()[0]);
    let mut out = Vec::new();
    let mut buf = [0u8; 1316];
    for i in 0..30u32 {
        let pts = Pts90khz::new(9000 + i64::from(i) * 9000);
        let (au, key) = video_au_sized(p.video, i, AuSizeMode::Compact);
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

/// Offset of the payload in the packet at `o`.
fn payload_at(bytes: &[u8], o: usize) -> usize {
    match (bytes[o + 3] >> 4) & 3 {
        3 => o + 5 + usize::from(bytes[o + 4]),
        _ => o + 4,
    }
}

/// The ten-per-second KLV stream with every other PES from the twentieth
/// on stripped of its PTS, and how many were stripped.
pub(crate) fn klv_with_stripped_pts() -> (Vec<u8>, u64) {
    let mut bytes = klv_every_tenth_of_a_second();
    let starts: Vec<usize> = (0..bytes.len())
        .step_by(188)
        .filter(|&o| pid_at(&bytes, o) == KLV_PID && bytes[o + 1] & 0x40 != 0)
        .collect();
    assert_eq!(starts.len(), 30);
    // From the twentieth record on (two seconds in), every other PES loses
    // its PTS: the flags say none, its five bytes become header stuffing.
    let mut stripped = 0;
    for &o in starts.iter().skip(20).step_by(2) {
        let pl = payload_at(&bytes, o);
        assert_eq!(bytes[pl + 7] & 0x80, 0x80, "the muxer wrote a PTS");
        bytes[pl + 7] &= 0x3F;
        bytes[pl + 9..pl + 14].fill(0xFF);
        stripped += 1;
    }
    (bytes, stripped)
}

#[test]
fn a_klv_pes_without_a_pts_takes_no_part_in_pts_order() {
    let (bytes, stripped) = klv_with_stripped_pts();
    let d = digest_config("default", &bytes, FEED_CHUNK);
    let klv = &d.pids[&KLV_PID];
    assert_eq!(klv.metadata, 30, "{klv:?}");
    assert_eq!(klv.pts_absent, stripped, "{klv:?}");
    assert_eq!(klv.pts_backward_breaks, 0, "{klv:?}");
    let f = failures(&bytes, &Annotations::default());
    assert!(!rules(&f).contains(&"pts-order"), "{f:?}");
}

#[test]
fn a_reviewed_cc_mismatch_is_recorded_as_excused() {
    let bytes = muxed(Some(None));
    let d = digest_config("default", &bytes, FEED_CHUNK);
    let mut w = walk(&bytes);
    w.cc_errors += 1;
    let plain = check(&d, &w, &Annotations::default());
    assert_eq!(rules(&plain), ["accounting"], "{plain:?}");
    assert!(!plain[0].excused);
    let ann = Annotations {
        known_cc_mismatch: true,
        reason: Some("reviewed: a re-typed PID".into()),
        ..Default::default()
    };
    let excused = check(&d, &w, &ann);
    assert_eq!(rules(&excused), ["accounting"], "{excused:?}");
    assert!(excused[0].excused);
    assert!(
        excused[0]
            .detail
            .starts_with("excused: reviewed: a re-typed PID;"),
        "{excused:?}"
    );
}

#[test]
fn a_pes_oversize_is_counted_apart_and_left_out_of_accounting() {
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    bytes.extend(psi.pmt(&pmt(0, &[(0x1B, 0x100)], 0)));
    // One unbounded video PES of 4.5 MiB, every counter in sequence: more
    // than the demuxer's per-PID reassembly cap, nothing lost on the wire.
    let head = [0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
    let mut cc = 0u8;
    bytes.extend(packet(0x100, cc, true, &head));
    for _ in 0..(9 << 19) / 184 {
        cc = (cc + 1) & 0x0F;
        bytes.extend(packet(0x100, cc, false, &[0xAA; 184]));
    }
    let d = digest_config("default", &bytes, FEED_CHUNK);
    let video = &d.pids[&0x100];
    assert!(video.pes_oversize >= 1, "{video:?}");
    assert_eq!(video.discontinuities, 0, "{video:?}");
    assert_eq!(d.stats.discontinuities, video.pes_oversize);
    assert_eq!(walk(&bytes).cc_errors, 0);
    let f = failures(&bytes, &Annotations::default());
    assert!(!rules(&f).contains(&"accounting"), "{f:?}");
}

#[test]
fn a_false_sync_candidate_after_a_sync_loss_is_refused_as_the_demuxer_refuses_it() {
    // Realistic access units: long runs of video packets.
    let file = std::env::temp_dir().join(format!("tst-corpus-falsesync-{}.ts", std::process::id()));
    let p = tst_interop::profiles::by_name("baseline").unwrap();
    tst_interop::r#gen::run(
        p,
        2.0,
        &file,
        KlvSet::Compact,
        0,
        AuSizeMode::Realistic { scale: 1 },
    )
    .unwrap();
    let clean = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_file(&file);
    let probe = tst_test_helpers::rawts::parsed(&clean);
    let video = probe
        .streams
        .iter()
        .find(|s| s.stream_type == 0x1B)
        .unwrap()
        .pid;
    // Seven video packets in a row past the middle: their byte 98 is past
    // any adaptation-field header, in data or stuffing.
    let r = (clean.len() / 188 / 2..clean.len() / 188 - 8)
        .map(|k| k * 188)
        .find(|&o| (0..7).all(|i| pid_at(&clean, o + i * 188) == video))
        .expect("seven video packets in a row");
    let prev_cc = clean[r - 188 + 3] & 0x0F;
    // Junk opens with a non-sync byte (sync lost), then a candidate packet
    // header on the video PID at junk offset 10 with a counter out of step, whose next two strides
    // land on sync bytes planted at payload offset 98 of the two real
    // packets: three strides hold, five of seven do not.
    const JUNK: usize = 100;
    let mut junk = vec![0u8; JUNK];
    junk[10..14].copy_from_slice(&[
        0x47,
        (video >> 8) as u8,
        video as u8,
        0x10 | ((prev_cc + 8) & 0x0F),
    ]);
    let mut bytes = clean[..r].to_vec();
    bytes.extend_from_slice(&junk);
    bytes.extend_from_slice(&clean[r..]);
    let fake = r + 10;
    for k in 1..7 {
        let at = fake + k * 188;
        if at < bytes.len() {
            bytes[at] = if k <= 2 { 0x47 } else { 0x00 };
        }
    }
    let (walked, demuxed) = both(&bytes);
    assert_eq!(walked, demuxed);
    assert_eq!(demuxed, 0);
}
