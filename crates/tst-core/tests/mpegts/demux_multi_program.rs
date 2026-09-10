//! Integration tests for multi-program TS demuxing.
//!
//! White-box PAT/PMT tracker tests (programs_for_test) live in
//! crates/tst-core/src/mpegts/demux/demuxer.rs #[cfg(test)] mod tests.

use tst_core::mpegts::demux::Demuxer;

use crate::psi_builders::{build_pat_section, build_pmt_section, psi_packet};

// Section and packet bytes come from the shared `psi_builders`; these
// adapters keep this file's call shape (PAT: programs then version; PMT:
// pid, program_number, pcr_pid, streams, version) and its CC=0 default.

/// One PAT packet declaring `programs` as `(program_number, pmt_pid)`.
fn pat_packet_with_programs(programs: &[(u16, u16)], version: u8) -> Vec<u8> {
    psi_packet(0x0000, &build_pat_section(version, programs), 0)
}

/// Like `pat_packet_with_programs` but with a custom continuity counter.
///
/// Use this when a test feeds two PAT packets on the same PID: the second
/// must use `cc=1` so DA-DEMUX-1's duplicate-suppression does not swallow it.
fn pat_packet_with_programs_cc(programs: &[(u16, u16)], version: u8, cc: u8) -> Vec<u8> {
    psi_packet(0x0000, &build_pat_section(version, programs), cc)
}

/// One PMT packet for `program_number` on `pmt_pid`; `streams` is
/// `(stream_type, elementary_pid)`, no descriptors.
fn pmt_packet_for_test(
    pmt_pid: u16,
    program_number: u16,
    pcr_pid: u16,
    streams: &[(u8, u16)],
    version: u8,
) -> Vec<u8> {
    let streams: Vec<(u8, u16, &[u8])> = streams
        .iter()
        .map(|&(st, pid)| (st, pid, &[][..]))
        .collect();
    psi_packet(
        pmt_pid,
        &build_pmt_section(program_number, pcr_pid, version, &streams),
        0,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn program_map_event_fires_per_program() {
    use tst_core::mpegts::demux::DemuxEvent;
    let mut demuxer = Demuxer::new();
    demuxer
        .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
        .unwrap();
    demuxer
        .feed(&pmt_packet_for_test(
            0x1000,
            1,
            0x1011,
            &[(0x1B, 0x1011)],
            0,
        ))
        .unwrap();
    demuxer
        .feed(&pmt_packet_for_test(
            0x1100,
            2,
            0x1111,
            &[(0x24, 0x1111)],
            0,
        ))
        .unwrap();

    let mut events = Vec::new();
    while let Some(ev) = demuxer.next_event() {
        events.push(ev);
    }

    let prog_maps: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .collect();
    assert_eq!(
        prog_maps.len(),
        2,
        "expected 2 ProgramMap events, got {prog_maps:?}"
    );
    assert!(prog_maps.iter().any(|pm| pm.program_number == 1));
    assert!(prog_maps.iter().any(|pm| pm.program_number == 2));
}

#[test]
fn program_map_carries_pmt_pid_from_pat() {
    // Verify that ProgramMap.pmt_pid is set from the PAT entry that declared
    // the program, not from any stream PID or PCR PID inside the PMT.
    use tst_core::mpegts::demux::DemuxEvent;
    let mut demuxer = Demuxer::new();
    demuxer
        .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
        .unwrap();
    demuxer
        .feed(&pmt_packet_for_test(
            0x1000,
            1,
            0x1011,
            &[(0x1B, 0x1011)],
            0,
        ))
        .unwrap();
    demuxer
        .feed(&pmt_packet_for_test(
            0x1100,
            2,
            0x1111,
            &[(0x24, 0x1111)],
            0,
        ))
        .unwrap();

    let prog_maps: Vec<_> = core::iter::from_fn(|| demuxer.next_event())
        .filter_map(|e| match e {
            DemuxEvent::ProgramMap(pm) => Some(pm),
            _ => None,
        })
        .collect();
    assert_eq!(prog_maps.len(), 2);
    let pm1 = prog_maps.iter().find(|pm| pm.program_number == 1).unwrap();
    let pm2 = prog_maps.iter().find(|pm| pm.program_number == 2).unwrap();
    assert_eq!(
        pm1.pmt_pid, 0x1000,
        "program 1 pmt_pid must match PAT entry"
    );
    assert_eq!(
        pm2.pmt_pid, 0x1100,
        "program 2 pmt_pid must match PAT entry"
    );
}

// ── Stats tests ───────────────────────────────────────────────────────────────

#[test]
fn demuxer_stats_programs_seen_reflects_pat_size() {
    let mut demuxer = Demuxer::new();
    assert_eq!(
        demuxer.stats().programs_seen,
        0,
        "no PAT received yet — programs_seen must be 0"
    );

    // Feed a PAT with two programs.
    demuxer
        .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
        .unwrap();
    assert_eq!(
        demuxer.stats().programs_seen,
        2,
        "after 2-program PAT, programs_seen must be 2"
    );

    // Feed a new PAT version that drops program 2 (CC=1 to avoid duplicate suppression).
    demuxer
        .feed(&pat_packet_with_programs_cc(&[(1, 0x1000)], 1, 1))
        .unwrap();
    assert_eq!(
        demuxer.stats().programs_seen,
        1,
        "after dropping program 2, programs_seen must be 1"
    );
}

// ---------------------------------------------------------------------------
// Multi-program PCR tracking (validate-1 B1 — Codex TS-TIME-01)
// ---------------------------------------------------------------------------

/// Build a 188-byte TS packet carrying a PCR in its adaptation field on
/// the given PID. `pcr_27mhz` is encoded per ISO/IEC 13818-1 §2.4.3.4:
/// 33-bit base (pcr/300) || 6 reserved 1-bits || 9-bit ext (pcr%300).
/// CC defaults to 0; no payload. This is the minimum shape `parse_ts_packet`
/// needs to populate `TsPacket::pcr_27mhz`.
fn pcr_packet(pid: u16, pcr_27mhz: u64) -> [u8; 188] {
    let base: u64 = pcr_27mhz / 300;
    let ext: u64 = pcr_27mhz % 300;
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47; // sync
    buf[1] = (pid >> 8) as u8 & 0x1F; // no PUSI, no TEI
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x20; // adaptation_field_control = 0b10 (adaptation only), CC=0
    buf[4] = 183; // af_length: fills the rest of the packet
    buf[5] = 0x10; // flags: PCR_flag=1 (bit 4), others 0
    buf[6] = (base >> 25) as u8;
    buf[7] = (base >> 17) as u8;
    buf[8] = (base >> 9) as u8;
    buf[9] = (base >> 1) as u8;
    buf[10] = (((base & 0x01) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01);
    buf[11] = (ext & 0xFF) as u8;
    // Bytes 12..188 are stuffing (0xFF), which matches the rest of the AF.
    buf
}

/// Two programs with independent PCR PIDs must not share a PCR comparison
/// slot. Before the fix, `Demuxer::last_pcr_27mhz` was a single global
/// `Option<u64>` and `check_pcr` compared any incoming PCR against it
/// regardless of PID; a >1s gap between program 1's PCR (PID 0x1011) and
/// program 2's PCR (PID 0x1111) produced a spurious `PcrAnomaly`. Per
/// ITU-T H.222.0 §2.4.3.5 each program has its own time base, so this is
/// a false positive.
#[test]
fn pcr_anomaly_does_not_fire_across_programs() {
    use tst_core::mpegts::demux::{DemuxEvent, NonConformantIssue};
    let mut demuxer = Demuxer::new();
    demuxer
        .feed(&pat_packet_with_programs(&[(1, 0x1000), (2, 0x1100)], 0))
        .unwrap();
    // Two programs each have a video ES that doubles as the PCR PID.
    demuxer
        .feed(&pmt_packet_for_test(
            0x1000,
            1,
            0x1011,
            &[(0x1B, 0x1011)],
            0,
        ))
        .unwrap();
    demuxer
        .feed(&pmt_packet_for_test(
            0x1100,
            2,
            0x1111,
            &[(0x1B, 0x1111)],
            0,
        ))
        .unwrap();

    // PCR_ANOMALY_THRESHOLD is 27_000_000 (1s @ 27 MHz). 1_000_000_000 ticks
    // (~37s) of separation between the two programs' bases must NOT trigger
    // an anomaly because each PID's time base is independent.
    demuxer.feed(&pcr_packet(0x1011, 1_000_000_000)).unwrap();
    demuxer.feed(&pcr_packet(0x1111, 2_000_000_000)).unwrap();

    let pcr_anomalies: Vec<_> = std::iter::from_fn(|| demuxer.next_event())
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { .. },
                    ..
                }
            )
        })
        .collect();
    assert!(
        pcr_anomalies.is_empty(),
        "cross-program PCRs must not produce PcrAnomaly events, got {pcr_anomalies:?}"
    );
}

/// Same setup, but the second PCR on PID 0x1011 jumps >1s relative to the
/// first PCR on PID 0x1011 — that IS a real anomaly on a single program's
/// own time base and must still fire. Regression-guards against the fix
/// over-correcting by suppressing all PCR anomalies.
#[test]
fn pcr_anomaly_fires_for_same_pid_jump() {
    use tst_core::mpegts::demux::{DemuxEvent, NonConformantIssue};
    let mut demuxer = Demuxer::new();
    demuxer
        .feed(&pat_packet_with_programs(&[(1, 0x1000)], 0))
        .unwrap();
    demuxer
        .feed(&pmt_packet_for_test(
            0x1000,
            1,
            0x1011,
            &[(0x1B, 0x1011)],
            0,
        ))
        .unwrap();

    // Two PCRs on the SAME PID, 28_000_000 ticks (~1.037s) apart — over the
    // 27_000_000 threshold, so this MUST emit PcrAnomaly.
    demuxer.feed(&pcr_packet(0x1011, 1_000_000_000)).unwrap();
    demuxer
        .feed(&pcr_packet(0x1011, 1_000_000_000 + 28_000_000))
        .unwrap();

    let pcr_anomalies: Vec<_> = std::iter::from_fn(|| demuxer.next_event())
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        pcr_anomalies.len(),
        1,
        "same-PID PCR jump of 28 MHz must emit exactly one PcrAnomaly, got {pcr_anomalies:?}"
    );
}

// White-box B8 (PAT cleanup) tests live in the demuxer.rs #[cfg(test)] mod
// (alongside the existing PAT/PMT tracker tests that use `programs_for_test`).
// See `pat_remove_program_clears_per_pid_state` etc. there.
