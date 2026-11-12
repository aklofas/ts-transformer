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
# machine in this invocation, so the libc, valgrind and the toolchain are the
# same on both sides; no absolute count is ever stored. The harness source and
# the judge come from the checkout this script runs from and are compiled
# against both commits. When the harness does not build against the old commit
# nothing is compared, and that fails unless the accepted file says so.
#
# Limits: instruction counts do not see cache, alignment or branch-prediction
# effects, and a compiler change moves every workload at once.
#
# Exit 0 pass, 1 fail, 2 a missing tool or a bad argument.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
old_ref=""; new_ref=""; out=""; threshold=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --threshold) threshold="$2"; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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
cleanup() {
  for side in old new; do
    git -C "$ROOT" worktree remove --force "$out/$side" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT

# Build the harness of this checkout against one commit; print the executable.
build() { # <side> <sha>
  local side="$1" wt="$out/$1" manifest
  git -C "$ROOT" worktree add --detach "$wt" "$2" >/dev/null 2>&1
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

new_bin="$(build new "$new_sha" || true)"
if [ -z "$new_bin" ]; then
  echo "bench-ir: the harness does not build against $new_ref; see $out/build-new.log" >&2
  exit 2
fi
old_bin="$(build old "$old_sha" || true)"
[ -n "$old_bin" ] || echo "bench-ir: the harness does not build against $old_ref ($out/build-old.log): nothing is compared" >&2

"$new_bin" gen-stream "$out/stream.ts"

# Total instructions of one workload run under callgrind.
count() { # <binary> <workload> <iterations>
  valgrind --tool=callgrind --callgrind-out-file=/dev/null "$1" run "$2" "$3" "$out/stream.ts" 2>&1 >/dev/null \
    | sed -n 's/^==[0-9]*== Collected : \([0-9]*\).*/\1/p'
}

"$new_bin" list > "$out/workloads.txt"
jobs_max="$(nproc 2>/dev/null || echo 2)"
while read -r workload iterations; do
  {
    new="$(count "$new_bin" "$workload" "$iterations")"
    old=""
    [ -z "$old_bin" ] || old="$(count "$old_bin" "$workload" "$iterations")"
    printf '%s\t%s\t%s\t%s\n' "$workload" "$iterations" "$old" "$new" > "$out/count-$workload.tsv"
  } &
  while [ "$(jobs -rp | wc -l)" -ge "$jobs_max" ]; do wait -n; done
done < "$out/workloads.txt"
wait
: > "$out/results.tsv"
while read -r workload _; do cat "$out/count-$workload.tsv" >> "$out/results.tsv"; done < "$out/workloads.txt"

python3 "$ROOT/scripts/dev/bench_ir.py" --results "$out/results.tsv" \
  --old "$old_ref" --new "$new_ref" --old-sha "$old_sha" --new-sha "$new_sha" \
  --accepted "$ROOT/tests/coverage/bench-accepted.toml" ${threshold:+--threshold "$threshold"} \
  --tool "$(valgrind --version)" --json "$out/bench-ir.json"
