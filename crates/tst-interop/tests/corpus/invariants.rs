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
