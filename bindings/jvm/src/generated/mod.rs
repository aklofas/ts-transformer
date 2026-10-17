//! Converters written by `print-records` from the `binding_record!`
//! declarations in `tst-core`. Each family file is generated end to end;
//! edit the declaration, then run `cargo run -p tst-pipeline --bin print-records -- --write`.

pub(crate) mod st0102;
pub(crate) mod st0601;
pub(crate) mod st0605;
pub(crate) mod st0805;
pub(crate) mod st0806;
pub(crate) mod st0903;
pub(crate) mod st1010;
