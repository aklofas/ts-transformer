//! Compiled into tst-c-core only under `--cfg tst_lint_fixture`
//! (scripts/check/rust/lint-fixtures.sh). Clippy must reject every item.

pub fn planted_expect(v: Option<u8>) -> u8 {
    v.expect("planted")
}

pub fn planted_unreachable() -> u8 {
    unreachable!("planted")
}

/// # Safety
/// Never called; it only has to reach clippy.
pub unsafe fn planted_raw_slice<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    // SAFETY: never called.
    unsafe { core::slice::from_raw_parts(ptr, len) }
}
