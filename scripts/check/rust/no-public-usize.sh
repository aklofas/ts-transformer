#!/usr/bin/env bash
# Verify no public counter / capacity / size field uses `usize`.
#
# `usize` is platform-dependent (4 bytes on 32-bit, 8 bytes on 64-bit) which
# makes it FFI-hostile: cdylib consumers on 32-bit Linux / Android-armv7 see
# different layouts than 64-bit consumers. Public counters / capacities /
# sizes should use `u64` to lock the wire shape.
#
# Allow-list covers idiomatic-Rust usage that doesn't escape to FFI:
#   - function args (FFI wrappers convert at the boundary)
#   - const counts (compile-time, no ABI shape)
#   - Vec::len passthroughs and collection-style accessors
#   - encode-helper sizing returns (Rust-only convention; FFI uses byte-vec
#     return shape)
#
# The grep also strips pub(crate) / pub(super) — they are not public surface.

set -euo pipefail

SRC_DIRS=(
    "crates/tst-core/src"
    "crates/tst-pipeline/src"
    "crates/tst-srt/src"
)

# Allow patterns (these are OK to use usize in public surface).
ALLOWLIST_PATTERNS=(
    'pub fn .*\(.*: usize'                                  # function args
    'pub const .*: usize'                                   # const counts (e.g., MAX_PROGRAMS)
    'pub fn .*-> usize \{ .*\.len\(\)'                      # Vec::len passthrough heuristic
    'pub fn len\(&self\) -> usize'                          # collection-style len()
    'pub fn capacity\(&self\) -> usize'                     # collection-style capacity()
    'pub fn payload_limit\(&self\) -> usize'                # Socket::payload_limit — mirrors Transport::max_payload contract
    'pub fn buffered_bytes\(&self\) -> usize'               # PesAssembler gauge (collection-style)
    'pub fn buf_len_bits\(&self\) -> usize'                 # Av1BitReader (#[doc(hidden)])
    'pub type \w+ = usize'                                  # type aliases (rare; documented)
    'pub fn encoded_len(_with|_standalone)?\b'              # encode-helper sizing (Rust-only)
    'pub fn ber_(oid_)?len(_u64)?\b'                        # BER length helpers (Rust-only; u64 sibling same rationale)
    'pub fn pull\(&mut self, out: &mut \[u8\]\) -> usize'   # Muxer::pull (bytes-written, Read::read-shaped)

    # Tuning-knob fields on pub config structs. Bucket (b) per Phase 1
    # inventory: caller-supplied capacity / threshold, FFI wrappers convert
    # at the C boundary. Keep usize for ergonomic Rust assignment; do not
    # widen to u64 unless an FFI consumer reports breakage.
    'pub buffer_packets: usize'                             # MuxerConfig
    'pub gap_buffer_capacity: usize'                        # ReconnectPolicy
    'pub max_unsynced_bytes: usize'                         # SenderConfig
    'pub length: usize'                                     # ImapbParams (1/2/4/8-byte mapping length)

    # Public fields inside pub(crate) / pub(super) parent structs — the
    # field uses `pub` so the strip-by-line filter can't see them, but the
    # parent struct is module-private and therefore not FFI-exposed.
    'pub payload_consumed: usize'                           # mpegts/mux/ts.rs WriteResult (pub(crate))
    'pub raw_header_len: usize'                             # codec/aac/adts.rs Header (pub(super))
    'pub byte_length: usize'                                # klv/st0601/tags.rs LinearRange (pub(crate))
)

violations=0
for dir in "${SRC_DIRS[@]}"; do
    while IFS= read -r line; do
        # Strip pub(crate) / pub(super) lines — not public surface.
        if echo "$line" | grep -qE 'pub\((crate|super)\)'; then
            continue
        fi
        allowed=false
        for pattern in "${ALLOWLIST_PATTERNS[@]}"; do
            if echo "$line" | grep -qE "$pattern"; then
                allowed=true
                break
            fi
        done

        if [[ "$allowed" == "false" ]]; then
            echo "VIOLATION: $line"
            violations=$((violations + 1))
        fi
    done < <(grep -rn 'pub.*: usize\|pub.* -> usize' "$dir" --include='*.rs' || true)
done

if [[ $violations -gt 0 ]]; then
    echo "FAIL: $violations public usize regressions (use u64 for FFI portability)"
    exit 1
fi

echo "OK: no public usize regressions"
