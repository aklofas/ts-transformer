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
`SocketConfig`, `ListenerConfig`, `DemuxerConfig`, `PairerConfig`,
`EncodeConfig`).

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
| `next_*` / `feed*` / `pull` | **In-process pull and feed.** A source that buffers results hands them out with `next_*` (`Demuxer::next_event`, `H264Depacketizer::next_au`); bytes go into a parser with `feed*` (`Demuxer::feed_aligned`); the muxer's output is taken with `pull`. A blocking wire read is `recv_*`, never `next_*`. |

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
(e.g., `SocketBuilder::send_timeout`, `ListenerBuilder::recv_latency`).
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
`MuxerConfigBuilder` + `MuxerProgramConfigBuilder` + `SocketBuilder` +
`ListenerBuilder` use chainable `&mut self -> &mut Self`.
`RawSenderConfig` is empty/`#[derive(Default)]` with no builder.

**Examples:**

```rust
// Builder warranted (≥2 knobs + validation):
pub struct SocketBuilder { ... }
impl SocketBuilder {
    pub fn new() -> Self { ... }
    pub fn payload_size(&mut self, n: u16) -> &mut Self { ... }
    pub fn passphrase(&mut self, p: Passphrase) -> &mut Self { ... }
    pub fn connect(&self, addr: ...) -> Result<Socket, ...> { ... }  // build + validate
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
let mut cfg = SocketConfig::default();
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
let cfg = SocketConfig { payload_size: Some(1316), ..Default::default() };
```

---

## Where construction invariants are enforced

**Rule:** Validation of cross-field invariants on `*Config` structs
happens at the **constructor boundary** that consumes the config —
`Muxer::new(config)`, `MuxerConfigBuilder::build()`, etc. — by calling
`config.validate()` and returning `Err(...)` on rejection. Where a config
has no `validate()`, the consuming constructor checks each field as it
applies it: `Socket::connect_with(&config, addr)` rejects a bad option
with `ConnectError::InvalidOption`.

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

- `DemuxerConfigBuilder`, `HlsPublisherBuilder`, `RtspClientBuilder`,
  `RistTransportBuilder` and `RistRecvTransportBuilder` take `self` in
  their setters and return `Self`.
- `UdpTransportBuilder::build`, `UdpRecvTransportBuilder::build`,
  `TcpTransportBuilder::build`, `TcpListenerBuilder::build`,
  `RtpSocketBuilder::build`, `RtpRecvSocketBuilder::build` and
  `HlsPublisherBuilder::build` open their socket or bind their listener.

## URL query keys

Every key a URL parser accepts. A key listed with two spellings accepts
either. Unknown keys are ignored by `udp`, `tcp`, `hls` and `rist` URLs and
rejected by `srt`, `rtp` and `rtsp(s)` URLs.

| Key | Meaning | Unit or values | Schemes |
|---|---|---|---|
| `iface` | Network interface for multicast and egress | interface name or literal IP address | `udp`, `rtp` |
| `localaddr` | Local address to bind | IP address | `udp` |
| `ttl` | Multicast hop limit | decimal 0 to 255 for `udp`; 1 to 255 for `rtp` | `udp`, `rtp` |
| `tos` | IP type of service | decimal, or hex with a `0x` prefix, 0 to 255 | `udp` |
| `rcvbuf`, `sndbuf` | Kernel socket buffer size | bytes; `K` and `M` suffixes are 1024-based; at most 256 MiB | `udp`, `tcp`, `rtp` |
| `pkt_size` | Datagram payload size | bytes with the `K`/`M` forms for `udp`; a positive multiple of 188 for `rtp`; at most 1456 for `srt` (also `payloadsize`, `payload_size`) | `udp`, `rtp`, `srt` |
| `ssrc` | RTP synchronisation source | decimal or `0x` hex, 32 bits | `rtp` |
| `pt` | RTP payload type | 1 to 127, not 33 | `rtp` |
| `source` | Accept datagrams only from this sender | IP address | `rtp` |
| `recv_timeout` | Receive deadline | milliseconds, nonzero | `rtp`, `rtsp(s)` |
| `transport` | RTSP media transport | `tcp` or `udp` | `rtsp(s)` |
| `rtsp_version` | Protocol version | `1.0` or `2.0` | `rtsp(s)` |
| `tcp_keepalive` | TCP keepalive on the control socket | seconds | `rtsp(s)` |
| `listen` | Bind and accept instead of connecting | boolean | `tcp(s)` |
| `nodelay` | Disable Nagle's algorithm | boolean | `tcp(s)` |
| `keepalive` | `SO_KEEPALIVE` idle time | seconds | `tcp(s)` |
| `connect_timeout` | Connect deadline | seconds for `tcp(s)`; milliseconds for `srt` (also `conntimeo`) | `tcp(s)`, `srt` |
| `cert`, `key` | Server certificate and private key | PEM file paths; a `tcps` listener or an `hlss` publisher | `tcps`, `hlss` |
| `ca` | Custom trust anchors for a caller (the system roots otherwise) | PEM file path | `tcps` |
| `output_dir` | Where segments and playlists are written | directory path | `hls(s)` |
| `segment_duration` | Target segment length | seconds | `hls(s)` |
| `playlist_window` | Segments kept in a live playlist | count | `hls(s)` |
| `mode` | Playlist mode for `hls(s)`; connection role for `srt` | `live`, `event`, `vod`; `caller`, `listener` | `hls(s)`, `srt` |
| `auth_user`, `auth_pass` | HTTP Basic credentials | text | `hls(s)` |
| `profile` | RIST profile | `simple` or `main` | `rist` |
| `recovery_maxbitrate` | Retransmit bandwidth cap (also `recovery-maxbitrate`, `bandwidth`) | kilobits per second; `bandwidth` with a different value is an error | `rist` |
| `buffer` | Recovery buffer | milliseconds | `rist` |
| `session_timeout` | Session timeout (also `session-timeout`) | milliseconds | `rist` |
| `aes-type`, `secret` | Encryption key size and passphrase | 128, 192 or 256; non-empty text (`aes-type` needs `secret`; `secret` alone means 256) | `rist` |
| `cname` | RIST canonical name | text | `rist` |
| `compression` | NULL-packet compression | boolean | `rist` |
| `congestion` | Congestion controller (also `smoother`) | `live` or `file` | `srt` |
| `latency` | Receiver delay (also `tsbpddelay`) | milliseconds | `srt` |
| `peerlatency`, `rcvlatency` | Minimum delay the sender asks of the receiver; the receiver's own delay | milliseconds | `srt` |
| `fc` | Flow-control window (also `ffs`) | packets | `srt` |
| `inputbw` | Input rate estimate | bytes per second | `srt` |
| `maxbw` | Bandwidth cap | bytes per second; 0 is relative to the input rate | `srt` |
| `oheadbw` | Recovery overhead | percent, 5 to 100 | `srt` |
| `mss` | Maximum segment size | bytes, 16 bits | `srt` |
| `linger` | Close linger | seconds | `srt` |
| `lossmaxttl` | Maximum reorder tolerance | packets | `srt` |
| `packetfilter` | Packet filter specification | libsrt spec string, for example `fec,cols:10,rows:5` | `srt` |
| `passphrase`, `pbkeylen` | Encryption passphrase and key length | text; 16, 24 or 32 bytes | `srt` |
| `streamid` | Stream identifier (also `srt_streamid`) | text | `srt` |
| `tlpktdrop` | Too-late packet drop | `0` or `1` | `srt` |
| `udprcvbuf`, `udpsndbuf` | UDP buffers (also `recv_buffer_size`, `send_buffer_size`) | bytes | `srt` |
| `x-recvtimeout`, `x-sendtimeout` | Blocking call deadlines | milliseconds | `srt` |

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

## See also

- `docs/reference/binding-authors.md` § "Builder shape" — the
  `&mut self -> &mut Self` builder shape and how each binding language
  spells it.
- `docs/reference/architecture.md` — crate boundaries and ownership.
