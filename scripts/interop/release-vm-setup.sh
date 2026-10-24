#!/usr/bin/env bash
# Prepare a fresh Ubuntu host as the release VM: the packages the corpus
# phase and the soak need, a Rust toolchain, the AWS CLI for the corpus
# bucket, the UDP receive-buffer ceiling the soak's legs expect, Google
# Chrome for the whep leg, and a clean checkout of the frozen commit with
# tst-interop built in release mode. Idempotent: a second run on the same
# host re-fetches and re-checks out, and apt skips what is installed.
#
# Usage:
#   bash release-vm-setup.sh <full 40-hex sha>
#
# Environment:
#   TSTRANS_REPO_URL   clone URL (default https://github.com/aklofas/ts-transformer.git)
#   TSTRANS_REPO_DIR   checkout directory (default ~/ts-transformer)
#
# Every stage echoes `<stage> rc=N` and the last line is `SETUP-DONE rc=N`;
# a watcher anchors on those at line start. Run it detached and poll the
# log (`nohup bash release-vm-setup.sh <sha> > ~/release-vm-setup.log 2>&1 &`):
# the apt and build stages take ten to twenty minutes together.
#
# Validated on Ubuntu 24.04 x86_64 (the release VM); see lib.sh for this
# directory's portability stance.
set -uo pipefail

SHA=${1:?usage: release-vm-setup.sh <full 40-hex sha>}
[[ ${#SHA} -eq 40 ]] || { echo "release-vm-setup.sh: need the full 40-hex sha, got '$SHA'" >&2; exit 2; }
REPO_URL=${TSTRANS_REPO_URL:-https://github.com/aklofas/ts-transformer.git}
REPO_DIR=${TSTRANS_REPO_DIR:-$HOME/ts-transformer}
export DEBIAN_FRONTEND=noninteractive
export PATH=$HOME/.cargo/bin:/snap/bin:$PATH

stage() { echo "$1 rc=$2"; }

# retry <n> <cmd...> — the clone, fetch and submodule steps talk to GitHub
# and to code.videolan.org, both of which have refused a connection or two
# in a row before (the flake ledger's "Runner setup fetch failure" row).
retry() {
  local n=$1 i; shift
  for ((i = 1; i <= n; i++)); do
    "$@" && return 0
    echo "release-vm-setup.sh: attempt $i/$n failed: $*" >&2
    sleep 30
  done
  return 1
}

rc=0
sudo -E apt-get update && sudo -E apt-get install -y \
  jq python3 build-essential cmake meson ninja-build clang libclang-dev pkg-config git curl unzip \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-rtsp ffmpeg || rc=$?
stage packages "$rc"
[[ $rc -eq 0 ]] || { stage SETUP-DONE "$rc"; exit "$rc"; }

# The corpus bucket client. Ubuntu's apt package is current enough; the
# snap is the fallback where the apt archive has none.
rc=0
if ! command -v aws >/dev/null 2>&1; then
  sudo -E apt-get install -y awscli || sudo snap install aws-cli --classic || rc=$?
fi
stage aws-cli "$rc"
[[ $rc -eq 0 ]] || { stage SETUP-DONE "$rc"; exit "$rc"; }

# rustup with no default toolchain: rust-toolchain.toml in the checkout
# names the pinned one, and the first cargo call installs it.
rc=0
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none || rc=$?
fi
stage rustup "$rc"
[[ $rc -eq 0 ]] || { stage SETUP-DONE "$rc"; exit "$rc"; }

# The soak's UDP legs and the stress sweep's rcvbuf rule both assume the
# kernel allows a 16 MiB receive buffer (scripts/interop/README.md, "UDP
# receive buffer").
rc=0
echo "net.core.rmem_max = 16777216" | sudo tee /etc/sysctl.d/90-tstrans-udp-rcvbuf.conf >/dev/null &&
  sudo sysctl --system >/dev/null && sysctl net.core.rmem_max || rc=$?
stage sysctl "$rc"

# Google Chrome for the whep leg (headless). The .deb is removed after the
# install: it is 140 MB the disk check below would otherwise count.
rc=0
if ! command -v google-chrome >/dev/null 2>&1; then
  deb=$(mktemp -d)/google-chrome-stable_current_amd64.deb
  curl -sSL -o "$deb" https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb &&
    sudo -E apt-get install -y "$deb" || rc=$?
  rm -f "$deb"
fi
command -v google-chrome >/dev/null 2>&1 && google-chrome --version
stage chrome "$rc"
[[ $rc -eq 0 ]] || { stage SETUP-DONE "$rc"; exit "$rc"; }

# The tree: clone once, fetch after; the frozen commit, clean, with every
# submodule at the recorded revision.
rc=0
if [[ -d "$REPO_DIR/.git" ]]; then
  (cd "$REPO_DIR" && retry 5 git fetch -q origin) || rc=$?
else
  retry 5 git clone -q "$REPO_URL" "$REPO_DIR" || rc=$?
fi
stage fetch "$rc"
[[ $rc -eq 0 ]] || { stage SETUP-DONE "$rc"; exit "$rc"; }

rc=0
(cd "$REPO_DIR" && git checkout -q "$SHA" && retry 5 git submodule update --init --recursive) || rc=$?
if [[ $rc -eq 0 ]]; then
  dirty=$(cd "$REPO_DIR" && git status --short)
  if [[ -n "$dirty" ]]; then
    echo "release-vm-setup.sh: the checkout is not clean:" >&2
    echo "$dirty" >&2
    rc=3
  fi
fi
echo "HEAD=$(cd "$REPO_DIR" && git rev-parse HEAD 2>/dev/null)"
stage checkout "$rc"
[[ $rc -eq 0 ]] || { stage SETUP-DONE "$rc"; exit "$rc"; }

# The release binary the corpus phase runs; soak.sh rebuilds the same tree
# at launch (seconds, warm). SRT_FORCE_VENDORED / RIST_FORCE_VENDORED: the
# VM has no system libsrt or librist, and the release evidence must be of
# the vendored ones.
rc=0
(cd "$REPO_DIR" && SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop) || rc=$?
stage build "$rc"
[[ $rc -eq 0 ]] && ls -la "$REPO_DIR/target/release/tst-interop"
stage SETUP-DONE "$rc"
exit "$rc"
