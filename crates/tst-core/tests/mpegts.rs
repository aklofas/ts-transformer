//! Domain harness: MPEG-TS container mux/demux + local-corpus integration tests
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `mpegts::<file>::` prefix.
#[path = "mpegts/au_cell_round_trip.rs"]
mod au_cell_round_trip;
#[path = "mpegts/au_cell_tolerance.rs"]
mod au_cell_tolerance;
#[path = "mpegts/au_reassembly.rs"]
mod au_reassembly;
#[path = "mpegts/audio_fixtures.rs"]
mod audio_fixtures;
#[path = "mpegts/audio_treat_as.rs"]
mod audio_treat_as;
#[path = "mpegts/av1_remux_fixpoint.rs"]
mod av1_remux_fixpoint;
#[path = "mpegts/av1_wrap_guard.rs"]
mod av1_wrap_guard;
#[path = "mpegts/demux.rs"]
mod demux;
#[path = "mpegts/demux_audio.rs"]
mod demux_audio;
#[path = "mpegts/demux_caps.rs"]
mod demux_caps;
#[path = "mpegts/demux_local.rs"]
mod demux_local;
#[path = "mpegts/demux_psi_program_number.rs"]
mod demux_psi_program_number;
#[path = "mpegts/demux_multi_program.rs"]
mod demux_multi_program;
#[path = "mpegts/demux_pes_validation.rs"]
mod demux_pes_validation;
#[path = "mpegts/demux_robustness.rs"]
mod demux_robustness;
#[path = "mpegts/demux_strict.rs"]
mod demux_strict;
#[path = "mpegts/demux_subtitle.rs"]
mod demux_subtitle;
#[path = "mpegts/from_program_map.rs"]
mod from_program_map;
#[path = "mpegts/mux.rs"]
mod mux;
#[path = "mpegts/mux_audio.rs"]
mod mux_audio;
#[path = "mpegts/mux_builder_errors.rs"]
mod mux_builder_errors;
#[path = "mpegts/mux_data.rs"]
mod mux_data;
#[path = "mpegts/mux_demux_audio_roundtrip.rs"]
mod mux_demux_audio_roundtrip;
#[path = "mpegts/mux_demux_subtitle_roundtrip.rs"]
mod mux_demux_subtitle_roundtrip;
#[path = "mpegts/mux_demux_video_raw_roundtrip.rs"]
mod mux_demux_video_raw_roundtrip;
#[path = "mpegts/mux_descriptor_body_length.rs"]
mod mux_descriptor_body_length;
#[path = "mpegts/mux_descriptor_invariant.rs"]
mod mux_descriptor_invariant;
#[path = "mpegts/mux_descriptors_roundtrip.rs"]
mod mux_descriptors_roundtrip;
#[path = "mpegts/mux_dvb_subtitle_pes.rs"]
mod mux_dvb_subtitle_pes;
#[path = "mpegts/mux_dvb_teletext_pes.rs"]
mod mux_dvb_teletext_pes;
#[path = "mpegts/mux_error_kind_routing.rs"]
mod mux_error_kind_routing;
#[path = "mpegts/mux_ffprobe.rs"]
mod mux_ffprobe;
#[path = "mpegts/mux_klv_pes.rs"]
mod mux_klv_pes;
#[path = "mpegts/mux_local.rs"]
mod mux_local;
#[path = "mpegts/mux_misp_push.rs"]
mod mux_misp_push;
#[path = "mpegts/mux_multi_program.rs"]
mod mux_multi_program;
#[path = "mpegts/mux_multi_stream.rs"]
mod mux_multi_stream;
#[path = "mpegts/mux_proptest.rs"]
mod mux_proptest;
#[path = "mpegts/mux_shorthand_multi_program.rs"]
mod mux_shorthand_multi_program;
#[path = "mpegts/mux_subtitle.rs"]
mod mux_subtitle;
#[path = "mpegts/psi_builders.rs"]
mod psi_builders;
#[path = "mpegts/psi_proptest.rs"]
mod psi_proptest;
#[path = "mpegts/pts_unwrap.rs"]
mod pts_unwrap;
#[path = "mpegts/subtitle_fixtures.rs"]
mod subtitle_fixtures;
#[path = "mpegts/subtitle_treat_as.rs"]
mod subtitle_treat_as;
