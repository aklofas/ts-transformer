# Open a sender from an `srt://...?...` URL

> **When to use this:** The connection target and tuning live in deployment config files or are passed in by an orchestrator.

> **Related:**
> - [guides/srt.md](/docs/guides/srt.md) — `SrtUrl`, `SocketConfig`, and how the URL's keys merge in
> - [Example: `sender_from_url`](/examples/sending/sender_from_url.rs)

Useful when the connection target and tuning live in deployment config
files (or are passed in by an orchestrator). Build a `SocketConfig`
from the parsed URL's keys, then connect.

```rust,no_run
use tst_srt::{SocketBuilder, SrtUrl};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = SrtUrl::parse(
        "srt://camera.local:9000?streamid=front&latency=200&passphrase=hunter-too-long",
    )?;
    let mut config = SocketBuilder::new().config();
    config.merge_from(&parsed.config);
    let _socket = tst_srt::Socket::connect_with(
        &config,
        format!("{}:{}", parsed.host, parsed.port).as_str(),
    )?;
    Ok(())
}
```

Runnable: [examples/sending/sender_from_url.rs](/examples/sending/sender_from_url.rs).
