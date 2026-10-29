# Releasing

A single `v*` tag publishes to **three immutable registries** through
three workflows — treat them as one release transaction:

| Registry | Artifact(s) | Workflow | Publish mode |
|---|---|---|---|
| **crates.io** | the 12 Rust library crates, in dependency-layer order | `crates-io.yml` | automatic (OIDC trusted publishing, per-layer tokens) |
| **PyPI** | `tstrans` wheels + sdist | `python-wheels.yml` | automatic (OIDC trusted publishing) |
| **Maven Central** | `org.tstrans:tstrans-jvm` fat JAR | `jvm-jar.yml` | **staged** — a maintainer inspects and clicks Publish |

A GitHub Release (notes seeded from the CHANGELOG highlights block) completes
the set — 15 published artifacts per release in total.

The 1.0 release has further conditions, listed with the check that proves
each in the [1.0 exit criteria](/docs/project/1.0-exit-criteria.md).

> **Release state.** All three registries are
> live: crates.io (all 12 crates; the first 11 published at 0.4.0 and
> OIDC-wired since, `tst-webrtc` from 0.9.0), PyPI `tstrans` (since 0.2.0),
> and Maven Central `org.tstrans:tstrans-jvm` (since 0.1.0). Standing cautions: every one of
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
   - **the four fuzz lockfiles** (`crates/{tst-core,tst-srt,tst-rtp,
     tst-webrtc}/fuzz/Cargo.lock`) — they record the workspace crates'
     versions too, and CI's fuzz compile step and release-qualify both run
     `cargo metadata --locked` there. Refresh each with
     `(cd crates/<c>/fuzz && cargo +nightly metadata --format-version 1 >/dev/null)`
     — unlocked metadata updates only the path dependencies' versions and
     leaves the registry pins alone — and commit them with the sweep;
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
   the soak, both on the frozen commit. Four scripts under `scripts/interop/`
   carry the steps; each prints `<stage> rc=N` lines and one final marker a
   watcher can anchor on:
   - Provision the VM. Its instance role reads the corpus bucket, which holds
     the real captures and the previous release's corpus baseline. Then, on
     the VM, `release-vm-setup.sh <full sha>`: packages, a Rust toolchain,
     the AWS CLI, the UDP receive-buffer ceiling, Google Chrome, and a clean
     checkout of the frozen commit with `tst-interop` built in release mode
     (`SETUP-DONE rc=0`).
   - `aws s3 sync s3://<bucket>/baselines/vX.Y.Z/ ~/baseline-vX.Y.Z/` fetches
     the previous release's digests. The 0.9.0 digests were never promoted
     under their tag's name, so before the first comparison against them
     copy them there once:
     ```bash
     aws s3 cp --recursive \
       s3://<bucket>/baselines/unreleased-9b8acb3b6fae559dc8b4dc0c4bbcc31af097d234/ \
       s3://<bucket>/baselines/v0.9.0/
     ```
   - A baseline written before the sweep kept a `manifest.txt` (the 0.9.0
     digests are one) is refused until its manifest is written, once, before
     the first comparison against it:
     ```bash
     ~/ts-transformer/target/release/tst-interop corpus manifest \
       ~/baseline-vX.Y.Z --expect-files 251
     ```
     The same first comparison changes the digest of every capture whose
     ST 0605 packs did not decode before. These are the captures whose
     baseline digest has undecoded KLV records; review each and pass its
     SHA-256 to the `release-vm-corpus.sh` run below as
     `--accept <sha256> --because <text>`:
     ```bash
     jq -r 'select(.prefix_seconds == null and .configs.default.klv_unknown > 0) | .sha256' ~/baseline-vX.Y.Z/*.json
     ```
     [Real-capture testing](/docs/project/real-capture-testing.md) has the
     rules behind both.
   - `release-vm-corpus.sh <full sha> --baseline ~/baseline-vX.Y.Z`
     runs the corpus phase: `corpus inventory` lists the captures,
     `corpus sweep` digests each one and compares it with the baseline,
     `corpus replay` sends each selected capture through the transports and
     judges what arrives, `corpus report` writes the result and its archive
     row, and the digests go to the bucket as the commit's unreleased
     baseline (`CORPUS-DONE rc=0`). The script refuses to run without
     either `--baseline DIR` or `--no-baseline` (a first run, with no
     previous release to compare with), and the archive row's note names
     the baseline's manifest digest or says none was compared. A failure
     stops the checklist here; it is a finding to fix, and the fix restarts
     at step 2. A reviewed change is accepted with
     `--accept <sha256> --because <text>`.
   - From a machine with an authenticated `gh`,
     `release-vm-soak.sh ci-green <full sha> ubuntu@<vm> --dispatch-wheels <ref>`
     dispatches the wheels dry run and creates `~/ci-green` on the VM once
     `ci`, `jvm-jar` and `python-wheels` have each succeeded on the commit.
     `<ref>` is a branch or tag whose head is the frozen commit (`main`
     while it still points there; otherwise push one, for example
     `git push origin <sha>:refs/heads/release/X.Y.Z`): a workflow dispatch
     takes no commit, and the script refuses a ref that has moved on.
   - On the VM, `release-vm-soak.sh launch <full sha> --outdir ~/soak-X.Y.Z
     --wait-marker ~/ci-green` waits for that marker, runs the pre-flight
     (soak.sh's own memory gate and the rtsp-publish disk rule, which needs
     1.2 × the pre-generated stream's peak free — about 141 GiB for 72 hours),
     launches `soak.sh` detached so no session limit can end it, and reports
     `SOAK-UP roles=N running_until=…` only once every role is alive. A
     refused pre-flight names the largest directories beside the outdir: a
     previous run's outdir is the usual cause, and
     `release-vm-soak.sh stop --outdir <old>` keeps its logs and frees its
     media. Run `launch` detached too (it waits for the marker and then for
     the roles, which can be an hour).
   - Check the run at 24 and 48 hours without touching it:
     `release-vm-soak.sh status --outdir ~/soak-X.Y.Z` lists every role
     alive, the absence of the fail-fast marker, and the SRT receiver's
     `reconnects`, which reads 4 and then 8.
   - When the run ends, harvest the corpus run directory and the soak
     directory. Write a checksum list for each on the VM and check the
     copies against it.
   - The `tst-interop report soak` that closes the run must print
     `overall_pass=true`, and the script exits 0. Any other verdict is a
     finding, not a rerun.
   - A commit that replaces the frozen one restarts the whole step on the
     same VM: `release-vm-soak.sh stop` on the running soak, then setup's
     checkout stage (or `release-vm-corpus.sh` on the new sha, which checks
     it out), then the chain above.
7. **The archive-entry PR.** A docs-only PR adds two rows to
   `tests/coverage/evidence-archives.toml`, each with the frozen commit as
   its `tree`:
   - the soak, with `expected_duration_s = 259200` and `legs = [...]`
     copied from the `legs=…` list on the last line `report soak` printed;
   - the corpus run, with the counts its archive row carries. Its `files`
     must equal `expected_files` in the file's `[corpus]` table. When the
     corpus has changed, the same PR updates `expected_files`.
   - `overall_pass` exactly as the harness printed it. A failed harness
     verdict stays `false`; the maintainer's review goes in
     `[archive.waiver]` (the verdicts it covers, the reason, the reviewer,
     the date and the release that fixes the cause). A release needs a raw
     pass, so a harness defect is fixed and the soak rerun.

   The same PR adds the soak to the runs table on the
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
    also needs a passing soak archive that declares 259 200 seconds and all
    five legs (srt, rist, rtsp-publish, rtsp-publish-es, whep), and a
    passing corpus archive for the soaked tree that swept the declared
    `expected_files` count and replayed all five paths (at least one file,
    none failed), with no change outside `CHANGELOG.md`,
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
    failed, short, superseded or leg-short soak archive, a missing or
    failed corpus archive, a corpus run that swept a different file
    count). A real leg failure is a finding to fix, not a rerun; rerun
    only a failure that belongs to a ledgered flake class.

    The ABI leg compares against the previous tag's C package from that
    tag's Release only when `SHA256SUMS` and the package's build-provenance
    attestation, signed by `release-qualify.yml` at that tag, verify it.
    Otherwise it builds the previous tag from source, and the record's ABI
    source reads `built: <reason>` instead of `attested`. Two cases always
    take the source build, by design: a fork, since the check names the
    signing workflow by the `aklofas/ts-transformer` slug, and a runner
    whose `gh` lacks the attestation flags, which records
    `built: attestation verify failed`.

    The run's bundle holds `qualification.json`, the rendered
    `qualification.md`, `TREE-EQUIVALENCE.txt`, the SBOMs, `SHA256SUMS`, the
    build attestations and the C package. Download it from the run's
    artefacts. `TREE-EQUIVALENCE.txt` names the soaked and the qualified
    commit (and the tag on a tag push), shows that nothing under `crates/` or
    `bindings/` changed between them, and lists every path that did; the
    record's `release_vm.soak.equivalence` lists the release documents among
    them.
11. **Commit the record.** In a docs-only PR, add the run's
    `qualification.json` as `docs/project/qualification.json` (CI's
    `check-qualification` step verifies the page matches it) and render the
    evidence page's generated block:
    ```bash
    bash scripts/gen/qualification.sh --write
    ```
    The same PR updates the
    [release findings ledger](/docs/project/release-findings.md): one row
    per defect found during this release-candidate cycle, with the class of
    test that now covers it.
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
13. **Tag** the commit that carries the qualification record from step 11,
    following the release procedure below. Three commits and two records are
    involved, each in one place:
    - the committed `docs/project/qualification.json` was collected on the
      commit it names (`source.sha`), the one step 10 qualified; the tag sits
      on the record PR's merge, which changes release documents only;
    - the tag push runs `release-qualify` again on the tagged commit, and
      that run's record (with `source.tag`) is in the Release bundle beside
      its `TREE-EQUIVALENCE.txt`;
    - the heavy evidence, the soak and the corpus run, ran on the soaked
      tree both records name in `release_vm`, and the equivalence file shows
      that tree differs from the qualified one in release documents only.

## Release procedure

(`vX.Y.Z` below stands for the release tag, e.g. `v0.3.0`.) The tag goes on
the record PR's merge commit (step 13 above). The tag push's own
qualification record and tree-equivalence proof are attached to the draft
Release; the committed record stays the one step 10 collected.

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
5. **Promote the corpus baseline.** The corpus run pushed its digests as the
   soaked commit's unreleased baseline; the next release compares with them
   under the tag's name:
   ```bash
   aws s3 cp --recursive s3://<bucket>/baselines/unreleased-<soaked sha>/ \
     s3://<bucket>/baselines/vX.Y.Z/
   ```
   `<soaked sha>` is the full commit the record's `release_vm.corpus.tree`
   names.
6. **GitHub Release.** The tag push also runs `release-qualify.yml` again.
   The publish workflows above do not wait for it: `jvm-jar.yml` and
   `python-wheels.yml` publish on their own tag triggers, in parallel,
   whatever that run's verdict. The pre-tag dispatch in step 10 of the
   checklist is the real gate. On a GO the tag run attaches the bundle and
   the C package to a **draft** Release for the tag. Edit that draft's notes
   from the CHANGELOG highlights block and publish it; do not create a second
   Release by hand. If the record is not a GO, no draft is created: read the
   reasons, then create the Release by hand.

## crates.io publish (Rust crates)

`ts-transformer` publishes its 12 publishable Rust library crates to
crates.io. Since the v0.4.0 first publish this is **wired to CI via
Trusted Publishing (OIDC)**: pushing a `v*` release tag runs
`.github/workflows/crates-io.yml`, which publishes all 12 crates in
dependency order with per-layer OIDC-minted tokens — no maintainer-held
API token. The crates.io side of the binding (per crate: repository
`aklofas/ts-transformer`, workflow `crates-io.yml`, environment
`crates-io`) lives at each crate's Settings → Trusted Publishing page;
renaming the workflow file or environment breaks it on both sides. Only the
tag-only `publish` job enters the `crates-io` environment, and the
environment's deployment policy in the repository's Settings → Environments
admits `v*` tags only. The policy is a repository setting, not a file in the
tree, so the maintainer sets it once and checks it before a release.

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

**Rehearsal:** the workflow's `rehearse` job, on `workflow_dispatch` or a PR
touching the workflow file or the release scripts, runs the identical
sequence as `cargo publish --dry-run` — packages and verify-builds
everything, uploads nothing. It has neither the environment nor an OIDC
token, so it cannot publish. The rehearsal
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
5. `tst-udp`, `tst-tcp`, `tst-hls`, `tst-rtp`, `tst-webrtc`, `tst-srt`,
   `tst-rist` (each depends on `tst-core` and `tst-pipeline` — the shared
   `tst_pipeline::binding` layer — with pinned `version` keys; `tst-srt`
   additionally depends on `tstrans-srt-sys` and `tst-rist` on
   `tstrans-rist-sys` — both already live from layer 2 — and `tst-webrtc`
   on `tst-rtp`, so publish `tst-webrtc` after `tst-rtp` is visible on the
   index. The rest publish in any order once layers 2-4 are live)

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

- Every workflow and composite action pins its actions by commit SHA,
  including `dtolnay/rust-toolchain`, which used to follow its moving
  `stable` ref. Dependabot moves those pins weekly; a toolchain or action
  update arrives as its pull request, never by a tag moving.
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
