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
    Invariants {
        video_stream_type,
        klv_stream_type,
        audio_expected: p.audio,
        program_count: p.programs,
        min_video_aus_per_sec: p.fps,
        min_klv_per_sec: p.klv_hz,
        expect_misp_sei: matches!(p.klv, KlvMode::AsyncWithMisp),
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
