# OSS-Fuzz Onboarding Artifacts

This directory contains the four artifacts (`project.yaml`, `Dockerfile`, `build.sh`, `README.md`) plus dictionaries / options / seed corpora that constitute the OSS-Fuzz onboarding bundle for `ts-transformer`.

It is consumed in two contexts:

1. **One-time submission** of these files to `google/oss-fuzz` so the project is enrolled in the OSS-Fuzz fleet.
2. **Continuous use** by OSS-Fuzz's nightly build cycle, which clones this repo and runs `build.sh` to produce fresh fuzz drivers.

## Layout

| Path | Purpose |
|---|---|
| `project.yaml` | OSS-Fuzz project metadata (contacts, language, sanitizers) |
| `Dockerfile` | Container image: base-builder-rust + cmake + repo clone |
| `build.sh` | Build script: cargo +nightly fuzz build, binary copy, seed/dict/options packaging |
| `targets/klv.dict` | libFuzzer dictionary for KLV decoders |
| `targets/<name>.options` | Per-target libFuzzer options (e.g., `max_len`) |

Committed synthetic seeds live canonically under `crates/<crate>/fuzz/seeds/<target>/` (single source of truth); `build.sh` zips them from there. Fixture-derived seeds (ST 0601 fixtures, plan #52 regression fixtures) are sourced from `crates/tst-core/tests/fixtures/`.

## Submission to google/oss-fuzz (one-time)

```bash
# Fork google/oss-fuzz under your GitHub account, clone the fork.
git clone --depth 1 https://github.com/<your-fork>/oss-fuzz.git ~/oss-fuzz
cd ~/oss-fuzz

# Copy these artifacts into projects/ts-transformer/.
# Run the cp from your ts-transformer workspace root (the dir containing this oss-fuzz/ subdir).
mkdir -p projects/ts-transformer
cp -r /path/to/ts-transformer/oss-fuzz/* projects/ts-transformer/

# Verify the build locally before opening the PR.
python3 infra/helper.py build_image ts-transformer
python3 infra/helper.py build_fuzzers --sanitizer address ts-transformer
python3 infra/helper.py check_build ts-transformer

# Smoke one target to confirm it runs.
python3 infra/helper.py run_fuzzer ts-transformer demux_feed -- -runs=1000

# Commit and push, then open a PR to google/oss-fuzz upstream.
git add projects/ts-transformer
git commit -m "Add ts-transformer project"
git push origin main
```

The PR review by Google's OSS-Fuzz maintainers usually completes within 1-2 business days if `check_build` passes locally. After merge, the OSS-Fuzz fleet starts fuzzing all bundled targets within 24 hours (currently 32: 27 in `tst-core`, 4 in `tst-rtp`, 1 in `tst-srt` — `build.sh` asserts the shipped-driver count against the `fuzz_targets/*.rs` inventory so this figure cannot silently drift).

> **Status (2026-08-18):** submission has NOT happened yet — the project is
> not enrolled upstream and there is no continuous OSS-Fuzz coverage today.
> The last local `helper.py` verification (see `VERIFICATION.md`) predates
> the target-inventory expansion to 31; re-run the full
> build_image/build_fuzzers/check_build/run_fuzzer sequence above before
> opening the upstream PR.

## Local rebuild (anytime, post-submission)

Once the OSS-Fuzz fork is set up, re-running the build is one command:

```bash
cd ~/oss-fuzz
cp -r /path/to/ts-transformer/oss-fuzz/* projects/ts-transformer/
python3 infra/helper.py build_fuzzers --sanitizer address ts-transformer
python3 infra/helper.py check_build ts-transformer
```

Useful when you've changed `build.sh`, `Dockerfile`, or `project.yaml` and want to verify before pushing to main.

## Reproducing an OSS-Fuzz crash report

When the OSS-Fuzz fleet finds a crash, you'll get an email with a reproducer attachment (`crash-<hash>`):

```bash
# Save the reproducer to a temp file.
cp ~/Downloads/crash-<hash> /tmp/repro

# Reproduce locally with cargo-fuzz directly (much faster than going through the OSS-Fuzz container).
# Run from the ts-transformer workspace root.
cd crates/tst-core
cargo +nightly fuzz run <target_name> /tmp/repro
```

If the bug reproduces, fix it in our tree, push to main, and OSS-Fuzz will pick up the fix on its next build cycle (within 24h).

## Maintenance — what triggers a new PR to google/oss-fuzz?

| Change | New PR needed? |
|---|---|
| Bug fix in any Rust source | No — OSS-Fuzz git-clones each cycle |
| New fuzz target added in our tree | No — build.sh discovers via `ls fuzz_targets/` |
| Submodule pin bump (libsrt / mbedTLS) | No — container re-clones with new submodules |
| New seed added under `crates/<crate>/fuzz/seeds/<target>/` | No — same |
| `project.yaml` change (sanitizers, contacts) | **Yes** |
| `Dockerfile` change (deps, base image) | **Yes** |
| `build.sh` change (logic, paths) | **Yes** |
| Adding `auto_ccs` recipients | **Yes** |

Estimated long-term burden: ~1 PR/year to `google/oss-fuzz` absent base-image drift.

## What is NOT here

- **CIFuzz GHA integration**. Deferred until the first OSS-Fuzz bug is triaged and we want regression prevention on PRs.
- **MemorySanitizer (MSan) builds**. Deferred until `tst-jni`/`tst-uniffi` introduce material new `unsafe`.
- **Coverage builds for public dashboard**. Deferred until we want to communicate fuzz reach publicly.
- **Crash-triage SLA / bug-response policy**. Runtime concern; surfaces once bugs start arriving.
