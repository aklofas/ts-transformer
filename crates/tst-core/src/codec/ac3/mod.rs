//! AC-3 (ATSC A/52) syncframe parser.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Stateless, minimal parser for the syncinfo + first few bsi fields of
//! an AC-3 elementary stream. No production code calls
//! [`parse_syncframe`]: the muxer writes a fixed
//! `AC-3_audio_stream_descriptor` (ATSC A/52 §A.4.3 + Table A4.1) and does
//! not derive it from a syncframe (a caller who wants the exact fields
//! parses one here and builds the descriptor with
//! [`crate::mpegts::descriptors::ac3_audio_stream_descriptor`]), and the
//! demuxer checks the two syncword bytes of a PES carrying
//! `data_alignment_indicator = 1` (ATSC A/52:2018 §A.6.3) itself.
//!
//! ## Spec coverage
//!
//! Parsed per ATSC A/52:2018 §5.4.1 (syncinfo) + §5.4.2 (bsi prefix):
//! - syncword (16 bits, must be 0x0B77).
//! - fscod (2 bits → sample rate, Table 5.6).
//! - frmsizecod (6 bits → frame length + nominal bit rate, Table 5.18).
//! - bsid (5 bits → AC-3 bitstream version; usually 8 for modern AC-3).
//! - bsmod (3 bits → audio service type; 0=CM, 1=ME, etc.).
//! - acmod (3 bits → audio coding mode; 2=2/0 stereo, 7=3/2 surround, etc.).
//! - lfeon (1 bit, but the surrounding optional cmixlev/surmixlev/dsurmod
//!   bits shift its position — see [`parse_syncframe`]).
//!
//! ## Not parsed (deferred)
//!
//! - dialnorm, compr, langcod, audprodie, time codes, addbsi — bsi tail
//!   fields after lfeon. The `AC-3_audio_stream_descriptor` fields
//!   (sample_rate_code / bit_rate_code / surround_mode / bsmod /
//!   num_channels) are all available from the syncinfo + first few bsi
//!   bits.
//! - audblk / auxdata / errorcheck — block-level audio data and CRCs.
//! - E-AC-3 (Annex E) — extension to AC-3, signalled by bsid 11..=16
//!   (an Annex E decoder mutes 9, 10 and above 16). `parse_syncframe`
//!   rejects bsid >= 9 as unsupported; if E-AC-3 support is needed, add a
//!   separate parser keyed on bsid.

mod decode;
#[cfg(test)]
mod tests;

pub use decode::{Ac3SyncInfo, parse_syncframe};
