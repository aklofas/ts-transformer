//! Tests of the raw-TS oracle (`tst_test_helpers::rawts`). They drive the
//! generator, profiles and oracles of this crate, so they live here and not
//! beside the reader.

use std::collections::{BTreeMap, BTreeSet};
use tst_test_helpers::rawts::*;

const PKT: usize = 188;
const SYNC: u8 = 0x47;
const TS_MODULUS: u64 = 1 << 33;

/// Per-process counter that keeps two concurrently-running tests
/// from picking the same scratch path. Two of these helpers' callers
/// ask for the same profile, and cargo runs tests in parallel, so a
/// path keyed only on the profile name has one test reading the file
/// another is still writing.
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch_path(tag: &str) -> std::path::PathBuf {
    let n = SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tst-interop-rawts-{tag}-{}-{n}.ts",
        std::process::id()
    ))
}

fn summary_of(profile: &str, seconds: f64) -> WireSummary {
    let p = crate::profiles::by_name(profile).unwrap();
    let path = scratch_path(profile);
    crate::r#gen::run(
        p,
        seconds,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let s = summarize_file(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    s
}

/// A [`Retention::Bounded`] series must stop growing while every
/// statistic that is not a median stays exactly what an unbounded one
/// would report. This is the whole memory argument for a multi-day
/// receive loop, asserted directly: `SAMPLE_CAP * 8` bytes and not a
/// byte more, no matter how long the capture runs.
#[test]
fn a_bounded_series_caps_its_sample_and_keeps_its_counters_exact() {
    const N: u64 = 200_000;
    let step = 1_800u64;
    let values: Vec<u64> = (0..N).map(|i| (i * step) % TS_MODULUS).collect();
    let bounded = TimestampSeries::from_values(Retention::Bounded, 0x1011, &values);
    let full = TimestampSeries::from_values(Retention::Full, 0x1011, &values);

    assert!(
        bounded.retained() <= SAMPLE_CAP,
        "retained {} deltas, cap is {SAMPLE_CAP}",
        bounded.retained()
    );
    assert_eq!(full.retained(), (N - 1) as usize, "Full keeps everything");
    // Exact in both modes: counts, wrap count, and the forward
    // min/max `VerifyMode::Strict` gates every interval on.
    assert_eq!(bounded.count(), full.count());
    assert_eq!(bounded.intervals(), full.intervals());
    assert_eq!(bounded.wraps(), full.wraps());
    assert_eq!(bounded.forward_min(), full.forward_min());
    assert_eq!(bounded.forward_max(), full.forward_max());
    // The two medians are the same statistic drawn from a sample
    // rather than the population — on a steady cadence they agree
    // exactly, which is the case every oracle actually meets.
    assert_eq!(bounded.forward_median(), full.forward_median());
    assert_eq!(bounded.positive_step_median(), full.positive_step_median());
}

/// The bound must not change a verdict. Every wire oracle is run
/// twice over the same capture — once off a `Full` reader, once off a
/// `Bounded` one — and must produce identical failure lists, in both
/// verify modes, for a profile whose PCR cadence, audio cadence and
/// PTS rollover exercise all four series statistics.
#[test]
fn bounded_retention_does_not_change_any_wire_oracle_verdict() {
    for profile in ["baseline", "audio", "pts-rollover"] {
        let bytes = bytes_of(profile, 7.0, &format!("bounded-{profile}"));
        let summarize = |retention| {
            let mut r = Reader::with_retention(retention);
            r.feed(&bytes).unwrap();
            r.finish().unwrap()
        };
        let full = summarize(Retention::Full);
        let bounded = summarize(Retention::Bounded);
        let p = crate::profiles::by_name(profile).unwrap();
        let inv = crate::profiles::invariants(p);
        // The per-program demux counts the oracles also read are
        // irrelevant here (identical for both readers by
        // construction), so an empty map keeps the comparison on the
        // wire half, which is the half retention touches.
        let per_program = BTreeMap::new();
        for mode in [
            crate::verify::VerifyMode::Strict,
            crate::verify::VerifyMode::Lossy,
        ] {
            assert_eq!(
                crate::oracles::check(
                    p,
                    &inv,
                    &full,
                    &per_program,
                    7.0,
                    0.7,
                    mode,
                    &crate::oracles::Explained::default(),
                    crate::verify::WireLayout::Generator,
                ),
                crate::oracles::check(
                    p,
                    &inv,
                    &bounded,
                    &per_program,
                    7.0,
                    0.7,
                    mode,
                    &crate::oracles::Explained::default(),
                    crate::verify::WireLayout::Generator,
                ),
                "{profile} in {mode:?}"
            );
        }
    }
}

/// Bytes of `profile`/`seconds`, straight from the generator.
fn bytes_of(profile: &str, seconds: f64, tag: &str) -> Vec<u8> {
    let p = crate::profiles::by_name(profile).unwrap();
    let path = scratch_path(tag);
    crate::r#gen::run(
        p,
        seconds,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let b = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    b
}

/// A clean capture must have ZERO rejected sections. Without this the
/// CRC check below would pass just as well if `crc32_mpeg2` always
/// disagreed: "rejects a damaged table" and "rejects every table" look
/// identical from the damaged side.
#[test]
fn a_clean_capture_rejects_no_psi_section() {
    let s = summary_of("two-program", 3.0);
    assert_eq!(s.psi_crc_rejected, 0);
    assert_eq!(s.programs.len(), 2, "both PMTs were accepted");
}

/// One flipped byte in a PAT's program loop must not be trusted.
///
/// In `two-program`, PAT byte 16 is the low byte of program 1's PMT
/// PID (0x1000). Flipping it to 0x1031 names a PID that is a real
/// ELEMENTARY STREAM in this multiplex — so a reader that trusted the
/// CRC-failed table would register 0x1031 as a PMT PID and then fail
/// to parse every one of that stream's 600 packets as a PSI section,
/// skipping each one. Measured before the CRC check existed: 600
/// resyncs and 6000 of 6600 packets counted.
///
/// The section is IGNORED instead: the PID map is untouched, packet
/// framing is unaffected so nothing resyncs, every packet is counted,
/// and the discard is reported rather than hidden.
#[test]
fn a_crc_failed_pat_is_ignored_and_poisons_nothing() {
    let mut bytes = bytes_of("two-program", 60.0, "crcpat");
    let packets = bytes.len() / PKT;
    assert_eq!(packets, 6600);
    assert_eq!(
        classify_packet(bytes[..PKT].try_into().unwrap())
            .unwrap()
            .pid,
        0
    );
    bytes[16] ^= 0x31;

    let mut r = Reader::new();
    r.set_resync_mode(true);
    r.feed(&bytes).unwrap();
    assert!(
        !r.is_pmt_pid(0x1031),
        "an elementary stream must not become a PMT PID"
    );
    assert!(
        r.take_resyncs().is_empty(),
        "framing is intact: no sync was lost"
    );
    let s = r.finish().unwrap();
    assert_eq!(s.packets, packets as u64, "no packet was skipped");
    assert_eq!(s.psi_crc_rejected, 1, "exactly the damaged PAT");
    // The undamaged PAT repetitions still carry the real topology.
    assert_eq!(s.programs.len(), 2);
    assert_eq!(s.programs[&1].pmt_pid, 0x1000);
}

#[test]
fn baseline_pmt_and_pes_shapes_match_the_measured_wire() {
    let s = summary_of("baseline", 3.0);
    let prog = &s.programs[&1];
    assert_eq!(prog.pmt_pid, 0x1000);
    assert_eq!(prog.pcr_pid, 0x1011);
    let video = prog.streams.iter().find(|st| st.pid == 0x1011).unwrap();
    assert_eq!(video.stream_type, 0x1B);
    assert_eq!(video.registration, None);
    let klv = prog.streams.iter().find(|st| st.pid == 0x1031).unwrap();
    assert_eq!(klv.stream_type, 0x06);
    assert_eq!(klv.registration, Some(*b"KLVA"));
    assert_eq!(s.pes[&0x1011].stream_ids, BTreeSet::from([0xE0]));
    assert_eq!(s.pes[&0x1011].first_payload_prefix, Some([0, 0, 0, 1]));
    assert_eq!(s.pes[&0x1031].stream_ids, BTreeSet::from([0xBD]));
    assert_eq!(s.pts[&0x1011].count(), 90);
    assert_eq!(s.pcr[&0x1011].count(), 45);
    assert_eq!(s.packets, 180);
}

#[test]
fn klv_sync_pmt_carries_0x15_with_metadata_descriptors() {
    let s = summary_of("klv-sync", 3.0);
    let klv = s.programs[&1]
        .streams
        .iter()
        .find(|st| st.pid == 0x1031)
        .unwrap();
    assert_eq!(klv.stream_type, 0x15);
    assert_eq!(klv.registration, Some(*b"KLVA"));
    assert!(klv.descriptor_tags.contains(&0x26) && klv.descriptor_tags.contains(&0x27));
    assert_eq!(s.pes[&0x1031].stream_ids, BTreeSet::from([0xFC]));
}

#[test]
fn av1_modes_differ_only_in_pes_stream_id_and_prefix() {
    let a = summary_of("av1-klv-a", 3.0);
    let b = summary_of("av1-klv-b", 3.0);
    assert_eq!(
        a.programs[&1]
            .streams
            .iter()
            .map(|s| (s.pid, s.stream_type, s.registration))
            .collect::<Vec<_>>(),
        b.programs[&1]
            .streams
            .iter()
            .map(|s| (s.pid, s.stream_type, s.registration))
            .collect::<Vec<_>>()
    );
    assert_eq!(a.pes[&0x1011].stream_ids, BTreeSet::from([0xE0]));
    assert_eq!(b.pes[&0x1011].stream_ids, BTreeSet::from([0xBD]));
    assert_eq!(
        &b.pes[&0x1011].first_payload_prefix.unwrap()[..3],
        &[0, 0, 1]
    );
    assert_ne!(
        &a.pes[&0x1011].first_payload_prefix.unwrap()[..3],
        &[0, 0, 1]
    );
}

#[test]
fn two_program_summary_has_both_programs_with_distinct_pids() {
    let s = summary_of("two-program", 3.0);
    assert_eq!(s.programs.len(), 2);
    assert_eq!(s.programs[&2].pmt_pid, 0x1100);
    assert!(s.packets_per_pid[&0x1111] > 0 && s.packets_per_pid[&0x1131] > 0);
}

#[test]
fn pts_rollover_stream_shows_one_raw_pts_decrease_at_seven_seconds() {
    let s = summary_of("pts-rollover", 7.0);
    // The series counts a rollover exactly (a property of one
    // consecutive pair), which is what the `pts_wrap` oracle reads.
    assert_eq!(s.pts[&0x1011].wraps(), 1);
}

/// `take_pcr_events` must report each base against the ordinal of the
/// packet that CARRIED it, independently of how the bytes were
/// chunked into `feed`. Checked against `classify_packet` walked over
/// the same bytes packet by packet — the `Reader`'s own streaming
/// bookkeeping compared to a flat, chunk-free enumeration.
#[test]
fn take_pcr_events_stamps_each_base_at_its_own_packet_ordinal() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path =
        std::env::temp_dir().join(format!("tst-interop-rawts-pcrat-{}.ts", std::process::id()));
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let want: Vec<(u64, u64)> = bytes
        .chunks_exact(PKT)
        .enumerate()
        .filter_map(|(i, c)| {
            let pkt: [u8; PKT] = c.try_into().expect("PKT bytes");
            classify_packet(&pkt)
                .expect("generated packets classify")
                .pcr_base
                .map(|b| (b, i as u64))
        })
        .collect();
    assert!(!want.is_empty(), "the baseline profile carries PCRs");

    // Odd chunk size on purpose: a PCR packet routinely straddles two
    // feeds, which is exactly the case a "did the latest PCR base
    // change since the previous feed" diff would mis-stamp.
    let mut r = Reader::new();
    let mut got = Vec::new();
    for chunk in bytes.chunks(101) {
        r.feed(chunk).unwrap();
        got.extend(r.take_pcr_events());
    }
    assert_eq!(got, want);
    // Drained: everything was taken, nothing is reported twice.
    assert!(r.take_pcr_events().is_empty());
}

/// Attribution anchors come from the PIDs the PMT declares as PCR_PID.
/// After a framing injection the hunt can lock on a payload 0x47 whose
/// "adaptation field" happens to carry the PCR flag; a PCR decoded from
/// such a packet on a PID that carries no PCR is noise, not an anchor.
#[test]
fn pcr_events_come_only_from_declared_pcr_pids() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path = scratch_path("pcrpid");
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let declared = summarize_file(&path)
        .unwrap()
        .programs
        .values()
        .next()
        .unwrap()
        .pcr_pid;
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut r = Reader::new();
    r.feed(&bytes).unwrap();
    assert!(!r.take_pcr_events().is_empty(), "the fixture carries PCRs");

    // A packet with an adaptation field carrying a PCR on the KLV PID.
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x10; // pusi 0, pid 0x1031
    pkt[2] = 0x31;
    pkt[3] = 0x30; // AF + payload, cc 0
    pkt[4] = 7; // adaptation_field_length
    pkt[5] = 0x10; // PCR flag
    pkt[6..12].copy_from_slice(&[0x00, 0x00, 0x12, 0x34, 0x7E, 0x00]);
    r.feed(&pkt).unwrap();
    assert!(
        r.take_pcr_events().is_empty(),
        "a PCR on an undeclared PID must not become an anchor"
    );

    pkt[1] = (declared >> 8) as u8 & 0x1F;
    pkt[2] = declared as u8;
    r.feed(&pkt).unwrap();
    assert_eq!(
        r.take_pcr_events().len(),
        1,
        "the declared PCR PID still anchors"
    );
}

#[test]
fn feed_tolerates_arbitrary_chunking_and_rejects_sync_loss() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path =
        std::env::temp_dir().join(format!("tst-interop-rawts-chunk-{}.ts", std::process::id()));
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut r = Reader::new();
    for chunk in bytes.chunks(101) {
        r.feed(chunk).unwrap();
    }
    assert_eq!(r.finish().unwrap().packets as usize, bytes.len() / 188);
    let mut bad = bytes.clone();
    bad[188 * 5] = 0x00;
    let mut r = Reader::new();
    let e = r.feed(&bad).unwrap_err();
    assert!(e.contains("sync"), "{e}");
}

#[test]
fn resync_discards_a_stale_partial_packet_without_losing_the_summary() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path = std::env::temp_dir().join(format!(
        "tst-interop-rawts-resync-{}.ts",
        std::process::id()
    ));
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    let mut r = Reader::new();
    // A partial packet (100 bytes < 188) left over from a connection
    // that just broke — never completes on its own.
    r.feed(&bytes[..100]).unwrap();
    r.resync();
    // A whole, independent valid stream from the "reconnected"
    // transport must feed clean (no sync-loss error) and its packet
    // count must reflect only THIS stream — the discarded partial
    // packet contributed nothing.
    r.feed(&bytes).unwrap();
    assert_eq!(r.finish().unwrap().packets as usize, bytes.len() / 188);
}

#[test]
fn finish_rejects_a_trailing_partial_packet() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path = std::env::temp_dir().join(format!(
        "tst-interop-rawts-trailing-{}.ts",
        std::process::id()
    ));
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let cut = 3 * PKT + 100;
    assert!(bytes.len() > cut, "fixture too short for this test");

    let mut r = Reader::new();
    r.feed(&bytes[..cut]).unwrap();
    let e = r.finish().unwrap_err();
    assert!(e.contains("trailing"), "{e}");
}

#[test]
fn feed_rejects_a_pat_pointer_field_past_the_payload() {
    // A minimal PID-0 (PAT) packet, PUSI set, adaptation_field_control
    // = payload-only (afc=01 -> byte 3 = 0x10), pointer_field = 255 —
    // nowhere near valid for a 184-byte payload, so `section()` must
    // reject it rather than reading (or panicking on) bytes past the
    // packet.
    let mut pkt = [0xFFu8; PKT];
    pkt[0] = SYNC;
    pkt[1] = 0x40; // PUSI set, PID high bits = 0
    pkt[2] = 0x00; // PID low byte = 0 (PAT)
    pkt[3] = 0x10; // afc = payload only
    pkt[4] = 0xFF; // pointer_field = 255

    let mut r = Reader::new();
    let e = r.feed(&pkt).unwrap_err();
    assert!(e.contains("pointer field"), "{e}");
}

#[test]
fn feed_rejects_a_pcr_flag_with_a_too_short_adaptation_field() {
    // adaptation_field_control = adaptation-field-only (afc=10 ->
    // byte 3 = 0x20), adaptation_field_length = 3 (too short to hold
    // the flags byte + a 6-byte PCR), PCR flag set anyway — malformed,
    // must error rather than reading past the declared field.
    let mut pkt = [0xFFu8; PKT];
    pkt[0] = SYNC;
    pkt[1] = 0x00;
    pkt[2] = 0x11; // an arbitrary non-PAT/PMT PID
    pkt[3] = 0x20; // afc = adaptation field only
    pkt[4] = 3; // adaptation_field_length
    pkt[5] = 0x10; // PCR flag set

    let mut r = Reader::new();
    let e = r.feed(&pkt).unwrap_err();
    assert!(e.contains('7'), "{e}");
}

#[test]
fn classify_packet_reports_pid_pusi_pcr_and_payload_offset() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path =
        std::env::temp_dir().join(format!("tst-interop-rawts-cls-{}.ts", std::process::id()));
    crate::r#gen::run(
        p,
        1.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let first: [u8; PKT] = bytes[..PKT].try_into().unwrap();
    let info = classify_packet(&first).unwrap();
    assert_eq!(info.pid, 0, "first packet of a fresh mux is the PAT");
    assert!(info.pusi);
    assert!(info.has_payload);
    assert_eq!(info.payload_off, 4);
    // Find a PCR-bearing packet and check the base decodes.
    let pcr_pkt = bytes
        .chunks_exact(PKT)
        .find(|c| c[3] & 0x20 != 0 && c[4] > 0 && c[5] & 0x10 != 0)
        .unwrap();
    let info = classify_packet(pcr_pkt.try_into().unwrap()).unwrap();
    assert!(info.pcr_base.is_some());
    assert_eq!(info.payload_off, 5 + usize::from(pcr_pkt[4]));
    let mut bad = first;
    bad[0] = 0x00;
    assert!(classify_packet(&bad).unwrap_err().contains("sync"));
}

#[test]
fn reader_exposes_packet_count_pcr_events_and_pmt_pids() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path =
        std::env::temp_dir().join(format!("tst-interop-rawts-acc-{}.ts", std::process::id()));
    crate::r#gen::run(
        p,
        1.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut r = Reader::new();
    assert_eq!(r.packets(), 0);
    assert!(r.take_pcr_events().is_empty());
    r.feed(&bytes).unwrap();
    assert_eq!(r.packets() as usize, bytes.len() / PKT);
    assert!(r.take_pcr_events().last().is_some());
    assert!(r.is_pmt_pid(0x1000));
    assert!(!r.is_pmt_pid(0x1011));
}

/// `pes_starts_per_pid` counts one per PES start on every PID —
/// including the async KLV PID, whose PES may carry no PTS — and
/// counts a spec-legal duplicate packet once.
#[test]
fn reader_counts_pes_starts_per_pid_and_ignores_a_duplicate_packet() {
    let bytes = bytes_of("baseline", 3.0, "pes-starts");
    let mut r = Reader::new();
    r.feed(&bytes).unwrap();
    let s = r.finish().unwrap();
    assert_eq!(s.pes_starts_per_pid[&0x1011], 90, "30 fps x 3 s");
    assert_eq!(s.pes_starts_per_pid[&0x1031], 30, "10 Hz x 3 s");

    // Duplicate the first video PES-start packet in place (same CC,
    // same bytes): the count must not move.
    let first = bytes
        .chunks_exact(PKT)
        .position(|c| {
            let pkt: [u8; PKT] = c.try_into().unwrap();
            let info = classify_packet(&pkt).unwrap();
            info.pid == 0x1011 && info.pusi
        })
        .expect("a video PES start");
    let mut dup = Vec::with_capacity(bytes.len() + PKT);
    dup.extend_from_slice(&bytes[..(first + 1) * PKT]);
    dup.extend_from_slice(&bytes[first * PKT..(first + 1) * PKT]);
    dup.extend_from_slice(&bytes[(first + 1) * PKT..]);
    let mut r = Reader::new();
    r.feed(&dup).unwrap();
    let s = r.finish().unwrap();
    assert_eq!(
        s.pes_starts_per_pid[&0x1011], 90,
        "a duplicate is not a second PES"
    );
    assert_eq!(s.packets_per_pid[&0x1011], bytes_of_pid(&bytes, 0x1011) + 1);
}

/// PR review finding: duplicate detection compared only the
/// continuity counter, so a non-conformant encoder that repeats a CC
/// WITHOUT repeating the packet's other bytes had its second PES
/// start silently swallowed — undercounting `pes_starts_per_pid` and
/// weakening `wire_vs_demux_*`. A same-CC packet whose payload
/// differs must count as its own PES start.
#[test]
fn reader_counts_a_same_cc_packet_with_different_bytes_as_a_new_pes_start() {
    let bytes = bytes_of("baseline", 3.0, "pes-starts-non-dup");
    let first = bytes
        .chunks_exact(PKT)
        .position(|c| {
            let pkt: [u8; PKT] = c.try_into().unwrap();
            let info = classify_packet(&pkt).unwrap();
            info.pid == 0x1011 && info.pusi
        })
        .expect("a video PES start");
    // Same CC as `first`, but flip a payload byte well past the PES
    // header so it stays a valid PES start with different content —
    // the "non-conformant encoder" case `pcr_masked_identical` must
    // reject as NOT a duplicate.
    let mut altered: [u8; PKT] = bytes[first * PKT..(first + 1) * PKT].try_into().unwrap();
    altered[100] ^= 0x01;
    let mut mutated = Vec::with_capacity(bytes.len() + PKT);
    mutated.extend_from_slice(&bytes[..(first + 1) * PKT]);
    mutated.extend_from_slice(&altered);
    mutated.extend_from_slice(&bytes[(first + 1) * PKT..]);
    let mut r = Reader::new();
    r.feed(&mutated).unwrap();
    let s = r.finish().unwrap();
    assert_eq!(
        s.pes_starts_per_pid[&0x1011], 91,
        "a same-CC packet with different bytes is not a duplicate: {s:?}"
    );
}

/// Packets on `pid` in `bytes` — the raw denominator
/// `reader_counts_pes_starts_per_pid_and_ignores_a_duplicate_packet`
/// checks the duplicated capture against.
fn bytes_of_pid(bytes: &[u8], pid: u16) -> u64 {
    bytes
        .chunks_exact(PKT)
        .filter(|c| classify_packet((*c).try_into().unwrap()).unwrap().pid == pid)
        .count() as u64
}

#[test]
fn resync_mode_hunts_forward_and_records_each_resync() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path =
        std::env::temp_dir().join(format!("tst-interop-rawts-hunt-{}.ts", std::process::id()));
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // Truncate packet 5 to 100 bytes and insert 37 garbage bytes after packet 20.
    let mut bad = Vec::new();
    for (i, pkt) in bytes.chunks_exact(PKT).enumerate() {
        if i == 5 {
            bad.extend_from_slice(&pkt[..100]);
        } else {
            bad.extend_from_slice(pkt);
        }
        if i == 20 {
            bad.extend(std::iter::repeat_n(0x55u8, 37));
        }
    }
    let mut r = Reader::new();
    r.set_resync_mode(true);
    for chunk in bad.chunks(1316) {
        r.feed(chunk).unwrap();
    }
    let got = r.take_resyncs();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].at_packets, 5);
    assert_eq!(got[0].skipped_bytes, 100);
    assert_eq!(got[1].at_packets, 20);
    assert_eq!(got[1].skipped_bytes, 37);
    assert_eq!(r.resync_count(), 2);
    // Every other packet was accepted.
    let s = r.finish().unwrap();
    assert_eq!(s.packets as usize, bytes.len() / PKT - 1);
}

#[test]
fn resync_mode_treats_a_trailing_partial_packet_as_a_resync_not_an_error() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path =
        std::env::temp_dir().join(format!("tst-interop-rawts-tail-{}.ts", std::process::id()));
    crate::r#gen::run(
        p,
        1.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut r = Reader::new();
    r.set_resync_mode(true);
    r.feed(&bytes[..3 * PKT + 50]).unwrap();
    let trailing = r.trailing_resync().expect("non-empty carry in resync mode");
    assert_eq!(trailing.at_packets, 3);
    assert_eq!(trailing.skipped_bytes, 50);
    let s = r.finish().unwrap();
    assert_eq!(s.packets, 3);
}

#[test]
fn resync_mode_records_a_malformed_packet_as_a_resync_and_continues() {
    let p = crate::profiles::by_name("baseline").unwrap();
    let path = std::env::temp_dir().join(format!(
        "tst-interop-rawts-malformed-{}.ts",
        std::process::id()
    ));
    crate::r#gen::run(
        p,
        2.0,
        &path,
        crate::fixtures::KlvSet::Compact,
        0,
        crate::fixtures::AuSizeMode::Compact,
    )
    .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut bad = bytes.clone();
    // Packet 7 (0-based): force the adaptation-field bit on and claim
    // an adaptation_field_length that overruns the 188-byte packet
    // (5 + 200 > 188) — malformed, but the sync byte is untouched, so
    // the resync-hunt's own byte/lookahead check accepts `off` as a
    // normal packet start and only the deeper `classify_packet` parse
    // catches it.
    bad[7 * PKT + 3] |= 0x20;
    bad[7 * PKT + 4] = 200;
    let mut r = Reader::new();
    r.set_resync_mode(true);
    for chunk in bad.chunks(1316) {
        r.feed(chunk).unwrap();
    }
    let got = r.take_resyncs();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].at_packets, 7);
    assert_eq!(got[0].skipped_bytes, PKT);
    let s = r.finish().unwrap();
    assert_eq!(s.packets as usize, bytes.len() / PKT - 1);

    let mut r = Reader::new();
    let e = r.feed(&bad).unwrap_err();
    assert!(e.contains("adaptation field length"), "{e}");
}

/// X-CORR-06A (E06): garbage with no sync candidate used to be kept
/// in the carry and rescanned on every feed — 10 000 x 1316 bytes of
/// 0x55 retained 13 160 000 bytes. Only the <= 187-byte suffix that
/// could still complete a packet may survive a feed, and the bytes
/// thrown away must still be counted in the recovery that follows.
#[test]
fn resync_mode_bounds_the_carry_on_confirmed_garbage_and_counts_it() {
    const CHUNKS: usize = 10_000;
    let garbage = [0x55u8; 1316];
    let mut r = Reader::with_retention(Retention::Bounded);
    r.set_resync_mode(true);
    for _ in 0..CHUNKS {
        r.feed(&garbage).unwrap();
        assert!(
            r.carry().len() < PKT,
            "carry holds {} bytes; only a partial-packet suffix may survive a feed",
            r.carry().len()
        );
    }
    // Now three clean packets: the hunt confirms the first and the
    // recovery names every garbage byte ever fed.
    let clean = bytes_of("baseline", 1.0, "carry-bound");
    r.feed(&clean[..3 * PKT]).unwrap();
    let got = r.take_resyncs();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].at_packets, 0);
    assert_eq!(got[0].skipped_bytes, CHUNKS * 1316);
    assert_eq!(r.packets(), 3);
    assert!(r.carry().is_empty());
}

/// The retained suffix may itself BEGIN at a real packet start that
/// only a later feed can confirm — the hunt never runs for it, so the
/// recovery has to be recorded where the packet is accepted. Feeding
/// exactly `n` garbage bytes plus the packet's first 187 puts the
/// boundary at `carry.len() - 187` on the nose.
#[test]
fn garbage_dropped_before_a_suffix_that_turns_out_to_be_a_packet_is_still_recorded() {
    let clean = bytes_of("baseline", 1.0, "suffix-start");
    let mut r = Reader::new();
    r.set_resync_mode(true);
    let mut first = vec![0x55u8; 1000];
    first.extend_from_slice(&clean[..PKT - 1]);
    r.feed(&first).unwrap();
    assert_eq!(
        r.carry().len(),
        PKT - 1,
        "the clean prefix is what survived"
    );
    assert_eq!(r.carry()[0], SYNC, "…and it starts at a packet boundary");
    assert_eq!(r.resync_count(), 0, "nothing is confirmed yet");

    r.feed(&clean[PKT - 1..]).unwrap();
    let got = r.take_resyncs();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].at_packets, 0);
    assert_eq!(got[0].skipped_bytes, 1000);
    assert_eq!(r.packets() as usize, clean.len() / PKT);
}

/// SIMP-11(a) / X-CORR-06B: recoveries are DRAINED per batch, like
/// PCR events, so a multi-day capture never holds (or re-copies) its
/// whole recovery history. Total events still equals the number of
/// recoveries; the lifetime count survives the drains.
#[test]
fn resyncs_are_drained_in_batches_and_counted_for_life() {
    let bytes = bytes_of("baseline", 3.0, "drain");
    // Truncate every 50th packet to 100 bytes: one recovery each.
    let mut bad = Vec::with_capacity(bytes.len());
    let mut expected = 0u64;
    for (i, pkt) in bytes.chunks_exact(PKT).enumerate() {
        if i % 50 == 49 {
            bad.extend_from_slice(&pkt[..100]);
            expected += 1;
        } else {
            bad.extend_from_slice(pkt);
        }
    }
    let mut r = Reader::with_retention(Retention::Bounded);
    r.set_resync_mode(true);
    let mut total = 0u64;
    for chunk in bad.chunks(1316) {
        r.feed(chunk).unwrap();
        let batch = r.take_resyncs();
        total += batch.len() as u64;
        // A 1316-byte feed spans 7 packets: at most one recovery.
        assert!(batch.len() <= 1, "{batch:?}");
    }
    assert_eq!(total, expected);
    assert_eq!(r.resync_count(), expected);
    assert!(r.take_resyncs().is_empty(), "already drained");
}

/// The trailing recovery counts discarded garbage too.
#[test]
fn trailing_resync_includes_garbage_already_discarded() {
    let mut r = Reader::new();
    r.set_resync_mode(true);
    r.feed(&[0x55u8; 1316]).unwrap();
    r.feed(&[0x55u8; 100]).unwrap();
    let t = r
        .trailing_resync()
        .expect("garbage is a trailing fragment in resync mode");
    assert_eq!(t.skipped_bytes, 1416);
    assert!(r.carry().len() < PKT);
}

/// A later audio PES whose first payload bytes were rewritten is counted
/// in the summary always, and reported with its packet ordinal only in
/// resync mode (the mode a corruption-judged capture runs in).
#[test]
fn a_later_pes_prefix_mismatch_is_reported_with_its_packet_ordinal_in_resync_mode() {
    let mut bytes = bytes_of("audio", 2.0, "prefix-events");
    let inv = crate::profiles::invariants(crate::profiles::by_name("audio").unwrap());
    let pid = inv.programs[0]
        .audio_pid
        .expect("audio profile has an audio PID");
    let starts: Vec<usize> = bytes
        .chunks_exact(PKT)
        .enumerate()
        .filter_map(|(i, c)| {
            let pkt: [u8; PKT] = c.try_into().expect("PKT bytes");
            let info = classify_packet(&pkt).expect("generated packets classify");
            (info.pid == pid && info.pusi).then_some(i)
        })
        .collect();
    assert!(starts.len() >= 3, "the capture holds several audio PES");
    let victim = starts[2];
    let off = victim * PKT;
    let info = classify_packet(bytes[off..off + PKT].try_into().unwrap()).unwrap();
    let hdr_len = usize::from(bytes[off + info.payload_off + 8]);
    let at = off + info.payload_off + 9 + hdr_len;
    bytes[at] = 0x00;
    bytes[at + 1] = 0x00;

    let mut plain = Reader::new();
    plain.feed(&bytes).unwrap();
    assert!(plain.take_prefix_mismatches().is_empty());
    assert_eq!(plain.finish().unwrap().pes[&pid].prefix_mismatches, 1);

    let mut r = Reader::new();
    r.set_resync_mode(true);
    for chunk in bytes.chunks(101) {
        r.feed(chunk).unwrap();
    }
    assert_eq!(r.take_prefix_mismatches(), vec![(pid, victim as u64)]);
    assert!(r.take_prefix_mismatches().is_empty(), "drained");
}

/// `bytes` read whole by a fresh [`Reader`].
pub(crate) fn summarize(bytes: &[u8]) -> WireSummary {
    let mut r = Reader::new();
    r.feed(bytes).unwrap();
    r.finish().unwrap()
}

/// One PSI section (H.222.0 §2.4.4): the 8-byte long-form header, `body`,
/// and the CRC-32.
fn psi_section(table_id: u8, ext: u16, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4;
    let mut s = vec![
        table_id,
        0xB0 | (len >> 8) as u8,
        len as u8,
        (ext >> 8) as u8,
        ext as u8,
        0xC1,
        0,
        0,
    ];
    s.extend_from_slice(body);
    let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

/// A 188-byte payload-only packet, `0xFF`-padded.
fn payload_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![
        SYNC,
        u8::from(pusi) << 6 | (pid >> 8) as u8,
        pid as u8,
        0x10,
    ];
    p.extend_from_slice(payload);
    p.resize(PKT, 0xFF);
    p
}

/// A 33-bit PTS or DTS field (H.222.0 Table 2-21) under its 4-bit prefix.
fn timestamp_field(prefix: u8, ts: u64) -> [u8; 5] {
    [
        prefix << 4 | ((ts >> 29) & 0x0E) as u8 | 1,
        (ts >> 22) as u8,
        ((ts >> 14) & 0xFE) as u8 | 1,
        (ts >> 7) as u8,
        ((ts << 1) & 0xFE) as u8 | 1,
    ]
}

/// PAT, PMT (program 1, PCR on `pcr_pid`, listing `pcr_pid` and
/// `pes_pid`), an adaptation-field-only packet carrying PCR base
/// `pcr_base` on `pcr_pid` (Table 2-6), then a PES start on `pes_pid`
/// with `pts`, and `dts` when given (Table 2-21).
pub(crate) fn pcr_then_pes_on(
    pcr_pid: u16,
    pes_pid: u16,
    pcr_base: u64,
    dts: Option<u64>,
    pts: u64,
) -> Vec<u8> {
    const PMT_PID: u16 = 0x1000;
    let mut out = Vec::with_capacity(4 * PKT);

    let pat = psi_section(0x00, 1, &[0, 1, 0xE0 | (PMT_PID >> 8) as u8, PMT_PID as u8]);
    out.extend(payload_packet(0, true, &[&[0u8][..], &pat].concat()));

    let mut pmt_body = vec![0xE0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xF0, 0];
    let mut streams = vec![(0x1B, pcr_pid)];
    if pes_pid != pcr_pid {
        streams.push((0x06, pes_pid));
    }
    for (stream_type, pid) in streams {
        pmt_body.extend([stream_type, 0xE0 | (pid >> 8) as u8, pid as u8, 0xF0, 0]);
    }
    let pmt = psi_section(0x02, 1, &pmt_body);
    out.extend(payload_packet(PMT_PID, true, &[&[0u8][..], &pmt].concat()));

    let mut pcr = vec![SYNC, (pcr_pid >> 8) as u8, pcr_pid as u8, 0x20, 183, 0x10];
    pcr.extend([
        (pcr_base >> 25) as u8,
        (pcr_base >> 17) as u8,
        (pcr_base >> 9) as u8,
        (pcr_base >> 1) as u8,
        ((pcr_base & 1) as u8) << 7 | 0x7E,
        0,
    ]);
    pcr.resize(PKT, 0xFF);
    out.extend(pcr);

    let mut pes = vec![0, 0, 1, 0xE0, 0, 0, 0x80];
    match dts {
        Some(dts) => {
            pes.extend([0xC0, 10]);
            pes.extend(timestamp_field(0b0011, pts));
            pes.extend(timestamp_field(0b0001, dts));
        }
        None => {
            pes.extend([0x80, 5]);
            pes.extend(timestamp_field(0b0010, pts));
        }
    }
    out.extend(payload_packet(pes_pid, true, &pes));
    out
}

pub(crate) fn pcr_then_pes(pid: u16, pcr_base: u64, dts: Option<u64>, pts: u64) -> Vec<u8> {
    pcr_then_pes_on(pid, pid, pcr_base, dts, pts)
}

/// H.222.0 §2.4.2: a decoder may start decoding an AU only once the STC
/// reaches its DTS, so the PCR that precedes the AU must be below it.
#[test]
fn the_reader_measures_the_lead_of_each_pes_over_the_last_pcr() {
    // packets: PCR base 90_000 on 0x100, then a PES on 0x100 with DTS 99_000
    let w = summarize(&pcr_then_pes(0x100, 90_000, Some(99_000), 108_000));
    assert_eq!(w.pcr_lead_min_ticks[&0x100], 9_000); // 100 ms
}
