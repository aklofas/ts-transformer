//! Mutation tests (spec §6.5): every wire oracle must FAIL when the
//! property it checks is removed, and the unmutated stream must pass. No
//! production-code test hooks — profile-level mutations tweak a copied
//! `Profile`; byte-level ones edit the generated file.

use std::path::PathBuf;

use tst_core::mpegts::mux::Av1CarriageMode;
use tst_interop::fixtures::{AuSizeMode, KlvSet};
use tst_interop::profiles::{self, Profile};
use tst_interop::report_types::VerifyReport;
use tst_interop::{r#gen, verify};

const PKT: usize = 188;

fn gen_to_temp(p: &Profile, seconds: f64, tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("tst-interop-mut-{tag}-{}.ts", std::process::id()));
    r#gen::run(p, seconds, &path, KlvSet::Compact, 0, AuSizeMode::Compact).expect("gen");
    path
}

fn verify_bytes(bytes: &[u8], p: &Profile, seconds: f64, tag: &str) -> VerifyReport {
    let path = std::env::temp_dir().join(format!(
        "tst-interop-mut-{tag}-verify-{}.ts",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write");
    let r = verify::verify_file(&path, p, seconds).expect("verify io");
    let _ = std::fs::remove_file(&path);
    r
}

fn assert_fails_with(r: &VerifyReport, verdict: &str) {
    assert!(
        !r.pass,
        "expected a failure starting with {verdict:?}, but the report passed"
    );
    assert!(
        r.failures.iter().any(|f| f.starts_with(verdict)),
        "expected a failure starting with {verdict:?}, got {:?}",
        r.failures
    );
}

fn pid_of(pkt: &[u8]) -> u16 {
    (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2])
}

fn packets(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.chunks_exact(PKT)
}

fn pid_filter(bytes: &[u8], drop: &[u16]) -> Vec<u8> {
    packets(bytes)
        .filter(|p| !drop.contains(&pid_of(p)))
        .flatten()
        .copied()
        .collect()
}

fn drop_nth_packet_on_pid(bytes: &[u8], pid: u16, n: usize) -> Vec<u8> {
    let mut seen = 0;
    let mut out = Vec::with_capacity(bytes.len());
    for p in packets(bytes) {
        if pid_of(p) == pid {
            seen += 1;
            if seen == n {
                continue;
            }
        }
        out.extend_from_slice(p);
    }
    out
}

/// CRC-32/MPEG-2: poly 0x04C11DB7, init 0xFFFFFFFF, no reflection, no xorout.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Rewrite `es_pid`'s stream_type in every PMT section on `pmt_pid`,
/// recomputing the section CRC so the demuxer still accepts it (the point
/// is to fool the PMT oracle, not to inject a CRC error).
fn rewrite_pmt_stream_type(bytes: &[u8], pmt_pid: u16, es_pid: u16, new_type: u8) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for chunk in out.chunks_exact_mut(PKT) {
        if pid_of(chunk) != pmt_pid || chunk[1] & 0x40 == 0 {
            continue;
        }
        let afc = (chunk[3] >> 4) & 3;
        let mut off = 4;
        if afc & 2 != 0 {
            off = 5 + usize::from(chunk[4]);
        }
        let ptr = usize::from(chunk[off]);
        let sec_start = off + 1 + ptr;
        let sec_len =
            (usize::from(chunk[sec_start + 1] & 0x0F) << 8) | usize::from(chunk[sec_start + 2]);
        let sec_end = sec_start + 3 + sec_len; // exclusive, includes CRC
        let body_start = sec_start + 3;
        let info_len =
            (usize::from(chunk[body_start + 7] & 0x0F) << 8) | usize::from(chunk[body_start + 8]);
        let mut k = body_start + 9 + info_len;
        while k + 5 <= sec_end - 4 {
            let pid = (u16::from(chunk[k + 1] & 0x1F) << 8) | u16::from(chunk[k + 2]);
            let es_len = (usize::from(chunk[k + 3] & 0x0F) << 8) | usize::from(chunk[k + 4]);
            if pid == es_pid {
                chunk[k] = new_type;
            }
            k += 5 + es_len;
        }
        let crc = crc32_mpeg2(&chunk[sec_start..sec_end - 4]);
        chunk[sec_end - 4..sec_end].copy_from_slice(&crc.to_be_bytes());
    }
    out
}

fn pes_payload_start(chunk: &[u8]) -> Option<usize> {
    if chunk[1] & 0x40 == 0 {
        return None;
    }
    let afc = (chunk[3] >> 4) & 3;
    let mut off = 4;
    if afc & 2 != 0 {
        off = 5 + usize::from(chunk[4]);
    }
    (chunk.get(off..off + 3) == Some(&[0, 0, 1])).then_some(off)
}

fn flip_pes_stream_id(bytes: &[u8], pid: u16, from: u8, to: u8) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for chunk in out.chunks_exact_mut(PKT) {
        if pid_of(chunk) != pid {
            continue;
        }
        if let Some(off) = pes_payload_start(chunk) {
            if chunk[off + 3] == from {
                chunk[off + 3] = to;
            }
        }
    }
    out
}

fn zero_adts_syncword(bytes: &[u8], pid: u16) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for chunk in out.chunks_exact_mut(PKT) {
        if pid_of(chunk) != pid {
            continue;
        }
        if let Some(off) = pes_payload_start(chunk) {
            let hdr_len = usize::from(chunk[off + 8]);
            let d = off + 9 + hdr_len;
            if d < PKT {
                chunk[d] = 0x00;
            }
        }
    }
    out
}

/// Overrun `PES_header_data_length` (byte 8 of the PES header, ITU-T
/// H.222.0 §2.4.3.7) on every `nth` PES start of `pid`. The demuxer's
/// `parse_complete` rejects the record ("PES too short for declared
/// header_data_length", tst-core `demux/pes.rs`) as a `MalformedPes`
/// non-conformance and emits NO `Metadata` for it, while the packet —
/// and its PES start — stays on the wire for the raw reader to count.
/// KLV rather than video because a compact KLV PES is ~60 bytes, so a
/// 255-byte header claim is guaranteed to overrun it; a video AU is
/// kilobytes and would merely lose 255 payload bytes.
fn overrun_pes_header_len_on_every_nth(bytes: &[u8], pid: u16, nth: usize) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let mut seen = 0;
    for chunk in out.chunks_exact_mut(PKT) {
        if pid_of(chunk) != pid {
            continue;
        }
        if let Some(off) = pes_payload_start(chunk) {
            seen += 1;
            if seen % nth == 0 {
                chunk[off + 8] = 0xFF;
            }
        }
    }
    out
}

const SECONDS: f64 = 3.0;
const ROLLOVER_SECONDS: f64 = 7.0;
const PROG1_PMT: u16 = 0x1000;
const PROG1_VIDEO: u16 = 0x1011;
const PROG1_KLV: u16 = 0x1031;
const PROG1_AUDIO: u16 = 0x1041;
const PROG2_VIDEO: u16 = 0x1111;
const PROG2_KLV: u16 = 0x1131;

#[test]
fn dropping_program_2_media_while_keeping_its_pmt_fails_program_accounting() {
    let p = profiles::by_name("two-program").unwrap();
    let path = gen_to_temp(p, SECONDS, "two-program");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mutated = pid_filter(&bytes, &[PROG2_VIDEO, PROG2_KLV]);
    let r = verify_bytes(&mutated, p, SECONDS, "two-program");
    assert_fails_with(&r, "program_2_video_floor");
    assert_fails_with(&r, "program_2_wire_media");
    // Program 1 must still be fine — the failure is localized.
    assert!(
        !r.failures.iter().any(|f| f.starts_with("program_1_")),
        "{:?}",
        r.failures
    );
}

#[test]
fn one_audio_frame_fails_audio_cadence() {
    // Profile-level: `schedule` derives the frame count from seconds; a
    // window so short it yields one frame is the honest way to get "one
    // frame" without a hook. verify against the FULL window.
    let p = profiles::by_name("audio").unwrap();
    let path = gen_to_temp(p, 1024.0 / 48_000.0, "audio-one");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let r = verify_bytes(&bytes, p, SECONDS, "audio-one");
    assert_fails_with(&r, "audio_cadence");
}

#[test]
fn zeroed_adts_syncword_fails_audio_codec() {
    let p = profiles::by_name("audio").unwrap();
    let path = gen_to_temp(p, SECONDS, "audio-sync");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let r = verify_bytes(
        &zero_adts_syncword(&bytes, PROG1_AUDIO),
        p,
        SECONDS,
        "audio-sync",
    );
    assert_fails_with(&r, "audio_codec_adts");
}

#[test]
fn pcr_tight_muxed_at_the_baseline_interval_fails_pcr_interval() {
    let tight = profiles::by_name("pcr-tight").unwrap();
    let mutated = Profile {
        pcr_interval_ms: 40,
        ..*tight
    };
    let path = gen_to_temp(&mutated, SECONDS, "pcr-tight-40");
    let r = verify::verify_file(&path, tight, SECONDS).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_fails_with(&r, "pcr_interval");
}

#[test]
fn pcr_sparse_muxed_at_the_baseline_interval_fails_pcr_interval() {
    let sparse = profiles::by_name("pcr-sparse").unwrap();
    let mutated = Profile {
        pcr_interval_ms: 40,
        ..*sparse
    };
    let path = gen_to_temp(&mutated, SECONDS, "pcr-sparse-40");
    let r = verify::verify_file(&path, sparse, SECONDS).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_fails_with(&r, "pcr_interval");
}

#[test]
fn swapped_av1_carriage_fails_av1_carriage_wire_in_both_directions() {
    for (name, other) in [
        ("av1-klv-a", Av1CarriageMode::Mpeg2TsBinding),
        ("av1-klv-b", Av1CarriageMode::InteropRawObu),
    ] {
        let p = profiles::by_name(name).unwrap();
        let mutated = Profile {
            av1_mode: Some(other),
            ..*p
        };
        let path = gen_to_temp(&mutated, SECONDS, name);
        let r = verify::verify_file(&path, p, SECONDS).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_fails_with(&r, "av1_carriage_wire");
    }
}

#[test]
fn pts_rollover_starting_at_zero_fails_pts_wrap_observed() {
    let p = profiles::by_name("pts-rollover").unwrap();
    let mutated = Profile {
        start_pts_ticks: 0,
        ..*p
    };
    let path = gen_to_temp(&mutated, ROLLOVER_SECONDS, "rollover-zero");
    let r = verify::verify_file(&path, p, ROLLOVER_SECONDS).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_fails_with(&r, "pts_wrap_observed");
}

#[test]
fn rewritten_pmt_stream_type_fails_pmt_stream_type() {
    let p = profiles::by_name("baseline").unwrap();
    let path = gen_to_temp(p, SECONDS, "pmt");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mutated = rewrite_pmt_stream_type(&bytes, PROG1_PMT, PROG1_VIDEO, 0x24);
    let r = verify_bytes(&mutated, p, SECONDS, "pmt");
    assert_fails_with(&r, &format!("pmt_stream_type_{PROG1_VIDEO}"));
    // The CRC rewrite must have been accepted: the PMT still parses and
    // program 1 is still seen (a CRC-rejected PMT would yield 0 programs).
    // What the demuxer's H.265 parser makes of the still-H.264-coded video
    // bytes underneath the relabeled stream_type is not this test's
    // business — that's the `nonconformant_event` oracle's job elsewhere,
    // not this PMT-oracle mutation's.
    assert_eq!(r.metrics.programs_seen, 1, "{:?}", r.failures);
}

#[test]
fn av1_binding_stream_with_video_stream_id_raises_nonconformant() {
    let p = profiles::by_name("av1-klv-b").unwrap();
    let path = gen_to_temp(p, SECONDS, "av1-nc");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let r = verify_bytes(
        &flip_pes_stream_id(&bytes, PROG1_VIDEO, 0xBD, 0xE0),
        p,
        SECONDS,
        "av1-nc",
    );
    assert_fails_with(&r, "nonconformant_event");
    assert!(r.metrics.nonconformant > 0);
}

#[test]
fn a_dropped_video_packet_is_fatal_in_strict_and_counted_in_lossy() {
    let p = profiles::by_name("baseline").unwrap();
    let path = gen_to_temp(p, SECONDS, "disc");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mutated = drop_nth_packet_on_pid(&bytes, PROG1_VIDEO, 40);
    let strict = verify_bytes(&mutated, p, SECONDS, "disc");
    assert_fails_with(&strict, "discontinuity_event");
    assert_eq!(strict.metrics.discontinuities, 1);
    // Lossy: same bytes through the Tally with VerifyMode::Lossy — verify_file
    // is Strict-only, so drive the demuxer directly.
    let lossy = tst_interop::verify::verify_bytes_with_mode(
        &mutated,
        p,
        SECONDS,
        tst_interop::verify::VerifyMode::Lossy,
    );
    assert!(lossy.pass, "{:?}", lossy.failures);
    assert_eq!(lossy.metrics.discontinuities, 1);
}

/// CORR-07: a demuxer that silently loses one record in four stays
/// inside the 70 % count slack (23 of 30 >= 21), so before the
/// `wire_vs_demux_*` oracle the only failure this mutation produced was
/// the `nonconformant_event` the demuxer happened to report. The wire
/// oracle must fail on the COUNT, independent of any event.
#[test]
fn every_fourth_klv_record_lost_in_the_demuxer_fails_wire_vs_demux() {
    let p = profiles::by_name("baseline").unwrap();
    let path = gen_to_temp(p, SECONDS, "wire-vs-demux");
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mutated = overrun_pes_header_len_on_every_nth(&bytes, PROG1_KLV, 4);
    let r = verify_bytes(&mutated, p, SECONDS, "wire-vs-demux");
    // 30 records at 10 Hz over 3 s; every 4th (7 of them) is rejected.
    assert_eq!(r.metrics.klv_records, 23, "{:?}", r.failures);
    // The pre-existing oracle still fires (the demuxer reported the
    // malformed header) …
    assert_fails_with(&r, "nonconformant_event");
    // … and the 70 % floor does NOT (23 >= 21): that gap is the finding.
    assert!(
        !r.failures.iter().any(|f| f.starts_with("KLV records:")),
        "the count floor must be the hole this oracle closes: {:?}",
        r.failures
    );
    assert_fails_with(&r, &format!("wire_vs_demux_{PROG1_KLV}"));
}
