//! PSI + PCR scheduling for `Muxer`: one delivery clock per program.
//!
//! A program's clock is the base of its last PCR. Every push moves it to
//! the push's decode time minus `pcr_lead_ms`, never backward, so the PCR a
//! receiver reads is always behind the decode time of the access unit that
//! follows it. `Muxer::plan_clock` works out what one push does to the
//! clock without touching state; `reserve_preamble` checks the result against
//! the buffer and only then commits it, so a refused push changes nothing.
//!
//! PSI rides the same clock: one PAT + one PMT per live program whenever the
//! clock has advanced `psi_interval_ms` since the last set.

use super::Muxer;
use super::psi::{PmtStreamEntry, write_pat_packet, write_pmt_packet};
use super::state::pmt_stream_type_byte;
use super::ts::{AdaptationField, write_pcr_only_packet};
use crate::error::MuxError;
use crate::mpegts::common::{Pcr27mhz, Pts90khz, pts_diff_33bit};
use alloc::vec::Vec;

/// One second, the longest an H.222.0 decoder buffer holds an access unit.
/// A push on the PCR PID more than this far behind the previous one cannot
/// be reordering, so it starts a new time base; a push on another PID more
/// than this far ahead of the PCR stream means that stream has gone silent.
pub(super) const TIMELINE_RESTART_90KHZ: i64 = 90_000;

const MASK_33: u64 = (1 << 33) - 1;

/// What the program clock decided for the push that was just reserved.
pub(super) struct ClockPlan {
    /// PCR for the push's first TS packet; `None` when that packet carries
    /// none.
    pub(super) pcr: Option<Pcr27mhz>,
    /// The first packet opens a new time base.
    pub(super) discontinuity: bool,
    /// The program clock after this push: 33-bit, 90 kHz.
    clock: u64,
}

impl ClockPlan {
    /// Adaptation field for the push's first TS packet.
    pub(super) fn first_af(&self, random_access: bool) -> AdaptationField {
        AdaptationField {
            random_access,
            pcr: self.pcr,
            discontinuity: self.discontinuity,
        }
    }
}

/// The effect of one push (or tick) on a program's clock, not yet applied.
struct ClockStep {
    /// The program clock after the push.
    clock: u64,
    /// The push's decode time, masked to 33 bits.
    decode: u64,
    psi: bool,
    /// A PCR-only packet goes out on the PCR PID before the payload.
    pcr_only: bool,
    /// The push's first packet carries the PCR.
    first_pcr: bool,
    /// The PCR of this push opens a new time base.
    restart: bool,
    /// `decode` becomes the PCR stream's latest timestamp.
    on_pcr_stream: bool,
}

impl Muxer {
    /// Work out what a push with decode time `decode_ts` on `pid` does to
    /// program `prog_idx`'s clock. `pid` is `None` for a tick,
    /// which stands in for a push on the PCR PID. `force_pcr` asks for a PCR
    /// on the first packet even when none is due (a key frame).
    #[inline(always)]
    fn plan_clock(
        &self,
        prog_idx: usize,
        decode_ts: Pts90khz,
        pid: Option<u16>,
        force_pcr: bool,
    ) -> ClockStep {
        let slot = &self.slots[prog_idx];
        let tick = pid.is_none();
        let on_pcr = pid.is_none_or(|p| p == slot.pcr_pid);
        let decode = decode_ts.masked_33bit();
        let since_pcr_stream = slot.pcr_stream_last.map(|v| pts_diff_33bit(decode, v));

        // A new time base needs two PCRs on the current one first
        // (H.222.0 §2.7.2). Until then the step back is held like a
        // reorder, but with a PCR, so the next push finds the two it needs.
        let behind = on_pcr && since_pcr_stream.is_some_and(|d| d < -TIMELINE_RESTART_90KHZ);
        let restart = behind && slot.pcrs_on_time_base >= 2;
        let refused = behind && !restart;
        // While the PCR stream is pushing, another PID's timestamps do not
        // move the clock: metadata stamped at capture can run ahead of
        // encoder-delayed video, and a PCR taken from it would pass the
        // video's decode time.
        let live = !on_pcr && since_pcr_stream.is_some_and(|d| d <= TIMELINE_RESTART_90KHZ);

        let candidate = (decode + (1 << 33) - self.pcr_lead_90khz) & MASK_33;
        let (new_clock, pcr_due) = match slot.clock {
            Some(clock) if live => (clock, false),
            Some(clock) if !restart => {
                // Never backward: a candidate behind the clock holds it.
                let advance = pts_diff_33bit(candidate, clock);
                if advance < 0 {
                    (clock, refused)
                } else {
                    (candidate, refused || advance >= self.pcr_interval_90khz)
                }
            }
            _ => (candidate, true),
        };
        let psi = restart
            || slot
                .psi_last
                .is_none_or(|last| pts_diff_33bit(new_clock, last) >= self.psi_interval_90khz);
        ClockStep {
            clock: new_clock,
            decode,
            psi,
            pcr_only: pcr_due && (tick || (!on_pcr && !live)),
            first_pcr: on_pcr && !tick && (pcr_due || force_pcr),
            restart,
            on_pcr_stream: on_pcr && !refused,
        }
    }

    /// `BufferFull` unless the PSI and PCR-only packets of `step` plus
    /// `payload_pkts` fit in the queue.
    fn reserve(&self, step: &ClockStep, payload_pkts: usize) -> Result<(), MuxError> {
        // 1 PAT + one PMT per live program, as `emit_psi` writes.
        let psi_packets = if step.psi {
            1 + self.live_slots().count()
        } else {
            0
        };
        if self.queue.len() + psi_packets + usize::from(step.pcr_only) + payload_pkts
            > self.config.buffer_packets
        {
            return Err(MuxError::BufferFull {
                capacity_packets: self.config.buffer_packets as u64,
            });
        }
        Ok(())
    }

    /// Advance the program clock for one push: check buffer headroom, then
    /// queue the PSI and PCR-only packets that are due.
    ///
    /// `decode_ts` is the push's decode time (DTS when the push has one,
    /// else PTS), `pid` the elementary-stream PID being pushed,
    /// `payload_pkts` the TS-packet count of the PES (from `ts_packets_for`),
    /// and `force_pcr` asks for a PCR on the first packet even when none is
    /// due. Returns what that first packet must carry. On `BufferFull`
    /// nothing has changed.
    pub(super) fn reserve_preamble(
        &mut self,
        prog_idx: usize,
        decode_ts: Pts90khz,
        pid: u16,
        payload_pkts: usize,
        force_pcr: bool,
    ) -> Result<ClockPlan, MuxError> {
        let step = self.plan_clock(prog_idx, decode_ts, Some(pid), force_pcr);
        self.reserve(&step, payload_pkts)?;
        Ok(self.commit_clock(prog_idx, &step))
    }

    /// Advance program `program_number`'s clock to `now` without pushing
    /// anything: queue the PSI and the PCR-only packet that are due.
    ///
    /// A push writes a PCR only on its own first packet, so a program whose
    /// PCR PID carries fewer than ten access units a second cannot meet the
    /// 100 ms PCR interval of H.222.0 §2.7.2 from pushes alone. Such a
    /// source calls `tick` between pushes, at least every
    /// [`MuxerConfig::pcr_interval_ms`] of stream time. It also keeps PSI
    /// repeating while nothing is pushed.
    ///
    /// `now` is a time on the PCR stream's decode timeline: the decode time
    /// an access unit pushed at this moment would have. The PCR written is
    /// `now` minus [`MuxerConfig::pcr_lead_ms`]. A tick counts as activity
    /// on the PCR stream: while ticks arrive, pushes on the program's other
    /// PIDs do not move the clock, and a `now` more than a second behind the
    /// previous tick or PCR-PID push starts a new time base, as such a push
    /// would. A tick that finds nothing due queues nothing.
    ///
    /// # Errors
    /// - [`MuxError::ProgramNotFound`] if no live program has that number.
    /// - [`MuxError::BufferFull`] if the packets do not fit; nothing changed.
    ///
    /// [`MuxerConfig::pcr_interval_ms`]: super::MuxerConfig::pcr_interval_ms
    /// [`MuxerConfig::pcr_lead_ms`]: super::MuxerConfig::pcr_lead_ms
    pub fn tick(&mut self, program_number: u16, now: Pts90khz) -> Result<(), MuxError> {
        let prog_idx = self
            .slots
            .iter()
            .position(|s| s.program_number() == Some(program_number))
            .ok_or(MuxError::ProgramNotFound { program_number })?;
        let step = self.plan_clock(prog_idx, now, None, false);
        self.reserve(&step, 0)?;
        self.commit_clock(prog_idx, &step);
        Ok(())
    }

    /// Apply `step`: queue its PSI and PCR-only packets and move the clock.
    #[inline(always)]
    fn commit_clock(&mut self, prog_idx: usize, step: &ClockStep) -> ClockPlan {
        if step.psi {
            self.emit_psi(step.clock);
        }
        let pcr = Pcr27mhz::new(step.clock * 300);
        if step.pcr_only {
            // Keeps the PCR PID within `pcr_interval_ms` when its own stream
            // is not pushing (KLV-only traffic on a video-PCR program).
            let mut pkt = [0u8; 188];
            write_pcr_only_packet(
                &mut pkt,
                self.slots[prog_idx].pcr_pid,
                pcr,
                step.restart,
                &self.counters,
            );
            self.queue.push_back(pkt);
        }
        let slot = &mut self.slots[prog_idx];
        if step.restart {
            slot.pcrs_on_time_base = 0;
        }
        if step.pcr_only || step.first_pcr {
            slot.clock = Some(step.clock);
            slot.pcrs_on_time_base = slot.pcrs_on_time_base.saturating_add(1);
        }
        if step.on_pcr_stream {
            slot.pcr_stream_last = Some(step.decode);
        }
        ClockPlan {
            pcr: step.first_pcr.then_some(pcr),
            discontinuity: step.restart && step.first_pcr,
            clock: step.clock,
        }
    }

    /// Count a push whose PES carries `pts` on a PID other than the PCR PID
    /// when the program clock has already passed that PTS: the unit reaches
    /// a receiver after the time it was meant for. It is muxed all the same.
    pub(super) fn count_if_late(
        &mut self,
        prog_idx: usize,
        pid: u16,
        pts: Pts90khz,
        plan: &ClockPlan,
    ) {
        if self.slots[prog_idx].pcr_pid != pid && pts_diff_33bit(pts.masked_33bit(), plan.clock) < 0
        {
            self.late_metadata_pushes += 1;
        }
    }

    /// Queue one PAT and one PMT per live program, and record `clock` as the
    /// time of every program's last PSI.
    fn emit_psi(&mut self, clock: u64) {
        // Emit one PAT that lists all live programs, then one PMT per live
        // program.
        // The PAT is emitted on every PSI tick regardless of which program
        // triggered the tick, so all programs' state is always visible to
        // receivers after a single PSI interval.
        let mut pat = [0u8; 188];
        let pat_entries = self.pat_entries();
        write_pat_packet(&mut pat, &pat_entries, self.pat_version, &mut self.counters);
        self.queue.push_back(pat);

        // One PMT per live program — iterate the full slot set so every live
        // program gets a fresh PMT on the tick (not just the triggering one).
        for slot in &self.slots {
            let Some(prog) = &slot.live else {
                continue;
            };
            let mut entries: Vec<PmtStreamEntry> = Vec::with_capacity(prog.streams.len());
            for (i, spec) in prog.streams.iter().enumerate() {
                let stream_type = pmt_stream_type_byte(spec);
                entries.push(PmtStreamEntry {
                    stream_type,
                    elementary_pid: spec.pid(),
                    descriptors: &slot.pmt_descriptor_cache[i],
                });
            }

            let mut pmt = [0u8; 188];
            write_pmt_packet(
                &mut pmt,
                prog,
                slot.pcr_pid,
                &entries,
                slot.pmt_version,
                &mut self.counters,
            )
            .expect("validated MuxerConfig must produce single-section PMT");
            self.queue.push_back(pmt);
        }

        // Every program's PMT just went out, so every slot is stamped: a
        // slot left at `None` would read as due on its next push and re-send
        // the whole set.
        for slot in self.slots.iter_mut() {
            slot.psi_last = Some(clock);
        }
    }
}
