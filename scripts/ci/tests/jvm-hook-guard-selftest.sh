#!/usr/bin/env bash
# Self-test of scripts/ci/jvm-hook-guard.sh: the hook symbols derived from a
# fixture source tree (each way a native can be compiled only under the
# hooks, and the forms the parser must refuse), then archives and libraries
# with and without a hook symbol, a sources JAR carrying a native, and the
# positive control. Ends with the derivation over the real tree.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
GUARD="$ROOT/scripts/ci/jvm-hook-guard.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
if python3 -c '' >/dev/null 2>&1; then py=python3; else py=python; fi

fail() { echo "FAIL: $*"; echo "--- out"; cat "$TMP/out" 2>/dev/null || true; exit 1; }
run() { set +e; bash "$GUARD" "$@" > "$TMP/out" 2>&1; rc=$?; set -e; }
expect_rc() { [ "$rc" -eq "$1" ] || fail "$2: rc=$rc, want $1"; }
has() { grep -qF -- "$1" "$TMP/out" || fail "$2: output lacks '$1'"; }
lacks() { ! grep -qF -- "$1" "$TMP/out" || fail "$2: output has '$1'"; }

# --- A fixture source tree --------------------------------------------------
S="$TMP/src"
mkdir -p "$S/hooks" "$S/rtp"
cat > "$S/lib.rs" <<'RS'
mod rtp;
#[cfg(feature = "jni-test-hooks")]
mod hooks;
RS
cat > "$S/hooks.rs" <<'RS'
mod deep;
jni_native! {
    fn Java_org_tstrans_internal_ModProbe_nOne(_env, _class) = () => {}
}
RS
cat > "$S/hooks/deep.rs" <<'RS'
jni_native! {
    fn Java_org_tstrans_internal_DeepProbe_nTwo(_env, _class) = () => {}
}
RS
cat > "$S/rtp.rs" <<'RS'
jni_native! {
    /// `org.tstrans.rtp.Session.nOpen()`: a shipped native.
    fn Java_org_tstrans_rtp_Session_nOpen(_env, _class) -> jlong = 0 => { 1 }
}

/// A doc example is not a native:
/// fn Java_org_tstrans_X_nY(env, _class) = () => {}
jni_native! {
    /// Gated on the native itself, after a doc line.
    #[cfg(feature = "jni-test-hooks")]
    fn Java_org_tstrans_internal_AttrProbe_nThree(_env, _class) = () => {}
    fn Java_org_tstrans_rtp_Session_nClose(_env, _class) = () => {}
}

// A gated block whose body holds braces in a string and a char literal.
#[cfg(all(feature = "jni-test-hooks", unix))]
jni_native! {
    fn Java_org_tstrans_rtp_Session_nProbeFour<'local>(_env, _class) = () => {
        let _ = format!("}}{}", '}');
        let _ = '{';
    }
}

#[cfg(feature = "jni-test-hooks")]
static GATED_STATIC: bool = true;
jni_native! {
    fn Java_org_tstrans_rtp_Session_nPlay(_env, _class) = () => {}
}
RS
run derive --src "$S"
expect_rc 0 "fixture derive"
for n in Java_org_tstrans_internal_ModProbe_nOne Java_org_tstrans_internal_DeepProbe_nTwo \
         Java_org_tstrans_internal_AttrProbe_nThree Java_org_tstrans_rtp_Session_nProbeFour; do
  has "native $n" "a hook native in the fixture"
done
for n in Java_org_tstrans_rtp_Session_nOpen Java_org_tstrans_rtp_Session_nClose \
         Java_org_tstrans_rtp_Session_nPlay Java_org_tstrans_X_nY; do
  lacks "$n" "a shipped native or a doc example"
done
# Hook-only classes and packages give their prefix; a class with shipped
# natives (Session) does not, so only its hook native's exact name counts.
has "prefix Java_org_tstrans_internal_" "the hook-only package"
has "prefix Java_org_tstrans_internal_AttrProbe_" "a hook-only class"
lacks "prefix Java_org_tstrans_rtp_" "a package with shipped natives"
echo "PASS: fixture derive"

# --- The parser refuses what it cannot classify ----------------------------
B="$TMP/bad"; mkdir -p "$B"
printf '#[cfg(any(feature = "jni-test-hooks", test))]\nfn Java_a_B_nC() {}\nfn Java_a_B_nD() {}\n' > "$B/lib.rs"
run derive --src "$B"
expect_rc 1 "cfg(any(...))"
has "cannot classify" "cfg(any(...))"
printf 'fn Java_a_B_nD() {}\n' > "$B/lib.rs"
run derive --src "$B"
expect_rc 1 "no hook native"
has "no jni-test-hooks native found" "no hook native"
echo "PASS: refusals"

# --- Libraries and archives -------------------------------------------------
mklib() { # mklib <path> <symbol...>: a stand-in library holding NUL-separated names
  local out="$1"; shift
  printf '\177ELF' > "$out"
  local s; for s in "$@"; do printf '\0%s\0' "$s" >> "$out"; done
}
mkzip() { # mkzip <zip> <entry>=<file>...
  "$py" -I - "$@" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w") as z:
    z.writestr("org/tstrans/Version.class", b"\xca\xfe")
    for spec in sys.argv[2:]:
        name, path = spec.split("=", 1)
        z.write(path, name)
PY
}
mklib "$TMP/plain.so" Java_org_tstrans_rtp_Session_nOpen Java_org_tstrans_rtp_Session_nClose
mklib "$TMP/hook.so" Java_org_tstrans_rtp_Session_nOpen Java_org_tstrans_rtp_Session_nProbeFour
# Mach-O names carry a leading underscore.
mklib "$TMP/hook.dylib" _Java_org_tstrans_rtp_Session_nOpen _Java_org_tstrans_internal_AttrProbe_nThree
# A hook native the parser has never seen, in a hook-only class.
mklib "$TMP/newhook.dll" Java_org_tstrans_rtp_Session_nOpen Java_org_tstrans_internal_AttrProbe_nNew

run check --src "$S" "$TMP/plain.so"
expect_rc 0 "plain library"
has "2 JNI symbol(s), none a test hook" "plain library"
for lib in hook.so hook.dylib newhook.dll; do
  run check --src "$S" "$TMP/$lib"
  expect_rc 1 "$lib"
  has "test-hook symbol(s)" "$lib"
done
has "Java_org_tstrans_internal_AttrProbe_nNew" "a new native in a hook class"

mkzip "$TMP/fat.jar" native/linux-x86_64/libtstjni.so="$TMP/plain.so" native/macos-aarch64/libtstjni.dylib="$TMP/plain.so"
run check --src "$S" "$TMP/fat.jar"
expect_rc 0 "fat JAR without hooks"
has "2 native entries" "fat JAR listing"
has "native/macos-aarch64/libtstjni.dylib" "fat JAR listing"
mkzip "$TMP/hookfat.jar" native/linux-x86_64/libtstjni.so="$TMP/plain.so" native/macos-aarch64/libtstjni.dylib="$TMP/hook.dylib"
run check --src "$S" "$TMP/fat.jar" "$TMP/hookfat.jar"
expect_rc 1 "a fat JAR with one hook library"
has "hookfat.jar!native/macos-aarch64/libtstjni.dylib: test-hook symbol(s)" "the hook entry is named"
mkzip "$TMP/x-sources.jar"
run check --src "$S" "$TMP/x-sources.jar"
expect_rc 0 "sources JAR without natives"
has "0 native entries" "sources JAR listing"
mkzip "$TMP/y-sources.jar" native/linux-x86_64/libtstjni.so="$TMP/plain.so"
run check --src "$S" "$TMP/y-sources.jar"
expect_rc 1 "sources JAR with a native"
has "a sources archive carries 1 native library" "sources JAR with a native"
echo "PASS: check"

# --- The positive control -----------------------------------------------------
run check --src "$S" --expect-hooks "$TMP/hook.so" "$TMP/hookfat.jar"
expect_rc 1 "expect-hooks over a JAR with one plain entry"
has "linux-x86_64/libtstjni.so: no hook symbol" "the plain entry is named"
run check --src "$S" --expect-hooks "$TMP/hook.so" "$TMP/hook.dylib"
expect_rc 0 "expect-hooks over hook builds"
run check --src "$S" --expect-hooks "$TMP/plain.so"
expect_rc 1 "expect-hooks over a plain build"
run check --src "$S" --expect-hooks "$TMP/x-sources.jar"
expect_rc 1 "expect-hooks with nothing to check"
has "no native library to check" "expect-hooks with nothing to check"
run check --src "$S" "$TMP/missing.so"
expect_rc 1 "a missing file"
echo "PASS: positive control"

# --- The real tree --------------------------------------------------------------
run derive
expect_rc 0 "derive over bindings/jvm/src"
has "prefix Java_org_tstrans_internal_" "the real tree's hook package"
has "prefix Java_org_tstrans_internal_PanicProbe_" "the real tree's panic probe"
echo "PASS: real tree"
echo "jvm-hook-guard selftest: all cases passed"
