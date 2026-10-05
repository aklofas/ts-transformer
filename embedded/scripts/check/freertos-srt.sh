#!/usr/bin/env bash
# Build + run one freertos-srt target under QEMU mps2-an386 and assert its PASS
# token(s). Usage: embedded/scripts/check/freertos-srt.sh <target>.
#
# Missing prerequisites (ARM cross-toolchain / QEMU / cmake / cargo) skip cleanly
# by default so a local box without the toolchain is green. Set
# FREERTOS_SRT_REQUIRE_TOOLS=1 to fail closed instead — CI hard-gates set it so a
# runner-image regression that drops a prerequisite goes red rather than quietly
# reducing coverage to a no-op pass.
set -euo pipefail
cd "$(dirname "$0")/../../.."
t="${1:?usage: embedded/scripts/check/freertos-srt.sh <exceptions|lwip-loopback|libsrt-smoke|loopback-arq|arq-connfail|example|srt-recv|fault-smoke|malloc-stress>}"
D=embedded/freertos-srt
REQUIRE="${FREERTOS_SRT_REQUIRE_TOOLS:-0}"

# Skip (default) or fail closed (FREERTOS_SRT_REQUIRE_TOOLS=1) when a tool is
# absent. A skip exits 0; a required-but-missing tool exits 1.
need() { # $1=binary  $2=human label
  command -v "$1" >/dev/null 2>&1 && return 0
  if [ "$REQUIRE" = "1" ]; then echo "FATAL: required tool '$2' not installed (FREERTOS_SRT_REQUIRE_TOOLS=1)"; exit 1; fi
  echo "SKIP: $2 not installed"; exit 0
}
need arm-none-eabi-g++ arm-none-eabi-g++
need arm-none-eabi-gcc arm-none-eabi-gcc
need qemu-system-arm   qemu-system-arm

# Entropy-stub contract: without TST_QEMU_TEST_ENTROPY the substrate must NOT
# emit the deterministic hooks (production builds must link real ones).
STUB=embedded/freertos-srt/substrate/syscalls_stub.c
TMPO=$(mktemp /tmp/syscalls_stub_XXXX.o)
arm-none-eabi-gcc -c "$STUB" -o "$TMPO" -I embedded/freertos-srt/substrate
if arm-none-eabi-nm "$TMPO" | grep -qE '[[:space:]]T[[:space:]]+(_getentropy|mbedtls_hardware_poll)$'; then
  echo "FAIL[entropy-stub]: deterministic hooks emitted without TST_QEMU_TEST_ENTROPY"; rm -f "$TMPO"; exit 1
fi
arm-none-eabi-gcc -c "$STUB" -o "$TMPO" -I embedded/freertos-srt/substrate -DTST_QEMU_TEST_ENTROPY=1
NM_OUT=$(arm-none-eabi-nm "$TMPO")
grep -qE '[[:space:]]T[[:space:]]+_getentropy$' <<<"$NM_OUT" || { echo "FAIL[entropy-stub]: hooks missing WITH the define"; rm -f "$TMPO"; exit 1; }
grep -qE '[[:space:]]T[[:space:]]+mbedtls_hardware_poll$' <<<"$NM_OUT" || { echo "FAIL[entropy-stub]: hooks missing WITH the define"; rm -f "$TMPO"; exit 1; }
rm -f "$TMPO"

# ---------------------------------------------------------------------------
# QEMU retry driver.
#
# Every QEMU leg is TCG-emulation-timing sensitive; three flake shapes are on
# record (rc=124 full-budget hang, qemu-system-arm crashing outright, and a
# labeled FAIL[...] whose assertion depends on emulation timing). Policy
# (2026-07-16): a failed leg is retried EXACTLY ONCE with the identical,
# already-built binary. A pass-on-retry is reported loudly (FLAKY PASS + a
# GitHub ::warning:: annotation) so flake frequency stays visible in CI; a
# deterministic regression fails both attempts and stays hard-red. Builds run
# OUTSIDE the retry — a build failure is never a flake.
# If retry proves insufficient, next escalations: QEMU -icount deterministic
# virtual time, or a budget scaled from a host-speed probe.
# ---------------------------------------------------------------------------

# Run firmware.elf once under QEMU with a wall-clock budget. Sets out/rc/t0/t1
# (globals — read by the verdict functions below). Token-only — not exit-code —
# verdicts because ARM semihosting SYS_EXIT propagation through qemu-system-arm
# is version-dependent; the firmware prints exactly one PASS line on success.
# stderr is folded in (2>&1) so SYS_WRITE0 diagnostics (FAIL[hardfault]/
# FAIL[assert]/...) appear in the failure dump — those go to QEMU stderr while
# printf/SYS_WRITE goes to stdout.
run_qemu() { # $1=timeout-seconds
  t0=$(date +%s)
  if out=$(timeout "$1" qemu-system-arm -machine mps2-an386 -nographic \
        -semihosting-config enable=on,target=native -kernel "$D/build/firmware.elf" 2>&1); then
    rc=0
  else
    rc=$?
  fi
  t1=$(date +%s)
}

# retry_leg <budget-s> <label> <verdict-fn>: run the leg, retry once on any
# verdict failure. The verdict fn reads the run_qemu globals, prints its own
# "  verdict: ..." reason when it fails, and returns 0/1.
retry_leg() {
  local budget="$1" label="$2" verdict="$3" first_rc first_elapsed
  run_qemu "$budget"
  if "$verdict"; then return 0; fi
  first_rc=$rc; first_elapsed=$((t1 - t0))
  echo "FIRST ATTEMPT FAILED ($label) — qemu rc=$first_rc, elapsed=${first_elapsed}s of ${budget}s budget; retrying once (known QEMU/TCG timing-flake class)"
  echo "----- QEMU output ($label, attempt 1) -----"; echo "$out"
  run_qemu "$budget"
  if "$verdict"; then
    echo "FLAKY PASS ($label): passed on retry; first failure rc=$first_rc elapsed=${first_elapsed}s"
    echo "::warning::freertos-srt $label: passed on retry (first failure rc=$first_rc, elapsed=${first_elapsed}s) — QEMU timing flake"
    return 0
  fi
  echo "GATE FAILED ($label) — qemu rc=$rc, elapsed=$((t1 - t0))s of ${budget}s budget (failed BOTH attempts)"
  echo "  (rc=124 + full budget = hang/timeout; fast nonzero rc = a labeled FAIL[...] exit — read the transcript)"
  echo "----- QEMU output ($label, attempt 2) -----"; echo "$out"
  exit 1
}

# Token verdict for the plain PASS-token legs. TOKEN is set by assert_pass.
v_token() {
  grep -q "$TOKEN" <<<"$out" && return 0
  echo "  verdict: expected token '$TOKEN' not found"
  return 1
}

assert_pass() { # $1=timeout  $2=token  $3=label
  TOKEN="$2"
  retry_leg "$1" "$3" v_token
}

# fault-smoke: the firmware faults ON PURPOSE; the gate asserts the fault is
# REPORTED (labeled, with a pc=) and FAST (not a silent wedge until timeout).
v_fault_smoke() {
  grep -q 'FAIL\[hardfault\] pc=0x' <<<"$out" || {
    echo "  verdict: no labeled fault report"; return 1; }
  [ $((t1 - t0)) -lt 25 ] || {
    echo "  verdict: fault report was not fast: $((t1 - t0))s"; return 1; }
  return 0
}

# arq-connfail: a caller-side connect failure must abort the listener join
# and report fast.
v_arq_connfail() {
  grep -q 'FAIL\[s3_srt_plain\]: where=connect' <<<"$out" || {
    echo "  verdict: no labeled connect-failure verdict — a caller-side failure"
    echo "  wedged the listener join instead of aborting it; EMB-JOIN-1 regressed"
    return 1; }
  [ $((t1 - t0)) -lt 30 ] || {
    echo "  verdict: failure was not fast: $((t1 - t0))s"; return 1; }
  return 0
}

case "$t" in
  exceptions)    ( cd "$D" && ./build.sh exceptions >/dev/null )
                 assert_pass 60 'PASS: s0_cpp_gate' "$t" ;;
  lwip-loopback) ( cd "$D" && ./build.sh lwip-loopback >/dev/null )
                 assert_pass 60 'PASS: s1_lwip ' "$t" ;;
  libsrt-smoke)  need cmake cmake
                 ( cd "$D" && ./build.sh libsrt-smoke >/dev/null )
                 assert_pass 90 'PASS: s2_libsrt' "$t" ;;
  loopback-arq)  need cmake cmake
                 ( cd "$D" && ENCRYPT=0 ./build.sh loopback-arq >/dev/null )
                 assert_pass 150 'PASS: s3_srt_plain' "$t plain"
                 ( cd "$D" && ENCRYPT=1 ./build.sh loopback-arq >/dev/null )
                 assert_pass 150 'PASS: s3_srt_aes' "$t aes" ;;
  example)       need cmake cmake
                 need cargo cargo
                 bash "$(dirname "$0")/../lib/run-freertos-srt-example.sh" || exit 1 ;;
  srt-recv)      need cmake cmake
                 need cargo cargo
                 bash "$(dirname "$0")/../lib/run-freertos-srt-srt-recv.sh" || exit 1 ;;
  fault-smoke)   ( cd "$D" && ./build.sh fault-smoke >/dev/null 2>&1 )
                 retry_leg 30 "$t" v_fault_smoke
                 echo "OK: deliberate fault produced labeled fast failure" ;;
  arq-connfail)  need cmake cmake
                 ( cd "$D" && ENCRYPT=0 ./build.sh loopback-arq-connfail >/dev/null )
                 retry_leg 60 "$t" v_arq_connfail
                 echo "OK: caller connect-failure aborted the listener and reported fast" ;;
  malloc-stress) ( cd "$D" && ./build.sh malloc-stress >/dev/null )
                 assert_pass 90 'PASS: s5_malloc_stress' "$t" ;;
  *)             echo "unknown target: $t (expected exceptions|lwip-loopback|libsrt-smoke|loopback-arq|arq-connfail|example|srt-recv|fault-smoke|malloc-stress)" >&2; exit 2 ;;
esac
echo "OK: freertos-srt $t"
