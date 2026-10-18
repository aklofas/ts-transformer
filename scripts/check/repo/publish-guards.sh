#!/usr/bin/env bash
# Every workflow that can be CALLED (declares `on.workflow_call`) keeps its
# publish-shaped jobs behind `inputs.dry_run`, so a qualification run that
# calls it can never publish. A job is publish-shaped when it has an
# `environment:`, a step `uses:` naming a publish action, or a step that runs
# `cargo publish`, `gradlew ... publish` or `gh release upload|create`.
# Usage: publish-guards.sh [workflow.yml ...]   (default: .github/workflows/*.yml)
# Uses python3 + PyYAML when importable, else a stdlib line parser.
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
if [[ $# -gt 0 ]]; then files=("$@"); else files=("$root"/.github/workflows/*.yml); fi

if python3 -c 'import yaml' 2>/dev/null; then
python3 - "${files[@]}" <<'PY'
import re, sys, yaml
PUB_USES = re.compile(r'pypa/gh-action-pypi-publish|publish', re.I)
PUB_RUN = re.compile(r'cargo publish|gradlew .*publish|gh release (upload|create)')
bad = []
for f in sys.argv[1:]:
    doc = yaml.safe_load(open(f))
    on = doc.get(True, doc.get('on')) or {}
    if not isinstance(on, dict) or 'workflow_call' not in on:
        continue
    for name, job in (doc.get('jobs') or {}).items():
        steps = job.get('steps') or []
        shaped = 'environment' in job or any(
            PUB_USES.search(str(s.get('uses', ''))) or PUB_RUN.search(str(s.get('run', '')))
            for s in steps)
        if shaped and 'inputs.dry_run' not in str(job.get('if', '')):
            bad.append(f"{f}: job '{name}' is publish-shaped and callable but its `if:` lacks inputs.dry_run")
if bad:
    print('\n'.join(bad)); sys.exit(1)
print('publish-guards: OK')
PY
else
fail=0
for f in "${files[@]}"; do
    grep -qE '^  workflow_call:' "$f" || continue
    awk -v F="$f" '
        function flush() { if (job != "" && shaped && !guarded) { printf "%s: job \x27%s\x27 is publish-shaped and callable but its `if:` lacks inputs.dry_run\n", F, job; bad=1 } }
        /^jobs:/ { injobs=1; next }
        injobs && /^  [A-Za-z0-9_-]+:/ { flush(); job=$1; sub(/:$/,"",job); shaped=0; guarded=0; next }
        injobs && /^    environment:/ { shaped=1 }
        injobs && /^    if:/ && /inputs\.dry_run/ { guarded=1 }
        injobs && /uses:.*publish/ { shaped=1 }
        injobs && /cargo publish|gradlew .*publish|gh release (upload|create)/ { shaped=1 }
        END { flush(); exit bad }' "$f" || fail=1
done
[[ $fail == 0 ]] && echo 'publish-guards: OK'
exit $fail
fi
