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
| `tst_core::transport::conformance::{SendOptions, RecvOptions}` | Per-transport knobs of the conformance test harness, not construction parameters of a transport or shell. | No rename; the module is Provisional (see `api-stability.md`). |
| `SrtTransport::with_max_payload(self, n) -> Self` | Chainable `self -> Self` modifier on an already-constructed value; reads like a constructor by prefix but is a fluent modifier. | Borderline — the method's rustdoc (`crates/tst-srt/src/transport.rs`) clarifies the modifier intent. No rename; reviewers should not flag new `with_*` modifiers on existing values, but new constructors should still match the `with_<aspect>` aspect-rule. |

---

## Method naming conventions

**Rule:** Public methods that perform operations follow these verb prefixes:

| Verb | Meaning |
|------|---------|
| `send_*` | **Wire / transport-bound operation.** Frames a message and pushes it to a `Transport`. May block, may fail with a transport error. Examples: `MuxSender::send_video`, `Sender::send_ts`. |
| `recv_*` | **Wire-read counterpart of `send_*`.** Reads from a `RecvTransport`. May block, may fail with a transport error or return `None` on stream end. Examples: `DemuxReceiver::recv_event`, `RawReceiver::recv_one`. |
| `push_*` | **In-process enqueue.** Hands data to an in-process buffer / queue / parser. No transport contract; no I/O; the operation completes synchronously and either succeeds or returns a structural error. Examples: `Muxer::push_video`, `TsFraming::push_strict`. |
| `pull_*` / `feed_*` | **Consume from an in-process source.** Pull existing buffered output, or feed incoming bytes to an in-process consumer. No transport contract. Example: `Demuxer::feed_aligned`. |

**Why:** Without a codified rule, future contributors will invent local
verb vocabularies and reviewers will have to re-litigate "is this a
wire op or a buffer op?" on every PR. The rule above already holds
across the workspace's public methods; making
it explicit lets new code be reviewed against it and new public APIs
be named consistently from the start.

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

## See also

- `docs/reference/binding-authors.md` § "Builder shape" — the
  `&mut self -> &mut Self` builder shape and how each binding language
  spells it.
- `docs/reference/architecture.md` — crate boundaries and ownership.
