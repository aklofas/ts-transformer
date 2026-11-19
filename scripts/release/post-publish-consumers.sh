#!/usr/bin/env bash
# Build a consumer of the PUBLISHED packages from each of the three registries.
#
#   usage: post-publish-consumers.sh <X.Y.Z> [pypi|crates|maven]...   (default: all three)
#
# Run it after a tag publish, once the registries show the release. Each part
# works in its own temp dir outside the checkout and fetches in a retry loop
# (index and CDN lag):
#   pypi    fresh venv, `pip install --no-cache-dir --only-binary=:all:
#           tstrans==X.Y.Z` (a wheel, never an sdist build), import
#           tstrans, tstrans.srt and tstrans.webrtc, check tstrans.__version__.
#   crates  `cargo new` with exact-version dependencies on every library crate
#           of the publish roster, a main.rs that builds a MuxerConfig and
#           pushes one access unit, `cargo build` with no [patch] and no path
#           dependency: the vendored libsrt, mbedTLS and librist build from the
#           published packages, so this is the check that those are complete.
#   maven   a throwaway Gradle project (fresh Gradle home) with mavenCentral() only, depending on
#           org.tstrans:tstrans-jvm:X.Y.Z, running the module-path consumer
#           from bindings/jvm/src/moduleSmoke (loads the native library out of
#           the published jar on this host's platform).
# Each part prints `PART <name>: rc=N`; the exit status is non-zero when any
# part failed.
#
# Overridable (the self-test and a run against an older release use these):
#   PPC_ATTEMPTS    fetch attempts per part (default 10)
#   PPC_INTERVAL    seconds between attempts (default 30)
#   PPC_PY_SNIPPET  file: the Python the venv runs, VERSION in its environment
#   PPC_RUST_MAIN   file: the consumer's main.rs
#   PPC_JAVA_SRC    directory: the module-path consumer's sources (module-info.java, org/...)
#   PPC_CRATES      space-separated crate names (default: the roster's library crates)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
ATTEMPTS="${PPC_ATTEMPTS:-10}"
INTERVAL="${PPC_INTERVAL:-30}"

usage() { echo "usage: $0 <X.Y.Z> [pypi|crates|maven]..." >&2; exit 2; }
[[ $# -ge 1 ]] || usage
VERSION="$1"; shift
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version '$VERSION' is not X.Y.Z" >&2; usage; }
PARTS=("$@")
[[ ${#PARTS[@]} -gt 0 ]] || PARTS=(pypi crates maven)
for p in "${PARTS[@]}"; do
  case "$p" in pypi|crates|maven) ;; *) echo "unknown part '$p'" >&2; usage ;; esac
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/post-publish-consumers.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# retry <what> <cmd...>: the command up to $ATTEMPTS times, $INTERVAL apart.
retry() {
  local what="$1" n=1; shift
  until "$@"; do
    if [[ "$n" -ge "$ATTEMPTS" ]]; then
      echo "$what: still failing after $n attempts" >&2
      return 1
    fi
    echo "$what: attempt $n failed, retrying in ${INTERVAL}s" >&2
    n=$((n + 1))
    sleep "$INTERVAL"
  done
}

part_pypi() {
  local dir="$WORK/pypi"; mkdir -p "$dir"
  python3 -m venv "$dir/venv" || { echo "could not create a venv (python3 -m venv)" >&2; return 1; }
  # Wheels only: a source build from the sdist would pass without testing a
  # wheel, and the part must fail on a platform that has none.
  retry "pip install tstrans==$VERSION" "$dir/venv/bin/pip" install --no-cache-dir --only-binary=:all: "tstrans==$VERSION" || {
    echo "no wheel for tstrans==$VERSION installed (no wheel for this platform or Python, or the index has not caught up)" >&2
    return 1
  }
  if [[ -n "${PPC_PY_SNIPPET:-}" ]]; then
    cp "$PPC_PY_SNIPPET" "$dir/check.py"
  else
    cat > "$dir/check.py" <<'PY'
import os
import tstrans
import tstrans.srt
import tstrans.webrtc

want = os.environ["VERSION"]
assert tstrans.__version__ == want, (tstrans.__version__, want)
print("tstrans", tstrans.__version__)
PY
  fi
  VERSION="$VERSION" "$dir/venv/bin/python" "$dir/check.py" || return 1
}

part_crates() {
  local dir="$WORK/crates/consumer" crates c
  mkdir -p "$dir/src"
  # A roster failure must not read as an empty list of dependencies.
  crates="${PPC_CRATES:-$(bash "$ROOT/scripts/lib/roster.sh" publishable | grep -v '^tstrans-')}" || return 1
  {
    printf '[package]\nname = "consumer"\nversion = "0.0.0"\nedition = "2021"\n\n[dependencies]\n'
    for c in $crates; do printf '%s = "=%s"\n' "$c" "$VERSION"; done
  } > "$dir/Cargo.toml"
  if [[ -n "${PPC_RUST_MAIN:-}" ]]; then
    cp "$PPC_RUST_MAIN" "$dir/src/main.rs"
  else
    cat > "$dir/src/main.rs" <<'RS'
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

fn main() {
    let mut mux = Muxer::new(MuxerConfig::default()).expect("valid config");
    let au = [0x00, 0x00, 0x00, 0x01, 0x65, 0xA5, 0xA5, 0xA5];
    mux.push_video(&au, Pts90khz::new(0), true).expect("push_video");
    let mut buf = [0u8; 1316];
    let n = mux.pull(&mut buf);
    assert!(n >= 188 && buf[0] == 0x47, "the muxer produced no TS packet");
    println!("muxed {n} bytes");
}
RS
  fi
  (
    cd "$dir"
    # Build the vendored C libraries from the published packages even where a
    # host libsrt or librist is installed.
    export SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1
    retry "cargo fetch (exact versions $VERSION)" cargo fetch || exit 1
    cargo build || exit 1
    cargo run -q || exit 1
    # Every library crate came from the registry: a [patch] or [source] in a
    # cargo config would otherwise be obeyed silently.
    cargo metadata --format-version 1 | jq -e '[.packages[] | select(.name | test("^(tst-|tstrans-)")) | .source]
        | length > 0 and all(. == "registry+https://github.com/rust-lang/crates.io-index")' > /dev/null \
      || { echo "a tst-* or tstrans-* crate did not come from crates.io (a [patch] or [source] replacement?)" >&2; exit 1; }
  )
}

part_maven() {
  local dir="$WORK/maven" src="${PPC_JAVA_SRC:-$ROOT/bindings/jvm/src/moduleSmoke/java}"
  [[ -f "$src/module-info.java" ]] || { echo "no module-path consumer sources at $src" >&2; return 1; }
  mkdir -p "$dir/src/main"
  cp -R "$src" "$dir/src/main/java"
  printf 'rootProject.name = "consumer"\n' > "$dir/settings.gradle.kts"
  cat > "$dir/build.gradle.kts" <<KTS
plugins { application }
repositories { mavenCentral() }
dependencies { implementation("org.tstrans:tstrans-jvm:$VERSION") }
tasks.register("resolveDeps") { doLast { configurations.runtimeClasspath.get().resolve() } }
tasks.withType<JavaCompile>().configureEach { options.release.set(17) }
application {
    mainModule.set("org.tstrans.smoke")
    mainClass.set("org.tstrans.smoke.Main")
}
KTS
  (
    cd "$dir"
    # A fresh Gradle home: no cached artefact and no init script (mavenLocal)
    # of the maintainer's can answer for Maven Central.
    export GRADLE_USER_HOME="$dir/gradle-home"
    # Only the resolve is retried: a compile error is not index lag.
    retry "gradle resolve org.tstrans:tstrans-jvm:$VERSION" gradle --no-daemon -q --refresh-dependencies resolveDeps || exit 1
    gradle --no-daemon -q run | tee "$dir/run.out" || exit 1
    grep -qx "version=$VERSION" "$dir/run.out" || { echo "the consumer did not print version=$VERSION" >&2; exit 1; }
  )
}

failed=0
for p in "${PARTS[@]}"; do
  rc=0
  echo "== part $p"
  "part_$p" || rc=$?
  echo "PART $p: rc=$rc"
  [[ "$rc" -eq 0 ]] || failed=1
done
exit "$failed"
