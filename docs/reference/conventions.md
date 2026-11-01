# Workspace Conventions

This file codifies workspace-wide policies that span crates. Per-policy
rationale, with examples, follows each rule. New contributors check this
file before introducing new public types or constructors; reviewers
check new code against these rules.

---

## Config vs Options: use `Config` workspace-wide

**Rule:** Public types holding caller-supplied construction parameters
are named `<Component>Config`. Do not use the `Options` suffix.

**Why:** Two suffixes for the same concept (`MuxerConfig` vs
`DemuxerOptions`) force readers to guess which suffix a particular type
uses. The workspace's construction-parameter types use `Config`
(`MuxerConfig`, `MuxerProgramConfig`, `SenderConfig`, `RawSenderConfig`,
`SrtSocketConfig`, `SrtListenerConfig`, `DemuxerConfig`, `PairerConfig`,
`EncodeConfig`).

A transport has one config type for its socket options, and the URL parses
into it. An SRT URL is a `SrtSocketConfig` holding exactly the keys the URL
carried, and `SrtListenerConfig` holds a `SrtSocketConfig` in `socket` next to
`backlog` and `reuse_addr`, so there is no separate URL representation
and no copy of the socket fields for listeners.

**Examples:**

```rust
// Good — workspace convention:
pub struct SenderConfig { ... }
pub struct RawSenderConfig { ... }
pub struct ReceiverConfig { ... }
pub struct PairerConfig { ... }
pub struct EncodeConfig { ... }

// Bad — do not use:
pub struct PairerOptions { ... }  // use PairerConfig
```

**Note on naming collisions:** `MuxError` has both `InvalidConfig`
(a flat static string) and `ConfigInvalid { reason }` (a formatted
reason). The two coexist; the latter is for diagnostics that
need a formatted reason. New error variants should also prefer the
`<Subject><Adjective>` ordering used by the rest of the enum
(`KlvTooLarge`, `BufferFull`, `AudioTooLarge`).

---

## Constructor naming convention

**Rule:** Public constructors on workspace types follow these patterns:

| Pattern | Meaning |
|---------|---------|
| `T::new(...)` | **Primary constructor.** Takes all required arguments. Use this for the canonical way to construct. |
| `T::from_<format>(...)` | **Parsing constructor.** Decodes from a wire format / encoding (`from_bytes`, `from_str_strict`, `from_u8`, `from_h273`, `from_raw`, `from_pts`, `from_millis`, `from_env`, `from_file`). Always reads from input. |
| `T::with_<aspect>(...)` | **Variant constructor.** Takes one optional behavioral knob in addition to the required args. Useful when the knob is part of construction (not chainable later). Example: `Demuxer::with_config(config)`. |
| `T::default()` | **Zero-arg fallback.** Only when `Default` is implementable and meaningful. |
| `T::builder(...)` | **Builder factory.** Returns a builder type that produces `T`. Use the builder pattern (see "Builder vs Default" below) when the rule applies. |

**Why:** Mixed patterns across the workspace (`Pairer::with_config(config)`,
`Sender::new(transport, config)`, `H264Sps::from_bytes(...)`,
`MuxerConfig::builder()`) confuse callers reading imports. Codifying
the rule lets future code be reviewed against it.

**Examples:**

```rust
// Good:
let sender = Sender::new(transport, config);             // primary
let pairer = Pairer::new(video_pid, klv_pid);            // primary with required args
let sps = H264Sps::from_bytes(nal_payload)?;             // parsing
let demuxer = Demuxer::with_config(config);              // variant
let cfg = MuxerConfig::default();                        // zero-arg fallback
let bldr = MuxerConfig::builder();                       // builder factory

// Acceptable but exceptional — document why if you use them:
let pairer = Pairer::with_config(v, k, config);          // explicit knob still fine
```

**Per-pattern rationale:**

- **`T::new(...)`** is the canonical primary constructor. Use it when the
  type has a small fixed set of required arguments and no optional
  knobs. If callers commonly want defaults plus 1–2 overrides, pair
  `new` with a builder factory (`T::builder()`) rather than overloading
  `new` with optional-argument variants.

- **`T::from_<format>(...)`** signals a parsing or decoding step from
  some input format. The `_<format>` suffix names the source: `from_bytes`,
  `from_str`, `from_u8`, `from_h273`, `from_raw`, `from_env`, `from_file`,
  `from_pts`, `from_millis`. The name should make it obvious what the
  input is — never just `from_value` or `from_input`.

- **`T::with_<aspect>(...)`** is for variant constructors that take one
  meaningful behavioral knob in addition to required args. The aspect
  noun should be generic enough to read naturally (`with_config`)
  rather than re-stating the type name (`with_demux_options`). Used
  sparingly — prefer a builder for ≥2 knobs.

- **`T::default()`** is the zero-arg fallback, ONLY when every field
  has a meaningful zero value. Types that have required args without
  meaningful defaults (e.g., `MuxerProgramConfig`'s required
  `program_number` + `pmt_pid`) deliberately omit `Default` and ship a
  `new(...)` constructor with the required args instead.

- **`T::builder(...)`** is the builder factory entry point. Use the
  builder pattern when the "Builder vs Default" rule below applies.

**Known outliers (listed for transparency, not silently renamed):**

| Site | Convention concern | Disposition |
|------|---------------------|-------------|
| `DemuxReceiver::with_demux_options(transport, options)` | Uses over-specific `_demux_options` noun; rule recommends generic `with_config`. | Rename candidate before 1.0 — every C-ABI demux-receiver open path calls it (`bindings/c/core/src/{receiver/demux_receiver,udp,tcp,rtp,rist,rtsp/client}`), so the rename is not a one-line change. |
| `tst_core::transport::conformance::{SendOptions, RecvOptions}` | Per-transport knobs of the conformance test harness, not construction parameters of a transport or shell. | No rename; the module sits behind tst-core's non-default `conformance` feature. |

---

## Method naming conventions

**Rule:** Public methods that perform operations follow these verb prefixes:

| Verb | Meaning |
|------|---------|
| `send_*` | **Hands bytes to a `Transport` the object owns.** May block and may fail with a transport error. Examples: `MuxSender::send_video`, `Sender::send_ts`. A type that owns no `Transport` never has a `send_*` method. |
| `recv_*` | **Reads from a `RecvTransport` the object owns.** May block; returns `None` (or an end-of-stream kind) at stream end. Examples: `DemuxReceiver::recv_event`, `RawReceiver::recv_one`. |
| `push_*` | **Hands data to an in-process consumer**: a muxer, a publisher (which may write files or cut segments as part of the call), a server mount. No transport contract; completes synchronously. Examples: `Muxer::push_video`, `MuxPublisher::push_video`, `MountHandle::push_klv`. |
| `next_*` / `feed*` / `pull` | **In-process pull and feed.** A source that buffers results hands them out with `next_*` (`Demuxer::next_event`, `H264Depacketizer::next_au`); bytes go into a parser with `feed*` (`Demuxer::feed_aligned`); the muxer's output is taken with `pull`. A blocking wire read is `recv_*`, never `next_*`; today's exception is `Receiver::next_packet`, which blocks on its transport. |

**Why:** Without a codified rule, future contributors will invent local
verb vocabularies and reviewers will have to re-litigate "is this a
wire op or a buffer op?" on every PR. The rule above already holds
across the workspace's public methods; making
it explicit lets new code be reviewed against it and new public APIs
be named consistently from the start.

**Suffix order:** `<verb>_<media>[_wire|_misp][_to][_with_dts]`, for
example `Muxer::push_video_wire_to_with_dts` and
`MuxSender::send_video_misp_to`. `_to` exists only on surfaces whose muxer
can carry several streams of one media (it takes a stream handle):
`Muxer`, `MuxSender` and an RTSP `MountHandle`. `MuxPublisher` takes
exactly one video stream, so it has no `_to` form and its MISP variant is
`push_video_misp`. `_with_dts` follows `_to` where both apply; a DTS push
always names a stream handle.

**Examples:**

```rust
// Good — verb matches operation kind:
mux_sender.send_video(handle, nal_bytes, pts)?;  // wire op (transport blocks)
muxer.push_video(handle, nal_bytes, pts)?;        // in-process queue op
demuxer.feed_aligned(&packet)?;                   // in-process consumer
demux_receiver.recv_event()?;                     // wire read

// Bad — wrong verb:
// pub fn send_to_muxer(&mut self, ...) { ... }  // should be push_*; no wire
// pub fn push_over_transport(&mut self, ...) { ... }  // should be send_*; has I/O
```

**Adjective use note:** `send_` and `recv_` also appear as adjectives
on builder config-setter methods that qualify a configured aspect
(e.g., `SrtSocketBuilder::send_timeout`, `SrtListenerBuilder::recv_latency`).
These are configuration setters returning `&mut Self`, not operations
— the rule above governs operation names, not adjective-prefixed
setters. Config setters follow the existing builder-method
conventions documented in "Builder vs Default" below.

---

## Builder vs Default

**Rule:** Use a chainable `&mut self -> &mut Self` builder when:

- The config has **≥ 2 knobs**, OR
- The config requires **validation** that should run at build time, OR
- Construction has **required fields** that are best collected
  positionally (`Builder::new(required_arg)` then chainable setters).

Use `#[derive(Default)]` plain struct (or a manual `Default` impl) when:

- The config is **empty / single-knob**, AND
- **No validation logic** exists, AND
- All fields are **independently safe** to set in any order.

**Why:** No documented rule means new contributors invent local
conventions. The rule above matches the workspace's existing shape:
`MuxerConfigBuilder` + `MuxerProgramConfigBuilder` + `SrtSocketBuilder` +
`SrtListenerBuilder` use chainable `&mut self -> &mut Self`.
`RawSenderConfig` is empty/`#[derive(Default)]` with no builder.

**Examples:**

```rust
// Builder warranted (≥2 knobs + validation):
pub struct SrtSocketBuilder { ... }
impl SrtSocketBuilder {
    pub fn new() -> Self { ... }
    pub fn payload_size(&mut self, n: u16) -> &mut Self { ... }
    pub fn passphrase(&mut self, p: Passphrase) -> &mut Self { ... }
    pub fn connect(&self, addr: ...) -> Result<SrtSocket, ...> { ... }  // build + validate
}

// Plain struct sufficient (empty config, no validation):
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct RawSenderConfig {}
```

---

## Public field policy for `*Config` structs

**Rule:** Public `*Config` structs in this workspace use
**field-public + `#[non_exhaustive]`**. New fields can be added
non-breakingly; struct-literal construction is denied outside the
crate (per Rust E0639 for `#[non_exhaustive]`), so callers must use
`Config::default()` and assign overrides, or use the corresponding
builder where one exists.

**Why:**

- **Considered alternative:** opaque builder-owned configs with all
  fields private. Rejected because the migration cost across ~20
  workspace config types is substantial, and the pre-1.0 policy of
  breaking the API freely when needed
  means we can revisit this post-1.0 if the field-public shape proves
  problematic. For now, field-public + `#[non_exhaustive]` gives 95%
  of the future-proofing benefit at 10% of the implementation cost.
- **Builder coexistence:** types that have a `*ConfigBuilder` (like
  `MuxerConfig`) keep both. The builder remains the recommended
  ergonomic path; the field-public form is the FFI-friendly path for
  bindings that don't want to thread a builder.

**Construction patterns callers should use:**

```rust
// Recommended for Rust callers — default-and-assign:
let mut cfg = SrtSocketConfig::default();
cfg.payload_size = Some(1316);
cfg.passphrase = Some(Passphrase::from_env("MY_KEY")?);

// For MuxerProgramConfig (no Default impl), use the in-crate constructor:
let mut prog = MuxerProgramConfig::new(1, 0x1000);
prog.streams = vec![StreamSpec::Video { pid: 0x1011, codec: VideoCodec::H264 }];
prog.stream_descriptors = vec![vec![]];

// Or via builder where available:
let mut bldr = MuxerConfig::builder();
bldr.add_program(MuxerProgramConfigBuilder::new(1, 0x1000).build());
let cfg = bldr.build()?;

// NOT supported (Rust compile error E0639):
let cfg = SrtSocketConfig { payload_size: Some(1316), ..Default::default() };
```

---

## Where construction invariants are enforced

**Rule:** Validation of cross-field invariants on `*Config` structs
happens at the **constructor boundary** that consumes the config —
`Muxer::new(config)`, `MuxerConfigBuilder::build()`, etc. — by calling
`config.validate()` and returning `Err(...)` on rejection. Where a config
has no `validate()`, the consuming constructor checks each field as it
applies it: `SrtSocket::connect_with(&config, addr)` rejects a bad option
with `SrtConnectError::InvalidOption`.

**Why:** Public-field configs (per the rule above) can be hand-built
with struct-update syntax (`SomeConfig { field: x, ..Default::default() }`
where `#[non_exhaustive]` permits) or via builders. Either way, the
constructor that ultimately consumes the config is the choke-point
where invariants are guaranteed to be checked. Catching invalid
configs at construction-time is preferable to panicking later from
inside the muxer or transport.

**Example — `MuxerProgramConfig.stream_descriptors` length invariant:**

`MuxerConfig::validate()` raises `MuxError::ConfigInvalid { reason }`
with a formatted reason naming the program number, actual streams
count, and actual stream_descriptors count. The richer diagnostic
helps callers locate the offending program in a multi-program config
without re-reading the code.

```rust
impl MuxerConfig {
    pub fn validate(&self) -> Result<(), MuxError> {
        for prog in &self.programs {
            if prog.stream_descriptors.len() != prog.streams.len() {
                return Err(MuxError::ConfigInvalid {
                    reason: format!(
                        "program {} has {} streams but {} stream_descriptors \
                         (lengths must match)",
                        prog.program_number,
                        prog.streams.len(),
                        prog.stream_descriptors.len(),
                    ),
                });
            }
            // ... other checks ...
        }
        Ok(())
    }
}
```

The constructor-boundary approach is preferred over method-boundary
checks (catching invariants only when `push_video` is called, for
example) because the failure surfaces at construction-time, where the
caller still has the config in hand to fix.

---

## Naming grammar

| Pattern | Means | Examples |
|---|---|---|
| `<Proto>Transport` / `<Proto>RecvTransport` | The send half and the receive half of a protocol, implementing `Transport` and `RecvTransport`. One type serves both when the protocol is bidirectional on one connection. | `UdpTransport` and `UdpRecvTransport`; `TcpTransport` for both |
| `<Proto>Url` | The parsed URL of that scheme. | `RtpUrl`, `RtspUrl`, `UdpUrl` |
| `<Thing>Config` / `<Thing>ConfigBuilder` | Plain-data configuration and its builder. | `MuxerConfig`, `DemuxerConfigBuilder` |
| `<Thing>Stats` | A snapshot value returned by a getter. | `MuxSenderStats`, `RtpStats` |
| `<Proto>Error` | The error type of a protocol crate. | `UdpError`, `RtspError`, `HlsError` |
| `<Proto>CancelHandle` | That transport's cancel handle. Most are aliases of `tst_core::cancel::FlagCancel`; `SrtCancelHandle` aliases `ResourceCancel`. | `UdpCancelHandle`, `RtspServerCancelHandle` |

## Builders

Rule: a builder's setters take `&mut self` and return `&mut Self`, and
`build()` validates and constructs without network I/O. A builder whose
product is connected or listening ends in a call named for the I/O:
`connect()`, `bind()`, `listen()`, `start()`.

Existing exceptions, kept until the next naming change:

- `RtspClientBuilder` takes `self` in its setters and returns `Self`.
- `UdpTransportBuilder::build`, `UdpRecvTransportBuilder::build`,
  `TcpTransportBuilder::build`, `TcpListenerBuilder::build` and
  `HlsPublisherBuilder::build` open their socket or bind their listener.

## URL query keys

Every key a URL parser accepts. A key listed with two spellings accepts
either. Unknown keys are ignored by `udp`, `tcp` (not `tcps`), `hls` and `rist` URLs and
rejected by `srt`, `tcps`, `rtp`, `rtsp(s)` and `whep(s)` URLs.

<!-- BEGIN GENERATED by print-url-keys; do not edit -->

| Key | Meaning | Unit or values | Schemes |
|---|---|---|---|
| `iface` | Network interface for multicast and egress | interface name or literal IP address | `udp` |
| `localaddr` | Local address to bind | IP address | `udp` |
| `pkt_size` | Datagram payload size | bytes with the `K`/`M` forms | `udp` |
| `rcvbuf` | Kernel socket buffer size | bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB | `udp`, `tcp(s)` |
| `sndbuf` | Kernel socket buffer size | bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB | `udp`, `tcp(s)` |
| `tos` | IP type of service | decimal, or hex with a `0x` prefix, 0 to 255 | `udp` |
| `ttl` | Multicast hop limit | decimal 0 to 255 | `udp` |
| `iface` | Multicast interface | interface name or IPv4 literal | `rtp` |
| `pkt_size` | Bytes per datagram (send URLs only) | positive multiple of 188; default 1316; the 12-byte RTP header comes out of it, so the smallest usable value is 376 (188 parses, but its first send fails with `TooLarge`) | `rtp` |
| `pt` | Payload type for elementary-stream ingest | 1..=127 except 33 | `rtp` |
| `rcvbuf` | `SO_RCVBUF` (receive URLs only) | bytes, `K`/`M` suffixes | `rtp` |
| `recv_timeout` | Receive deadline | milliseconds, nonzero | `rtp`, `rtsp(s)` |
| `sndbuf` | `SO_SNDBUF` (send URLs only) | bytes, `K`/`M` suffixes | `rtp` |
| `source` | Accept datagrams only from this peer (receive URLs only) | IPv4 or IPv6 literal | `rtp` |
| `ssrc` | Synchronization source | u32 decimal or `0x` hex; default random | `rtp` |
| `ttl` | Multicast TTL / hop limit (send) | 1..=255; default 1 (unicast), 8 (multicast) | `rtp` |
| `rtsp_version` | Version written on request lines | `1.0` or `2.0`; default `1.0` | `rtsp(s)` |
| `tcp_keepalive` | `SO_KEEPALIVE` idle time for the control socket | seconds; default OS keepalive off | `rtsp(s)` |
| `transport` | SETUP transport preference | `tcp` or `udp`; default prefer UDP with TCP fallback | `rtsp(s)` |
| `ca` | Custom trust anchors for a caller (the system roots otherwise) | PEM file path; `tcps` only | `tcp(s)` |
| `cert` | Server certificate | PEM file path; `tcps` only, a listener | `tcp(s)` |
| `connect_timeout` | Connect deadline | seconds | `tcp(s)` |
| `keepalive` | `SO_KEEPALIVE` idle time | seconds | `tcp(s)` |
| `key` | Server private key | PEM file path; `tcps` only, a listener | `tcp(s)` |
| `listen` | Bind and accept instead of connecting | boolean | `tcp(s)` |
| `nodelay` | Disable Nagle's algorithm | boolean | `tcp(s)` |
| `pkt_size` | Largest payload chunk one send accepts | bytes with the `K`/`M` forms | `tcp(s)` |
| `auth_pass` | HTTP Basic password | text | `hls(s)` |
| `auth_user` | HTTP Basic user name | text | `hls(s)` |
| `cert` | Server certificate | PEM file path; an `hlss` publisher | `hls(s)` |
| `key` | Server private key | PEM file path; an `hlss` publisher | `hls(s)` |
| `mode` | Playlist mode | `live`, `event`, `vod` | `hls(s)` |
| `output_dir` | Where segments and playlists are written | directory path | `hls(s)` |
| `playlist_window` | Segments kept in a live playlist | count | `hls(s)` |
| `segment_duration` | Target segment length | seconds | `hls(s)` |
| `aes-type` | Encryption key size | 128, 192 or 256; needs `secret` | `rist` |
| `bandwidth` | Alias of `recovery_maxbitrate`; a different value is an error | kilobits per second | `rist` |
| `buffer` | Recovery buffer | milliseconds | `rist` |
| `cname` | RIST canonical name | text | `rist` |
| `compression` | NULL-packet compression | boolean | `rist` |
| `pkt_size` | Largest payload one send accepts (1..=9968) | bytes with the `K`/`M` forms | `rist` |
| `profile` | RIST profile | `simple` or `main` | `rist` |
| `recovery_maxbitrate` (also `recovery-maxbitrate`) | Bandwidth cap (payload plus retransmissions) | kilobits per second | `rist` |
| `secret` | Encryption passphrase | non-empty text; alone it means AES-256 | `rist` |
| `session_timeout` (also `session-timeout`) | Session timeout | milliseconds | `rist` |
| `congestion` (also `smoother`) | Congestion-control algorithm | `live` or `file` | `srt` |
| `conntimeo` (also `connect_timeout`) | Connect deadline | milliseconds | `srt` |
| `fc` (also `ffs`) | Flow-control window | packets | `srt` |
| `inputbw` | Input bandwidth estimate for the send budget | bytes per second | `srt` |
| `latency` (also `tsbpddelay`) | Receiver buffering delay | milliseconds | `srt` |
| `linger` | Close grace period for unsent data | seconds | `srt` |
| `lossmaxttl` | Reorder tolerance before a loss is reported | packets | `srt` |
| `maxbw` | Maximum send bandwidth | bytes per second; 0 is relative to the input rate | `srt` |
| `mode` | Connection role | `caller` or `listener`; default `caller` | `srt` |
| `mss` | Maximum segment size | bytes, 16 bits | `srt` |
| `oheadbw` | Recovery bandwidth overhead | percent, 5 to 100 | `srt` |
| `packetfilter` | Packet filter (forward error correction) configuration | libsrt spec string, for example `fec,cols:10,rows:5` | `srt` |
| `passphrase` | Encryption passphrase | 10 to 80 bytes | `srt` |
| `payloadsize` (also `pkt_size`, `payload_size`) | Payload bytes per packet | bytes, at most 1456 | `srt` |
| `pbkeylen` | Encryption key length | 16, 24 or 32 bytes | `srt` |
| `peerlatency` | Latency the peer should use when receiving | milliseconds | `srt` |
| `rcvlatency` | Receiver buffering delay of this side | milliseconds | `srt` |
| `streamid` (also `srt_streamid`) | Stream identifier sent in the handshake | text up to 512 bytes | `srt` |
| `tlpktdrop` | Drop packets that arrive too late | `0` or `1` | `srt` |
| `udprcvbuf` (also `recv_buffer_size`) | Kernel UDP receive buffer | bytes | `srt` |
| `udpsndbuf` (also `send_buffer_size`) | Kernel UDP send buffer | bytes | `srt` |
| `x-recvtimeout` | Receive timeout | milliseconds | `srt` |
| `x-sendtimeout` | Send timeout | milliseconds | `srt` |
| `advertise` | Address offered to viewers as the media candidate | literal IP address; default the bind address | `whep(s)` |
| `cert` | Server certificate | PEM file path; a `wheps` server | `whep(s)` |
| `cors_origin` | `Access-Control-Allow-Origin` value | origin or `*`; default `*` | `whep(s)` |
| `key` | Server private key | PEM file path; a `wheps` server | `whep(s)` |
| `max_viewers` | Viewers connected at once | count, 1 to 64; default 10 | `whep(s)` |
| `media_port` | UDP port shared by every viewer's media | port; default the URL's port | `whep(s)` |
| `token` | Bearer token every POST and DELETE must carry | text | `whep(s)` |

<!-- END GENERATED by print-url-keys -->

Boolean values are `1`, `0`, `true`, `false`, `yes`, `no`, `on` and `off`
for `tcp(s)` and `rist`; `srt` accepts only `0` and `1`. An `srt` URL names
a libsrt option this library does not expose with an error that carries the
`SRTO_*` name rather than ignoring it.

## Stats getters

- `stats()` is the object's own counters (`MuxSender::stats`,
  `RtspServer::stats`).
- `socket_stats()` is the transport-level `SocketStats` every transport
  fills the same way.
- `<native>_stats()` holds counters only one protocol has: `rtp_stats`,
  `rtcp_stats`, `hls_stats`, `publisher_stats`, `depay_stats`,
  `demuxer_stats`, `stream_codec_stats`.
- `reset_stats()` resets the first kind only. It exists on the pipeline
  shells, the demuxer and muxer, the pairer and `MountHandle`; it never
  clears `socket_stats()`. `RtspServer`, the publish mount handle and the
  transports have no `reset_stats()`.

---

## Exhaustive kind enums

Public enums are `#[non_exhaustive]` so a variant can be added in a minor
release. The one exception is a **kind enum**: the fieldless `*ErrorKind`
returned by an error's `kind()` (`MuxErrorKind`, `DemuxErrorKind`,
`KlvDecodeErrorKind`, `KlvFieldErrorKind`, `KlvEncodeErrorKind`,
`CodecParseErrorKind`, `TransportErrorKind`). A kind enum exists to be
matched exhaustively — the binding error-kind table matches all seven with
no wildcard — so a new kind is meant to be a compile error for every such
match. The error enums themselves stay `#[non_exhaustive]`, and `kind()` is
written in the defining crate with no wildcard, so a new error variant is a
compile error until it is classified.

## Crossing records

A struct that crosses to Python and the JVM field by field is declared once,
through `tst_core::binding_record!`. The macro emits the struct exactly as
written (derives, docs, visibility) plus a hidden `BINDING_SCHEMA` constant
that the binding generator reads, so the struct is the schema. The macro is
exported by `tst-core`, and a record is declared in the crate that owns it: the
stats records live in `tst-core`, `tst-pipeline`, `tst-srt`, `tst-udp`,
`tst-tcp`, `tst-rist`, `tst-hls` and `tst-rtp`. The generator is the
`tst-binding-gen` crate, which depends on all of them.

A field takes `@[…]` prefixes; they may repeat, e.g.
`@[custom] @[java("…Code")]`. `custom` and `skip` exclude each other:

- `@[custom]` — the field crosses through a hand-written converter per host,
  named `<record>_<field>_to_py` / `<record>_<field>_from_py` (Python) and
  `build_<record>_<field>` / `read_<record>_<field>` (JVM).
- `@[skip]` — the field does not cross.
- `@[java("name")]` — the JVM name, where it is not the camelCase of the Rust
  field.
- `@[py_default("text")]` — the Python source text of the dataclass default,
  where the type does not imply the one the host needs
  (`@[py_default("5_000_000")] pub update_interval_us: u64`). It wins over the
  default the type implies; the Java record has no default to carry.

Where the generated text goes:

- The Python and JVM converters are whole generated files under
  `bindings/{python,jvm}/src/generated/<family>.rs`.
- The dataclass, its stub entry and the Java record are blocks between
  `BEGIN GENERATED` and `END GENERATED` marker lines (`#` in Python, `//` in
  Java) of a hand-owned file. A block ends inside the class or record body, so
  everything after the end marker (`__post_init__`, helper methods, a Java
  compact constructor, the closing brace) is hand-written and survives
  regeneration; `--check` compares the block only. The Java block carries the
  Javadoc, the record header with its components and, past eight components,
  the `Builder`; the package, imports and the closing brace stay hand-written.
- The registry names each record's public path (`tst_core::klv::st0601::X`),
  which the converters use, so the declaring module may be private.

Field types and defaults:

- Rust `i8`/`i16` cross as Java `int` (range-checked on the way in), `u8`/`u16`
  as `int`, `u32`/`u64` as `long`, `Vec<u8>` as `ByteBuffer`, `Vec<T>` as
  `List<T>`; an `Option<T>` is the boxed type, and a Builder sets an optional
  scalar by its primitive.
- A `usize` count and a `Duration` cross to the host only: a count as an
  unsigned 64-bit value (`long` in Java) and a duration as whole microseconds
  in the same type, saturating. A record that holds either is registered
  `ToHost`; name the host field `…_us` with `py_rename` and `@[java("…Us")]`.
- A `@[custom]` `Vec<u8>` or `[u8; 16]` field is `byte[]` in Java and `bytes` in
  Python; a plain `Vec<u8>` is a `ByteBuffer`.
- A required field that follows an optional one takes a host default: the
  dataclass writes `= 0` / `= 0.0` / `= False` / `= ""` / `= b""` / `= ()`, and
  the Builder starts it at the same value. A required record has no such
  default and is refused. A `@[custom]` field takes the default of its row in
  the type table; its converter owns the rest.
- A required `String` component that is `null` in a Java record is an
  `IllegalArgumentException` naming it (`CotConfig.platformType must not be
  null`); an optional one is `None`.
- A record only one host exposes is registered for that host: `jvm = false`
  for a record Python alone exposes (the TCP, UDP and RIST stats), `python =
  false` for one only Java does (the demuxer and pairer stats, which Python
  hands over as dicts).
- A record that crosses one way is registered with that direction: `ToHost`
  has no reader and no `_from_py`, `FromHost` has no builder and no `_to_py`.
- A record whose Java shape the schema cannot express (the ST 1204 `CoreId`
  flattens each typed UUID into two components) is registered as Python-only:
  it has no Java block and no JVM converter, and its Java record and JNI
  converters stay hand-written.
- A record Java builds itself, from other records (the H.265 profile/tier/level
  block is made from an SPS or a VPS), is registered without a JNI builder: its
  Java record is generated, and no native hands one over.
- The host docs are the Rust docs: the record's, and each crossing field's.
  A dataclass docstring lists the documented fields as bullets; Javadoc has
  them as `@param`. A line `<!-- rust-only -->` ends the host-facing part of a
  doc (a doctest, a note on the owned/borrowed pair); rustdoc renders it as
  nothing. The text before it must read correctly once Rust paths are reduced
  to plain names, and `print_records_emit` refuses Rust primitive types,
  SCREAMING_CASE constants, function names and markdown emphasis in a code
  span or Javadoc.

A custom field names its host types in a row of the generator's type table
(`CUSTOM` in `typemap.rs`, keyed by the Rust type with paths and `Option<…>`
removed). The converters are hand-written functions with these signatures:

| Host | Function | Signature |
|---|---|---|
| Python | `<record>_<field>_to_py` | `(py: Python<'_>, &Rust) -> PyResult<Py<PyAny>>` |
| Python | `<record>_<field>_from_py` | `(obj: &Bound<'_, PyAny>) -> PyResult<Rust>`; receives the record object and reads the field itself |
| JVM | `build_<record>_<field>` | `(env: &mut Env<'l>, &Rust) -> jni::errors::Result<JObject<'l>>`; `null` for an absent value |
| JVM | `read_<record>_<field>` | `(env: &mut Env<'_>, obj: &JObject<'_>) -> jni::errors::Result<Rust>`; receives the record object and reads the field itself |

A custom row whose Java type is a primitive (`PayloadType`, a Java `long`) is
a required field only: its `build_<record>_<field>` returns that primitive
(`i64`) instead of an object, and the generator passes it by value.

The Python annotation of a custom enum field is `<Enum> | int | None`: the
enum when the code point is known, the raw `int` when it is not.

### Native Python classes

A record that only crosses to the host and that Python holds as a class of
its own, rather than a dataclass, is registered as a **native class**. The
generator writes `bindings/python/src/generated/<family>.rs`: a
`#[pyclass(frozen, get_all)]` struct of host-typed copies, built with
`From<&Rust>`, with the field docs as the getters' docstrings, and a
`register` function that adds every class of the family to the module. The
class keeps no Rust value, so a function that needs the Rust record back (a
slice-header parser given an SPS) re-parses the raw bytes the class carries.
A family holds native classes or dataclasses, not both. The stub entry is a
block between the markers of the `.pyi`; it ends inside the class, so
properties the module adds at import time follow the end marker. A native
class has no `.py` block: the module re-exports it from the extension.

A native field holds:

- a scalar, `String` or `Option<…>` as the same Rust type;
- bytes (`Vec<u8>`, `SharedBytes`, `[u8; N]`) as `crate::util::HostBytes`,
  which the getter turns into a Python `bytes` (a bare `Vec<u8>` would be a
  `list`);
- a nested record by value (a getter returns a fresh object, as the hand
  getters did), `Vec<Record>` as a list and `BTreeMap<u8, Record>` as a dict
  (a `HashMap` in Java);
- an enum the bindings already convert by hand as its host type, through a row
  of `HOSTS` in `typemap.rs` (the Python class, the Java type and the JVM
  builder). These cross to the host only;
- a `@[custom]` field through `<record>_<field>_to_py(&Rust) -> Host` (no `py`
  argument, no `PyResult`), unless its row says the host takes the Rust value
  as it is (a `Copy` type such as `Option<u32>`); the Java side still needs
  its `build_<record>_<field>`.

What a registry row adds beyond the fields: `host_name` (the Rust
`AdtsFrameOwned` is `AdtsFrame` to the hosts), `py_ctor` (a constructor taking
every crossing field, in order, all required), `py_eq` (`==` by field),
`py_repr` (a `format!` string and its arguments; without it `__repr__` lists
the fields), `py_rename` (a Python attribute named otherwise than the Rust
field) and `py_extras`:
methods and static methods the fields do not give, each calling the hand-written
`crate::custom::<record>_<name>(self, …)` and declared in the stub.

The generator is never its own oracle: byte-level expectations come from the
hand-built spec bytes and the hash-pinned fixtures, not from generated text.

## See also

- `docs/reference/binding-authors.md` § "Builder shape" — the
  `&mut self -> &mut Self` builder shape and how each binding language
  spells it.
- `docs/reference/architecture.md` — crate boundaries and ownership.
