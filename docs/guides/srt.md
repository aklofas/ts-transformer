# SRT Transport Guide


> **Who this is for:** You're integrating the SRT transport layer — opening sockets, listeners, builders; tuning latency / encryption / reconnect.

> **You will learn:**
> - How `SrtSocket`, `SrtListener`, and `SrtSocketBuilder` compose
> - How to enable AES encryption with a passphrase
> - The full set of supported SRT URL query parameters
> - How `ResourceCancel` lets you cancel blocking I/O from another thread
> - How `ManagedTransport` wraps a SrtSocket for automatic reconnect
> - Why `SrtTransport` implements both `Transport` and `RecvTransport` —
>   sender vs. receiver is a preset choice, not a type choice

## Introduction

When you need to send or receive raw SRT messages directly — handshake,
encryption, latency tuning, stream identification, statistics, and the per-call
error model — `tst_srt` is the layer you reach for. It's a safe Rust wrapper
over libsrt 1.5.7 with `SrtSocket` and `SrtListener` types modeled on
`std::net::TcpStream` / `TcpListener`.

Read this guide if your data path is byte-oriented and you handle the
framing yourself. If instead you have NAL units plus KLV blobs, pre-muxed
TS bytes, or arbitrary application messages and want reconnect plus
optional gap-buffering on top of an SRT socket, read
[guides/pipeline.md](/docs/guides/pipeline.md) — `tst_pipeline::*` composes
`tst_srt` into ready-made sender shells.

For wire-protocol details, see the IETF draft `draft-sharabayko-srt`,
the canonical normative reference for SRT 1.5.

## `SrtSocket` and `SrtListener`

The connection model mirrors `std::net::TcpStream` / `TcpListener`:

- Caller: `SrtSocketBuilder::new()...connect(addr)` returns a `SrtSocket`.
  Synchronous; blocks until the SRT handshake completes (including
  key-material exchange when encryption is on) or errors.
- Listener: `SrtListenerBuilder::new()...bind(addr)` returns a `SrtListener`.
  `listener.accept()` blocks until the next peer's handshake completes
  and returns `(SrtSocket, SocketAddr)`.
- Drop closes the socket. `SrtSocket::close` and `SrtListener::close` exist
  for callers who want to close before drop.

Caller, sending five messages then closing:

```rust,no_run
use tst_srt::SrtSocketBuilder;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut socket = SrtSocketBuilder::new()
        .latency(Duration::from_millis(120))
        .connect("127.0.0.1:9000")?;
    for i in 0..5 {
        let msg = format!("hello {i}");
        socket.send(msg.as_bytes())?;
    }
    socket.close();
    Ok(())
}
```

Listener, accepting one peer and draining to EOF (compare
[examples/srt_listener_to_file.rs](/examples/receiving/srt_listener_to_file.rs)):

```rust,no_run
use tst_srt::error::RecvError;
use tst_srt::SrtListenerBuilder;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut listener = SrtListenerBuilder::new()
        .latency(Duration::from_millis(120))
        .bind("127.0.0.1:9000")?;
    let (mut socket, _peer) = listener.accept()?;
    let mut buf = [0u8; 1500];
    loop {
        match socket.recv(&mut buf) {
            Ok(_n) => { /* handle buf[..n] */ }
            Err(RecvError::ConnectionBroken) => break,
            Err(e) => return Err(Box::new(e)),
        }
    }
    Ok(())
}
```

## Builders vs. config structs

Two equivalent forms construct the same `SrtSocket`:

- The builder — `SrtSocketBuilder::new().latency(...).passphrase(...).connect(addr)` —
  is a fluent wrapper. Each setter takes the typed wrapper
  (`Passphrase`, `KeyLength`, `StreamId`, ...), mutates the builder in
  place, and returns `&mut Self` so calls can be chained on a temporary
  or a `let mut b = SrtSocketBuilder::new();` binding. Terminal call is
  `connect(addr)`, which takes `&self` and clones the inner config so
  the builder can be reused. The shape translates directly to Kotlin's
  `apply { }`, Swift's mutable local, Java's chain, and Python's
  step-wise — see `docs/reference/binding-authors.md`. `SrtListenerBuilder` follows
  the identical shape: `&mut self -> &mut Self` mutators, terminal
  `bind(addr)` taking `&self` and cloning the inner `SrtListenerConfig`.
- The config struct — `SrtSocketConfig` — is the canonical type. Every
  field is `pub`, so bindings (UniFFI dictionaries, JNI POJOs,
  cbindgen C structs) consume it directly. It is `#[non_exhaustive]`, so
  outside `tst-srt` you start from `SrtSocketConfig::default()` (or a preset),
  assign the fields you need, and call `SrtSocket::connect_with(&cfg, addr)`.

Rust callers prefer the builder; binding generators prefer the struct
because it maps onto plain dictionary / POJO / C-struct shapes. The
builder's `.config()` method exposes the underlying struct for
inspection or copying.

```rust,no_run
use tst_srt::{SrtSocket, SrtSocketBuilder, SrtSocketConfig};
use std::time::Duration;

fn build_via_builder() -> Result<SrtSocket, Box<dyn std::error::Error>> {
    let s = SrtSocketBuilder::new()
        .latency(Duration::from_millis(120))
        .connect("127.0.0.1:9000")?;
    Ok(s)
}

fn build_via_config() -> Result<SrtSocket, Box<dyn std::error::Error>> {
    let mut cfg = SrtSocketConfig::default();
    cfg.latency = Some(Duration::from_millis(120));
    let s = SrtSocket::connect_with(&cfg, "127.0.0.1:9000")?;
    Ok(s)
}
```

`SrtListenerBuilder` and `SrtListenerConfig` mirror the same pattern; the listener's
socket options are a `SrtSocketConfig` in `SrtListenerConfig::socket`, next to
`backlog` and `reuse_addr`. The listener applies peer latency, the SRT and UDP
send buffers and the input bandwidth to the sockets it accepts, and
`SrtListenerBuilder` has a setter for each (`peer_latency`, `send_buf_bytes`,
`udp_send_buffer_bytes`, `input_bandwidth`).

## Sender / receiver presets

The library targets live-streaming over radio links from gimbaled
platforms (drones, manned ISR aircraft, helicopters with EO/IR turrets).
Three socket options have domain-tuned values that differ from libsrt's
defaults:

| Field             | libsrt default              | Sender preset | Receiver preset |
|-------------------|-----------------------------|---------------|-----------------|
| `connect_timeout` | 3 s                         | 15 s          | 15 s            |
| `linger`          | off (drains in background)  | 5 s           | off             |
| `role`            | `Role::Receiver` (default)  | `Sender`      | `Receiver`      |

The 15 s `connect_timeout` accommodates LOS interruptions, antenna
repointing, and radio warm-up. The 5 s sender-side `linger` lets a small
backlog flush on graceful close without stalling a `ManagedTransport`
reconnect cycle. The `role` flag drives `SRTO_SENDER` for HSv4-peer
compatibility (older Teradek/Makito gear, cable-industry hardware);
harmless under HSv5.

Receivers don't have an outbound queue to drain, so the receiver preset
leaves `linger` at libsrt's default. `Role::Receiver` (the default) does
not set `SRTO_SENDER`.

### Applying the presets

Two consumer paths, both supported in `tst_srt`:

```rust,no_run
use tst_srt::{SrtSocket, SrtSocketBuilder, SrtSocketConfig};
use tst_srt::options::Passphrase;
use std::time::Duration;

fn via_builder(passphrase: Passphrase) -> Result<SrtSocket, Box<dyn std::error::Error>> {
    // Builder chain — pure-Rust idiomatic
    let socket = SrtSocketBuilder::new()
        .sender_defaults()
        .passphrase(passphrase)
        .latency(Duration::from_millis(200))
        .connect("host:9000")?;
    Ok(socket)
}

fn via_struct(passphrase: Passphrase) -> Result<SrtSocket, Box<dyn std::error::Error>> {
    // Struct construction — FFI / UniFFI / JNI dictionary mirror
    let mut cfg = SrtSocketConfig::sender_defaults();
    cfg.passphrase = Some(passphrase);
    cfg.latency = Some(Duration::from_millis(200));
    let socket = SrtSocket::connect_with(&cfg, "host:9000")?;
    Ok(socket)
}
```

The receiver side mirrors exactly: `SrtSocketBuilder::new().receiver_defaults()`
or `SrtSocketConfig::receiver_defaults()`.

### Filling in defaults on an existing config

If a config came from elsewhere (URL parse, deserialized from a file)
and you want the preset to fill in only the fields the caller didn't
set, use the in-place merge:

```rust,no_run
# use tst_srt::SrtSocketConfig;
fn apply(mut cfg: SrtSocketConfig) -> SrtSocketConfig {
    cfg.merge_sender_defaults();
    // connect_timeout / linger / role get the preset values only if
    // the URL or other source didn't already set them.
    cfg
}
```

Merge-if-default semantics: `connect_timeout` and `linger` only fill if
`None`; `role` only fills if it is at the default (`Role::Receiver`).
Calling the merge twice is idempotent.

### Opting out

Don't call the preset — `SrtSocketConfig::default()` gives all-`None` /
`Role::Receiver`, which preserves libsrt's raw defaults across the
board. Every caller-mode SRT open in the `tst-c` C ABI (`tst_*_open`,
plain and managed, sender and receiver side) applies the sender preset
internally.

## Encryption

- `Passphrase::new(s)` accepts 10 to 80 bytes of UTF-8 (libsrt's own
  limit; the length is counted in bytes). Returns
  `Result<Passphrase, PassphraseError>`. Backed by
  `secrecy::SecretString` — `Debug` redacts and the buffer zeroes on
  drop. `Passphrase::from_env(var)` and `Passphrase::from_file(path)`
  cover the usual deployment shapes.
- `KeyLength` enum: `Aes128`, `Aes192`, `Aes256`; `KeyLength::default()`
  is `Aes128`. A config whose `key_length` is `None` does not set
  `SRTO_PBKEYLEN`, so libsrt negotiates the length with the peer.
- Encryption is gated by the `mbedtls` cargo feature on `srt-sys`
  (published as `tstrans-srt-sys`), on by default.
  `--no-default-features` builds an unencrypted libsrt;
  setting a passphrase against that build fails at handshake.
- Both peers must agree on the passphrase. A mismatch surfaces as
  `SrtConnectError::Rejected { reason: RejectReason::BadSecret, .. }`
  on the caller; the listener sees nothing — libsrt rejects the handshake
  before `accept()` returns (the `AcceptError::PeerRejected` variant exists
  but nothing produces it today).
- A key-length mismatch is not rejected: the handshake negotiates it.
  The side with `Role::Sender` (`SRTO_SENDER`, set by `sender_defaults()`)
  keeps an explicitly set length, the other side adopts it, and libsrt
  logs a warning. A side with `key_length` unset takes the peer's length,
  so a passphrase-only sender connecting to an AES-256 peer adopts
  AES-256. Set the same `KeyLength` on both ends when the cipher strength
  must not depend on the peer.
- A listener can require a different passphrase per stream ID; see
  [Listen rules](#listen-rules). A rule carries the passphrase only. libsrt
  fixes the key length from the caller's key material before the rule is
  consulted, so the length cannot differ per ID, and
  `SrtListenerBuilder::key_length` only states a preference to callers that
  leave theirs unset.

Paired listener and caller, mirroring
[examples/encrypted_send_recv.rs](/examples/sending/encrypted_send_recv.rs):

```rust,no_run
use tst_srt::{KeyLength, SrtListenerBuilder, Passphrase, SrtSocketBuilder};
use std::time::Duration;

fn listen() -> Result<(), Box<dyn std::error::Error>> {
    let mut listener = SrtListenerBuilder::new()
        .passphrase(Passphrase::new("shared-secret-not-for-production")?)
        .key_length(KeyLength::Aes256)
        .latency(Duration::from_millis(120))
        .bind("127.0.0.1:9000")?;
    let (_socket, _peer) = listener.accept()?;
    Ok(())
}

fn call() -> Result<(), Box<dyn std::error::Error>> {
    let mut socket = SrtSocketBuilder::new()
        .passphrase(Passphrase::new("shared-secret-not-for-production")?)
        .key_length(KeyLength::Aes256)
        .latency(Duration::from_millis(120))
        .connect("127.0.0.1:9000")?;
    socket.send(b"hello")?;
    Ok(())
}
```

## Latency tuning

The `latency` setter takes a `Duration` and maps to libsrt's
`SRTO_LATENCY` (with per-direction siblings `SRTO_RCVLATENCY` /
`SRTO_PEERLATENCY` for asymmetric values).

- 120 ms — conventional starting value for live SRT, matches every
  example in this repo. Enough budget for typical round-trip jitter
  and a handful of retransmits on LAN or good 4G.
- 250 to 500 ms — marginal links: long-haul satellite, congested 4G,
  multi-hop public-internet paths.
- 1000 ms or more — when reliability strictly outweighs latency:
  archival capture, store-and-forward, low-priority feeds.

The recovery window is bounded by `latency`. Too low and late packets
are dropped (counted in `SrtStats::packets_dropped_send_side` on the sender
or `SrtStats::packets_dropped_recv_side` on the receiver, under
`Congestion::Live`); too high and you pay the full setting in wall-clock
delay. Tune by measurement.

End-to-end latency in a `pipeline::*` shell adds the muxer's PCR/PSI
cadence and any reconnect gap-buffer to the SRT-level latency above —
see [guides/pipeline.md](/docs/guides/pipeline.md) for the full breakdown.

## Bandwidth and packet handling

- `MaxBandwidth` (`SRTO_MAXBW`): leave it unset to keep libsrt's default
  (`-1`: no limit; 1 Gbps cap in live mode). The variants set the raw
  value: `InputRelative` → `0`, which libsrt treats as relative to the input
  rate (`input_bandwidth` + `overhead_bandwidth_pct`) — the setting libsrt
  recommends for live streams with a fairly constant bitrate; `Infinite` → `-1`;
  `Limited(n)` → an absolute cap of `n` bytes/s.
- `Congestion::Live` vs. `Congestion::File`: `Live` drops late packets
  (TLPKTDROP) so the decoder isn't blocked on stale bytes — the right
  choice for video. `File` preserves every packet at the cost of
  unbounded latency.
- `too_late_packet_drop` (`SRTO_TLPKTDROP`) toggles drop behaviour
  explicitly. Defaults track the congestion mode; live video wants it
  on.
- `flow_window_packets` (`SRTO_FC`) sets the receiver's recovery window.
  libsrt picks a sensible default from `latency` and the input rate;
  tune only when bench results demand it.

Defaults are calibrated for live video. Touch only when measurement
demonstrates a specific shortfall.

## Stream IDs

`StreamId::new(s)` accepts UTF-8 of at most 512 bytes and
returns `Result<StreamId, StreamIdError>`. The caller sets the ID on
the builder before `connect`; the listener reads it post-`accept` via
`socket.stream_id() -> Option<&str>`.

```rust,no_run
use tst_srt::{SrtListenerBuilder, SrtSocketBuilder, StreamId};
use std::time::Duration;

fn caller() -> Result<(), Box<dyn std::error::Error>> {
    let id = StreamId::new("sensor-platform-1")?;
    let _socket = SrtSocketBuilder::new()
        .stream_id(id)
        .latency(Duration::from_millis(120))
        .connect("127.0.0.1:9000")?;
    Ok(())
}

fn listener_inspect() -> Result<(), Box<dyn std::error::Error>> {
    let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:9000")?;
    let (socket, peer) = listener.accept()?;
    match socket.stream_id() {
        Some(id) if id.starts_with("sensor-") => { /* keep, dispatch by id */ }
        _ => { /* drop the socket */ drop(socket); }
    }
    let _ = peer;
    Ok(())
}
```

The common pattern is to encode a platform identifier — synthetic
values like `"sensor-platform-1"` or `"TEST-001"` are appropriate for
examples and tests; never hard-code real call signs or operational
identifiers in source.

`AccessControl::parse(stream_id)` reads the structured `#!::` form
(`#!::u=alice,r=cam1,m=request`) into typed fields: `user()`,
`resource()`, `host()`, `session()`, `kind()` and `mode()`, plus
`get(key)` and `pairs()` for any other key. Only the flat form is
understood. A free-form ID returns `AccessControlError::NotAccessControl`,
a nested `#!{…}` ID returns `Unsupported`, and a malformed or repeated key
returns `Syntax` or `DuplicateKey`. `StreamId::access_control()` is the
same call on an ID you already hold. The JVM and Python bindings have the same parser.

### Listen rules

A listener can decide while the handshake is still running, before
`accept()` returns. `SrtListener::rules()` returns a `ListenRules` handle
onto a table keyed by stream ID. Each entry is a `ListenRule`: `Accept`,
`AcceptWith { passphrase }` or `Reject(RejectReason)`. Take the handle
before the accept loop starts, because `accept()` borrows the listener
mutably and parks:

```rust,no_run
use tst_srt::error::RejectReason;
use tst_srt::{ListenRule, Passphrase, SrtListenerBuilder};

fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:9000")?;
    let rules = listener.rules();

    // Only this name may connect, and only with its own passphrase.
    let amber = ListenRule::AcceptWith { passphrase: Passphrase::new("amber-secret-1")? };
    rules.set("publish:amber", amber.clone())?;
    // Everything else is refused with 1404 (`RejectReason::NotFound`).
    rules.set_default(ListenRule::Reject(RejectReason::NotFound))?;

    loop {
        let (socket, _peer) = listener.accept()?;
        if let Some(id) = socket.stream_id().map(str::to_owned) {
            // One live publisher per name: refuse the next caller.
            rules.set(&id, ListenRule::Reject(RejectReason::Conflict))?;
            // ... hand `socket` to a worker; when it ends, put the name back:
            // rules.set(&id, amber.clone())?;
        }
    }
}
```

- **Matching** is an exact string compare on the stream ID as a C string,
  because that is what libsrt's hook sees. A NUL byte is therefore refused
  in rule keys and in `StreamId::new` (`ListenRuleError::InteriorNul`,
  `StreamIdError::InteriorNul`); `remove` of such a key returns `false`.
  A caller with no stream ID, including a handshake-v4 caller, or one whose
  stream ID is not valid UTF-8, is checked against the default rule.
- **A handshake-v4 caller is refused wherever a passphrase applies**: a
  builder or URL passphrase on the listener, or an `AcceptWith` rule that
  decides the caller. v4 callers cannot carry the key material, so the
  listener refuses them with `RejectReason::VersionUnsupported` (1505) and
  the caller sees the connection closed. Without a passphrase they connect
  as before and get the default rule.
- **`accept()` checks the identity it matched.** If an accepted socket
  reports a stream ID that differs from the one the rules decided on, the
  socket is closed and the listener keeps waiting. This covers a handshake
  that carries its stream ID twice, and holds with a host libsrt too.
  `SrtSocket::stream_id()` drops libsrt's trailing NUL padding, so a
  one-character ID reads back as itself.
- **The default rule is `Accept`.** A listener whose table you never touch
  behaves exactly as it did before, including a builder-wide passphrase.
- **`Reject` codes run from 1000 to `i32::MAX - 1000`.** Lower codes
  belong to libsrt, and `set` returns `ListenRuleError::InternalReason`
  for them; a code above the upper bound is one libsrt cannot send, and
  `set` returns `ListenRuleError::ReasonOutOfRange`. Use a predefined
  `RejectReason` or an application code of 2000 or more.
- **Edits can come from any thread.** `ListenRules` is a cheap clone of the
  same table, so move a clone to a worker and call `set`, `remove`,
  `set_default` or `clear` while the accept thread is parked. An edit
  applies from the next handshake. After the listener closes, `set` and
  `set_default` return `ListenRuleError::Closed` and `remove` returns
  `false`.
- **Restore the original rule, do not just remove it.** Setting a name to
  `Reject(Conflict)` overwrites its earlier rule, and `remove` would drop
  the name to the default. Keep a clone of each rule you installed
  (`ListenRule` is `Clone`), set it back when the publisher ends, and
  `remove` only names that had no rule of their own.
- **There is a race window.** A rule installed after `accept()` returns does
  not cover a caller whose handshake completed in that instant. Keep the
  application's own check after `accept()` when a duplicate must never
  get through.
- **Decisions that need the peer address stay after `accept()`.** The
  table is keyed by stream ID only.
- **The caller sees the refusal at once.** Its connect fails with
  `SrtConnectError::Rejected { reason, .. }` rather than timing out, and
  the listener's `accept()` never returns for it. A caller that sends the
  wrong passphrase to an `AcceptWith` rule gets `BadSecret`, and one that
  sends none gets `Unsecure`.

The runnable version is
[examples/receiving/srt_listener_rules.rs](/examples/receiving/srt_listener_rules.rs);
the recipe is
[Accept or refuse SRT callers by stream ID](/docs/cookbook/receiving/srt-listen-rules.md).

## Packet filters

`PacketFilter::new(spec)` accepts libsrt's packet-filter spec verbatim
after validating charset and a 512-byte length cap. Returns
`Result<PacketFilter, PacketFilterError>`. The wrapper is validation
only; the spec semantics are libsrt's.

```rust,no_run
use tst_srt::{PacketFilter, SrtSocketBuilder};
use std::time::Duration;

fn fec_caller() -> Result<(), Box<dyn std::error::Error>> {
    let pf = PacketFilter::new("fec,cols:10,rows:5,arq:onreq")?;
    let _socket = SrtSocketBuilder::new()
        .packet_filter(pf)
        .latency(Duration::from_millis(250))
        .connect("127.0.0.1:9000")?;
    Ok(())
}
```

The spec format — FEC column / row sizing, ARQ modes, filter chaining —
is documented upstream at the Haivision libsrt repository
(`Haivision/srt`). A typed FEC builder is deferred — see "Typed
packet-filter / FEC builder" in
[`docs/project/deferred-features.md`](/docs/project/deferred-features.md).

## `SrtStats`

`socket.stats()` returns `Result<Stats, IoError>`, a snapshot of
libsrt's per-socket performance counters. The fields most useful for
operational dashboards:

- `bytes_sent`, `bytes_received` (`u64`).
- `bytes_lost_recv_side`, `bytes_lost_send_side` (`u64`) — split by
  which side observed the loss. `bytes_lost_send_side` is always 0
  (libsrt's `CBytePerfMon` doesn't expose a byte counter for sender
  loss; use `packets_lost_send_side` instead).
- `packets_sent`, `packets_received` (`u64`).
- `packets_lost_recv_side`, `packets_lost_send_side` (`u64`) — read
  the `_send_side` field on a sender (counts NAKs received), the
  `_recv_side` field on a receiver (counts sequence-gap discoveries).
  The opposite side's counter is always ~0.
- `packets_dropped_recv_side`, `packets_dropped_send_side` (`u64`) —
  same role split: too-late drops on the path the local socket
  controls.
- `packets_retransmitted` (`u64`).
- `rtt: Duration` — smoothed round-trip estimate.
- `mbps_estimated_bandwidth: f64` — libsrt's bandwidth probe.
- `send_buffer_packets`, `recv_buffer_packets` (`u32`) — queue depth;
  rising values mean pacing is falling behind input rate.

`stats()` is cheap — call on whatever cadence your dashboard needs.

```rust,no_run
use tst_srt::SrtSocket;
use std::thread;
use std::time::Duration;

fn dashboard(socket: &SrtSocket) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let s = socket.stats()?;
        eprintln!(
            "rtt={:?} sent={} recv={} lost_send={} lost_recv={} retx={} mbps={:.2} sndq={} rcvq={}",
            s.rtt, s.packets_sent, s.packets_received,
            s.packets_lost_send_side, s.packets_lost_recv_side,
            s.packets_retransmitted, s.mbps_estimated_bandwidth,
            s.send_buffer_packets, s.recv_buffer_packets,
        );
        thread::sleep(Duration::from_secs(1));
    }
}
```

## Error model

`tst-srt` uses per-call-category enums so `match` is meaningful at
every call site. Each enum carries an `Other { kind: SrtErrno, message:
String }` catch-all for libsrt errors outside the specific variants.

- `BindError` — `SrtListener::bind_with` failed: `AddressInUse`,
  `PermissionDenied`, `InvalidAddress`, `InvalidOption`, `System`,
  `Other`.
- `AcceptError` — `SrtListener::accept` failed: `TimedOut`,
  `ListenerClosed`, `PeerRejected { reason, detail }`, `System`,
  `Other`.
- `SrtConnectError` — `SrtSocket::connect_with` failed: `InvalidAddress`,
  `BadEncryption { detail }`, `Rejected { reason, detail }`,
  `TimedOut`, `Refused`, `InvalidOption`, `System`, `Other`.
- `SendError` — `SrtSocket::send` failed: `TimedOut`, `ConnectionBroken`,
  `PayloadTooLarge { actual, limit }`, `QueueFull`, `System`, `Other`.
- `RecvError` — `SrtSocket::recv` failed: `TimedOut`, `ConnectionBroken`,
  `BufferTooSmall { buf_len, message_len }`, `System`, `Other`.
- `IoError` — generic libsrt I/O surface (`close`, `peer_addr`,
  `local_addr`, `stats`): `SocketClosed`, `System`, `Other`.

The embedded helper enums `SrtErrno` and `RejectReason` are themselves
`#[non_exhaustive]`, so a wildcard arm is required where they are
matched.

Recovery summary:

| Error variant | Recoverable? | Action |
| --- | --- | --- |
| `RecvError::TimedOut` | Yes | Continue / retry |
| `RecvError::ConnectionBroken` | No | Close, re-bind / re-connect |
| `SendError::Other { kind: SrtErrno::Async, .. }` | Yes | Send buffer full (libsrt `EASYNCSND`) — backoff / retry. `SrtTransport` maps it to `TransportError::Backpressure`. (`SendError::QueueFull` exists but is never produced.) |
| `SendError::ConnectionBroken` | No | Reconnect (consider `ManagedTransport`) |
| `SrtConnectError::TimedOut` | Yes | Retry with longer timeout |
| `SrtConnectError::Rejected { reason: BadSecret, .. }` | No | Verify passphrase match |
| `SrtConnectError::BadEncryption { .. }` | No | Verify passphrase / key length |
| `BindError::AddressInUse` | No | Pick a different port |
| `BindError::PermissionDenied` | No | Run with the right user / capability |

`ManagedTransport` (in `pipeline::*`) automates the reconnect loop on
`SendError::ConnectionBroken`. See [guides/pipeline.md](/docs/guides/pipeline.md).

## Blocking semantics

The public API is sync blocking. Calls that block:

- `SrtSocketBuilder::connect` / `SrtSocket::connect_with` — until the SRT
  handshake completes or an error fires.
- `SrtListener::accept` — blocks indefinitely until the next peer's
  handshake completes. Use `SrtListener::accept_timeout` to impose a
  deadline (see below).
- `SrtSocket::recv` — until a message is available, the connection
  breaks, or the configured `recv_timeout` expires.
- `SrtSocket::send` — until the bytes are accepted by libsrt's send
  buffer; under flow-control backpressure this can block until the
  buffer drains or `send_timeout` expires.

Calls that don't block: builder / config setters, `SrtSocket::stats`,
`SrtSocket::peer_addr`, `SrtSocket::local_addr`, `SrtSocket::stream_id`,
`SrtListener::close`. `SrtSocket::close` (and dropping an `SrtSocket`)
can block while unsent data drains, for up to the configured `linger`:
5 s with `sender_defaults()`, no wait at libsrt's default (off).

Timeouts are configured pre-`connect` / pre-`bind` via the builder's
`recv_timeout(Duration)` and `send_timeout(Duration)` (or
`SrtSocketConfig::recv_timeout` / `send_timeout`); retune post-connect
via `SrtSocket::set_recv_timeout` / `SrtSocket::set_send_timeout`.

**Bounding `accept`:** libsrt's `srt_accept` does not honor
`SRTO_RCVTIMEO` — `SrtListenerBuilder::recv_timeout` and
`SrtListener::set_recv_timeout` apply to *accepted sockets*, not to the
`accept` call itself. To impose a deadline on the accept call, use
[`SrtListener::accept_timeout(duration)`](/crates/tst-srt/src/listener.rs)
instead of `listener.accept()`. It registers the listener fd with a
one-shot `srt_epoll_wait` and returns `AcceptError::TimedOut` when
`duration` elapses with no incoming connection.

```rust
use std::time::Duration;
use tst_srt::SrtListenerBuilder;

let mut listener = SrtListenerBuilder::new().bind("127.0.0.1:9000")?;
// accept_timeout returns Err(AcceptError::TimedOut) after 500 ms.
match listener.accept_timeout(Duration::from_millis(500)) {
    Ok((socket, peer)) => { /* handle connection */ }
    Err(tst_srt::AcceptError::TimedOut) => { /* no peer yet */ }
    Err(e) => return Err(e.into()),
}
```

There is no `set_nonblocking`. Async support is deferred — see the
sync-vs-async section in [reference/architecture.md](/docs/reference/architecture.md).

## Cancelling a blocked call

`SrtSocket::cancel_handle()` and `SrtListener::cancel_handle()` return a
clone-able `ResourceCancel` (`Send + Sync`, idempotent). Calling
`cancel()` from any thread closes the underlying SRT socket: a thread
parked in `send` / `recv` wakes with a connection error, and a parked
`accept()` returns. Once the socket is wrapped in `SrtTransport`, the same
cancel surfaces as `TransportError::ExplicitClose`. The full contract —
including the pipeline shells' `cancel_handle()` — is in
[reference/srt-cancel-handle.md](/docs/reference/srt-cancel-handle.md).

## URL parsing

Senders accept `srt://host:port?key=value&...` URLs in addition to
builder-style configuration. The URL parser uses libsrt's published URL
key vocabulary; values from the URL override values set on the builder
when both name the same option.

### Honored keys (Group 1)

| URL key | libsrt option | Type | Notes |
|---|---|---|---|
| `passphrase` | `SRTO_PASSPHRASE` | STRING | 10 to 80 bytes of UTF-8 |
| `pbkeylen` | `SRTO_PBKEYLEN` | INT | 16, 24, or 32; absent means libsrt negotiates |
| `latency` | `SRTO_LATENCY` | INT (ms) | non-negative; ffmpeg-style alias `tsbpddelay` (still ms — note ffmpeg's URL parses it as µs, so a value ≥10s logs a warn-level unit-mismatch hint) |
| `rcvlatency` | `SRTO_RCVLATENCY` | INT (ms) | non-negative |
| `peerlatency` | `SRTO_PEERLATENCY` | INT (ms) | non-negative |
| `mss` | `SRTO_MSS` | INT | u16 range |
| `payloadsize` | `SRTO_PAYLOADSIZE` | INT | at most 1456 (libsrt's live-mode maximum); ffmpeg-style aliases `pkt_size`, `payload_size` |
| `maxbw` | `SRTO_MAXBW` | INT64 | u64 range; non-negative only |
| `inputbw` | `SRTO_INPUTBW` | INT64 | u64 range |
| `oheadbw` | `SRTO_OHEADBW` | INT | 5..=100 |
| `streamid` | `SRTO_STREAMID` | STRING | UTF-8, ≤512 bytes; ffmpeg-style alias `srt_streamid` |
| `lossmaxttl` | `SRTO_LOSSMAXTTL` | INT | u32 range |
| `tlpktdrop` | `SRTO_TLPKTDROP` | BOOL | `0` or `1` only |
| `fc` | `SRTO_FC` | INT | u32 range; ffmpeg-style alias `ffs` |
| `packetfilter` | `SRTO_PACKETFILTER` | STRING | filter spec |
| `congestion` | `SRTO_CONGESTION` | ENUM | `live` or `file` (lowercase); ffmpeg-style alias `smoother` (libsrt's pre-1.4.1 name) |
| `conntimeo` | `SRTO_CONNTIMEO` | INT (ms) | non-negative; ffmpeg-style alias `connect_timeout` |
| `linger` | `SRTO_LINGER` | INT (seconds) | non-negative; matches ffmpeg URL units; `0` closes immediately |
| `udprcvbuf` | `SRTO_UDP_RCVBUF` | INT (bytes) | kernel UDP recv buffer; ffmpeg-style alias `recv_buffer_size`; Linux clamps to `net.core.rmem_max` |
| `udpsndbuf` | `SRTO_UDP_SNDBUF` | INT (bytes) | kernel UDP send buffer; ffmpeg-style alias `send_buffer_size`; Linux clamps to `net.core.wmem_max` |

### `ts-transformer` extension keys (Group 2)

These two keys have no libsrt-URL precedent and are specific to this
library. The `x-` prefix marks them as extensions and reserves the
namespace from accidental collision with future libsrt URL keys. These
keys are **not portable to other SRT tooling** (`srt-live-transmit`,
FFmpeg, GStreamer, etc.) — use builder-style config if portability matters.

| URL key | libsrt option | Type | Notes |
|---|---|---|---|
| `x-recvtimeout` | `SRTO_RCVTIMEO` | INT (ms) | bounds blocking recv on a connected/accepted socket; on a Listener this sets the timeout inherited by accepted sockets, but does **not** bound the `accept` call itself — libsrt ignores `SRTO_RCVTIMEO` on `srt_accept` |
| `x-sendtimeout` | `SRTO_SNDTIMEO` | INT (ms) | bounds blocking send and synchronous handshake |

### Mode and authority

`mode=caller` (the default when `?mode=` is absent) and `mode=listener`
are both accepted. `mode=rendezvous` rejects with
`SrtUrlError::UnsupportedMode`. Userinfo (`srt://user:pass@host:port`) is
explicitly rejected, with a hint pointing at `?passphrase=...` —
userinfo is not used by mainstream SRT tooling and is too easy to
leak through logs.

### Conflict resolution

When a URL query parameter and a builder setter both target the same
option, **the URL value wins**. On a repeated key every occurrence is
validated and the last valid one wins, so an invalid occurrence anywhere
is an error.
`SrtUrl::config` is a `SrtSocketConfig` holding exactly the keys the URL
carried. Layer it over a config you built with
`cfg.merge_from(&parsed.config)`: every key the URL set overwrites, every
other field keeps its value, and `role` is never touched. A listener takes
it as `listener_cfg.socket.merge_from(&parsed.config)`. To open the URL
directly, `SrtUrl::socket_config(role)` returns the dial config (the
sender preset is merged under the URL for `Role::Sender`) and
`SrtUrl::listener_config()` the listener's. The caller's `SrtSocketBuilder`
is never mutated by URL parsing.

### Strictness

Strict ASCII canonical forms — matches libsrt apps' own URL parser:

- Integers: decimal digits only, without a sign or leading zeros and with
  no `ms`/`s` suffixes; the signed options also refuse a negative value.
- Booleans: `0` or `1` only — not `true`/`false`/`yes`/`no`.
- Enums: lowercase only — `live`, not `Live`.
- Strings: percent-decoded once (`%XX` only). `+` is not decoded as a
  space: it stays a literal `+`, and `%2B` also decodes to `+`. A
  truncated or non-hex escape (`%2`, `%XY`) is rejected with
  `SrtUrlError::Syntax`, and that error names the escape's offset and the
  key, never the value.
- IPv6 hosts: an RFC 6874 zone delimiter is written `%25` and decodes to
  `%`, so `srt://[fe80::1%25eth0]:9000` has the host `fe80::1%eth0`. A
  named zone resolves to its interface through the system resolver.

Anything else rejects with `SrtUrlError::InvalidValue` carrying the key
name and a description.

### Deferred / unsupported keys

About two dozen libsrt URL keys are recognized by name but not yet
exposed (the parser knows them and rejects with `SrtUrlError::UnsupportedKey`
carrying the libsrt `SRTO_*` name so the operator can tell whether a
URL was malformed or asked for an option this library doesn't ship
yet). See [`deferred-features.md`](/docs/project/deferred-features.md) for the full
list and the trigger to revisit each one.

## What's deferred

Each item below maps to an entry in
[`docs/project/deferred-features.md`](/docs/project/deferred-features.md).

- Reactor / async / `srt_epoll_*` exposure — connection counts of ten
  or fewer per process make thread-per-connection adequate.
- Bonding / connection groups (`SRTO_GROUP*`) — no current consumer
  needs link bonding.
- Key rotation (`SRTO_KMREFRESHRATE`, `SRTO_KMPREANNOUNCE`) — typical
  stream durations don't trigger AES rekey thresholds.
- Protocol-version pinning (`SRTO_PEERVERSION`, `SRTO_MINVERSION`) —
  libsrt 1.5.7 negotiates with anything 1.3 or newer.
- Typed FEC / packet-filter builder — pass the libsrt spec string
  verbatim today.
- Peer-address decisions at SRT handshake time — the rules table is keyed by stream ID, so a decision that needs the caller's address stays after `accept()`; see [deferred features](/docs/project/deferred-features.md).
- Custom congestion-controller selection — `Live` and `File` ship.

## See also

- **Runnable example:** `cargo run -p tst-examples --example sender_from_url` — [examples/sending/sender_from_url.rs](/examples/sending/sender_from_url.rs)
- **Runnable example:** `cargo run -p tst-examples --example encrypted_send_recv` — [examples/sending/encrypted_send_recv.rs](/examples/sending/encrypted_send_recv.rs)
- [guides/pipeline.md](/docs/guides/pipeline.md) — the higher-level shells that wrap a `SrtSocket`.
- [reference/architecture.md](/docs/reference/architecture.md) — how `tst-srt` fits with `tst-core` and `tst-pipeline`.
