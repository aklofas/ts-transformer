//! Stream-profile registry: the 12 canonical MPEG-TS/KLV wire shapes exercised
//! by the interop test driver.
//!
//! Each [`Profile`] names one shape (codec + KLV carriage + audio/program
//! count + cadence knobs). [`invariants`] derives the wire-format oracle a
//! captured stream must satisfy for that shape — computed independently from
//! the MPEG-TS/tst-core spec values (not delegated to tst-core's own
//! enum-to-byte mapping), so a regression in both the muxer and this oracle
//! at once is still caught by downstream verification tasks.

use tst_core::mpegts::mux::Av1CarriageMode;

use crate::impair::XorShift64;
use crate::{mux_setup, schedule};

/// Video codec carried on a profile's video PID.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
    H266,
}

/// KLV carriage mode for a profile's metadata PID.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KlvMode {
    Async,
    Sync,
    AsyncWithMisp,
}

/// One canonical MPEG-TS/KLV stream shape.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub name: &'static str,
    pub video: VideoCodec,
    /// `Some` only when `video == VideoCodec::Av1`.
    pub av1_mode: Option<Av1CarriageMode>,
    pub klv: KlvMode,
    /// AAC-ADTS second stream.
    pub audio: bool,
    /// 1 or 2.
    pub programs: u8,
    pub pcr_interval_ms: u32,
    pub psi_interval_ms: u32,
    /// 90 kHz ticks; `pts-rollover` starts near 2^33.
    pub start_pts_ticks: u64,
    /// 30.
    pub fps: u32,
    /// 10.
    pub klv_hz: u32,
}

/// One program's expected PIDs, for the wire-level oracles in
/// [`crate::oracles`] — derived from [`mux_setup`]'s PID constants, the
/// same ones `mux_setup::build_config` actually wires up, so a captured
/// stream is checked against the PIDs it was really muxed onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedProgram {
    pub program_number: u16,
    pub video_pid: u16,
    pub klv_pid: u16,
    pub audio_pid: Option<u16>,
}

/// Wire-format oracle a captured stream must satisfy for a [`Profile`] to be
/// considered conformant.
pub struct Invariants {
    pub video_stream_type: u8,
    /// 0x06 async / 0x15 sync.
    pub klv_stream_type: u8,
    pub audio_expected: bool,
    pub program_count: u8,
    /// fps, minus slack applied by the caller.
    pub min_video_aus_per_sec: u32,
    pub min_klv_per_sec: u32,
    pub expect_misp_sei: bool,
    /// Configured PCR interval — the oracle's *lower* bound; see
    /// [`crate::oracles`]'s PCR cadence rule.
    pub pcr_interval_ms: u32,
    /// `1000.0 / p.fps` — the slack the PCR cadence oracle's *upper*
    /// bound adds on top of `pcr_interval_ms` (the muxer only stamps PCR
    /// on a content packet, so the observed interval can overshoot the
    /// configured one by up to one frame period).
    pub frame_period_ms: f64,
    /// `Some` only when `p.video == VideoCodec::Av1` — the AV1 carriage
    /// oracle is a no-op for every other profile.
    pub av1_mode: Option<Av1CarriageMode>,
    /// Expected AAC sample rate, `Some` iff `p.audio`.
    pub audio_sample_rate_hz: Option<u32>,
    /// PIDs of every program this profile mixes onto the wire.
    pub programs: Vec<ExpectedProgram>,
}

const FPS: u32 = 30;
const KLV_HZ: u32 = 10;

// PCR/PSI defaults and legal range come from `MuxerConfig`:
// - Defaults 40ms/100ms: crates/tst-core/src/mpegts/mux/config.rs:168-169
//   (`pcr_interval_ms: 40, psi_interval_ms: 100`).
// - Validation: crates/tst-core/src/mpegts/mux/config.rs:820-826 — the
//   builder rejects `pcr_interval_ms` outside `1..=100` and
//   `psi_interval_ms < 10`. `pcr-tight` uses the builder's minimum (1);
//   `pcr-sparse` uses the builder's maximum (100), which is also the
//   H.222.0-driven ceiling this codebase enforces.
const BASELINE_PCR_MS: u32 = 40;
const BASELINE_PSI_MS: u32 = 100;
const PCR_TIGHT_MS: u32 = 1;
const PCR_SPARSE_MS: u32 = 100;

/// `pts-rollover`'s start point: 5 s (450_000 ticks at 90 kHz) below the
/// 33-bit PTS wraparound boundary (2^33, ITU-T H.222.0 §2.4.3.6).
const PTS_ROLLOVER_START: u64 = (1u64 << 33) - 450_000;

const PROFILES: &[Profile] = &[
    Profile {
        name: "baseline",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "klv-sync",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Sync,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "misp",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::AsyncWithMisp,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "h265-klv",
        video: VideoCodec::H265,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "av1-klv-a",
        video: VideoCodec::Av1,
        av1_mode: Some(Av1CarriageMode::InteropRawObu),
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "av1-klv-b",
        video: VideoCodec::Av1,
        av1_mode: Some(Av1CarriageMode::Mpeg2TsBinding),
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "h266-klv",
        video: VideoCodec::H266,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "audio",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: true,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "two-program",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 2,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "pcr-tight",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: PCR_TIGHT_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "pcr-sparse",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: PCR_SPARSE_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: 0,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
    Profile {
        name: "pts-rollover",
        video: VideoCodec::H264,
        av1_mode: None,
        klv: KlvMode::Async,
        audio: false,
        programs: 1,
        pcr_interval_ms: BASELINE_PCR_MS,
        psi_interval_ms: BASELINE_PSI_MS,
        start_pts_ticks: PTS_ROLLOVER_START,
        fps: FPS,
        klv_hz: KLV_HZ,
    },
];

/// All 12 canonical profiles, in registry order.
pub fn all() -> &'static [Profile] {
    PROFILES
}

/// Build the `Demuxer` config a conformant receiver of `p`'s traffic must
/// use.
///
/// The audit finding this closes: a default-constructed `Demuxer` assumes
/// `Av1CarriageMode::Mpeg2TsBinding`, but `av1-klv-a` deliberately carries
/// AV1 the `InteropRawObu` way (PES `stream_id=0xE0`, raw OBUs — see that
/// profile's own `av1_mode`). Demuxing `av1-klv-a`'s traffic with a
/// default-config `Demuxer` therefore makes every access unit non-
/// conformant (`NonConformantIssue::Av1WrongStreamId`) — silently, before
/// this crate started counting/gating those events. Every demuxer this
/// crate builds must come from this function, not `Demuxer::new()`/
/// `DemuxerConfig::default()`, so each profile is demuxed the way it was
/// muxed.
pub fn demuxer_config(p: &Profile) -> tst_core::mpegts::demux::DemuxerConfig {
    tst_core::mpegts::demux::DemuxerConfig::builder()
        .av1_carriage(p.av1_mode.unwrap_or_default())
        .build()
}

/// Look up a profile by its `name`.
pub fn by_name(n: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == n)
}

/// Salt mixed into the seed before drawing profiles, for the same reason
/// [`crate::impair::SCHEDULE_SALT`] exists: `soak.sh` passes ONE `--seed`
/// to the impairment engine, the impairment schedule, the corruption tap
/// AND this draw, and unsalted they would all walk correlated state
/// trajectories (the profile picked would be a function of the first
/// impairment decisions). Changing this constant re-draws every seed's
/// profiles — archived soak evidence quotes its seed, not its profile
/// names, so the mapping from seed to profiles must stay stable.
pub const PROFILE_SALT: u64 = 0x9F0F_11E5_0000_0A11;

/// Draw `count` DISTINCT profile names deterministically from `seed` —
/// `soak.sh --profile auto`'s per-leg selection, exposed to it through
/// the `pick-profiles` subcommand so the shell never reimplements
/// [`XorShift64`].
///
/// Each slot draws an index into [`all`] and re-rolls until it lands on a
/// name no earlier slot took, so the returned names are distinct: two legs
/// running the same profile would halve the shape coverage one soak run
/// buys. The re-roll loop is bounded (see `MAX_DRAWS` below) rather than
/// unbounded — a bounded loop cannot wedge a 72h run's launch even if the
/// generator were somehow degenerate.
///
/// # Errors
///
/// `count` greater than the registry size, which no draw could satisfy
/// distinctly, and the (unreachable in practice) exhausted-draw-budget
/// case.
pub fn pick(seed: u64, count: usize) -> Result<Vec<&'static str>, String> {
    let profiles = all();
    if count > profiles.len() {
        return Err(format!(
            "pick: asked for {count} distinct profile(s) but the registry has only {}",
            profiles.len()
        ));
    }
    /// Draw attempts allowed per slot before giving up. Every slot has at
    /// least one acceptable index left (`count <= profiles.len()` is
    /// checked above), so the expected number of re-rolls is small; this
    /// is purely the "never spin forever" bound.
    const MAX_DRAWS: usize = 1000;

    let mut rng = XorShift64::new(seed ^ PROFILE_SALT);
    let mut picked: Vec<&'static str> = Vec::with_capacity(count);
    while picked.len() < count {
        let mut drawn = None;
        for _ in 0..MAX_DRAWS {
            let idx = (rng.next_u64() % profiles.len() as u64) as usize;
            let name = profiles[idx].name;
            if !picked.contains(&name) {
                drawn = Some(name);
                break;
            }
        }
        match drawn {
            Some(name) => picked.push(name),
            None => {
                return Err(format!(
                    "pick: {MAX_DRAWS} draws for slot {} all landed on an already-picked profile \
                     (seed {seed})",
                    picked.len()
                ));
            }
        }
    }
    Ok(picked)
}

/// Derive the wire-format invariants a captured stream must satisfy for `p`
/// to be considered conformant.
///
/// Stream-type bytes are the PMT `stream_type` values fixed by ISO/IEC
/// 13818-1 (H.264=0x1B, H.265=0x24, H.266=0x33 — see
/// `crates/tst-core/src/mpegts/common/mod.rs:47-65`) and by tst-core's AV1
/// carriage (0x06 PES-private-data regardless of `av1_mode` — see
/// `crates/tst-core/src/mpegts/mux/state.rs:795`). KLV is 0x06 PrivateData
/// for async carriage (including `AsyncWithMisp`) or 0x15
/// SynchronousMetadata for sync carriage.
pub fn invariants(p: &Profile) -> Invariants {
    let video_stream_type = match p.video {
        VideoCodec::H264 => 0x1B,
        VideoCodec::H265 => 0x24,
        VideoCodec::H266 => 0x33,
        VideoCodec::Av1 => 0x06,
    };
    let klv_stream_type = match p.klv {
        KlvMode::Sync => 0x15,
        KlvMode::Async | KlvMode::AsyncWithMisp => 0x06,
    };
    let mut programs = vec![ExpectedProgram {
        program_number: 1,
        video_pid: mux_setup::PROG1_VIDEO_PID,
        klv_pid: mux_setup::PROG1_KLV_PID,
        audio_pid: p.audio.then_some(mux_setup::PROG1_AUDIO_PID),
    }];
    if p.programs == 2 {
        programs.push(ExpectedProgram {
            program_number: 2,
            video_pid: mux_setup::PROG2_VIDEO_PID,
            klv_pid: mux_setup::PROG2_KLV_PID,
            audio_pid: None,
        });
    }
    Invariants {
        video_stream_type,
        klv_stream_type,
        audio_expected: p.audio,
        program_count: p.programs,
        min_video_aus_per_sec: p.fps,
        min_klv_per_sec: p.klv_hz,
        expect_misp_sei: matches!(p.klv, KlvMode::AsyncWithMisp),
        pcr_interval_ms: p.pcr_interval_ms,
        frame_period_ms: 1000.0 / p.fps as f64,
        av1_mode: p.av1_mode,
        audio_sample_rate_hz: p.audio.then_some(schedule::AUDIO_SAMPLE_RATE_HZ),
        programs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_twelve_unique_named_profiles() {
        let profiles = all();
        assert_eq!(profiles.len(), 12);
        let mut names: Vec<&str> = profiles.iter().map(|p| p.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 12, "profile names must be unique");
    }

    #[test]
    fn by_name_roundtrips_every_profile() {
        for p in all() {
            let found = by_name(p.name).expect("by_name must find every registered profile");
            assert_eq!(found.name, p.name);
        }
        assert!(by_name("does-not-exist").is_none());
    }

    #[test]
    fn pts_rollover_starts_within_five_seconds_of_the_wrap() {
        let p = by_name("pts-rollover").expect("pts-rollover profile must exist");
        let wrap = 1u64 << 33;
        assert!(p.start_pts_ticks < wrap);
        assert!(wrap - p.start_pts_ticks <= 450_000);
    }

    #[test]
    fn av1_profiles_carry_distinct_carriage_modes() {
        let a = by_name("av1-klv-a").expect("av1-klv-a profile must exist");
        let b = by_name("av1-klv-b").expect("av1-klv-b profile must exist");
        assert_eq!(a.video, VideoCodec::Av1);
        assert_eq!(b.video, VideoCodec::Av1);
        let a_mode = a.av1_mode.expect("av1-klv-a must set av1_mode");
        let b_mode = b.av1_mode.expect("av1-klv-b must set av1_mode");
        assert_ne!(a_mode, b_mode);
    }

    #[test]
    fn pcr_tight_is_stricter_than_pcr_sparse() {
        let tight = by_name("pcr-tight").expect("pcr-tight profile must exist");
        let sparse = by_name("pcr-sparse").expect("pcr-sparse profile must exist");
        assert!(tight.pcr_interval_ms < sparse.pcr_interval_ms);
    }

    /// The seed->profiles mapping is archived evidence's only record of
    /// which shapes a soak run exercised (the run quotes its seed), so the
    /// draw must be reproducible from the seed alone.
    #[test]
    fn pick_is_deterministic_for_a_seed() {
        for seed in [0u64, 1, 3, 7, 42, u64::MAX] {
            let a = pick(seed, 2).expect("2 distinct profiles must be drawable");
            let b = pick(seed, 2).expect("2 distinct profiles must be drawable");
            assert_eq!(a, b, "seed {seed} must draw the same profiles every time");
        }
        // Different seeds must not all collapse onto one answer.
        let distinct: std::collections::BTreeSet<Vec<&str>> =
            (0u64..32).map(|s| pick(s, 2).unwrap()).collect();
        assert!(
            distinct.len() > 1,
            "32 seeds drew a single profile pair: {distinct:?}"
        );
    }

    #[test]
    fn pick_returns_distinct_registered_names() {
        for seed in 0u64..64 {
            let names = pick(seed, 2).expect("2 distinct profiles must be drawable");
            assert_eq!(names.len(), 2);
            assert_ne!(names[0], names[1], "seed {seed} drew a duplicate pair");
            for n in &names {
                assert!(by_name(n).is_some(), "seed {seed} drew unknown profile {n}");
            }
        }
    }

    /// The whole registry is drawable at once (the boundary case), and one
    /// more than that is an error rather than a wedged re-roll loop.
    #[test]
    fn pick_handles_the_registry_size_boundary() {
        let n = all().len();
        let every = pick(9, n).expect("the whole registry must be drawable distinctly");
        assert_eq!(every.len(), n);
        let mut sorted = every.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            n,
            "a full draw must be a permutation: {every:?}"
        );

        let e = pick(9, n + 1).unwrap_err();
        assert!(
            e.contains(&format!("{}", n + 1)) && e.contains("registry"),
            "{e}"
        );
    }

    #[test]
    fn invariants_map_stream_types_and_misp_expectation() {
        let baseline = invariants(by_name("baseline").unwrap());
        assert_eq!(baseline.video_stream_type, 0x1B);
        assert_eq!(baseline.klv_stream_type, 0x06);
        assert!(!baseline.expect_misp_sei);

        let sync = invariants(by_name("klv-sync").unwrap());
        assert_eq!(sync.klv_stream_type, 0x15);

        let misp = invariants(by_name("misp").unwrap());
        assert_eq!(misp.klv_stream_type, 0x06);
        assert!(misp.expect_misp_sei);

        let h265 = invariants(by_name("h265-klv").unwrap());
        assert_eq!(h265.video_stream_type, 0x24);

        let h266 = invariants(by_name("h266-klv").unwrap());
        assert_eq!(h266.video_stream_type, 0x33);

        let av1_a = invariants(by_name("av1-klv-a").unwrap());
        let av1_b = invariants(by_name("av1-klv-b").unwrap());
        assert_eq!(av1_a.video_stream_type, 0x06);
        assert_eq!(av1_b.video_stream_type, 0x06);
    }
}
