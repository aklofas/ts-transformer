# Receive MPEG-TS over RIST

> **When to use this:** Receiver side of RIST — bind URL form (`rist://@host:port`) with optional AES PSK decryption.

> **Related:**
> - [Send MPEG-TS over RIST](/docs/cookbook/sending/rist.md)
> - [Receive MPEG-TS over UDP](/docs/cookbook/receiving/udp.md) — lower-level alternative without recovery

Companion to [Send MPEG-TS over RIST](/docs/cookbook/sending/rist.md). Binds a RIST receiver
using the ffmpeg `@`-prefix convention for "listen on this address."

## Code

```rust
use tst_pipeline::DemuxReceiver;
use tst_rist::{RistProfile, RistRecvTransportBuilder};
use std::time::Duration;

// Simple Profile receiver MUST use an EVEN port (librist uses port + port+1
// for RTP + RTCP). Main Profile multiplexes RTCP into the same socket so
// any port works.
let rx = RistRecvTransportBuilder::new("rist://@0.0.0.0:9000")?
    .profile(RistProfile::Simple)
    .buffer(Duration::from_millis(200))
    .listen()?;
let mut receiver = DemuxReceiver::new(rx);
for event in &mut receiver {
    let event = event?;
    // event is a DemuxEvent — ProgramMap, Sample (video/audio), Metadata (KLV), ...
}
```

`buffer` is a fixed recovery window: `Duration::from_millis(200)` sets both
of librist's recovery bounds to 200 ms, so the window no longer starts at the
600 ms midpoint of a 200 to 1000 ms range. A buffer under 1 ms is refused
when the transport is opened. The URL form is `buffer=200`.

## Main Profile + AES-256 decryption

```rust
use tst_rist::{EncryptionKey, RistProfile, RistRecvTransportBuilder};

let rx = RistRecvTransportBuilder::new("rist://@0.0.0.0:9000")?
    .profile(RistProfile::Main)
    .encryption(EncryptionKey::aes256("shared-psk-here"))
    .listen()?;
```

PSK must match the sender's; otherwise packets arrive but fail decryption
and the receiver sees no data (the librist log channel reports
"Decryption failed" at WARN level).

## Backpressure on quiet links

librist's poll API returns timeout every 100ms when no packets arrive,
which `tst-rist` surfaces as `TransportError::Backpressure`. `DemuxReceiver`
treats Backpressure as "retry"; bare `recv_bytes` callers must do the same:

```rust
use tst_core::transport::{RecvTransport, TransportError};

let mut buf = vec![0u8; rx.max_payload() + 64];
loop {
    match rx.recv_bytes(&mut buf) {
        Ok(n) => process(&buf[..n]),
        Err(TransportError::Backpressure { .. }) => continue,
        Err(TransportError::Closed) => break,
        Err(e) => return Err(e.into()),
    }
}
```

## Verify with ffmpeg

```bash
ffmpeg -f mpegts -i 'video.ts' \
    -c copy -f rtp_mpegts 'rist://10.0.0.5:9000?profile=simple&buffer=200'
```
