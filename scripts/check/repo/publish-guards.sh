#!/usr/bin/env bash
# The publish jobs' guards, read from the workflow files. Three rules:
#
# 1. Every workflow that can be CALLED (declares `on.workflow_call`) keeps its
#    publish-shaped jobs behind `inputs.dry_run`, so a qualification run that
#    calls it can never publish. The guard must make the job impossible under
#    `dry_run: true`: one top-level conjunct of the `if:` has to be
#    `!inputs.dry_run`, `inputs.dry_run == false` or `inputs.dry_run != true`,
#    and the `if:` has no `||`. A mention inside a negation or a longer name is
#    not a guard. A job is publish-shaped when it has an `environment:`, a step
#    `uses:` naming a publish action, or a step that runs `cargo publish`,
#    `gradlew ... publish` or `gh release upload|create`.
#    Callers are checked too: every `uses: ./.github/workflows/<x>.yml` job whose
#    callee declares a `dry_run` input must pass `with: dry_run: true`.
# 2. In every workflow, a job with an `environment:` is tag-only: its `if:` has
#    the conjuncts `github.event_name == 'push'` and
#    `startsWith(github.ref, 'refs/tags/v')`. The environment is the publishing
#    identity a registry trusts; a rehearsal never enters it.
# 3. A job that holds credentials (an `environment:`, or `id-token: write` /
#    `write-all` from its own or the workflow's permissions) pins every remote
#    action to a 40-hex commit sha: a moved tag must not run inside it. A local
#    composite action (`./...`) runs inside the job, so its `uses:` are held to
#    the same rule; one that cannot be read fails.
#
# A condition is read once per reader: drop a trailing comment and a
# `${{ ... }}` wrapper, refuse `||`, split at depth-0 `&&`, strip balanced outer
# parentheses from each conjunct, remove whitespace and compare. A condition
# that cannot be split (unbalanced) has no conjuncts, so it guards nothing.
# Usage: publish-guards.sh [workflow.yml ...]   (default: .github/workflows/*.yml)
# Uses python3 + PyYAML when importable, else a stdlib line parser (which reads
# single-line `if:` values only; a folded one guards nothing).
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
if [[ $# -gt 0 ]]; then files=("$@"); else files=("$root"/.github/workflows/*.yml); fi

NEG_MSG='is publish-shaped and callable but its `if:` does not negate inputs.dry_run'
TAG_MSG="has an environment but its \`if:\` does not require github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"

if python3 -c 'import yaml' 2>/dev/null; then
NEG_MSG="$NEG_MSG" TAG_MSG="$TAG_MSG" python3 - "${files[@]}" <<'PY'
import os, re, sys, yaml
PUB_USES = re.compile(r'pypa/gh-action-pypi-publish|publish', re.I)
PUB_RUN = re.compile(r'cargo publish|gradlew .*publish|gh release (upload|create)')
GUARDS = {'!inputs.dry_run', 'inputs.dry_run==false', 'inputs.dry_run!=true'}
TAG_CONJ = {"github.event_name=='push'", "startsWith(github.ref,'refs/tags/v')"}
SHA = re.compile(r'[^@\s]+@[0-9a-f]{40}')
NEG_MSG, TAG_MSG = os.environ['NEG_MSG'], os.environ['TAG_MSG']

def outer_wraps(p):
    """True when p's first '(' closes at its last character."""
    if not p.startswith('(') or not p.endswith(')'):
        return False
    depth, q = 0, None
    for i, ch in enumerate(p):
        if q:
            if ch == q: q = None
        elif ch in '\'"': q = ch
        elif ch == '(': depth += 1
        elif ch == ')':
            depth -= 1
            if depth == 0: return i == len(p) - 1
    return False

def conjuncts(cond):
    """The normalised top-level conjuncts, or [] when the condition cannot be read."""
    c = cond.strip()
    if c.startswith('${{') and c.endswith('}}'):
        c = c[3:-2]
    if '||' in c:
        return []
    parts, cur, depth, q, i = [], '', 0, None, 0
    while i < len(c):
        ch = c[i]
        if q:
            if ch == q: q = None
        elif ch in '\'"': q = ch
        elif ch == '(': depth += 1
        elif ch == ')':
            depth -= 1
            if depth < 0: return []
        elif c.startswith('&&', i) and depth == 0:
            parts.append(cur); cur = ''; i += 2; continue
        cur += ch; i += 1
    if depth or q:
        return []
    parts.append(cur)
    out = []
    for p in parts:
        p = re.sub(r'\s+', '', p)
        while outer_wraps(p):
            p = p[1:-1]
        out.append(p.replace('"', "'"))
    return out

def cred_perms(p):
    return p == 'write-all' or (isinstance(p, dict) and p.get('id-token') == 'write')

def unpinned(u):
    return not u.startswith('./') and not SHA.fullmatch(u)

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
    wfdir = os.path.dirname(os.path.abspath(f))
    repo = os.path.dirname(os.path.dirname(wfdir))
    jobs = doc.get('jobs') or {}
    callable_ = isinstance(on, dict) and 'workflow_call' in on
    for name, job in jobs.items():
        u = str(job.get('uses', ''))
        if u.startswith('./.github/workflows/') and callee_has_dry_run(os.path.join(wfdir, os.path.basename(u))):
            if (job.get('with') or {}).get('dry_run') is not True:
                bad.append(f"{f}: job '{name}' calls {u} but does not pass `with: dry_run: true`")
        steps = job.get('steps') or []
        cj = conjuncts(str(job.get('if', '')))
        has_env = 'environment' in job
        if callable_:
            shaped = has_env or any(
                PUB_USES.search(str(s.get('uses', ''))) or PUB_RUN.search(str(s.get('run', '')))
                for s in steps)
            if shaped and not GUARDS.intersection(cj):
                bad.append(f"{f}: job '{name}' {NEG_MSG}")
        if has_env and not TAG_CONJ.issubset(cj):
            bad.append(f"{f}: job '{name}' {TAG_MSG}")
        perms = job.get('permissions', doc.get('permissions'))
        if not (has_env or cred_perms(perms)):
            continue
        refs = [u] if u else []
        refs += [str(s['uses']) for s in steps if 'uses' in s]
        for r in refs:
            if unpinned(r):
                bad.append(f"{f}: job '{name}' holds credentials but uses {r}, which is not pinned to a commit sha")
            elif r.startswith('./') and not r.startswith('./.github/workflows/'):
                act = next((os.path.join(repo, r, n) for n in ('action.yml', 'action.yaml')
                            if os.path.isfile(os.path.join(repo, r, n))), None)
                if act is None:
                    bad.append(f"{f}: job '{name}' holds credentials but {r}/action.yml cannot be read")
                    continue
                a = yaml.safe_load(open(act)) or {}
                for s in ((a.get('runs') or {}).get('steps') or []):
                    if 'uses' in s and unpinned(str(s['uses'])):
                        bad.append(f"{f}: job '{name}' holds credentials but {r} uses {s['uses']}, which is not pinned to a commit sha")
if bad:
    print('\n'.join(bad)); sys.exit(1)
print('publish-guards: OK')
PY
else
fail=0
for f in "${files[@]}"; do
    d="$(dirname "$f")"
    repo="$(cd "$d/../.." 2>/dev/null && pwd || echo "$d/../..")"
    callable=0; grep -qE '^  workflow_call:' "$f" && callable=1
    awk -v F="$f" -v D="$d" -v R="$repo" -v C="$callable" -v SQ="'" -v NEG="$NEG_MSG" -v TAG="$TAG_MSG" '
        function outer_wraps(p,   i, ch, q, depth) {
            if (substr(p, 1, 1) != "(" || substr(p, length(p), 1) != ")") return 0
            q = ""; depth = 0
            for (i = 1; i <= length(p); i++) {
                ch = substr(p, i, 1)
                if (q != "") { if (ch == q) q = "" }
                else if (ch == SQ || ch == "\"") q = ch
                else if (ch == "(") depth++
                else if (ch == ")") { depth--; if (depth == 0) return i == length(p) }
            }
            return 0
        }
        # Fills CJ[1..n] with the normalised top-level conjuncts; returns n, 0 when unreadable.
        function conjuncts(c,   n, i, ch, q, depth, cur, k, p) {
            sub(/^[ \t]+/, "", c); sub(/[ \t]+$/, "", c)
            if (substr(c, 1, 3) == "${{" && substr(c, length(c) - 1) == "}}") c = substr(c, 4, length(c) - 5)
            if (index(c, "||")) return 0
            n = 0; q = ""; depth = 0; cur = ""
            for (i = 1; i <= length(c); i++) {
                ch = substr(c, i, 1)
                if (q != "") { if (ch == q) q = "" }
                else if (ch == SQ || ch == "\"") q = ch
                else if (ch == "(") depth++
                else if (ch == ")") { depth--; if (depth < 0) return 0 }
                else if (ch == "&" && substr(c, i + 1, 1) == "&" && depth == 0) { CJ[++n] = cur; cur = ""; i++; continue }
                cur = cur ch
            }
            if (depth != 0 || q != "") return 0
            CJ[++n] = cur
            for (k = 1; k <= n; k++) {
                p = CJ[k]; gsub(/[ \t]/, "", p)
                while (outer_wraps(p)) p = substr(p, 2, length(p) - 2)
                gsub(/"/, SQ, p); CJ[k] = p
            }
            return n
        }
        function has_conj(want,   k) { for (k = 1; k <= NC; k++) if (CJ[k] == want) return 1; return 0 }
        function pinned(u,   ref) {
            if (u ~ /^\.\//) return 1
            if (u !~ /^[^@ ]+@[0-9a-f]+$/) return 0
            ref = u; sub(/^[^@]*@/, "", ref)
            return length(ref) == 40
        }
        function uses_of(line,   u) { u = line; sub(/^[ \t-]*uses:[ \t]*/, "", u); sub(/[ \t]+#.*$/, "", u); sub(/[ \t]+$/, "", u); return u }
        function check_local(r,   path, line, u, found) {
            path = R "/" substr(r, 3) "/action.yml"
            if ((getline line < path) < 0) {
                path = R "/" substr(r, 3) "/action.yaml"
                if ((getline line < path) < 0) { printf "%s: job \x27%s\x27 holds credentials but %s/action.yml cannot be read\n", F, job, r; bad = 1; return }
            }
            do {
                if (line ~ /^[ \t-]*uses:/) { u = uses_of(line); if (!pinned(u)) { printf "%s: job \x27%s\x27 holds credentials but %s uses %s, which is not pinned to a commit sha\n", F, job, r, u; bad = 1 } }
            } while ((getline line < path) > 0)
            close(path)
        }
        function flush(   line, hasdr, k, cred) {
            if (job == "") return
            if (callee != "") {
                hasdr = 0
                while ((getline line < (D "/" callee)) > 0) { if (line ~ /^      dry_run:/) hasdr = 1 }
                close(D "/" callee)
                if (hasdr && !passes) { printf "%s: job \x27%s\x27 calls ./.github/workflows/%s but does not pass `with: dry_run: true`\n", F, job, callee; bad = 1 }
            }
            NC = conjuncts(cond)
            if (C && (env || shaped) && !(has_conj("!inputs.dry_run") || has_conj("inputs.dry_run==false") || has_conj("inputs.dry_run!=true"))) { printf "%s: job \x27%s\x27 %s\n", F, job, NEG; bad = 1 }
            if (env && !(has_conj("github.event_name==" SQ "push" SQ) && has_conj("startsWith(github.ref," SQ "refs/tags/v" SQ ")"))) { printf "%s: job \x27%s\x27 %s\n", F, job, TAG; bad = 1 }
            cred = env || jcred || (!jperm && topcred)
            if (cred) for (k = 1; k <= nu; k++) {
                if (!pinned(U[k])) { printf "%s: job \x27%s\x27 holds credentials but uses %s, which is not pinned to a commit sha\n", F, job, U[k]; bad = 1 }
                else if (U[k] ~ /^\.\// && U[k] !~ /^\.\/\.github\/workflows\//) check_local(U[k])
            }
        }
        # The workflow-level permissions, read before `jobs:`.
        !injobs && /^permissions:[ \t]*write-all/ { topcred = 1 }
        !injobs && /^permissions:/ { intop = 1; next }
        !injobs && intop && /^  id-token:[ \t]*write/ { topcred = 1 }
        !injobs && /^[^ #]/ { intop = 0 }
        /^jobs:/ { injobs = 1; next }
        injobs && /^  [A-Za-z0-9_-]+:/ { flush(); job = $1; sub(/:$/, "", job); callee = ""; passes = 0; env = 0; shaped = 0; cond = ""; jperm = 0; jcred = 0; inperm = 0; nu = 0; next }
        injobs && /^    [A-Za-z0-9_-]+:/ { inperm = 0 }
        injobs && /^    permissions:/ { jperm = 1; inperm = 1; if ($0 ~ /write-all/) jcred = 1 }
        injobs && inperm && /^      id-token:[ \t]*write/ { jcred = 1 }
        injobs && /^    uses: \.\/\.github\/workflows\// { callee = $2; sub(/.*\//, "", callee) }
        injobs && /^      dry_run: true[ ]*$/ { passes = 1 }
        injobs && /^    environment:/ { env = 1 }
        # A trailing YAML comment is not part of the condition.
        injobs && /^    if:/ { cond = $0; sub(/[ \t]#.*$/, "", cond); sub(/^    if:/, "", cond) }
        injobs && /^[ \t-]*uses:/ { U[++nu] = uses_of($0) }
        injobs && /uses:.*publish/ { shaped = 1 }
        injobs && /cargo publish|gradlew .*publish|gh release (upload|create)/ { shaped = 1 }
        END { flush(); exit bad }' "$f" || fail=1
done
[[ $fail == 0 ]] && echo 'publish-guards: OK'
exit $fail
fi
