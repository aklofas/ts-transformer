# tst-hls integration-test map

The tst-hls test suite uses standalone binaries — one file per domain.
This file documents what each binary covers.

## Test binaries

| Binary | Feature gates | What it covers |
|---|---|---|
| `net_hls_e2e` | `serve` | `MuxPublisher<HlsPublisher>` over TCP; an `ffmpeg` pull client fetches `/playlist.m3u8` and verifies byte-identity with the source TS. Skipped gracefully if `ffmpeg` is absent. Moved from `crates/tst-tcp/tests/hls_e2e.rs`. |
| `net_http_hardening` | `serve` | Serve-from-known-set security model (CWE-22 closure): traversal paths and unknown segment names → 404; only segmenter-known names serve bytes. Non-GET methods rejected. Basic-auth covers all routes. |
| `net_klv_interop` | `serve` | KLV-over-HLS client contract: `MuxPublisher<HlsPublisher>` produces segments that a fresh per-segment `Demuxer` recovers PAT-first (independent decodability), with KLV carried in the hls.js-native shape — a dedicated PES PID tagged with the `KLVA` registration descriptor, bare-SMPTE-UL payloads that round-trip byte-identically, and strictly monotonic 90 kHz PTS. Re-demuxes segment files off disk (never speaks HTTP), but serve-gated because `HlsPublisherBuilder` pulls the server in. |

## Equivalence note

`hls_e2e` was originally in `crates/tst-tcp/tests/` (gated `#![cfg(feature = "hls")]`).
It was relocated here when the HLS module moved from `tst-tcp` into `tst-hls`;
the feature gate was updated from `hls` → `serve` to match tst-hls's feature names.
