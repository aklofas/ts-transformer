# Contributing

Issues and pull requests are welcome. This page is the process half;
[`docs/reference/conventions.md`](docs/reference/conventions.md) is the
code-style half, [`docs/reference/public-api.md`](docs/reference/public-api.md)
the public-API workflow and [`docs/reference/api-stability.md`](docs/reference/api-stability.md)
the per-module stability tiers. Security reports go through
[`SECURITY.md`](SECURITY.md), not the issue tracker.

## Toolchain

`rust-toolchain.toml` pins Rust 1.85; `cargo` inside the workspace picks it
up via rustup. The native libraries build from vendored submodules
(`git submodule update --init --recursive`): libsrt and mbedTLS via
`cmake`, librist via `meson` + `ninja`. `SRT_FORCE_VENDORED=1
RIST_FORCE_VENDORED=1` skips the `pkg-config` probe so the build matches
CI. A cold build takes 3–5 minutes; warm builds seconds.

## Before you push

CI (`.github/workflows/ci.yml`) runs fmt + clippy, three test modes on four
platforms, doctests, nightly rustdoc, ~47 bash rails and three ratchets.
Run the same set locally from the workspace root:

```bash
export SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
for feats in srt rtp udp tcp hls rist udp,tcp,hls,rist; do    # per-feature tst-c clippy
  cargo clippy -p tst-c -p tst-c-core --all-targets --no-default-features \
    --features "tst-c/${feats//,/,tst-c/}" -- -D warnings
done
cargo nextest run --workspace --no-fail-fast                    # plain `cargo test` also works
cargo nextest run --workspace --no-default-features --no-fail-fast
cargo nextest run --workspace --all-features --no-fail-fast
cargo test --doc --workspace --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo +nightly doc --workspace --no-deps --all-features
for s in $(find scripts/check embedded/scripts/check -name '*.sh' ! -name 'freertos-srt.sh'); do
  bash "$s" </dev/null || echo "FAIL: $s"
done
```

Read the rail output, not only the exit codes: a rail prints `SKIP:` or
`WARNING:` when a prerequisite is missing (a built Python extension for
`scripts/check/python/stubtest.sh`, QEMU for the embedded gates) and that
is not a pass.

Three ratchets need a deliberate update when your change is intentional:

- **Public API** — `cargo public-api` baselines for ten crates at
  `crates/<dir>/public-api.txt`. Re-render with the nightly PINNED in
  `ci.yml` (search `nightly-2026-07-03`):
  `cargo +nightly-2026-07-03 public-api -p <pkg> --simplified > crates/<dir>/public-api.txt`.
  An unpinned nightly renders `std::io` vs `core::io` paths differently
  and produces false drift.
- **`#[non_exhaustive]` count** — `BASELINE` in `ci.yml` (search
  `non_exhaustive count`) must not decrease; measure with
  `rg -c '^\s*#\[non_exhaustive\]' crates/ bindings/ --type rust | awk -F: '{s+=$2} END {print s}'`
  and set the baseline to the observed value.
- **Fuzz harnesses** — separate workspaces under `crates/*/fuzz/`, not
  covered by `--workspace`; a signature change needs
  `cargo +nightly fuzz check` in each (and `cargo +1.85 fmt --check` there).

Bindings: `bindings/python` builds with `maturin develop --release` in its
own venv and tests with `pytest`; `bindings/jvm` runs `./gradlew test`.
The embedded sub-project has its own gates under `embedded/scripts/check/`.

## Pull requests

One branch per work package, rebase-merged after CI is green on all four
platforms. Every user-visible change gets a `CHANGELOG.md` entry under
`[Unreleased]`. Commit subjects read `<area>: <what changed>` with a body
only when the why is not obvious; no generated-by trailers. A red CI leg
is a rerun candidate only when it matches a known flake class (runner-load
timeouts in the Windows loopback group, a vendor-fetch 5xx). Three
failures are real regressions and are never rerun: the RTSP max-sessions
burst cap assert, the interop `transparent_relay` loss test, and the HTTP
`oom_guard` tests.
