# ts-transformer

**Read, build, and transform MPEG-TS with typed MISB KLV metadata.** Mux and
demux transport streams, encode and decode ST 0601 telemetry, parse H.264 /
H.265 / H.266 / AV1 elementary streams — offline against files, or live. And
when you need a wire: stream over UDP, TCP / TLS, RTP (incl. RTSP), SRT, or
RIST, or publish HLS — with reconnect, encryption, and live stats built in.
Rust core; C, Python, and JVM bindings; `no_std` embedded support.

*The Swiss-Army knife for MPEG-TS + KLV streams.*

[![CI](https://github.com/aklofas/ts-transformer/actions/workflows/ci.yml/badge.svg)](https://github.com/aklofas/ts-transformer/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/tstrans.svg)](https://pypi.org/project/tstrans/)
[![Maven Central](https://img.shields.io/maven-central/v/org.tstrans/tstrans-jvm.svg)](https://central.sonatype.com/artifact/org.tstrans/tstrans-jvm)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV: 1.85](https://img.shields.io/badge/MSRV-1.85-blue.svg)](rust-toolchain.toml)

| Language | Status | Start here |
|---|---|---|
| **Rust** | Shipping | [`docs/languages/rust.md`](docs/languages/rust.md) |
| **C** | Shipping · ABI 0.21 | [`docs/languages/c.md`](docs/languages/c.md) |
| **Python** | Shipping · `tstrans` on PyPI | [`docs/languages/python.md`](docs/languages/python.md) |
| **JVM** | Shipping · `org.tstrans:tstrans-jvm` on Maven Central | [`docs/languages/jvm.md`](docs/languages/jvm.md) |
| **Embedded (bare-metal / RTOS)** | Shipping · QEMU-gated `no_std` core + C staticlib | [`docs/languages/embedded.md`](docs/languages/embedded.md) |

## See it in 30 seconds

Mux one H.264 access unit + one KLV blob into 188-byte MPEG-TS packets — no network, no files:

```rust
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default config: one program, H.264 video on PID 0x1011, KLV on 0x1031.
    let mut muxer = Muxer::new(MuxerConfig::default())?;

    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xA5, 0xA5, 0xA5];
    muxer.push_video(&nal, Pts90khz::new(0), /* key_frame */ true)?;
    muxer.push_klv(&[0x06, 0x0E, 0x2B, 0x34, 0xDE, 0xAD, 0xBE, 0xEF], Pts90khz::new(0), 0x00)?;

    let mut buf = [0u8; 1316];
    let n = muxer.pull(&mut buf);
    println!("muxed {n} bytes ({} TS packets)", n / 188);
    Ok(())
}
```

Feed those bytes to a `Demuxer` and typed events come back out:

```rust
use tst_core::mpegts::demux::Demuxer;

let mut demux = Demuxer::new();
demux.feed(&buf[..n])?;
while let Some(event) = demux.next_event() {
    println!("{event:?}"); // DemuxEvent::Video / Metadata (KLV) / Audio / …
}
```

Swap `Muxer` for `MuxSender` to push those packets straight onto an RTP / SRT /
RIST link, or `DemuxReceiver` to pull typed events off one. The same surface
ships in Python (`tstrans`), C (`tstrans.h`), and the JVM (`org.tstrans`).

**Want the full mux → SRT → demux round-trip?** [`docs/start/quickstart.md`](docs/start/quickstart.md) gets you there in 10 minutes.

## Scope

| | |
|---|---|
| **Container** | MPEG-TS (single- and multi-program; PAT / PMT / PCR auto-generated) |
| **Video** | H.264, H.265, H.266 / VVC, AV1 |
| **Audio** | AAC (ADTS + LATM), MPEG-2 Audio (MP2 / MP3), AC-3 |
| **Subtitles** | DVB subtitling, DVB teletext, CEA-708, WebVTT-in-TS |
| **Metadata** | MISB ST 0601 (FMV), ST 0102 (security), ST 0605 (amend tags), ST 0903 (VMTI), ST 1204 (Core ID), ST 0806 (RVT), ST 1010 (SDCC error covariance), ST 0805 (KLV→CoT conversion); ST 1402 carriage, H.222.0 §2.12.4.2 Metadata AU cells, ST 0604 MISP timestamps |
| **Transport** | UDP, raw TCP / TLS, RTP (incl. RTSP client + server), SRT 1.5 (Haivision libsrt, vendored), RIST (VideoLAN librist), and an HLS publisher (segmenter + optional built-in HTTP server) — all shipping. See the [HLS guide](docs/guides/hls.md). |
| **Encryption** | AES-128 / 192 / 256 over SRT via vendored mbedTLS 3.6 LTS, on by default |

Full feature-by-feature matrix: [`docs/reference/compatibility.md`](docs/reference/compatibility.md).

## Works with your stack

ts-transformer does the wire format; pair it with the tools you already use:

- **Encoders** — x264 / x265, FFmpeg, GStreamer, NVENC, or PyAV produce the
  encoded NAL units / OBUs you push into the muxer (there is no encoder or
  decoder inside).
- **Players** — VLC, mpv, ffplay, or any MPEG-TS-capable viewer plays the
  output directly; hls.js consumes the HLS publisher in a browser.
- **Repackagers** — MPEG-TS is the native container; hand the stream to FFmpeg
  or GStreamer downstream when delivery needs MP4 / fMP4 / DASH.
- **Servers** — it's a library, so it embeds in your own service; front it
  with a media server (e.g. MediaMTX) where you need RTMP / WebRTC endpoints.

## Install

`tstrans` is on **PyPI** and `org.tstrans:tstrans-jvm` is on **Maven Central**.
The Rust core and C bindings build from source:

```bash
git clone --recurse-submodules https://github.com/aklofas/ts-transformer.git
cd ts-transformer/ts-transformer

SRT_FORCE_VENDORED=1 cargo build --release            # Rust workspace
SRT_FORCE_VENDORED=1 cargo build --release -p tst-c   # C bindings → cdylib + staticlib + tstrans.h + tstrans.pc
```

The build vendors and compiles libsrt 1.5.7 + mbedTLS 3.6 LTS from submodules — ~3–5 minutes
cold, seconds warm. Feature flags (`mbedtls`, `file`, per-transport) are documented in
[`docs/languages/rust.md`](docs/languages/rust.md).

- **Python** — `pip install tstrans` (core) or `pip install tstrans[pandas]` (DataFrame + NumPy adapters). See [`docs/languages/python.md`](docs/languages/python.md).
- **JVM** — `org.tstrans:tstrans-jvm` on Maven Central. See [`docs/languages/jvm.md`](docs/languages/jvm.md).
- **Rust** — `cargo add tst-core` (plus `tst-pipeline` / `tst-srt` for the live pipeline) from crates.io.

## Documentation

→ **[`docs/index.md`](docs/index.md)** routes five reader audiences (cold-domain reader, evaluator,
language integrator, domain expert, binding author). Or jump straight in:

| If you are… | Start here |
|---|---|
| New to MPEG-TS / KLV / SRT | [`docs/start/concepts.md`](docs/start/concepts.md) — plain-language explainers |
| Evaluating the library | [`docs/start/overview.md`](docs/start/overview.md) — what it does, its scope boundaries, and what pairs with it |
| Writing your first code | [`docs/start/quickstart.md`](docs/start/quickstart.md) — working mux + demux in 10 minutes |
| Building something real | [`docs/cookbook/index.md`](docs/cookbook/index.md) — 40+ task-oriented recipes |
| Looking up a type or error | [`docs/reference/`](docs/reference/) — architecture, conventions, public-API policy |
| Checking what's safe to depend on | [`docs/reference/api-stability.md`](docs/reference/api-stability.md) — per-module Stable/Provisional/Internal tiers |
| Wrapping for a new language | [`docs/reference/binding-authors.md`](docs/reference/binding-authors.md) — ABI, error mapping, stability tiers |

## Validated against

The wire format is standards-conformant, and outputs are checked against **FFmpeg / ffprobe**,
**TSDuck**, **GStreamer**, **VLC**, **mpv**, and reference MISB tooling. Use any of them as the
encoder, decoder, validator, or viewer on either side of a ts-transformer pipeline.

A continuously-refreshed public evidence run exchanges real traffic with those same tools over
live SRT / RIST / UDP / TCP / HLS / RTSP sessions, plus a seeded impairment-soak harness (72-hour
run published: zero crashes, 12/12 outage reconnects, flat memory) — see
[`docs/project/validation-evidence.md`](docs/project/validation-evidence.md) for the current
census, methodology, and every documented gap.

## Status

Pre-1.0, released on PyPI (`tstrans`) and Maven Central (`org.tstrans:tstrans-jvm`);
C ABI **0.21**. Sender + receiver pipelines are complete, and **all four Tier-1
targets gate CI** — Linux x86_64, Linux aarch64, macOS arm64, and Windows MSVC.
Hundreds of tests run across both feature modes, with bash ratchets and
`cargo public-api` baselines guarding the surface on every commit.

Public API may change between pre-1.0 releases; everything is recorded in [`CHANGELOG.md`](CHANGELOG.md).

## Contributing

Issues and PRs welcome — start with [`CONTRIBUTING.md`](CONTRIBUTING.md)
(toolchain, the pre-push check set, the ratchets) and
[`docs/reference/conventions.md`](docs/reference/conventions.md) for code style.

Suspected security vulnerability? Please report it privately per
[`SECURITY.md`](SECURITY.md) instead of opening a public issue.

## License

Licensed under **MIT** ([`LICENSE-MIT`](LICENSE-MIT)) or **Apache-2.0** ([`LICENSE-APACHE`](LICENSE-APACHE)),
at your option.
