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
    muxed_n(klv, 30)
}

/// `n` video AUs, each followed by `payload` on the KLV PID.
pub fn muxed_klv_file(payload: &[u8], n: u32) -> Vec<u8> {
    muxed_n(Some(Some(payload)), n)
}

/// [`muxed`] with `n` video AUs.
fn muxed_n(klv: Option<Option<&[u8]>>, n: u32) -> Vec<u8> {
    muxed_by(n, |i| match klv {
        None => None,
        Some(Some(raw)) => Some(raw.to_vec()),
        Some(None) => Some(klv_record_for(KlvSet::Compact, 0, i).unwrap()),
    })
}

/// `n` video AUs on 0x100, each followed on the KLV PID by what `klv`
/// gives for its index, if anything.
fn muxed_by(n: u32, klv: impl Fn(u32) -> Option<Vec<u8>>) -> Vec<u8> {
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
    for i in 0..n {
        let pts = Pts90khz::new(i64::from(i) * 3000);
        let (au, key) = video_au_sized(p.video, i, AuSizeMode::Compact);
        mux.push_video_to(vh, &au, pts, key).unwrap();
        if let Some(raw) = klv(i) {
            mux.push_klv_to(kh, &raw, pts, 0).unwrap();
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
        max_private_records: Some(30),
        reason: Some("vendor private data".into()),
        ..Default::default()
    };
    let f = failures(&bytes, &ann);
    assert_eq!(rules(&f), ["klv-unknown"], "{f:?}");
    assert!(f[0].excused, "{f:?}");
    assert!(
        f[0].detail
            .starts_with("excused: vendor private data; 30 private record(s) (bound 30)"),
        "{f:?}"
    );
}

#[test]
fn a_declared_klv_pid_with_no_records_passes() {
    let bytes = muxed(None);
    let d = digest_config("default", &bytes, FEED_CHUNK);
    assert_eq!(d.klv_records, 0);
    assert!(d.pids.get(&KLV_PID).is_none_or(|p| p.metadata == 0));
    assert!(failures(&bytes, &Annotations::default()).is_empty());
}

/// A capture joined to itself: its timeline restarts once on every PID.
fn pts_jump_capture() -> Vec<u8> {
    let first = gen_profile_bytes("baseline", 2.0);
    let mut bytes = first.clone();
    bytes.extend_from_slice(&first);
    bytes
}

#[test]
fn a_pts_jump_backwards_fails_unless_known() {
    let bytes = pts_jump_capture();
    let f = failures(&bytes, &Annotations::default());
    assert!(rules(&f).contains(&"pts-order"), "{f:?}");
}

/// The reviewed damage passes as excused; one more of it, or the same
/// damage on another PID, fails.
#[test]
fn an_annotation_excuses_only_up_to_its_reviewed_bound_and_pids() {
    let bytes = muxed_with_one_truncated_record();
    let mut ann = Annotations {
        known_klv_truncation: true,
        max_klv_truncated: Some(30),
        klv_truncation_pids: vec![KLV_PID],
        reason: Some("packet loss".into()),
        ..Default::default()
    };
    assert!(failures(&bytes, &ann).iter().all(|f| f.excused));
    ann.max_klv_truncated = Some(29); // muxed() pushes 30 cut records
    assert!(
        failures(&bytes, &ann)
            .iter()
            .any(|f| !f.excused && f.detail.contains("exceeds the reviewed bound 29"))
    );
    ann.max_klv_truncated = Some(30);
    ann.klv_truncation_pids = vec![KLV_PID + 1];
    assert!(
        failures(&bytes, &ann)
            .iter()
            .any(|f| !f.excused && f.detail.contains("does not list"))
    );
}

/// The PID of the baseline profile whose timeline breaks when the capture
/// is joined to itself; the other streams carry no backward PTS break.
const RESTART_PID: u16 = 0x1011;

#[test]
fn a_waived_pts_break_and_a_private_pid_skip_write_excused_rows() {
    // The count and PID the unannotated run of this capture reports.
    let bytes = pts_jump_capture();
    let plain = failures(&bytes, &Annotations::default());
    assert!(
        plain
            .iter()
            .any(|x| x.pid == Some(RESTART_PID) && x.detail == "1 backward PTS breaks"),
        "{plain:?}"
    );
    let ann = Annotations {
        known_discontinuity: true,
        max_pts_backward_breaks: Some(1),
        discontinuity_pids: vec![RESTART_PID],
        reason: Some("recorder restart".into()),
        ..Default::default()
    };
    let f = failures(&bytes, &ann);
    assert!(
        f.iter().any(|x| x.rule == "pts-order" && x.excused),
        "{f:?}"
    );
    assert!(f.iter().all(|x| x.excused), "{f:?}");
    assert!(
        f.iter().any(|x| x.pid == Some(RESTART_PID)
            && x.detail == "excused: recorder restart; 1 backward PTS breaks (bound 1)"),
        "{f:?}"
    );
    // One break more than reviewed fails, and so does an unlisted PID.
    let tight = Annotations {
        max_pts_backward_breaks: Some(0),
        ..ann.clone()
    };
    let f = failures(&bytes, &tight);
    assert!(
        f.iter().any(|x| x.rule == "pts-order"
            && x.pid == Some(RESTART_PID)
            && !x.excused
            && x.detail
                == "1 backward PTS breaks (1 over the listed PIDs) exceeds the reviewed bound 0"),
        "{f:?}"
    );
    let elsewhere = Annotations {
        discontinuity_pids: vec![0x100],
        ..ann
    };
    let f = failures(&bytes, &elsewhere);
    assert!(
        f.iter().any(|x| x.rule == "pts-order"
            && x.pid == Some(RESTART_PID)
            && !x.excused
            && x.detail.contains("does not list")),
        "{f:?}"
    );
    // A PID of private data is still recorded, as excused.
    let bytes = muxed_with_a_foreign_record();
    let private = Annotations {
        non_misb_private_pids: vec![KLV_PID],
        max_private_records: Some(30),
        reason: Some("vendor data".into()),
        ..Default::default()
    };
    let f = failures(&bytes, &private);
    assert_eq!(rules(&f), ["klv-unknown"], "{f:?}");
    assert!(f[0].excused, "{f:?}");
}

/// The muxed capture with every KLV record cut three bytes short of its
/// declared length, as packet loss leaves a record.
fn muxed_with_one_truncated_record() -> Vec<u8> {
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    muxed(Some(Some(&ls[..ls.len() - 3])))
}

/// The muxed capture with every KLV record under a UL that is no
/// registered MISB item.
fn muxed_with_a_foreign_record() -> Vec<u8> {
    muxed(Some(Some(&crate::digest::foreign_ul_item())))
}

#[test]
fn a_truncated_record_is_excused_only_by_known_klv_truncation() {
    let bytes = muxed_with_one_truncated_record();
    let d = digest_config("default", &bytes, FEED_CHUNK);
    assert_eq!(d.pids[&KLV_PID].klv_unknown, 30);
    assert_eq!(d.pids[&KLV_PID].klv_truncated, Some(30));
    assert_eq!(
        rules(&failures(&bytes, &Annotations::default())),
        ["klv-unknown"]
    );
    let ann = Annotations {
        known_klv_truncation: true,
        max_klv_truncated: Some(30),
        klv_truncation_pids: vec![KLV_PID],
        reason: Some("packet loss".into()),
        ..Default::default()
    };
    let f = failures(&bytes, &ann);
    assert!(f.iter().all(|x| x.excused), "{f:?}");
    assert!(
        f.iter().any(|x| x.rule == "klv-unknown"
            && x.detail == "excused: packet loss; 30 truncated record(s) (bound 30)"),
        "{f:?}"
    );
    // A foreign UL on the same PID is not excused by the truncation key.
    let bytes = muxed_with_a_foreign_record();
    assert_eq!(
        digest_config("default", &bytes, FEED_CHUNK).pids[&KLV_PID].klv_truncated,
        Some(0)
    );
    assert!(
        failures(&bytes, &ann)
            .iter()
            .any(|x| x.rule == "klv-unknown" && !x.excused)
    );
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
    // unclassified sample on the KLV PID, never as a KLV record, so the
    // PID's PES on the wire also show no KLV in the digest.
    let bytes = muxed(Some(Some(b"not klv")));
    let f = failures(&bytes, &Annotations::default());
    assert_eq!(rules(&f), ["klv-unknown", "klv-missing"], "{f:?}");
    assert_eq!(f[0].pid, Some(KLV_PID));
    assert!(f[0].detail.contains("unknown sample"), "{f:?}");
    assert_eq!(
        f[1].detail,
        "30 KLV PES on the wire, digest 0 metadata events"
    );
    let ann = Annotations {
        non_misb_private_pids: vec![KLV_PID],
        max_private_records: Some(30),
        reason: Some("vendor private data".into()),
        ..Default::default()
    };
    let f = failures(&bytes, &ann);
    assert_eq!(rules(&f), ["klv-unknown", "klv-missing"], "{f:?}");
    assert!(f.iter().all(|x| x.excused), "{f:?}");
    assert!(
        f[0].detail
            .starts_with("excused: vendor private data; 30 private record(s) (bound 30)"),
        "{f:?}"
    );
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
        max_cc_mismatch: Some(1),
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
    assert!(excused[0].detail.ends_with("(bound 1)"), "{excused:?}");
    // A second unexplained difference is more than was reviewed.
    w.cc_errors += 1;
    let over = check(&d, &w, &ann);
    assert!(!over[0].excused, "{over:?}");
    assert!(
        over[0].detail.contains("exceeds the reviewed bound 1"),
        "{over:?}"
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

/// Excused truncations on a PID do not hide label-less payloads on it:
/// those are judged on their own and fail.
#[test]
fn unknown_samples_beside_excused_truncations_still_fail() {
    let ls = klv_record_for(KlvSet::Compact, 0, 0).unwrap();
    let bytes = muxed_by(30, |i| {
        Some(if i % 2 == 0 {
            ls[..ls.len() - 3].to_vec()
        } else {
            b"not klv".to_vec()
        })
    });
    let d = digest_config("default", &bytes, FEED_CHUNK);
    let klv = &d.pids[&KLV_PID];
    assert_eq!(
        (klv.klv_unknown, klv.klv_truncated),
        (15, Some(15)),
        "{klv:?}"
    );
    assert_eq!(klv.unknown_samples, 15, "{klv:?}");
    let ann = Annotations {
        known_klv_truncation: true,
        max_klv_truncated: Some(15),
        klv_truncation_pids: vec![KLV_PID],
        reason: Some("packet loss".into()),
        ..Default::default()
    };
    let f = failures(&bytes, &ann);
    assert!(
        f.iter()
            .any(|x| x.excused && x.detail.contains("15 truncated record(s)")),
        "{f:?}"
    );
    assert!(
        f.iter().any(|x| x.rule == "klv-unknown"
            && x.pid == Some(KLV_PID)
            && !x.excused
            && x.detail == "15 unknown sample(s)"),
        "{f:?}"
    );
}

/// The demuxer stops emitting KLV on a PID the wire carries: simulated
/// by editing the digest, the way a regression would leave it.
#[test]
fn klv_that_disappears_or_is_reclassified_fails_klv_missing() {
    let bytes = muxed(Some(None));
    let w = walk(&bytes);
    assert!(w.klv_pes_starts[&KLV_PID] >= 30, "{w:?}");
    let mut d = digest_config("default", &bytes, FEED_CHUNK);
    d.pids.get_mut(&KLV_PID).unwrap().kind = "unknown".into();
    let f = check(&d, &w, &Annotations::default());
    assert_eq!(rules(&f), ["klv-missing"], "{f:?}");
    assert_eq!(f[0].pid, Some(KLV_PID));
    assert_eq!(f[0].detail, "30 KLV PES on the wire, digest kind unknown");
    d.pids.remove(&KLV_PID);
    let f = check(&d, &w, &Annotations::default());
    assert_eq!(rules(&f), ["klv-missing"], "{f:?}");
    assert_eq!(f[0].detail, "30 KLV PES on the wire, digest absent");
    // Reviewed private data on the PID excuses it, within the bound.
    let ann = Annotations {
        non_misb_private_pids: vec![KLV_PID],
        max_private_records: Some(30),
        reason: Some("vendor private data".into()),
        ..Default::default()
    };
    let f = check(&d, &w, &ann);
    assert_eq!(rules(&f), ["klv-missing"], "{f:?}");
    assert!(f[0].excused, "{f:?}");
    assert_eq!(
        f[0].detail,
        "excused: vendor private data; 30 KLV PES on the wire, digest absent (bound 30)"
    );
    let tight = Annotations {
        max_private_records: Some(29),
        ..ann
    };
    let f = check(&d, &w, &tight);
    assert!(!f[0].excused, "{f:?}");
}

/// A KLV PID whose records all arrive as metadata events passes; one whose
/// digest keeps its kind but counts no metadata does not.
#[test]
fn klv_pes_with_no_metadata_events_fails_klv_missing() {
    let bytes = muxed(Some(None));
    let w = walk(&bytes);
    let mut d = digest_config("default", &bytes, FEED_CHUNK);
    assert!(check(&d, &w, &Annotations::default()).is_empty());
    d.pids.get_mut(&KLV_PID).unwrap().metadata = 0;
    let f = check(&d, &w, &Annotations::default());
    assert_eq!(rules(&f), ["klv-missing"], "{f:?}");
    assert_eq!(
        f[0].detail,
        "30 KLV PES on the wire, digest 0 metadata events"
    );
}

/// Private data on a 0x06 stream without a KLVA registration is not
/// expected to carry KLV.
#[test]
fn a_private_stream_without_klva_is_not_expected_to_carry_klv() {
    let mut cc = 0u8;
    let mut bytes = section_packets(0, &mut cc, &pat(0));
    bytes.extend(section_packets(
        PMT_PID,
        &mut cc,
        &pmt(0, &[(0x06, 0x200)], 0),
    ));
    bytes.extend(es(0x200, &[0, 1, 2, 3]));
    assert!(walk(&bytes).klv_pes_starts.is_empty());
}

/// A PES start on a 0x15 stream, or on a 0x06 stream with a KLVA
/// registration, is counted; a payload that does not open a PES is not.
#[test]
fn klv_pes_starts_are_counted_on_klv_carriers_only() {
    let pes = [0u8, 0, 1, 0xFC, 0, 0];
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    let mut body = vec![0xE1, 0x00, 0xF0, 0x00];
    body.extend_from_slice(&[0x15, 0xE3, 0x00, 0xF0, 0x00]);
    body.extend_from_slice(&[0x06, 0xE3, 0x01, 0xF0, 0x06, 0x05, 0x04]);
    body.extend_from_slice(b"KLVA");
    body.extend_from_slice(&[0x06, 0xE3, 0x02, 0xF0, 0x00]);
    bytes.extend(psi.pmt(&long_section(2, 1, 0, &body)));
    for pid in [0x300, 0x301, 0x302] {
        bytes.extend(packet(pid, 0, true, &pes));
        bytes.extend(packet(pid, 1, false, &pes));
        bytes.extend(packet(pid, 2, true, &[0xAA]));
        bytes.extend(packet(pid, 3, true, &pes));
    }
    let w = walk(&bytes);
    assert_eq!(
        w.klv_pes_starts.into_iter().collect::<Vec<_>>(),
        [(0x300, 2), (0x301, 2)]
    );
}

// ---- KLV carriers: the walk must classify as the demuxer does

fn registration(id: &[u8; 4]) -> Vec<u8> {
    [&[0x05, 4][..], id].concat()
}

/// A `metadata_descriptor` naming format 0xFF with `id`, under
/// application format 0xFFFF with the same identifier.
fn metadata_descriptor(id: &[u8; 4]) -> Vec<u8> {
    let mut d = vec![0x26, 13, 0xFF, 0xFF];
    d.extend_from_slice(id);
    d.push(0xFF);
    d.extend_from_slice(id);
    d.extend_from_slice(&[0x00, 0x0F]);
    d
}

/// One PMT declaring each `(stream_type, descriptors)` on PIDs from 0x300
/// on, then one PES start per PID.
fn carrier_capture(streams: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut body = vec![0xE1, 0x00, 0xF0, 0x00];
    for (i, (ty, desc)) in streams.iter().enumerate() {
        let pid = 0x300 + i as u16;
        body.extend_from_slice(&[*ty, 0xE0 | (pid >> 8) as u8, pid as u8]);
        body.extend_from_slice(&[0xF0 | (desc.len() >> 8) as u8, desc.len() as u8]);
        body.extend_from_slice(desc);
    }
    let mut psi = Psi::new();
    let mut bytes = psi.pat(0);
    bytes.extend(psi.pmt(&long_section(2, 1, 0, &body)));
    for i in 0..streams.len() {
        bytes.extend(packet(0x300 + i as u16, 0, true, &[0, 0, 1, 0xFC, 0, 0]));
    }
    bytes
}

fn walked_carriers(bytes: &[u8]) -> Vec<u16> {
    walk(bytes).klv_pes_starts.into_keys().collect()
}

/// The PIDs the demuxer's PMT classification calls KLV.
fn demuxed_carriers(bytes: &[u8]) -> Vec<u16> {
    use tst_core::mpegts::demux::StreamKind;
    let mut d = Demuxer::new();
    let _ = d.feed(bytes);
    d.flush();
    let mut out = Vec::new();
    while let Some(ev) = d.next_event() {
        if let DemuxEvent::ProgramMap(pm) = ev {
            out = pm
                .streams
                .iter()
                .filter(|s| matches!(s.kind, StreamKind::KlvSync { .. } | StreamKind::KlvAsync))
                .map(|s| s.pid)
                .collect();
        }
    }
    out.sort();
    out
}

#[test]
fn a_0x15_stream_whose_metadata_descriptor_names_id3_is_not_a_klv_carrier() {
    let bytes = carrier_capture(&[
        (0x15, metadata_descriptor(b"ID3 ")),
        (0x15, metadata_descriptor(b"KLVA")),
    ]);
    assert_eq!(walked_carriers(&bytes), [0x301]);
}

#[test]
fn a_0x06_klva_stream_another_kind_claims_first_is_not_a_klv_carrier() {
    let klva = registration(b"KLVA");
    let bytes = carrier_capture(&[
        (0x06, [registration(b"AV01"), klva.clone()].concat()),
        (
            0x06,
            [
                klva.clone(),
                vec![0x59, 8, b'e', b'n', b'g', 0x10, 0, 1, 0, 2],
            ]
            .concat(),
        ),
        (0x06, klva),
    ]);
    assert_eq!(walked_carriers(&bytes), [0x302]);
}

/// The walk's copy of the demuxer's cascade, case by case against the
/// demuxer itself, so neither can drift from the other unseen.
#[test]
fn the_walks_klv_carriers_are_the_demuxers() {
    let klva = registration(b"KLVA");
    let with = |extra: Vec<u8>| [klva.clone(), extra].concat();
    let streams = vec![
        (0x15, vec![]),
        (0x15, metadata_descriptor(b"KLVA")),
        (0x15, metadata_descriptor(b"ID3 ")),
        // Format 0x10 (TeM) under application format 0x0100.
        (0x15, vec![0x26, 5, 0x01, 0x00, 0x10, 0x00, 0x0F]),
        // The same descriptor cut short names nothing: still KLV.
        (0x15, vec![0x26, 3, 0x01, 0x00, 0x10]),
        (0x06, vec![]),
        (0x06, klva.clone()),
        (0x06, with(registration(b"AV01"))),
        (
            0x06,
            with(vec![0x59, 8, b'e', b'n', b'g', 0x10, 0, 1, 0, 2]),
        ),
        (0x06, with(vec![0x56, 5, b'e', b'n', b'g', 0x09, 0x00])),
        (0x06, with(vec![0x46, 5, b'e', b'n', b'g', 0x09, 0x00])),
        (0x06, with(registration(b"VTTC"))),
        (0x06, with(registration(b"GA94"))),
        (0x06, registration(b"ID3 ")),
        (0x1B, klva.clone()),
        // Format 0x11 (BiM) under application format 0x0100.
        (0x15, vec![0x26, 5, 0x01, 0x00, 0x11, 0x00, 0x0F]),
        // Application format 0xFFFF carries a 4-byte identifier, so the
        // format (0x10, not 0xFF) sits at offset 6.
        (
            0x15,
            vec![
                0x26, 9, 0xFF, 0xFF, b'a', b'b', b'c', b'd', 0x10, 0x00, 0x0F,
            ],
        ),
    ];
    let bytes = carrier_capture(&streams);
    let demuxed = demuxed_carriers(&bytes);
    assert_eq!(
        demuxed,
        [0x300, 0x301, 0x304, 0x306],
        "the cases cover both answers"
    );
    assert_eq!(walked_carriers(&bytes), demuxed);
}
