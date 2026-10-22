#!/usr/bin/env bash
# Every workflow that can be CALLED (declares `on.workflow_call`) keeps its
# publish-shaped jobs behind `inputs.dry_run`, so a qualification run that
# calls it can never publish. The guard must make the job impossible under
# `dry_run: true`: the `if:` has to contain `!inputs.dry_run`,
# `inputs.dry_run == false` or `inputs.dry_run != true` and no `||`; a mere
# mention of the input is not a guard. A job is publish-shaped when it has an
# `environment:`, a step `uses:` naming a publish action, or a step that runs
# `cargo publish`, `gradlew ... publish` or `gh release upload|create`.
# Callers are checked too: every `uses: ./.github/workflows/<x>.yml` job whose callee
# declares a `dry_run` input must pass `with: dry_run: true`.
# Usage: publish-guards.sh [workflow.yml ...]   (default: .github/workflows/*.yml)
# Uses python3 + PyYAML when importable, else a stdlib line parser.
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
if [[ $# -gt 0 ]]; then files=("$@"); else files=("$root"/.github/workflows/*.yml); fi

if python3 -c 'import yaml' 2>/dev/null; then
python3 - "${files[@]}" <<'PY'
import os, re, sys, yaml
PUB_USES = re.compile(r'pypa/gh-action-pypi-publish|publish', re.I)
PUB_RUN = re.compile(r'cargo publish|gradlew .*publish|gh release (upload|create)')
NEGATED = re.compile(r'!\s*inputs\.dry_run|inputs\.dry_run\s*==\s*false|inputs\.dry_run\s*!=\s*true')
def guarded(cond):
    return bool(NEGATED.search(cond)) and '||' not in cond
bad = []
def callee_has_dry_run(path):
    try:
        d = yaml.safe_load(open(path))
    except OSError:
        return False
    o = d.get(True, d.get('on')) or {}
    wc = o.get('workflow_call') if isinstance(o, dict) else None
    return isinstance(wc, dict) and 'dry_run' in (wc.get('inputs') or {})
for f in sys.argv[1:]:
    doc = yaml.safe_load(open(f))
    on = doc.get(True, doc.get('on')) or {}
    wfdir = os.path.join(os.path.dirname(os.path.abspath(f)))
    for name, job in (doc.get('jobs') or {}).items():
        u = str(job.get('uses', ''))
        if u.startswith('./.github/workflows/') and callee_has_dry_run(os.path.join(wfdir, os.path.basename(u))):
            if (job.get('with') or {}).get('dry_run') is not True:
                bad.append(f"{f}: job '{name}' calls {u} but does not pass `with: dry_run: true`")
    if not isinstance(on, dict) or 'workflow_call' not in on:
        continue
    for name, job in (doc.get('jobs') or {}).items():
        steps = job.get('steps') or []
        shaped = 'environment' in job or any(
            PUB_USES.search(str(s.get('uses', ''))) or PUB_RUN.search(str(s.get('run', '')))
            for s in steps)
        if shaped and not guarded(str(job.get('if', ''))):
            bad.append(f"{f}: job '{name}' is publish-shaped and callable but its `if:` does not negate inputs.dry_run")
if bad:
    print('\n'.join(bad)); sys.exit(1)
print('publish-guards: OK')
PY
else
fail=0
for f in "${files[@]}"; do
    d="$(dirname "$f")"
    awk -v F="$f" -v D="$d" '
        function flush(   cmd, c, line, hasdr) {
            if (job != "" && callee != "") {
                hasdr = 0
                while ((getline line < (D "/" callee)) > 0) { if (line ~ /^      dry_run:/) hasdr = 1 }
                close(D "/" callee)
                if (hasdr && !passes) { printf "%s: job \x27%s\x27 calls ./.github/workflows/%s but does not pass `with: dry_run: true`\n", F, job, callee; bad=1 }
            }
        }
        /^jobs:/ { injobs=1; next }
        injobs && /^  [A-Za-z0-9_-]+:/ { flush(); job=$1; sub(/:$/,"",job); callee=""; passes=0; next }
        injobs && /^    uses: \.\/\.github\/workflows\// { callee=$2; sub(/.*\//,"",callee) }
        injobs && /^      dry_run: true[ ]*$/ { passes=1 }
        END { flush(); exit bad }' "$f" || fail=1
    grep -qE '^  workflow_call:' "$f" || continue
    awk -v F="$f" '
        function flush() { if (job != "" && shaped && !guarded) { printf "%s: job \x27%s\x27 is publish-shaped and callable but its `if:` does not negate inputs.dry_run\n", F, job; bad=1 } }
        /^jobs:/ { injobs=1; next }
        injobs && /^  [A-Za-z0-9_-]+:/ { flush(); job=$1; sub(/:$/,"",job); shaped=0; guarded=0; next }
        injobs && /^    environment:/ { shaped=1 }
        injobs && /^    if:/ && (/![ ]*inputs\.dry_run/ || /inputs\.dry_run[ ]*==[ ]*false/ || /inputs\.dry_run[ ]*!=[ ]*true/) && !/\|\|/ { guarded=1 }
        injobs && /uses:.*publish/ { shaped=1 }
        injobs && /cargo publish|gradlew .*publish|gh release (upload|create)/ { shaped=1 }
        END { flush(); exit bad }' "$f" || fail=1
done
[[ $fail == 0 ]] && echo 'publish-guards: OK'
exit $fail
fi
