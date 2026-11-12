#!/usr/bin/env bash
# Compare tst-core's instruction counts between two commits.
#
# Wall-clock benches cannot gate a release: the placement of unchanged code
# moves a criterion number by up to a fifth between two builds. Instruction
# counts do not move with placement, so this script builds the fixed-iteration
# harness (crates/tst-core/benches/ir.rs) against both commits, runs every
# workload under `valgrind --tool=callgrind` and hands the totals to
# scripts/dev/bench_ir.py. A workload that grew by more than the threshold
# fails unless tests/coverage/bench-accepted.toml accepts it.
#
# usage: scripts/dev/bench-ir.sh <old-ref> <new-ref> [--out <dir>] [--threshold <percent>]
#   <out>/results.tsv   workload, iterations, old total, new total
#   <out>/bench-ir.json the judged record
#
# Both commits are built in detached worktrees under <out> and run on this
# machine in this invocation, with the toolchain this checkout pins, so the
# libc, valgrind and the compiler are the same on both sides; no absolute
# count is ever stored. The harness source and the judge come from the
# checkout this script runs from and are compiled against both commits. When
# the harness does not build against the old commit nothing is compared, and
# that fails unless the accepted file says so.
#
# Limits: instruction counts do not see cache, alignment or branch-prediction
# effects. Because both sides use this checkout's compiler, a toolchain bump
# between the two commits does not show up either. A workload that grows a
# buffer by reallocation counts up to about 3 % differently from one
# invocation to the next, because the heap layout follows the environment and
# the argument lengths; both sides of one invocation share those, so compare
# deltas within a run, never counts across runs.
#
# Exit 0 pass, 1 fail, 2 a missing tool, a bad argument or a workload that
# did not run.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
old_ref=""; new_ref=""; out=""; threshold=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --threshold) threshold="$2"; shift 2 ;;
    -h|--help) sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "bench-ir: unknown argument $1" >&2; exit 2 ;;
    *) if [ -z "$old_ref" ]; then old_ref="$1"; elif [ -z "$new_ref" ]; then new_ref="$1"; else
         echo "bench-ir: unexpected argument $1" >&2; exit 2; fi; shift ;;
  esac
done
if [ -z "$old_ref" ] || [ -z "$new_ref" ]; then
  echo "usage: scripts/dev/bench-ir.sh <old-ref> <new-ref> [--out <dir>] [--threshold <percent>]" >&2
  exit 2
fi
command -v valgrind >/dev/null || { echo "bench-ir: valgrind is not installed" >&2; exit 2; }
old_sha="$(git -C "$ROOT" rev-parse -q --verify "$old_ref^{commit}")" || { echo "bench-ir: no commit $old_ref" >&2; exit 2; }
new_sha="$(git -C "$ROOT" rev-parse -q --verify "$new_ref^{commit}")" || { echo "bench-ir: no commit $new_ref" >&2; exit 2; }

[ -n "$out" ] || out="$(mktemp -d)"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
# A reused <out> may keep its target directories, but never a result or a
# checkout of an earlier run: either would be judged as this run's.
rm -f "$out"/count-*.tsv "$out"/valgrind-*.log "$out/results.tsv" "$out/bench-ir.json"
for side in old new; do
  [ ! -e "$out/$side" ] || { echo "bench-ir: $out/$side exists; remove it (git worktree remove --force) and run again" >&2; exit 2; }
done
# One compiler for both sides: each worktree's own rust-toolchain.toml would
# otherwise pick the compiler its commit pinned.
if [ -z "${RUSTUP_TOOLCHAIN:-}" ]; then
  RUSTUP_TOOLCHAIN="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$ROOT/rust-toolchain.toml")"
  [ -n "$RUSTUP_TOOLCHAIN" ] || { echo "bench-ir: no channel in $ROOT/rust-toolchain.toml" >&2; exit 2; }
fi
export RUSTUP_TOOLCHAIN
cleanup() {
  for side in old new; do
    git -C "$ROOT" worktree remove --force "$out/$side" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

# Build the harness of this checkout against one commit; print the executable.
build() { # <side> <sha>
  local side="$1" wt="$out/$1" manifest
  git -C "$ROOT" worktree add --detach "$wt" "$2" >/dev/null 2>&1 || return 1
  cp "$ROOT/crates/tst-core/benches/ir.rs" "$wt/crates/tst-core/benches/ir.rs"
  manifest="$wt/crates/tst-core/Cargo.toml"
  if ! grep -Eq '^name *= *"ir"$' "$manifest"; then
    printf '\n[[bench]]\nname    = "ir"\nharness = false\n' >> "$manifest"
  fi
  ( cd "$wt" && CARGO_TARGET_DIR="$out/target-$side" \
      cargo bench -p tst-core --bench ir --no-run --message-format=json 2> "$out/build-$side.log" ) \
    | python3 -c '
import json, sys
exe = ""
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m.get("target", {}).get("name") == "ir" and m.get("executable"):
        exe = m["executable"]
print(exe)'
}

# The build log's tail goes to stderr: on a CI runner the file itself is gone
# with the job.
new_bin="$(build new "$new_sha" || true)"
if [ -z "$new_bin" ]; then
  echo "bench-ir: the harness does not build against $new_ref:" >&2
  tail -n 40 "$out/build-new.log" >&2 || true
  exit 2
fi
old_bin="$(build old "$old_sha" || true)"
if [ -z "$old_bin" ]; then
  echo "bench-ir: the harness does not build against $old_ref, so nothing is compared:" >&2
  tail -n 40 "$out/build-old.log" >&2 || true
fi

"$new_bin" gen-stream "$out/stream.ts"

# Total instructions of one workload run under callgrind; empty when the
# workload failed. valgrind passes the program's exit status on and prints a
# total even for a run that panicked, so the status decides.
count() { # <side> <binary> <workload> <iterations>
  if valgrind --tool=callgrind --callgrind-out-file=/dev/null "$2" run "$3" "$4" "$out/stream.ts" \
       > "$out/valgrind-$1-$3.log" 2>&1; then
    sed -n 's/^==[0-9]*== Collected : \([0-9]*\).*/\1/p' "$out/valgrind-$1-$3.log"
  fi
}

"$new_bin" list > "$out/workloads.txt"
jobs_max="$(nproc 2>/dev/null || echo 2)"
while read -r workload iterations; do
  {
    new="$(count new "$new_bin" "$workload" "$iterations")"
    old=""
    [ -z "$old_bin" ] || old="$(count old "$old_bin" "$workload" "$iterations")"
    printf '%s\t%s\t%s\t%s\n' "$workload" "$iterations" "$old" "$new" > "$out/count-$workload.tsv"
  } &
  while [ "$(jobs -rp | wc -l)" -ge "$jobs_max" ]; do wait -n || true; done
done < "$out/workloads.txt"
wait
# Every workload must have counted on each side that built: a failed run is
# an error with the workload's name, never a row the judge might read as
# absent or as a large improvement.
: > "$out/results.tsv"
while read -r workload _; do
  IFS=$'\t' read -r _ _ old new < "$out/count-$workload.tsv" || true
  for side in new ${old_bin:+old}; do
    if [ -z "${!side}" ]; then
      echo "bench-ir: workload $workload failed on the $side side:" >&2
      tail -n 20 "$out/valgrind-$side-$workload.log" >&2 || true
      exit 2
    fi
  done
  cat "$out/count-$workload.tsv" >> "$out/results.tsv"
done < "$out/workloads.txt"

python3 "$ROOT/scripts/dev/bench_ir.py" --results "$out/results.tsv" \
  --old "$old_ref" --new "$new_ref" --old-sha "$old_sha" --new-sha "$new_sha" \
  --accepted "$ROOT/tests/coverage/bench-accepted.toml" ${threshold:+--threshold "$threshold"} \
  --tool "$(valgrind --version)" --json "$out/bench-ir.json"
