//! PMT body `program_number` must match PAT assignment.
//!
//! A validly-checksummed PMT whose body carries `program_number = M` that
//! arrives on a PMT PID whose PAT entry declared `program_number = N` (N ≠ M)
//! must be rejected: the demuxer emits a
//! `NonConformantIssue::PmtProgramNumberMismatch` event and DOES NOT adopt the
//! bogus topology (no `ProgramMap` event, no video `Sample` events from the
//! mislabeled program's streams).
//!
//! `StrictMode::Full` additionally converts the mismatch into a fatal
//! `DemuxError::StrictRejection`.

use crate::psi_builders::{self, pack_section};
use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig, NonConformantIssue, StrictMode};

// ─────────────────────────────────────────────────────────────────────────────
// Section builders (shared `psi_builders`)
// ─────────────────────────────────────────────────────────────────────────────

/// A valid-CRC PAT section declaring `program_number` on PMT PID `pmt_pid`.
fn build_pat_section(program_number: u16, pmt_pid: u16) -> Vec<u8> {
    psi_builders::build_pat_section(0, &[(program_number, pmt_pid)])
}

/// A valid-CRC PMT section with a single H.264 (0x1B) elementary stream.
///
/// `program_number` goes into the PMT body's `program_number` field — caller
/// sets this to a value that may differ from the PAT's declared number to
/// trigger the mismatch.
fn build_pmt_section(program_number: u16, pcr_pid: u16, video_pid: u16) -> Vec<u8> {
    psi_builders::build_pmt_section(program_number, pcr_pid, 0, &[(0x1B, video_pid, &[])])
}

/// Drain all queued events; return `(nonconformant_issues, program_map_count)`.
fn drain_events(demuxer: &mut Demuxer) -> (Vec<NonConformantIssue>, usize) {
    let mut issues = Vec::new();
    let mut maps = 0usize;
    while let Some(ev) = demuxer.next_event() {
        match ev {
            DemuxEvent::NonConformant { issue, .. } => issues.push(issue),
            DemuxEvent::ProgramMap(_) => maps += 1,
            _ => {}
        }
    }
    (issues, maps)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

const PMT_PID: u16 = 0x0100;
const PCR_PID: u16 = 0x0200;
const VIDEO_PID: u16 = 0x0201;
const PAT_PROGRAM: u16 = 1;
const PMT_PROGRAM: u16 = 99; // deliberately ≠ PAT_PROGRAM

/// Build the raw bytes for a PAT declaring program 1 on PMT_PID, followed by
/// a PMT on PMT_PID whose body claims program 99 (≠ 1).
fn build_mismatched_stream() -> Vec<u8> {
    let pat_sec = build_pat_section(PAT_PROGRAM, PMT_PID);
    let pmt_sec = build_pmt_section(PMT_PROGRAM, PCR_PID, VIDEO_PID);

    let mut buf = Vec::new();
    for pkt in pack_section(0x0000, &pat_sec, 0) {
        buf.extend_from_slice(&pkt);
    }
    for pkt in pack_section(PMT_PID, &pmt_sec, 0) {
        buf.extend_from_slice(&pkt);
    }
    buf
}

/// Lenient (default StrictMode::Off):
///   - a `PmtProgramNumberMismatch` event is emitted,
///   - NO `ProgramMap` event is emitted (topology NOT adopted),
///   - the video PID is not registered (no Sample events possible).
#[test]
fn pmt_program_number_mismatch_emits_nonconformant_event() {
    let bytes = build_mismatched_stream();
    let mut d = Demuxer::new();
    d.feed(&bytes).unwrap();
    let (issues, map_count) = drain_events(&mut d);

    // Exactly one PmtProgramNumberMismatch issue.
    let mismatch_issues: Vec<_> = issues
        .iter()
        .filter(|i| matches!(i, NonConformantIssue::PmtProgramNumberMismatch { .. }))
        .collect();
    assert_eq!(
        mismatch_issues.len(),
        1,
        "expected exactly one PmtProgramNumberMismatch, got issues: {issues:?}"
    );

    // The mismatch event must carry the right pids/programs.
    if let NonConformantIssue::PmtProgramNumberMismatch {
        pid,
        pat_program,
        pmt_program,
    } = mismatch_issues[0]
    {
        assert_eq!(*pid, PMT_PID, "pid must be the PMT PID");
        assert_eq!(
            *pat_program, PAT_PROGRAM,
            "pat_program must match the PAT entry"
        );
        assert_eq!(
            *pmt_program, PMT_PROGRAM,
            "pmt_program must be the PMT body value"
        );
    }

    // Topology must NOT be adopted under the false identity.
    assert_eq!(
        map_count, 0,
        "no ProgramMap must be emitted for a mismatched PMT"
    );
}

/// Display: the issue formats to a string that mentions the PID
/// and both program numbers.
#[test]
fn pmt_program_number_mismatch_display() {
    let issue = NonConformantIssue::PmtProgramNumberMismatch {
        pid: 0x0100,
        pat_program: 1,
        pmt_program: 99,
    };
    let s = format!("{issue}");
    assert!(s.contains("0x0100"), "Display must mention the PID: {s}");
    assert!(
        s.contains("program_number=1"),
        "Display must mention pat_program: {s}"
    );
    assert!(
        s.contains("program_number=99"),
        "Display must mention pmt_program: {s}"
    );
}

/// Strict rejection: StrictMode::Full converts the mismatch into a
/// fatal `DemuxError::StrictRejection` returned from `feed`.
#[test]
fn pmt_program_number_mismatch_strict_full_rejects() {
    let bytes = build_mismatched_stream();
    let mut d = Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
    // PAT may succeed; the PMT is what triggers the rejection.
    // Feed the whole buffer and check for StrictRejection.
    let result = d.feed(&bytes);
    assert!(
        matches!(result, Err(DemuxError::StrictRejection(_))),
        "StrictMode::Full must reject on PmtProgramNumberMismatch; got {result:?}"
    );
}

/// Regression guard: a well-formed PMT where program_number MATCHES the PAT
/// must still emit a ProgramMap and must NOT emit a PmtProgramNumberMismatch.
#[test]
fn pmt_program_number_matches_pat_is_accepted() {
    let pat_sec = build_pat_section(PAT_PROGRAM, PMT_PID);
    // PMT body also claims PAT_PROGRAM — correct.
    let pmt_sec = build_pmt_section(PAT_PROGRAM, PCR_PID, VIDEO_PID);

    let mut buf = Vec::new();
    for pkt in pack_section(0x0000, &pat_sec, 0) {
        buf.extend_from_slice(&pkt);
    }
    for pkt in pack_section(PMT_PID, &pmt_sec, 0) {
        buf.extend_from_slice(&pkt);
    }

    let mut d = Demuxer::new();
    d.feed(&buf).unwrap();
    let (issues, map_count) = drain_events(&mut d);

    let mismatch_issues: Vec<_> = issues
        .iter()
        .filter(|i| matches!(i, NonConformantIssue::PmtProgramNumberMismatch { .. }))
        .collect();
    assert!(
        mismatch_issues.is_empty(),
        "matching PMT must not trigger PmtProgramNumberMismatch"
    );
    assert_eq!(
        map_count, 1,
        "matching PMT must emit exactly one ProgramMap"
    );
}
