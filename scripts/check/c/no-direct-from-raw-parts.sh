#!/usr/bin/env bash
# All C-ABI byte-slice construction that takes a caller-supplied length must
# route through ffi_slice/ffi_slice_mut (bindings/c/core/src/ffi_slice.rs),
# which enforce the (NULL,0) and len<=isize::MAX preconditions.
# A direct from_raw_parts on a caller-supplied length reintroduces the
# soundness gap these helpers close.
#
# Exclusions:
# - src/ffi_slice.rs  — the helpers themselves contain the from_raw_parts calls.
# - src/event.rs      — all from_raw_parts are in #[cfg(test)] blocks only,
#                       reading arena-internal (not caller-supplied) lengths.
#                       Re-check if event.rs ever gains non-test slice construction.
# - src/config/streams.rs lines matching `from_raw_parts(language, 3)` — three
#                       ISO 639-2 fixed-length (literal 3) language-code reads;
#                       not a caller-supplied length. Any other from_raw_parts in
#                       that file (e.g. with a variable length) IS caught.
set -euo pipefail
cd "$(dirname "$0")/../../.."
hits=$(grep -rn 'from_raw_parts' bindings/c/core/src --include='*.rs' \
  | grep -v 'src/ffi_slice.rs' \
  | grep -v 'src/event.rs' \
  | grep -v 'from_raw_parts(language, 3)' \
  || true)
if [ -n "$hits" ]; then
  echo "FAIL: direct from_raw_parts outside allowed sites in bindings/c/core/src/:"
  echo "$hits"
  exit 1
fi
echo "OK: no direct from_raw_parts on caller-supplied lengths in bindings/c/core/src/"
