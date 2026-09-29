//! Lifetime of a PID's PCR history across topology changes.
//!
//! The history a `PcrAnomaly` is judged against lives exactly as long as at
//! least one program declares the PID as its `PCR_PID`:
//!
//! - Retired when the last declaration goes. PMT v0 declares PCR_PID =
//!   elementary PID 0x1011 and a PCR is seen. PMT v1 moves PCR_PID to 0x0200
//!   while 0x1011 stays on as a plain elementary stream and carries NO PCR.
//!   PMT v2 moves PCR_PID back to 0x1011. The first PCR after v2 opens a new
//!   declaration epoch: it is a seed, not a jump against the baseline
//!   recorded under v0.
//! - Kept while a declaration remains. A PCR PID shared by two programs
//!   keeps its history when only ONE of them moves its clock elsewhere or
//!   leaves the PAT, and a PID dropped from its program's stream list keeps
//!   it while the program still names it as `PCR_PID`.

use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, StrictMode};

use crate::psi_builders::{build_pat_section, build_pmt_section, psi_packet};

const PMT_PID: u16 = 0x1000;
const ES_PID: u16 = 0x1011;
const DEDICATED_PCR_PID: u16 = 0x0200;

fn pat(programs: &[(u16, u16)]) -> Vec<u8> {
    pat_version(programs, 0, 0)
}

/// One PAT packet. `cc` must advance per packet on the PAT PID.
fn pat_version(programs: &[(u16, u16)], version: u8, cc: u8) -> Vec<u8> {
    psi_packet(0x0000, &build_pat_section(version, programs), cc)
}

/// One PMT packet. `cc` must advance per packet on the same PMT PID.
fn pmt(pmt_pid: u16, program: u16, pcr_pid: u16, es_pid: u16, version: u8, cc: u8) -> Vec<u8> {
    psi_packet(
        pmt_pid,
        &build_pmt_section(program, pcr_pid, version, &[(0x1B, es_pid, &[][..])]),
        cc,
    )
}

/// Adaptation-field-only packet carrying a PCR (ISO/IEC 13818-1 §2.4.3.4).
fn pcr_packet(pid: u16, pcr_27mhz: u64) -> [u8; 188] {
    let base: u64 = pcr_27mhz / 300;
    let ext: u64 = pcr_27mhz % 300;
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47;
    buf[1] = (pid >> 8) as u8 & 0x1F;
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x20; // adaptation only, CC=0
    buf[4] = 183;
    buf[5] = 0x10; // PCR_flag
    buf[6] = (base >> 25) as u8;
    buf[7] = (base >> 17) as u8;
    buf[8] = (base >> 9) as u8;
    buf[9] = (base >> 1) as u8;
    buf[10] = (((base & 0x01) as u8) << 7) | 0x7E | ((ext >> 8) as u8 & 0x01);
    buf[11] = (ext & 0xFF) as u8;
    buf
}

fn drain(d: &mut Demuxer) -> Vec<DemuxEvent> {
    core::iter::from_fn(|| d.next_event()).collect()
}

fn program_maps(events: &[DemuxEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, DemuxEvent::ProgramMap(_)))
        .count()
}

fn pcr_anomalies(events: &[DemuxEvent]) -> Vec<&DemuxEvent> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DemuxEvent::NonConformant {
                    issue: NonConformantIssue::PcrAnomaly { .. },
                    ..
                }
            )
        })
        .collect()
}

/// Feed PAT, PMT v0 (PCR on the ES PID), a PCR seed, PMT v1 (PCR moved to the
/// dedicated PID, ES stays, no PCR on it), PMT v2 (PCR back on the ES PID).
/// Asserts every PMT was accepted so the test cannot pass vacuously.
fn demote_then_repromote(d: &mut Demuxer) {
    d.feed(&pat(&[(1, PMT_PID)])).unwrap();
    d.feed(&pmt(PMT_PID, 1, ES_PID, ES_PID, 0, 0)).unwrap();
    d.feed(&pcr_packet(ES_PID, 0)).unwrap(); // v0 baseline
    d.feed(&pmt(PMT_PID, 1, DEDICATED_PCR_PID, ES_PID, 1, 1))
        .unwrap();
    // The demoted ES PID carries NO PCR here: the retirement must come from
    // the PMT update itself, not from a packet arriving on the PID.
    d.feed(&pmt(PMT_PID, 1, ES_PID, ES_PID, 2, 2)).unwrap();
    let events = drain(d);
    assert_eq!(
        program_maps(&events),
        3,
        "precondition: PMT v0, v1 and v2 were all accepted: {events:?}"
    );
    assert!(
        pcr_anomalies(&events).is_empty(),
        "precondition: no anomaly so far: {events:?}"
    );
}

#[test]
fn pcr_repromotion_starts_new_baseline() {
    let mut d = Demuxer::new();
    demote_then_repromote(&mut d);

    // 4 s @ 27 MHz past the v0 baseline: legitimate for a new epoch.
    d.feed(&pcr_packet(ES_PID, 108_000_000)).unwrap();
    let events = drain(&mut d);
    let anomalies = pcr_anomalies(&events);
    assert!(
        anomalies.is_empty(),
        "the first PCR after re-promotion was judged against the baseline of \
         the retired declaration: {anomalies:?}"
    );
}

#[test]
fn pcr_repromotion_does_not_reject_under_timing_only() {
    let mut d = Demuxer::with_config(
        DemuxerConfig::builder()
            .strict(StrictMode::TimingOnly)
            .build(),
    );
    demote_then_repromote(&mut d);

    let r = d.feed(&pcr_packet(ES_PID, 108_000_000));
    assert!(
        !matches!(r, Err(DemuxError::StrictRejection(_))),
        "a valid demote/re-promote topology ended a TimingOnly session: {r:?}"
    );
    assert!(r.is_ok(), "got {r:?}");
}

/// Control: once re-promoted, the new epoch is a normal timeline — a real
/// jump on it still fires.
#[test]
fn pcr_jump_after_repromotion_seed_still_fires() {
    let mut d = Demuxer::new();
    demote_then_repromote(&mut d);
    d.feed(&pcr_packet(ES_PID, 108_000_000)).unwrap();
    let _ = drain(&mut d);
    d.feed(&pcr_packet(ES_PID, 108_000_000 + 54_000_000))
        .unwrap();
    let events = drain(&mut d);
    assert_eq!(
        pcr_anomalies(&events).len(),
        1,
        "a 2 s jump inside the new epoch must be reported: {events:?}"
    );
}

/// Control: two programs share one dedicated PCR PID. Program 1 moves its
/// clock onto its own ES PID; program 2 still declares the shared PID, so the
/// shared PID's history must survive and a jump on it must still be reported.
#[test]
fn shared_pcr_pid_keeps_history_when_one_program_moves_away() {
    const PMT2_PID: u16 = 0x1100;
    const ES2_PID: u16 = 0x1111;

    let mut d = Demuxer::new();
    d.feed(&pat(&[(1, PMT_PID), (2, PMT2_PID)])).unwrap();
    d.feed(&pmt(PMT_PID, 1, DEDICATED_PCR_PID, ES_PID, 0, 0))
        .unwrap();
    d.feed(&pmt(PMT2_PID, 2, DEDICATED_PCR_PID, ES2_PID, 0, 0))
        .unwrap();
    d.feed(&pcr_packet(DEDICATED_PCR_PID, 0)).unwrap(); // shared baseline
    // Program 1 only: PCR_PID moves to its ES PID.
    d.feed(&pmt(PMT_PID, 1, ES_PID, ES_PID, 1, 1)).unwrap();
    let events = drain(&mut d);
    assert_eq!(
        program_maps(&events),
        3,
        "precondition: all three PMTs were accepted: {events:?}"
    );
    assert!(pcr_anomalies(&events).is_empty(), "{events:?}");

    // Program 2's clock (still on the shared PID) jumps 2 s.
    d.feed(&pcr_packet(DEDICATED_PCR_PID, 54_000_000)).unwrap();
    let events = drain(&mut d);
    assert_eq!(
        pcr_anomalies(&events).len(),
        1,
        "program 2 still declares the shared PCR PID: its history must be \
         kept and the 2 s jump reported: {events:?}"
    );
}

/// Same topology with a smooth clock: the shared PID is left by one program
/// and the OTHER program's timeline continues — no anomaly may be invented.
#[test]
fn shared_pcr_pid_smooth_clock_is_quiet_when_one_program_moves_away() {
    const PMT2_PID: u16 = 0x1100;
    const ES2_PID: u16 = 0x1111;

    let mut d = Demuxer::new();
    d.feed(&pat(&[(1, PMT_PID), (2, PMT2_PID)])).unwrap();
    d.feed(&pmt(PMT_PID, 1, DEDICATED_PCR_PID, ES_PID, 0, 0))
        .unwrap();
    d.feed(&pmt(PMT2_PID, 2, DEDICATED_PCR_PID, ES2_PID, 0, 0))
        .unwrap();
    d.feed(&pcr_packet(DEDICATED_PCR_PID, 0)).unwrap();
    d.feed(&pmt(PMT_PID, 1, ES_PID, ES_PID, 1, 1)).unwrap();
    d.feed(&pcr_packet(DEDICATED_PCR_PID, 2_700_000)).unwrap(); // +100 ms
    d.feed(&pcr_packet(ES_PID, 900_000_000)).unwrap(); // program 1's new clock: a seed
    let events = drain(&mut d);
    assert!(pcr_anomalies(&events).is_empty(), "{events:?}");
}

/// Two programs share one dedicated PCR PID and a PAT change drops one of
/// them. The surviving program still declares the PID, so its history must
/// survive the removal.
#[test]
fn shared_pcr_pid_keeps_history_when_one_program_leaves_the_pat() {
    const PMT2_PID: u16 = 0x1100;
    const ES2_PID: u16 = 0x1111;

    let mut d = Demuxer::new();
    d.feed(&pat(&[(1, PMT_PID), (2, PMT2_PID)])).unwrap();
    d.feed(&pmt(PMT_PID, 1, DEDICATED_PCR_PID, ES_PID, 0, 0))
        .unwrap();
    d.feed(&pmt(PMT2_PID, 2, DEDICATED_PCR_PID, ES2_PID, 0, 0))
        .unwrap();
    d.feed(&pcr_packet(DEDICATED_PCR_PID, 0)).unwrap(); // shared baseline
    // PAT v1 lists program 2 only.
    d.feed(&pat_version(&[(2, PMT2_PID)], 1, 1)).unwrap();
    let events = drain(&mut d);
    assert_eq!(
        program_maps(&events),
        2,
        "precondition: both PMTs were accepted: {events:?}"
    );
    // Precondition: the PAT change took effect — program 1's PMT PID is no
    // longer a PMT PID, so a new PMT version on it is not adopted.
    d.feed(&pmt(PMT_PID, 1, ES_PID, ES_PID, 1, 1)).unwrap();
    let events = drain(&mut d);
    assert_eq!(
        program_maps(&events),
        0,
        "precondition: program 1 left with the PAT change: {events:?}"
    );

    d.feed(&pcr_packet(DEDICATED_PCR_PID, 54_000_000)).unwrap();
    let events = drain(&mut d);
    assert_eq!(
        pcr_anomalies(&events).len(),
        1,
        "program 2 still declares the shared PCR PID: its history must be \
         kept and the 2 s jump reported: {events:?}"
    );
}

/// A PMT update drops the elementary stream on the PCR PID from the stream
/// list but keeps the PID as `PCR_PID` (it becomes a dedicated PCR PID). The
/// declaration never lapsed, so the history is kept.
#[test]
fn pcr_pid_keeps_history_when_its_elementary_stream_is_dropped() {
    const OTHER_ES_PID: u16 = 0x1012;

    let mut d = Demuxer::new();
    d.feed(&pat(&[(1, PMT_PID)])).unwrap();
    d.feed(&pmt(PMT_PID, 1, ES_PID, ES_PID, 0, 0)).unwrap();
    d.feed(&pcr_packet(ES_PID, 0)).unwrap();
    d.feed(&pmt(PMT_PID, 1, ES_PID, OTHER_ES_PID, 1, 1))
        .unwrap();
    let events = drain(&mut d);
    assert_eq!(
        program_maps(&events),
        2,
        "precondition: PMT v0 and v1 were both accepted: {events:?}"
    );

    d.feed(&pcr_packet(ES_PID, 54_000_000)).unwrap();
    let events = drain(&mut d);
    assert_eq!(
        pcr_anomalies(&events).len(),
        1,
        "the PID is still the declared PCR_PID: the 2 s jump must be \
         reported: {events:?}"
    );
}
