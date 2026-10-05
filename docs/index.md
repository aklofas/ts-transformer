# ts-transformer

ts-transformer combines encoded video, audio, and KLV metadata into MPEG-TS
streams, and extracts them again. Use it to process `.ts` files, relay live
streams, or pair video with telemetry. It provides a Rust core with C,
Python, and JVM bindings.

The library handles the container, metadata, and network transport. Your
application supplies the video encoder or decoder. If these terms are new,
start with [MPEG-TS, KLV, and SRT in plain terms](/docs/start/concepts.md).

## Pick your starting point

| What you want to do | Start here | Then read |
|---|---|---|
| Learn the domain | [Concepts](/docs/start/concepts.md) | [Overview](/docs/start/overview.md), then the [Rust quickstart](/docs/start/quickstart.md) or your [language guide](#which-language-should-i-pick) |
| Evaluate the library | [Overview](/docs/start/overview.md) | [Feature matrix](/docs/reference/compatibility.md), [validation evidence](/docs/project/validation-evidence.md), [unsupported features](/docs/project/deferred-features.md) |
| Add it to your application | [Choose a language](#which-language-should-i-pick) | Installation and first examples in that language's guide |
| Build a specific workflow | [Cookbook](/docs/cookbook/index.md) | [Muxing](/docs/guides/mpegts-mux.md), [demuxing](/docs/guides/mpegts-demux.md), [KLV](/docs/guides/klv.md), or [HLS](/docs/guides/hls.md) |
| Understand an API or diagnose a failure | [Architecture](/docs/reference/architecture.md) or [troubleshooting](/docs/troubleshooting.md) | [API stability](/docs/reference/api-stability.md), [STANAG 4609 / MISP conformance](/docs/reference/stanag-4609.md) |
| Write a binding or contribute | [Binding-authors guide](/docs/reference/binding-authors.md) | [Public API policy](/docs/reference/public-api.md), [conventions](/docs/reference/conventions.md), [SRT cancellation](/docs/reference/srt-cancel-handle.md) |

For deployment planning, see [code size](/docs/project/code-size.md) and
[benchmarks and capacity testing](/docs/project/benchmarks.md). Project
policies cover [licensing](/README.md#license),
[security](/SECURITY.md), and [releasing](/docs/project/releasing.md).

## Which language should I pick?

| Language | Surface | When to pick |
|---|---|---|
| **[Rust](/docs/languages/rust.md)** | Mux/demux, typed KLV, all transports, and low-level APIs | Rust applications; direct access to the core |
| **[C](/docs/languages/c.md)** | Mux/demux and transport APIs through `tstrans.h`; transports are build-time options | C/C++ applications or another language's native interface |
| **[Python](/docs/languages/python.md)** | File processing, typed KLV, pandas adapters, live transports, HLS publishing, and pairing | Notebooks, data analysis, and streaming applications |
| **[JVM](/docs/languages/jvm.md)** | Mux/demux, typed KLV, SRT, RTP/RTSP, and pairing; no UDP, TCP, RIST, or HLS binding | Java, Kotlin, Scala, or other JVM applications |
| **[Embedded](/docs/languages/embedded.md)** | `no_std` Rust core, offline C static library, and a FreeRTOS SRT reference port | Bare-metal or RTOS firmware |

## What kind of pages live here?

Choose a page by the amount of guidance you need:

- **Follow a first example:** [Quickstart](/docs/start/quickstart.md).
- **Solve a specific problem:** [Cookbook](/docs/cookbook/index.md).
- **Look up details:** [Reference](/docs/reference/).
- **Understand how it works:** [Concepts](/docs/start/concepts.md), followed by the topic [guides](/docs/guides/).
