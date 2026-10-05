# Receive into a file

> **When to use this:** Archiving a stream or building a test fixture from a live producer.

> **Related:**
> - [guides/srt.md](/docs/guides/srt.md) — `ListenerBuilder`, `accept`, and the recv error variants
> - [Example: `srt_listener_to_file`](/examples/receiving/srt_listener_to_file.rs)

Reach for this when archiving a stream or building a test fixture from a live producer. `Listener::accept` returns a connected `Socket`; the recv loop drains until `ConnectionBroken`.

A 1500-byte buffer comfortably fits SRT's default 1316-byte payload, so each `recv` returns one whole message. The match handles data, the end of the connection (`ConnectionBroken`), and a defensive timeout.

```rust,no_run
use tst_srt::ListenerBuilder;
use std::fs::File;
use std::io::Write;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Same bind-then-step pattern as SocketBuilder: mutators return
    // `&mut Self`, terminal `bind` takes `&self`.
    let mut lb = ListenerBuilder::new();
    lb.latency(Duration::from_millis(120));
    let mut listener = lb.bind("0.0.0.0:9000")?;
    let (mut socket, _peer) = listener.accept()?;
    let mut out = File::create("out.ts")?;
    let mut buf = [0u8; 1500];
    loop {
        match socket.recv(&mut buf) {
            Ok(n) => out.write_all(&buf[..n])?,
            Err(tst_srt::error::RecvError::ConnectionBroken) => break,
            Err(tst_srt::error::RecvError::TimedOut) => continue,
            Err(e) => return Err(Box::new(e)),
        }
    }
    Ok(())
}
```

Runnable: [examples/receiving/srt_listener_to_file.rs](/examples/receiving/srt_listener_to_file.rs).
