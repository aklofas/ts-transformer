# Runnable examples

Hands-on, runnable companions to the cookbook recipes. Every example is
self-contained and builds via `cargo`; each folder's README gives the
read-order.

```sh
cargo run -p tst-examples --example <name>
```

`<name>` is the basename without `.rs`. To list every example name, run
`cargo run -p tst-examples --example` with no name (cargo prints the
available examples).

## Where to start

| If you want to… | Start here |
|---|---|
| See the smallest possible thing this library does | [`getting-started/hello_world.rs`](getting-started/hello_world.rs) |
| Send video + KLV over an SRT link | [`sending/send_pipeline_to_socket.rs`](sending/send_pipeline_to_socket.rs) |
| Write an MPEG-TS file from scratch | [`muxing/mux_to_file.rs`](muxing/mux_to_file.rs) |
| Decode KLV from a `.ts` capture | [`receiving/demux_to_events.rs`](receiving/demux_to_events.rs) → [`klv-metadata/extract_klv.rs`](klv-metadata/extract_klv.rs) → [`klv-metadata/klv_decode_file.rs`](klv-metadata/klv_decode_file.rs) |
| Pair video AUs with KLV records | [`pairing/pair_klv_pipeline.rs`](pairing/pair_klv_pipeline.rs) |
| Pull resolution / profile / sample-rate out of a stream | [`codec-parsing/parse_video_parameters.rs`](codec-parsing/parse_video_parameters.rs) |

## Categories

Each folder is a curriculum: examples are numbered in read-order in the
folder's own README, with explicit "diffs from previous" pointers where
the progression is cumulative.

| Folder | Covers | Cookbook |
|---|---|---|
| [`getting-started/`](getting-started/) | 1-page first-encounter example | [Send a single TS packet](../docs/cookbook/sending/send-single-packet.md) |
| [`sending/`](sending/) | SRT + transport-trait senders | [Sending](../docs/cookbook/index.md#-sending--put-a-ts-on-the-wire) |
| [`muxing/`](muxing/) | File-only mux (no SRT); single + multi-program; codecs | [Muxing](../docs/cookbook/index.md#-muxing--build-a-ts-no-network) |
| [`receiving/`](receiving/) | SRT receivers + file-replay demux | [Receiving](../docs/cookbook/index.md#-receiving--take-a-ts-off-the-wire) |
| [`klv-metadata/`](klv-metadata/) | ST 0601 / ST 0102 / ST 0903 encode + decode | [KLV](../docs/cookbook/index.md#-klv--encode-and-decode-metadata-directly) |
| [`pairing/`](pairing/) | Video AU ↔ KLV pairing (manual + `Pairer` helper) | [Pairing](../docs/cookbook/index.md#-pairing--align-klv-with-video-frames) |
| [`codec-parsing/`](codec-parsing/) | H.264 / H.265 / H.266 / AV1 parameter sets, audio, subtitles | [Codecs](../docs/cookbook/index.md#-codecs--parse-video-and-audio-elementary-streams) |
| [`operations/`](operations/) | Reconnect, fan-out, ops-flavored patterns | [Operations](../docs/cookbook/index.md#-operations--lifecycle-stats-shutdown-fixtures) |

## Maintainer tooling

Fixture-generators are not examples — they're maintainer-only scripts
that regenerate the test corpus at `crates/tst-core/tests/fixtures/`.
They live alongside the tests at `crates/tst-core/tests/tools/` and are
declared as `[[bin]]` targets in `tst-core`:

```sh
cargo run -p tst-core --bin gen-synthetic-fixtures
cargo run -p tst-core --bin gen-subtitle-fixtures -- crates/tst-core/tests/fixtures/subtitles
cargo run -p tst-core --bin gen-h266-fixtures
cargo run -p tst-core --bin gen-av1-fixtures
```

Re-run them only when the encoder is intentionally changed; commit the
diff alongside the change.

## C examples

C ABI examples mirror this taxonomy under
[`../bindings/c/examples/`](../bindings/c/examples/). Linux
x86_64 only by build convention; build with `gcc` + the cbindgen-emitted
header. See that folder's README for build invocation.
