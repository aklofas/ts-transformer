# Relay a captured `.ts` file over SRT

> **When to use this:** You have a `.ts` capture you want to replay over SRT — regression-testing receivers, rebroadcasting an archive, exercising a downstream pipeline.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `Sender<T>` and the byte-stream-oriented send path
> - [Example: `ts_relay_from_file`](/examples/receiving/ts_relay_from_file.rs)

Reach for this when you have a `.ts` capture you want to replay over SRT — regression-testing receivers, rebroadcasting an archive, exercising a downstream pipeline. `Sender` accepts arbitrary byte chunks, verifies TS sync, and emits 7-packet (1316-byte) bundles to the wrapped transport.

The sender is byte-stream oriented — file reads of any size are fine, the sender handles 188-alignment and bundling internally. `flush()` emits any buffered partial bundle so the tail of a finite input reaches the wire.

```rust,no_run
use tst_pipeline::{Sender, SenderConfig};
use tst_srt::{SocketBuilder, SrtTransport};
use std::fs::File;
use std::io::Read;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Mutators take `&mut self` and `connect` takes `&self`: a one-statement
    // chain off `SocketBuilder::new()` also works, but binding the builder
    // first lets you set options step by step and reuse it.
    let mut sb = SocketBuilder::new();
    sb.latency(Duration::from_millis(120));
    let socket = sb.connect("127.0.0.1:9000")?;
    let mut sender = Sender::new(SrtTransport::new(socket), SenderConfig::default());
    let mut file = File::open("input.ts")?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        sender.send_ts(&buf[..n])?;
    }
    sender.flush()?;
    sender.close();
    Ok(())
}
```

Runnable: [examples/receiving/ts_relay_from_file.rs](/examples/receiving/ts_relay_from_file.rs).
