# tst-c integration-test movement map

The 22 auto-discovered top-level `tests/*.rs` integration binaries were
consolidated into **4 domain harnesses** (`abi`, `receiving`, `muxing`,
`transports`), same `#[path] mod` pattern as the other crates.

## What changed (and what did not)

- **Test bodies are unchanged, with one exception** (filenames kept):
  `receiving/demux_receiver_loopback.rs` had its loopback port band changed from
  `28_500 + pid%500` to `29_000 + pid%1000`. The old band overlapped
  `ts_receiver_loopback`'s `28_000 + pid%1000` for half of all pids. That was
  harmless while each file was its own binary (each ran in a separate process
  with a distinct pid, sequentially), but consolidation runs them concurrently
  in **one** process (one pid), so the two listeners bound the same port ~50% of
  runs → flaky hang/bind-fail. The three fixed-port receivers now occupy
  distinct 1000-wide bands (raw 27_000, ts 28_000, demux 29_000); `live_pair`
  already uses an ephemeral `:0` port. Verified stable 8/8 under consolidation.
- Fully-qualified paths gained a `<domain>::<file>::` prefix. Filtering still
  works: `cargo test -p tst-c --test muxing multi_program::`.
- Each member keeps its file-level `#![cfg(feature = "…")]` gate (srt/rtp/
  udp/tcp/hls/rist); a gated-out member compiles to an empty module, so every
  feature mode builds. No paths needed fixing — the hygiene tests anchor on
  `env!("CARGO_MANIFEST_DIR")`, which is unaffected by the move.
- **Not touched:** the explicit `url_open` `[[test]]` (already folder-shaped)
  and `tests/smoke.c` (a C source read by `abi/smoke.rs` via the manifest path).

## Offline-muxer module relocation (ABI 8 → 9)

The standalone offline `tst_muxer_*` C ABI surface was relocated from
`src/sender/muxer.rs` (gated on `#[cfg(feature = "srt")]` via
`sender/mod.rs`) to the top-level unconditional `src/muxer.rs`, mirroring
the already-unconditional `src/demuxer.rs`. The test import paths changed
from `tstrans::sender::muxer` to `tstrans::muxer` across all muxing tests.
The `tst_muxer_*` declarations in `tstrans.h` moved out of the
`#if defined(TST_HAS_SRT)` blocks and are now emitted unconditionally.

## Equivalence check

No test added/dropped/renamed: tst-c's `cargo test -- --list` count is
unchanged (484) and the test leaf-name multiset is byte-identical before/after
(active + `--ignored`, both feature modes).

## Movement table

### `abi/` — C ABI surface + hygiene: smoke, version, symbol audit, header drift, feature-matrix compile, error routing.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `error_routing.rs` | `abi/error_routing.rs` |
| `feature_matrix_compile.rs` | `abi/feature_matrix_compile.rs` |
| `header_drift.rs` | `abi/header_drift.rs` |
| `smoke.rs` | `abi/smoke.rs` |
| `symbol_audit.rs` | `abi/symbol_audit.rs` |
| `version_check.rs` | `abi/version_check.rs` |

### `receiving/` — C ABI receiver loopbacks (demux/raw/TS) and sender+receiver live pairing.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `demux_receiver_loopback.rs` | `receiving/demux_receiver_loopback.rs` |
| `live_pair.rs` | `receiving/live_pair.rs` |
| (new 2026-09-06) | `receiving/managed_listener_cancel.rs` |
| `raw_receiver_loopback.rs` | `receiving/raw_receiver_loopback.rs` |
| `ts_receiver_loopback.rs` | `receiving/ts_receiver_loopback.rs` |

### `muxing/` — C ABI muxing: multi-program/stream, audio+subtitle, codec stats, demux-config AV1 parity.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `audio_subtitle.rs` | `muxing/audio_subtitle.rs` |
| `codec_stats.rs` | `muxing/codec_stats.rs` |
| `demux_config_av1_parity.rs` | `muxing/demux_config_av1_parity.rs` |
| `multi_program.rs` | `muxing/multi_program.rs` |
| `multi_program_event_identity.rs` | `muxing/multi_program_event_identity.rs` |
| `multi_stream.rs` | `muxing/multi_stream.rs` |
| `stats.rs` | `muxing/stats.rs` |

### `transports/` — C ABI transport open smokes: HLS, RIST, RTP, TCP, UDP.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `hls_publish_smoke.rs` | `transports/hls_publish_smoke.rs` |
| `rist_open_smoke.rs` | `transports/rist_open_smoke.rs` |
| `rtp_open_smoke.rs` | `transports/rtp_open_smoke.rs` |
| `tcp_open_smoke.rs` | `transports/tcp_open_smoke.rs` |
| (new, Arc 2 WP-D) | `transports/udp_close_cancels_first.rs` |
| `udp_open_smoke.rs` | `transports/udp_open_smoke.rs` |
