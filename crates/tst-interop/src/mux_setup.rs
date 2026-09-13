//! Profile → `MuxerConfig` mapping: the single place a [`Profile`] becomes
//! a concrete mux configuration.
//!
//! `gen::run` and `send::run` both build their muxer
//! from [`build_config`], so the wire shape a profile produces is defined
//! in exactly one place. Stream handles are deliberately NOT returned
//! here: `VideoStreamHandle`/`KlvStreamHandle`/`AudioStreamHandle` are
//! only valid on the `Muxer` (or `MuxSender`) instance that produced them
//! (see `tst_core::mpegts::mux::VideoStreamHandle`'s "Bound to producer"
//! contract) — a handle read off a throwaway `Muxer` built from a cloned
//! config would be a different, unrelated producer from the caller's real
//! one. Callers construct their own live `Muxer`/`MuxSender` from this
//! config and read handles off *that* via its own
//! `video_handles()`/`klv_handles()`/`audio_handles()` accessors (`Muxer`:
//! `crates/tst-core/src/mpegts/mux/push_{video,klv,audio}.rs`;
//! `MuxSender`: `crates/tst-pipeline/src/mux_sender.rs:815,827,838`).

use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

use crate::profiles::{KlvMode, Profile, VideoCodec};

/// Program 1's PIDs — the same conventional values `MuxerConfig::default()`
/// and the mux examples use (`examples/muxing/mux_to_file.rs`,
/// `mux_av1_with_klv.rs`).
pub(crate) const PROG1_PMT_PID: u16 = 0x1000;
pub(crate) const PROG1_VIDEO_PID: u16 = 0x1011;
pub(crate) const PROG1_KLV_PID: u16 = 0x1031;
pub(crate) const PROG1_AUDIO_PID: u16 = 0x1041;

/// Program 2's PIDs, for `two-program` — a clean 0x100 stride off program
/// 1's range (`examples/muxing/repack_two_programs.rs`'s renumbering
/// scheme), so the two programs' PIDs never collide.
pub(crate) const PROG2_PMT_PID: u16 = 0x1100;
pub(crate) const PROG2_VIDEO_PID: u16 = 0x1111;
pub(crate) const PROG2_KLV_PID: u16 = 0x1131;

fn mux_video_codec(c: VideoCodec) -> MuxVideoCodec {
    match c {
        VideoCodec::H264 => MuxVideoCodec::H264,
        VideoCodec::H265 => MuxVideoCodec::H265,
        VideoCodec::H266 => MuxVideoCodec::H266,
        VideoCodec::Av1 => MuxVideoCodec::Av1,
    }
}

/// Build the `MuxerConfig` a [`Profile`] describes.
///
/// KLV carriage: `Sync` maps to `SynchronousMetadata` + `carries_pts:
/// true` (required by `MuxerConfig::validate` for that stream type);
/// `Async`/`AsyncWithMisp` both map to `PrivateData` + `carries_pts:
/// false` — MISP only changes what rides inside the video SEI, not the
/// KLV PID's carriage (mirrors `verify::expected_klv_carriage`).
///
/// `two-program` gets a second program (own PIDs) mirroring the first;
/// the caller pushes the same content onto both programs' handles.
pub fn build_config(p: &Profile) -> MuxerConfig {
    let codec = mux_video_codec(p.video);
    let (klv_stream_type, carries_pts) = match p.klv {
        KlvMode::Sync => (KlvStreamType::SynchronousMetadata, true),
        KlvMode::Async | KlvMode::AsyncWithMisp => (KlvStreamType::PrivateData, false),
    };

    let mut prog1 = MuxerProgramConfigBuilder::new(1, PROG1_PMT_PID);
    prog1.add_video(PROG1_VIDEO_PID, codec);
    prog1.add_klv(PROG1_KLV_PID, klv_stream_type, carries_pts);
    if p.audio {
        prog1.add_audio(PROG1_AUDIO_PID, AudioCodec::Aac);
    }

    let mut builder = MuxerConfig::builder();
    builder.add_program(prog1.build());

    if p.programs == 2 {
        let mut prog2 = MuxerProgramConfigBuilder::new(2, PROG2_PMT_PID);
        prog2.add_video(PROG2_VIDEO_PID, codec);
        prog2.add_klv(PROG2_KLV_PID, klv_stream_type, carries_pts);
        builder.add_program(prog2.build());
    }

    builder.pcr_interval_ms(p.pcr_interval_ms);
    builder.psi_interval_ms(p.psi_interval_ms);
    if let Some(mode) = p.av1_mode {
        builder.av1_carriage(mode);
    }

    builder
        .build()
        .expect("every registered Profile must build a valid MuxerConfig")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles;
    use tst_core::mpegts::mux::{Av1CarriageMode, Muxer, StreamSpec};

    #[test]
    fn single_program_profile_has_one_video_and_klv_handle_no_audio() {
        let p = profiles::by_name("baseline").expect("baseline profile must exist");
        let cfg = build_config(p);
        assert_eq!(cfg.programs.len(), 1);
        // Handles are only meaningful off a live Muxer built from this
        // config (see this module's doc comment) — construct one here
        // purely to assert the shape build_config produced.
        let muxer = Muxer::new(cfg).expect("valid config must construct");
        assert_eq!(muxer.video_handles().len(), 1);
        assert_eq!(muxer.klv_handles().len(), 1);
        assert!(muxer.audio_handles().is_empty());
    }

    #[test]
    fn audio_profile_has_one_audio_handle() {
        let p = profiles::by_name("audio").expect("audio profile must exist");
        let cfg = build_config(p);
        let muxer = Muxer::new(cfg).expect("valid config must construct");
        assert_eq!(muxer.audio_handles().len(), 1);
    }

    #[test]
    fn two_program_profile_has_two_video_and_klv_handles() {
        let p = profiles::by_name("two-program").expect("two-program profile must exist");
        let cfg = build_config(p);
        assert_eq!(cfg.programs.len(), 2);
        let muxer = Muxer::new(cfg).expect("valid config must construct");
        assert_eq!(muxer.video_handles().len(), 2);
        assert_eq!(muxer.klv_handles().len(), 2);
    }

    #[test]
    fn klv_sync_profile_sets_synchronous_metadata_carries_pts() {
        let p = profiles::by_name("klv-sync").expect("klv-sync profile must exist");
        let cfg = build_config(p);
        let klv_spec = cfg.programs[0]
            .streams
            .iter()
            .find(|s| matches!(s, StreamSpec::Klv { .. }))
            .expect("klv-sync config must have a KLV stream");
        match klv_spec {
            StreamSpec::Klv {
                stream_type,
                carries_pts,
                ..
            } => {
                assert_eq!(*stream_type, KlvStreamType::SynchronousMetadata);
                assert!(*carries_pts);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn av1_klv_a_profile_carries_the_configured_carriage_mode() {
        let p = profiles::by_name("av1-klv-a").expect("av1-klv-a profile must exist");
        let cfg = build_config(p);
        assert_eq!(cfg.av1_carriage, Av1CarriageMode::InteropRawObu);
    }

    #[test]
    fn av1_klv_b_profile_carries_the_configured_carriage_mode() {
        let p = profiles::by_name("av1-klv-b").expect("av1-klv-b profile must exist");
        let cfg = build_config(p);
        assert_eq!(cfg.av1_carriage, Av1CarriageMode::Mpeg2TsBinding);
    }

    #[test]
    fn pcr_tight_profile_config_matches_its_pcr_and_psi_intervals() {
        let p = profiles::by_name("pcr-tight").expect("pcr-tight profile must exist");
        let cfg = build_config(p);
        assert_eq!(cfg.pcr_interval_ms, p.pcr_interval_ms);
        assert_eq!(cfg.psi_interval_ms, p.psi_interval_ms);
    }

    #[test]
    fn pcr_sparse_profile_config_matches_its_pcr_and_psi_intervals() {
        let p = profiles::by_name("pcr-sparse").expect("pcr-sparse profile must exist");
        let cfg = build_config(p);
        assert_eq!(cfg.pcr_interval_ms, p.pcr_interval_ms);
        assert_eq!(cfg.psi_interval_ms, p.psi_interval_ms);
    }
}
