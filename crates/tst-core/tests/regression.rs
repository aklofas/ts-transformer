//! Domain harness: fixed spec-conformance regressions (KLV BER/BER-OID
//! symmetry, IMAPB and ST 0903 FOV spec vectors) and optional cross-impl
//! interop probes (external AV1 decoders, WebVTT/CEA-708 tools)
//! (consolidated from the former per-file tests/*.rs — see tests/MOVEMENT_MAP.md).
//!
//! Each `mod` below is one former top-level integration-test file, now
//! compiled into this single binary. Test bodies are unchanged; only the
//! module path gained a `regression::<file>::` prefix.
#[path = "regression/av1_external_decoder.rs"]
mod av1_external_decoder;
#[path = "regression/ber_oid_symmetry.rs"]
mod ber_oid_symmetry;
#[path = "regression/imapb_spec_vectors.rs"]
mod imapb_spec_vectors;
#[path = "regression/st0903_incremental.rs"]
mod st0903_incremental;
#[path = "regression/subtitle_interop.rs"]
mod subtitle_interop;
