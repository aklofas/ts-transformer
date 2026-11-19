#!/usr/bin/env bash
# The public-API ratchet: every crate with a committed public-api.txt
# (scripts/lib/roster.sh public-api) is rendered with `cargo public-api` and
# diffed against its baseline, then the optional TLS items are checked to be in
# the baselines.
#
#   usage: public-api-diff.sh [<nightly toolchain>]   e.g. nightly-2026-07-03
#          (default: the one line of scripts/lib/public-api-toolchain)
#   env:   PUBLIC_API_ONLY  space-separated package names: check only those
#                           (the roster-size guard still applies)
#          PUBLIC_API_OUT   where the rendered files go (default /tmp)
#
# The toolchain must be installed and `cargo-public-api` on PATH. The pin lives
# in scripts/lib/public-api-toolchain: cargo-public-api renders rustdoc JSON with this toolchain, and an unpinned `nightly` has
# caused false std::io<->core::io baseline drift, so a bare `nightly` is
# refused here.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
tc="${1:-$(head -n1 scripts/lib/public-api-toolchain)}"
case "$tc" in
  nightly-[0-9]*) ;;
  *) echo "usage: $0 [nightly-YYYY-MM-DD]" >&2; exit 2 ;;
esac
out="${PUBLIC_API_OUT:-/tmp}"

# Assigned first: a roster.sh failure inside the loop's command substitution
# would otherwise leave an empty loop and a pass.
pairs="$(bash scripts/lib/roster.sh public-api)"
test "$(printf '%s\n' $pairs | grep -c .)" -eq 11 || { echo "public-api roster is not the 11 baselined crates: $pairs" >&2; exit 1; }
# Each crate's declared feature envelope ([package.metadata.public-api] in its
# Cargo.toml), so optional surfaces such as TLS are ratcheted.
feats="$(bash scripts/lib/roster.sh public-api-features)"
for pair in $pairs; do
  pkg="${pair%%:*}"
  dir="${pair##*:}"
  if [ -n "${PUBLIC_API_ONLY:-}" ]; then
    case " $PUBLIC_API_ONLY " in *" $pkg "*) ;; *) continue ;; esac
  fi
  feat="$(printf '%s\n' "$feats" | awk -F'\t' -v p="$pkg" '$1 == p {print $2}')"
  test -n "$feat" || { echo "no public-api-features row for $pkg" >&2; exit 1; }
  fargs=()
  [ "$feat" = "-" ] || fargs=(--features "$feat")
  SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo "+$tc" public-api -p "$pkg" "${fargs[@]}" --simplified > "$out/public-api-current-$pkg.txt"
  if ! diff -u "$dir/public-api.txt" "$out/public-api-current-$pkg.txt"; then
    echo "Public API drift in $pkg. Update $dir/public-api.txt or revert the change." >&2
    exit 1
  fi
done
# The optional TLS items must be in the baselines, not only in a render.
rg -q 'pub mod tst_core::net::tls' crates/tst-core/public-api.txt || { echo "tst-core baseline lacks the tls surface" >&2; exit 1; }
rg -q 'tls_root_certs' crates/tst-rtp/public-api.txt || { echo "tst-rtp baseline lacks the rtsp-server-tls surface" >&2; exit 1; }
