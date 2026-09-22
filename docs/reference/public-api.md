# Public API Policy

This document codifies the public API conventions for the `ts-transformer`
workspace (`tst-core`, `tst-pipeline`, `tst-srt`, `tst-c`). It applies to
the **pre-1.0 era** — once we reach 1.0, SemVer rules govern; the
conventions here become the *baseline* for what crosses the SemVer
contract boundary.

This page governs *what stays public at all* — the binding-canonical
workflow below decides whether an item is kept reachable for a real
consumer. It does not say how much that item is expected to churn once
it's public. That's a separate question, answered one layer up by
[`docs/reference/api-stability.md`](/docs/reference/api-stability.md),
which classifies every top-level public module into a Stable /
Provisional / Experimental / Internal tier. Read this page to learn *why*
an item is public; read that page to learn *how much it might move under
you*.

## Layer model

Three layers of intended-public surface, from most stable to least:

1. **Root re-exports** (`tst_core::Foo`, `tst_pipeline::Foo`, `tst_srt::Foo`)
   — the small set of types and traits a typical caller imports in one
   `use` line. Drift here is high-impact; expect SemVer to take it
   seriously post-1.0.

2. **Module-level public API** (`tst_core::mpegts::demux::Demuxer`,
   `tst_pipeline::mux_sender::MuxSender`, etc.) — the curated, supported
   per-module surface for advanced Rust users. Stability expectations
   match the root re-exports.

3. **`low_level` namespaces** (`tst_core::mpegts::demux::low_level::*`,
   future `tst_core::mpegts::descriptors::low_level::*` if needed) —
   explicitly-named extension points for fuzz harnesses, third-party
   tools, and advanced consumers that need direct access to parser
   internals. **Stability: experimental.** May change between minor
   versions before 1.0; post-1.0 expectations TBD.

Anything not in one of these three categories is **private** and may
change without notice.

## Binding-canonical-workflow rule

Before privatizing a public item, the implementer must:

1. Grep `bindings/c/core/src/` for use sites of the item.
2. Read the JVM binding ([`docs/languages/jvm.md`](/docs/languages/jvm.md)
   and `bindings/jvm/src/`) to confirm it does not reach the item through a
   canonical workflow.
3. Grep `crates/tst-core/fuzz/fuzz_targets/`, `crates/tst-core/tests/`,
   and `examples/` for cross-crate uses.

If any of those checks finds an item being used through a canonical
workflow (i.e., not a hidden field-poking workaround), the item:

- Stays publicly accessible.
- Moves under an explicit `low_level` namespace if the audit confirms
  the use is "advanced consumer territory" rather than "expected
  curated-API consumer territory."

The default bias is **usability over privacy**: keep items reachable for
real consumers, even if it means more public surface. The `low_level`
namespace exists specifically to signal "this is reachable, but you're
opting out of the curated stability contract."

## Module visibility convention

Each module that hosts implementation submodules should follow this
pattern:

```rust
// my_mod/mod.rs
mod helper_a;        // implementation detail
mod helper_b;        // implementation detail
mod types;           // private — re-exported as needed below

pub mod public_sub;  // module is part of the curated surface
pub mod low_level;   // extension points (if applicable)

pub use types::{PublicType, AnotherPublicType};
pub use helper_a::PublicHelperFunction;
```

Submodules that are `pub mod` are the *intentional* public surface.
Submodules that are `mod` are private; if anything inside them needs to
be public, surface it via an explicit `pub use` from `mod.rs`. This
keeps the public surface visible at the top of each `mod.rs`.

## Binding crates: no `cargo public-api` baseline (by design)

Ten Rust library crates carry a committed `public-api.txt` baseline that
CI checks via `cargo public-api` on every push: `tstrans-rist-sys`,
`tstrans-mbedtls-src`, `tst-core`, `tst-hls`, `tst-pipeline`, `tst-rist`,
`tst-rtp`, `tst-srt`, `tst-tcp`, and `tst-udp`.

The three binding crates — `bindings/c` (tst-c), `bindings/c/core`
(tst-c-core), and `bindings/python` (tst-py) — intentionally carry **no**
`public-api.txt`. Their consumer contract is not their Rust surface:

- **tst-c / tst-c-core.** The Rust surface of these crates is a cdylib/staticlib
  leaf (`pub use tst_c_core::*`) plus `#[no_mangle] extern "C"` glue.
  `cargo public-api` on a cdylib is not meaningful; the real ABI contract is
  the committed cbindgen-generated header `bindings/c/include/tstrans.h`, the
  `TST_ABI_VERSION_MAJOR` / `TST_ABI_VERSION_MINOR` macros it defines, and the
  C-ABI ratchets under `scripts/check/c/` (especially
  `abi-rustdoc-coverage.sh`, `header-conditional-sections.sh`,
  and `header-mirror-enum-export.sh`).
- **tst-py.** The Rust surface is `#[pymodule]` / `#[pymethods]` PyO3 glue —
  not the Python contract. The Python consumer surface is gated by the
  committed `.pyi` stubs under `bindings/python/python/tstrans/`, the
  `py.typed` marker, the pytest suite, the ratchets under
  `scripts/check/python/`, and — since 0.7.0 — the error-kind vocabulary
  itself, which is `tst_pipeline::binding::BindingErrorKind::name()` checked
  at `import tstrans` (the per-kind Python error-mapping ratchet was retired
  in Arc 2 WP-B2; `scripts/ratchets/kind-equivalence.tsv` records the
  Rust-C-Python-JVM member mapping instead).

**Rule:** do not add `cargo public-api` baselines to binding crates unless
they become actual CI release gates. Adding a baseline to a binding crate
that isn't wired into CI creates misleading drift noise without gating
anything.

See `docs/reference/binding-authors.md` for the full C-ABI error-mapping
contract and the Python/JVM/Swift binding-shape conventions.

## Regenerating baselines

Baselines are rendered by `cargo public-api` on the **pinned nightly toolchain**
declared in the CI public-api step (see `.github/workflows/ci.yml`, currently
`nightly-2026-07-03`). Nightly rustdoc occasionally changes how re-exported
`std`/`core` paths render, which shows up as spurious whole-baseline drift, so
an unpinned nightly cannot be trusted for rendering. To regenerate:

    cargo +nightly-2026-07-03 public-api -p <crate> --simplified > crates/<crate>/public-api.txt

To bump the pin: pick a new date, re-render all 10 baselines with it, and land
the pin bump and the re-rendered baselines in the same commit.

## Cross-references

- `docs/reference/api-stability.md` — per-module stability tiers (the churn-expectation layer on top of this policy).
- `docs/reference/conventions.md` — naming, constructor verbs, builder rules.
- `docs/reference/binding-authors.md` — JNI / UniFFI / C ABI conventions.
- `docs/reference/architecture.md` — crate graph and high-level pipeline model.
- `docs/project/deferred-features.md` — what's not yet supported and the
  trigger to revisit.

## Examples in this codebase

- `tst_core::mpegts::demux::low_level` (introduced 2026-05-19, plan
  Wave 3.1 Plan A) — re-exports `Reassembler`, `parse_pat`, `parse_pmt`,
  `KlvShape`, `classify_klv`, `walk_descriptors`, and related types so
  fuzz harnesses and advanced consumers reach them without depending on
  private submodule paths.
- `tst_core::mpegts::descriptors` — the canonical home for descriptor
  construction (`registration`, `metadata_klva`, etc.) and parsing
  (`RawDescriptor`, `walk_descriptors`, `find_descriptor_tag`, etc.).
