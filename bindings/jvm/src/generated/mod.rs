//! Converters written by `print-records` from the `binding_record!`
//! declarations in `tst-core` and the crates that own a record. Each family file is generated end to end;
//! edit the declaration, then run `cargo run -p tst-binding-gen --bin print-records -- --write`.

pub(crate) mod codec;
pub(crate) mod st0102;
pub(crate) mod st0601;
pub(crate) mod st0605;
pub(crate) mod st0805;
pub(crate) mod st0806;
pub(crate) mod st0903;
pub(crate) mod st1010;
pub(crate) mod stats_hls;
pub(crate) mod stats_mpegts;
pub(crate) mod stats_pipeline;
pub(crate) mod stats_rtp;
pub(crate) mod stats_shells;
pub(crate) mod stats_srt;
