//! AV1 (AOM Bitstream Spec) OBU parsers.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Hand-rolled per the AV1 Bitstream & Decoding Process Specification.
//! Reuses internal LEB128 primitives for OBU size encoding
//! and AV1's `uvlc` reads.
//!
//! ## Spec coverage
//!
//! Parsed per the AV1 Bitstream & Decoding Process Specification:
//! - Sequence Header OBU: profile / level / tier / dimensions /
//!   bit depth / chroma format / color info / still-picture flags.
//! - Frame Header OBU (light): frame_type / show_frame /
//!   show_existing_frame.
//! - OBU stream collector [`parse_obu_stream`]: walks `Vec<Obu>`,
//!   partial-success-tolerant.
//!
//! ## Not parsed (deferred)
//!
//! - Per-frame size override in Frame Header — full reference frame
//!   management not surfaced; `frame_size` is always `None`.
//!   See `docs/project/deferred-features.md`.
//! - Tile Group OBUs (pass-through unparsed via `codec::Obu`).
//! - Metadata OBUs (pass-through).
//! - Padding OBUs.

pub(crate) mod decode;
mod model;

#[cfg(test)]
mod tests;

pub use decode::{parse_frame_header_light, parse_obu_stream, parse_sequence_header};
pub use model::{Av1FrameHeaderLight, Av1ObuStream, Av1SequenceHeader};
