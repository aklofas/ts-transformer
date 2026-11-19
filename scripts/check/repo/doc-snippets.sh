#!/usr/bin/env bash
# The Java and C code blocks of docs/languages/{jvm,c}.md that carry a
# `<!-- compile: ... -->` marker compile against the real binding, so a method
# that does not exist or a wrong argument order fails CI instead of a reader.
# Marker grammar and the wrapper: scripts/gen/doc_snippets.py.
#
#   doc-snippets.sh c <include-dir>     cc -fsyntax-only against tstrans.h of an
#                                       all-features package (c-package.sh)
#   doc-snippets.sh java <classpath>    javac against the built jar or classes
#
# Without a built input the rail prints SKIP and exits 0, like stubtest.sh, so
# the bare sweep of scripts/check stays green; CI passes the inputs. Linux or
# any host with cc / javac; JDK 17+ (compiled with --release 17).
# DOC_SNIPPETS_PAGE names one page instead of the language's default.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
lang="${1:-}"
input="${2:-}"

if [ -z "$lang" ] || [ -z "$input" ]; then
  echo "doc-snippets: SKIP (no input: pass 'c <include-dir>' or 'java <classpath>')"
  exit 0
fi

case "$lang" in
  c) page="${DOC_SNIPPETS_PAGE:-$ROOT/docs/languages/c.md}"; ext=c ;;
  java) page="${DOC_SNIPPETS_PAGE:-$ROOT/docs/languages/jvm.md}"; ext=java ;;
  *) echo "usage: doc-snippets.sh c <include-dir> | java <classpath>" >&2; exit 2 ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
python3 -I "$ROOT/scripts/gen/doc_snippets.py" extract --out "$tmp/src" "$page" > "$tmp/list"

files=()
while IFS= read -r f; do
  case "$f" in *."$ext") files+=("$f") ;; esac
done < "$tmp/list"
# A page whose markers all went missing would pass by compiling nothing.
if [ "${#files[@]}" -eq 0 ]; then
  echo "doc-snippets: FAIL: no marked $lang snippet found in $page" >&2
  exit 1
fi

if [ "$lang" = c ]; then
  rc=0
  for f in "${files[@]}"; do
    cc -fsyntax-only -Wall -Werror \
      -I "$input" "$f" || { echo "doc-snippets: FAIL: $f (from $page)" >&2; rc=1; }
  done
  [ "$rc" = 0 ] || exit 1
else
  mkdir -p "$tmp/out"
  javac --release 17 -proc:none -Xlint:-options -d "$tmp/out" -cp "$input" "${files[@]}" \
    || { echo "doc-snippets: FAIL: a snippet of $page did not compile" >&2; exit 1; }
fi
echo "doc-snippets: ok: ${#files[@]} $lang snippet files of $(basename "$page") compile"
