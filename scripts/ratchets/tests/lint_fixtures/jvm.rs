//! Compiled into tst-jni only under `--cfg tst_lint_fixture`
//! (scripts/check/rust/lint-fixtures.sh). Clippy must reject every item.

pub fn planted_expect(v: Option<u8>) -> u8 {
    v.expect("planted")
}

pub fn planted_unreachable() -> u8 {
    unreachable!("planted")
}
