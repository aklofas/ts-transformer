#!/usr/bin/env bash
# Emit a `[patch.crates-io]` table that points every PUBLISHABLE workspace
# crate at its in-tree path — for the crates.io DRY-RUN rehearsal only.
#
#   usage: dry-run-patch-config.sh >> .cargo/config.toml
#
# Why: `cargo publish --dry-run -p <crate>` packages the crate with its
# path dependencies rewritten to registry requirements (`tst-core =
# "^0.7.1"`), then resolves those against the crates.io index. Between a
# release version sweep and its tag publish the index cannot satisfy the
# bumped requirements (it still holds the previous version), so every
# layer-2+ rehearsal fails with "failed to select a version for the
# requirement" — on the pull_request path-filter run of crates-io.yml as
# much as on a workflow_dispatch. A config-level `[patch.crates-io]`
# entry per publishable crate makes the resolver take the in-tree copy
# (whose version matches the requirement by construction, asserted by
# scripts/check/repo/release-version-consistency.sh) instead of the index,
# so the rehearsal exercises exactly what the tag publish will ship:
# packaging, manifest normalisation and the verify build of the `.crate`.
# cargo 1.85 has no multi-package overlay for `cargo package` (the
# `-p a -p b` form still resolves each crate's deps against the index),
# so the patch table is the only token-free way to rehearse ahead of
# the index.
#
# NEVER apply this to the real publish: `cargo publish` must resolve
# against the registry so a dependency that is not live yet fails loudly
# instead of being papered over by the tree. crates-io.yml writes the
# table only when its `publish` output is 0.
#
# Caveat the patch cannot hide: a STALE `version =` key (e.g. `^0.6.0`
# after a bump to 0.7.1) is not satisfied by the in-tree 0.7.1, so the
# resolver falls back to the index's 0.6.0 and the dry-run passes with
# the stale pin. release-version-consistency.sh (run first in the
# workflow) is the guard for that case, not this file.
#
# Publishable = every workspace member without `publish = false`
# (`.publish == null` in cargo metadata; `publish = false` renders as []).
# Paths are absolute so the table is valid from any config location.
set -euo pipefail

echo "[patch.crates-io]"
cargo metadata --format-version 1 --no-deps \
  | jq -r '.packages[]
           | select(.publish == null)
           | "\(.name) = { path = \"\(.manifest_path | sub("/Cargo\\.toml$"; ""))\" }"' \
  | sort
