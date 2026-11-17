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
    let event = match event {
        Ok(event) => event,
        Err(e) if e.kind == tst_pipeline::ShellErrorKind::Backpressure => continue,
        Err(e) => return Err(e.into()),
    };
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

librist returns a timeout after a quiet 100 ms poll. `tst-rist` surfaces it
as `TransportError::Backpressure`, and `DemuxReceiver` propagates it as a
`ShellErrorKind::Backpressure` error. Retry it in either receive loop and
propagate every other error; a bare `recv_bytes` loop, which also ends on
`Closed`, looks like this:

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

## The publisher's address and the `source` pin

A RIST receiver records the address of the sender whose block it delivered
last, and a `source` key in its URL keeps it to one IP. Neither is proof of
who is sending. The address is the source address of the sender's UDP
datagrams, and nothing proves the sender can receive there: it can be
forged, the Simple profile has no handshake at all, and a pre-shared key
does not prove it either (a recording replayed one way from another
address was accepted and delivered, with a key). A sender that claims the
pinned address gets through; with a key, data replayed from a forged
address does too. `source=first` latches the IP of the first delivered
block and holds it until the receiver is closed, with no release. Without a
key that is the first block delivered, whoever sent it; with a pre-shared
key only a sender whose data decrypts can take it, and that includes a
recording of the publisher's earlier data replayed from any address. Use the address for logging and per-address rules, not as an
identity.

```rust
use tst_core::transport::RecvTransport;
use tst_rist::RistRecvTransportBuilder;

let rx = RistRecvTransportBuilder::new("rist://@0.0.0.0:9000?source=first")?.listen()?;
// The shell will own the transport: take the shared source record first.
let source = rx.source_handle().expect("a RIST receiver keeps a source record");
let mut receiver = tst_pipeline::DemuxReceiver::new(rx);
// ... read events as above; then, from any thread:
if let Some(publisher) = source.peer_addr() {
    println!("publishing from {publisher}; {} turned away", source.rejected());
}
```

`RistRecvTransportBuilder::source(SourcePin)` sets the same pin without
the URL key. `RistRecvTransport::peer_addr()` and `RistStats::source_rejected`
(`stats()`) read the same record as the handle, and the handle never waits
behind a receive parked on another thread.

- **What is reported.** The address (IP and UDP source port) of the sender
  whose block was delivered last and whose address is known; `None` before
  the first such block. In the Simple profile the port is the sender's data
  socket's. An IPv4 sender on a dual-stack bind is reported as plain IPv4.
  Without a `source` key every sender that reaches the receiver is
  delivered, interleaved, and `peer_addr()` names whichever delivered last.
- **What the pin compares.** The IP address only, never the port, so a
  sender that restarts, or whose NAT changes its port, continues. If the
  publisher's IP changes under a pin, its data is rejected and counted, and
  the receiver delivers nothing until it is closed and reopened.
- **What `source_rejected` counts.** Datagrams from a new sender whose IP
  the pin refuses (librist asks again for each one), datagrams whose source
  address could not be read, and delivered blocks refused on the read path
  (from an IP other than the one `first` latched, or from a sender the
  receiver holds no address for). Neither sender is told. A block refused on
  the read path returns `TransportError::Backpressure`, which the receive
  loops above already retry.
- **Refusals.** A sender URL or config with the key is refused at connect
  with `RistError::InvalidConfig`. A value that is neither an IP literal nor
  `first` is refused. A misspelt key (`?sourse=`) is ignored like any
  unknown key on `rist://`, which leaves the receiver without a pin.

The UDP recipe's [section on the same key](/docs/cookbook/receiving/udp.md#the-publishers-address-and-the-source-pin)
links each binding's getters.

## Verify with ffmpeg

```bash
ffmpeg -f mpegts -i 'video.ts' \
    -c copy -f rtp_mpegts 'rist://10.0.0.5:9000?profile=simple&buffer=200'
```
