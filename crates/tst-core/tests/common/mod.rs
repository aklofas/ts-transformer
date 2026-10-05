//! Shared helpers for `tst-core` integration tests.
//!
//! Each integration test under `tests/*.rs` declares `mod common;` at
//! the top to pull these in. `tests/common/mod.rs` (not `tests/common.rs`)
//! is the Cargo-required shape — a file at `tests/common.rs` would be
//! compiled as its own integration-test binary.
//!
//! ## When to add helpers here vs. promote to `tst-test-helpers`
//!
//! Add a helper here when it's needed by ≥2 integration-test files in
//! THIS crate (`tst-core`) and the logic is test-specific (no production
//! users). The pattern parallels `crates/tst-srt/tests/common/mod.rs`
//! (its Loopback helper).
//!
//! Promote to the workspace's `tst-test-helpers` dev-dep crate only
//! when the helper is needed across CRATES — that's the established bar
//! for cross-crate test infrastructure. See `tst-test-helpers` itself
//! for what currently lives there.

#![allow(dead_code)]

/// Diagnostic breakdown of an IMAPB tolerance computation.
///
/// `tol` is what callers pass to `prop_assert!`; `scale` and `fp_eps`
/// are the two component error bounds so failure messages can show
/// which one dominated. See `imapb_tol` for the derivation.
pub struct ImapbTol {
    /// Final tolerance: `max(scale, fp_eps)`. Use this for the
    /// round-trip assertion (`(decoded - input).abs() <= imapb_tol(..).tol`).
    pub tol: f64,
    /// IMAPB quantization step at this (min, max, length).
    /// Round-trip error is at most `scale/2` in theory; we use full
    /// `scale` for safety. Dominates the tolerance for short lengths
    /// + large spans.
    pub scale: f64,
    /// f64 ULP floor scaled by field magnitude. Dominates the
    /// tolerance for long lengths + small spans where `scale` drops
    /// below f64 representation precision.
    pub fp_eps: f64,
}

/// IMAPB tolerance for round-trip property tests on f64 IMAPB values.
///
/// Combines two error sources per ST 1201.5 §8.9 + IEEE 754:
/// 1. **IMAPB quantization step** — `scale = 2^ceil(log2(span)) / 2^(8L-1)`.
///    Encode rounds to nearest grid point so the integer-rounding
///    error is at most `scale/2`; we allow full `scale` for safety.
/// 2. **f64 ULP propagation** — at small `scale` (large `L` and small
///    span), f64 representation error dominates. The intermediate
///    `(value - min)` and `sf * (i + offset) + min` terms have
///    magnitudes near `span` and `max(|min|, |max|)`, so the bound is
///    `f64::EPSILON * max(span, |min|, |max|, 1) * 4` (safety factor 4).
///
/// Returns an [`ImapbTol`] so failure messages can show which
/// component dominated (`scale` vs `fp_eps`).
///
/// Used by:
/// - `tests/klv_proptest.rs::imapb_roundtrip` (substrate-level test)
/// - `tests/klv_typed_set_proptest.rs` (typed-set proptests for ST 0601
///   IMAPB tags and ST 0903 FOV fields)
///
/// `length` is the wire byte width (1..=8 per ST 1201.5 §8.9). For
/// typed-set tests that aggregate multiple tags with different widths,
/// pass the SMALLEST width in use to get a conservative (looser) bound
/// that's safe for all of them.
pub fn imapb_tol(min: f64, max: f64, length: usize) -> ImapbTol {
    let span = max - min;
    let log2_ceil = span.log2().ceil();
    let scale = 2f64.powf(log2_ceil) / 2f64.powi(8 * length as i32 - 1);
    let magnitude = span.max(min.abs()).max(max.abs()).max(1.0);
    let fp_eps = f64::EPSILON * magnitude * 4.0;
    ImapbTol {
        tol: scale.max(fp_eps),
        scale,
        fp_eps,
    }
}
