# Releasing

A single `v*` tag publishes to **three immutable registries** through
three workflows — treat them as one release transaction:

| Registry | Artifact(s) | Workflow | Publish mode |
|---|---|---|---|
| **crates.io** | the 11 Rust library crates, in dependency-layer order | `crates-io.yml` | automatic (OIDC trusted publishing, per-layer tokens) |
| **PyPI** | `tstrans` wheels + sdist | `python-wheels.yml` | automatic (OIDC trusted publishing) |
| **Maven Central** | `org.tstrans:tstrans-jvm` fat JAR | `jvm-jar.yml` | **staged** — a maintainer inspects and clicks Publish |

A GitHub Release (notes seeded from the CHANGELOG highlights block) completes
the set — 14 published artifacts per release in total.

The 1.0 release has further conditions, listed with the check that proves
each in the [1.0 exit criteria](/docs/project/1.0-exit-criteria.md).

> **Release state.** All three registries are
> live: crates.io (all 11 crates, first published at 0.4.0 and OIDC-wired
> since), PyPI `tstrans` (since 0.2.0), and Maven Central
> `org.tstrans:tstrans-jvm` (since 0.1.0). Standing cautions: every one of
> these registries is immutable — **never tag a release without explicit
> maintainer confirmation** — and on the tag run **watch the OIDC
> `publish to PyPI` job**: a *skipped* publish means nothing reached PyPI
> (the way v0.1.0 silently missed). For crates.io, watch the `crates-io`
> run's ordered layers and verify docs.rs builds afterwards.

## One-time prerequisites

1. **GPG signing key** (for Maven artifact signatures):
   ```bash
   gpg --full-generate-key            # RSA 4096, your identity
   gpg --list-secret-keys --keyid-format=long
   # Publish the public key so Central can verify signatures:
   gpg --keyserver keyserver.ubuntu.com --send-keys <KEYID>
   # Export the ASCII-armored private key for the CI secret:
   gpg --armor --export-secret-keys <KEYID> > tstrans-signing-key.asc
   ```

2. **Central Portal user token** — at https://central.sonatype.com →
   Account → Generate User Token. Yields a username + password pair.
   (The `org.tstrans` namespace is already claimed + verified.)

3. **GitHub repository secrets** (Settings → Secrets and variables → Actions):
   | Secret | Value |
   |---|---|
   | `MAVEN_CENTRAL_USERNAME` | Portal token username |
   | `MAVEN_CENTRAL_PASSWORD` | Portal token password |
   | `SIGNING_KEY` | contents of `tstrans-signing-key.asc` (full armored block, including the `-----BEGIN/END-----` lines and newlines) |
   | `SIGNING_PASSWORD` | the GPG key passphrase (empty string if none) |

   PyPI needs no secret — it uses OIDC trusted publishing, configured and
   first exercised successfully at v0.2.0.

## Pre-tag checklist

The steps run in this order. From step 3 until the tag, nothing that ships
changes: the soak and the real-capture corpus run judge the release-prep
merge commit, and the qualification run checks that nothing under
`crates/` or `bindings/` changed after it.

1. **Version consistency (release preflight).** The rail checks six sources —
   workspace `Cargo.toml`, `bindings/python/pyproject.toml`, the tst-py
   `bindings/python/Cargo.toml`, the C `TST_VERSION_*` constants in
   `bindings/c/core/src/lib.rs`, the committed `bindings/c/include/tstrans.h`
   macros, and the Python version test:
   ```bash
   bash scripts/check/repo/release-version-consistency.sh
   ```
   It must report all-consistent at the release version before you tag. The
   same rail runs in CI on the tag and gates both publish workflows.

   The rail does **not** cover everything the version sweep must touch —
   also update by hand:
   - **every internal path-dependency's pinned `version = "X.Y.Z"` key** —
     `crates/{srt-sys,rist-sys,tst-pipeline,tst-srt,tst-rist,tst-udp,tst-tcp,
     tst-hls,tst-rtp,tst-webrtc}/Cargo.toml` (21 keys at 0.9.0,
     dev-dependencies included; the rail's OK line prints the number it
     compared). Miss one of these and the crates.io ordered publish below
     fails at layer 2 — after
     the irreversible layer-1 publish already landed. `release-version-
     consistency.sh` now asserts these match the workspace version (see
     `RVC_DEP_TOMLS` in the script), so this step is CI-enforced, not just
     convention;
   - the workspace `Cargo.lock` **and the embedded sub-project lockfiles**
     (`embedded/baremetal-qemu{,-c}/Cargo.lock`,
     `embedded/freertos-srt/example/host/Cargo.lock`) — all record workspace
     crate versions, and the embedded QEMU gates build `--locked`, so a stale
     one fails CI loudly (a `cargo metadata` run in each directory re-syncs);
   - the regenerated committed `tstrans.h` (regeneration picks up the new
     `TST_VERSION_*` values);
   - the `#define TST_VERSION_*` snippet in `docs/languages/c.md` — the
     doc-currency ratchet (`doc-abi-and-st1910-currency.sh` Rule 4) pins it
     to the workspace version and will hold CI red until it matches;
   - `crates/mbedtls-src/Cargo.toml`'s `version` field. It carries a
     `+3.6.7`-style local-version-identifier suffix pinned to the bundled
     Mbed TLS release (e.g. `0.4.0+3.6.7`) — bump the base number on every
     workspace version sweep, and bump the suffix independently whenever the
     vendored `crates/mbedtls-src/vendor/mbedtls` submodule itself advances.
   - Re-pin `rust-toolchain.toml` and `rust-version` to stable minus six
     releases (check `rustup check` for current stable); update the sites
     `msrv-consistency.sh` lists.
2. **The release-prep PR.** One PR carries the version sweep from step 1 and
   the CHANGELOG: the `[Unreleased]` section is retitled `[X.Y.Z]` with a
   date placeholder, and a fresh `[Unreleased]` stub opens above it. The
   "Release highlights" block is the seed for the GitHub Release notes.
3. **Freeze.** The release-prep merge commit is the soaked commit. Nothing
   outside `CHANGELOG.md`, `docs/` and
   `tests/coverage/evidence-archives.toml` changes after it until the tag,
   so a lockfile, toolchain pin or submodule change counts as much as source
   code. The one exception inside `docs/` is the JavaScript under
   `docs/webrtc/`, which the soak's browser leg runs, so only Markdown there
   counts as a document. A fix that must land elsewhere restarts the
   checklist at step 2,
   because the qualification run refuses a soak whose tree differs from the
   qualified one anywhere outside those three paths.
4. **`main` is green** — `ci`, `jvm-jar`, and `python-wheels` all passing on
   the frozen commit. Every rerun on that commit matches a row of
   the [CI flake ledger](/docs/project/ci-flakes.md), and the latest
   `ci-health` run's `ALERT` lines all have a row there; one that does not is
   a finding to settle before the soak starts.
5. **Dry-run the wheels:**
   `gh workflow run python-wheels.yml` builds every wheel + sdist and skips
   publish (publish is tag-gated). Confirm all legs are green, and watch for a
   leg sitting *queued* — that is the v0.1.0 failure mode.
6. **The release VM.** One dedicated cloud VM runs the corpus phase and then
   the soak, both on the frozen commit:
   - Provision the VM. Its instance role reads the corpus bucket, which holds
     the real captures and the previous release's corpus baseline.
   - Check out the frozen commit, clean, with its submodules, and build
     `tst-interop` with `--release`; the corpus phase runs that binary and
     the soak rebuilds the same tree.
   - Run the corpus phase: `tst-interop corpus inventory` lists the
     captures, `corpus sweep` digests each one and compares it with the
     previous release's baseline, `corpus replay` sends each capture through
     the transports and judges what arrives, and `corpus report` writes the
     result and its archive row. A failure stops the checklist here; it is a
     finding to fix, and the fix restarts at step 2.
   - Launch the soak detached, so no session limit can end it:
     ```bash
     SRT_RECONNECT_MODE=background nohup bash scripts/interop/soak.sh \
       --outdir ~/soak-X.Y.Z --hours 72 &
     ```
     The script refuses to start when its declared memory gate does not fit
     the VM.
   - Check the run at 24 and 48 hours without touching it: every process is
     alive, there is no fail-fast marker, and the SRT receiver's
     `reconnects` reads 4 and then 8.
   - When the run ends, harvest the corpus run directory and the soak
     directory. Write a checksum list for each on the VM and check the
     copies against it.
   - The `tst-interop report soak` that closes the run must print
     `overall_pass=true`, and the script exits 0. Any other verdict is a
     finding, not a rerun.
7. **The archive-entry PR.** A docs-only PR adds two rows to
   `tests/coverage/evidence-archives.toml`: the soak, with
   `expected_duration_s = 259200`, and the corpus run, each with the frozen
   commit as its `tree`. The same PR adds the soak to the runs table on the
   [validation evidence page](/docs/project/validation-evidence.md).
8. **Destroy the VM** once both archives are harvested and checked.
9. **The date commit.** Replace the CHANGELOG date placeholder with the
   release date. The commit touches the CHANGELOG only, so the frozen tree
   still stands for it.
10. **Qualify the candidate.** Once `ci` is green on the date commit, dispatch
    the qualification run on that exact commit, in release mode:
    ```bash
    gh workflow run release-qualify.yml -f sha=<the full 40-hex commit sha> -f release=true
    ```
    The commit must be on `main` or be a `v*` tag; the run's `resolve` job
    refuses anything else. One run calls the wheels, JVM, interop,
    sanitizer, fuzz, ABI and semver legs in rehearsal mode (nothing is
    published) and writes a record. The run must end **GO**: every leg
    succeeded on that commit, the tree was clean, and the interop census
    matches `expectations.toml`. `release=true` adds the release gate: GO
    also needs a passing soak archive that declares 259 200 seconds and a
    passing corpus archive for the soaked tree that replayed all five paths
    (at least one file, none failed), with no change outside `CHANGELOG.md`,
    `docs/` and `tests/coverage/evidence-archives.toml` between the soaked
    tree and this commit. The JavaScript under `docs/webrtc/` is not a
    document, because the soak's browser leg runs it. A `v*` tag push
    applies the same gate. The gate
    judges the newest 72-hour soak that stands for the commit. A newer failed
    soak of any declared duration on the same tree is a reason for NO-GO,
    while a newer passing short run never replaces the 72-hour pass. Without
    the input the record is a mid-cycle one and states that the gate was not
    applied.

    A **NO-GO** record lists its reasons (a failed or missing leg, a stale
    `ci` result, a dirty tree, an unexpected interop failure, a missing,
    failed, short or superseded soak archive, a missing or failed corpus
    archive). A real leg failure is a finding to fix, not a rerun; rerun
    only a failure that belongs to a ledgered flake class.

    The run's bundle holds `qualification.json`, the rendered
    `qualification.md`, the SBOMs, `SHA256SUMS`, the build attestations and
    the C package. Download it from the run's artefacts.
11. **Commit the record.** In a docs-only PR, add the run's
    `qualification.json` as `docs/project/qualification.json` (CI's
    `check-qualification` step verifies the page matches it) and render the
    evidence page's generated block:
    ```bash
    bash scripts/gen/qualification.sh --write
    ```
12. **Maintainer-local checks.** Some validation needs inputs that are not on a
    CI runner, so the record lists them as `maintainer_local` and they stay
    with you. Run each and note the result before tagging:
    - long fuzz runs, 10 minutes per target;
    - the benchmark regression check;
    - the one-hour synthetic soak.

    The real-capture corpus check runs on the release VM in step 6.

    The separate pre-release validation battery is retired. Its other steps
    are covered as follows:

    | Former step | Now covered by |
    |---|---|
    | Regression tier | `ci` |
    | Baseline mux | the interop matrix's generated input |
    | `tsanalyze` / `tspsi` / `ffprobe` goldens | the `analyze/{tsanalyze,tsp-analyze,ffprobe}/*` cells |
    | FFmpeg muxer differential | retired; the `srt-live/*` and `decode/*` cells use FFmpeg as a peer |
    | Player decode matrix | the `decode/*` cells |
    | C demux round trip | `bindings/c/tests/receiving/demux_receiver_loopback.rs` |
    | HLS segment probe | the `hls/{ffmpeg,tsp}-pull` cells and `crates/tst-hls/tests/net_hls_e2e.rs` |
    | PTS-rollover and PCR-jitter checks | removed; they never ran |
13. **Tag** the qualified date commit, following the release procedure below.

## Release procedure

(`vX.Y.Z` below stands for the release tag, e.g. `v0.3.0`.)

1. **Tag and push** (only after explicit maintainer confirmation — Maven
   Central is immutable):
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
2. The tag triggers all three publish workflows (`crates-io.yml` is
   covered in its own section below):
   - **python-wheels.yml** builds wheels + sdist and **publishes to PyPI via
     OIDC trusted publishing.** **Watch the publish job:** if any required
     wheel leg is cancelled or stuck queued, the `needs:`-gated publish job
     is *skipped* (not failed); a skipped publish means nothing reached PyPI
     (the v0.1.0 failure mode).
   - **jvm-jar.yml** builds the 4 native libs, assembles the fat jar, then the
     `publish` job uploads a signed bundle to the **Central Portal (staged)**.
3. **Release Maven manually:** https://central.sonatype.com → Deployments,
   inspect the staged `org.tstrans:tstrans-jvm:X.Y.Z` bundle (validation must
   pass), then click **Publish**. It appears on Maven Central shortly after.
4. **Verify BEFORE advertising:**
   - `pip install tstrans==X.Y.Z` resolves and imports.
   - A Gradle / Maven resolve of `org.tstrans:tstrans-jvm:X.Y.Z` succeeds.
   - Every crate page at `https://crates.io/crates/<pkg>` shows X.Y.Z and
     the docs.rs builds are green (see the crates.io section below).
   Only **after** all three verify, announce the release.
5. **GitHub Release.** The tag push also runs `release-qualify.yml` again.
   The publish workflows above do not wait for it: `jvm-jar.yml` and
   `python-wheels.yml` publish on their own tag triggers, in parallel,
   whatever that run's verdict. The pre-tag dispatch in step 10 of the
   checklist is the real gate. On a GO the tag run attaches the bundle and
   the C package to a **draft** Release for the tag. Edit that draft's notes
   from the CHANGELOG highlights block and publish it; do not create a second
   Release by hand. If the record is not a GO, no draft is created: read the
   reasons, then create the Release by hand.

## crates.io publish (Rust crates)

`ts-transformer` publishes its 11 publishable Rust library crates to
crates.io. Since the v0.4.0 first publish this is **wired to CI via
Trusted Publishing (OIDC)**: pushing a `v*` release tag runs
`.github/workflows/crates-io.yml`, which publishes all 11 crates in
dependency order with per-layer OIDC-minted tokens — no maintainer-held
API token. The crates.io side of the binding (per crate: repository
`aklofas/ts-transformer`, workflow `crates-io.yml`, environment
`crates-io`) lives at each crate's Settings → Trusted Publishing page;
renaming the workflow file or environment breaks it on both sides.

**At release time:** the tag you push for PyPI/Maven triggers this too.
Watch the `crates-io` run; after it goes green do the same per-crate
verification as always — `https://crates.io/crates/<pkg>` shows the new
version, docs.rs builds are green. **First green OIDC publish → revoke
the maintainer API token** (it becomes dead weight; the OIDC path is
then the only publisher).

**If the run fails mid-sequence:** fix the cause and re-run the job —
the layer script (`scripts/release/publish-crates-layer.sh`) skips
crates already live at their current version, so re-runs fast-forward
through the published prefix (crates.io publishes are immutable; that
skip is what makes re-running safe). Versions are resolved per crate
from `cargo metadata` — `tstrans-mbedtls-src` carries a build-metadata
suffix (`X.Y.Z+<mbedtls-version>`), so there is no single shared
version string.

**Rehearsal:** `workflow_dispatch` (or a PR touching the workflow file)
runs the identical sequence as `cargo publish --dry-run` — packages and
verify-builds everything, uploads nothing, needs no token. The rehearsal
resolves internal dependencies from the tree (a `[patch.crates-io]` table
written by `scripts/release/dry-run-patch-config.sh`, dry-run only), so it
also passes after a release version sweep, when the index cannot yet
satisfy the bumped `version =` keys. To rehearse locally at such a
version: `bash scripts/release/dry-run-patch-config.sh >> .cargo/config.toml`,
run the layer script with `PUBLISH=0`, then delete `.cargo/config.toml`.

### Manual fallback (token-based)

If the OIDC path is unavailable, the pre-0.4.1 manual flow below still
works with a maintainer API token (scope: publish-update). It follows
the same order and verification steps the workflow automates.

**Publish order** (topo-sorted from the dependency graph — a crate cannot
publish until every crate it depends on with a `version =` key is already
live on the index):

1. `tstrans-mbedtls-src` (no internal dependencies — the base of the chain)
2. `tstrans-srt-sys`, `tstrans-rist-sys` (each depends on `tstrans-mbedtls-src`
   as a build-dependency; these two can publish in either order relative to
   each other)
3. `tst-core` (no internal path dependencies with a `version` key)
4. `tst-pipeline` (depends on `tst-core`)
5. `tst-udp`, `tst-tcp`, `tst-hls`, `tst-rtp`, `tst-srt`, `tst-rist` (each
   depends on `tst-core` and `tst-pipeline` — the shared
   `tst_pipeline::binding` layer — with pinned `version` keys; `tst-srt`
   additionally depends on `tstrans-srt-sys` and `tst-rist` on
   `tstrans-rist-sys` — both already live from layer 2. Publish in any
   order once layers 2-4 are live)

For each crate, in the order above:

```bash
cargo publish --dry-run -p <pkg>   # catches metadata/packaging problems for free
cargo publish -p <pkg>
```

**Wait for index visibility between layers** — crates.io's index takes on
the order of tens of seconds to a couple of minutes to propagate a fresh
publish; a same-session `cargo publish` for a *dependent* crate can otherwise
fail with the same "no matching package named ..." error the
`publish-package-sanity` CI rail's manual-fallback branch works around
pre-first-publish (see below). Confirm the crate's `https://crates.io/crates/<pkg>`
page is live before moving to the next layer, not just that the `cargo
publish` command returned success.

**Post-publish, before moving on:**
- Check `https://crates.io/crates/<pkg>` resolves and shows the expected
  version.
- Check the docs.rs build status at `https://docs.rs/<pkg>` — docs.rs builds
  automatically on publish; a build failure there (e.g. the vendored native
  source failing to compile in docs.rs's sandboxed build environment) is a
  real signal worth investigating even though it doesn't block the publish
  itself.

**Rail note:** the `publish-package-sanity` CI rail
(`scripts/check/rust/publish-package-sanity.sh`) cannot run a real `cargo
package --no-verify` on `tstrans-srt-sys` / `tstrans-rist-sys` whenever the
`tstrans-mbedtls-src` version they pin is not yet live on crates.io
(Cargo's packaging step validates every
path-dependency-with-a-`version`-key against the real index, independent
of `--no-verify`). That happens in two windows: before the first-ever
publish, and — recurring every release — between a version sweep and the
new version's publish. In both, the rail falls back to a manual tar+gzip
size reconstruction of the exact `cargo package --list` file set and
self-heals the moment the new `tstrans-mbedtls-src` version is live.

## Notes

- Maven Central releases are **permanent/immutable** — a bad bundle means
  shipping a corrective `0.2.1`, never overwriting. That is why the publish
  stages for manual inspection (`publishToMavenCentral(automaticRelease =
  false)` in `bindings/jvm/build.gradle.kts`). Confirm that flag's current
  value before publishing if you intend to change the staged behavior.
- crates.io publishes are likewise **permanent/immutable** — a published
  version can never be overwritten or deleted, only `cargo yank`ed (which
  hides it from new dependency resolution but does not remove it from the
  index). A bad crates.io publish means shipping a corrective patch version,
  same as Maven above.
- If the Portal upload fails with an opaque error, re-run the publish with
  `--info` added to the `./gradlew publishToMavenCentral` invocation to capture
  the HTTP response body from the Portal.
- The fat jar covers linux-x86_64/aarch64, macos-aarch64, windows-x86_64.
  macOS Intel (`macos-x86_64`) is deferred — see
  [deferred-features.md](deferred-features.md). Intel-Mac Python users install
  from the sdist.
