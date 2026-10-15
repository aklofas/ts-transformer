#!/usr/bin/env bash
# Negative-case self-test for the fail-closed ratchets: proves they actually
# DETECT their failure mode rather than passing on a clean tree. Hermetic:
# builds synthetic fixtures in a tmpdir, so it never depends on the real
# source tree.
#
# Gated by scripts/check/repo/ratchet-self-test.sh, which ci.yml runs on
# linux-x86_64.
#
# The whole error-mapping coverage scaffold is gone: the `rust` driver (the
# C binding's per-transport `*_error_to_code` converters were deleted for
# `tst_pipeline::binding::BindingError`), the `py` / `pyarm` driver (tst-py
# has no per-kind `make_<proto>_error` call sites or hand-written
# per-variant mapper arms left to count), and the `java` rail — at which
# point `scripts/ratchets/lib/coverage.sh` and
# `scripts/ratchets/error-mapping.tsv` had no reader at all and went too.
# What all three were guarding is now one Rust table:
# `scripts/check/rust/kind-table-coverage.sh` plus
# `scripts/check/repo/kind-equivalence.sh`, and binding-side
# `raise.rs::check_error_kinds` at `import tstrans`.
#
# The surviving cases are the C-header rail's, the blocking-slot-reader
# rail's and the duplicate-test-helper rail's.
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

# ---- header rail: tool-failure fixtures ----------------------------------
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
    fn recv(&self, py: Python<'_>) -> Vec<u8> {
        crate::util::allow_threads_parking(py, || self.inner.lock().unwrap().recv())
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
{ cat "$tmp/gl/listed.tsv"; printf '%s\t%s\t%s\n' "$GLF" Publisher::recv "fixture"; } > "$tmp/gl/stale_parking.tsv"

expect "gil locks: a .lock() taken with the GIL held is caught"         1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv"     bash "$SG"
expect "gil locks: the same functions, allowlisted, pass"               0 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/listed.tsv"    bash "$SG"
expect "gil locks: a row for one type does not excuse another's getter" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/othertype.tsv" bash "$SG"
expect "gil locks: a lock inside allow_threads is not a finding (stale row fails)" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held" SG_LOCK_ALLOWLIST="$tmp/gl/stale.tsv" bash "$SG"
expect "gil locks: a lock inside allow_threads_parking is not a finding (stale row fails)" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held" SG_LOCK_ALLOWLIST="$tmp/gl/stale_parking.tsv" bash "$SG"
expect "gil locks: an allowlist row without a reason fails"             1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held"  SG_LOCK_ALLOWLIST="$tmp/gl/noreason.tsv"  bash "$SG"
expect "gil locks: a scan that matches nothing fails closed"            1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/clean" SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv"     bash "$SG"

# ---- gil locks: every lock spelling, and a lock inside a macro body -------
# A line scanner looking only for `.lock()` misses `.read()` / `.write()` /
# `.try_lock()` / the UFCS forms, and a lock taken inside a macro_rules!
# body has no enclosing `fn` the scanner can attribute it to — it must be
# a hard failure, not a silent skip.
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

# `fn` was never reset at a closing brace, so a macro_rules! body AFTER a
# function inherited that function's name and
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
# The UFCS forms end in "(" with an
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

# ---- duplicate test helpers (scripts/check/repo/no-duplicate-test-helpers.sh) ----
# One helper body in two test files is caught (renamed copies too) unless it
# is allowlisted with a reason; a stale row, a row without a reason and an
# empty scan fail; a short body and two copies in one file are not findings.
DTH="$DIR/../check/repo/no-duplicate-test-helpers.sh"
mkdir -p "$tmp/dth/rs/a/tests" "$tmp/dth/rs/b/tests" "$tmp/dth/py/tests" "$tmp/dth/java/src/test" \
         "$tmp/dth/short/x/tests" "$tmp/dth/one/x/tests" "$tmp/dth/none/src"
for d in a b; do cat > "$tmp/dth/rs/$d/tests/t.rs" <<'XEOF'
fn drain(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}
XEOF
done
sed -i 's/^fn drain(/fn drain_all(/' "$tmp/dth/rs/b/tests/t.rs"
for d in a b; do cat > "$tmp/dth/py/tests/test_$d.py" <<'XEOF'
def idr():
    data = bytearray(b"\x00\x00\x00\x01")
    data.append(0x65)
    data.extend(b"\x88" * 8)
    return bytes(data)


def test_other():
    assert True
XEOF
done
for d in A B; do cat > "$tmp/dth/java/src/test/${d}Test.java" <<'XEOF'
class T {
    private static byte[] idr() {
        byte[] b = new byte[8];
        b[3] = 1;
        b[4] = 0x65;
        return b;
    }
}
XEOF
done
for d in a b; do printf 'fn tiny() -> u8 {\n    1\n}\n' > "$tmp/dth/short/x/tests/$d.rs"; done
{ cat "$tmp/dth/rs/a/tests/t.rs"; sed 's/^fn drain(/fn drain2(/' "$tmp/dth/rs/a/tests/t.rs"; } > "$tmp/dth/one/x/tests/t.rs"
printf '%s\t%s\t%s\n' "$tmp/dth/rs/a/tests/t.rs" drain fixture "$tmp/dth/rs/b/tests/t.rs" drain_all fixture > "$tmp/dth/listed.tsv"
{ cat "$tmp/dth/listed.tsv"; printf '%s\t%s\t%s\n' "$tmp/dth/rs/a/tests/t.rs" gone fixture; } > "$tmp/dth/stale.tsv"
printf '%s\t%s\n' "$tmp/dth/rs/a/tests/t.rs" drain > "$tmp/dth/noreason.tsv"
: > "$tmp/dth/empty.tsv"

expect "dup helpers: one Rust body in two test files is caught (renamed copy too)" 1 env DTH_ROOTS="$tmp/dth/rs"    DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: the same two copies, allowlisted, pass"                     0 env DTH_ROOTS="$tmp/dth/rs"    DTH_ALLOWLIST="$tmp/dth/listed.tsv"   bash "$DTH"
expect "dup helpers: a stale allowlist row fails"                                1 env DTH_ROOTS="$tmp/dth/rs"    DTH_ALLOWLIST="$tmp/dth/stale.tsv"    bash "$DTH"
expect "dup helpers: an allowlist row without a reason fails"                    1 env DTH_ROOTS="$tmp/dth/rs"    DTH_ALLOWLIST="$tmp/dth/noreason.tsv" bash "$DTH"
expect "dup helpers: a Python def body in two test files is caught"              1 env DTH_ROOTS="$tmp/dth/py"    DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: a Java private static body in two test files is caught"     1 env DTH_ROOTS="$tmp/dth/java"  DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: a body under the line floor is not a finding"               0 env DTH_ROOTS="$tmp/dth/short" DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: two copies inside ONE file are not a cross-file finding"     0 env DTH_ROOTS="$tmp/dth/one"   DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: a scan that finds no test file fails closed"                1 env DTH_ROOTS="$tmp/dth/none"  DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"

# ---- duplicate test helpers, scan rules and regeneration ----
mkdir -p "$tmp/dth/tf/a/tests" "$tmp/dth/tf/b/tests" "$tmp/dth/cm/a/tests" "$tmp/dth/cm/b/tests" \
         "$tmp/dth/three/a/tests" "$tmp/dth/three/b/tests" "$tmp/dth/three/c/tests" \
         "$tmp/dth/two/a/tests" "$tmp/dth/two/b/tests"
for d in a b; do cat > "$tmp/dth/tf/$d/tests/t.rs" <<'XEOF'
#[test]
fn same_body() {
    let mut out = Vec::new();
    out.push(1u8);
    out.push(2u8);
    assert_eq!(out.len(), 2);
}
XEOF
done
cp "$tmp/dth/rs/a/tests/t.rs" "$tmp/dth/cm/a/tests/t.rs"
sed 's|^    let mut out = Vec::new();|    // a note that only the copy has\n    let mut out = Vec::new();   |' "$tmp/dth/rs/a/tests/t.rs" > "$tmp/dth/cm/b/tests/t.rs"
for d in a b c; do cp "$tmp/dth/rs/a/tests/t.rs" "$tmp/dth/three/$d/tests/t.rs"; done
for d in a b; do cp "$tmp/dth/rs/a/tests/t.rs" "$tmp/dth/two/$d/tests/t.rs"; done
{ for d in a b c; do printf '%s\t%s\t%s\n' "$tmp/dth/three/$d/tests/t.rs" drain fixture; done; } > "$tmp/dth/three.tsv"
env DTH_ROOTS="$tmp/dth/rs" DTH_ALLOWLIST="$tmp/dth/listed.tsv" bash "$DTH" --print-allowlist > "$tmp/dth/printed.tsv"
env DTH_ROOTS="$tmp/dth/rs" DTH_ALLOWLIST="$tmp/dth/empty.tsv"  bash "$DTH" --print-allowlist > "$tmp/dth/printed-new.tsv"

expect "dup helpers: --print-allowlist output, fed back as the allowlist, passes"          0 env DTH_ROOTS="$tmp/dth/rs"    DTH_ALLOWLIST="$tmp/dth/printed.tsv"  bash "$DTH"
expect "dup helpers: --print-allowlist keeps the reason of existing rows"                 0 awk -F'\t' '$3 == "fixture" { n++ } END { exit n == 2 ? 0 : 1 }' "$tmp/dth/printed.tsv"
expect "dup helpers: --print-allowlist leaves new rows without a reason (the rail then fails them)" 1 env DTH_ROOTS="$tmp/dth/rs" DTH_ALLOWLIST="$tmp/dth/printed-new.tsv" bash "$DTH"
expect "dup helpers: identical #[test] bodies in two files are not helpers"               0 env DTH_ROOTS="$tmp/dth/tf"    DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: a copy that only adds a comment line is still a copy"                1 env DTH_ROOTS="$tmp/dth/cm"    DTH_ALLOWLIST="$tmp/dth/empty.tsv"    bash "$DTH"
expect "dup helpers: a three-file group, all listed, passes"                               0 env DTH_ROOTS="$tmp/dth/three" DTH_ALLOWLIST="$tmp/dth/three.tsv"    bash "$DTH"
expect "dup helpers: a three-file group with one file gone leaves a stale row"             1 env DTH_ROOTS="$tmp/dth/two"   DTH_ALLOWLIST="$tmp/dth/three.tsv"    bash "$DTH"

if [[ "$fail" == 0 ]]; then echo "self-test: ALL OK"; fi
exit "$fail"
