//! Domain harness: codec parser + carriage integration tests (H.264/H.265/H.266, AV1, audio) + corpus cross-checks
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `codec::<file>::` prefix.

// Shared test helpers, declared once at the binary root so
// `crate::common::*` resolves for every member and `common/mod.rs` is not
// loaded as a module twice (clippy::duplicate_mod).
#[path = "common/mod.rs"]
mod common;
#[path = "codec/audio_corpus_cross_check.rs"]
mod audio_corpus_cross_check;
#[path = "codec/audio_frame_roundtrip.rs"]
mod audio_frame_roundtrip;
#[path = "codec/av1_carriage_roundtrip.rs"]
mod av1_carriage_roundtrip;
#[path = "codec/av1_codec_integration.rs"]
mod av1_codec_integration;
#[path = "codec/av1_no_panic.rs"]
mod av1_no_panic;
#[path = "codec/codec_parameter_sets.rs"]
mod codec_parameter_sets;
#[path = "codec/codec_stats.rs"]
mod codec_stats;
#[path = "codec/codec_stats_mux.rs"]
mod codec_stats_mux;
#[path = "codec/corpus_cross_check.rs"]
mod corpus_cross_check;
#[path = "codec/h265_real_x265_round_trip.rs"]
mod h265_real_x265_round_trip;
#[path = "codec/h266_carriage_roundtrip.rs"]
mod h266_carriage_roundtrip;
#[path = "codec/h266_codec_integration.rs"]
mod h266_codec_integration;
#[path = "codec/h266_vui_overflow.rs"]
mod h266_vui_overflow;
#[path = "codec/h266_real_encoder_round_trip.rs"]
mod h266_real_encoder_round_trip;
