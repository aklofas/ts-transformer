# Graceful shutdown from another thread via `SrtCancelHandle`

> **When to use this:** The main thread is parked in `send_*` / `recv_*` and a sibling thread (signal handler, watchdog, FFI lifecycle observer) needs to wake it.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — `cancel_handle()` on every long-lived shell
> - [`reference/srt-cancel-handle.md`](/docs/reference/srt-cancel-handle.md) — full pattern + per-language idiom table

Use case: the main thread is parked in `send_*` / `recv_*`, and a
sibling thread (signal handler, lifecycle observer, parent-process
watchdog, FFI consumer holding a Kotlin `Job`) needs to wake it so the
process can shut down promptly. Time-sliced polling — `recv_with_timeout`
in a loop — is the wrong shape; the cancel handle is.

Every long-lived shell exposes `cancel_handle() -> Option<Arc<dyn
TransportCancel + Send + Sync>>`. The handle is `Send + Sync`,
one-shot, and idempotent — clone it freely, fire it from any thread.
The parked syscall returns within one libsrt I/O cycle (typically
3–10 ms).

```rust,no_run
use tst_pipeline::{Sender, SenderConfig, TransportCancel};
use tst_core::transport::{Transport, TransportError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

# struct Sink;
# impl Transport for Sink {
#     fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> { Ok(()) }
#     fn max_payload(&self) -> usize { 1316 }
#     fn close(&mut self) {}
#     fn is_alive(&self) -> bool { true }
# }
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sender = Sender::new(Sink, SenderConfig::default());

    // Snapshot the cancel handle BEFORE the send loop. After this point
    // the handle is owned by the cancel thread; the sender keeps running
    // on the main thread until cancel() fires.
    let cancel: Arc<dyn TransportCancel + Send + Sync> = sender
        .cancel_handle()
        .expect("real transports return Some");

    // Cancel thread: in a real program this is your signal handler
    // (e.g. via the `ctrlc` crate or `signal-hook`), a watchdog timer,
    // or a JNI/PyO3 entry point firing on FFI lifecycle events.
    let cancel_clone = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(30));
        cancel_clone.cancel(); // wakes any parked send_ts on the main thread
    });

    // Main thread: parked send returns Err once cancel() fires.
    let pkt = vec![0u8; 188];
    loop {
        match sender.send_ts(&pkt) {
            Ok(()) => continue,
            Err(_e) => break, // cancellation surfaces as TransportError::ExplicitClose
        }
    }
    Ok(())
}
```

See [`srt-cancel-handle.md`](/docs/reference/srt-cancel-handle.md) for the full pattern,
threading guarantees, and per-language idiom table (Java/Kotlin,
Swift, Python, C). No standalone example; the snippet above runs as
a doctest.
