#!/usr/bin/env bash
# L3 surface-manifest ratchet.
#
# Enforced (fails the build on violation):
#   (a) every owning_tests path in tests/coverage/surface-manifest.toml exists on disk;
#   (b) every binding column symbol resolves in that binding's source
#       (feature-tagged "[feature=X]" entries are skipped unless X is in BUILT_FEATURES);
#   (b2) every [[surface]] row carries all five binding columns (c, python, java,
#       swift, kotlin) — "<prefix>:unaudited" / ":deferred" / ":n/a" declare a
#       column whose twin is unchecked / absent today / absent by design;
#   (c) closure: every mappable public-api.txt item is mapped ([[surface]] item)
#       or exempted ([[exempt]] item).
#
# Run it:    bash scripts/check/repo/surface-manifest.sh
# Self-test: bash scripts/check/repo/surface-manifest.sh --self-test
#
# Overridable (for the self-test):
#   SURFACE_MANIFEST        path to the manifest TOML
#   SURFACE_BASELINE_DIR    directory containing <crate>/public-api.txt files
#   SURFACE_CRATES          space-separated list of crate names to check
#   SURFACE_BUILT_FEATURES  space-separated features considered built (default: all)
#   SURFACE_C_HEADER        path to the C binding header
#   SURFACE_PYI_DIR         directory to search for Python binding symbols
#   SURFACE_REQUIRED_PREFIXES  binding columns every [[surface]] row must carry
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

SURFACE_MANIFEST="${SURFACE_MANIFEST:-$ROOT/tests/coverage/surface-manifest.toml}"
SURFACE_BASELINE_DIR="${SURFACE_BASELINE_DIR:-$ROOT/crates}"
SURFACE_CRATES="${SURFACE_CRATES:-rist-sys tst-core tst-pipeline tst-rist tst-rtp tst-srt tst-tcp tst-udp}"
SURFACE_BUILT_FEATURES="${SURFACE_BUILT_FEATURES:-srt rtp udp tcp hls rist}"
SURFACE_C_HEADER="${SURFACE_C_HEADER:-$ROOT/bindings/c/include/tstrans.h}"
SURFACE_PYI_DIR="${SURFACE_PYI_DIR:-$ROOT/bindings/python/python/tstrans}"
SURFACE_JAVA_DIR="${SURFACE_JAVA_DIR:-$ROOT/bindings/jvm/src/main/java}"

# ---------------------------------------------------------------------------
# extract_items: stdin is a public-api.txt; stdout is one canonical key per
# mappable line. Mappable = pub fn/struct/enum/trait/type/const (incl. const fn).
# auto-derived impl lines are NOT mappable and are skipped.
# KEEP IN SYNC with scripts/gen/surface-exemptions.sh extract_items.
# ---------------------------------------------------------------------------
extract_items() {
  awk '
    /^pub (const fn|fn|struct|enum|trait|type|const) / {
      line=$0
      sub(/^pub const fn /, "pub fn ", line)           # normalize "pub const fn"
      sub(/^pub (fn|struct|enum|trait|type|const) /, "", line)
      sub(/[(<].*$/, "", line)                         # drop fn args / generics
      sub(/[[:space:]]*=.*$/, "", line)                # drop type alias RHS: " = ..."
      sub(/:[[:space:]].*$/, "", line)                 # drop ": Type" annotation
      sub(/[[:space:]]+$/, "", line)
      if (line != "") print line
    }
  '
}

# ---------------------------------------------------------------------------
# run_check: validate the manifest against the baselines and binding sources
# ---------------------------------------------------------------------------
run_check() {
  [ -f "$SURFACE_MANIFEST" ] || { echo "FAIL: manifest not found: $SURFACE_MANIFEST" >&2; return 1; }
  local errs; errs="$(mktemp)"; trap 'rm -f "$errs"' RETURN
  local fail=0

  local mapped exempt
  mapped="$(mktemp)"; exempt="$(mktemp)"
  # All "item = " lines in [[surface]] AND [[exempt]] sections
  grep -E '^item = ' "$SURFACE_MANIFEST" | sed -E 's/^item = "(.*)"/\1/' | sort -u > "$mapped"
  # Only items from [[exempt]] sections (the awk skips [[surface]] items)
  awk '
    /^\[\[exempt\]\]/ { e=1; next }
    /^\[\[surface\]\]/ { e=0; next }
    e && /^item = / { sub(/^item = "/, ""); sub(/".*$/, ""); print }
  ' "$SURFACE_MANIFEST" | sort -u > "$exempt"

  # (a) owning_tests paths exist.
  local p
  grep -E '^owning_tests = ' "$SURFACE_MANIFEST" | grep -oE '"[^"]+"' | tr -d '"' \
  | while IFS= read -r p; do
    [ -f "$ROOT/$p" ] || echo "FAIL: owning test path missing: $p"
  done >> "$errs"

  # (b) binding symbols resolve (feature-aware).
  local b feat sym leaf
  grep -E '^bindings = ' "$SURFACE_MANIFEST" | grep -oE '"[^"]+"' | tr -d '"' \
  | while IFS= read -r b; do
    feat=""
    sym="$b"
    # Extract optional [feature=X] suffix
    if printf '%s' "$b" | grep -q '\[feature='; then
      feat="$(printf '%s' "$b" | sed -E 's/.*\[feature=([a-z]+)\].*/\1/')"
      sym="$(printf '%s' "$b" | sed -E 's/ *\[feature=[a-z]+\]//')"
      # Skip if feature not built
      printf '%s\n' $SURFACE_BUILT_FEATURES | grep -Fxq "$feat" || continue
    fi
    case "$sym" in
      *:deferred|*:n/a|*:unaudited)
        # Five-column sentinels — declared, never resolved (rule b2):
        #   :unaudited  nobody has checked whether a twin exists (the value the
        #               bulk migration wrote; burn these down to a real symbol,
        #               :deferred or :n/a as rows are audited)
        #   :deferred   no twin in that binding TODAY
        #   :n/a        no twin by design
        # This arm MUST precede `c:*)`: the resolution arms are unbounded
        # substring greps, and the word "deferred" really does occur in
        # tstrans.h (1x) and in the Python (3 files) and Java (11 files)
        # sources — so a sentinel that reached them would silently "resolve"
        # and rule (b) would pass for the wrong reason.
        : ;;
      c:*)
        grep -Fq "${sym#c:}" "$SURFACE_C_HEADER" \
          || echo "FAIL: c symbol unresolved: ${sym#c:}" ;;
      python:*)
        leaf="${sym#python:}"
        leaf="${leaf##*.}"   # last dotted component
        # NOTE: this is an UNBOUNDED SUBSTRING match anywhere in the binding
        # sources — it can false-pass (e.g. the leaf appears only in a comment
        # or a longer identifier). Intentional looseness for the L3 bootstrap;
        # tighten to a word-boundary (`grep -w`) or AST-aware check when a row
        # graduates and column (b) must be trustworthy for that symbol.
        grep -rFq "$leaf" "$SURFACE_PYI_DIR" \
          || echo "FAIL: python symbol unresolved: ${sym#python:} (leaf: $leaf)" ;;
      java:*)
        leaf="${sym#java:}"
        leaf="${leaf##*.}"   # last dotted component (e.g. feed/nextEvent/Demuxer)
        # Same UNBOUNDED SUBSTRING looseness as the python: arm (L3 bootstrap) —
        # resolves the Java leaf anywhere in the JVM binding sources. Tighten to a
        # word-boundary / AST-aware check when java: column trust matters per-symbol.
        grep -rFq "$leaf" "$SURFACE_JAVA_DIR" \
          || echo "FAIL: java symbol unresolved: ${sym#java:} (leaf: $leaf)" ;;
      swift:*|kotlin:*)
        : ;;   # RESERVED until tst-uniffi lands; presence-only, not resolved
      *)
        echo "FAIL: unknown binding prefix: $sym" ;;
    esac
  done >> "$errs"

  # (b2) five-column rule (Arc 2 R1 / X-META-02): every [[surface]] row's
  # `bindings` array carries at least one entry per required prefix. A
  # binding with no twin today says so explicitly with the sentinel
  # "<prefix>:deferred" (or "<prefix>:n/a" for by-design gaps) instead of
  # omitting the column — an omitted column is indistinguishable from a
  # forgotten one, which is exactly how the Swift/Kotlin columns stayed
  # empty for three months. Sentinels are never resolved.
  local required_prefixes="${SURFACE_REQUIRED_PREFIXES:-c python java swift kotlin}"
  awk -v req="$required_prefixes" '
    BEGIN { n = split(req, P, " ") }
    function flush() {
      if (!insurf) return
      if (!hasb) { printf "FAIL: [[surface]] row %s has no bindings array (need every column: %s)\n", item, req; return }
      for (i = 1; i <= n; i++) {
        if (index(bline, "\"" P[i] ":") == 0)
          printf "FAIL: [[surface]] row %s is missing the %s: column (use \"%s:deferred\" when there is no twin)\n", item, P[i], P[i]
      }
    }
    /^\[\[surface\]\]/ { flush(); insurf = 1; hasb = 0; item = "?"; bline = ""; next }
    /^\[\[exempt\]\]/  { flush(); insurf = 0; next }
    insurf && /^item = /     { item = $0; sub(/^item = "/, "", item); sub(/".*$/, "", item) }
    insurf && /^bindings = / { hasb = 1; bline = $0 }
    END { flush() }
  ' "$SURFACE_MANIFEST" >> "$errs"

  # (c) closure: every mappable baseline item is mapped or exempted.
  local c f item
  for c in $SURFACE_CRATES; do
    f="$SURFACE_BASELINE_DIR/$c/public-api.txt"; [ -f "$f" ] || continue
    while IFS= read -r item; do
      [ -n "$item" ] || continue
      grep -Fxq "$item" "$mapped" && continue
      grep -Fxq "$item" "$exempt" && continue
      echo "FAIL: un-catalogued public item (add to [[surface]] or [[exempt]] in surface-manifest.toml): $item"
    done < <(extract_items < "$f")
  done >> "$errs"

  rm -f "$mapped" "$exempt"

  if [ -s "$errs" ]; then
    cat "$errs" >&2
    fail=1
  fi
  return "$fail"
}

# ---------------------------------------------------------------------------
# self_test: build throwaway manifests and assert each negative is caught
# ---------------------------------------------------------------------------
self_test() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  local rc

  expect() { # <expected pass|fail> <label>
    local want="$1" label="$2"
    if ( cd "$ROOT" && run_check ) >/dev/null 2>&1; then rc=pass; else rc=fail; fi
    if [ "$rc" = "$want" ]; then
      echo "  ok: $label (expected $want)"
    else
      echo "  SELF-TEST FAIL: $label expected $want got $rc" >&2
      return 1
    fi
  }

  # Set up an isolated fake tree
  mkdir -p "$tmp/crates/demo"
  printf 'pub fn demo::a()\npub struct demo::B\n' > "$tmp/crates/demo/public-api.txt"
  : > "$tmp/h.h"   # empty C header

  export SURFACE_BASELINE_DIR="$tmp/crates"
  export SURFACE_CRATES="demo"
  export SURFACE_MANIFEST="$tmp/m.toml"
  export SURFACE_C_HEADER="$tmp/h.h"
  export SURFACE_PYI_DIR="$tmp"
  export SURFACE_BUILT_FEATURES="srt"

  # (1) Both items exempted -> pass
  printf '[[exempt]]\nitem = "demo::a"\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/m.toml"
  expect pass "all exempted" || return 1

  # (2) One item neither mapped nor exempted -> fail (closure)
  printf '[[exempt]]\nitem = "demo::a"\n' > "$tmp/m.toml"
  expect fail "un-catalogued item" || return 1

  # (3) Mapped row with a missing owning test -> fail
  # Five-column sentinels so the expected failure can only come from rule (a).
  printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/NO_SUCH_FILE.rs"]\nbindings = ["c:deferred", "python:deferred", "java:deferred", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/m.toml"
  expect fail "missing owning test" || return 1

  # (4) Mapped row with owning test present but unresolved c symbol -> fail
  # Use a real repo-relative path that exists (tests/coverage/README.md)
  printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:nope_sym_xyz", "python:deferred", "java:deferred", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/m.toml"
  expect fail "unresolved c symbol" || return 1

  # (5) Five-column rule: a row missing one column -> fail; all sentinels -> pass
  printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:deferred", "java:deferred", "swift:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/m.toml"
  expect fail "row missing the kotlin column" || return 1
  printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:n/a", "java:unaudited", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/m.toml"
  expect pass "all five columns present as sentinels (:deferred / :n/a / :unaudited)" || return 1

  # (6) SURFACE_REQUIRED_PREFIXES really drives rule (b2) — the same two-column
  # row that fails under the default set passes when the set is narrowed.
  printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/m.toml"
  expect fail "a two-column row under the default prefix set" || return 1
  SURFACE_REQUIRED_PREFIXES="c python" expect pass "the same row under SURFACE_REQUIRED_PREFIXES='c python'" || return 1

  echo "self-test: PASS"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
  self_test
else
  # Prove the checker still catches its negatives before trusting a pass.
  # Subshell so the self-test's env overrides can't leak into run_check.
  ( self_test ) >/dev/null
  run_check
  echo "surface-manifest: OK"
fi
