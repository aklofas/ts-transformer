#!/usr/bin/env bash
# Self-test for the attach job of .github/workflows/release-qualify.yml: its
# upload step's own `run:` text, extracted from the workflow and run with `gh`
# stubbed on PATH. A published Release is refused by name, a draft gets its
# assets replaced, a missing Release is created as a draft.
# RELEASE_QUALIFY_WORKFLOW overrides the workflow path.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WF="${RELEASE_QUALIFY_WORKFLOW:-$HERE/../../.github/workflows/release-qualify.yml}"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/    /' "$T/log" "$T/gh.log" 2>/dev/null; fail=1; }

# The block under `run: |` of the step named "Verify the checksums, then upload".
awk '/^      - name: Verify the checksums, then upload$/ {s = 1; next}
     s && /^        run: \|$/ {r = 1; next}
     r && /^      - / {exit}
     r && /^  [A-Za-z0-9_-]+:/ {exit}
     r {sub(/^          /, ""); print}' "$WF" > "$T/step.sh"
grep -q 'gh release upload' "$T/step.sh" || { echo "FAIL: no attach step found in $WF"; exit 1; }

mkdir -p "$T/bin" "$T/rt/bundle"
echo record > "$T/rt/bundle/qualification.json"
(cd "$T/rt/bundle" && sha256sum qualification.json > SHA256SUMS)
# view: exits with $T/view-rc; with --json it prints $T/draft. upload/create log.
cat > "$T/bin/gh" <<STUB
#!/usr/bin/env bash
echo "gh \$*" >> "$T/gh.log"
case "\$1 \$2" in
  "release view") rc="\$(cat "$T/view-rc")"; [ "\$rc" = 0 ] || exit "\$rc"
                  case " \$* " in *" --json isDraft "*) cat "$T/draft" ;; esac ;;
  "release upload"|"release create") ;;
  *) echo "unexpected gh call: \$*" >&2; exit 3 ;;
esac
STUB
chmod +x "$T/bin/gh"
run() { # <view-rc> <isDraft output>
  echo "$1" > "$T/view-rc"; printf '%s' "$2" > "$T/draft"; : > "$T/gh.log"
  env PATH="$T/bin:$PATH" RUNNER_TEMP="$T/rt" TAG=v0.0.1 GITHUB_REPOSITORY=x/y bash -eo pipefail "$T/step.sh" > "$T/log" 2>&1
}
uploads() { grep -cE '^gh release (upload|create)' "$T/gh.log"; }

run 0 false; rc=$?
[ $rc != 0 ] && grep -qxF '::error::the v0.0.1 Release is published; its assets are not replaced' "$T/log" && [ "$(uploads)" = 0 ] \
  && ok "a published Release is refused by name, nothing uploaded" || no "published Release: rc=$rc"
run 0 ""; rc=$?
[ $rc != 0 ] && [ "$(uploads)" = 0 ] && ok "an unreadable draft flag is refused" || no "unreadable isDraft: rc=$rc"
run 0 true; rc=$?
[ $rc = 0 ] && grep -q '^gh release upload v0.0.1 -R x/y --clobber' "$T/gh.log" && ok "a draft gets its assets replaced" || no "draft Release: rc=$rc"
run 1 ""; rc=$?
[ $rc = 0 ] && grep -q '^gh release create v0.0.1 -R x/y --draft' "$T/gh.log" && ok "a missing Release is created as a draft" || no "missing Release: rc=$rc"

[ $fail = 0 ] && echo "test_attach_draft_only: OK"
exit $fail
