//! Per-PID Metadata AU cell reassembler.
//!
//! State machine for accumulating fragmented sync-metadata AU cells per
//! H.222.0 V9 §2.12.4.2 Table 2-157 (`cell_fragment_indication` =
//! First / Middle / Last) into complete Access Units. Single-cell
//! (`Complete`) cells pass through unchanged. Failure modes surface as
//! [`MultiCellAuReason`] variants — Orphan / SequenceGap /
//! ConcurrentFirst / Overflow.
//!
//! Owned by the demuxer (one instance per [`super::Demuxer`]). Keyed by
//! PID, because PIDs are unique within a TS even when shared across
//! PMTs. Cleared wholesale on `Demuxer::reset_sync()` and on PMT
//! version change.

use crate::mpegts::au_cell::{AuCellHeader, CellFragmentIndication};
use alloc::vec::Vec;
use hashbrown::HashMap;

use super::event::MultiCellAuReason;

/// In-flight reassembly state for one PID.
#[derive(Debug)]
struct PidReassemblyState {
    /// Header from the `First` cell. Surfaced on emit (CFI forced to
    /// `Complete`, see [`AuCellReassembler::process_cell`]).
    first_header: AuCellHeader,
    /// Accumulated inner-byte payload (concatenated AU cell
    /// `AU_cell_data_byte` regions).
    buf: Vec<u8>,
    /// Number of cells accumulated so far (≥ 1 once a `First` is seen).
    cell_count: u32,
}

impl PidReassemblyState {
    fn next_expected_seq(&self) -> u8 {
        self.first_header
            .sequence_number
            .wrapping_add(self.cell_count as u8)
    }
}

/// Outcome of feeding one AU cell into the reassembler.
#[derive(Debug)]
pub(crate) enum ReassembleOutcome<'a> {
    /// A complete AU is ready to emit. `header` is the first cell's
    /// header (with `cell_fragment_indication` forced to `Complete`),
    /// `payload` is the concatenated inner bytes (zero-copy borrow into
    /// the reassembler's internal buffer for the multi-cell case, or a
    /// borrow into the caller's slice for the single-cell case),
    /// `cell_count` is how many cells contributed (1 for Complete,
    /// ≥ 2 for reassembled).
    Emit {
        header: AuCellHeader,
        payload: &'a [u8],
        cell_count: u32,
    },
    /// Fragment buffered; no emit yet. Wait for more cells.
    Buffered,
    /// Reassembly failed. Buffer (if any) has been dropped. The caller
    /// SHOULD emit `NonConformantIssue::MultiCellAu { reason, dropped_bytes, .. }`.
    Failure {
        reason: MultiCellAuReason,
        dropped_bytes: usize,
    },
}

#[derive(Debug)]
pub(crate) struct AuCellReassembler {
    per_pid: HashMap<u16, PidReassemblyState>,
    cap_per_pid: usize,
    /// Aggregate cap on the sum of all per-PID buffer bytes. Defends a
    /// multi-PID flood where each PID stays under `cap_per_pid` but the
    /// total explodes (mirrors the PES `Reassembler::cap_total` shape).
    cap_total: usize,
    /// Ceiling on the number of PIDs with an in-flight (open `First`)
    /// reassembly. Bounds active-PID count when an adversary opens a
    /// `First` for thousands of distinct PIDs and never sends `Last`.
    max_in_flight_pids: usize,
    /// Live sum of `per_pid[*].buf.len()`. Maintained on every
    /// insert/append/drop/emit path so the aggregate cap can be enforced
    /// in O(1).
    total_buffered: usize,
}

impl AuCellReassembler {
    /// Construct with a per-PID cap and the aggregate-bound defaults
    /// (`DEFAULT_AU_CELL_CAP_TOTAL`, `DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS`).
    #[cfg(test)]
    pub(crate) fn new(cap_per_pid: usize) -> Self {
        Self::with_limits(
            cap_per_pid,
            super::types::DEFAULT_AU_CELL_CAP_TOTAL,
            super::types::DEFAULT_AU_CELL_MAX_IN_FLIGHT_PIDS,
        )
    }

    pub(crate) fn with_limits(
        cap_per_pid: usize,
        cap_total: usize,
        max_in_flight_pids: usize,
    ) -> Self {
        Self {
            per_pid: HashMap::new(),
            cap_per_pid,
            cap_total,
            max_in_flight_pids,
            total_buffered: 0,
        }
    }

    /// Decrement the aggregate counter by a just-dropped buffer's length.
    ///
    /// Every drop path (the `process_cell` failure arms plus `reset_pid`
    /// and `clear_after_emit`) routes through here so the accounting
    /// invariant is checked uniformly. `debug_assert!` surfaces a future
    /// under-decrement bug (the counter should always be at least as large
    /// as any single live buffer it contains) in tests / debug builds;
    /// release builds keep the `saturating_sub` clamp as a safety net so a
    /// miscount can never wrap to a huge value.
    #[inline]
    fn decrement_total(&mut self, dropped_len: usize) {
        debug_assert!(
            self.total_buffered >= dropped_len,
            "AU-cell aggregate counter under-decrement: total_buffered={} < dropped_len={}",
            self.total_buffered,
            dropped_len
        );
        self.total_buffered = self.total_buffered.saturating_sub(dropped_len);
    }

    /// Feed one AU cell into the reassembler. Returns the outcome.
    ///
    /// For `ConcurrentFirst` failures the caller MUST emit the failure
    /// event AND then re-call `process_cell` with the same cell — the
    /// internal state is now empty so the second call follows the
    /// empty-state row of the state table and either buffers (if the
    /// new cell is `First`) or emits (if `Complete`).
    pub(crate) fn process_cell<'a>(
        &'a mut self,
        pid: u16,
        header: AuCellHeader,
        payload: &'a [u8],
    ) -> ReassembleOutcome<'a> {
        match (
            self.per_pid.contains_key(&pid),
            header.cell_fragment_indication,
        ) {
            // Empty + Complete → emit directly.
            (false, CellFragmentIndication::Complete) => ReassembleOutcome::Emit {
                header,
                payload,
                cell_count: 1,
            },

            // Empty + First → open buffer.
            (false, CellFragmentIndication::First) => {
                if payload.len() > self.cap_per_pid {
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::Overflow,
                        dropped_bytes: payload.len(),
                    };
                }
                // Aggregate active-PID ceiling: refuse to open a new PID
                // beyond the limit. Deterministic rejection (we do NOT
                // evict an existing PID — that would silently drop another
                // stream's in-flight AU). The adversary's flood of unique
                // First cells is bounded; legitimate streams keep theirs.
                if self.per_pid.len() >= self.max_in_flight_pids {
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::TooManyPids,
                        dropped_bytes: payload.len(),
                    };
                }
                // Aggregate byte ceiling: checked add against the running
                // total before committing the allocation.
                if self.total_buffered.saturating_add(payload.len()) > self.cap_total {
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::OverflowTotal,
                        dropped_bytes: payload.len(),
                    };
                }
                self.total_buffered += payload.len();
                self.per_pid.insert(
                    pid,
                    PidReassemblyState {
                        first_header: header,
                        buf: payload.to_vec(),
                        cell_count: 1,
                    },
                );
                ReassembleOutcome::Buffered
            }

            // Empty + Middle/Last → orphan.
            (false, CellFragmentIndication::Middle | CellFragmentIndication::Last) => {
                ReassembleOutcome::Failure {
                    reason: MultiCellAuReason::Orphan,
                    dropped_bytes: payload.len(),
                }
            }

            // Buffering + Complete → drop buffer (concurrent), then caller re-enters.
            (true, CellFragmentIndication::Complete) => {
                let dropped = self.per_pid.remove(&pid).unwrap();
                self.decrement_total(dropped.buf.len());
                let _ = header;
                ReassembleOutcome::Failure {
                    reason: MultiCellAuReason::ConcurrentFirst,
                    dropped_bytes: dropped.buf.len(),
                }
            }

            // Buffering + First → drop buffer (concurrent). Caller re-calls.
            (true, CellFragmentIndication::First) => {
                let dropped = self.per_pid.remove(&pid).unwrap();
                self.decrement_total(dropped.buf.len());
                ReassembleOutcome::Failure {
                    reason: MultiCellAuReason::ConcurrentFirst,
                    dropped_bytes: dropped.buf.len(),
                }
            }

            // Buffering + Middle → seq check + append.
            (true, CellFragmentIndication::Middle) => {
                let state = self.per_pid.get_mut(&pid).unwrap();
                if header.sequence_number != state.next_expected_seq() {
                    let dropped = self.per_pid.remove(&pid).unwrap();
                    self.decrement_total(dropped.buf.len());
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::SequenceGap,
                        dropped_bytes: dropped.buf.len() + payload.len(),
                    };
                }
                if state.buf.len() + payload.len() > self.cap_per_pid {
                    let dropped = self.per_pid.remove(&pid).unwrap();
                    self.decrement_total(dropped.buf.len());
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::Overflow,
                        dropped_bytes: dropped.buf.len() + payload.len(),
                    };
                }
                // Aggregate byte ceiling on append: drop this PID's buffer
                // and surface OverflowTotal if appending would breach it.
                if self.total_buffered + payload.len() > self.cap_total {
                    let dropped = self.per_pid.remove(&pid).unwrap();
                    self.decrement_total(dropped.buf.len());
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::OverflowTotal,
                        dropped_bytes: dropped.buf.len() + payload.len(),
                    };
                }
                let state = self.per_pid.get_mut(&pid).unwrap();
                state.buf.extend_from_slice(payload);
                state.cell_count += 1;
                self.total_buffered += payload.len();
                ReassembleOutcome::Buffered
            }

            // Buffering + Last → seq check + append + emit.
            (true, CellFragmentIndication::Last) => {
                let state = self.per_pid.get_mut(&pid).unwrap();
                if header.sequence_number != state.next_expected_seq() {
                    let dropped = self.per_pid.remove(&pid).unwrap();
                    self.decrement_total(dropped.buf.len());
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::SequenceGap,
                        dropped_bytes: dropped.buf.len() + payload.len(),
                    };
                }
                if state.buf.len() + payload.len() > self.cap_per_pid {
                    let dropped = self.per_pid.remove(&pid).unwrap();
                    self.decrement_total(dropped.buf.len());
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::Overflow,
                        dropped_bytes: dropped.buf.len() + payload.len(),
                    };
                }
                if self.total_buffered + payload.len() > self.cap_total {
                    let dropped = self.per_pid.remove(&pid).unwrap();
                    self.decrement_total(dropped.buf.len());
                    return ReassembleOutcome::Failure {
                        reason: MultiCellAuReason::OverflowTotal,
                        dropped_bytes: dropped.buf.len() + payload.len(),
                    };
                }
                let state = self.per_pid.get_mut(&pid).unwrap();
                state.buf.extend_from_slice(payload);
                state.cell_count += 1;
                self.total_buffered += payload.len();
                let state_ref = self.per_pid.get(&pid).unwrap();
                let mut first_header = state_ref.first_header;
                first_header.cell_fragment_indication = CellFragmentIndication::Complete;
                ReassembleOutcome::Emit {
                    header: first_header,
                    payload: &state_ref.buf,
                    cell_count: state_ref.cell_count,
                }
            }
        }
    }

    /// Clear the buffer for one PID (operational reset, not a wire-format
    /// violation; no NonConformant emit). Reserved for future hooks; not
    /// called from the current pes_emit / reset_sync paths (those use
    /// [`Self::reset_all`]).
    #[allow(dead_code)]
    pub(crate) fn reset_pid(&mut self, pid: u16) {
        if let Some(dropped) = self.per_pid.remove(&pid) {
            self.decrement_total(dropped.buf.len());
        }
    }

    /// Clear all buffers. Called from `Demuxer::reset_sync()` and on PMT
    /// version change.
    pub(crate) fn reset_all(&mut self) {
        self.per_pid.clear();
        self.total_buffered = 0;
    }

    /// On `Emit` of a multi-cell AU the caller drains the buffer via this
    /// method (since `Emit`'s `&[u8]` borrow prevents `&mut self`
    /// operations during the emit). Call AFTER copying / consuming the
    /// `Emit::payload` slice.
    pub(crate) fn clear_after_emit(&mut self, pid: u16) {
        if let Some(dropped) = self.per_pid.remove(&pid) {
            self.decrement_total(dropped.buf.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(cfi: CellFragmentIndication, seq: u8) -> AuCellHeader {
        AuCellHeader {
            metadata_service_id: 0,
            sequence_number: seq,
            cell_fragment_indication: cfi,
            decoder_config_flag: false,
            random_access_indicator: true,
        }
    }

    #[test]
    fn complete_cell_emits_directly() {
        let mut r = AuCellReassembler::new(1024);
        let out = r.process_cell(
            0x100,
            hdr(CellFragmentIndication::Complete, 42),
            &[0xAA, 0xBB],
        );
        match out {
            ReassembleOutcome::Emit {
                header,
                payload,
                cell_count,
            } => {
                assert_eq!(header.sequence_number, 42);
                assert_eq!(payload, &[0xAA, 0xBB]);
                assert_eq!(cell_count, 1);
            }
            _ => panic!("expected Emit, got {:?}", out),
        }
    }

    #[test]
    fn first_middle_last_reassembles() {
        let mut r = AuCellReassembler::new(1024);
        assert!(matches!(
            r.process_cell(0x100, hdr(CellFragmentIndication::First, 10), &[0x11; 3]),
            ReassembleOutcome::Buffered
        ));
        assert!(matches!(
            r.process_cell(0x100, hdr(CellFragmentIndication::Middle, 11), &[0x22; 3]),
            ReassembleOutcome::Buffered
        ));
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Last, 12), &[0x33; 3]);
        match out {
            ReassembleOutcome::Emit {
                header,
                payload,
                cell_count,
            } => {
                assert_eq!(
                    header.cell_fragment_indication,
                    CellFragmentIndication::Complete
                );
                assert_eq!(header.sequence_number, 10);
                assert_eq!(payload.len(), 9);
                assert_eq!(&payload[..3], &[0x11; 3]);
                assert_eq!(&payload[3..6], &[0x22; 3]);
                assert_eq!(&payload[6..], &[0x33; 3]);
                assert_eq!(cell_count, 3);
            }
            _ => panic!("expected Emit, got {:?}", out),
        }
        r.clear_after_emit(0x100);
        assert!(!r.per_pid.contains_key(&0x100));
    }

    #[test]
    fn orphan_middle_fails() {
        let mut r = AuCellReassembler::new(1024);
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Middle, 5), &[0xAA; 10]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::Orphan,
                dropped_bytes: 10
            }
        ));
    }

    #[test]
    fn orphan_last_fails() {
        let mut r = AuCellReassembler::new(1024);
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Last, 5), &[0xAA; 7]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::Orphan,
                dropped_bytes: 7
            }
        ));
    }

    #[test]
    fn sequence_gap_in_middle_fails_and_drops_buffer() {
        let mut r = AuCellReassembler::new(1024);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 10), &[0x11; 3]);
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Middle, 12), &[0x22; 3]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::SequenceGap,
                dropped_bytes: 6,
            }
        ));
        assert!(!r.per_pid.contains_key(&0x100));
    }

    #[test]
    fn concurrent_first_drops_old_buffer() {
        let mut r = AuCellReassembler::new(1024);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 10), &[0x11; 5]);
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::First, 20), &[0x22; 5]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::ConcurrentFirst,
                dropped_bytes: 5,
            }
        ));
        assert!(matches!(
            r.process_cell(0x100, hdr(CellFragmentIndication::First, 20), &[0x22; 5]),
            ReassembleOutcome::Buffered
        ));
    }

    #[test]
    fn complete_while_buffering_drops_old_then_passes_through() {
        // State-table row 5: (Buffering, Complete) → Failure(ConcurrentFirst),
        // caller re-enters with the same Complete cell against now-empty state
        // (which then matches row 1 → Emit).
        let mut r = AuCellReassembler::new(1024);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 10), &[0x11; 5]);
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Complete, 20), &[0x22; 5]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::ConcurrentFirst,
                dropped_bytes: 5,
            }
        ));
        // Re-entry: same Complete on now-empty state → Emit (cell_count=1).
        let reentry = r.process_cell(0x100, hdr(CellFragmentIndication::Complete, 20), &[0x22; 5]);
        match reentry {
            ReassembleOutcome::Emit {
                header,
                payload,
                cell_count,
            } => {
                assert_eq!(header.sequence_number, 20);
                assert_eq!(payload, &[0x22; 5]);
                assert_eq!(cell_count, 1);
            }
            _ => panic!("expected Emit on re-entry, got {:?}", reentry),
        }
    }

    #[test]
    fn overflow_drops_buffer() {
        let mut r = AuCellReassembler::new(10);
        assert!(matches!(
            r.process_cell(0x100, hdr(CellFragmentIndication::First, 0), &[0xAA; 5]),
            ReassembleOutcome::Buffered
        ));
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Middle, 1), &[0xBB; 10]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::Overflow,
                dropped_bytes: 15,
            }
        ));
        assert!(!r.per_pid.contains_key(&0x100));
    }

    #[test]
    fn sequence_wrap_mod_256_is_not_a_gap() {
        let mut r = AuCellReassembler::new(1024);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 254), &[0x11; 3]);
        r.process_cell(0x100, hdr(CellFragmentIndication::Middle, 255), &[0x22; 3]);
        let out = r.process_cell(0x100, hdr(CellFragmentIndication::Last, 0), &[0x33; 3]);
        assert!(matches!(out, ReassembleOutcome::Emit { cell_count: 3, .. }));
    }

    #[test]
    fn multi_pid_isolation() {
        let mut r = AuCellReassembler::new(1024);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 0), &[0xAA; 3]);
        r.process_cell(0x200, hdr(CellFragmentIndication::First, 100), &[0xBB; 3]);
        let out_a = r.process_cell(0x100, hdr(CellFragmentIndication::Last, 1), &[0xCC; 3]);
        assert!(matches!(out_a, ReassembleOutcome::Emit { .. }));
        r.clear_after_emit(0x100);
        let out_b = r.process_cell(0x200, hdr(CellFragmentIndication::Last, 101), &[0xDD; 3]);
        assert!(matches!(out_b, ReassembleOutcome::Emit { .. }));
        r.clear_after_emit(0x200);
        assert!(r.per_pid.is_empty());
    }

    #[test]
    fn reset_all_clears_buffers_silently() {
        let mut r = AuCellReassembler::new(1024);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 0), &[0xAA; 3]);
        r.process_cell(0x200, hdr(CellFragmentIndication::First, 0), &[0xBB; 3]);
        r.reset_all();
        assert!(r.per_pid.is_empty());
    }

    // ---- Aggregate-bound tests ---------------------------------------

    /// Many distinct PIDs each opening a `First` cell (never sending
    /// `Last`) must NOT grow retained bytes unboundedly: either the
    /// active-PID count or the aggregate byte total caps it. Here we keep
    /// each PID well under its per-PID cap so ONLY the aggregate guards
    /// can fire.
    #[test]
    fn many_distinct_pids_first_only_is_bounded() {
        // Generous per-PID cap so per-PID overflow never triggers; small
        // PID ceiling so the active-PID guard is what bounds us.
        let mut r = AuCellReassembler::with_limits(
            /*per_pid*/ 1 << 20,
            /*total*/ 1 << 30,
            /*max_pids*/ 8,
        );
        let mut buffered = 0usize;
        for pid in 0u16..1000 {
            match r.process_cell(pid, hdr(CellFragmentIndication::First, 0), &[0xAA; 16]) {
                ReassembleOutcome::Buffered => buffered += 1,
                ReassembleOutcome::Failure {
                    reason: MultiCellAuReason::TooManyPids,
                    ..
                } => {}
                other => panic!("unexpected outcome for pid {pid}: {other:?}"),
            }
        }
        // Active PIDs never exceeds the ceiling.
        assert!(
            r.per_pid.len() <= 8,
            "active PID count {} exceeded ceiling 8",
            r.per_pid.len()
        );
        // The counter agrees with the live buffers.
        assert_eq!(
            r.total_buffered,
            r.per_pid.values().map(|s| s.buf.len()).sum::<usize>()
        );
        assert_eq!(buffered, 8);
    }

    /// Aggregate byte cap fires even when each PID is individually under
    /// its per-PID cap and the active-PID ceiling is generous.
    #[test]
    fn aggregate_byte_cap_rejects_when_total_exceeded() {
        // per_pid 1 KiB (each First is 200 B, under it); total 1 KiB
        // (so the 6th First would push us over); max_pids generous.
        let mut r = AuCellReassembler::with_limits(1024, 1024, 1000);
        let mut buffered = 0usize;
        let mut total_failures = 0usize;
        for pid in 0u16..50 {
            match r.process_cell(pid, hdr(CellFragmentIndication::First, 0), &[0xAA; 200]) {
                ReassembleOutcome::Buffered => buffered += 1,
                ReassembleOutcome::Failure {
                    reason: MultiCellAuReason::OverflowTotal,
                    ..
                } => total_failures += 1,
                other => panic!("unexpected outcome for pid {pid}: {other:?}"),
            }
        }
        assert!(
            r.total_buffered <= 1024,
            "total_buffered {} exceeded cap",
            r.total_buffered
        );
        assert_eq!(buffered, 5, "5 * 200 = 1000 <= 1024 fit; 6th rejected");
        assert!(total_failures > 0);
        assert_eq!(
            r.total_buffered,
            r.per_pid.values().map(|s| s.buf.len()).sum::<usize>()
        );
    }

    /// The aggregate counter must be decremented on EVERY drop path, not
    /// just `clear_after_emit`. Exercises all decrement arms so a future
    /// miscount in any one of them surfaces here (and trips the
    /// `decrement_total` `debug_assert!`):
    ///
    /// - `Middle` arm: per-PID overflow, sequence gap.
    /// - `Last` arm: per-PID overflow, sequence gap.
    /// - `Buffering+First` ConcurrentFirst drop.
    /// - `Buffering+Complete` ConcurrentFirst drop.
    /// - `reset_pid`.
    /// - clean emit + `clear_after_emit`.
    ///
    /// After every mutation the aggregate counter is asserted equal to the
    /// live buffer sum.
    #[test]
    fn aggregate_counter_tracks_all_drop_paths() {
        let mut r = AuCellReassembler::with_limits(64, 1 << 20, 1000);
        let live_sum =
            |r: &AuCellReassembler| r.per_pid.values().map(|s| s.buf.len()).sum::<usize>();

        // Open PIDs to be dropped via each path.
        r.process_cell(0x10, hdr(CellFragmentIndication::First, 0), &[0xAA; 10]);
        r.process_cell(0x20, hdr(CellFragmentIndication::First, 0), &[0xBB; 10]);
        r.process_cell(0x30, hdr(CellFragmentIndication::First, 0), &[0xCC; 10]);
        assert_eq!(r.total_buffered, live_sum(&r));

        // Middle-arm per-PID append overflow drops PID 0x10's buffer.
        let _ = r.process_cell(0x10, hdr(CellFragmentIndication::Middle, 1), &[0xAA; 100]);
        assert!(!r.per_pid.contains_key(&0x10));
        assert_eq!(r.total_buffered, live_sum(&r));

        // Middle-arm sequence gap drops PID 0x20's buffer.
        let _ = r.process_cell(0x20, hdr(CellFragmentIndication::Middle, 99), &[0xBB; 5]);
        assert!(!r.per_pid.contains_key(&0x20));
        assert_eq!(r.total_buffered, live_sum(&r));

        // Buffering+First ConcurrentFirst drops PID 0x30's open buffer.
        // (The `(true, First)` arm drops and signals; it does NOT
        // re-buffer — a real caller re-enters the cell against the now-
        // empty state. So PID 0x30 is empty after this.)
        let out = r.process_cell(0x30, hdr(CellFragmentIndication::First, 50), &[0xDD; 7]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::ConcurrentFirst,
                ..
            }
        ));
        assert!(!r.per_pid.contains_key(&0x30));
        assert_eq!(r.total_buffered, live_sum(&r));

        // Buffering+Complete ConcurrentFirst: open a fresh PID, then send a
        // Complete while it is still buffering → drop + signal.
        r.process_cell(0x35, hdr(CellFragmentIndication::First, 0), &[0xEE; 9]);
        assert_eq!(r.total_buffered, live_sum(&r));
        let out = r.process_cell(0x35, hdr(CellFragmentIndication::Complete, 60), &[0xEF; 3]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::ConcurrentFirst,
                ..
            }
        ));
        assert!(!r.per_pid.contains_key(&0x35));
        assert_eq!(r.total_buffered, live_sum(&r));

        // Last-arm sequence gap: open a PID, advance one valid Middle,
        // then send a Last with a wrong sequence_number → drop.
        r.process_cell(0x50, hdr(CellFragmentIndication::First, 0), &[0x11; 8]);
        r.process_cell(0x50, hdr(CellFragmentIndication::Middle, 1), &[0x22; 8]);
        assert_eq!(r.total_buffered, live_sum(&r));
        let out = r.process_cell(0x50, hdr(CellFragmentIndication::Last, 99), &[0x33; 8]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::SequenceGap,
                ..
            }
        ));
        assert!(!r.per_pid.contains_key(&0x50));
        assert_eq!(r.total_buffered, live_sum(&r));

        // Last-arm per-PID overflow: a Last whose payload pushes the
        // buffer past cap_per_pid (64) is dropped.
        r.process_cell(0x60, hdr(CellFragmentIndication::First, 0), &[0x44; 10]);
        assert_eq!(r.total_buffered, live_sum(&r));
        let out = r.process_cell(0x60, hdr(CellFragmentIndication::Last, 1), &[0x55; 100]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::Overflow,
                ..
            }
        ));
        assert!(!r.per_pid.contains_key(&0x60));
        assert_eq!(r.total_buffered, live_sum(&r));

        // reset_pid drops a still-open PID.
        r.process_cell(0x70, hdr(CellFragmentIndication::First, 0), &[0x66; 12]);
        assert_eq!(r.total_buffered, live_sum(&r));
        r.reset_pid(0x70);
        assert!(!r.per_pid.contains_key(&0x70));
        assert_eq!(r.total_buffered, 0);
        assert_eq!(r.total_buffered, live_sum(&r));

        // Clean emit path: First+Last, clear_after_emit zeroes the counter.
        r.process_cell(0x40, hdr(CellFragmentIndication::First, 0), &[0x11; 4]);
        assert_eq!(r.total_buffered, live_sum(&r));
        let out = r.process_cell(0x40, hdr(CellFragmentIndication::Last, 1), &[0x22; 4]);
        assert!(matches!(out, ReassembleOutcome::Emit { .. }));
        r.clear_after_emit(0x40);
        assert_eq!(r.total_buffered, 0);
        assert_eq!(r.total_buffered, live_sum(&r));
    }

    /// The aggregate-byte (`OverflowTotal`) drop on the append arms
    /// (`Middle` and `Last`) also decrements the counter correctly. Sets a
    /// tight `cap_total` so the second cell's append breaches it.
    #[test]
    fn aggregate_overflow_on_append_arms_decrements_counter() {
        let live_sum =
            |r: &AuCellReassembler| r.per_pid.values().map(|s| s.buf.len()).sum::<usize>();

        // Middle arm: First buffers 40 B (< cap_total 50); the Middle's
        // +40 B would make 80 > 50 → OverflowTotal, buffer dropped.
        let mut r = AuCellReassembler::with_limits(
            /*per_pid*/ 1024, /*total*/ 50, /*max_pids*/ 16,
        );
        r.process_cell(0x10, hdr(CellFragmentIndication::First, 0), &[0xAA; 40]);
        assert_eq!(r.total_buffered, 40);
        let out = r.process_cell(0x10, hdr(CellFragmentIndication::Middle, 1), &[0xBB; 40]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::OverflowTotal,
                ..
            }
        ));
        assert!(!r.per_pid.contains_key(&0x10));
        assert_eq!(r.total_buffered, 0);
        assert_eq!(r.total_buffered, live_sum(&r));

        // Last arm: same shape, terminating cell.
        let mut r = AuCellReassembler::with_limits(1024, 50, 16);
        r.process_cell(0x20, hdr(CellFragmentIndication::First, 0), &[0xCC; 40]);
        let out = r.process_cell(0x20, hdr(CellFragmentIndication::Last, 1), &[0xDD; 40]);
        assert!(matches!(
            out,
            ReassembleOutcome::Failure {
                reason: MultiCellAuReason::OverflowTotal,
                ..
            }
        ));
        assert!(!r.per_pid.contains_key(&0x20));
        assert_eq!(r.total_buffered, 0);
        assert_eq!(r.total_buffered, live_sum(&r));
    }

    /// `reset_all` zeroes the aggregate counter too.
    #[test]
    fn reset_all_zeroes_aggregate_counter() {
        let mut r = AuCellReassembler::with_limits(1024, 1 << 20, 1000);
        r.process_cell(0x100, hdr(CellFragmentIndication::First, 0), &[0xAA; 3]);
        r.process_cell(0x200, hdr(CellFragmentIndication::First, 0), &[0xBB; 3]);
        assert!(r.total_buffered > 0);
        r.reset_all();
        assert_eq!(r.total_buffered, 0);
    }
}
