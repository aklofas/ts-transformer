//! Low-level extension points for the MPEG-TS demuxer.
//!
//! **Stability: Internal** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! No stability promise: this module is public solely for the
//! binding-canonical workflow and documented extension points (see the
//! header above), and its items may change in any release. Use only
//! when the curated [`Demuxer`] / [`DemuxEvent`] API is insufficient —
//! typical examples are fuzz harnesses, third-party tools that
//! introspect raw PSI sections, and advanced consumers that need direct
//! access to PES reassembly state.
//!
//! For 99% of use cases, prefer:
//! - [`crate::mpegts::demux::Demuxer`] for stream parsing.
//! - [`crate::mpegts::demux::DemuxEvent`] for typed event consumption.
//! - [`crate::mpegts::descriptors`] for descriptor construction and parsing.
//!
//! [`Demuxer`]: crate::mpegts::demux::Demuxer
//! [`DemuxEvent`]: crate::mpegts::demux::DemuxEvent

// PES reassembly extension point (consumed by fuzz harness `demux_pes_reassembly`).
pub use crate::mpegts::demux::pes::{PesPayload, Reassembler, ReassemblyOutcome};

// PSI section parsers and their result types (consumed by fuzz harness `demux_psi`
// and integration tests for descriptor introspection).
pub use crate::mpegts::demux::psi::{
    Pat, PatEntry, Pmt, PmtStream, PsiParseError, extract_metadata_link, extract_user_label,
    has_klva_registration, parse_pat, parse_pmt, walk_descriptors,
};

// Payload classification and AU-cell iteration (consumed by the integration
// test `au_cell_round_trip`).
pub use crate::mpegts::demux::payload::{KlvShape, classify_klv, iter_au_cells};

// Canonical descriptor type (definition lives at `crate::mpegts::descriptors`;
// re-exported here so low-level consumers don't need to cross module boundaries).
pub use crate::mpegts::descriptors::RawDescriptor;
