# Graceful shutdown from another thread via `ResourceCancel`

> **When to use this:** The main thread is parked in `send_*` / `recv_*` and a sibling thread (signal-dispatch thread, watchdog, FFI lifecycle observer) needs to wake it.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `cancel_handle()` on every long-lived shell
> - [`reference/srt-cancel-handle.md`](/docs/reference/srt-cancel-handle.md) — full pattern + per-language idiom table

Use case: the main thread is parked in `send_*` / `recv_*`, and a
sibling thread (signal-dispatch thread, lifecycle observer, parent-process
watchdog, FFI consumer holding a Kotlin `Job`) needs to wake it so the
process can shut down promptly. Time-sliced polling — `recv_with_timeout`
in a loop — is the wrong shape; the cancel handle is.

Every long-lived shell exposes `cancel_handle() -> Option<Arc<dyn
TransportCancel + Send + Sync>>`. The handle is `Send + Sync`,
one-shot, and idempotent — clone it freely, fire it from any thread.
On SRT the parked call returns within one libsrt I/O cycle (typically
3–10 ms). UDP and RIST observe the cancel at their next 100 ms poll
tick; see the
[coverage table](/docs/reference/srt-cancel-handle.md#cancel-coverage-by-transport)
for every transport.

```rust,no_run
use tst_pipeline::{Sender, SenderConfig, ShellErrorKind, TransportCancel};
use tst_srt::{SrtSocketBuilder, SrtTransport};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = SrtSocketBuilder::new().connect("127.0.0.1:9000")?;
    let mut sender = Sender::new(SrtTransport::new(socket), SenderConfig::default());

    // Snapshot the cancel handle BEFORE the send loop. After this point
    // the handle is owned by the cancel thread; the sender keeps running
    // on the main thread until cancel() fires.
    // `None` only for a transport without cancel support (e.g. a bare
    // in-memory test sink); SrtTransport always returns `Some`.
    let cancel: Arc<dyn TransportCancel + Send + Sync> = sender
        .cancel_handle()
        .expect("SrtTransport supports cancel");

    // Cancel thread: in a real program this is the thread your signal
    // handling runs on (e.g. the `ctrlc` crate's handler thread or a
    // `signal-hook` iterator; never a raw signal handler), a watchdog timer,
    // or a JNI/PyO3 entry point firing on FFI lifecycle events.
    let cancel_clone = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(30));
        cancel_clone.cancel(); // wakes any parked send_ts on the main thread
    });

    // Main thread: send valid TS until cancel() fires. A null packet
    // (PID 0x1FFF) stands in for real content; the sender's framer needs
    // 0x47 sync bytes, and bytes without them trip its unsynced-bytes
    // watchdog (`NoSyncAfterLimit`) long before the cancel.
    let mut pkt = [0xFFu8; 188];
    pkt[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
    loop {
        match sender.send_ts(&pkt) {
            Ok(()) => continue,
            // A cancel surfaces as `TransportError::ExplicitClose`, which
            // the sender reports with kind `Closed`.
            Err(e) if e.kind == ShellErrorKind::Closed => break,
            // Anything else is a real failure, not the shutdown.
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
```

See [`srt-cancel-handle.md`](/docs/reference/srt-cancel-handle.md) for the full pattern,
threading guarantees, and per-language idiom table (Java/Kotlin,
Swift, Python, C). No standalone example.
