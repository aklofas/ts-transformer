#!/usr/bin/env bash
# Installs the pinned ripgrep release: downloaded with `gh` (needs GH_TOKEN),
# checked against a SHA-256 table, unpacked under $RUNNER_TEMP/ripgrep and put
# on PATH. Runs on Linux and on Git-for-Windows bash, which has no `unzip`
# guarantee, so the zip is opened with Python. The hash is read from stdin:
# with a file name holding a backslash (RUNNER_TEMP on Windows) GNU sha256sum
# prefixes its output with one.
#
# Environment: RUNNER_TEMP (default /tmp); GITHUB_PATH, when set, receives the
# install directory. Three download attempts, 20 s and 60 s apart, each bounded
# to 300 s. RIPGREP_SHA256_OVERRIDE replaces the table entry; it is honoured
# only outside GitHub Actions (for the self-test).
set -euo pipefail

version=15.2.0
waits="20 60"
# A wall-clock bound for one network call, when GNU timeout is available (not
# on macOS; Windows' own timeout.exe is a different tool).
bound=()
if timeout --version 2>/dev/null | grep -q GNU; then bound=(timeout -k 10 300); fi
dest="${RUNNER_TEMP:-/tmp}/ripgrep"

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Linux/x86_64)
    target=x86_64-unknown-linux-musl; ext=tar.gz
    sum=33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c ;;
  Linux/aarch64 | Linux/arm64)
    target=aarch64-unknown-linux-gnu; ext=tar.gz
    sum=a740b91c82eaf9914cfedd353572f2791cbe0162c84101ee0951058f4dcbc90d ;;
  MINGW*/x86_64 | MSYS*/x86_64)
    target=x86_64-pc-windows-msvc; ext=zip
    sum=71b2fef860abe467217a538ff31de02f5258807c0129f771846f87bd029aafc5 ;;
  *) echo "install-ripgrep.sh: no ripgrep $version asset for platform $os/$arch" >&2; exit 1 ;;
esac
[ -n "${GITHUB_ACTIONS:-}" ] || sum="${RIPGREP_SHA256_OVERRIDE:-$sum}"
name="ripgrep-$version-$target"
asset="$name.$ext"

command -v gh >/dev/null 2>&1 || { echo "install-ripgrep.sh: gh is not installed (needed to download $asset)" >&2; exit 1; }
[ -n "${GH_TOKEN:-}" ] || { echo "install-ripgrep.sh: GH_TOKEN is not set (gh needs it to download $asset)" >&2; exit 1; }

mkdir -p "$dest"
attempt=1
while :; do
  if ${bound[@]+"${bound[@]}"} gh release download "$version" -R BurntSushi/ripgrep -p "$asset" -D "$dest" --clobber; then
    break
  fi
  [ "$attempt" -lt 3 ] || { echo "install-ripgrep.sh: download of $asset failed 3 times" >&2; exit 1; }
  w="$(echo "$waits" | awk -v i="$attempt" '{print $i}')"
  echo "::notice::download of $asset attempt $attempt failed; retrying"
  [ -z "$w" ] || sleep "$w"
  attempt=$((attempt + 1))
done

if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum < "$dest/$asset" | cut -d' ' -f1)"
else
  got="$(shasum -a 256 < "$dest/$asset" | cut -d' ' -f1)"
fi
if [ "$got" != "$sum" ]; then
  echo "install-ripgrep.sh: checksum mismatch for $asset (got $got, want $sum)" >&2
  exit 1
fi

case "$ext" in
  tar.gz) tar -xzf "$dest/$asset" -C "$dest" ;;
  zip)
    py=python
    command -v python >/dev/null 2>&1 || py=python3
    "$py" -c "import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$dest/$asset" "$dest" ;;
esac

[ -z "${GITHUB_PATH:-}" ] || echo "$dest/$name" >> "$GITHUB_PATH"
export PATH="$dest/$name:$PATH"
rg --version | head -1
