# Use a custom (non-SRT) transport

> **When to use this:** The sender shells fit but the wire isn't SRT — UDP, file, in-memory test harness, your own protocol.

> **Related:**
> - [guides/pipeline.md](/docs/guides/pipeline.md) — the `Transport` trait contract
> - [Example: `custom_transport`](/examples/sending/custom_transport.rs)

Reach for this when the sender shells fit but the wire isn't SRT — UDP, file, in-memory test harness, your own protocol. `MuxSender`, `Sender`, and `RawSender` are all generic over `T: Transport`; implement the trait once and they all compose.

The trait has four required methods: `send_bytes`, `max_payload`, `is_alive`, `close` (`cancel_handle` and `socket_stats` default to `None`). Your impl needs to be `Send`, not `Sync` — the shells handle internal synchronization where required.

```rust,no_run
use tst_pipeline::{Transport, TransportError};
use std::sync::{Arc, Mutex};

struct MemTransport {
    packets: Arc<Mutex<Vec<Vec<u8>>>>,
    alive: bool,
    max_payload: usize,
}

impl Transport for MemTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge { len: msg.len(), max: self.max_payload });
        }
        if !self.alive { return Err(TransportError::Closed); }
        self.packets.lock().unwrap().push(msg.to_vec());
        Ok(())
    }
    fn max_payload(&self) -> usize { self.max_payload }
    fn is_alive(&self) -> bool { self.alive }
    fn close(&mut self) { self.alive = false; }
}
```

Runnable: [examples/sending/custom_transport.rs](/examples/sending/custom_transport.rs).
