# Receive MPEG-TS over UDP

> **When to use this:** Ingest raw UDP unicast or multicast MPEG-TS from ffmpeg, VLC, or STANAG 4609 senders.

> **Related:**
> - [Send MPEG-TS over UDP](/docs/cookbook/sending/udp.md)
> - [MPEG-TS demux guide](/docs/guides/mpegts-demux.md)

```rust
use tst_pipeline::DemuxReceiver;
use tst_udp::UdpRecvTransport;

let rx = UdpRecvTransport::listen("udp://@239.10.0.1:5004?iface=eth0&rcvbuf=8M")?;
let mut receiver = DemuxReceiver::new(rx);

for event in &mut receiver {
    match event? {
        // ... handle DemuxEvent variants
        _ => {}
    }
}
```

## URL parameters

| Parameter | Default | Meaning |
|---|---|---|
| `iface` | OS default | Multicast join interface (IP addr or interface name) |
| `rcvbuf` | OS default | SO_RCVBUF in bytes (`8M` typical for high-bitrate streams); Linux clamps to `net.core.rmem_max` — a warning is logged when clamped |

For unicast bind, prefix the host with `@` to make intent explicit:
`udp://@0.0.0.0:5004`.

A multicast receiver binds its own group on Linux and macOS for IPv4, so it
delivers only that group's datagrams, not unicast sent to the port. On
Windows the receiver binds the wildcard address, so it also receives other
groups' traffic and unicast sent to the port. An IPv6 multicast receiver binds
the wildcard (`?iface=` is not supported for IPv6 yet), so it also sees
unicast sent to the port; outside Linux it sees other groups' traffic too, and
Linux kernels before 4.20 leave IPv6 receivers unfiltered.

## The publisher's address and the `source` pin

A UDP receiver records the source address of the datagram it delivered
last, and a `source` key in its URL keeps it to one IP. Neither is proof of
who is sending. The address is whatever the datagrams carry, and nothing
proves the sender can receive there: anyone who can reach the port can
forge it. (An SRT, RTSP or TCP peer address is proven by a handshake; a UDP
one is not.) `source=first` is taken by the first datagram with a payload
that reaches the receiver, whoever sent it, and holds until the receiver
is closed, with no release. Use the address for logging and per-address
rules, not as an identity.

| Key | Values | Meaning |
|---|---|---|
| `source` | an IP literal, or `first` | Deliver datagrams from that IP only, any port; `first` takes the IP of the first delivered datagram. Receive URLs only. |

```rust
use std::net::IpAddr;
use tst_core::transport::{RecvTransport, SourcePin};
use tst_pipeline::DemuxReceiver;
use tst_udp::{UdpRecvTransport, UdpRecvTransportBuilder};

// The first publisher to send takes the port; every other IP is dropped.
let rx = UdpRecvTransport::listen("udp://@0.0.0.0:5004?source=first")?;
// The shell will own the transport: take the shared source record first.
// A UDP receiver always has one.
let source = rx.source_handle().expect("a UDP receiver keeps a source record");
let mut receiver = DemuxReceiver::new(rx);

if let Some(_event) = receiver.recv_event()? {
    // A datagram was delivered, so its sender is recorded.
    if let Some(publisher) = source.peer_addr() {
        println!("publishing from {publisher}; {} datagrams turned away", source.rejected());
    }
}

// The same pin without a URL key: one IP, any port.
let mut b = UdpRecvTransportBuilder::from_url("udp://@0.0.0.0:5006")?;
b.source(SourcePin::Ip(IpAddr::from([10, 0, 0, 5])));
let pinned = b.build()?;
```

- **What is reported.** `UdpRecvTransport::peer_addr()` returns the source
  of the datagram delivered last, or `None` before the first one. An empty
  datagram is not delivered and does not count. Without a `source` key
  every sender that reaches the receiver is delivered, interleaved, and
  `peer_addr()` names whichever delivered last. An IPv4 sender on a
  dual-stack (`[::]`) bind is reported in IPv4 form.
- **Reading it from a shell.** `RecvTransport::source_handle()` returns a
  `SourceHandle` whose clones share the receiver's record; take it before
  the transport moves into a shell. `peer_addr()`, `rejected()` and
  `pin()` on it never wait behind a receive parked on another thread.
- **What the pin compares.** The IP address only, never the port, so a
  sender that restarts, or whose NAT changes its port, continues. An IPv4
  sender on a dual-stack bind is compared in IPv4 form. If the publisher's
  IP changes under a pin, its datagrams are rejected and counted, and the
  receiver delivers nothing until it is closed and reopened.
- **What happens to the rest.** A datagram turned away is counted in
  `UdpStats::source_rejected` (`stats()`) and in `SourceHandle::rejected()`,
  and in no received counter. Neither sender is told. `source=first` is not
  taken by an empty datagram.
- **Deadlines.** `recv_timeout` keeps waiting after a datagram the pin
  turns away while the time since the call began is below its deadline,
  so it returns no later than twice the deadline.
- **Windows.** A datagram larger than the caller's buffer, from any sender,
  fails the receive before its source is known.
- **Refusals.** A sender URL with the key is refused with a URL error, and
  so is a value that is neither an IP literal nor `first`. An IPv6 pin on
  a receiver bound to an IPv4 address is refused at open, since it could
  never match; an IPv4 pin on an IPv6 bind is allowed.
- **A misspelt key** (`?sourse=`) is ignored like any unknown key on
  `udp://`, which leaves the receiver without a pin;
  `source_handle()`'s `pin()` then reads `None`.

The same key and getters exist on [RIST receivers](/docs/cookbook/receiving/rist.md#the-publishers-address-and-the-source-pin)
and in every binding: [C](/docs/languages/c.md#udp-and-rist-ingest-the-publishers-address-tst_has_udp-tst_has_rist),
[Python](/docs/languages/python.md#the-publishers-address-and-the-source-pin),
[JVM](/docs/languages/jvm.md#the-publishers-address-peeraddr-and-the-source-pin).
