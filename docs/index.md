# ts-transformer

> ts-transformer reads, builds, and transforms MPEG-TS — mux + demux, typed
> MISB ST 0601 KLV encode/decode, H.264 / H.265 / H.266 / AV1 parsing — and,
> when you need a wire, streams it over UDP, TCP, RTP, SRT, or RIST (or
> publishes HLS) with reconnect, encryption, and typed metadata decoding
> handled. From a `.ts` file on disk to a live encrypted uplink in ~30 lines.
>
> Rust core. C, Python, and JVM bindings.

> 🆕  **First time touching MPEG-TS, KLV, or SRT?**
> Read [start/concepts.md](/docs/start/concepts.md) first — five minutes of plain-language explainers before anything else.

## Pick your starting point

| | | |
|---|---|---|
| **🆕 New to this domain** <br><br> Plain-English explainers before you read any API. <br><br> → [What is this?](/docs/start/overview.md) <br> → [MPEG-TS / KLV / SRT in 5 min](/docs/start/concepts.md) <br> → [Quickstart](/docs/start/quickstart.md) | **🔍 Evaluating the library** <br><br> "Is this what I need?" <br><br> → [Overview](/docs/start/overview.md) <br> → [Feature matrix](/docs/reference/compatibility.md) <br> → [Interop + soak evidence](/docs/project/validation-evidence.md) <br> → [What's not yet supported](/docs/project/deferred-features.md) <br> → [License: MIT or Apache-2.0](/README.md#license) | **⚡ Pick your language** <br><br> Drop-in for your app. <br><br> → [Rust](/docs/languages/rust.md) <br> → [C](/docs/languages/c.md) <br> → [Python](/docs/languages/python.md) <br> → [JVM](/docs/languages/jvm.md) <br> → [Embedded (no_std)](/docs/languages/embedded.md) <br> → [Decision table](#which-language-should-i-pick) (below) |
| **🔧 Build something real** <br><br> Deep guides and recipes. <br><br> → [Mux MPEG-TS](/docs/guides/mpegts-mux.md) <br> → [Demux MPEG-TS](/docs/guides/mpegts-demux.md) <br> → [KLV](/docs/guides/klv.md) <br> → [HLS](/docs/guides/hls.md) <br> → [Cookbook (40+ recipes)](/docs/cookbook/index.md) | **📚 Look up a type or error** <br><br> Reference and API lookup. <br><br> → [Architecture](/docs/reference/architecture.md) <br> → [Public API policy](/docs/reference/public-api.md) <br> → [API stability tiers](/docs/reference/api-stability.md) <br> → [Conventions](/docs/reference/conventions.md) <br> → [STANAG 4609 / MISP conformance](/docs/reference/stanag-4609.md) <br> → [Troubleshooting](/docs/troubleshooting.md) | **🧩 Port a binding / contribute** <br><br> Wrap ts-transformer for a new language. <br><br> → [Binding-authors guide](/docs/reference/binding-authors.md) <br> → [Public API policy](/docs/reference/public-api.md) <br> → [SRT cancel-handle](/docs/reference/srt-cancel-handle.md) <br> → [Security policy](/SECURITY.md) |

## Which language should I pick?

| Language | Surface | When to pick |
|---|---|---|
| **Rust** | Full `Sender` / `Receiver` + low-level primitives | Embedding in a Rust app; want type-level guarantees |
| **C** | Full sender + receiver surface (`cdylib` + `staticlib` + `tstrans.h`) | Embedded targets; cross-language linkage; maximum ABI stability |
| **Python** | Offline `.ts` inspect/build, typed KLV decode/encode, DataFrame adapters, **and** live UDP / TCP / RTP (incl. RTSP) / SRT / RIST transports + Pairer | Notebooks; KLV-to-DataFrame ETL; offline processing; live ingest/egress |
| **JVM** (`tstrans-jvm` / `org.tstrans` on Maven Central) | Mirrors the Python surface: mux + demux, typed KLV, RTP (incl. RTSP) + SRT transports, pairing | JVM backend consumers |
| **Embedded** (bare-metal / RTOS) | `no_std` Rust core + C staticlib + FreeRTOS/lwIP/libsrt reference port, all QEMU-gated | No-OS or RTOS firmware; MCU with no host runtime |

## What kind of pages live here?

This site organizes content four ways. Knowing which kind you're reading helps you spot what you need:

- **Tutorials** — guided, end-to-end. Start at [`start/quickstart.md`](/docs/start/quickstart.md).
- **How-to guides** — recipes for specific problems. See the [Cookbook](/docs/cookbook/index.md).
- **Reference** — information lookup, structured. Lives under [`reference/`](/docs/reference/).
- **Concepts** — explanations of the domain. Start with [`start/concepts.md`](/docs/start/concepts.md), then deep-dive in [`guides/`](/docs/guides/).

(This is the [Diátaxis](https://diataxis.fr/) framework. You don't have to learn it — just notice that the page you're on is one of those four.)
