#!/usr/bin/env bash
# Ratchet: enforce full-closure SHA-256 provenance for committed fixtures.
#   - every git-tracked file under each fixture-manifest.toml group root must
#     appear exactly once in fixture-hashes.toml;
#   - every listed file must exist, be repo-relative (no leading /, no ..), and
#     its recomputed SHA-256 must match;
#   - no listed file may be untracked/removed.
# Regenerate with scripts/gen/fixture-manifest.sh after intentional changes.
set -euo pipefail
# FIXTURE_ROOT overrides the repository root (the self-test points it at a
# temporary git repository).
ROOT="${FIXTURE_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$ROOT"
FM="tests/coverage/fixture-manifest.toml"
HASHES="tests/coverage/fixture-hashes.toml"
fail=0
err() { echo "FAIL: $*" >&2; fail=1; }

[[ -f "$HASHES" ]] || { echo "FAIL: $HASHES missing — run scripts/gen/fixture-manifest.sh" >&2; exit 1; }

# Both temps created before arming the trap (so an early exit under `set -u`
# can't hit an unbound $listed and leak $expected).
expected="$(mktemp)"
listed="$(mktemp)"
trap 'rm -f "$expected" "$listed"' EXIT

# Expected set: every tracked file under every group root.
grep -oE '^root = "[^"]+"' "$FM" | sed -E 's/^root = "(.*)"/\1/' | while IFS= read -r root; do
    git ls-files -- "$root"
done | LC_ALL=C sort -u > "$expected" || true
[[ -s "$expected" ]] || err "no fixture roots resolved from $FM"

# Listed set + per-file hash verification, one [[file]] record at a time:
# each record carries exactly one path and one sha256.
row=0 path="" npath=0 want="" nsha=0
check_record() {
    [[ "$row" -gt 0 ]] || return 0
    if [[ "$npath" -eq 0 ]]; then err "row $row: missing path"; return 0; fi
    if [[ "$npath" -gt 1 ]]; then err "row $row: two path lines"; return 0; fi
    if [[ "$nsha" -eq 0 ]]; then err "row $row: missing sha256 for $path"; return 0; fi
    if [[ "$nsha" -gt 1 ]]; then err "row $row: two sha256 lines for $path"; return 0; fi
    case "$path" in
        /*)   err "listed path is absolute: $path"; return 0 ;;
        *..*) err "listed path contains '..': $path"; return 0 ;;
    esac
    echo "$path" >> "$listed"
    if [[ ! -f "$path" ]]; then err "listed file does not exist: $path"; return 0; fi
    got="$(sha256sum "$path" | cut -d' ' -f1)"
    [[ "$got" == "$want" ]] || err "sha256 drift for $path (have $got, manifest $want)"
}
while IFS= read -r line; do
    case "$line" in
        '[[file]]')
            check_record
            row=$((row + 1)) path="" npath=0 want="" nsha=0 ;;
        'path = '*)
            [[ "$row" -gt 0 ]] || { err "path line before the first [[file]] record: $line"; continue; }
            path="$(printf '%s' "$line" | sed -E 's/^path = "(.*)"/\1/')"
            npath=$((npath + 1)) ;;
        'sha256 = '*)
            [[ "$row" -gt 0 ]] || { err "sha256 line before the first [[file]] record: $line"; continue; }
            want="$(printf '%s' "$line" | sed -E 's/^sha256 = "(.*)"/\1/')"
            nsha=$((nsha + 1)) ;;
    esac
done < "$HASHES"
check_record
LC_ALL=C sort -o "$listed" "$listed"
while IFS= read -r p; do err "listed twice: $p"; done < <(uniq -d "$listed")

# Closure both ways.
while IFS= read -r p; do grep -qxF "$p" "$listed" || err "unlisted committed fixture: $p (run scripts/gen/fixture-manifest.sh)"; done < "$expected"
while IFS= read -r p; do grep -qxF "$p" "$expected" || err "listed file no longer tracked under a fixture root: $p"; done < "$listed"

[[ "$fail" -eq 0 ]] && echo "PASS: fixture-hashes (closure + sha256)"
exit "$fail"
