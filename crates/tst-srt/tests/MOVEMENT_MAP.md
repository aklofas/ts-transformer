# tst-srt integration-test movement map

Two changes landed together:

1. **Within-crate consolidation.** The SRT transport tests were grouped
   from per-file binaries into **5 domain harnesses** (`url`, `stats`,
   `builder`, `loopback`, `pipeline`). Same `#[path] mod` pattern as tst-core:
   `tests/<domain>.rs` includes its members from `tests/<domain>/`.
2. **Cross-crate ownership cleanup.** Eight tests that did **not** depend on
   SRT were moved to the crate that owns the behavior they exercise (see the
   second table). Their imports proved they only touch `tst_core` /
   `tst_pipeline`, so they did not belong in the SRT transport crate.

## What changed (and what did not)

- **Test bodies are unchanged.** Pure relocation.
- Fully-qualified paths gained a `<domain>::<file>::` prefix (members are now
  modules). Filtering still works: `cargo test -p tst-srt --test loopback handshake::`.
- The shared loopback helpers + the `require_loopback!` macro live in
  `tests/common/mod.rs`. Because that macro is `#[macro_export]` and resolves
  `$crate::common::…`, `common` is now declared **once at each domain binary's
  root** (`#[macro_use] #[path = "common/mod.rs"] mod common;`) instead of
  per-file; member references were updated from `common::…` to `crate::common::…`.
  (The `url` domain does not use the helper, so it has no `common`.)

## Equivalence check

No test was added/dropped/renamed: across the whole workspace the
`cargo test -- --list` count is unchanged and the test leaf-name multiset is
byte-identical before and after (active + `--ignored`, both feature modes).
Per-crate counts shift by exactly the cross-crate moves: tst-srt -19,
tst-core +16, tst-pipeline +3 (net zero).

## Within-crate moves

### `url/` — SRT URL parsing and vocabulary coverage. SRT transport surface — stays in tst-srt.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `url_parser.rs` | `url/url_parser.rs` |
| `url_parser_boundaries.rs` | `url/url_parser_boundaries.rs` |
| `url_vocabulary_coverage.rs` | `url/url_vocabulary_coverage.rs` |

### `stats/` — SRT socket and transport statistics. SRT transport surface — stays in tst-srt.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `socket_stats.rs` | `stats/socket_stats.rs` |
| `stats.rs` | `stats/stats.rs` |

### `builder/` — SRT socket/listener construction, options, lifecycle, and I/O. SRT transport surface — stays in tst-srt.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `io.rs` | `builder/io.rs` |
| `lifecycle.rs` | `builder/lifecycle.rs` |
| `linger.rs` | `builder/linger.rs` |
| `listener.rs` | `builder/listener.rs` |
| `options.rs` | `builder/options.rs` |
| `stream_id.rs` | `builder/stream_id.rs` |
| `udp_buffer.rs` | `builder/udp_buffer.rs` |

### `loopback/` — SRT end-to-end loopback transfer, handshake, encryption, timeouts. SRT transport surface — stays in tst-srt.

| old `tests/…` | new `tests/…` |
| --- | --- |
| (new 2026-09-16) | `loopback/accept_handle.rs` |
| `cancellation_loopback.rs` | `loopback/cancellation_loopback.rs` |
| `connect_timeout.rs` | `loopback/connect_timeout.rs` |
| `encrypted_packet_filter.rs` | `loopback/encrypted_packet_filter.rs` |
| `getaddrinfo_walk.rs` | `loopback/getaddrinfo_walk.rs` |
| `handshake.rs` | `loopback/handshake.rs` |
| `ipv6_loopback.rs` | `loopback/ipv6_loopback.rs` |
| `listener_accept_timeout.rs` | `loopback/listener_accept_timeout.rs` |
| (new 2026-09-06) | `loopback/listener_cancel.rs` |
| `maxbw_roundtrip.rs` | `loopback/maxbw_roundtrip.rs` |
| `payload_limit.rs` | `loopback/payload_limit.rs` |
| `srto_sender.rs` | `loopback/srto_sender.rs` |

### `pipeline/` — pipeline shells (MuxSender/Receiver/Managed) over SRT transport. Needs SRT — stays in tst-srt.

| old `tests/…` | new `tests/…` |
| --- | --- |
| `pipeline_managed.rs` | `pipeline/pipeline_managed.rs` |
| `pipeline_receiver_live.rs` | `pipeline/pipeline_receiver_live.rs` |
| `pipeline_receiver_live_corpus.rs` | `pipeline/pipeline_receiver_live_corpus.rs` |
| `pipeline_sender.rs` | `pipeline/pipeline_sender.rs` |

## Cross-crate moves (ownership cleanup)

| old `tst-srt/tests/…` | new location | why it moved |
| --- | --- | --- |
| `codec_av1_corpus.rs` | `tst-core/tests/codec/codec_av1_corpus.rs` | AV1 codec corpus check; imports only `tst_core`. |
| `codec_h266_corpus.rs` | `tst-core/tests/codec/codec_h266_corpus.rs` | H.266 codec corpus check; imports only `tst_core`. |
| `local_codec_corpus.rs` | `tst-core/tests/codec/local_codec_corpus.rs` | H.264/H.265 corpus check; imports only `tst_core`. |
| `local_fixtures.rs` | `tst-core/tests/klv/local_fixtures.rs` | KLV local-fixture decode; imports only `tst_core::klv`. |
| `mpegts_demux_local.rs` | `tst-core/tests/mpegts/demux_local.rs` | demux over local TS corpus; imports only `tst_core`. |
| `mpegts_mux_local.rs` | `tst-core/tests/mpegts/mux_local.rs` | mux over local corpus; imports only `tst_core`. |
| `mpegts_mux_ffprobe.rs` | `tst-core/tests/mpegts/mux_ffprobe.rs` | mux output validated by ffprobe; imports only `tst_core` (a cross-crate fixture path hack was removed). |
| `pipeline_sender_unit.rs` | `tst-pipeline/tests/pipeline_sender_unit.rs` | unit-tests `tst_pipeline::MuxSender` over a mock transport; needs no SRT. |
