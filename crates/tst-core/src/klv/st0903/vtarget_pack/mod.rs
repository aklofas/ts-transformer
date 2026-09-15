//! ST 0903.6 VTargetPack — `value_bytes` payload of Tag 101 (vTargetSeries)
//! in the top-level VMTI LS. Each pack is a single tracked target.
//!
//! Decode + encode here is the per-pack roundtrip; the vTargetSeries
//! container layer lives at `klv::st0903::decode::decode_vtarget_series`.

pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod model;

#[cfg(test)]
mod tests;

pub(crate) use decode::{read_pack, read_pack_strict};
pub(crate) use encode::{encoded_len, write_pack};
pub use model::{VTargetPack, VTargetPackError};
