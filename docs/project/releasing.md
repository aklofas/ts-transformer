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

> **Release state (updated at the 0.5.0 gate).** All three registries are
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

1. **Version consistency (REL-01 preflight).** The rail checks six sources —
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
     tst-hls,tst-rtp}/Cargo.toml` (12 keys total: `tst-srt` has 3 including a
     dev-dependency, `tst-rist` has 2, the rest have 1 each). Miss one of
     these and the crates.io ordered publish below fails at layer 2 — after
     the irreversible layer-1 publish already landed. `release-version-
     consistency.sh` now asserts these match the workspace version (see
     `RVC_DEP_TOMLS` in the script), so this step is CI-enforced, not just
     convention;
   - the workspace `Cargo.lock`, the fuzz-workspace lockfiles
     (`crates/*/fuzz/Cargo.lock`), **and the embedded sub-project lockfiles**
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
2. **`main` is green** — `ci`, `jvm-jar`, and `python-wheels` all passing on
   the commit you intend to tag.
3. **Dry-run the wheels** (recommended):
   `gh workflow run python-wheels.yml` builds every wheel + sdist and skips
   publish (publish is tag-gated). Confirm all legs are green, and watch for a
   leg sitting *queued* — that is the v0.1.0 failure mode.
4. **Refresh the published interop/soak evidence.** Run a full local
   `scripts/interop/run-matrix.sh` matrix and note the resulting census
   (total/PASS/FAIL/EXPECTED-UNSUPPORTED/SKIPPED) against what
   [`docs/project/validation-evidence.md`](/docs/project/validation-evidence.md) currently
   claims — a changed census (a new unexpected `FAIL`, or a documented gap
   now passing) means the page needs updating before the release, not after.
   Also confirm the linked public CI run
   (`.github/workflows/interop.yml`, weekly + `workflow_dispatch`) is the
   latest one and still green; update the cited run URL if a newer one has
   landed since the page was last touched.
5. **CHANGELOG:** retitle the `[Unreleased]` section to `[X.Y.Z] — <date>`
   and open a fresh `[Unreleased]` stub. The "Release highlights" block is
   the seed for the GitHub Release notes.
6. - [ ] Decide which Tier-B steps the interop matrix now owns (see the
   "`release-validation.sh` consolidation" entry in
   [`deferred-features.md`](/docs/project/deferred-features.md)) — retire
   the duplicated player-decode / ffmpeg round-trip steps or record why not.

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
   Only **after** all three verify, announce the release (GitHub Release
   notes seeded from the CHANGELOG highlights block).

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
verify-builds everything, uploads nothing, needs no token. Caveat:
rehearsals only fully pass while the workspace version is a *published*
version; after a release version sweep (pre-tag), layer-2+ dry-runs fail
dependency resolution because the index can't satisfy the bumped
`version =` keys yet. Rehearse between releases, not after the sweep.

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
   depends on `tst-core`; `tst-srt` additionally depends on
   `tstrans-srt-sys` and `tst-rist` on `tstrans-rist-sys` — both already
   live from layer 2. `tst-pipeline` appears only as an unversioned
   dev-dependency in some of these crates' test suites — not a real,
   version-pinned publish-order dependency. Publish in any order once
   layers 2-4 are live)

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
