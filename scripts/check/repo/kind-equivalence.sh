#!/usr/bin/env bash
# kind-equivalence: the binding kind tables are generated from
# crates/tst-pipeline/src/binding/kind.rs (the `kinds!` rows and
# `Domain::kinds`): scripts/ratchets/kind-equivalence.tsv, the kind-enum
# blocks in bindings/python/python/tstrans/exceptions.py and in the thirteen
# bindings/jvm/src/main/java/org/tstrans/*Exception.java. This rail fails
# when a committed copy differs from a fresh render. The import-time
# check_error_kinds (Python) and verify_kind_tables (JVM) stay as the
# runtime check.
#
# Regenerate: cargo run -p tst-pipeline --bin print-kinds -- --write
# Overridable: KIND_EQUIV_ROOT (a tree to check instead of this one; the
# ratchet self-test points it at a planted copy).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 \
    cargo run -q -p tst-pipeline --bin print-kinds -- --check --root "${KIND_EQUIV_ROOT:-$ROOT}"
