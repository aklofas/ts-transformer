//! Domain harness: C ABI muxing: multi-program/stream, audio+subtitle, codec stats, demux config parity
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `muxing::<file>::` prefix. Per-file `#![cfg(feature=…)]`
//! gates still apply (a gated-out member compiles to an empty module).
#[allow(dead_code)]
#[path = "common/mod.rs"]
mod common;
#[allow(dead_code)]
#[path = "muxing/common.rs"]
mod muxing_common;
#[path = "muxing/audio_subtitle.rs"]
mod audio_subtitle;
#[path = "muxing/av1_carriage_provenance.rs"]
mod av1_carriage_provenance;
#[path = "muxing/codec_stats.rs"]
mod codec_stats;
#[path = "muxing/data_stream.rs"]
mod data_stream;
#[path = "muxing/demux_config_av1_parity.rs"]
mod demux_config_av1_parity;
#[path = "muxing/multi_program.rs"]
mod multi_program;
#[path = "muxing/multi_program_event_identity.rs"]
mod multi_program_event_identity;
#[path = "muxing/multi_stream.rs"]
mod multi_stream;
#[path = "muxing/stats.rs"]
mod stats;
#[path = "muxing/demuxer_offline.rs"]
mod demuxer_offline;
#[path = "muxing/muxer_dts.rs"]
mod muxer_dts;
#[path = "muxing/muxer_misp.rs"]
mod muxer_misp;
