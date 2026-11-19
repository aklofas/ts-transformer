#!/usr/bin/env bash
# Self-test for scripts/release/post-publish-consumers.sh with python3, pip,
# cargo and gradle stubbed on PATH: argument handling, the retry loop, which
# parts run, and the `PART <name>: rc=N` summary with its exit status.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../post-publish-consumers.sh"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
cases=0
ok() { cases=$((cases + 1)); }
no() { echo "FAIL: $1"; sed 's/^/    /' "$T/out"; exit 1; }

mkdir -p "$T/bin"
# python3 -m venv <dir>: a venv whose pip and python are stubs that log their call.
cat > "$T/bin/python3" <<STUB
#!/bin/sh
[ "\$1" = "-m" ] && [ "\$2" = "venv" ] || exit 9
mkdir -p "\$3/bin"
printf '#!/bin/sh\necho "pip \$*" >> "$T/calls.log"\nn=\$(cat "$T/pip.count" 2>/dev/null || echo 0); n=\$((n + 1)); echo \$n > "$T/pip.count"\n[ \$n -gt \${STUB_PIP_FAILS:-0} ]\n' > "\$3/bin/pip"
printf '#!/bin/sh\necho "python \$*" >> "$T/calls.log"\n[ "\${STUB_PY_FAIL:-0}" = 0 ]\n' > "\$3/bin/python"
chmod +x "\$3/bin/pip" "\$3/bin/python"
STUB
cat > "$T/bin/cargo" <<STUB
#!/bin/sh
echo "cargo \$*" >> "$T/calls.log"
[ "\$1" = fetch ] && { n=\$(cat "$T/fetch.count" 2>/dev/null || echo 0); n=\$((n + 1)); echo \$n > "$T/fetch.count"; [ \$n -gt \${STUB_FETCH_FAILS:-0} ]; exit; }
[ "\$1" = build ] && [ "\${STUB_BUILD_FAIL:-0}" != 0 ] && exit 1
exit 0
STUB
cat > "$T/bin/gradle" <<STUB
#!/bin/sh
echo "gradle \$*" >> "$T/calls.log"
exit 0
STUB
chmod +x "$T/bin/"*
mkdir -p "$T/java"; echo 'module org.tstrans.smoke {}' > "$T/java/module-info.java"

run() { # <env assignments...> -- <args...>
  : > "$T/calls.log"; rm -f "$T/pip.count" "$T/fetch.count"
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done; shift
  env PATH="$T/bin:$PATH" PPC_INTERVAL=0 PPC_ATTEMPTS=3 PPC_CRATES="tst-core tst-srt" PPC_JAVA_SRC="$T/java" "${envs[@]}" \
    bash "$SCRIPT" "$@" > "$T/out" 2>&1
}
expect_rc() { # <desc> <rc> ...run args
  local desc="$1" want="$2"; shift 2
  run "$@"; local rc=$?
  [ "$rc" = "$want" ] || no "$desc: rc $rc, want $want"
  ok
}
has() { grep -qF -- "$2" "$T/$3" || no "$1: lacks '$2' in $3"; ok; }
hasnt() { ! grep -qF -- "$2" "$T/$3" || no "$1: unexpected '$2' in $3"; ok; }

expect_rc "no version" 2 -- ; has "no version" "usage:" out
expect_rc "a bad version" 2 -- 0.10 ; has "a bad version" "not X.Y.Z" out
expect_rc "an unknown part" 2 -- 0.10.0 npm ; has "an unknown part" "unknown part 'npm'" out

expect_rc "all parts pass" 0 -- 0.10.0
for part in pypi crates maven; do has "all parts pass" "PART $part: rc=0" out; done
has "pip gets the exact version" "tstrans==0.10.0" calls.log
has "the consumer is built" "cargo build" calls.log

expect_rc "one part only" 0 -- 0.10.0 maven
hasnt "one part only" "PART pypi" out; hasnt "one part only" "pip " calls.log; has "one part only" "PART maven: rc=0" out

expect_rc "pip lag is retried" 0 STUB_PIP_FAILS=2 -- 0.10.0 pypi
[ "$(grep -c '^pip install' "$T/calls.log")" = 3 ] || no "pip lag is retried: want 3 install attempts"; ok
has "pip lag is retried" "PART pypi: rc=0" out

expect_rc "pip that never succeeds fails the part" 1 STUB_PIP_FAILS=9 -- 0.10.0 pypi
[ "$(grep -c '^pip install' "$T/calls.log")" = 3 ] || no "attempt cap: want 3 install attempts"; ok
has "pip that never succeeds" "PART pypi: rc=1" out
hasnt "the import is not run after a failed install" "python " calls.log

expect_rc "a failed import fails the part" 1 STUB_PY_FAIL=1 -- 0.10.0 pypi
has "a failed import" "PART pypi: rc=1" out

expect_rc "cargo fetch lag is retried" 0 STUB_FETCH_FAILS=1 -- 0.10.0 crates
has "cargo fetch lag" "PART crates: rc=0" out

expect_rc "a failed build fails the part" 1 STUB_BUILD_FAIL=1 -- 0.10.0 crates
has "a failed build" "PART crates: rc=1" out
hasnt "no run after a failed build" "cargo run" calls.log

expect_rc "one failing part fails the run, the others still run" 1 STUB_BUILD_FAIL=1 -- 0.10.0
has "the failing part" "PART crates: rc=1" out; has "the passing parts" "PART pypi: rc=0" out; has "the passing parts" "PART maven: rc=0" out

expect_rc "missing consumer sources fail the maven part" 1 PPC_JAVA_SRC="$T/none" -- 0.10.0 maven
has "missing sources" "PART maven: rc=1" out

echo "PASS: $cases cases"
exit $fail
