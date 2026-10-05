# Getting Started


> **Who this is for:** You've installed ts-transformer (or are about to) and want a working sender + receiver in 10 minutes.

> **You will learn:**
> - How to add ts-transformer to a Rust project (C, Python and the JVM have their own language pages)
> - How to wire a sender + receiver over loopback SRT
> - How to mux H.264 + KLV and send it over SRT
> - How to record the received stream to a `.ts` file with the bundled example pair
> - Where to go next based on what you're building

When you want to send and receive bytes over SRT in 10 minutes, start here. The
goal of this page is to get a working program in front of you fast — for
background on how the pieces fit together, read
[concepts.md](/docs/start/concepts.md) or
[reference/architecture.md](/docs/reference/architecture.md) as a sibling read, not before
this one.

This guide walks through three runnable snippets in order: a raw send,
a raw receive, and a video-frame send through the pipeline shell. By
the end you'll have a sender and receiver talking over loopback and a
finished `.ts` file on disk.

## Prerequisites

- Rust 1.85+ via rustup. Check: `rustc --version`. The repo's
  `rust-toolchain.toml` pins to 1.85 for local development.
- C/C++ toolchain (`cmake`, `pkg-config`, `python3`, `build-essential`).
  Required because the SRT snippets below pull in `tst-srt`, which
  depends on `srt-sys` (published as `tstrans-srt-sys`) — it builds
  vendored libsrt and mbedTLS from source. `tst-core` alone is pure
  Rust and needs no C/C++ toolchain.
- Debian/Ubuntu:
  `sudo apt-get install -y build-essential cmake pkg-config python3`.
- macOS: `brew install cmake pkg-config` (`python3` is pre-installed).

## Get the code

```bash
git clone --recurse-submodules https://github.com/aklofas/ts-transformer.git
cd ts-transformer
```

If you cloned without `--recurse-submodules`:

```bash
git submodule update --init --recursive
```

The submodules are `crates/srt-sys/vendor/srt` (libsrt 1.5.7) and
`crates/mbedtls-src/vendor/mbedtls` (mbedTLS 3.6.x LTS). Both are required
for the default build.

## Add it to your project

All `ts-transformer` crates are published on crates.io:

```toml
[dependencies]
tst-core = "0.7"
```

Note on cold builds: `tst-core` itself is pure Rust — it has no
native dependencies and builds in seconds. It's adding `tst-srt` (or
`tst-rist`) that compiles the vendored libsrt (librist for `tst-rist`)
+ mbedTLS from source on the first build (~3-5 minutes; warm builds are seconds). Building
`tst-srt` with `--no-default-features` skips the mbedTLS build for
faster iteration (this also disables encryption — only do this for
testing).

## Send your first packet

```rust
use tst_srt::SocketBuilder;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bind the builder before chaining: mutators take `&mut self` and
    // return `&mut Self`, while `connect` takes `&self`, so a single
    // fluent chain off the temporary `SocketBuilder::new()` would dangle.
    let mut sb = SocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let mut socket = sb.connect("127.0.0.1:9000")?;
    socket.send(b"hello, srt")?;
    // The receiver holds each message for its 120 ms latency before
    // delivering it, and closing ends the connection with whatever it
    // still holds (`linger` only waits for the ACK), so pause first.
    std::thread::sleep(Duration::from_millis(500));
    socket.close()?;
    Ok(())
}
```

Run with `cargo run`. The 120 ms latency is the conventional starting
point for live SRT; the handshake settles on the larger of the two
peers' latencies. `connect` blocks
until the SRT handshake completes; `send` blocks until the message is
queued for the wire.

## Receive your first packet

```rust
use tst_srt::ListenerBuilder;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Same bind-then-step pattern as `SocketBuilder`: mutators return
    // `&mut Self`, terminal `bind` takes `&self`.
    let mut lb = ListenerBuilder::new();
    lb.latency(Duration::from_millis(120));
    let mut listener = lb.bind("0.0.0.0:9000")?;
    let (mut socket, peer) = listener.accept()?;
    println!("accepted from {peer}");
    let mut buf = [0u8; 1500];
    loop {
        match socket.recv(&mut buf) {
            Ok(n) => println!("recv {n} bytes: {:?}", &buf[..n.min(20)]),
            Err(tst_srt::error::RecvError::ConnectionBroken) => break,
            Err(e) => return Err(Box::new(e)),
        }
    }
    Ok(())
}
```

Run the receiver in one terminal, then the sender from the previous
section in another. The receiver prints
`recv 10 bytes: [104, 101, 108, 108, 111, 44, 32, 115, 114, 116]` and
exits cleanly when the peer closes. The 1500-byte buffer is comfortably above the default
SRT payload size (1316 bytes), so each `recv` returns one whole
message.

## Send a video frame

Switch from raw bytes to a typed video sender:

```rust
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_srt::{SocketBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bind-then-step: builder mutators return `&mut Self`, `connect`
    // takes `&self`, so split the chain across statements on a bound
    // builder.
    let mut sb = SocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let socket = sb.connect("127.0.0.1:9000")?;
    let transport = SrtTransport::new(socket);
    let sender = MuxSender::new(transport, MuxerConfig::default())?;

    // Synthetic Annex-B IDR access unit + KLV blob.
    let nal = vec![0x00, 0x00, 0x00, 0x01, 0x65, /* ... payload bytes ... */];
    let klv = vec![0x06, 0x0E, 0x2B, 0x34, /* ... ST 0601 record ... */];
    sender.send_video(&nal, /*pts=*/ Pts90khz::new(0), /*key_frame=*/ true)?;
    // metadata_service_id = 0x00 is the default per ST 1402.2 App. B Table 2.
    sender.send_klv(&klv, /*pts=*/ Pts90khz::new(0), /*metadata_service_id=*/ 0x00)?;

    sender.close();
    Ok(())
}
```

`MuxSender` wraps an `mpegts::mux::Muxer` and an `SrtTransport`. It
auto-mux's NAL units and KLV blobs into a single MPEG-TS stream and
sends each TS chunk over SRT. `pts` is in 90 kHz ticks (the TS
clock); `key_frame` should be true for IDR frames.

In production, replace the synthetic generator with your encoder's
output. See
[../examples/sending/send_pipeline_to_socket.rs](/examples/sending/send_pipeline_to_socket.rs)
for a runnable version with five frames and pacing.

## Run the example pair

The fastest way to see end-to-end behavior is the bundled example
pair:

```bash
# terminal A
cargo run -p tst-examples --example srt_listener_to_file -- 127.0.0.1:9000 /tmp/out.ts
# terminal B
cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
```

The receiver writes incoming bytes to `/tmp/out.ts`. After the sender
exits, `file /tmp/out.ts` reports `MPEG transport stream data`. The
sender produces five synthetic frames plus matching KLV records at
roughly 30 fps, then closes; the receiver drains until the connection
is broken and exits cleanly.

## Seeing what's happening — wiring `tracing-subscriber`

`ts-transformer` emits `tracing` events on every pipeline shell
open/close, on each reconnect attempt, on back-pressure threshold
crossings, and on forwarded libsrt log lines. To see them, add
`tracing-subscriber` and wire it once at startup:

```toml
[dependencies]
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

```rust
use tracing_subscriber::{fmt, EnvFilter};

fn main() {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())  // honors RUST_LOG
        .init();

    // ... rest of your program ...
}
```

Then run with a `RUST_LOG` filter that picks the targets you want:

```bash
RUST_LOG=tst_pipeline=info,srt=warn cargo run -p tst-examples --example mux_h265_with_klv
```

Useful filter targets:

| Target                          | What it covers                                              |
|---------------------------------|-------------------------------------------------------------|
| `tst_pipeline::mux_sender`      | MuxSender lifecycle + back-pressure threshold warns         |
| `tst_pipeline::sender`          | Sender lifecycle                                            |
| `tst_pipeline::raw_sender`      | RawSender lifecycle                                         |
| `tst_pipeline::demux_receiver`  | DemuxReceiver lifecycle                                     |
| `tst_pipeline::receiver`        | Receiver lifecycle                                          |
| `tst_pipeline::raw_receiver`    | RawReceiver lifecycle                                       |
| `tst_pipeline::reconnect`       | Sender-side managed-transport reconnect attempts + give-up  |
| `tst_pipeline::managed_receive` | Receiver-side managed-transport reconnect attempts          |
| `srt`                           | libsrt-internal logs (forwarded from the C library)         |
| `tst_core::codec`               | Codec parser warnings (e.g., H.265 SPS parse failures)      |

## See also

- **Runnable example:** `cargo run -p tst-examples --example hello_world` — [examples/getting-started/hello_world.rs](/examples/getting-started/hello_world.rs)
- [start/concepts.md](/docs/start/concepts.md) — MPEG-TS, KLV, and SRT in plain terms.
- [reference/architecture.md](/docs/reference/architecture.md) — how the crates compose.

## Where to go next

- [reference/architecture.md](/docs/reference/architecture.md) — how the pieces fit together.
- [guides/srt.md](/docs/guides/srt.md) — `Socket`, `Listener`, encryption,
  latency, stats.
- [guides/klv.md](/docs/guides/klv.md) — encoding and decoding ST 0601 KLV.
- [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) — the TS muxer's knobs.
- [guides/pipeline.md](/docs/guides/pipeline.md) — picking among `MuxSender`,
  `Sender`, and `RawSender`.
- [cookbook/index.md](/docs/cookbook/index.md) — recipes for common multi-step tasks.
- [troubleshooting.md](/docs/troubleshooting.md) — common failure modes.
- [reference/compatibility.md](/docs/reference/compatibility.md) — feature-by-feature support
  matrix.
