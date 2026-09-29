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
#   SURFACE_PYI_DIR         the Python package directory (.pyi stubs + .py sources)
#   SURFACE_PY_PACKAGE      the package name a module-qualified cell starts with
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
      if (sym ~ /^(unaudited|deferred|n\/a)$/) next
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

  # (7) python: cells resolve by definition. A fake package with one class
  # defined in two modules, one module-level function and one unique class.
  mkdir -p "$tmp/py"
  printf 'class Transport:\n    def send(self, payload: bytes) -> None: ...\n\ndef decode_thing(buf: bytes) -> int: ...\n' > "$tmp/py/udp.pyi"
  printf 'class Transport:\n    def send(self, payload: bytes) -> None: ...\n' > "$tmp/py/tcp.pyi"
  printf 'class B:\n    count: int\n    def feed(self, buf: bytes) -> None: ...\n\nclass BEvent: ...\n' > "$tmp/py/mpegts.pyi"
  export SURFACE_PYI_DIR="$tmp/py"
  py_row() { # <item> <python cell>
    printf '[[surface]]\nitem = "%s"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:%s", "java:deferred", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "%s"\n' \
      "$1" "$2" "$3" > "$tmp/m.toml"
  }
  py_row "demo::a" "decode_thing" "demo::B"
  expect pass "python: module-level function, unqualified" || return 1
  py_row "demo::a" "send" "demo::B"
  expect fail "python: bare method name (the old leaf match resolved it)" || return 1
  py_row "demo::a" "Transport.send" "demo::B"
  expect fail "python: Class.member defined in two modules, no module given" || return 1
  py_row "demo::a" "tstrans.udp.Transport.send" "demo::B"
  expect pass "python: module-qualified Class.member" || return 1
  py_row "demo::a" "tstrans.tcp.decode_thing" "demo::B"
  expect fail "python: defined, but not in the module the cell names" || return 1
  py_row "demo::a" "B.nope" "demo::B"
  expect fail "python: class exists, member does not" || return 1
  py_row "demo::a" "B.count" "demo::B"
  expect pass "python: class attribute" || return 1
  py_row "demo::a" "decode_thing [feature=rist]" "demo::B"
  expect pass "python: unbuilt feature is skipped" || return 1
  py_row "demo::a" "send [feature=srt]" "demo::B"
  expect fail "python: built feature is resolved" || return 1
  # Owner rule: demo::B::x is a member of B, and B has a Python class.
  printf 'pub fn demo::a()\npub fn demo::B::x()\n' > "$tmp/crates/demo/public-api.txt"
  py_row "demo::B::x" "decode_thing" "demo::a"
  expect fail "python: a real function that is not the owner class's member" || return 1
  py_row "demo::B::x" "BEvent" "demo::a"
  expect fail "python: a real class that is not the owner class" || return 1
  py_row "demo::B::x" "B.feed" "demo::a"
  expect pass "python: the owner class's member" || return 1
  py_row "demo::B::x" "B" "demo::a"
  expect pass "python: the owner class itself" || return 1

  # (8) An EMPTY definition index resolves nothing. A package directory with
  # no .pyi/.py in it (a moved tree, a wrong SURFACE_PYI_DIR) must fail every
  # python: cell that names a symbol; only sentinel cells survive it.
  mkdir -p "$tmp/py-empty"
  export SURFACE_PYI_DIR="$tmp/py-empty"
  py_row "demo::a" "no_such_function" "demo::B::x"
  expect fail "python: a named symbol against an empty definition index" || return 1
  py_row "demo::a" "deferred" "demo::B::x"
  expect pass "python: sentinel cells against an empty definition index" || return 1

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
