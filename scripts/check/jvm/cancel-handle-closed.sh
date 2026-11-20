#!/usr/bin/env bash
# Bash ratchet: every `.cancelHandle()` and `::cancelHandle` in the JVM test
# sources is closed.
# A JVM cancel handle holds a native registry entry until its own close();
# nothing frees it otherwise, so a test that takes one and drops it leaks it
# for the rest of the run.
#
# A `.cancelHandle()` passes when it is
#   - the initializer of a resource in a try-with-resources header
#     (`try (CancelHandle h = rx.cancelHandle()) {`, also on a later line of
#     a header whose resources end in `;`),
#   - the argument of `new Watchdog(`, `new Parked(` (first or second
#     argument) or `c.add(` / `cleanup.add(` (a Cleanup), which close it, or
#     of `cancelNow(` (as `() -> x.cancelHandle()`),
#   - closed at once (`x.cancelHandle().close();`),
#   - returned (`return x.cancelHandle();`: the caller's own site is checked),
#   - or marked `// cancel-handle: <reason>` on its line or the line above.
# A method reference `x::cancelHandle` passes as the argument of `cancelNow(`,
# anywhere after a `c.add(` / `cleanup.add(` or an `assertThrows(` on its line
# (a closed shell's cancelHandle() throws, so no handle exists), or marked.
#
# What a line scanner cannot see: a receiver chain split across lines (the
# site fails; put `x.cancelHandle()` on one line), whether the try resource
# or the Cleanup entry is the handle itself rather than something made from
# it, and whether a variable named `c` or `cleanup` is a Cleanup.
#
# Run it:       bash scripts/check/jvm/cancel-handle-closed.sh
# Overridable (the ratchet self-test points it at fixtures):
#   CANCEL_HANDLE_JAVA_DIR  the Java test source root to scan
# Portable: find + POSIX awk, bash 3.2 syntax.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
JAVA_DIR="${CANCEL_HANDLE_JAVA_DIR:-$ROOT/bindings/jvm/src/test/java}"

[ -d "$JAVA_DIR" ] || { echo "FAIL: cancel-handle-closed: Java test source root not found: $JAVA_DIR"; exit 2; }

files="$(find "$JAVA_DIR" -name '*.java' | LC_ALL=C sort)"
[ -n "$files" ] || { echo "FAIL: cancel-handle-closed: no Java file under $JAVA_DIR"; exit 2; }

# shellcheck disable=SC2086
result="$(printf '%s\n' "$files" | while IFS= read -r f; do
  awk -v rel="${f#"$JAVA_DIR"/}" -v sq="'" '
    # The line without string and char literal contents and without its
    # comments, so a "//" inside a URL is not taken for a comment.
    function code(line,    out, i, c, n, q, rest, j) {
      out = ""; q = ""; n = length(line)
      for (i = 1; i <= n; i++) {
        c = substr(line, i, 1)
        if (q != "") {
          if (c == "\\") { i++; continue }
          if (c == q) { q = ""; out = out c }
          continue
        }
        if (c == "\"" || c == sq) { q = c; out = out c; continue }
        if (c == "/" && substr(line, i + 1, 1) == "/") break
        if (c == "/" && substr(line, i + 1, 1) == "*") {
          rest = substr(line, i + 2); j = index(rest, "*/")
          if (j == 0) break
          i = i + 1 + j + 1; continue
        }
        out = out c
      }
      return out
    }
    function marked(line) { return line ~ /\/\/[ \t]*cancel-handle:[ \t]*[^ \t]/ }
    # Whether the line continues a try-with-resources header: walking back
    # over lines that end a resource (`;`) reaches a `try (` with no brace.
    function in_header(k,    m) {
      for (m = k - 1; m >= 1 && m >= k - 8; m--) {
        if (codes[m] ~ /^[ \t]*$/) continue
        if (codes[m] ~ /[{}]/) return 0
        if (codes[m] ~ /try[ \t]*\(/) return 1
        if (codes[m] !~ /;[ \t]*$/) return 0
      }
      return 0
    }
    { lines[NR] = $0; codes[NR] = code($0) }
    END {
      recv = "[A-Za-z_][A-Za-z0-9_.()]*"
      decl = "[A-Za-z_][A-Za-z0-9_.<>]*[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*=[ \t]*" recv "$"
      cleanup = "(^|[^A-Za-z0-9_])(c|cleanup)\\.add[ \t]*\\("
      arg = "(new[ \t]+(ShellPeers\\.)?(Watchdog|Parked)|(^|[^A-Za-z0-9_])(c|cleanup)\\.add|cancelNow)[ \t]*\\([ \t]*([A-Za-z_][A-Za-z0-9_.]*[ \t]*,[ \t]*)?(\\([ \t]*\\)[ \t]*->[ \t]*)?" recv "$"
      for (k = 1; k <= NR; k++) {
        c = codes[k]
        t = lines[k]; sub(/^[ \t]+/, "", t)
        if (t ~ /^(\*|\/\*|\/\/)/) continue
        rest = c; off = 0
        while ((p = index(rest, ".cancelHandle()")) > 0) {
          sites++
          pre = substr(c, 1, off + p - 1)
          ok = 0
          post = substr(c, off + p + length(".cancelHandle()"))
          if (marked(lines[k]) || (k > 1 && marked(lines[k - 1]))) ok = 1
          else if (post ~ /^[ \t]*\.close\(\)/) ok = 1
          else if (pre ~ /return[ \t]+[A-Za-z_][A-Za-z0-9_.()]*$/) ok = 1
          else if (pre ~ arg) ok = 1
          else if (pre ~ decl) {
            if (pre ~ /try[ \t]*\(/) ok = 1
            else if (pre ~ /^[ \t]*[A-Za-z_][A-Za-z0-9_.<>]*[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*=/ && in_header(k)) ok = 1
          }
          if (!ok) printf "SITE\t%s:%d: %s\n", rel, k, t
          off += p + length(".cancelHandle()") - 1
          rest = substr(c, off + 1)
        }
        rest = c; off = 0
        while ((p = index(rest, "::cancelHandle")) > 0) {
          sites++
          pre = substr(c, 1, off + p - 1)
          ok = 0
          if (marked(lines[k]) || (k > 1 && marked(lines[k - 1]))) ok = 1
          else if (pre ~ /cancelNow[ \t]*\([ \t]*[A-Za-z_][A-Za-z0-9_.]*$/) ok = 1
          else if (pre ~ /assertThrows[ \t]*\(/) ok = 1
          else if (pre ~ cleanup) ok = 1
          if (!ok) printf "SITE\t%s:%d: %s\n", rel, k, t
          off += p + length("::cancelHandle") - 1
          rest = substr(c, off + 1)
        }
      }
      printf "COUNT\t%d\n", sites + 0
    }' "$f"
done)"

total=0
while IFS="$(printf '\t')" read -r tag val; do
  [ "$tag" = COUNT ] && total=$((total + val))
done <<EOF_RESULT
$result
EOF_RESULT

leaks="$(printf '%s\n' "$result" | grep '^SITE' | cut -f2- || true)"
if [ "$total" -eq 0 ]; then
  echo "FAIL: cancel-handle-closed: no cancelHandle site found under $JAVA_DIR: nothing was checked"
  exit 2
fi
if [ -n "$leaks" ]; then
  printf '%s\n' "$leaks" | while IFS= read -r l; do
    echo "FAIL: cancel-handle-closed: $l"
  done
  echo "FAIL: cancel-handle-closed: $(printf '%s\n' "$leaks" | grep -c .) of $total cancelHandle sites are never closed (try-with-resources, Watchdog, Parked, Cleanup.add, cancelNow, or // cancel-handle: <reason>)"
  exit 1
fi
echo "OK: cancel-handle-closed: all $total cancelHandle sites under ${JAVA_DIR#"$ROOT"/} are closed"
