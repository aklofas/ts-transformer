//! What a unit-test module needs from `alloc` when the crate is built
//! `no_std` (`cargo test -p tst-core --no-default-features --lib`); a test
//! module that uses `Vec`, `String`, `format!`, `vec!`, `to_string()` or
//! `to_owned()` starts with `use crate::test_prelude::*;`.

#![allow(unused_imports, reason = "each test module takes what it needs")]

pub(crate) use alloc::borrow::ToOwned;
pub(crate) use alloc::boxed::Box;
pub(crate) use alloc::format;
pub(crate) use alloc::string::{String, ToString};
pub(crate) use alloc::vec;
pub(crate) use alloc::vec::Vec;
