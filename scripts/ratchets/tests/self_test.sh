#!/usr/bin/env bash
# Negative-case self-test for the fail-closed ratchets: proves they actually
# DETECT their failure mode rather than passing on a clean tree. Hermetic:
# builds synthetic fixtures in a tmpdir, so it never depends on the real
# source tree.
#
# Gated by scripts/check/repo/ratchet-self-test.sh, which ci.yml runs on
# linux-x86_64.
#
# The whole error-mapping coverage scaffold is gone as of Arc 2, and its
# cases with it: the `rust` driver in WP-B1 (the C binding's per-transport
# `*_error_to_code` converters were deleted for
# `tst_pipeline::binding::BindingError`), the `py` / `pyarm` driver in WP-B2
# (tst-py has no per-kind `make_<proto>_error` call sites or hand-written
# per-variant mapper arms left to count), and the `java` rail in WP-B3 — at
# which point `scripts/ratchets/lib/coverage.sh` and
# `scripts/ratchets/error-mapping.tsv` had no reader at all and went too.
# What all three were guarding is now one Rust table:
# `scripts/check/rust/kind-table-coverage.sh` plus
# `scripts/check/repo/kind-equivalence.sh`, and binding-side
# `raise.rs::check_error_kinds` at `import tstrans`.
#
# The surviving cases are the C-header rail's and the blocking-slot-reader
# rail's.
set -uo pipefail
DIR="$(cd "$(dirname "$0")/.." && pwd)"          # scripts/ratchets

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail=0
expect() { # <desc> <want_rc> <cmd...>
    local desc="$1" want="$2"; shift 2
    "$@" >"$tmp/out" 2>&1; local got=$?
    if [[ "$got" == "$want" ]]; then
        echo "ok: $desc"
    else
        echo "FAIL: $desc (got rc=$got want=$want)"; sed 's/^/    /' "$tmp/out"; fail=1
    fi
}

# ---- header rail: tool-failure fixtures (deep-review-4 X-META-01 / E12) ---
# The rail must never print PASS when the generator failed, and must
# distinguish "not installed locally" (SKIP, rc 0) from "not installed in
# CI" (FAIL). Hermetic: a fixture header carries the two defines and one
# guarded typedef block; the shim never reads the real cbindgen.toml.
HDR="$DIR/../check/c/header-conditional-sections.sh"
mkdir -p "$tmp/shim"
printf '#!/bin/sh\necho "shim: cbindgen failed" >&2\nexit 1\n' > "$tmp/shim/cbindgen"
chmod +x "$tmp/shim/cbindgen"
cat > "$tmp/fixture.h" <<'EOF'
#define TST_HAS_SRT 1
#define TST_HAS_RTP 1
#if defined(TST_HAS_RTP)
typedef struct TstRtpFixture TstRtpFixture;
#endif
EOF

expect "header rail: failing cbindgen shim on PATH fails closed"  1 env PATH="$tmp/shim:$PATH" CI=1 HCS_HEADER="$tmp/fixture.h" bash "$HDR"
expect "header rail: cbindgen absent under CI fails closed"        1 env CI=1 HCS_CBINDGEN="$tmp/nonexistent-cbindgen" HCS_HEADER="$tmp/fixture.h" bash "$HDR"
expect "header rail: cbindgen absent locally is SKIP (rc 0)"       0 env CI= HCS_CBINDGEN="$tmp/nonexistent-cbindgen" HCS_HEADER="$tmp/fixture.h" bash "$HDR"

# ---- blocking slot readers (scripts/check/c/snapshot-getters.sh) -----------
# A getter that takes the slot with the blocking `with_ref()` must be caught
# unless it is allowlisted with a reason; `try_with_ref()` and a snapshot read
# must not be; a stale allowlist row and a scan that matches nothing both fail.
SG="$DIR/../check/c/snapshot-getters.sh"
mkdir -p "$tmp/sg/blocking" "$tmp/sg/clean"
cat > "$tmp/sg/blocking/shell.rs" <<'EOF'
impl Shell {
    // a comment that mentions with_ref( must not count
    fn peer_addr(&self) -> String {
        self.owned
            .with_ref(|t| t.peer().to_string())
            .unwrap_or_default()
    }
    fn is_alive(&self) -> bool {
        alive_probe(&self.owned, |s| s.is_alive())
    }
}
#[cfg(test)]
mod tests {
    fn in_a_test() { reg.with_ref(1, |v| *v); }
}
EOF
cat > "$tmp/sg/clean/shell.rs" <<'EOF'
impl Shell {
    fn peer_addr(&self) -> String { self.owned.snapshot().to_string() }
    fn is_alive(&self) -> bool {
        matches!(self.owned.try_with_ref(|s| s.is_alive()), None | Some(Ok(true)))
    }
}
EOF
printf '# none\n' > "$tmp/sg/empty.tsv"
printf '%s\t%s\t%s\t%s\n' "$tmp/sg/blocking/shell.rs" peer_addr needs-slot "fixture" > "$tmp/sg/listed.tsv"
printf '%s\t%s\t%s\t%s\n' "$tmp/sg/blocking/shell.rs" peer_addr needs-slot "fixture" \
                           "$tmp/sg/blocking/shell.rs" is_alive  needs-slot "fixture" > "$tmp/sg/stale.tsv"
printf '%s\t%s\t%s\n' "$tmp/sg/blocking/shell.rs" peer_addr needs-slot > "$tmp/sg/noreason.tsv"

expect "slot readers: a getter calling with_ref() is caught"    1 env SG_ONLY_READERS=1 SG_ROOTS="$tmp/sg/blocking" SG_ALLOWLIST="$tmp/sg/empty.tsv"    bash "$SG"
expect "slot readers: the same getter, allowlisted, passes"     0 env SG_ONLY_READERS=1 SG_ROOTS="$tmp/sg/blocking" SG_ALLOWLIST="$tmp/sg/listed.tsv"   bash "$SG"
expect "slot readers: a stale allowlist row fails"              1 env SG_ONLY_READERS=1 SG_ROOTS="$tmp/sg/blocking" SG_ALLOWLIST="$tmp/sg/stale.tsv"    bash "$SG"
expect "slot readers: an allowlist row without a reason fails"  1 env SG_ONLY_READERS=1 SG_ROOTS="$tmp/sg/blocking" SG_ALLOWLIST="$tmp/sg/noreason.tsv" bash "$SG"
expect "slot readers: a scan that matches nothing fails closed" 1 env SG_ONLY_READERS=1 SG_ROOTS="$tmp/sg/clean"    SG_ALLOWLIST="$tmp/sg/empty.tsv"    bash "$SG"

# ---- locks taken with the GIL held (same script, check 3) ------------------
# A `.lock()` outside `allow_threads(` must be caught unless its function is
# allowlisted with a reason — per TYPE, so allowlisting one class's getter
# does not excuse another's of the same name. A lock taken inside
# `allow_threads(` must not be reported, one inside a `with_gil(` nested in
# it must; a stale row, a row without a reason and an empty scan all fail.
mkdir -p "$tmp/gl/held" "$tmp/gl/clean"
cat > "$tmp/gl/held/shell.rs" <<'EOF'
impl Publisher {
    fn push(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        // the defect: .lock() here, guard held across the GIL release
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| poisoned())?;
        py.allow_threads(|| guard.push(data)).map_err(to_py)
    }
    fn local_addr(&self) -> String {
        self.inner.lock().unwrap().local_addr()
    }
    fn stats(&self, py: Python<'_>) -> Stats {
        py.allow_threads(|| self.inner.lock().unwrap().stats())
    }
    fn add_sink(&self, py: Python<'_>, cb: Py<PyAny>) {
        let errors = self.errors.clone();
        py.allow_threads(move || {
            self.inner.lock().unwrap().add_sink(Box::new(move |pkt: &[u8]| {
                Python::with_gil(|py| {
                    if let Err(e) = cb.call1(py, (pkt,)) {
                        *errors.lock().unwrap() = Some(e);
                    }
                });
            }));
        })
    }
}

impl Handle {
    fn local_addr(&self) -> String {
        self.inner.lock().unwrap().local_addr()
    }
}
#[cfg(test)]
mod tests {
    fn in_a_test() { M.lock().unwrap(); }
}
EOF
cat > "$tmp/gl/clean/shell.rs" <<'EOF'
impl Publisher {
    fn stats(&self, py: Python<'_>) -> Stats {
        py.allow_threads(|| self.inner.lock().unwrap().stats())
    }
    fn local_addr(&self) -> String { self.local_addr.to_string() }
}
EOF
GLF="$tmp/gl/held/shell.rs"
printf '# none\n' > "$tmp/gl/empty.tsv"
printf '%s\t%s\t%s\n' "$GLF" Publisher::push "fixture" "$GLF" Publisher::local_addr "fixture" \
                      "$GLF" Publisher::add_sink "fixture" "$GLF" Handle::local_addr "fixture" > "$tmp/gl/listed.tsv"
printf '%s\t%s\t%s\n' "$GLF" Publisher::push "fixture" "$GLF" Publisher::local_addr "fixture" \
                      "$GLF" Publisher::add_sink "fixture" > "$tmp/gl/othertype.tsv"
{ cat "$tmp/gl/listed.tsv"; printf '%s\t%s\t%s\n' "$GLF" Publisher::stats "fixture"; } > "$tmp/gl/stale.tsv"
{ grep -v 'Handle::' "$tmp/gl/listed.tsv"; printf '%s\t%s\n' "$GLF" Handle::local_addr; } > "$tmp/gl/noreason.tsv"

expect "gil locks: a .lock() taken with the GIL held is caught"         1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv"     bash "$SG"
expect "gil locks: the same functions, allowlisted, pass"               0 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/listed.tsv"    bash "$SG"
expect "gil locks: a row for one type does not excuse another's getter" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/othertype.tsv" bash "$SG"
expect "gil locks: a lock inside allow_threads is not a finding (stale row fails)" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held" SG_LOCK_ALLOWLIST="$tmp/gl/stale.tsv" bash "$SG"
expect "gil locks: an allowlist row without a reason fails"             1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/noreason.tsv"  bash "$SG"
expect "gil locks: a scan that matches nothing fails closed"            1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/clean" SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv"     bash "$SG"

# ---- gil locks: every lock spelling, and a lock inside a macro body -------
# review #7, external report R7-03 = internal report R7-06: a line scanner
# looking only for `.lock()` misses `.read()` / `.write()` / `.try_lock()` /
# the UFCS forms, and a lock taken inside a macro_rules! body has no
# enclosing `fn` the scanner can attribute it to — it must be a hard
# failure, not a silent skip.
mkdir -p "$tmp/gl/rwlock" "$tmp/gl/macro"
cat > "$tmp/gl/rwlock/shell.rs" <<'EOF'
impl Publisher {
    fn local_addr(&self) -> String {
        self.inner.read().unwrap().local_addr()
    }
    fn set_name(&self, n: &str) {
        self.inner.write().unwrap().name = n.into();
    }
    fn peek(&self) -> bool {
        self.inner.try_lock().is_ok()
    }
    fn ufcs(&self) -> u64 {
        Mutex::lock(&self.inner).unwrap().count
    }
}
EOF
printf '%s\t%s\t%s\n' "$tmp/gl/rwlock/shell.rs" Publisher::local_addr "fixture" \
                      "$tmp/gl/rwlock/shell.rs" Publisher::set_name  "fixture" \
                      "$tmp/gl/rwlock/shell.rs" Publisher::peek      "fixture" \
                      "$tmp/gl/rwlock/shell.rs" Publisher::ufcs      "fixture" > "$tmp/gl/rwlock-allow.tsv"
cat > "$tmp/gl/macro/shell.rs" <<'EOF'
macro_rules! held_guard {
    ($m:expr) => { $m.lock().unwrap() };
}
impl Publisher {
    fn push(&self, py: Python<'_>) {
        let guard = held_guard!(self.inner);
        py.allow_threads(|| std::thread::yield_now());
        drop(guard);
    }
}
fn allowed() {
    let guard = SAFE.lock().unwrap();
    drop(guard);
}
EOF
printf '%s\t%s\t%s\n' "$tmp/gl/macro/shell.rs" allowed 'SAFE has no holder that releases the GIL' > "$tmp/gl/macro-allow.tsv"

# review #7 PR 271 reviewer finding: `fn` was never reset at a closing brace,
# so a macro_rules! body AFTER a function inherited that function's name and
# its allowlist row silently excused it. Here `allowed` comes FIRST (its own
# closing `}` must clear `fn`) and the macro comes after; with only `allowed`
# allowlisted, the macro's lock must still be a hard <unattributed:...>
# failure, not silently excused by `allowed`'s row.
mkdir -p "$tmp/gl/macro-after-fn"
cat > "$tmp/gl/macro-after-fn/shell.rs" <<'EOF'
fn allowed() {
    let guard = SAFE.lock().unwrap();
    drop(guard);
}
macro_rules! held_guard {
    ($m:expr) => { $m.lock().unwrap() };
}
EOF
printf '%s\t%s\t%s\n' "$tmp/gl/macro-after-fn/shell.rs" allowed 'SAFE has no holder that releases the GIL' > "$tmp/gl/macro-after-fn-allow.tsv"

expect "gil locks: .read()/.write()/.try_lock()/UFCS spellings are caught" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/rwlock" SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv" bash "$SG"
expect "gil locks: allowlisting all four rwlock/UFCS functions passes"    0 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/rwlock" SG_LOCK_ALLOWLIST="$tmp/gl/rwlock-allow.tsv" bash "$SG"
expect "gil locks: a lock inside a macro body is refused, not skipped"    1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/macro"  SG_LOCK_ALLOWLIST="$tmp/gl/macro-allow.tsv" bash "$SG"
expect "gil locks: a macro body AFTER an allowlisted fn is not excused by that fn's row" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/macro-after-fn" SG_LOCK_ALLOWLIST="$tmp/gl/macro-after-fn-allow.tsv" bash "$SG"

# ---- gil locks: a UFCS spelling's trailing "(" must stay paren-balanced ---
# review #7 PR 271 Copilot finding: the UFCS forms end in "(" with an
# argument following, not a balanced "()" like the dotted spellings. Skipping
# that "(" without counting it under-closes `at`/`wg` when its matching ")"
# is later seen, so a second lock later in the SAME allow_threads block was
# misclassified as GIL-held. `inside` (two UFCS/dotted locks back to back,
# both inside allow_threads) must never be flagged; `outside` (a bare UFCS
# lock with the GIL held) must.
mkdir -p "$tmp/gl/ufcs-paren"
cat > "$tmp/gl/ufcs-paren/shell.rs" <<'EOF'
impl Publisher {
    fn inside(&self, py: Python<'_>) -> u64 {
        py.allow_threads(|| {
            let a = Mutex::lock(&self.x).unwrap().count;
            let b = self.y.lock().unwrap().count;
            a + b
        })
    }
    fn outside(&self) -> u64 {
        Mutex::lock(&self.x).unwrap().count
    }
}
EOF
printf '%s\t%s\t%s\n' "$tmp/gl/ufcs-paren/shell.rs" Publisher::outside "fixture" > "$tmp/gl/ufcs-paren-allow.tsv"

expect "gil locks: a UFCS lock immediately followed by another, both inside allow_threads, is never a finding (only the outside one is)" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/ufcs-paren" SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv" bash "$SG"
expect "gil locks: allowlisting only the outside UFCS lock passes"                                                                      0 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/ufcs-paren" SG_LOCK_ALLOWLIST="$tmp/gl/ufcs-paren-allow.tsv" bash "$SG"

if [[ "$fail" == 0 ]]; then echo "self-test: ALL OK"; fi
exit "$fail"
