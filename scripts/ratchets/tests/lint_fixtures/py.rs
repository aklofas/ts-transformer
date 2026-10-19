//! Compiled into tst-py only under `--cfg tst_lint_fixture`
//! (scripts/check/rust/lint-fixtures.sh). Clippy must reject every item.

pub fn planted_bare_detach(py: pyo3::Python<'_>) -> u8 {
    py.detach(|| 0)
}

pub fn planted_expect(v: Option<u8>) -> u8 {
    v.expect("planted")
}
