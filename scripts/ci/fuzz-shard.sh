#!/usr/bin/env bash
# Runs one shard of the fuzz leg: each of the shard's targets (dealt by
# scripts/ci/fuzz_shards.py from tests/coverage/fuzz-targets.toml) under
# `cargo +nightly fuzz run` for the given time, one after another.
#
#   fuzz-shard.sh <shard> <shards> <seconds-per-target> <out-dir>
#
# Writes <out-dir>/fuzz-shard-<shard>.json, one row per target:
#   ok     exit 0 after at least the given time
#   crash  a non-zero exit that left artefact files: the target ran, and
#          `artefacts` (the files in its artefact directory) counts the crashes
#   error  a non-zero exit with no artefact (a build failure, a lockfile that
#          would need re-resolving, artefacts left by an earlier run): did not run
#   short  exit 0 before the time was up: did not run for what the record
#          would claim
# A failed target does not stop the shard: the rest run, then the script exits
# 1. For every target that did not end `ok`, <out-dir>/failures/<crate>/<target>/
# holds the last lines of its output and, for a crash, its artefact files (the
# reproducers).
# Exits non-zero without a result file when the shard has no target to run.
#
# In a workflow it also sets the step output `failed` (the first failed target,
# for the artefact name) and adds a table to the step summary.
set -euo pipefail

[ $# -eq 4 ] || { echo "usage: $0 <shard> <shards> <seconds-per-target> <out-dir>" >&2; exit 2; }
shard="$1"; shards="$2"; seconds="$3"; out="$4"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[[ "$seconds" =~ ^[1-9][0-9]*$ ]] || { echo "seconds-per-target must be a whole number, at least 1 (got '$seconds')" >&2; exit 2; }

mkdir -p "$out/logs"
out="$(cd "$out" && pwd)"
python3 "$ROOT/scripts/ci/fuzz_shards.py" targets --root "$ROOT" --shards "$shards" --shard "$shard" > "$out/targets.txt"
[ -s "$out/targets.txt" ] || { echo "shard $shard has no target to run" >&2; exit 1; }

summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
rows="$out/rows.jsonl"; : > "$rows"
checked=""; lock_ok=0; failed=0; first_failed=""
{
  echo "### Fuzz shard $shard, $seconds s per target"
  echo
  echo "| Target | Outcome | Artefact files | Seconds |"
  echo "|---|---|---|---|"
} >> "$summary"

# The targets come on fd 3 so that nothing a target runs can read them.
while read -r crate name <&3; do
  dir="$ROOT/crates/$crate/fuzz"
  log="$out/logs/$crate-$name.log"
  keep="$out/failures/$crate/$name"
  if [ "$crate" != "$checked" ]; then
    checked="$crate"; lock_ok=1
    echo "::group::toolchain and lockfile of $crate"
    rustc +nightly -V
    cargo +nightly fuzz --version
    # The committed lockfile is the resolution the record attests: a crate
    # whose lockfile would need re-resolving runs no target, and cargo's own
    # message is kept as the reason.
    (cd "$dir" && cargo +nightly metadata --locked --format-version 1 > /dev/null) 2> "$out/logs/$crate.lock.log" || lock_ok=0
    cat "$out/logs/$crate.lock.log"
    echo "::endgroup::"
  fi
  echo "::group::fuzz $crate/$name"
  outcome=error; found=0; elapsed=0
  if [ "$lock_ok" -ne 1 ]; then
    cp "$out/logs/$crate.lock.log" "$log"
  elif [ -n "$(find "$dir/artifacts/$name" -type f 2> /dev/null | head -n 1)" ]; then
    # Whatever is in the directory after the run has to be this run's.
    echo "crates/$crate/fuzz/artifacts/$name already holds files from an earlier run; move them away" | tee "$log"
  else
    corpus=()
    [ -d "$dir/seeds/$name" ] && corpus=("seeds/$name")
    start="$(date +%s)"
    rc=0
    (cd "$dir" && cargo +nightly fuzz run "$name" ${corpus[@]+"${corpus[@]}"} -- "-max_total_time=$seconds") < /dev/null 2>&1 | tee "$log" || rc="${PIPESTATUS[0]}"
    elapsed=$(($(date +%s) - start))
    [ -d "$dir/artifacts/$name" ] && found="$(find "$dir/artifacts/$name" -type f | wc -l | tr -d ' ')"
    if [ "$rc" -eq 0 ] && [ "$elapsed" -ge "$seconds" ]; then
      outcome=ok
    elif [ "$rc" -eq 0 ]; then
      outcome=short
      echo "$crate/$name exited 0 after $elapsed s, before its $seconds s were up" | tee -a "$log"
    elif [ "$found" -gt 0 ]; then
      outcome=crash
    fi
  fi
  if [ "$outcome" != ok ]; then
    failed=$((failed + 1))
    [ -n "$first_failed" ] || first_failed="$crate-$name"
    mkdir -p "$keep"
    [ "$outcome" = crash ] && cp -R "$dir/artifacts/$name/." "$keep/"
    tail -n 200 "$log" > "$keep/output-tail.txt"
    echo "::error::fuzz target $crate/$name: $outcome ($found artefact file(s))"
  fi
  jq -n -c --arg crate "$crate" --arg name "$name" --arg outcome "$outcome" --argjson artefacts "$found" --argjson elapsed "$elapsed" \
    '{crate: $crate, name: $name, outcome: $outcome, artefacts: $artefacts, elapsed_s: $elapsed}' >> "$rows"
  echo "| \`$crate/$name\` | $outcome | $found | $elapsed |" >> "$summary"
  echo "::endgroup::"
done 3< "$out/targets.txt"

jq -s --argjson shard "$shard" --argjson shards "$shards" --argjson seconds "$seconds" \
  '{shard: $shard, shards: $shards, seconds_per_target: $seconds, targets: .}' "$rows" > "$out/fuzz-shard-$shard.json"
cat "$out/fuzz-shard-$shard.json"
if [ "$failed" -gt 0 ]; then
  [ "$failed" -eq 1 ] || first_failed="$first_failed-and-$((failed - 1))-more"
  echo "failed=$first_failed" >> "${GITHUB_OUTPUT:-/dev/null}"
  exit 1
fi
