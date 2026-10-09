# Getting Started

> **Who this is for:** You write Rust and want to send and receive data over a local SRT connection. For other languages, start with the [C](/docs/languages/c.md), [Python](/docs/languages/python.md), or [JVM](/docs/languages/jvm.md) guide.

> **You will learn:**
> - How to add ts-transformer to a Rust project (C, Python and the JVM have their own language pages)
> - How to wire a sender + receiver over loopback SRT
> - How to mux H.264 + KLV and send it over SRT
> - How to record the received stream to a `.ts` file with the bundled example pair
> - Where to go next based on what you're building

First, send a text message between two programs on your machine. Then
replace the sender with one that builds MPEG-TS from video and metadata.
Finally, use the repository's examples to record a stream to a `.ts` file.
For background on the terms, see [concepts](/docs/start/concepts.md).

These examples use an unencrypted loopback connection. See the
[encrypted-send recipe](/docs/cookbook/sending/send-encrypted.md) when you
are ready to configure a passphrase on both peers.

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

Clone the repository to use its bundled examples in
[Run the example pair](#run-the-example-pair). You can skip this step while
working through the standalone sender and receiver below.

```bash
git clone --recurse-submodules https://github.com/aklofas/ts-transformer.git
cd ts-transformer
```

If you cloned without `--recurse-submodules`:

```bash
git submodule update --init --recursive
```

The SRT examples need the native sources in `crates/srt-sys/vendor/srt`
(libsrt) and `crates/mbedtls-src/vendor/mbedtls` (mbedTLS). The recursive
clone initializes these along with the repository's other submodules.

## Add it to your project

In a directory outside the repository, create a small application:

```bash
cargo new srt-hello
cd srt-hello
mkdir -p src/bin
```

Add these crates under the existing `[dependencies]` heading in `Cargo.toml`:

```toml
[dependencies]
tst-core = "0.7"      # MPEG-TS and KLV
tst-pipeline = "0.7"  # Combines the muxer with a transport
tst-srt = "0.7"       # SRT sockets
```

The raw socket examples use `tst-srt`; the video example also uses
`tst-core` and `tst-pipeline`. A first build can take several minutes while
the native SRT dependencies compile. Declaring
`tst-srt = { version = "0.7", default-features = false }` skips the mbedTLS
build for faster iteration; it also disables encryption, so use it only for
testing (inside this repository, `--no-default-features` on `tst-srt` does
the same). For offline MPEG-TS or KLV processing, `tst-core` alone is enough
and requires no C/C++ toolchain.

## Send your first packet

Save this as `src/bin/send.rs`. Create the receiver in the next section
before running it.

```rust
use tst_srt::SocketBuilder;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure the socket, then connect to the local receiver.
    let mut sb = SocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let mut socket = sb.connect("127.0.0.1:9000")?;
    socket.send(b"hello, srt")?;
    // The receiver holds each message for its 120 ms latency before
    // delivering it, and closing ends the connection with whatever it
    // still holds (`linger` only waits for the ACK), so pause first.
    std::thread::sleep(Duration::from_millis(500));
    socket.close();
    Ok(())
}
```

The 120 ms setting gives SRT time to recover missing packets before
delivery; the handshake settles on the larger of the two peers'
latencies. `connect` waits for the handshake; `send` queues the message.
The pause is sufficient for this local demonstration, but is not an
acknowledgment that a receiving application has processed the data.

## Receive your first packet

Save this as `src/bin/receive.rs`:

```rust
use tst_srt::ListenerBuilder;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Listen locally and wait for one sender to connect.
    let mut lb = ListenerBuilder::new();
    lb.latency(Duration::from_millis(120));
    let mut listener = lb.bind("127.0.0.1:9000")?;
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

From the `srt-hello` directory, start the receiver in terminal A:

```bash
cargo run --bin receive
```

It waits silently in `accept()` until a sender connects. In terminal B,
also in `srt-hello`, run:

```bash
cargo run --bin send
```

The receiver prints an `accepted from ...` line followed by
`recv 10 bytes: [104, 101, 108, 108, 111, 44, 32, 115, 114, 116]` and
exits cleanly when the peer closes. The 1500-byte buffer is comfortably above the default
SRT payload size (1316 bytes), so each `recv` returns one whole
message.

## Send a video frame

Now use `MuxSender` to combine video and KLV metadata into MPEG-TS before
sending it. Replace `src/bin/send.rs` with the code below, then start the
receiver and sender again using the same commands.

The video bytes are a synthetic test payload, not a decodable picture.
They demonstrate muxing without requiring an encoder. The KLV record is
encoded from typed values.

```rust
use tst_core::klv::st0601::{UasDatalinkLs, encode_to_vec};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_srt::{SocketBuilder, SrtTransport};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sb = SocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let socket = sb.connect("127.0.0.1:9000")?;
    let transport = SrtTransport::new(socket);
    let sender = MuxSender::new(transport, MuxerConfig::default())?;

    // Annex-B start code, IDR NAL header, and test bytes.
    let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xA5, 0xA5, 0xA5];
    let record = UasDatalinkLs {
        timestamp_us: Some(0), // Fixed test timestamp, not the current time.
        ..UasDatalinkLs::default()
    };
    let klv = encode_to_vec(&record)?;
    sender.send_video(&nal, /*pts=*/ Pts90khz::new(0), /*key_frame=*/ true)?;
    // The default mux config carries KLV asynchronously, without a KLV PTS.
    sender.send_klv(&klv, /*pts=*/ Pts90khz::new(0), /*metadata_service_id=*/ 0x00)?;

    // Allow delivery in this short demo before finishing the stream.
    std::thread::sleep(Duration::from_millis(500));
    sender.finish()?;
    Ok(())
}
```

`MuxSender` combines a `Muxer` with an `SrtTransport`: each `send_video`
or `send_klv` call builds TS packets and sends them. The receiver still
prints raw bytes; it does not decode the video or KLV.

`pts` uses 90 kHz ticks: 90,000 ticks is one second. Pass a complete encoded
video access unit per call, and mark IDR frames with `key_frame: true`.
The default configuration carries asynchronous KLV, so the KLV PTS is not
written into the stream. For frame-aligned metadata, configure
[synchronous KLV](/docs/guides/mpegts-mux.md).

In an application, replace the test bytes with your encoder's output and
populate the KLV fields from your metadata source.

## Run the example pair

To record received MPEG-TS to disk, run this pair from the repository
checkout created in [Get the code](#get-the-code), in two terminals:

```bash
# terminal A
cargo run -p tst-examples --example srt_listener_to_file -- 127.0.0.1:9000 /tmp/out.ts
# terminal B
cargo run -p tst-examples --example send_pipeline_to_socket -- 127.0.0.1:9000
```

The receiver writes incoming bytes to `/tmp/out.ts` and reports the byte
count when the connection ends. The sender produces five synthetic video
payloads and KLV-shaped test records, with a short pause between sends.
This checks the muxing, transport, and file-writing path; the capture is
not playable video or a source of valid telemetry. The example sources are
[`srt_listener_to_file.rs`](/examples/receiving/srt_listener_to_file.rs) and
[`send_pipeline_to_socket.rs`](/examples/sending/send_pipeline_to_socket.rs).

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
