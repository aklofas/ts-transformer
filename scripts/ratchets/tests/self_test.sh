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
# the exhaustive `kind()` of each tst-core error and the wildcard-free maps
# in `kind.rs` (the compiler), `scripts/check/repo/kind-equivalence.sh`, and
# binding-side `raise.rs::check_error_kinds` at `import tstrans`.
#
# The surviving cases are the C-header rail's and the duplicate-test-helper
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

# plant <file> <from> <to> : replace every <from> (a sed BRE, no `|`) with <to>
# in <file>. Portable: no `sed -i`.
plant() { sed "s|$2|$3|g" "$1" > "$1.new" && mv "$1.new" "$1"; }

SG="$DIR/../check/c/snapshot-getters.sh"

# ---- locks taken with the GIL held (same script, check 2) ------------------
# A `.lock()` outside `detach(` must be caught unless its function is
# allowlisted with a reason — per TYPE, so allowlisting one class's getter
# does not excuse another's of the same name. A lock taken inside
# `detach(` must not be reported, one inside an `attach(` nested in
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
        py.detach(|| guard.push(data)).map_err(to_py)
    }
    fn local_addr(&self) -> String {
        self.inner.lock().unwrap().local_addr()
    }
    fn stats(&self, py: Python<'_>) -> Stats {
        py.detach(|| self.inner.lock().unwrap().stats())
    }
    fn recv(&self, py: Python<'_>) -> Vec<u8> {
        crate::util::allow_threads_parking(py, || self.inner.lock().unwrap().recv())
    }
    fn add_sink(&self, py: Python<'_>, cb: Py<PyAny>) {
        let errors = self.errors.clone();
        py.detach(move || {
            self.inner.lock().unwrap().add_sink(Box::new(move |pkt: &[u8]| {
                Python::attach(|py| {
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
        py.detach(|| self.inner.lock().unwrap().stats())
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
expect "gil locks: a lock inside detach is not a finding (stale row fails)" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/held" SG_LOCK_ALLOWLIST="$tmp/gl/stale.tsv" bash "$SG"
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
        py.detach(|| std::thread::yield_now());
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
# is later seen, so a second lock later in the SAME detach block was
# misclassified as GIL-held. `inside` (two UFCS/dotted locks back to back,
# both inside detach) must never be flagged; `outside` (a bare UFCS
# lock with the GIL held) must.
mkdir -p "$tmp/gl/ufcs-paren"
cat > "$tmp/gl/ufcs-paren/shell.rs" <<'EOF'
impl Publisher {
    fn inside(&self, py: Python<'_>) -> u64 {
        py.detach(|| {
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

expect "gil locks: a UFCS lock immediately followed by another, both inside detach, is never a finding (only the outside one is)" 1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/ufcs-paren" SG_LOCK_ALLOWLIST="$tmp/gl/empty.tsv" bash "$SG"
expect "gil locks: allowlisting only the outside UFCS lock passes"                                                                      0 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/ufcs-paren" SG_LOCK_ALLOWLIST="$tmp/gl/ufcs-paren-allow.tsv" bash "$SG"

# ---- gil locks: detach( / attach( match only as whole names -------------
# `py.detach(` and `Python::attach(` are the GIL release and re-acquire;
# `foo_detach(` is not a release (its lock is GIL-held, a finding) and
# `reattach(` inside a detach block is not a re-acquire (its lock stays
# released, not a finding). The exact pair below passes; dropping
# fake_release from it fails, and adding reattach_inside to it fails stale.
mkdir -p "$tmp/gl/anchor"
cat > "$tmp/gl/anchor/shell.rs" <<'EOF'
impl Publisher {
    fn fake_release(&self) -> u64 {
        self.foo_detach(|| self.x.lock().unwrap().count)
    }
    fn reattach_inside(&self, py: Python<'_>) -> u64 {
        py.detach(|| self.reattach(|| self.x.lock().unwrap().count))
    }
    fn real_attach(&self, py: Python<'_>) {
        py.detach(|| Python::attach(|_py| { self.x.lock().unwrap(); }))
    }
}
EOF
GLA="$tmp/gl/anchor/shell.rs"
printf '%s\t%s\t%s\n' "$GLA" Publisher::fake_release "fixture" "$GLA" Publisher::real_attach "fixture" > "$tmp/gl/anchor-allow.tsv"
printf '%s\t%s\t%s\n' "$GLA" Publisher::real_attach "fixture" > "$tmp/gl/anchor-nofake.tsv"
{ cat "$tmp/gl/anchor-allow.tsv"; printf '%s\t%s\t%s\n' "$GLA" Publisher::reattach_inside "fixture"; } > "$tmp/gl/anchor-stale.tsv"

expect "gil locks: py.detach( and Python::attach( still match as whole names" 0 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/anchor" SG_LOCK_ALLOWLIST="$tmp/gl/anchor-allow.tsv"  bash "$SG"
expect "gil locks: foo_detach( is not a GIL release"                         1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/anchor" SG_LOCK_ALLOWLIST="$tmp/gl/anchor-nofake.tsv" bash "$SG"
expect "gil locks: reattach( is not a GIL re-acquire (stale row fails)"      1 env SG_ONLY_LOCKS=1 SG_LOCK_ROOTS="$tmp/gl/anchor" SG_LOCK_ALLOWLIST="$tmp/gl/anchor-stale.tsv"  bash "$SG"

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

mkdir -p "$tmp/dth/tr/a/tests" "$tmp/dth/tr/b/tests"
cat > "$tmp/dth/tr/a/tests/t.rs" <<'XEOF'
trait Clock {
    fn now(
        &self,
        unit: u8,
    ) -> u8;
}

XEOF
cat "$tmp/dth/rs/a/tests/t.rs" >> "$tmp/dth/tr/a/tests/t.rs"
cp "$tmp/dth/rs/a/tests/t.rs" "$tmp/dth/tr/b/tests/t.rs"
expect "dup helpers: a bodiless multi-line trait declaration does not hide a duplicate after it" 1 env DTH_ROOTS="$tmp/dth/tr" DTH_ALLOWLIST="$tmp/dth/empty.tsv" bash "$DTH"

mkdir -p "$tmp/dth/gen/a/tests" "$tmp/dth/gen/b/tests"
for d in a b; do sed 's/^fn drain(mux: &mut Muxer)/fn drain<R, T>(mux: \&mut Muxer)/' "$tmp/dth/rs/a/tests/t.rs" > "$tmp/dth/gen/$d/tests/t.rs"; done
printf '%s\t%s\t%s\n' "$tmp/dth/gen/a/tests/t.rs" drain fixture "$tmp/dth/gen/b/tests/t.rs" drain fixture > "$tmp/dth/gen.tsv"
expect "dup helpers: a generic helper is listed by its identifier, not its type parameters" 0 env DTH_ROOTS="$tmp/dth/gen" DTH_ALLOWLIST="$tmp/dth/gen.tsv" bash "$DTH"

# ---- doc ABI currency (scripts/check/repo/doc-abi-and-st1910-currency.sh) ----
DAC="$DIR/../check/repo/doc-abi-and-st1910-currency.sh"
d="$tmp/dac"; mkdir -p "$d/bindings/c/core/src" "$d/docs/languages" "$d/docs/reference" "$d/crates"
printf 'pub const TST_ABI_VERSION_MINOR: c_int = 21;\n' > "$d/bindings/c/core/src/lib.rs"
printf '[workspace.package]\nversion = "0.6.0"\n' > "$d/Cargo.toml"
printf '# x\n\nC ABI **0.21** today.\n' > "$d/README.md"
printf '#define TST_ABI_VERSION_MINOR 21\n#define TST_VERSION_MAJOR 0\n#define TST_VERSION_MINOR 6\n#define TST_VERSION_PATCH 0\n' > "$d/docs/languages/c.md"
printf 'ABI minor is **21** today.\n' > "$d/docs/reference/binding-authors.md"
dac() { DOC_ABI_ROOT="$d" bash "$DAC"; }
printf '| `tst-c` | ABI version **0.20** (additive). |\n' > "$d/docs/reference/compatibility.md"
expect "doc ABI: planted stale bold minor fails"  1 dac
printf '| `tst-c` | ABI version 0.20 (additive). |\n' > "$d/docs/reference/compatibility.md"
expect "doc ABI: planted stale plain minor fails" 1 dac
printf '| `tst-c` | ABI version **0.21** (additive). |\n' > "$d/docs/reference/compatibility.md"
expect "doc ABI: current minor passes"            0 dac

# ---- scenario manifest (scripts/check/repo/scenario-manifest.sh) ----
R_SM="$DIR/../check/repo/scenario-manifest.sh"
mkdir -p "$tmp/sm/s"
: > "$tmp/sm/s/in.ts"
printf '{}' > "$tmp/sm/s/g.json"
sm() { SCENARIO_MANIFEST="$tmp/sm/scenarios.toml" bash "$R_SM"; }
cat > "$tmp/sm/scenarios.toml" <<'TOML'
[[scenario]]
id = "a"
kind = "demux"
input = "s/in.ts"
golden = "s/g.json"
features = []
tier = "A"
schema_version = 0
TOML
expect "scenario manifest: valid single-scenario manifest" 0 sm
cat > "$tmp/sm/scenarios.toml" <<'TOML'
[[scenario]]
id = "a"
kind = "demux"
input = "s/in.ts"
golden = "s/g.json"
features = []
tier = "A"
schema_version = 0

[[scenario]]
id = "a"
kind = "roundtrip"
input = "s/in.ts"
golden = "s/g.json"
features = []
tier = "A"
schema_version = 0
TOML
expect "scenario manifest: duplicate id" 1 sm
cat > "$tmp/sm/scenarios.toml" <<'TOML'
[[scenario]]
id = "b"
kind = "badkind"
input = "s/in.ts"
golden = "s/g.json"
features = []
tier = "A"
schema_version = 0
TOML
expect "scenario manifest: unknown kind" 1 sm
cat > "$tmp/sm/scenarios.toml" <<'TOML'
[[scenario]]
id = "c"
kind = "demux"
input = "s/in.ts"
golden = "s/g.json"
features = ["srt", "quic"]
tier = "A"
schema_version = 0
TOML
expect "scenario manifest: unknown feature token" 1 sm
cat > "$tmp/sm/scenarios.toml" <<'TOML'
[[scenario]]
id = "d"
kind = "demux"
input = "s/no-such-input.ts"
golden = "s/g.json"
features = []
tier = "A"
schema_version = 0
TOML
expect "scenario manifest: missing input file" 1 sm
cat > "$tmp/sm/scenarios.toml" <<'TOML'
[[scenario]]
id = "e"
kind = "demux"
input = "s/in.ts"
golden = "s/no-such-golden.json"
features = []
tier = "A"
schema_version = 0
TOML
expect "scenario manifest: missing golden file" 1 sm
# No ids must fail cleanly, not abort the script under `set -euo pipefail`
# when the id grep finds no matches.
printf '# manifest with no scenarios\n' > "$tmp/sm/scenarios.toml"
expect "scenario manifest: manifest with no scenario ids" 1 sm

# ---- surface manifest (scripts/check/repo/surface-manifest.sh) ----
R_SMF="$DIR/../check/repo/surface-manifest.sh"
mkdir -p "$tmp/smf/crates/demo" "$tmp/smf/py" "$tmp/smf/py-empty"
printf 'pub fn demo::a()\npub struct demo::B\n' > "$tmp/smf/crates/demo/public-api.txt"
: > "$tmp/smf/h.h"   # empty C header
# smf_* shell variables are the per-case knobs; the defaults pin an empty
# un-catalogued set (sha256 of no lines).
smf_pyi="$tmp/smf"; smf_count=0; smf_umax="999 999 999"
smf_sha=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
smf() {
    SURFACE_BASELINE_DIR="$tmp/smf/crates" SURFACE_CRATES=demo SURFACE_MANIFEST="$tmp/smf/m.toml" \
    SURFACE_C_HEADER="$tmp/smf/h.h" SURFACE_PYI_DIR="$smf_pyi" SURFACE_BUILT_FEATURES=srt \
    SURFACE_UNAUDITED_COUNT="$smf_count" SURFACE_UNAUDITED_SHA256="$smf_sha" \
    SURFACE_UNAUDITED_CELLS_MAX="$smf_umax" bash "$R_SMF"
}
smf_req() { SURFACE_REQUIRED_PREFIXES="c python" smf; }
smf_row() { # <item> <python cell> <exempt item>
    printf '[[surface]]\nitem = "%s"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:%s", "java:deferred", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "%s"\n' \
        "$1" "$2" "$3" > "$tmp/smf/m.toml"
}
printf '[[exempt]]\nitem = "demo::a"\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: all exempted" 0 smf
printf '[[exempt]]\nitem = "demo::a"\n' > "$tmp/smf/m.toml"
expect "surface manifest: un-catalogued item" 1 smf
# The manifest must parse as TOML: an unterminated string fails even when
# every item is named.
printf '[[exempt]]\nitem = "demo::a"\nreason = "unterminated\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: an unterminated string is not valid TOML" 1 smf
printf '[[exempt]]\nitem = "demo::a"\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
# Five-column sentinels so the expected failure can only come from rule (a).
printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/NO_SUCH_FILE.rs"]\nbindings = ["c:deferred", "python:deferred", "java:deferred", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: missing owning test" 1 smf
# A real repo-relative path that exists (tests/coverage/README.md).
printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:nope_sym_xyz", "python:deferred", "java:deferred", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: unresolved c symbol" 1 smf
printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:deferred", "java:deferred", "swift:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: row missing the kotlin column" 1 smf
printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:n/a", "java:unaudited", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: all five columns present as sentinels (:deferred / :n/a / :unaudited)" 0 smf
# SURFACE_REQUIRED_PREFIXES really drives rule (b2): the same two-column row
# that fails under the default set passes when the set is narrowed.
printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:deferred", "python:deferred"]\n[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: a two-column row under the default prefix set" 1 smf
expect "surface manifest: the same row under SURFACE_REQUIRED_PREFIXES='c python'" 0 smf_req
# python: cells resolve by definition. A fake package with one class defined
# in two modules, one module-level function and one unique class.
printf 'class Transport:\n    def send(self, payload: bytes) -> None: ...\n\ndef decode_thing(buf: bytes) -> int: ...\n' > "$tmp/smf/py/udp.pyi"
printf 'class Transport:\n    def send(self, payload: bytes) -> None: ...\n' > "$tmp/smf/py/tcp.pyi"
printf 'class B:\n    count: int\n    def feed(self, buf: bytes) -> None: ...\n\nclass BEvent: ...\n' > "$tmp/smf/py/mpegts.pyi"
smf_pyi="$tmp/smf/py"
smf_row "demo::a" "decode_thing" "demo::B"
expect "surface manifest: python: module-level function, unqualified" 0 smf
smf_row "demo::a" "send" "demo::B"
expect "surface manifest: python: bare method name (the old leaf match resolved it)" 1 smf
smf_row "demo::a" "Transport.send" "demo::B"
expect "surface manifest: python: Class.member defined in two modules, no module given" 1 smf
smf_row "demo::a" "tstrans.udp.Transport.send" "demo::B"
expect "surface manifest: python: module-qualified Class.member" 0 smf
smf_row "demo::a" "tstrans.tcp.decode_thing" "demo::B"
expect "surface manifest: python: defined, but not in the module the cell names" 1 smf
smf_row "demo::a" "B.nope" "demo::B"
expect "surface manifest: python: class exists, member does not" 1 smf
smf_row "demo::a" "B.count" "demo::B"
expect "surface manifest: python: class attribute" 0 smf
smf_row "demo::a" "decode_thing [feature=rist]" "demo::B"
expect "surface manifest: python: unbuilt feature is skipped" 0 smf
smf_row "demo::a" "send [feature=srt]" "demo::B"
expect "surface manifest: python: built feature is resolved" 1 smf
# Owner rule: demo::B::x is a member of B, and B has a Python class.
printf 'pub fn demo::a()\npub fn demo::B::x()\n' > "$tmp/smf/crates/demo/public-api.txt"
smf_row "demo::B::x" "decode_thing" "demo::a"
expect "surface manifest: python: a real function that is not the owner class's member" 1 smf
smf_row "demo::B::x" "BEvent" "demo::a"
expect "surface manifest: python: a real class that is not the owner class" 1 smf
smf_row "demo::B::x" "B.feed" "demo::a"
expect "surface manifest: python: the owner class's member" 0 smf
smf_row "demo::B::x" "B" "demo::a"
expect "surface manifest: python: the owner class itself" 0 smf
# An EMPTY definition index resolves nothing: a package directory with no
# .pyi/.py in it (a moved tree, a wrong SURFACE_PYI_DIR) must fail every
# python: cell that names a symbol; only sentinel cells survive it.
smf_pyi="$tmp/smf/py-empty"
smf_row "demo::a" "no_such_function" "demo::B::x"
expect "surface manifest: python: a named symbol against an empty definition index" 1 smf
smf_row "demo::a" "deferred" "demo::B::x"
expect "surface manifest: python: sentinel cells against an empty definition index" 0 smf
# Rule (c) pins the un-catalogued SET, not only its size: the pinned set
# passes, and the same count with a different item (one gone, one new) fails
# on the digest.
printf 'pub fn demo::a()\npub struct demo::B\n' > "$tmp/smf/crates/demo/public-api.txt"
smf_count=1; smf_sha=4b00f8db406822b96eeba4fc4cff6c67f0df4cc48602125672ed68077e174a47
printf '[[exempt]]\nitem = "demo::a"\n' > "$tmp/smf/m.toml"
expect "surface manifest: closure: the pinned un-catalogued set {demo::B}" 0 smf
printf '[[exempt]]\nitem = "demo::B"\n' > "$tmp/smf/m.toml"
expect "surface manifest: closure: same count, different item" 1 smf
smf_count=0; smf_sha=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
# An item key followed by an inline comment still names its item.
printf '[[exempt]]\nitem = "demo::a" # kept\n[[exempt]]\nitem = "demo::B" # kept\n' > "$tmp/smf/m.toml"
expect "surface manifest: closure: item keys with trailing inline comments" 0 smf

# `unaudited` is frozen per column: SURFACE_UNAUDITED_CELLS_MAX is "c python
# java", and a column whose count of :unaudited cells rises above its maximum fails.
smf_row_cells() { # <c cell> <python cell> <java cell>
    printf '[[surface]]\nitem = "demo::a"\nowning_tests = ["tests/coverage/README.md"]\nbindings = ["c:%s", "python:%s", "java:%s", "swift:deferred", "kotlin:deferred"]\n[[exempt]]\nitem = "demo::B"\n' \
        "$1" "$2" "$3" > "$tmp/smf/m.toml"
}
smf_pyi="$tmp/smf"
smf_row_cells deferred deferred unaudited
smf_umax="0 0 0"
expect "surface manifest: a java :unaudited cell above its column maximum" 1 smf
smf_umax="0 0 1"
expect "surface manifest: a java :unaudited cell at its column maximum" 0 smf
smf_umax="999 999 999"
# A reason can follow the sentinel: deferred(reason) and n/a(reason) resolve.
smf_row_cells "deferred(the C ABI has no twin yet)" "n/a(Python hands the numbers over as a dict)" "deferred(java record is hand-written)"
expect "surface manifest: sentinel cells that carry a reason" 0 smf
# A derive leaf without a row passes through an [[exempt_rule]]; one that
# no rule matches still counts as un-catalogued.
printf 'pub fn demo::a()\npub fn demo::B::clone(&self) -> demo::B\npub fn demo::B::frob(&self)\n' > "$tmp/smf/crates/demo/public-api.txt"
printf '[[exempt_rule]]\nitem_regex = %s\nreason = "derived trait method"\n[[exempt]]\nitem = "demo::a"\n' "'::(clone|eq|fmt)\$'" > "$tmp/smf/m.toml"
smf_count=1; smf_sha=$(printf 'demo::B::frob\n' | sha256sum | cut -d' ' -f1)
expect "surface manifest: a derive leaf without a row passes through the rule" 0 smf
printf '[[exempt]]\nitem = "demo::a"\n' > "$tmp/smf/m.toml"
expect "surface manifest: without the rule the derive leaf is un-catalogued" 1 smf
smf_count=0; smf_sha=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
printf 'pub fn demo::a()\npub struct demo::B\n' > "$tmp/smf/crates/demo/public-api.txt"

# ---- platform capabilities (scripts/check/repo/platform-capabilities.sh) ----
R_PC="$DIR/../check/repo/platform-capabilities.sh"
mkdir -p "$tmp/pc/crates/demo/tests"
printf '#![cfg(not(target_os = "windows"))]\n#[test]\nfn a(){}\n' > "$tmp/pc/crates/demo/tests/g.rs"
# All three overrides point at the temp tree so the enumerator strips $tmp/pc
# as the root prefix and every file reference resolves inside it.
pc() { PLATCAP_SCAN_DIRS="$tmp/pc/crates" PLATCAP_FILE="$tmp/pc/m.toml" PLATCAP_ROOT="$tmp/pc" bash "$R_PC"; }
printf '[[platform_skip]]\nfile = "crates/demo/tests/g.rs"\nplatform = "windows-msvc"\ncapability = "x"\nclass = "gap"\n' > "$tmp/pc/m.toml"
expect "platform capabilities: synced gate" 0 pc
: > "$tmp/pc/m.toml"
expect "platform capabilities: unlisted gate" 1 pc
printf '[[platform_skip]]\nfile = "crates/demo/tests/ghost.rs"\nplatform = "windows-msvc"\ncapability = "x"\nclass = "gap"\n' > "$tmp/pc/m.toml"
expect "platform capabilities: stale entry" 1 pc
printf '[[platform_skip]]\nfile = "crates/demo/tests/g.rs"\nplatform = "windows-msvc"\ncapability = "x"\nclass = "blocked_bug"\nexpires_after = "2000-01-01"\n' > "$tmp/pc/m.toml"
expect "platform capabilities: expired blocked_bug" 1 pc
printf '[[platform_skip]]\nfile = "crates/demo/tests/g.rs"\nplatform = "windows-msvc"\ncapability = "x"\nclass = "gap"\n[[capability]]\nplatform = "macos-arm64"\nstatus = "A-soft"\nsoft_until = "2000-01-01"\n' > "$tmp/pc/m.toml"
expect "platform capabilities: lapsed A-soft" 1 pc

# ---- skip ledger (scripts/check/repo/skip-ledger-sync.sh) ----
R_SL="$DIR/../check/repo/skip-ledger-sync.sh"
mkdir -p "$tmp/sl/crates/demo/tests"
printf '#[test]\n#[ignore = "x"]\nfn alpha() {}\n' > "$tmp/sl/crates/demo/tests/a.rs"
sl() { SKIP_SCAN_DIRS="$tmp/sl/crates" SKIP_LEDGER_FILE="$tmp/sl/ledger.toml" bash "$R_SL"; }
printf '[[skip]]\ntest = "alpha"\nclass = "diagnostic"\n' > "$tmp/sl/ledger.toml"
expect "skip ledger: synced ledger" 0 sl
: > "$tmp/sl/ledger.toml"
expect "skip ledger: unledgered #[ignore]" 1 sl
printf '[[skip]]\ntest = "alpha"\nclass = "diagnostic"\n[[skip]]\ntest = "ghost"\nclass = "diagnostic"\n' > "$tmp/sl/ledger.toml"
expect "skip ledger: stale ledger entry" 1 sl
printf '[[skip]]\ntest = "alpha"\nclass = "diagnostic"\n[[skip]]\ntest = "ghost"\nclass = "blocked_bug"\nresolved = "2026-01-01"\n' > "$tmp/sl/ledger.toml"
expect "skip ledger: resolved history entry" 0 sl
printf '[[skip]]\ntest = "alpha"\nclass = "placeholder"\nexpires_after = "2000-01-01"\n' > "$tmp/sl/ledger.toml"
expect "skip ledger: expired placeholder" 1 sl

# ---- release version consistency (scripts/check/repo/release-version-consistency.sh) ----
R_RVC="$DIR/../check/repo/release-version-consistency.sh"
mkdir -p "$tmp/rvc"
rvc_fixtures() { # <version>
    local ver="$1" maj rest min pat
    maj="${ver%%.*}"; rest="${ver#*.}"; min="${rest%%.*}"; pat="${rest#*.}"
    printf '[workspace.package]\nversion      = "%s"\n' "$ver" > "$tmp/rvc/Cargo.toml"
    printf '[project]\nname    = "tstrans"\nversion = "%s"\n' "$ver" > "$tmp/rvc/pyproject.toml"
    printf '[package]\nname    = "tst-py"\nversion = "%s"\n' "$ver" > "$tmp/rvc/py-cargo.toml"
    # An ABI line proves the TST_VERSION_ anchor skips TST_ABI_VERSION_.
    {
        printf 'pub const TST_VERSION_MAJOR: crate::c_types::c_int = %s;\n' "$maj"
        printf 'pub const TST_VERSION_MINOR: crate::c_types::c_int = %s;\n' "$min"
        printf 'pub const TST_VERSION_PATCH: crate::c_types::c_int = %s;\n' "$pat"
        printf 'pub const TST_ABI_VERSION_MINOR: crate::c_types::c_int = 13;\n'
    } > "$tmp/rvc/lib.rs"
    {
        printf '#define TST_ABI_VERSION_MINOR 13\n'
        printf '#define TST_VERSION_MAJOR %s\n' "$maj"
        printf '#define TST_VERSION_MINOR %s\n' "$min"
        printf '#define TST_VERSION_PATCH %s\n' "$pat"
    } > "$tmp/rvc/tstrans.h"
    printf '    assert tstrans.__version__ == "%s"\n' "$ver" > "$tmp/rvc/test_version.py"
    # A single-line inline-table internal path-dependency, mirroring the real
    # `crates/*/Cargo.toml` shape the dep-version check scans.
    printf '[dependencies]\ntst-core = { path = "../tst-core", version = "%s" }\n' "$ver" > "$tmp/rvc/dep-crate.toml"
}
rvc() {
    RVC_CARGO_TOML="$tmp/rvc/Cargo.toml" RVC_PYPROJECT="$tmp/rvc/pyproject.toml" \
    RVC_PY_CARGO="$tmp/rvc/py-cargo.toml" RVC_C_LIB="$tmp/rvc/lib.rs" \
    RVC_C_HEADER="$tmp/rvc/tstrans.h" RVC_PY_TEST="$tmp/rvc/test_version.py" \
    RVC_DEP_TOMLS="$tmp/rvc/dep-crate.toml" GITHUB_REF_NAME="" bash "$R_RVC" "$@"
}
rvc_fixtures "0.2.0"
expect "release version: all sources consistent" 0 rvc
printf '    assert tstrans.__version__ == "0.1.0"\n' > "$tmp/rvc/test_version.py"
expect "release version: one source flipped to 0.1.0" 1 rvc
rvc_fixtures "0.2.0"
expect "release version: expected arg 0.2.0 matches" 0 rvc 0.2.0
expect "release version: expected arg 9.9.9 mismatches" 1 rvc 9.9.9
rvc_fixtures "0.2.0"
printf '[dependencies]\ntst-core = { path = "../tst-core", version = "0.1.0" }\n' > "$tmp/rvc/dep-crate.toml"
expect "release version: internal dep version key flipped to 0.1.0" 1 rvc
rvc_fixtures "0.2.0"
expect "release version: internal dep version key restored" 0 rvc

# ---- roster (scripts/lib/roster.sh) + roster-sync (scripts/check/repo/roster-sync.sh) ----
ROSTER="$DIR/../lib/roster.sh"; RSYNC="$DIR/../check/repo/roster-sync.sh"
mkdir -p "$tmp/ro"
printf '{"workspace_root":"%s","workspace_members":[],"packages":[]}\n' "$tmp/ro" > "$tmp/ro/empty.json"
expect "roster: metadata with no members fails closed" 1 env ROSTER_ROOT="$tmp/ro" ROSTER_METADATA="$tmp/ro/empty.json" bash "$ROSTER" members
expect "roster: an unknown list name fails"             1 bash "$ROSTER" no-such-list
REPO="$(cd "$DIR/../.." && pwd)"
# One cargo metadata call, shared by every roster-sync row below.
(cd "$REPO" && cargo metadata --no-deps --format-version 1) > "$tmp/ro/meta.json"
rs_copy() { # a copy of the real files roster-sync reads, for planting
    rm -rf "$tmp/rs"; mkdir -p "$tmp/rs/.github/workflows" "$tmp/rs/scripts/check/repo"
    cp "$REPO/.github/workflows/ci.yml" "$REPO/.github/workflows/sanitizers.yml" "$REPO/.github/workflows/crates-io.yml" "$tmp/rs/.github/workflows/"
    cp "$REPO/scripts/check/repo/release-version-consistency.sh" "$tmp/rs/scripts/check/repo/"
}
rsync_() { ROSTER_METADATA="$tmp/ro/meta.json" RSYNC_FILES_ROOT="$tmp/rs" bash "$RSYNC"; }
rs_copy;                                                                         expect "roster-sync: the real files agree"                 0 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/sanitizers.yml" ' -p tst-hls' '';      expect "roster-sync: a crate dropped from a sanitizer job" 1 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/crates-io.yml" ' tst-rist$' '';        expect "roster-sync: a crate dropped from the publish layers" 1 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/crates-io.yml" 'publish-crates-layer.sh tst-core tst-pipeline' 'publish-crates-layer.sh tst-core'; plant "$tmp/rs/.github/workflows/crates-io.yml" 'tst-srt tst-rist$' 'tst-srt tst-rist tst-pipeline'
                                                                                 expect "roster-sync: a crate published before its dependency" 1 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/ci.yml" 'name: macos-arm64' 'name: macos-arm64-gone'; expect "roster-sync: a gating build leg renamed away" 1 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/ci.yml" 'continue: false' 'continue: true';          expect "roster-sync: a build leg made non-gating" 1 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/ci.yml" 'continue-on-error: \${{ matrix.continue }}' 'continue-on-error: true'; expect "roster-sync: the build job made continue-on-error: true" 1 rsync_
rs_copy; plant "$tmp/rs/.github/workflows/ci.yml" 'name: windows-msvc-features' 'name: windows-msvc-features-gone'; expect "roster-sync: the windows features leg renamed away" 1 rsync_
rs_copy; plant "$tmp/rs/scripts/check/repo/release-version-consistency.sh" ' \$ROOT/crates/tst-udp/Cargo.toml' ''; expect "roster-sync: a manifest dropped from the version rail" 1 rsync_

# ---- network test binaries (scripts/check/repo/net-test-binaries.sh) ----
NTB="$DIR/../check/repo/net-test-binaries.sh"
mkdir -p "$tmp/ntb/tests/loopy"
printf 'fn t() { let _ = std::net::UdpSocket::bind("127.0.0.1:0"); }\n' > "$tmp/ntb/tests/loopy/a.rs"
printf '#[path = "loopy/a.rs"] mod a;\n' > "$tmp/ntb/tests/loopy.rs"
printf 'fn offline() {}\n' > "$tmp/ntb/tests/offline.rs"
cp "$tmp/ntb/tests/loopy.rs" "$tmp/ntb/tests/net_loopy.rs"
ntb_meta() { # <test target name>... : a metadata file listing those targets
    local t first=1
    { printf '{"workspace_root":"%s","workspace_members":["x"],"packages":[{"id":"x","name":"x","publish":[],"manifest_path":"%s/Cargo.toml","dependencies":[],"targets":[' "$tmp/ntb" "$tmp/ntb"
      for t in "$@"; do [ $first = 1 ] || printf ','; first=0; printf '{"name":"%s","kind":["test"],"src_path":"%s/tests/%s.rs"}' "$t" "$tmp/ntb" "$t"; done
      printf ']}]}\n'; } > "$tmp/ntb/meta.json"
}
ntb() { ROSTER_METADATA="$tmp/ntb/meta.json" NTB_NEXTEST="$tmp/ntb/nextest.toml" bash "$NTB"; }
printf 'filter = "binary(/^net_/)"\nfilter = "binary(/^net_/)"\nfilter = "binary(/^net_/)"\n' > "$tmp/ntb/nextest.toml"
ntb_meta net_loopy offline; expect "net binaries: socket binary named net_* passes"       0 ntb
ntb_meta loopy offline;     expect "net binaries: socket binary not named net_* fails"    1 ntb
ntb_meta;                   expect "net binaries: no test targets fails closed"           1 ntb
printf 'fn offline() {}\n' > "$tmp/ntb/tests/net_offline.rs"
ntb_meta net_offline;       expect "net binaries: a net_ binary with no socket fails"     1 ntb
ntb_meta net_loopy; printf 'filter = "binary(/^net_/)"\n' > "$tmp/ntb/nextest.toml"
                            expect "net binaries: a nextest override without the regex fails" 1 ntb

# ---- MSRV consistency (scripts/check/repo/msrv-consistency.sh) ----
MSRV="$DIR/../check/repo/msrv-consistency.sh"; v=9.99
ms_copy() {
    rm -rf "$tmp/ms"; mkdir -p "$tmp/ms/docs/languages" "$tmp/ms/docs/reference" "$tmp/ms/docs/start" "$tmp/ms/.github/workflows" "$tmp/ms/embedded/scripts/check" "$tmp/ms/scripts/interop" "$tmp/ms/crates/foo"
    for p in rust-toolchain.toml Cargo.toml CONTRIBUTING.md README.md docs/languages/rust.md docs/reference/compatibility.md docs/start/quickstart.md scripts/interop/README.md .github/workflows/ci.yml; do cp "$REPO/$p" "$tmp/ms/$p"; done
    printf '[package]\nname = "foo"\nrust-version.workspace = true\n' > "$tmp/ms/crates/foo/Cargo.toml"
}
ms() { MSRV_ROOT="$tmp/ms" bash "$MSRV"; }
pin="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$REPO/rust-toolchain.toml")"
ms_copy;                                                       expect "msrv: the real stated sites agree"              0 ms
ms_copy; plant "$tmp/ms/README.md" "$pin" "$v";                expect "msrv: a stated site drifts"                     1 ms
ms_copy; printf '[toolchain]\nchannel = "%s"\n' "$v" > "$tmp/ms/rust-toolchain.toml"; expect "msrv: channel bumped, stated sites not swept" 1 ms
ms_copy; rm "$tmp/ms/docs/reference/compatibility.md";         expect "msrv: a stated site file removed"               1 ms
ms_copy; printf '[toolchain]\nchannel = "stable"\n' > "$tmp/ms/rust-toolchain.toml"; expect "msrv: a non-numeric channel is not an MSRV" 1 ms
ms_copy; printf '        toolchain: "%s"\n' "$v" >> "$tmp/ms/.github/workflows/ci.yml";        expect "msrv: a workflow toolchain literal"   1 ms
ms_copy; printf '  RUSTUP_TOOLCHAIN: "%s"\n' "$v" >> "$tmp/ms/.github/workflows/ci.yml";      expect "msrv: a RUSTUP_TOOLCHAIN literal"     1 ms
ms_copy; printf 'rustup target add x --toolchain %s\n' "$v" > "$tmp/ms/embedded/scripts/check/g.sh"; expect "msrv: an embedded --toolchain literal" 1 ms
ms_copy; printf 'cargo +%s fmt\n' "$v" > "$tmp/ms/scripts/x.sh";                       expect "msrv: a cargo +X literal"                1 ms
ms_copy; printf 'rustup toolchain install %s\n' "$v" > "$tmp/ms/scripts/x.sh";            expect "msrv: a toolchain install X literal"     1 ms
ms_copy; mkdir -p "$tmp/ms/embedded/freertos-srt/substrate"; printf 'rustup target add x --toolchain %s\n' "$v" > "$tmp/ms/embedded/freertos-srt/substrate/b.sh"; expect "msrv: a freertos substrate --toolchain literal" 1 ms
ms_copy; printf '      - uses: dtolnay/rust-toolchain@%s\n' "$v" >> "$tmp/ms/.github/workflows/ci.yml"; expect "msrv: a dtolnay/rust-toolchain@X.Y pin" 1 ms
ms_copy; printf '[package]\nname = "foo"\nrust-version = "%s"\n' "$pin" > "$tmp/ms/crates/foo/Cargo.toml"; expect "msrv: a member manifest with its own rust-version" 1 ms

# ---- roster-sync check 5: every member inherits [workspace.lints] ----
mkdir -p "$tmp/ro/nolints" "$tmp/ro/lints"
printf '[package]\nname = "tst-core"\n' > "$tmp/ro/nolints/Cargo.toml"
printf '[package]\nname = "tst-core"\n\n[lints]\nworkspace = true\n' > "$tmp/ro/lints/Cargo.toml"
for v in nolints lints; do
    python3 -I -c '
import json, sys
m = json.load(open(sys.argv[1]))
for p in m["packages"]:
    if p["name"] == "tst-core":
        p["manifest_path"] = sys.argv[2]
json.dump(m, open(sys.argv[3], "w"))' "$tmp/ro/meta.json" "$tmp/ro/$v/Cargo.toml" "$tmp/ro/meta-$v.json"
done
rs_copy
rs_lints() { ROSTER_METADATA="$tmp/ro/meta-$1.json" RSYNC_FILES_ROOT="$tmp/rs" bash "$RSYNC"; }
rs_nolints() { rs_lints nolints; }
rs_withlints() { rs_lints lints; }
expect "roster-sync: a member manifest without [lints] workspace = true" 1 rs_nolints
expect "roster-sync: the same member with the lint table inherited"       0 rs_withlints

# ---- no unreachable / .expect in binding production code (scripts/check/repo/no-unreachable-expect-in-bindings.sh) ----
NUE="$DIR/../check/repo/no-unreachable-expect-in-bindings.sh"
mkdir -p "$tmp/nue/x/src"
nue() { NUE_ROOT="$tmp/nue" NUE_DIRS="x/src" bash "$NUE"; }
nue_src() { printf "$1" > "$tmp/nue/x/src/lib.rs"; } # the fixture is $tmp/nue/x/src/lib.rs
nue_src 'fn a() -> u8 { let v: Option<u8> = None; v.expect("boom") }\n#[cfg(test)]\nmod t { fn b() { unreachable!() } }\n'
expect "no-unreachable-expect: production .expect(" 1 nue
nue_src 'fn a() -> u8 { 1 }\n#[cfg(test)]\nmod t { fn b() { unreachable!(); let v: Option<u8> = None; v.expect("x"); } }\n'
expect "no-unreachable-expect: test-module unreachable!/.expect( ignored" 0 nue
nue_src 'fn a() { match 1 { 1 => {}, _ => unreachable!("no") } }\n'
expect "no-unreachable-expect: production unreachable!" 1 nue
nue_src '/// Never `.expect()` here; see also unreachable!.\n//! module prose naming .expect(\nfn a() -> u8 { 1 }\n'
expect "no-unreachable-expect: comment prose naming unreachable!/.expect(" 0 nue
nue_src 'fn a() -> u8 { let v: Option<u8> = None; v.expect("boom") } // trailing note\n'
expect "no-unreachable-expect: a trailing comment does not shield the code on its line" 1 nue
nue_src 'struct S;\nimpl S {\n    #[cfg(test)]\n    fn t() {}\n}\nfn a() -> u8 { let v: Option<u8> = None; v.expect("boom") }\n'
expect "no-unreachable-expect: an indented #[cfg(test)] does not end the production region" 1 nue
nue_src 'fn a() -> u8 { 1 }\n'
expect "no-unreachable-expect: a clean file" 0 nue
# A column-0 #[cfg(test)] on a NON-module item is not the file tail: a next-line
# item, a same-line item, and a multi-line fn whose own body may `.expect(`.
nue_src '#[cfg(test)]\nextern crate std;\nfn a() -> u8 { let v: Option<u8> = None; v.expect("boom") }\n'
expect "no-unreachable-expect: a cfg(test) extern-crate item does not end the production region" 1 nue
nue_src '#[cfg(test)] use core::fmt::Debug;\nfn a() { unreachable!("boom") }\n'
expect "no-unreachable-expect: a same-line cfg(test) item does not end the production region" 1 nue
nue_src '#[cfg(test)]\npub(crate) fn helper() -> u8 {\n    let v: Option<u8> = None;\n    v.expect("fine in a test helper")\n}\nfn a() -> u8 { 1 }\n'
expect "no-unreachable-expect: a cfg(test) fn body may expect, and scanning resumes after it" 0 nue
nue_src '#[cfg(test)]\npub(crate) fn helper() -> u8 {\n    1\n}\nfn a() -> u8 { let v: Option<u8> = None; v.expect("boom") }\n'
expect "no-unreachable-expect: production code AFTER a cfg(test) fn is still scanned" 1 nue

# kind-equivalence: a hand-edited Python member value fails the generated == committed check.
KE_REPO="$(cd "$DIR/../.." && pwd)"
ke="$tmp/kind-equiv"
mkdir -p "$ke/scripts/ratchets" "$ke/bindings/python/python/tstrans" "$ke/bindings/jvm/src/main/java/org/tstrans"
cp "$KE_REPO/scripts/ratchets/kind-equivalence.tsv" "$ke/scripts/ratchets/"
cp "$KE_REPO/bindings/python/python/tstrans/exceptions.py" "$ke/bindings/python/python/tstrans/"
cp "$KE_REPO"/bindings/jvm/src/main/java/org/tstrans/*Exception.java "$ke/bindings/jvm/src/main/java/org/tstrans/"
expect "kind-equivalence: an unchanged copy passes" 0 env KIND_EQUIV_ROOT="$ke" bash "$KE_REPO/scripts/check/repo/kind-equivalence.sh"
plant "$ke/bindings/python/python/tstrans/exceptions.py" '    INPUT_MALFORMED = 0' '    INPUT_MALFORMED = 99'
expect "kind-equivalence: a hand-edited Python value fails" 1 env KIND_EQUIV_ROOT="$ke" bash "$KE_REPO/scripts/check/repo/kind-equivalence.sh"

# dependency-lattice: a false row fails, the committed table passes.
printf 'tst-core\t(default)\t-\tserde_json\n' > "$tmp/lattice.tsv"
expect "dependency-lattice: a plane holding a forbidden crate fails" 1 env LATTICE_TSV="$tmp/lattice.tsv" bash scripts/check/repo/dependency-lattice.sh
grep -q 'contains serde_json' "$tmp/out" || { echo "FAIL: dependency-lattice: the forbidden-crate row failed for another reason"; sed 's/^/    /' "$tmp/out"; fail=1; }
expect "dependency-lattice: the committed table passes"              0 bash scripts/check/repo/dependency-lattice.sh

# abi-compat: the C ABI compatibility rail judges planted C libraries (SKIPs without abidiff).
expect "abi-compat: the planted-library matrix" 0 bash scripts/check/c/tests/abi-compat-selftest.sh

# publish-guards: a callable workflow's publish-shaped job needs `inputs.dry_run` in its `if:`.
pg="$tmp/publish-guards"; mkdir -p "$pg"
cat > "$pg/bad.yml" <<'YML'
name: bad
on:
  workflow_call:
    inputs:
      dry_run: {type: boolean, default: false}
jobs:
  publish:
    runs-on: ubuntu-latest
    if: github.event_name == 'push'
    environment: pypi
    steps:
      - run: echo hi
YML
sed "s/github.event_name == 'push'/github.event_name == 'push' \&\& !inputs.dry_run/" "$pg/bad.yml" > "$pg/good.yml"
expect "publish-guards: an unguarded callable publish job fails" 1 bash scripts/check/repo/publish-guards.sh "$pg/bad.yml"
grep -q "job 'publish'" "$tmp/out" || { echo "FAIL: publish-guards: the failure did not name the job"; sed 's/^/    /' "$tmp/out"; fail=1; }
expect "publish-guards: a guarded job passes" 0 bash scripts/check/repo/publish-guards.sh "$pg/good.yml"
# A guard must make the job impossible under dry_run: true; a mention is not a guard.
sed "s/github.event_name == 'push'/inputs.dry_run/" "$pg/bad.yml" > "$pg/inverted.yml"
sed "s/github.event_name == 'push'/github.event_name == 'push' || !inputs.dry_run/" "$pg/bad.yml" > "$pg/disjunct.yml"
sed "s/github.event_name == 'push'/\${{ !inputs.dry_run \&\& github.ref_type == 'tag' }}/" "$pg/bad.yml" > "$pg/wrapped.yml"
sed "s/github.event_name == 'push'/github.event_name == 'push'  # !inputs.dry_run/" "$pg/bad.yml" > "$pg/commented.yml"

# publish-guards, caller side: a job calling a callable workflow that declares dry_run must pass `dry_run: true`.
cat > "$pg/callee.yml" <<'YML'
name: callee
on:
  workflow_call:
    inputs:
      dry_run: {type: boolean, default: false}
jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
YML
cat > "$pg/caller-bad.yml" <<'YML'
name: caller
on: push
jobs:
  call:
    uses: ./.github/workflows/callee.yml
    with:
      dry_run: false
YML
cat > "$pg/caller-omit.yml" <<'YML'
name: caller
on: push
jobs:
  call:
    uses: ./.github/workflows/callee.yml
YML
sed 's/dry_run: false/dry_run: true/' "$pg/caller-bad.yml" > "$pg/caller-good.yml"
# a python3 without PyYAML forces the awk fallback
mkdir -p "$pg/noyaml"
real_py="$(command -v python3)"
printf '#!/bin/sh\n[ "$1" = "-c" ] && exit 1\nexec %s "$@"\n' "$real_py" > "$pg/noyaml/python3"
chmod +x "$pg/noyaml/python3"
for mode in python awk; do
  if [[ $mode == awk ]]; then pre=(env PATH="$pg/noyaml:$PATH"); else pre=(); fi
  expect "publish-guards ($mode): a caller passing dry_run: false fails" 1 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/caller-bad.yml"
  grep -q "job 'call'" "$tmp/out" || { echo "FAIL: publish-guards ($mode): the failure did not name the job"; sed 's/^/    /' "$tmp/out"; fail=1; }
  expect "publish-guards ($mode): a caller omitting dry_run fails" 1 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/caller-omit.yml"
  expect "publish-guards ($mode): a caller passing dry_run: true passes" 0 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/caller-good.yml"
  expect "publish-guards ($mode): an inverted guard fails" 1 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/inverted.yml"
  expect "publish-guards ($mode): a disjunctive guard fails" 1 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/disjunct.yml"
  expect "publish-guards ($mode): a wrapped negated guard passes" 0 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/wrapped.yml"
  expect "publish-guards ($mode): a guard only in a trailing comment fails" 1 "${pre[@]}" bash scripts/check/repo/publish-guards.sh "$pg/commented.yml"
done

if [[ "$fail" == 0 ]]; then echo "self-test: ALL OK"; fi
exit "$fail"
