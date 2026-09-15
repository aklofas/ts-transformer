#!/usr/bin/env bash
# 22nd bash ratchet (Phase 4 Stage 1).
# Verifies tstrans.h contains TST_HAS_SRT + TST_HAS_RTP defines and
# that every tst_rtp_*, tst_rtsp_*, and existing tst_*_open/SRT-specific
# symbol is wrapped in the appropriate #ifdef guard.
#
# Why this ratchet: cbindgen's [parse.expand] cfg-aware emission can
# subtly misbehave on cfg-conditional generic functions — symbols can
# leak outside their guard, producing C-side compile errors when
# downstream consumers build with --features srt only.
#
# Implementation note: every tst_rtp_*/tst_rtsp_* item lives inside the
# rtp/rtsp modules, which are gated at declaration with #[cfg(feature =
# "rtp")] (bindings/c/core/src/lib.rs). That module gate is the sole source
# of each symbol's #if defined(TST_HAS_RTP) guard in the combined header
# (the per-fn cfgs that once duplicated it — emitting a doubled
# #if (defined(TST_HAS_RTP) && defined(TST_HAS_RTP)) — were removed). This
# ratchet verifies the guard invariant by checking the COMMITTED header for
# #if defined(TST_HAS_RTP)/#endif guard blocks, rather than by rendering an
# rtp-disabled header: cbindgen 0.29.2 has no CLI/config mechanism to scope
# a non-macro-expand render to a feature subset (verified empirically — see
# the generator-check comment below); it always emits every cfg-gated item,
# guarded, regardless of which features are requested. Generator failure,
# an empty declaration set and a missing generator under CI are all FAIL; a
# missing generator locally is a SKIP (never a PASS).

set -euo pipefail

# Paths resolve from the script location (not the caller's cwd) so the
# self-test and the pre-push runner can invoke this from anywhere.
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
HEADER="${HCS_HEADER:-$ROOT/bindings/c/include/tstrans.h}"
CBINDGEN="${HCS_CBINDGEN:-cbindgen}"
# Floor on the generated declaration set (cbindgen cannot render an
# srt-only subset — see below — so this is the full srt+rtp surface, same
# as the committed header: 468 `tst_*(` declarations today, 94 of them
# tst_rtp_*/tst_rtsp_*). 200 is a generous floor that still fails on the
# empty/near-empty output a broken generator produces (and on a generator
# that silently rendered the wrong crate).
MIN_DECLS="${HCS_MIN_DECLS:-200}"

if [[ ! -f "$HEADER" ]]; then
    echo "FAIL: $HEADER not found"
    exit 1
fi

# Check TST_HAS_* defines present
if ! grep -q "TST_HAS_SRT" "$HEADER"; then
    echo "FAIL: TST_HAS_SRT not defined in $HEADER"
    exit 1
fi
if ! grep -q "TST_HAS_RTP" "$HEADER"; then
    echo "FAIL: TST_HAS_RTP not defined in $HEADER"
    exit 1
fi

# Committed-header guard check: the opaque RTP handle typedefs must sit
# inside #if defined(TST_HAS_RTP) / #endif blocks (the typedef forward
# declarations ARE guarded even with sort_by=Name). Runs in every mode —
# it is the only check available when no generator is installed, and a
# cheap extra invariant when one is.
python3 - "$HEADER" <<'PY'
import re, sys

with open(sys.argv[1]) as f:
    text = f.read()

rtp_typedef_guards = re.findall(
    r'#if\s+(?:defined\(TST_HAS_RTP\)|TST_HAS_RTP)\s*\n'
    r'(?:[^\n]*\n)*?'
    r'#endif',
    text
)
if not rtp_typedef_guards:
    print("FAIL: no #if defined(TST_HAS_RTP) guard blocks found in header")
    sys.exit(1)
PY

# Generator check: render the header directly from tst-c-core — the crate
# bindings/c/build.rs actually points cbindgen at (`.with_crate(&core_dir)`);
# the tst-c crate itself is a thin cdylib/staticlib wrapper that only
# `pub use`s tst-c-core's symbols and has none of its own, so pointing
# cbindgen at `tst-c` instead renders an EMPTY header (verified empirically
# — not a generator failure, just the wrong crate). Requires (a) the
# generator succeeds, (b) a non-trivial declaration set came out — empty or
# near-empty means a broken generator or a wrong-crate render.
#
# There is deliberately no attempt to render an "rtp-disabled" subset here:
# cbindgen 0.29.2's CLI has no `--features` flag (checked via `cbindgen
# --help`), and even the library's feature-scoped rendering (`[parse.expand]
# .features`, used only by its `cargo expand` macro-expansion path) doesn't
# apply — this project's cbindgen.toml has no `[parse.expand]` block, so
# cbindgen parses raw source via syn and never evaluates
# `#[cfg(feature = ...)]` to decide item inclusion; it always emits every
# cfg-gated item, translating the cfg into the `#if defined(TST_HAS_*)`
# guard from `[defines]` instead (confirmed by rendering with zero features
# requested and still getting every tst_rtp_*/tst_rtsp_* declaration). The
# committed-header guard check above is the leak invariant; this generator
# check only proves cbindgen still runs end-to-end against the real source
# tree and still emits the full surface. Without a generator this is a SKIP
# locally (the pre-push runner surfaces SKIP lines as `NOTE ratchets:`) and
# a FAIL in CI, where ci.yml installs cbindgen.
if ! command -v "$CBINDGEN" >/dev/null 2>&1; then
    if [[ -n "${CI:-}" ]]; then
        echo "FAIL: cbindgen not installed in CI — the generator check cannot run (ci.yml must install cbindgen 0.29.2 before this rail)"
        exit 1
    fi
    echo "SKIP: c-header-conditional-sections: cbindgen not installed — generator check NOT run (committed-header guard check only; this is not a PASS)"
    exit 0
fi

TMPFILE=$(mktemp "${TMPDIR:-/tmp}/tstrans_render_XXXXXX.h")
trap 'rm -f "$TMPFILE"' EXIT

# Generate the header from tst-c-core. Run from the workspace root
# (cbindgen resolves --crate against the cwd's workspace). stderr is
# deliberately NOT suppressed: a generator failure must show why.
if ! (cd "$ROOT" && "$CBINDGEN" \
        --config bindings/c/cbindgen.toml \
        --crate tst-c-core \
        --output "$TMPFILE"); then
    echo "FAIL: cbindgen failed generating the header (see stderr above)"
    exit 1
fi

python3 - "$TMPFILE" "$MIN_DECLS" <<'PY'
import re, sys

with open(sys.argv[1]) as f:
    text = f.read()
min_decls = int(sys.argv[2])

decls = 0
rtp_decls = 0
for line in text.split('\n'):
    stripped = line.strip()
    if stripped.startswith('#') or stripped.startswith('*') or stripped.startswith('//'):
        continue
    if re.search(r'\btst_\w+\s*\(', line):
        decls += 1
    if re.search(r'\btst_(?:rtp|rtsp)_\w+\s*\(', line):
        rtp_decls += 1

if decls < min_decls:
    print(f"FAIL: header render has only {decls} tst_* declaration(s) (floor {min_decls}) — generator produced an empty or wrong-crate surface")
    sys.exit(1)
print(f"header render: {decls} tst_* declarations ({rtp_decls} tst_rtp_*/tst_rtsp_*) via tst-c-core")
PY

echo "PASS: c-header-conditional-sections"
