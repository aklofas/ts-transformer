#!/usr/bin/env bash
# Self-test of scripts/ci/fuzz-shard.sh against stub `cargo` and `rustc`: no
# fuzzer is built. The stub's `fuzz run` behaves by the target's name:
#   ok_*     runs for its -max_total_time, prints libFuzzer's closing line
#            ("Done N runs in S second(s)"), exits 0
#   crash_*  writes artifacts/<name>/crash-1, exits 77
#   error_*  exits 101 and writes no artefact (a build failure)
#   short_*  exits 0 at once
#   built_*  takes longer than the budget to "build", fuzzes for none of it,
#            exits 0
#   idle_*   prints a closing line that counts no run, exits 0
#   slow_*   as ok_*, and writes artifacts/<name>/slow-unit-1 (libFuzzer's
#            file for an input that took over ten seconds, which is no crash)
# Asserts each outcome in the result file, that the shard goes on after a
# failure, that a crashing target's artefact files and each failed target's
# output are kept, that a passing target's artefact files are kept apart from
# the failures and warned about, that a target whose artefact directory holds
# files from an earlier run is refused and not run, that a crate whose lockfile check fails
# runs no target, and that a shard with nothing to run writes no result. Each
# result is then merged with scripts/ci/fuzz_shards.py.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*"; exit 1; }

mkdir "$TMP/bin"
cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
[ "$1" = "+nightly" ] || { echo "stub cargo: expected +nightly, got $1" >&2; exit 2; }
shift
case "$1 ${2:-}" in
  "metadata --locked")
    case "$PWD" in */crates/"${STUB_LOCK_FAIL:-none}"/fuzz) echo "error: the lock file needs to be updated" >&2; exit 101 ;; esac
    echo '{}' ;;
  "fuzz --version") echo "cargo-fuzz 0.0.0-stub" ;;
  "fuzz run")
    name="$3"; shift 3
    echo "$PWD run $name $*" >> "$TMP/cargo.log"
    secs="$(printf '%s\n' "$@" | sed -n 's/^-max_total_time=//p')"
    case "$name" in
      ok_*) sleep "$secs"; echo "Done 1000 runs in $((secs + 1)) second(s)" >&2 ;;
      built_*) sleep "$((secs + 1))"; echo "Done 3 runs in 0 second(s)" >&2 ;;
      idle_*) echo "Done 0 runs in $((secs + 1)) second(s)" >&2 ;;
      slow_*) sleep "$secs"; mkdir -p "artifacts/$name"; echo slow > "artifacts/$name/slow-unit-1"
              echo "Slowest unit: 11 s:" >&2; echo "Done 2 runs in $((secs + 1)) second(s)" >&2 ;;
      crash_*) mkdir -p "artifacts/$name"; echo boom > "artifacts/$name/crash-1"
               echo "thread panicked at fuzz_targets/$name.rs" >&2; exit 77 ;;
      error_*) echo "error: could not compile $name" >&2; exit 101 ;;
      short_*) : ;;
      *) echo "stub cargo: unknown target $name" >&2; exit 2 ;;
    esac ;;
  *) echo "stub cargo: unexpected call: $*" >&2; exit 2 ;;
esac
STUB
printf '#!/usr/bin/env bash\necho "rustc 0.0.0-stub"\n' > "$TMP/bin/rustc"
chmod +x "$TMP/bin/cargo" "$TMP/bin/rustc"
export TMP PATH="$TMP/bin:$PATH"

# make_tree <dir> <crate/target>... : a tree with the two scripts, a manifest
# naming the targets and a fuzz workspace per crate.
make_tree() {
  local tree="$1"; shift
  mkdir -p "$tree/scripts/ci" "$tree/tests/coverage"
  cp "$ROOT/scripts/ci/fuzz-shard.sh" "$ROOT/scripts/ci/fuzz_shards.py" "$tree/scripts/ci/"
  echo "schema_version = 0" > "$tree/tests/coverage/fuzz-targets.toml"
  local t
  for t in "$@"; do
    printf '\n[[target]]\ncrate = "%s"\nname = "%s"\n' "${t%%/*}" "${t##*/}" >> "$tree/tests/coverage/fuzz-targets.toml"
    mkdir -p "$tree/crates/${t%%/*}/fuzz"
    : > "$tree/crates/${t%%/*}/fuzz/Cargo.toml"
  done
}
# shard <tree> <shard> <shards> <out> : runs the tree's copy, prints its exit code.
shard() {
  local rc=0
  : > "$4.output"
  GITHUB_OUTPUT="$4.output" GITHUB_STEP_SUMMARY="$4.summary" \
    bash "$1/scripts/ci/fuzz-shard.sh" "$2" "$3" 2 "$4" > "$4.log" 2>&1 || rc=$?
  echo "$rc"
}
outcomes() { jq -r '[.targets[] | "\(.crate)/\(.name)=\(.outcome):\(.artefacts)"] | join(" ")' "$1"; }
merged() { # <tree> <shards> <results-dir> : prints "<rc> <targets_run> <crashes>"
  local rc=0
  mkdir -p "$3"
  python3 "$1/scripts/ci/fuzz_shards.py" merge --root "$1" --shards "$2" --seconds 2 --results "$3" \
    --out "$3/fuzz.json" > "$3/merge.log" 2>&1 || rc=$?
  echo "$rc $(jq -r '"\(.targets_run) \(.crashes)"' "$3/fuzz.json" 2>/dev/null || echo "- -")"
}

# Case 1: one shard holding every outcome, one target of it with an artefact
# directory left by an earlier run.
T="$TMP/mixed"; O="$TMP/mixed-out"
make_tree "$T" alpha/ok_a alpha/crash_b alpha/error_c beta/built_f beta/idle_g beta/ok_d beta/ok_stale beta/short_e
mkdir -p "$T/crates/beta/fuzz/artifacts/ok_stale" "$T/crates/beta/fuzz/seeds/ok_d"
echo old > "$T/crates/beta/fuzz/artifacts/ok_stale/crash-old"
: > "$TMP/cargo.log"
[ "$(shard "$T" 1 1 "$O")" = 1 ] || { cat "$O.log"; fail "a shard with a crash, a build failure and a short run must exit 1"; }
R="$O/fuzz-shard-1.json"
[ -f "$R" ] || { cat "$O.log"; fail "no result file was written"; }
want="alpha/crash_b=crash:1 alpha/error_c=error:0 alpha/ok_a=ok:0 beta/built_f=short:0 beta/idle_g=short:0 beta/ok_d=ok:0 beta/ok_stale=error:0 beta/short_e=short:0"
[ "$(outcomes "$R")" = "$want" ] || fail "outcomes are [$(outcomes "$R")], want [$want]"
[ "$(jq -c '[.shard, .shards, .seconds_per_target]' "$R")" = "[1,1,2]" ] || fail "the result does not name its shard and seconds"
[ "$(jq '[.targets[] | select(.outcome == "ok") | .elapsed_s >= 2] | all' "$R")" = true ] || fail "an ok target's elapsed_s is below its budget"
[ "$(grep -c ' run ' "$TMP/cargo.log")" = 7 ] || { cat "$TMP/cargo.log"; fail "the shard did not run seven of its eight targets"; }
if grep -q " run ok_stale " "$TMP/cargo.log"; then fail "a target with artefacts from an earlier run was run"; fi
grep -q "/crates/beta/fuzz run ok_d seeds/ok_d -- -max_total_time=2$" "$TMP/cargo.log" || { cat "$TMP/cargo.log"; fail "ok_d was not run on its seed corpus"; }
grep -q "/crates/alpha/fuzz run ok_a -- -max_total_time=2$" "$TMP/cargo.log" || { cat "$TMP/cargo.log"; fail "ok_a was not run with the budget alone"; }
[ "$(cat "$O/failures/alpha/crash_b/crash-1")" = boom ] || fail "the reproducer of crash_b was not kept"
grep -q "thread panicked at fuzz_targets/crash_b.rs" "$O/failures/alpha/crash_b/output-tail.txt" || fail "the output of crash_b was not kept"
grep -q "could not compile error_c" "$O/failures/alpha/error_c/output-tail.txt" || fail "the output of error_c was not kept"
grep -q "already holds files from an earlier run" "$O/failures/beta/ok_stale/output-tail.txt" || fail "the refusal of ok_stale was not kept"
[ ! -e "$O/failures/beta/ok_stale/crash-old" ] || fail "an artefact left by an earlier run was kept as a reproducer"
grep -q "exited 0 having fuzzed 0 s of its 2 s" "$O/failures/beta/short_e/output-tail.txt" || fail "the short run's reason was not kept"
# A slow build is not fuzzing time: the target's own closing line decides.
grep -q "exited 0 having fuzzed 0 s of its 2 s" "$O/failures/beta/built_f/output-tail.txt" || fail "a target that built for longer than its budget and fuzzed for none of it counted as run"
# A closing line that counts no run is not a run, however long it says it took.
grep -q "exited 0 having fuzzed 0 s of its 2 s" "$O/failures/beta/idle_g/output-tail.txt" || fail "a target whose closing line counts no run counted as run"
[ ! -e "$O/failures/alpha/ok_a" ] || fail "a passing target has a failures directory"
[ ! -e "$O/kept" ] || fail "a shard whose passing targets left no artefact file has a kept directory"
grep -q "^Toolchain: .rustc 0.0.0-stub., .cargo-fuzz 0.0.0-stub.\$" "$O.summary" || { cat "$O.summary"; fail "the step summary does not name the toolchain"; }
grep -qx "failed=alpha-crash_b-and-5-more" "$O.output" || { cat "$O.output"; fail "the step output does not name the first failed target"; }
grep -q 'alpha/crash_b. | crash | 1 |' "$O.summary" || { cat "$O.summary"; fail "the step summary has no row for crash_b"; }
[ "$(merged "$T" 1 "$O")" = "1 3 1" ] || { cat "$O/merge.log"; fail "the merge of the mixed shard is [$(merged "$T" 1 "$O")], want [1 3 1]"; }

# Case 2: two clean shards.
T="$TMP/clean"; O="$TMP/clean-out"
make_tree "$T" alpha/ok_a beta/ok_d
: > "$TMP/cargo.log"
[ "$(shard "$T" 1 2 "$O")" = 0 ] || { cat "$O.log"; fail "a clean shard must exit 0"; }
[ "$(shard "$T" 2 2 "$O")" = 0 ] || { cat "$O.log"; fail "a clean shard must exit 0"; }
[ "$(outcomes "$O/fuzz-shard-1.json")" = "alpha/ok_a=ok:0" ] || fail "shard 1 of 2 ran [$(outcomes "$O/fuzz-shard-1.json")]"
[ "$(outcomes "$O/fuzz-shard-2.json")" = "beta/ok_d=ok:0" ] || fail "shard 2 of 2 ran [$(outcomes "$O/fuzz-shard-2.json")]"
[ ! -e "$O/failures" ] || fail "a clean shard has a failures directory"
[ ! -e "$O/kept" ] || fail "a clean shard has a kept directory"
if grep -q '^failed=' "$O.output"; then fail "a clean shard set the failed output"; fi
[ "$(merged "$T" 2 "$O")" = "0 2 0" ] || { cat "$O/merge.log"; fail "the merge of two clean shards is [$(merged "$T" 2 "$O")], want [0 2 0]"; }
# With one of the two results gone the merge fails and counts one target.
rm "$O/fuzz-shard-2.json"
[ "$(merged "$T" 2 "$O")" = "1 1 0" ] || { cat "$O/merge.log"; fail "the merge without shard 2 is [$(merged "$T" 2 "$O")], want [1 1 0]"; }

# Case 3: a target that exits 0 after its time and leaves an artefact file
# passed; the file and the target's output are kept apart from the failures,
# with a warning that names the target.
T="$TMP/slow"; O="$TMP/slow-out"
make_tree "$T" alpha/ok_a alpha/slow_s
[ "$(shard "$T" 1 1 "$O")" = 0 ] || { cat "$O.log"; fail "a shard whose only finding is a passing target's artefact file must exit 0"; }
[ "$(outcomes "$O/fuzz-shard-1.json")" = "alpha/ok_a=ok:0 alpha/slow_s=ok:1" ] || fail "slow case ran [$(outcomes "$O/fuzz-shard-1.json")]"
[ "$(cat "$O/kept/alpha/slow_s/slow-unit-1" 2> /dev/null)" = slow ] || { cat "$O.log"; fail "the artefact file of a passing target was not kept"; }
grep -q "Slowest unit: 11 s" "$O/kept/alpha/slow_s/output-tail.txt" || fail "the output of a passing target that left an artefact file was not kept"
[ ! -e "$O/kept/alpha/ok_a" ] || fail "a passing target without an artefact file has a kept directory"
[ ! -e "$O/failures" ] || fail "a passing target's artefact file made a failures directory"
if grep -q '^failed=' "$O.output"; then fail "a passing target's artefact file set the failed output"; fi
grep -q "^::warning::fuzz target alpha/slow_s passed and left 1 artefact file(s)" "$O.log" || { cat "$O.log"; fail "no warning names the passing target that left an artefact file"; }
if grep -q "^::error::" "$O.log"; then fail "a passing target's artefact file raised an error"; fi
grep -q 'alpha/slow_s. | ok | 1 |' "$O.summary" || { cat "$O.summary"; fail "the step summary has no row for slow_s"; }
[ "$(merged "$T" 1 "$O")" = "0 2 0" ] || { cat "$O/merge.log"; fail "the merge of the slow shard is [$(merged "$T" 1 "$O")], want [0 2 0]"; }
grep -q "1 artefact file(s) left by targets that passed" "$O/merge.log" || { cat "$O/merge.log"; fail "the merge does not say that a passing target left an artefact file"; }
[ "$(jq '.artefacts_kept' "$O/fuzz.json")" = 1 ] || fail "fuzz.json does not count the kept artefact file"

# Case 4: a crate whose lockfile would need re-resolving runs no target; the
# shard's other crate still runs.
T="$TMP/lock"; O="$TMP/lock-out"
make_tree "$T" alpha/ok_a alpha/ok_b beta/ok_d
: > "$TMP/cargo.log"
[ "$(STUB_LOCK_FAIL=alpha shard "$T" 1 1 "$O")" = 1 ] || { cat "$O.log"; fail "a failed lockfile check must exit 1"; }
[ "$(outcomes "$O/fuzz-shard-1.json")" = "alpha/ok_a=error:0 alpha/ok_b=error:0 beta/ok_d=ok:0" ] || fail "lockfile case ran [$(outcomes "$O/fuzz-shard-1.json")]"
if grep -q "/crates/alpha/fuzz run" "$TMP/cargo.log"; then fail "a target of the crate with the stale lockfile was run"; fi
grep -q "lock file needs to be updated" "$O/failures/alpha/ok_a/output-tail.txt" || fail "cargo's lockfile message was not kept"

# Case 5: a shard outside the plan, a time that is no number and an empty
# manifest run nothing and write no result.
T="$TMP/clean"; O="$TMP/none-out"
: > "$TMP/cargo.log"
rc=0; bash "$T/scripts/ci/fuzz-shard.sh" 1 2 0 "$O" > "$O.log" 2>&1 || rc=$?
[ "$rc" != 0 ] || fail "a budget of 0 s exited 0"
[ ! -s "$TMP/cargo.log" ] || fail "a budget of 0 s ran a target"
[ ! -e "$O/fuzz-shard-1.json" ] || fail "a budget of 0 s wrote a result"
[ "$(shard "$T" 3 2 "$O")" != 0 ] || fail "a shard outside the plan exited 0"
[ ! -e "$O/fuzz-shard-3.json" ] || fail "a shard outside the plan wrote a result"
T="$TMP/empty"; O="$TMP/empty-out"
make_tree "$T"
[ "$(shard "$T" 1 1 "$O")" != 0 ] || fail "an empty manifest exited 0"
[ ! -e "$O/fuzz-shard-1.json" ] || fail "an empty manifest wrote a result"
[ "$(merged "$T" 1 "$TMP/empty-out")" = "1 - -" ] || fail "the merge of an empty manifest did not fail without a file"

echo "fuzz-shard self-test: OK"
