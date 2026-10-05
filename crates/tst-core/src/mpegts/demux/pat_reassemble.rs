//! Surgical multi-section PAT reassembly. PMT multi-section is
//! intentionally NOT reassembled (still rejected); see psi.rs/parse_pat.
//!
//! Distinct from `PsiSectionAssembler` (one section across TS PACKETS) — this
//! layer assembles multiple `section_number`s of ONE table across the
//! complete `0..=last_section_number` range.

use super::psi::{PatEntry, PatSection};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// DoS cap: refuse to buffer more than this many program entries across all
/// sections of one table. A spec-max PAT is 256 sections; the muxer caps at
/// 16 streams, so any real PAT is tiny. Mirrors the demuxer's existing 4 KiB
/// section posture (4 bytes/entry → ~32 KiB at the cap).
const MAX_PAT_ENTRIES: usize = 8192;

#[derive(Debug, Default)]
pub(super) struct PatReassembler {
    /// (transport_stream_id, version, current_next_indicator) of the table
    /// currently being assembled. `None` when idle.
    key: Option<(u16, u8, bool)>,
    last_section_number: u8,
    /// section_number → that section's program entries.
    sections: BTreeMap<u8, Vec<PatEntry>>,
    entry_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PatReassemblyOutcome {
    /// Still waiting for more sections of a valid in-progress table.
    Pending,
    /// All sections present and contiguous; here is the merged program list.
    Complete(Vec<PatEntry>),
    /// Genuinely-broken multi-section input — the DoS cap was exceeded, or a
    /// section disagreed on `last_section_number` for the same table. Caller
    /// emits `PsiMultiSectionUnsupported`. Buffer is reset.
    Broken,
}

impl PatReassembler {
    pub(super) fn clear(&mut self) {
        self.key = None;
        self.last_section_number = 0;
        self.sections.clear();
        self.entry_count = 0;
    }

    /// Feed one section of a multi-section PAT (caller guarantees
    /// `s.last_section_number != 0`).
    pub(super) fn accept(&mut self, s: &PatSection) -> PatReassemblyOutcome {
        let key = (s.transport_stream_id, s.version, s.current_next_indicator);
        match self.key {
            Some(k) if k != key => {
                // A new tsid/version/current_next mid-assembly means the prior
                // table will never complete. Restart fresh on the new key.
                // (Treat a bare version bump as a fresh table, not Broken —
                // the sender rotated the PAT; "don't blame the sender".)
                self.clear();
                self.start(key, s);
            }
            None => self.start(key, s),
            Some(_) => {
                // Same (tsid, version, current_next) table: every section MUST
                // agree on last_section_number. A mismatch is malformed input
                // (the table size cannot change without a version bump) —
                // reject rather than risk a premature partial-apply.
                if s.last_section_number != self.last_section_number {
                    self.clear();
                    return PatReassemblyOutcome::Broken;
                }
            }
        }
        // Insert (or overwrite a re-sent) section.
        let prev = self.sections.insert(s.section_number, s.programs.clone());
        self.entry_count =
            self.entry_count - prev.as_ref().map_or(0, |p| p.len()) + s.programs.len();
        if self.entry_count > MAX_PAT_ENTRIES {
            self.clear();
            return PatReassemblyOutcome::Broken;
        }
        // Complete iff every section 0..=last is present.
        let want = self.last_section_number as usize + 1;
        if self.sections.len() == want
            && (0..=self.last_section_number).all(|n| self.sections.contains_key(&n))
        {
            let mut merged = Vec::with_capacity(self.entry_count);
            for n in 0..=self.last_section_number {
                if let Some(entries) = self.sections.get(&n) {
                    merged.extend(entries.iter().cloned());
                }
            }
            self.clear();
            PatReassemblyOutcome::Complete(merged)
        } else {
            PatReassemblyOutcome::Pending
        }
    }

    fn start(&mut self, key: (u16, u8, bool), s: &PatSection) {
        self.key = Some(key);
        self.last_section_number = s.last_section_number;
        self.sections.clear();
        self.entry_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpegts::demux::psi::PatEntry;

    fn make_section(
        tsid: u16,
        version: u8,
        section_number: u8,
        last_section_number: u8,
        programs: &[(u16, u16)],
    ) -> PatSection {
        PatSection {
            transport_stream_id: tsid,
            version,
            current_next_indicator: true,
            section_number,
            last_section_number,
            programs: programs
                .iter()
                .map(|&(pn, pid)| PatEntry {
                    program_number: pn,
                    pid,
                })
                .collect(),
        }
    }

    #[test]
    fn two_section_pat_completes() {
        let mut r = PatReassembler::default();
        let s0 = make_section(1, 0, 0, 1, &[(1, 0x100)]);
        let s1 = make_section(1, 0, 1, 1, &[(2, 0x200)]);
        assert_eq!(r.accept(&s0), PatReassemblyOutcome::Pending);
        let out = r.accept(&s1);
        match out {
            PatReassemblyOutcome::Complete(programs) => {
                assert_eq!(programs.len(), 2);
                assert_eq!(programs[0].program_number, 1);
                assert_eq!(programs[1].program_number, 2);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_stays_pending() {
        let mut r = PatReassembler::default();
        let s0 = make_section(1, 0, 0, 1, &[(1, 0x100)]);
        assert_eq!(r.accept(&s0), PatReassemblyOutcome::Pending);
    }

    #[test]
    fn version_flip_restarts_fresh() {
        let mut r = PatReassembler::default();
        // First table version 0, section 0 only
        let s0v0 = make_section(1, 0, 0, 1, &[(1, 0x100)]);
        assert_eq!(r.accept(&s0v0), PatReassemblyOutcome::Pending);
        // Version flip — section 0 of version 1 arrives before version 0 completes
        let s0v1 = make_section(1, 1, 0, 1, &[(3, 0x300)]);
        assert_eq!(r.accept(&s0v1), PatReassemblyOutcome::Pending);
        // Now complete version 1
        let s1v1 = make_section(1, 1, 1, 1, &[(4, 0x400)]);
        match r.accept(&s1v1) {
            PatReassemblyOutcome::Complete(p) => {
                assert_eq!(p.len(), 2);
                assert_eq!(p[0].program_number, 3);
                assert_eq!(p[1].program_number, 4);
            }
            other => panic!("expected Complete after version flip, got {other:?}"),
        }
    }

    #[test]
    fn clear_resets_state() {
        let mut r = PatReassembler::default();
        let s0 = make_section(1, 0, 0, 1, &[(1, 0x100)]);
        r.accept(&s0);
        r.clear();
        assert_eq!(r.key, None);
        assert_eq!(r.entry_count, 0);
        assert!(r.sections.is_empty());
    }

    /// Build a PatSection with `n` program entries directly (bypass the wire
    /// format — the cap is on cumulative entry COUNT, not bytes, so the
    /// section need not be a valid on-wire PAT).
    fn make_section_n_entries(
        version: u8,
        section_number: u8,
        last_section_number: u8,
        n: usize,
    ) -> PatSection {
        PatSection {
            transport_stream_id: 1,
            version,
            current_next_indicator: true,
            section_number,
            last_section_number,
            programs: (0..n)
                .map(|i| PatEntry {
                    // program_number 0 is the NIT; offset so every entry is a
                    // real program. Values wrap mod 2^16 — irrelevant to the
                    // cap, which counts entries, not distinct numbers.
                    program_number: (i as u16).wrapping_add(1),
                    pid: 0x0100,
                })
                .collect(),
        }
    }

    /// A malformed multi-section PAT whose sections disagree on
    /// last_section_number must NOT complete with a partial set — it returns
    /// Broken and clears, so a subsequent valid table still completes.
    #[test]
    fn inconsistent_last_section_number_returns_broken_and_clears() {
        let mut r = PatReassembler::default();
        // Section 0 claims a 2-section table (last=1)...
        let s0 = make_section(1, 0, 0, 1, &[(1, 0x100)]);
        assert_eq!(r.accept(&s0), PatReassemblyOutcome::Pending);
        // ...but section 1 of the SAME (tsid, version, current_next) claims last=3.
        let s1_bad = make_section(1, 0, 1, 3, &[(2, 0x200)]);
        assert_eq!(r.accept(&s1_bad), PatReassemblyOutcome::Broken);
        assert_eq!(r.key, None);
        assert!(r.sections.is_empty());
        // A fresh valid table on a new key still completes (not wedged).
        let v0 = make_section(5, 1, 0, 1, &[(9, 0x900)]);
        let v1 = make_section(5, 1, 1, 1, &[(10, 0xA00)]);
        assert_eq!(r.accept(&v0), PatReassemblyOutcome::Pending);
        assert!(matches!(r.accept(&v1), PatReassemblyOutcome::Complete(p) if p.len() == 2));
    }

    /// SECURITY: cumulative entries exceeding MAX_PAT_ENTRIES (8192) across a
    /// table's sections must trip the DoS cap → `Broken`, and the buffer must
    /// be cleared so a subsequent valid table on a fresh key still completes.
    #[test]
    fn cumulative_entries_over_cap_returns_broken_and_clears() {
        let mut r = PatReassembler::default();
        // ~5000 + ~5000 = ~10000 > 8192. Section 0 alone stays under the cap
        // (Pending); section 1 pushes the cumulative count over → Broken.
        let s0 = make_section_n_entries(0, 0, 1, 5000);
        let s1 = make_section_n_entries(0, 1, 1, 5000);
        assert_eq!(r.accept(&s0), PatReassemblyOutcome::Pending);
        assert_eq!(r.accept(&s1), PatReassemblyOutcome::Broken);
        // Buffer must be cleared after Broken.
        assert_eq!(r.key, None);
        assert_eq!(r.entry_count, 0);
        assert!(r.sections.is_empty());

        // A fresh, valid two-section table on a NEW key must still complete —
        // proving the cap fire didn't wedge the reassembler.
        let v0 = make_section(2, 1, 0, 1, &[(7, 0x700)]);
        let v1 = make_section(2, 1, 1, 1, &[(8, 0x800)]);
        assert_eq!(r.accept(&v0), PatReassemblyOutcome::Pending);
        match r.accept(&v1) {
            PatReassemblyOutcome::Complete(programs) => {
                assert_eq!(programs.len(), 2);
                assert_eq!(programs[0].program_number, 7);
                assert_eq!(programs[1].program_number, 8);
            }
            other => panic!("expected Complete after cap recovery, got {other:?}"),
        }
    }
}
