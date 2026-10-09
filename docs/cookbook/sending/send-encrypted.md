# Send video + KLV with passphrase encryption

> **When to use this:** You need a secure SRT uplink with passphrase-derived AES-CTR encryption negotiated at handshake.

> **Related:**
> - [guides/srt.md](/docs/guides/srt.md) — `Passphrase`, `KeyLength`, and the SRT handshake
> - [Example: `encrypted_send_recv`](/examples/sending/encrypted_send_recv.rs)

Reach for this when you need a secure uplink. SRT's encryption is AES-CTR with a passphrase-derived key, negotiated during the handshake; both peers must agree on the same passphrase and key length.

The diff against an unencrypted setup is small: `passphrase(...)` plus `key_length(...)` on both the `SocketBuilder` and the `ListenerBuilder`. `Passphrase::new` validates length (10–79 ASCII-printable bytes, libsrt's own constraint).

```rust,no_run
use tst_srt::{KeyLength, Passphrase, SocketBuilder};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let passphrase = Passphrase::new("shared-secret-not-for-production")?;
    // Mutators return `&mut Self` and `connect` takes `&self`: a
    // one-statement chain off `SocketBuilder::new()` also works, but a
    // bound builder can be stepped and reused.
    let mut sb = SocketBuilder::new();
    sb.passphrase(passphrase);
    sb.key_length(KeyLength::Aes256);
    sb.latency(Duration::from_millis(120));
    let mut socket = sb.connect("127.0.0.1:9000")?;
    socket.send(b"encrypted hello")?;
    socket.close();
    Ok(())
}
```

Runnable: [examples/sending/encrypted_send_recv.rs](/examples/sending/encrypted_send_recv.rs).
