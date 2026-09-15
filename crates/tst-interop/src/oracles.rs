//! Wire-level, demuxer-independent oracles (spec §6.3). Every failure
//! string starts with its verdict name so mutation tests can assert on
//! it. Each oracle consumes the raw [`WireSummary`] (never tst-core's
//! `Demuxer` output — see `rawts`'s own module doc for why that
//! independence is the point) alongside the demux-side per-program
//! [`ProgramCounts`], both parameterized by a profile's
//! [`Invariants`].

use std::collections::BTreeMap;

use tst_core::mpegts::mux::Av1CarriageMode;

use crate::profiles::{Invariants, KlvMode, Profile};
use crate::rawts::WireSummary;
use crate::verify::{self, ProgramCounts, VerifyMode};

const PTS_WRAP: u64 = 1 << 33;
const PCR_LOWER_SLACK_MS: f64 = 0.5;
const PCR_UPPER_SLACK_MS: f64 = 1.0;
const AUDIO_CADENCE_TOLERANCE: f64 = 0.10;
const AUDIO_PTS_STEP_TOLERANCE: f64 = 0.05;
const AAC_SAMPLES_PER_FRAME: f64 = 1024.0;

/// Run all seven wire-level oracles and concatenate their failures.
/// `explained` is how many demuxed events a corruption log and the
/// capture's own discontinuity/non-conformance tallies account for —
/// see `wire_vs_demux` (private; `--document-private-items` renders it).
// Each parameter is a distinct fact one of the seven oracles needs, all
// of them already owned by the single caller (`verify::Tally::finish`) —
// a struct here would only move the same list one line up.
#[allow(clippy::too_many_arguments)]
pub fn check(
    p: &Profile,
    inv: &Invariants,
    wire: &WireSummary,
    per_program: &BTreeMap<u16, ProgramCounts>,
    seconds: f64,
    slack: f64,
    mode: VerifyMode,
    explained: u64,
) -> Vec<String> {
    let mut f = Vec::new();
    f.extend(program_accounting(inv, wire, per_program, seconds, slack));
    f.extend(audio(inv, wire, seconds));
    f.extend(pcr_interval(inv, wire, mode));
    f.extend(av1_carriage(inv, wire));
    f.extend(pts_wrap(p, inv, wire, seconds));
    f.extend(pmt_streams(p, inv, wire));
    f.extend(wire_vs_demux(p, inv, wire, per_program, mode, explained));
    f
}

/// Oracle 1: per-program video AU / KLV record counts each clear
/// `slack` of nominal, and every program's media PIDs actually carry
/// packets on the wire.
fn program_accounting(
    inv: &Invariants,
    wire: &WireSummary,
    per_program: &BTreeMap<u16, ProgramCounts>,
    seconds: f64,
    slack: f64,
) -> Vec<String> {
    let mut f = Vec::new();
    let min_video = verify::min_count(inv.min_video_aus_per_sec, seconds, slack);
    let min_klv = verify::min_count(inv.min_klv_per_sec, seconds, slack);
    for ep in &inv.programs {
        let n = ep.program_number;
        let c = per_program.get(&n).copied().unwrap_or_default();
        if c.video_aus < min_video {
            f.push(format!(
                "program_{n}_video_floor: got {} AUs, want >= {min_video}",
                c.video_aus
            ));
        }
        if c.klv_records < min_klv {
            f.push(format!(
                "program_{n}_klv_floor: got {} records, want >= {min_klv}",
                c.klv_records
            ));
        }
        let pk = |pid: u16| wire.packets_per_pid.get(&pid).copied().unwrap_or(0);
        if pk(ep.video_pid) == 0 || pk(ep.klv_pid) == 0 {
            f.push(format!(
                "program_{n}_wire_media: video PID {} {} pkts, KLV PID {} {} pkts",
                ep.video_pid,
                pk(ep.video_pid),
                ep.klv_pid,
                pk(ep.klv_pid)
            ));
        }
    }
    f
}

/// Oracle 2: on the audio PID, the raw PES payload starts with an ADTS
/// AAC frame at the expected sample rate; the frame count and median
/// PTS step match that rate's real cadence (1024 samples/frame).
fn audio(inv: &Invariants, wire: &WireSummary, seconds: f64) -> Vec<String> {
    // `audio_sample_rate_hz` and `programs[0].audio_pid` are both
    // `p.audio.then_some(..)` in `profiles::invariants` — always Some
    // together or None together — so one combined gate covers both
    // instead of two sequential (and, given that coupling, effectively
    // redundant) early returns.
    let (Some(expected_rate), Some(pid)) = (inv.audio_sample_rate_hz, inv.programs[0].audio_pid)
    else {
        return Vec::new();
    };
    let mut f = Vec::new();
    match wire.pes.get(&pid).and_then(|s| s.first_payload_prefix) {
        // ADTS syncword 0xFFF (ISO/IEC 13818-7 §6.2.1) plus `layer == 00`
        // (byte 1, bits 2-1): ADTS AAC always sets layer to 0, so this is
        // what distinguishes a real AAC-ADTS header from an MPEG-1/2
        // Layer II/III frame, which shares the same 0xFFF syncword and
        // ID bit but sets a nonzero layer — without this check an
        // MPEG-audio frame with a coincidentally-matching sample-rate
        // index byte would misread as a passing ADTS header. Profile
        // (ID) bit is byte 1 bit 3, not checked here (both MPEG-2 and
        // MPEG-4 ADTS are accepted).
        Some(pfx) if pfx[0] == 0xFF && pfx[1] & 0xF0 == 0xF0 && pfx[1] & 0x06 == 0 => {
            let rate_index = (pfx[2] >> 2) & 0x0F;
            let rate = adts_sample_rate(rate_index);
            if rate != Some(expected_rate) {
                f.push(format!(
                    "audio_codec_adts: sample_rate_index {rate_index} = {rate:?} Hz, want {expected_rate}"
                ));
            }
        }
        other => f.push(format!(
            "audio_codec_adts: first audio PES payload {other:?} is not an ADTS frame"
        )),
    }
    // …and the same check over every LATER PES on the PID: an ADTS
    // syncword is constant, so one that stops looking like the first is
    // damage the first-PES check alone would sail past.
    if let Some(n) = wire
        .pes
        .get(&pid)
        .map(|s| s.prefix_mismatches)
        .filter(|&n| n > 0)
    {
        f.push(format!(
            "audio_codec_adts: {n} later PES payload(s) on PID {pid} do not start like the first"
        ));
    }
    let frames = wire.pts.get(&pid).map_or(0, |s| s.count()) as f64;
    let expected_frames = seconds * expected_rate as f64 / AAC_SAMPLES_PER_FRAME;
    if (frames - expected_frames).abs() > expected_frames * AUDIO_CADENCE_TOLERANCE {
        f.push(format!(
            "audio_cadence: {frames} frames, want {expected_frames:.0} ± {:.0}%",
            AUDIO_CADENCE_TOLERANCE * 100.0
        ));
    }
    // The median of the positive consecutive PTS steps, over every step
    // on an offline capture and over a bounded uniform sample of them on
    // a live one (`rawts::Retention`) — either way the same statistic,
    // against the same ±tolerance.
    if let Some(median) = wire.pts.get(&pid).and_then(|s| s.positive_step_median()) {
        let want = AAC_SAMPLES_PER_FRAME * 90_000.0 / expected_rate as f64;
        if (median - want).abs() > want * AUDIO_PTS_STEP_TOLERANCE {
            f.push(format!(
                "audio_pts_step: median {median:.0} ticks, want {want:.0} ± {:.0}%",
                AUDIO_PTS_STEP_TOLERANCE * 100.0
            ));
        }
    }
    f
}

fn adts_sample_rate(index: u8) -> Option<u32> {
    [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ]
    .get(usize::from(index))
    .copied()
}

/// Oracle 3: PCR cadence — the MEDIAN interval on each program's PCR PID
/// is bounded below by the configured interval (minus a small
/// tolerance); the upper bound is the configured interval plus one
/// frame period (plus a small tolerance), applied to every interval in
/// `Strict` and to the median in `Lossy` (loss widens gaps, never
/// narrows them).
///
/// The lower bound checks the median, not the raw minimum: tst-core's
/// muxer legitimately emits standalone PCR-only adaptation-field
/// packets when a push to some OTHER (non-PCR) PID finds PCR overdue
/// (`crates/tst-core/src/mpegts/mux/scheduling.rs`'s `pcr_only_due` /
/// `maybe_emit_pcr_only`) — a real mechanism, not a bug, that keeps PCR
/// within its H.222.0 Annex D ceiling when the caller pushes mostly to
/// non-PCR PIDs. This can make individual intervals shorter than
/// configured (the `audio` profile's ~46.9 Hz cadence exceeds the PCR
/// PID's own natural push rate and triggers it repeatedly) — a
/// legitimate minority of short intervals, never the majority, so the
/// median stays a reliable signal even when the raw minimum isn't. `min`
/// is still reported in the failure detail for a human reader, purely
/// informational.
fn pcr_interval(inv: &Invariants, wire: &WireSummary, mode: VerifyMode) -> Vec<String> {
    let mut f = Vec::new();
    let lower = inv.pcr_interval_ms as f64 - PCR_LOWER_SLACK_MS;
    let upper = inv.pcr_interval_ms as f64 + inv.frame_period_ms + PCR_UPPER_SLACK_MS;
    for ep in &inv.programs {
        // A missing program here is `pmt_streams`'s job to report (as
        // `pmt_missing_program_<n>`) — skip it silently rather than
        // duplicate that finding under a second, unrelated verdict name.
        let Some(prog) = wire.programs.get(&ep.program_number) else {
            continue;
        };
        let Some(pcrs) = wire.pcr.get(&prog.pcr_pid) else {
            f.push(format!(
                "pcr_interval: no PCR on program {}'s PCR PID {}",
                ep.program_number, prog.pcr_pid
            ));
            continue;
        };
        // PCR's base field is the same 33-bit modulus as PES PTS (ITU-T
        // H.222.0 V9 §2.4.3.5 defines both the PCR base and the PTS/DTS
        // fields as 33-bit binary counters at 90 kHz) — `pts-rollover`'s
        // capture wraps it too, so `rawts` measures every interval as a
        // wrap-aware forward distance (mod 2^33), not a plain
        // subtraction.
        //
        // A single measurable interval (2 PCR samples) is already enough
        // to check against the bounds below — min == median == max, a
        // degenerate but valid sample, not a reason to skip the check.
        // Only zero intervals (0 or 1 PCR samples total) is insufficient.
        let (Some(min_ticks), Some(max_ticks), Some(median_ticks)) = (
            pcrs.forward_min(),
            pcrs.forward_max(),
            pcrs.forward_median(),
        ) else {
            f.push(format!(
                "pcr_interval: only {} interval(s) on PID {} ({} PCR sample(s)), need >= 1",
                pcrs.intervals(),
                prog.pcr_pid,
                pcrs.count()
            ));
            continue;
        };
        let min = min_ticks as f64 / 90.0;
        // `forward_max` is a running maximum, exact in both retention
        // modes — so `Strict`'s "every interval within bounds" reading
        // below stays exact on a live capture too.
        let max = max_ticks as f64 / 90.0;
        // The upper-middle element for an even count (not an
        // average-of-two) — deliberate: an average could land BELOW
        // `lower` even when no individual interval actually did (e.g.
        // one very short catch-up interval dragging a two-value average
        // down), which would defeat the whole point of gating on the
        // median instead of the min.
        let median = median_ticks / 90.0;
        let upper_observed = match mode {
            VerifyMode::Strict => max,
            VerifyMode::Lossy => median,
        };
        if median < lower || upper_observed > upper {
            f.push(format!(
                "pcr_interval: PID {} min {min:.3} / median {median:.3} / max {max:.3} ms, want median >= {lower:.1} and {} <= {upper:.1} ({} mode), for a configured {} ms",
                prog.pcr_pid,
                if mode == VerifyMode::Strict { "max" } else { "median" },
                if mode == VerifyMode::Strict {
                    "every interval, Strict"
                } else {
                    "median, Lossy"
                },
                inv.pcr_interval_ms
            ));
        }
    }
    f
}

/// Oracle 4: on the AV1 PID, mode B (`Mpeg2TsBinding`) carries PES
/// `stream_id=0xBD` with `ts_open_bitstream_unit` framing
/// (`00 00 01` prefix); mode A (`InteropRawObu`) carries `stream_id=
/// 0xE0` with a raw OBU header (forbidden bit clear, `obu_type` a
/// temporal delimiter / sequence header / frame header).
fn av1_carriage(inv: &Invariants, wire: &WireSummary) -> Vec<String> {
    let Some(mode) = inv.av1_mode else {
        return Vec::new();
    };
    let pid = inv.programs[0].video_pid;
    let Some(shape) = wire.pes.get(&pid) else {
        return vec![format!("av1_carriage_wire: no PES on video PID {pid}")];
    };
    let pfx = shape.first_payload_prefix.unwrap_or([0; 4]);
    let ok = match mode {
        Av1CarriageMode::Mpeg2TsBinding => {
            shape.stream_ids.iter().all(|&s| s == 0xBD) && pfx[..3] == [0, 0, 1]
        }
        Av1CarriageMode::InteropRawObu => {
            let obu_type = (pfx[0] >> 3) & 0x0F;
            shape.stream_ids.iter().all(|&s| s == 0xE0)
                && pfx[0] & 0x80 == 0
                && matches!(obu_type, 1 | 2 | 6)
        }
        _ => false,
    };
    let mut f = Vec::new();
    if !ok {
        f.push(format!(
            "av1_carriage_wire: mode {mode:?}, stream_ids {:?}, payload prefix {:02x?}",
            shape.stream_ids, pfx
        ));
    }
    // The prefix check above reads the FIRST PES only; this holds every
    // later one to the same opening bytes (`00 00` for the binding's
    // `ts_open_bitstream_unit`, `12 00` for a raw-OBU temporal
    // delimiter), so carriage that degrades mid-capture is caught too.
    if shape.prefix_mismatches > 0 {
        f.push(format!(
            "av1_carriage_wire: {} later PES payload(s) on PID {pid} do not start like the first",
            shape.prefix_mismatches
        ));
    }
    f
}

/// Oracle 5: `pts-rollover` must show at least one raw-PTS wrap
/// (consecutive-PTS decrease of more than half the 33-bit modulus) on
/// the video PID when its window crosses 2^33; every other profile must
/// show none.
fn pts_wrap(p: &Profile, inv: &Invariants, wire: &WireSummary, seconds: f64) -> Vec<String> {
    let expect_wrap = p.start_pts_ticks + (seconds * 90_000.0) as u64 > PTS_WRAP;
    let pid = inv.programs[0].video_pid;
    // Counted exactly by the reader as each PTS arrives, under either
    // retention mode: a rollover is a property of one consecutive pair,
    // not of the series a median is drawn from.
    let decreases = wire.pts.get(&pid).map_or(0, |s| s.wraps());
    match (expect_wrap, decreases) {
        (true, 0) => vec![format!(
            "pts_wrap_observed: window {seconds}s from start {} crosses 2^33 but no raw PTS wrap was seen on PID {pid}",
            p.start_pts_ticks
        )],
        (false, n) if n > 0 => vec![format!(
            "pts_wrap_unexpected: {n} raw PTS wrap(s) on PID {pid} in a window that never reaches 2^33"
        )],
        _ => Vec::new(),
    }
}

/// Oracle 6: every PMT ES entry's `stream_type` matches
/// `Invariants::{video,klv}_stream_type` (audio always `0x0F`); the KLV
/// PID carries a `KLVA` registration descriptor (plus `metadata`
/// (0x26) + `metadata_STD` (0x27) descriptors for sync carriage); the
/// AV1 PID carries an `AV01` registration descriptor.
fn pmt_streams(p: &Profile, inv: &Invariants, wire: &WireSummary) -> Vec<String> {
    let mut f = Vec::new();
    for ep in &inv.programs {
        let Some(prog) = wire.programs.get(&ep.program_number) else {
            f.push(format!(
                "pmt_missing_program_{}: no PMT seen",
                ep.program_number
            ));
            continue;
        };
        let find = |pid: u16| prog.streams.iter().find(|s| s.pid == pid);
        // video
        match find(ep.video_pid) {
            None => f.push(format!(
                "pmt_stream_type_{}: video PID absent from PMT",
                ep.video_pid
            )),
            Some(s) => {
                if s.stream_type != inv.video_stream_type {
                    f.push(format!(
                        "pmt_stream_type_{}: 0x{:02x}, want 0x{:02x}",
                        ep.video_pid, s.stream_type, inv.video_stream_type
                    ));
                }
                if inv.av1_mode.is_some() && s.registration != Some(*b"AV01") {
                    f.push(format!(
                        "pmt_descriptor_{}: AV01 registration missing ({:?})",
                        ep.video_pid, s.registration
                    ));
                }
            }
        }
        // klv
        match find(ep.klv_pid) {
            None => f.push(format!(
                "pmt_stream_type_{}: KLV PID absent from PMT",
                ep.klv_pid
            )),
            Some(s) => {
                if s.stream_type != inv.klv_stream_type {
                    f.push(format!(
                        "pmt_stream_type_{}: 0x{:02x}, want 0x{:02x}",
                        ep.klv_pid, s.stream_type, inv.klv_stream_type
                    ));
                }
                if s.registration != Some(*b"KLVA") {
                    f.push(format!(
                        "pmt_descriptor_{}: KLVA registration missing ({:?})",
                        ep.klv_pid, s.registration
                    ));
                }
                if p.klv == KlvMode::Sync
                    && !(s.descriptor_tags.contains(&0x26) && s.descriptor_tags.contains(&0x27))
                {
                    f.push(format!(
                        "pmt_descriptor_{}: sync KLV needs metadata (0x26) + metadata_STD (0x27) descriptors, got {:02x?}",
                        ep.klv_pid, s.descriptor_tags
                    ));
                }
            }
        }
        if let Some(apid) = ep.audio_pid {
            match find(apid) {
                Some(s) if s.stream_type == 0x0F => {}
                Some(s) => f.push(format!(
                    "pmt_stream_type_{apid}: 0x{:02x}, want 0x0f (AAC ADTS)",
                    s.stream_type
                )),
                None => f.push(format!("pmt_stream_type_{apid}: audio PID absent from PMT")),
            }
        }
    }
    f
}

/// Oracle 7 (deep review #4 CORR-07, Q10): per media PID, the demuxer's
/// event count against the raw reader's independent PES-start count.
///
/// [`crate::verify::NOMINAL_COUNT_SLACK`] (70 % of nominal) is a floor
/// for truncated captures, not a count check: a demuxer silently
/// dropping one access unit in four clears it. This is the count check.
/// The wire side is [`WireSummary::pes_starts_per_pid`] (one per PES
/// start, duplicates excluded); the demux side is the per-program tally
/// of `Sample` / `Metadata` events, keyed by the program the profile
/// muxes each PID into — this crate's muxer emits exactly one PES per
/// video AU, KLV record and ADTS frame, and the demuxer emits exactly
/// one event per PES it accepts.
///
/// `demux >= wire - explained - boundary`, per PID:
///
/// - `explained` is what the capture itself accounts for: the attributed
///   injection count when a corruption log is attached (a truncated or
///   dropped PES-start packet costs the demuxer the access unit while
///   the wire may still show its start) plus, under `Lossy`, every
///   `Discontinuity` and `NonConformant` the capture recorded — each is
///   a place the demuxer legitimately gave up on a PES. Under `Strict`
///   (offline `verify`, `recv --strict` on a transparent cell) those
///   events are failures in their own right, not an excuse for a missing
///   access unit. Loss that produced NO event is exactly what this
///   oracle catches; `verify::Tally::finish` computes the term.
/// - `boundary` is what no demuxer can be held to at the EDGES of a
///   capture, independent of loss: everything that arrives before it has
///   acquired PAT + PMT, plus the one access unit still sitting in the
///   reassembler when the capture ends. The head half is not slack, it
///   is measured — dropping a baseline capture's opening PAT costs the
///   demuxer exactly 3 video AUs, 1 KLV record and 5 audio frames (one
///   `psi_interval_ms` at each PID's own rate) and produces NO event of
///   any kind, because until the tables arrive it does not know those
///   PIDs exist. A live receiver that joins mid-stream lands in the same
///   place, and so does a corruption tap damaging the opening tables
///   (packet 0 of a fresh mux is the PAT). Both halves are per-PID and
///   CONSTANT in the length of the capture, so a demuxer losing a fixed
///   fraction of access units is still caught on anything long enough to
///   matter.
///
/// Only the lower bound is checked: more events than PES starts would
/// be a duplication defect, not the silent-loss class this closes, and
/// hand-fed tallies in this crate's own tests routinely stack an extra
/// event on a generated wire.
fn wire_vs_demux(
    p: &Profile,
    inv: &Invariants,
    wire: &WireSummary,
    per_program: &BTreeMap<u16, ProgramCounts>,
    mode: VerifyMode,
    explained: u64,
) -> Vec<String> {
    let mut f = Vec::new();
    // One PSI repetition interval of units at `rate_hz`, plus the one
    // unflushed unit at teardown — see this function's doc comment.
    let boundary = |rate_hz: f64| -> u64 {
        (f64::from(p.psi_interval_ms) / 1000.0 * rate_hz).ceil() as u64 + 1
    };
    for ep in &inv.programs {
        let c = per_program
            .get(&ep.program_number)
            .copied()
            .unwrap_or_default();
        let mut pids = vec![
            (
                ep.video_pid,
                c.video_aus,
                "video AUs",
                f64::from(inv.min_video_aus_per_sec),
            ),
            (
                ep.klv_pid,
                c.klv_records,
                "KLV records",
                f64::from(inv.min_klv_per_sec),
            ),
        ];
        if let (Some(apid), Some(rate)) = (ep.audio_pid, inv.audio_sample_rate_hz) {
            pids.push((
                apid,
                c.audio_frames,
                "audio frames",
                f64::from(rate) / AAC_SAMPLES_PER_FRAME,
            ));
        }
        for (pid, demux, what, rate_hz) in pids {
            let wire_pes = wire.pes_starts_per_pid.get(&pid).copied().unwrap_or(0);
            let boundary = boundary(rate_hz);
            let floor = wire_pes.saturating_sub(explained + boundary);
            if demux < floor {
                f.push(format!(
                    "wire_vs_demux_{pid}: demuxer emitted {demux} {what} but the wire carries \
                     {wire_pes} PES start(s) on PID {pid} ({mode:?}: want >= {wire_pes} - \
                     {explained} explained - {boundary} boundary = {floor})"
                ));
            }
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::profiles;
    use crate::rawts::{self, PesShape, Program, Stream};

    fn stream(
        pid: u16,
        stream_type: u8,
        registration: Option<[u8; 4]>,
        descriptor_tags: &[u8],
    ) -> Stream {
        Stream {
            pid,
            stream_type,
            registration,
            descriptor_tags: descriptor_tags.to_vec(),
        }
    }

    fn program_counts(entries: &[(u16, u64, u64)]) -> BTreeMap<u16, ProgramCounts> {
        entries
            .iter()
            .map(|&(pn, video, klv)| {
                (
                    pn,
                    ProgramCounts {
                        video_aus: video,
                        klv_records: klv,
                        audio_frames: 0,
                    },
                )
            })
            .collect()
    }

    /// Two programs (`two-program`'s real PIDs), all four media PIDs
    /// carrying nonzero traffic — a wire the per-program floor checks
    /// alone can distinguish (no `_wire_media` noise).
    fn two_program_wire() -> WireSummary {
        WireSummary {
            packets_per_pid: BTreeMap::from([
                (0x1011, 100),
                (0x1031, 100),
                (0x1111, 100),
                (0x1131, 100),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn program_accounting_fails_when_program_2_has_no_media() {
        let inv = profiles::invariants(profiles::by_name("two-program").unwrap());
        let wire = two_program_wire();
        let seconds = 3.0;
        let slack = crate::verify::NOMINAL_COUNT_SLACK;

        let empty_prog2 = program_counts(&[(1, 90, 30), (2, 0, 0)]);
        let f = program_accounting(&inv, &wire, &empty_prog2, seconds, slack);
        assert!(
            f.iter().any(|s| s.starts_with("program_2_video_floor")),
            "{f:?}"
        );
        assert!(
            f.iter().any(|s| s.starts_with("program_2_klv_floor")),
            "{f:?}"
        );

        let healthy = program_counts(&[(1, 90, 30), (2, 90, 30)]);
        let f2 = program_accounting(&inv, &wire, &healthy, seconds, slack);
        assert!(f2.is_empty(), "{f2:?}");
    }

    #[test]
    fn program_wire_media_fails_when_program_2_pids_carry_no_packets() {
        let inv = profiles::invariants(profiles::by_name("two-program").unwrap());
        let mut wire = two_program_wire();
        wire.packets_per_pid.remove(&0x1111);
        let counts = program_counts(&[(1, 90, 30), (2, 90, 30)]);
        let f = program_accounting(
            &inv,
            &wire,
            &counts,
            3.0,
            crate::verify::NOMINAL_COUNT_SLACK,
        );
        assert!(
            f.iter().any(|s| s.starts_with("program_2_wire_media")),
            "{f:?}"
        );
    }

    fn audio_wire(prefix: [u8; 4], pts: Vec<u64>) -> WireSummary {
        WireSummary {
            pes: BTreeMap::from([(
                0x1041,
                PesShape {
                    stream_ids: BTreeSet::new(),
                    first_payload_prefix: Some(prefix),
                    prefix_mismatches: 0,
                },
            )]),
            pts: BTreeMap::from([(
                0x1041,
                rawts::TimestampSeries::from_values(rawts::Retention::Full, 0x1041, &pts),
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn audio_oracles_check_adts_cadence_and_pts_step() {
        let inv = profiles::invariants(profiles::by_name("audio").unwrap());
        let good_prefix = [0xFF, 0xF9, 0x0C, 0x00];
        let good_pts: Vec<u64> = (0..141).map(|i| i as u64 * 1920).collect();

        assert!(audio(&inv, &audio_wire(good_prefix, good_pts.clone()), 3.0).is_empty());

        let one_frame = audio(&inv, &audio_wire(good_prefix, vec![0]), 3.0);
        assert!(
            one_frame.iter().any(|f| f.starts_with("audio_cadence")),
            "{one_frame:?}"
        );

        let bad_codec = audio(&inv, &audio_wire([0x00, 0, 0, 0], good_pts.clone()), 3.0);
        assert!(
            bad_codec.iter().any(|f| f.starts_with("audio_codec_adts")),
            "{bad_codec:?}"
        );

        let bad_step_pts: Vec<u64> = (0..141).map(|i| i as u64 * 3000).collect();
        let bad_step = audio(&inv, &audio_wire(good_prefix, bad_step_pts), 3.0);
        assert!(
            bad_step.iter().any(|f| f.starts_with("audio_pts_step")),
            "{bad_step:?}"
        );
    }

    #[test]
    fn audio_codec_adts_flags_rate_index_mismatch_and_rejects_non_aac_layer() {
        let inv = profiles::invariants(profiles::by_name("audio").unwrap());
        let good_pts: Vec<u64> = (0..141).map(|i| i as u64 * 1920).collect();

        // Valid ADTS syncword + layer, but sample_rate_index 4 = 44.1kHz,
        // not the `audio` profile's expected 48kHz (index 3).
        let wrong_rate = audio(
            &inv,
            &audio_wire([0xFF, 0xF9, 0x10, 0x80], good_pts.clone()),
            3.0,
        );
        assert!(
            wrong_rate
                .iter()
                .any(|f| f.starts_with("audio_codec_adts") && f.contains("44100")),
            "{wrong_rate:?}"
        );

        // MPEG-1 Layer II shares the 0xFFF syncword and can even share a
        // valid-looking sample-rate-index byte — only the layer bits
        // (00 vs 10 here) tell them apart, so this must still fail.
        let mpeg1_layer2 = audio(
            &inv,
            &audio_wire([0xFF, 0xFD, 0x50, 0x80], good_pts.clone()),
            3.0,
        );
        assert!(
            mpeg1_layer2
                .iter()
                .any(|f| f.starts_with("audio_codec_adts")),
            "{mpeg1_layer2:?}"
        );

        // A real ADTS header (layer 00) at the correct rate must pass.
        let real_adts = audio(&inv, &audio_wire([0xFF, 0xF9, 0x4C, 0x80], good_pts), 3.0);
        assert!(
            !real_adts.iter().any(|f| f.starts_with("audio_codec_adts")),
            "{real_adts:?}"
        );
    }

    fn wire_with_pcr(pid: u16, ticks: &[u64]) -> WireSummary {
        WireSummary {
            pcr: BTreeMap::from([(
                pid,
                rawts::TimestampSeries::from_values(rawts::Retention::Full, pid, ticks),
            )]),
            programs: BTreeMap::from([(
                1,
                Program {
                    program_number: 1,
                    pmt_pid: 0x1000,
                    pcr_pid: pid,
                    streams: Vec::new(),
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn pcr_interval_is_bounded_below_by_config_and_above_by_config_plus_frame_period() {
        let tight = profiles::invariants(profiles::by_name("pcr-tight").unwrap());
        let pass = wire_with_pcr(0x1011, &[0, 3000, 6000]); // 33.333ms x2, median 33.333
        assert!(pcr_interval(&tight, &pass, VerifyMode::Strict).is_empty());
        let fail = wire_with_pcr(0x1011, &[0, 6000, 12000]); // 66.667ms x2, median 66.667 > upper 35.3
        let f = pcr_interval(&tight, &fail, VerifyMode::Strict);
        assert!(f.iter().any(|s| s.starts_with("pcr_interval")), "{f:?}");

        let sparse = profiles::invariants(profiles::by_name("pcr-sparse").unwrap());
        // Median 66.667ms < the 99.5ms lower bound — every interval here
        // is short, so both the min and the median are below bound.
        let median_too_low = wire_with_pcr(0x1011, &[0, 6000, 12000]);
        let f = pcr_interval(&sparse, &median_too_low, VerifyMode::Strict);
        assert!(f.iter().any(|s| s.starts_with("pcr_interval")), "{f:?}");
        let pass2 = wire_with_pcr(0x1011, &[0, 9000, 18000]); // 100ms x2, median 100
        assert!(pcr_interval(&sparse, &pass2, VerifyMode::Strict).is_empty());

        // A FEW short intervals (each individually below the lower
        // bound) alongside a majority at the configured cadence must
        // still PASS — this is exactly the `audio` profile's real wire
        // shape: tst-core's muxer legitimately shortens a minority of
        // intervals via PCR-only catch-up packets (see this oracle's own
        // doc comment) while the MEDIAN stays on-cadence. Two of eight
        // intervals are 11.111ms (well under 99.5); the other six are
        // 100ms; sorted, the median (4th of 8, 0-indexed) lands on 100ms.
        let few_short_but_compliant_median = wire_with_pcr(
            0x1011,
            &[0, 9000, 10000, 19000, 20000, 29000, 38000, 47000, 56000],
        );
        assert!(
            pcr_interval(&sparse, &few_short_but_compliant_median, VerifyMode::Strict).is_empty()
        );
    }

    #[test]
    fn pcr_lossy_mode_uses_median_for_the_upper_bound() {
        let inv = profiles::invariants(profiles::by_name("baseline").unwrap());
        // Five 66.667ms intervals, one 133.333ms gap.
        let wire = wire_with_pcr(0x1011, &[0, 6000, 12000, 18000, 30000, 36000, 42000]);
        let strict = pcr_interval(&inv, &wire, VerifyMode::Strict);
        assert!(
            strict.iter().any(|s| s.starts_with("pcr_interval")),
            "{strict:?}"
        );
        assert!(pcr_interval(&inv, &wire, VerifyMode::Lossy).is_empty());
    }

    #[test]
    fn pcr_interval_reports_missing_pcr_and_insufficient_samples() {
        let inv = profiles::invariants(profiles::by_name("baseline").unwrap());

        // The program's PMT declares a PCR PID, but the wire never
        // actually carried a PCR on it.
        let no_pcr = WireSummary {
            programs: BTreeMap::from([(
                1,
                Program {
                    program_number: 1,
                    pmt_pid: 0x1000,
                    pcr_pid: 0x1011,
                    streams: Vec::new(),
                },
            )]),
            ..Default::default()
        };
        let f = pcr_interval(&inv, &no_pcr, VerifyMode::Strict);
        assert!(
            f.iter()
                .any(|s| s.starts_with("pcr_interval") && s.contains("no PCR")),
            "{f:?}"
        );

        // Exactly one PCR sample on the wire — zero intervals to measure.
        let one_pcr = wire_with_pcr(0x1011, &[0]);
        let f = pcr_interval(&inv, &one_pcr, VerifyMode::Strict);
        assert!(
            f.iter()
                .any(|s| s.starts_with("pcr_interval") && s.contains("interval")),
            "{f:?}"
        );

        // Exactly TWO PCR samples — one measurable interval — must be
        // enough to pass: min == median == max, a degenerate but valid
        // sample, not "insufficient data".
        let two_pcr = wire_with_pcr(0x1011, &[0, 3600]); // 40.0ms, baseline's exact configured interval
        assert!(pcr_interval(&inv, &two_pcr, VerifyMode::Strict).is_empty());
    }

    fn wire_with_pes(pid: u16, stream_id: u8, prefix: [u8; 4]) -> WireSummary {
        WireSummary {
            pes: BTreeMap::from([(
                pid,
                PesShape {
                    stream_ids: BTreeSet::from([stream_id]),
                    first_payload_prefix: Some(prefix),
                    prefix_mismatches: 0,
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn av1_carriage_wire_checks_stream_id_and_prefix_per_mode() {
        let inv_b = profiles::invariants(profiles::by_name("av1-klv-b").unwrap());
        let pass_b = wire_with_pes(0x1011, 0xBD, [0, 0, 1, 0x12]);
        assert!(av1_carriage(&inv_b, &pass_b).is_empty());
        let fail_b = wire_with_pes(0x1011, 0xE0, [0x12, 0, 0, 0]);
        let f = av1_carriage(&inv_b, &fail_b);
        assert!(
            f.iter().any(|s| s.starts_with("av1_carriage_wire")),
            "{f:?}"
        );

        let inv_a = profiles::invariants(profiles::by_name("av1-klv-a").unwrap());
        let pass_a = wire_with_pes(0x1011, 0xE0, [0x12, 0, 0, 0]);
        assert!(av1_carriage(&inv_a, &pass_a).is_empty());
        let fail_a = wire_with_pes(0x1011, 0xBD, [0, 0, 1, 0x12]);
        let f = av1_carriage(&inv_a, &fail_a);
        assert!(
            f.iter().any(|s| s.starts_with("av1_carriage_wire")),
            "{f:?}"
        );
    }

    fn wire_with_pts(pid: u16, pts: Vec<u64>) -> WireSummary {
        WireSummary {
            pts: BTreeMap::from([(
                pid,
                rawts::TimestampSeries::from_values(rawts::Retention::Full, pid, &pts),
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn pts_wrap_expected_iff_window_crosses_2_33() {
        const WRAP: u64 = 1u64 << 33;
        let p_roll = profiles::by_name("pts-rollover").unwrap();
        let inv_roll = profiles::invariants(p_roll);

        let wraps = wire_with_pts(0x1011, vec![WRAP - 100, 50]);
        assert!(pts_wrap(p_roll, &inv_roll, &wraps, 7.0).is_empty());

        let no_wrap = wire_with_pts(0x1011, vec![100, 200, 300]);
        let f = pts_wrap(p_roll, &inv_roll, &no_wrap, 7.0);
        assert!(
            f.iter().any(|s| s.starts_with("pts_wrap_observed")),
            "{f:?}"
        );

        let p_base = profiles::by_name("baseline").unwrap();
        let inv_base = profiles::invariants(p_base);
        let unexpected = wire_with_pts(0x1011, vec![WRAP - 100, 50]);
        let f = pts_wrap(p_base, &inv_base, &unexpected, 3.0);
        assert!(
            f.iter().any(|s| s.starts_with("pts_wrap_unexpected")),
            "{f:?}"
        );

        let short_window = wire_with_pts(0x1011, vec![100, 200, 300]);
        assert!(pts_wrap(p_roll, &inv_roll, &short_window, 3.0).is_empty());
    }

    fn wire_with_program(
        program_number: u16,
        pmt_pid: u16,
        pcr_pid: u16,
        streams: Vec<Stream>,
    ) -> WireSummary {
        WireSummary {
            programs: BTreeMap::from([(
                program_number,
                Program {
                    program_number,
                    pmt_pid,
                    pcr_pid,
                    streams,
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn pmt_stream_types_and_descriptors_match_the_profile() {
        let baseline = profiles::by_name("baseline").unwrap();
        let inv = profiles::invariants(baseline);

        let ok = wire_with_program(
            1,
            0x1000,
            0x1011,
            vec![
                stream(0x1011, 0x1B, None, &[]),
                stream(0x1031, 0x06, Some(*b"KLVA"), &[0x05]),
            ],
        );
        assert!(pmt_streams(baseline, &inv, &ok).is_empty());

        let wrong_video_type = wire_with_program(
            1,
            0x1000,
            0x1011,
            vec![
                stream(0x1011, 0x24, None, &[]),
                stream(0x1031, 0x06, Some(*b"KLVA"), &[0x05]),
            ],
        );
        let f = pmt_streams(baseline, &inv, &wrong_video_type);
        assert!(
            f.iter().any(|s| s.starts_with("pmt_stream_type_4113")),
            "{f:?}"
        );

        let missing_klv_registration = wire_with_program(
            1,
            0x1000,
            0x1011,
            vec![
                stream(0x1011, 0x1B, None, &[]),
                stream(0x1031, 0x06, None, &[]),
            ],
        );
        let f = pmt_streams(baseline, &inv, &missing_klv_registration);
        assert!(
            f.iter().any(|s| s.starts_with("pmt_descriptor_4145")),
            "{f:?}"
        );

        let sync = profiles::by_name("klv-sync").unwrap();
        let inv_sync = profiles::invariants(sync);
        let missing_sync_tags = wire_with_program(
            1,
            0x1000,
            0x1011,
            vec![
                stream(0x1011, 0x1B, None, &[]),
                stream(0x1031, 0x15, Some(*b"KLVA"), &[]),
            ],
        );
        let f = pmt_streams(sync, &inv_sync, &missing_sync_tags);
        assert!(
            f.iter().any(|s| s.starts_with("pmt_descriptor_4145")),
            "{f:?}"
        );

        let sync_ok = wire_with_program(
            1,
            0x1000,
            0x1011,
            vec![
                stream(0x1011, 0x1B, None, &[]),
                stream(0x1031, 0x15, Some(*b"KLVA"), &[0x26, 0x27]),
            ],
        );
        assert!(pmt_streams(sync, &inv_sync, &sync_ok).is_empty());
    }

    #[test]
    fn pmt_streams_reports_missing_program_and_wrong_audio_stream_type() {
        let audio_profile = profiles::by_name("audio").unwrap();
        let inv = profiles::invariants(audio_profile);

        // No PMT at all for program 1.
        let no_pmt = WireSummary::default();
        let f = pmt_streams(audio_profile, &inv, &no_pmt);
        assert!(
            f.iter().any(|s| s.starts_with("pmt_missing_program_1")),
            "{f:?}"
        );

        // PMT present, video/KLV correct, but the audio PID's
        // stream_type isn't 0x0F (AAC ADTS) — 0x03 here (MPEG-1 audio).
        let audio_pid = inv.programs[0]
            .audio_pid
            .expect("audio profile must have an audio PID");
        let wrong_audio_type = wire_with_program(
            1,
            0x1000,
            0x1011,
            vec![
                stream(0x1011, 0x1B, None, &[]),
                stream(0x1031, 0x06, Some(*b"KLVA"), &[0x05]),
                stream(audio_pid, 0x03, None, &[]),
            ],
        );
        let f = pmt_streams(audio_profile, &inv, &wrong_audio_type);
        assert!(
            f.iter()
                .any(|s| s.starts_with(&format!("pmt_stream_type_{audio_pid}"))),
            "{f:?}"
        );
    }
    /// CORR-07 / Q10: the wire-vs-demux floors, and the exact size of
    /// the per-PID boundary allowance they subtract.
    ///
    /// Every profile repeats PSI every 100 ms, so the allowance is one
    /// tenth of a second of units plus the unflushed tail one: 4 on
    /// `baseline`'s 30 fps video PID, 2 on its 10 Hz KLV PID, 6 on the
    /// `audio` profile's 46.875 Hz audio PID. Pinning those numbers is
    /// what keeps the allowance from quietly growing into slack.
    #[test]
    fn wire_vs_demux_holds_the_demuxer_to_the_raw_readers_pes_count() {
        let p = profiles::by_name("baseline").unwrap();
        let inv = profiles::invariants(p);
        let wire = WireSummary {
            pes_starts_per_pid: BTreeMap::from([(0x1011, 90), (0x1031, 30)]),
            ..Default::default()
        };
        let counts = |video: u64, klv: u64| program_counts(&[(1, video, klv)]);

        // Exact: passes in both tiers.
        assert!(wire_vs_demux(p, &inv, &wire, &counts(90, 30), VerifyMode::Strict, 0).is_empty());
        assert!(wire_vs_demux(p, &inv, &wire, &counts(90, 30), VerifyMode::Lossy, 0).is_empty());
        // Exactly at the boundary allowance (4 video, 2 KLV): still clean.
        assert!(wire_vs_demux(p, &inv, &wire, &counts(86, 28), VerifyMode::Strict, 0).is_empty());
        assert!(wire_vs_demux(p, &inv, &wire, &counts(86, 28), VerifyMode::Lossy, 0).is_empty());
        // One unit past it on each PID: both named, so the allowance is
        // finite and per-PID rather than a blanket excuse.
        let f = wire_vs_demux(p, &inv, &wire, &counts(85, 27), VerifyMode::Strict, 0);
        assert!(
            f.iter().any(|s| s.starts_with("wire_vs_demux_4113")),
            "{f:?}"
        );
        assert!(
            f.iter().any(|s| s.starts_with("wire_vs_demux_4145")),
            "{f:?}"
        );
        // Every fourth KLV record gone: 23 of 30 clears the 70 % floor
        // (21) and must NOT clear this one.
        let f = wire_vs_demux(p, &inv, &wire, &counts(90, 23), VerifyMode::Strict, 0);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            f[0].starts_with(
                "wire_vs_demux_4145: demuxer emitted 23 KLV records but the wire carries 30"
            ),
            "{}",
            f[0]
        );
        // Explained events widen the floor by exactly their count: the
        // KLV floor is 30 - explained - 2, so 5 excuses 23 and 4 does not.
        assert!(wire_vs_demux(p, &inv, &wire, &counts(90, 23), VerifyMode::Lossy, 5).is_empty());
        assert!(!wire_vs_demux(p, &inv, &wire, &counts(90, 23), VerifyMode::Lossy, 4).is_empty());
        // A profile with audio checks the audio PID too.
        let p_a = profiles::by_name("audio").unwrap();
        let inv_a = profiles::invariants(p_a);
        let wire_a = WireSummary {
            pes_starts_per_pid: BTreeMap::from([(0x1011, 90), (0x1031, 30), (0x1041, 141)]),
            ..Default::default()
        };
        let mut c = program_counts(&[(1, 90, 30)]);
        c.get_mut(&1).unwrap().audio_frames = 100;
        let f = wire_vs_demux(p_a, &inv_a, &wire_a, &c, VerifyMode::Strict, 0);
        assert!(
            f.iter()
                .any(|s| s.starts_with("wire_vs_demux_4161: demuxer emitted 100 audio frames")),
            "{f:?}"
        );
        // …and 135 of 141 (exactly the audio allowance of 6) does not.
        c.get_mut(&1).unwrap().audio_frames = 135;
        assert!(
            wire_vs_demux(p_a, &inv_a, &wire_a, &c, VerifyMode::Strict, 0).is_empty(),
            "the audio allowance is 6 frames, not more"
        );
        c.get_mut(&1).unwrap().audio_frames = 134;
        assert!(
            wire_vs_demux(p_a, &inv_a, &wire_a, &c, VerifyMode::Strict, 0)
                .iter()
                .any(|s| s.starts_with("wire_vs_demux_4161")),
            "the audio allowance is 6 frames, not more"
        );
    }
}
