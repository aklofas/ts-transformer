#!/usr/bin/env bash
# binding-records: the Python and JVM projections of every record declared with
# tst_core::binding_record! are generated: the converter pair in
# bindings/python/src/generated/<family>.rs, the JNI pair in
# bindings/jvm/src/generated/<family>.rs, the dataclass blocks in the Python
# module and its stub, and the Java record. This rail fails when a committed
# copy, or a generated file that does not exist yet, differs from a fresh
# render. The registry is crates/tst-pipeline/tests/tools/print_records/schema.rs.
#
# Regenerate: cargo run -p tst-pipeline --bin print-records -- --write
# Overridable: BINDING_RECORDS_ROOT (a tree to check instead of this one).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 \
    cargo run -q -p tst-pipeline --bin print-records -- --check --root "${BINDING_RECORDS_ROOT:-$ROOT}"
