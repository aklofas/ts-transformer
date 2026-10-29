#!/usr/bin/env bash
# Bash ratchet: every `.ordinal()` in the JVM binding's main sources is a
# pinned site. An enum's ordinal passed to native code is a wire value: each
# one is pinned by OrdinalContractTest (bindings/jvm/src/test/java/org/tstrans/),
# which holds every enum's ordinal order to the native codes. A new site must
# be added there and here, or pass an explicit code instead.
#
# The pinned list is scripts/ratchets/ordinal-sites.tsv: one row per
# occurrence, `<path under the Java source root><TAB><expression>`, sorted.
# Comment lines and trailing `//` comments are not sites.
#
# Fails closed (exit 2, naming file:line) on a line it cannot read: one whose
# `.ordinal()` receiver is not a dotted chain of names and `()` calls (an
# indexed receiver, a call with arguments, a chained call split across lines;
# reformat it onto one line), or one that holds both a `"` and a `//`, where a
# string literal and a comment cannot be told apart.
#
# Run it:       bash scripts/check/jvm/ordinal-sites.sh
# Regenerate:   bash scripts/check/jvm/ordinal-sites.sh --regen
# Overridable (the ratchet self-test points these at fixtures):
#   ORDINAL_SITES_JAVA_DIR  the Java source root to scan
#   ORDINAL_SITES_LIST      the pinned list
# Portable: grep/sed/sort/comm only, bash 3.2 syntax.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
JAVA_DIR="${ORDINAL_SITES_JAVA_DIR:-$ROOT/bindings/jvm/src/main/java}"
LIST="${ORDINAL_SITES_LIST:-$ROOT/scripts/ratchets/ordinal-sites.tsv}"
EXPR='[A-Za-z_][A-Za-z0-9_]*(\(\))?(\.[A-Za-z_][A-Za-z0-9_]*(\(\))?)*\.ordinal\(\)'

[ -d "$JAVA_DIR" ] || { echo "FAIL: ordinal-sites: Java source root not found: $JAVA_DIR"; exit 2; }

sites() { # stdout: sorted `<path><TAB><expression>` rows
  local hits rc
  hits="$(grep -rnE --include='*.java' '\.ordinal\(\)' "$JAVA_DIR")" && rc=0 || rc=$?
  [ "$rc" -le 1 ] || { echo "FAIL: ordinal-sites: grep failed under $JAVA_DIR" >&2; return 2; }
  [ -n "$hits" ] || return 0
  printf '%s\n' "$hits" | while IFS= read -r hit; do
    file="${hit%%:*}"; rest="${hit#*:}"; lineno="${rest%%:*}"; line="${rest#*:}"
    rel="${file#"$JAVA_DIR"/}"
    case "$(printf '%s' "$line" | sed 's/^[[:space:]]*//')" in '//'*|'*'*|'/*'*) continue ;; esac
    case "$line" in
      *'"'*) case "$line" in *'//'*)
        echo "FAIL: ordinal-sites: $rel:$lineno: cannot classify a line mixing a string literal and a comment: $line" >&2
        exit 2 ;; esac ;;
    esac
    line="$(printf '%s' "$line" | sed 's|//.*$||')"
    found="$(printf '%s\n' "$line" | grep -oE '\.ordinal\(\)' | grep -c . || true)"
    exprs="$(printf '%s\n' "$line" | grep -oE "$EXPR" || true)"
    if [ "$(printf '%s\n' "$exprs" | grep -c . || true)" -lt "$found" ]; then
      echo "FAIL: ordinal-sites: $rel:$lineno: cannot parse the receiver of every .ordinal() (put the receiver on the line as a dotted chain of names): $line" >&2
      exit 2
    fi
    [ -n "$exprs" ] || continue
    printf '%s\n' "$exprs" | while IFS= read -r e; do
      printf '%s\t%s\n' "$rel" "$e"
    done
  done | LC_ALL=C sort
}

have="$(sites)" || exit 2
if [ "${1:-}" = "--regen" ]; then
  printf '%s\n' "$have" | grep -v '^$' > "$LIST" || true
  echo "ordinal-sites: wrote $(grep -c . "$LIST" || true) site(s) to $LIST"
  exit 0
fi
[ -r "$LIST" ] || { echo "FAIL: ordinal-sites: cannot read the pinned list $LIST (bash $0 --regen)"; exit 2; }
want="$(LC_ALL=C sort "$LIST")"
added="$(LC_ALL=C comm -13 <(printf '%s\n' "$want" | grep -v '^$' || true) <(printf '%s\n' "$have" | grep -v '^$' || true))"
removed="$(LC_ALL=C comm -23 <(printf '%s\n' "$want" | grep -v '^$' || true) <(printf '%s\n' "$have" | grep -v '^$' || true))"
fail=0
if [ -n "$added" ]; then
  printf '%s\n' "$added" | while IFS="$(printf '\t')" read -r f e; do
    echo "FAIL: ordinal-sites: new ordinal site $f: $e (pin it in OrdinalContractTest and run --regen, or pass an explicit code)"
  done
  fail=1
fi
if [ -n "$removed" ]; then
  printf '%s\n' "$removed" | while IFS="$(printf '\t')" read -r f e; do
    echo "FAIL: ordinal-sites: pinned ordinal site gone $f: $e (run --regen)"
  done
  fail=1
fi
[ "$fail" -eq 0 ] && echo "ordinal-sites: OK ($(printf '%s\n' "$have" | grep -c . || true) pinned sites)"
exit "$fail"
