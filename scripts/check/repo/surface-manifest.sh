#!/usr/bin/env bash
# L3 surface-manifest ratchet.
#
# Enforced (fails the build on violation):
#   (a) every owning_tests path in tests/coverage/surface-manifest.toml exists on disk;
#   (b) every binding column symbol resolves in that binding's source
#       (feature-tagged "[feature=X]" entries are skipped unless X is in BUILT_FEATURES);
#       c: and java: are substring matches; python: names a definition
#       (function | Class | Class.member, see check_python_symbols);
#   (b2) every [[surface]] row carries all five binding columns (c, python, java,
#       swift, kotlin) — "<prefix>:unaudited" / ":deferred" / ":n/a" declare a
#       column whose twin is unchecked / absent today / absent by design;
#       ":deferred(reason)" and ":n/a(reason)" say why, in the cell;
#   (b3) "unaudited" is frozen per column: the number of :unaudited cells in
#       the c, python and java columns may not exceed UNAUDITED_CELLS_MAX;
#   (c) closure: the mappable public-api.txt items that no [[surface]] or
#       [[exempt]] row names are exactly the pinned set (UNAUDITED_COUNT
#       items whose sorted list hashes to UNAUDITED_SHA256). An item matching
#       an [[exempt_rule]] item_regex in the manifest is covered by that rule
#       and is neither listed nor counted.
#
# Run it:          bash scripts/check/repo/surface-manifest.sh
# Pinned set:      bash scripts/check/repo/surface-manifest.sh --list-unaudited
# Re-pin values:   bash scripts/check/repo/surface-manifest.sh --print-baseline
#
# Overridable (scripts/ratchets/tests/self_test.sh points these at fixtures):
#   SURFACE_MANIFEST        path to the manifest TOML
#   SURFACE_BASELINE_DIR    directory containing <crate>/public-api.txt files
#   SURFACE_CRATES          space-separated list of crate names to check
#   SURFACE_BUILT_FEATURES  space-separated features considered built (default: all)
#   SURFACE_C_HEADER        path to the C binding header
#   SURFACE_PYI_DIR         the Python package directory (.pyi stubs + .py sources)
#   SURFACE_PY_PACKAGE      the package name a module-qualified cell starts with
#   SURFACE_REQUIRED_PREFIXES  binding columns every [[surface]] row must carry
#   SURFACE_UNAUDITED_COUNT / SURFACE_UNAUDITED_SHA256  rule (c) baseline
#   SURFACE_UNAUDITED_CELLS_MAX  rule (b3) maxima, "<c> <python> <java>"
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

SURFACE_MANIFEST="${SURFACE_MANIFEST:-$ROOT/tests/coverage/surface-manifest.toml}"
SURFACE_BASELINE_DIR="${SURFACE_BASELINE_DIR:-$ROOT/crates}"
SURFACE_CRATES="${SURFACE_CRATES:-$(bash "$ROOT/scripts/lib/roster.sh" surface | tr '\n' ' ')}"
SURFACE_BUILT_FEATURES="${SURFACE_BUILT_FEATURES:-srt rtp udp tcp hls rist}"
SURFACE_C_HEADER="${SURFACE_C_HEADER:-$ROOT/bindings/c/include/tstrans.h}"
SURFACE_PYI_DIR="${SURFACE_PYI_DIR:-$ROOT/bindings/python/python/tstrans}"
SURFACE_JAVA_DIR="${SURFACE_JAVA_DIR:-$ROOT/bindings/jvm/src/main/java}"

# Rule (c) baseline: the mappable baseline items that no [[surface]] or
# [[exempt]] row names, pinned by count and by the sha256 of their sorted
# list. The 2026-06-03 bootstrap left them uncatalogued. Giving one a row
# shrinks the set: re-pin both values with --print-baseline in that change.
UNAUDITED_COUNT=2081
UNAUDITED_SHA256=57342eb014e824e430a14a80c56209165d61cd240963421350632b4fe66c7da8

# Rule (b3): the most :unaudited cells each binding column may hold, in the
# order "c python java". The number may only fall: auditing a cell lowers it,
# and a new row names a real symbol, :deferred(reason) or :n/a(reason).
UNAUDITED_CELLS_MAX="317 228 184"

# ---------------------------------------------------------------------------
# extract_items: stdin is a public-api.txt; stdout is one canonical key per
# mappable line. Mappable = pub fn/struct/enum/trait/type/const (incl. const fn).
# auto-derived impl lines are NOT mappable and are skipped.
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

# sha256_of: hex digest of stdin; sha256sum on Linux, shasum on macOS.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 | cut -d' ' -f1
  else
    echo "FAIL: surface-manifest needs sha256sum (GNU coreutils) or shasum (perl, ships with macOS); install one." >&2
    return 1
  fi
}

# ---------------------------------------------------------------------------
# list_unaudited: stdout is the sorted, de-duplicated set of mappable
# baseline items that no [[surface]] or [[exempt]] row names — the set
# rule (c) pins by UNAUDITED_COUNT and UNAUDITED_SHA256.
# ---------------------------------------------------------------------------
list_unaudited() {
  local named c f rules
  rules="$(rule_regex)"
  named="$(mktemp)"
  grep -E '^item = ' "$SURFACE_MANIFEST" | sed -nE 's/^item = "([^"]+)".*/\1/p' | LC_ALL=C sort -u > "$named"
  for c in $SURFACE_CRATES; do
    f="$SURFACE_BASELINE_DIR/$c/public-api.txt"; [ -f "$f" ] || continue
    extract_items < "$f"
  done | LC_ALL=C sort -u | LC_ALL=C comm -23 - "$named" \
  | { if [ -n "$rules" ]; then grep -Ev -e "$rules" || true; else cat; fi; }
  rm -f "$named"
}

# rule_regex: the [[exempt_rule]] item_regex values of the manifest, joined
# into one extended regex; empty when the manifest has none.
rule_regex() {
  sed -nE "s/^item_regex = '(.*)'[[:space:]]*(#.*)?\$/(\\1)/p" "$SURFACE_MANIFEST" | paste -sd'|' -
}

# rows_covered_by_rule: how many [[exempt]] rows name an item an
# [[exempt_rule]] already covers (such a row is redundant).
rows_covered_by_rule() {
  local rules; rules="$(rule_regex)"
  [ -n "$rules" ] || { echo 0; return; }
  awk '/^\[\[/ { ex = ($0 == "[[exempt]]") } ex && /^item = / { print }' "$SURFACE_MANIFEST" \
    | sed -nE 's/^item = "([^"]+)".*/\1/p' | { grep -Ec -e "$rules" || true; } | tail -1
}

# unaudited_cells: stdout is the number of :unaudited cells in each binding
# column, "<c> <python> <java>".
unaudited_cells() {
  local p out=""
  for p in c python java; do
    out="$out$({ grep -E '^bindings = ' "$SURFACE_MANIFEST" | grep -o "\"$p:unaudited" || true; } | wc -l | tr -d ' ') "
  done
  echo "${out% }"
}

# ---------------------------------------------------------------------------
# py_index: stdout is one line per Python DEFINITION under SURFACE_PYI_DIR
# (.pyi stubs and .py sources), as "<module>\t<name>\t<kind>":
#   kind=class   a module-level class            name = Class
#   kind=def     a module-level function         name = function
#   kind=member  a method/attribute of a class   name = Class.member
# <module> is the dotted path below the package ("klv", "pandas.frames").
# Only module-level classes and their direct (4-space) members are indexed;
# inherited members are not — name the class that defines the member.
# ---------------------------------------------------------------------------
py_index() {
  local f rel mod
  find "$SURFACE_PYI_DIR" -type f \( -name '*.pyi' -o -name '*.py' \) | sort \
  | while IFS= read -r f; do
    rel="${f#"$SURFACE_PYI_DIR"/}"
    mod="${rel%.*}"
    mod="${mod//\//.}"
    mod="${mod%.__init__}"
    [ "$mod" = "__init__" ] && mod=""
    awk -v mod="$mod" '
      function ident(s) { sub(/[^A-Za-z0-9_].*$/, "", s); return s }
      /^class [A-Za-z_]/ {
        cls = ident(substr($0, 7)); print mod "\t" cls "\tclass"; next
      }
      /^def [A-Za-z_]/       { cls = ""; print mod "\t" ident(substr($0, 5))  "\tdef"; next }
      /^async def [A-Za-z_]/ { cls = ""; print mod "\t" ident(substr($0, 11)) "\tdef"; next }
      /^[A-Za-z_]/ { cls = "" }   # any other module-level statement ends the class body
      cls != "" && /^    def [A-Za-z_]/       { print mod "\t" cls "." ident(substr($0, 9))  "\tmember"; next }
      cls != "" && /^    async def [A-Za-z_]/ { print mod "\t" cls "." ident(substr($0, 15)) "\tmember"; next }
      cls != "" && /^    [A-Za-z_][A-Za-z0-9_]*[ ]*[:=]/ { print mod "\t" cls "." ident(substr($0, 5)) "\tmember" }
    ' "$f"
  done | sort -u
}

# ---------------------------------------------------------------------------
# check_python_symbols: rule (b) for the python: column. stdout is FAIL lines.
#
# A python: cell names a DEFINITION, not a string that occurs somewhere:
#   python:function              a module-level def
#   python:Class                 a module-level class
#   python:Class.member          a method/attribute defined on that class
#   python:tstrans.<module>.<one of the above>   the same, in that module only
# The unqualified forms must be defined in exactly ONE module; a name several
# modules define (`Transport`, `CancelHandle`, ...) has to carry its module.
#
# Owner rule: when the Rust item is `...::Owner::leaf` and the binding has a
# Python class called `Owner`, the twin is that class or one of its members.
# This is what keeps a cell honest about WHICH symbol it names: before it, a
# leaf was substring-matched against every file, so `Demuxer::feed` could
# point at `decode_uas_datalink`, `Demuxer::next_event` at the `DemuxEvent`
# type and `UdpTransport::connect` at `send`, and all three "resolved".
# ---------------------------------------------------------------------------
check_python_symbols() {
  local idx; idx="$(mktemp)"
  py_index > "$idx"
  awk '
    /^\[\[(surface|exempt)\]\]/ { item = "?" }
    /^item = / { item = $0; sub(/^item = "/, "", item); sub(/".*$/, "", item) }
    /^bindings = / {
      line = $0
      while (match(line, /"python:[^"]*"/)) {
        print item "\t" substr(line, RSTART + 8, RLENGTH - 9)
        line = substr(line, RSTART + RLENGTH)
      }
    }
  ' "$SURFACE_MANIFEST" \
  | awk -F '\t' -v pkg="${SURFACE_PY_PACKAGE:-tstrans}" -v built="$SURFACE_BUILT_FEATURES" '
    BEGIN { nb = split(built, B, " "); for (i = 1; i <= nb; i++) BUILT[B[i]] = 1 }
    # The two inputs are told apart by an explicit phase (assignment operands
    # below), never by `FNR == NR`: with an EMPTY index that test stays true
    # through the whole manifest, every row is swallowed as an index row and
    # no cell is checked at all.
    phase == 1 {
      nidx++
      MODS[$1] = 1
      DEF[$1 SUBSEP $2] = 1
      WHERE[$2] = (WHERE[$2] == "" ? "" : WHERE[$2] ", ") ($1 == "" ? pkg : pkg "." $1)
      COUNT[$2]++
      if ($3 == "class") CLASS[$2] = 1
      next
    }
    {
      item = $1; sym = $2; shown = sym
      if (sym ~ /^(unaudited|deferred|n\/a)(\(.*\))?$/) next
      if (match(sym, / *\[feature=[a-z]+\]/)) {
        feat = substr(sym, RSTART, RLENGTH); gsub(/[^a-z=]/, "", feat); sub(/^feature=/, "", feat)
        sym = substr(sym, 1, RSTART - 1)
        if (!(feat in BUILT)) next
      }
      if (!nidx) {
        # Fail closed, once: nothing can resolve, and one line that names the
        # cause reads better than an "unresolved" line per row.
        if (!warned++)
          print "FAIL: python definition index is empty (no def/class under SURFACE_PYI_DIR) but the manifest names python symbols, first: " shown " (row " item ")"
        next
      }
      mod = ""; hasmod = 0; rest = sym
      if (index(sym, pkg ".") == 1) {
        rest = substr(sym, length(pkg) + 2)
        # longest module prefix that exists ("pandas.frames" before "pandas")
        n = split(rest, P, "."); best = 0; cand = ""
        for (i = 1; i < n; i++) {
          cand = (i == 1 ? P[1] : cand "." P[i])
          if (cand in MODS) { best = i; mod = cand }
        }
        if (best == 0) {
          print "FAIL: python symbol unresolved: " shown " (row " item "): no module " pkg "." P[1]
          next
        }
        hasmod = 1
        rest = P[best + 1]; for (i = best + 2; i <= n; i++) rest = rest "." P[i]
      }
      if (rest !~ /^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$/) {
        print "FAIL: python symbol malformed: " shown " (row " item "): want function, Class or Class.member, optionally prefixed " pkg ".<module>."
        next
      }
      if (hasmod) {
        if (!((mod SUBSEP rest) in DEF)) {
          print "FAIL: python symbol unresolved: " shown " (row " item "): " pkg "." mod " defines no " rest
          next
        }
      } else if (!(rest in COUNT)) {
        print "FAIL: python symbol unresolved: " shown " (row " item "): not a module-level def/class or a Class.member in the Python binding (a method needs its class: Class." rest ")"
        next
      } else if (COUNT[rest] > 1) {
        print "FAIL: python symbol ambiguous: " shown " (row " item "): defined in " WHERE[rest] " — write " pkg ".<module>." rest
        next
      }
      n = split(item, S, "::")
      if (n >= 2 && S[n - 1] ~ /^[A-Z]/ && (S[n - 1] in CLASS)) {
        owner = S[n - 1]
        if (rest != owner && index(rest, owner ".") != 1)
          print "FAIL: python symbol is not the twin: " shown " (row " item "): " owner " has a Python class, so the twin is " owner " or " owner ".<member>"
      }
    }
  ' phase=1 "$idx" phase=2 -
  rm -f "$idx"
}

# ---------------------------------------------------------------------------
# run_check: validate the manifest against the baselines and binding sources
# ---------------------------------------------------------------------------
run_check() {
  [ -f "$SURFACE_MANIFEST" ] || { echo "FAIL: manifest not found: $SURFACE_MANIFEST" >&2; return 1; }
  local errs; errs="$(mktemp)"; trap 'rm -f "$errs"' RETURN
  local fail=0

  # (a) owning_tests paths exist.
  local p
  { grep -E '^owning_tests = ' "$SURFACE_MANIFEST" | grep -oE '"[^"]+"' || true; } | tr -d '"' \
  | while IFS= read -r p; do
    [ -f "$ROOT/$p" ] || echo "FAIL: owning test path missing: $p"
  done >> "$errs"

  # (b) binding symbols resolve (feature-aware).
  local b feat sym leaf
  { grep -E '^bindings = ' "$SURFACE_MANIFEST" | grep -oE '"[^"]+"' || true; } | tr -d '"' \
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
      *:deferred|*:n/a|*:unaudited|*:deferred\(*\)|*:n/a\(*\))
        # Five-column sentinels — declared, never resolved (rule b2):
        #   :unaudited  nobody has checked whether a twin exists (the value the
        #               bulk migration wrote; burn these down to a real symbol,
        #               :deferred or :n/a as rows are audited)
        #   :deferred   no twin in that binding TODAY
        #   :n/a        no twin by design
        # :deferred and :n/a may carry their reason: :deferred(reason).
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
        : ;;   # resolved by DEFINITION, per row, in check_python_symbols below
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

  # (b) continued — python: symbols, by definition and per row.
  check_python_symbols >> "$errs"

  # (b2) five-column rule: every [[surface]] row's
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
    /^\[\[exempt_rule\]\]/ { flush(); insurf = 0; next }
    insurf && /^item = /     { item = $0; sub(/^item = "/, "", item); sub(/".*$/, "", item) }
    insurf && /^bindings = / { hasb = 1; bline = $0 }
    END { flush() }
  ' "$SURFACE_MANIFEST" >> "$errs"

  # (b3) unaudited is frozen per column: each count may only fall.
  local have_cells want_cells i cols
  have_cells="$(unaudited_cells)"
  want_cells="${SURFACE_UNAUDITED_CELLS_MAX:-$UNAUDITED_CELLS_MAX}"
  cols=(c python java)
  for i in 0 1 2; do
    local h w
    h="$(echo "$have_cells" | cut -d' ' -f$((i + 1)))"
    w="$(echo "$want_cells" | cut -d' ' -f$((i + 1)))"
    if [ "$h" -gt "$w" ]; then
      echo "FAIL: ${cols[$i]}: column holds $h :unaudited cells, above its maximum $w. A new or edited row names a real symbol, :deferred(reason) or :n/a(reason)." >> "$errs"
    fi
  done
  local covered; covered="$(rows_covered_by_rule)"
  if [ "$covered" != "0" ]; then
    echo "surface-manifest: warning: $covered [[exempt]] rows name items an [[exempt_rule]] already covers; delete them" >&2
  fi

  # (c) closure: the set of mappable baseline items that no row names must
  # equal the pinned baseline, by count and by digest.
  local unaudited n sum want_n want_sum
  want_n="${SURFACE_UNAUDITED_COUNT:-$UNAUDITED_COUNT}"
  want_sum="${SURFACE_UNAUDITED_SHA256:-$UNAUDITED_SHA256}"
  unaudited="$(mktemp)"
  list_unaudited > "$unaudited"
  n="$(wc -l < "$unaudited" | tr -d ' ')"
  sum="$(sha256_of < "$unaudited")"
  if [ "$n" != "$want_n" ] || [ "$sum" != "$want_sum" ]; then
    {
      echo "FAIL: the un-catalogued public item set changed: $n items, sha256 $sum (pinned: $want_n items, $want_sum)."
      echo "      Public items this branch added or removed: git diff main -- 'crates/*/public-api.txt'"
      echo "      A new item needs a [[surface]] row or an [[exempt]] row with a reason in surface-manifest.toml."
      echo "      Re-pin only for a deliberate change: bash scripts/check/repo/surface-manifest.sh --print-baseline"
      echo "      A re-pin is legitimate only when the set shrinks or an item is removed or renamed; a re-pin whose count goes UP is never acceptable unless the measured crate roster itself grew; then every added item must belong to a newly listed crate (a new public item gets a manifest row)."
    } >> "$errs"
  fi
  rm -f "$unaudited"

  if [ -s "$errs" ]; then
    cat "$errs" >&2
    fail=1
  fi
  return "$fail"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
case "${1:-}" in
  --list-unaudited)
    list_unaudited ;;
  --print-baseline)
    t="$(mktemp)"; list_unaudited > "$t"
    echo "UNAUDITED_COUNT=$(wc -l < "$t" | tr -d ' ')"
    echo "UNAUDITED_SHA256=$(sha256_of < "$t")"
    echo "UNAUDITED_CELLS_MAX=\"$(unaudited_cells)\""
    rm -f "$t" ;;
  *)
    run_check
    echo "surface-manifest: OK" ;;
esac
