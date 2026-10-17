# Binding-author starter

This page is the entry point for authors writing language bindings on top of
`ts-transformer`. The Rust API is sync-blocking and generic over a
`Transport` / `RecvTransport`; binding code targets one concrete binding object per
`(shell, transport)` pair for cross-language stability.

## The shape the bindings use

**C** keeps one `extern "C"` family per `(shell, transport)` pair
(`tst_srt_mux_sender_*`, `tst_udp_mux_sender_*`, …): a C symbol cannot
dispatch on a runtime type, so the generic shells are monomorphised per
transport in `transport_impls`.

**Python and the JVM** (and the planned Swift/Kotlin binding) expose one
class per shell — `Sender`, `Receiver`, `MuxSender`, `DemuxReceiver` in
`tstrans.shells` / `org.tstrans.shells` — over a type-erased transport. A
shell is opened from a URL: the scheme selects a transport crate's opener
(`tst_srt::opener`, `tst_rtp::opener`, `tst_udp::opener`,
`tst_tcp::opener`, `tst_rist::opener`), which returns
`tst_pipeline::binding::Opened<Box<dyn Transport>>` — the transport, its
cancel handle (taken before boxing) and its error `Domain`. The binding
wraps it in `MuxSender<Box<dyn Transport>>` etc. inside
`tst_pipeline::binding::Owned<Shell, view::Snapshot<H>>`; the snapshot's
`domain` decides which host exception class a cross-domain kind
(`Closed`, `Broken`, `TooLarge`, …) is raised as. A transport-specific
accessor (SRT's `srt_stats()`) downcasts through `Transport::as_any`.

`Managed*` (SRT reconnect) stays SRT-specific: its openers are
`tst_srt::shells::*_from_url`, which return `ManagedHandles`.

`Owned` owns the handle lifecycle, the shared cancel latch and the stats
snapshot in every binding.

## Per-language patterns

### Java / Kotlin (JNI)

Binding shape: the Java object holds a `long` handle **id**, not a pointer.
The native side keeps the shell in a process-wide `HandleRegistry<T>`
(`bindings/jvm/src/handle.rs`); every native call takes a short lease on
the entry, so a use-after-close is a lookup miss (an
`IllegalStateException`) rather than a dangling dereference, and `close()`
cancels first (a receive parked in native code unparks) before it removes
the entry. Every JNI entry point is a `jni_native!` block
(`bindings/jvm/src/panic.rs`), which runs the body under `jni_catch` so a
Rust panic becomes a `RuntimeException` instead of unwinding into the JVM;
the rail `jni-catch-coverage.sh` refuses a bare `extern "system"` function.
The sketch below is illustrative; the shipped classes live under
`org.tstrans.*`.

```kotlin
class MuxSender private constructor(private val handle: Long) : AutoCloseable {
    companion object {
        fun open(url: String, config: MuxerConfig): MuxSender =
            MuxSender(nativeOpen(url, config.toNative()))
    }
    fun sendVideo(payload: ByteArray, ptsTicks: Long) {
        nativeSendVideo(handle, payload, ptsTicks)
    }
    override fun close() { nativeClose(handle) }
}

// Kotlin idiomatic usage: .use { } for AutoCloseable
MuxSender.open(url, config).use { sender ->
    sender.sendVideo(payload, pts)
}
```

### Swift / Kotlin (UniFFI)

Not shipped. The planned `tst-uniffi` crate wraps the Rust crates directly
(not the C ABI) over a binding-shared layer extracted from `tst-pipeline`;
its shape — sync methods plus a cancel-token object, `ReconnectMode::Background`
by default for managed senders, per-call receive timeouts — is planned and
is recorded here when the crate lands.

### Python (PyO3)

Binding shape: `pyclass` with `__enter__` / `__exit__`.

```python
class MuxSender:
    def __init__(self, url: str, config: MuxerConfig):
        self._handle = _native.mux_sender_open(url, config._native)
    def send_video(self, payload: bytes, pts: int) -> None:
        _native.mux_sender_send_video(self._handle, payload, pts)
    def __enter__(self): return self
    def __exit__(self, exc_type, exc, tb):
        _native.mux_sender_close(self._handle)

# Python idiomatic usage: with-as block
with MuxSender(url, config) as sender:
    sender.send_video(payload, pts)
```

### C (`tst-c` ABI)

Binding shape: opaque handle (`tst_mux_sender_t *`) with explicit
`tst_*_close`. See `bindings/c/include/tstrans.h` for the full ABI.

```c
tst_mux_config_t *cfg = tst_mux_config_new();
tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
struct tst_mux_sender_t *sender = tst_mux_sender_open(url, cfg);
tst_mux_config_free(cfg); /* tst_mux_sender_open copies what it needs */
if (sender == NULL) { /* tst_get_last_error_str() describes the failure */ }
tst_mux_sender_send_video(sender, payload, payload_len, pts_ticks);
tst_mux_sender_close(sender);
```

### C ABI error-mapping contract

Every C-ABI error code surfaced through `tst_get_last_error()` and the
direct negative-return-value contract derives from one of two
explicit-coverage paths in `bindings/c/core/src/error.rs`:

**Shell-entry path (the common case).** Every C entry point that owns
a shell handle (`tst_mux_sender_*`, `tst_sender_*`,
`tst_raw_sender_*`, `tst_demux_receiver_*`, `tst_receiver_*`,
`tst_raw_receiver_*`) routes errors through one generic helper:

```rust
record_shell_error<E>(e: E) -> i32 where BindingError: From<E>
// 1. BindingError::from(e) -> BindingErrorKind
// 2. TstError::from_kind(kind) -> TstError
// 3. set_last_error(code, &e.to_string())
// 4. return negative code
```

Two invariants guard this path against silent regressions, both checked
by the compiler:

- every tst-core error the table classifies has a `kind()` written in
  tst-core with no wildcard, returning an exhaustive kind enum
  (`conventions.md` §"Exhaustive kind enums"); `kind.rs` maps each kind
  enum with no wildcard, so a new error variant or a new kind fails to
  build until it is classified;
- `ShellErrorKind` is `coarsen(BindingErrorKind)` (an exhaustive match in
  `kind.rs`): the shells compute their `kind` from the same table the
  bindings and the C ABI read, coarsened to the shells' smaller vocabulary.
  The C `record_shell_error` goes through `BindingError::from`.

**Raw-mapper path (standalone muxer + open helpers).** Every raw error
reaching the C ABI (`MuxError`, `DemuxError`, `KlvDecodeError`, the
transport crates' own errors) projects through
`tst_pipeline::binding::BindingError` → `TstError::from_kind`. The C-side
entry points (`record_mux_error`, `record_demux_error`,
`record_klv_decode_error`, `record_with_context`) are thin wrappers over
that one table. `kind_of_mux`'s fallback arm forwards to
`MuxError::kind()` for every `MuxError` variant with no precise code of
its own; that fallback is total.

### Adding an error kind

Add the row to `kinds!` in `crates/tst-pipeline/src/binding/kind.rs` (its
doc text is what Python and Java users read), add the kind to each
`Domain::kinds()` list that raises it (and, for a new domain, a `Domain`
variant and a Python value table in
`crates/tst-pipeline/tests/tools/print_kinds.rs`), then run
`cargo run -p tst-pipeline --bin print-kinds -- --write`.
`scripts/check/repo/kind-equivalence.sh` fails until the generated files
are committed. If the kind joins a Python enum, add its `(name, value)` row to
`bindings/python/tests/test_error_kind_values.py` in the same change: that file
is the independent record of the values Python has shipped, and the generator
cannot be its own oracle. A new JVM exception class is created with its empty
`// BEGIN GENERATED` / `// END GENERATED` block before `print-kinds --write`
fills it (`UdpException.java` is the model).

### Adding a crossing record

A struct that crosses to Python and the JVM field by field is declared once
with `tst_core::binding_record!` (marks and naming are in
`docs/reference/conventions.md` § "Crossing records"). Then:

1. Add one row to `records()` in
   `crates/tst-binding-gen/tests/tools/print_records/schema.rs`: the family (the
   generated-file name), the Python module, the Java package directory, the
   direction (`Both`, `ToHost` or `FromHost`) and the record's
   `BINDING_SCHEMA`; a record whose Java shape the schema cannot express sets
   `jvm = false` and is generated for Python only, and one that only Java
   exposes sets `python = false`. A family is one generated file per host, so
   a native class lives in the family of its Python module (the stats classes
   are `stats_<module>`): the family's `register` function adds its classes to
   one module. A record is declared in the crate that owns it, with
   `tst_core::binding_record!`, and the registry names it by that crate's
   public path. That list is the only
   hand-maintained one; a record declared with the macro and missing from it
   fails `every_declared_record_is_registered`.
2. Put an empty `# BEGIN GENERATED by print-records (<Name>); do not edit` /
   `# END GENERATED by print-records (<Name>)` pair where the dataclass
   belongs in the Python module and in its `.pyi` stub, then run
   `cargo run -p tst-binding-gen --bin print-records -- --write`. It writes
   `bindings/python/src/generated/<family>.rs`,
   `bindings/jvm/src/generated/<family>.rs` and the Java record whole, and
   fills the two marker blocks.
3. Review the diff in those five places. A field type outside the type table
   (`typemap.rs`) is a generator error naming the record and field, never a
   silent skip. Optional fields come after the required ones. The doc
   comments of the record and of each field become the docstring and the
   Javadoc: the first paragraph of each, so write it as complete sentences.
4. Write the converters for each `@[custom]` field by the names in
   `conventions.md`, in `crate::custom` of each binding.
5. A record that only crosses to the host, such as a parser's output, is
   registered as a native Python class (`conventions.md` § "Native Python
   classes"): the marker pair goes in the `.pyi` stub only, and the methods
   its row lists are written in `crate::custom` of the Python binding.

`scripts/check/repo/binding-records.sh` runs `print-records --check` and fails
until the committed files match a fresh render. Run the independent pins as
well: the hash-pinned fixtures, the hand-built spec-byte tests and the shared
golden parity test never come from generated text, so a wrong row in the type
table still fails them.

### Transient vs persistent error codes

Two negative `TST_E_*` codes — `TST_E_NOT_AVAILABLE` (-13) and
`TST_E_NOT_FOUND` (-14) — share a "the data you asked for isn't here"
shape but differ on whether the caller should retry:

| Code | Contract | When | Bindings should... |
|------|----------|------|---------------------|
| `TST_E_NOT_AVAILABLE` (-13) | **Transient** — the next call may succeed. | A `tst_managed_*` handle's underlying transport is reconnecting; stats / socket_stats are momentarily inaccessible. | Surface as a "retry later" signal. Example: Java/Kotlin → return `Optional.empty()` and let the caller poll; Swift → return `nil`; Python → return `None`. No user-visible exception. |
| `TST_E_NOT_FOUND` (-14) | **Persistent** — the next call with the same key will return the same error. | A per-PID accessor (e.g., `tst_*_get_stream_codec_stats`) was asked about a PID the demuxer has never seen on this handle. | Surface as a fail-fast lookup miss. Example: Java/Kotlin → throw `NoSuchElementException`; Swift → throw a typed error; Python → raise `KeyError`. |

`TST_E_INVALID_USAGE` (-9) is the third sibling — used when the handle
is in a fundamentally wrong state for the call (e.g., `tst_*_send_video`
on a handle that's already been closed). Distinct from both above
because it's a programmer bug, not a runtime data-availability question.

When designing your binding's retry policy, key the decision off the
TST_E code, not the message string (the message is a debug aid and is
not part of the stable contract — see "C ABI error-mapping contract"
above).

## Cancel handles

Every long-lived shell exposes `cancel_handle()` returning
`Option<Arc<dyn TransportCancel + Send + Sync>>`, and since 0.7.0 it is
`Some` on **every** transport: SRT (`ResourceCancel`), RTP
(`RtpCancelHandle`), TCP (`TcpCancelHandle`), UDP (`UdpCancelHandle`) and
RIST (`RistCancelHandle`). The `Option` survives only for pure in-memory
test mocks; a binding never has to handle `None` for a real transport —
`tst_pipeline::binding::Owned` holds the handle as a plain `Arc` and
fires it first on `close()`. In Python and the JVM every shell's
`cancel_handle()` returns the binding's one `CancelHandle`
(`tstrans.shells.CancelHandle`, `org.tstrans.shells.CancelHandle`);
`RtspCancelHandle` / `RtspServerCancelHandle` stay the RTSP control-plane
handles.
A handle is `Send + Sync`; `cancel()` is one-shot and idempotent, and
`is_cancelled()` (required on the trait since 0.7.0) reads the shared
latch — every clone, and a handle obtained after the cancel, agrees.

**One outcome.** A cancel that lands while an op is parked ends that op
with `TransportError::ExplicitClose`, and every later op returns it at
its entry check, on every transport that has a handle and on the managed
wrappers — and on the RTSP control plane: a cancelled `RtspClient` call
(`RtspError::LocalCancel`) projects to the same `Closed` kind
— the normative table lives in the `tst_core::transport` rustdoc
and `tst_core::transport::conformance` pins it in each crate's
`tests/*conformance.rs`. Bindings therefore project one kind for a cancel:
`TstError::Closed` (−7) / `<Domain>Error(CLOSED)` / `<Domain>Exception(CLOSED)`, via
`BindingErrorKind`. The detail is `cancelled from another thread` wherever
the `BindingError` reaches the caller unchanged — the Rust binding layer,
Python, the JVM, and the C **send** path. The C **receive** path is the one
exception: a receive that ends `EndOfStream` goes through
`record_recv_closed`, which picks its code and message from the shared cancel
flag so a caller close and a peer EOS can be told apart; a typed `Closed` (a
cancel or close, whichever handle issued it) is −7 regardless of the flag.
Either way a −7 there carries `receiver was cancelled or closed by caller`
instead. Key on the CODE, not
the message (see "C ABI error-mapping contract" above). The
transport's OWN `close()` keeps producing `Closed` (`ExplicitClose` on
`ManagedRecvTransport`, whose close is a caller-initiated end); the
binding layer tells "cancelled" from "closed" with `Owned::is_cancelled()`,
never by inspecting the variant. Through 0.6.x the plain SRT shells
reported a cancel as `Broken` (the connection error `srt_close` provoked);
since 0.7.0 they report `Closed` like every other transport. Bindings should expose the handle
as a language-native shutdown primitive (e.g. Kotlin `Job.cancel()`
analog, Swift `Task.cancel()` analog, Python `threading.Event`-shaped).
See `docs/reference/srt-cancel-handle.md` for the full pattern.

## Host naming grammar

| Concept | Python | JVM | Rule |
|---|---|---|---|
| open a shell from a URL | `MuxSender.open(url, program)` | `MuxSender.open(url, config)` | the one constructor of a URL-opened class; per-transport options are URL keys |
| a raw transport | `tstrans.<t>.Transport` / `RecvTransport` | `org.tstrans.rtp.Transport` / `RecvTransport` | no `RawSender`/`RawReceiver` host class |
| a TS-through shell | `tstrans.shells.Sender` / `Receiver` | `org.tstrans.shells.Sender` / `Receiver` | `Sender`/`Receiver` always mean TS bytes through a transport |
| a builder | `udp` / `tcp` / `rist` `Transport.builder()…build()`, `srt.Builder` | `srt.Builder`, `HlsPublisher.builder()`; no raw-transport builders | only where the Rust side is a builder; `rtp.Transport` takes URL keys instead |
| a failure | `<Domain>Error(kind=<Domain>ErrorKind.X)` | `<Domain>Exception(Kind.X)` | the domain is the transport's (or `Mux`, `Demux`, `Klv…`, `Codec`) |
| end of a receive session | `tstrans.shells.EndReason` | `org.tstrans.shells.EndReason` | one enum per binding |

## Builder shape

Every public builder (`MuxerConfigBuilder`, `MuxerProgramConfigBuilder`,
`SrtSocketBuilder`, `SrtListenerBuilder`) uses `&mut self -> &mut Self`. This
shape translates directly to:

| Language | Builder usage |
|----------|---------------|
| Kotlin | `MuxerConfigBuilder().apply { addProgram(...); }.build()` |
| Java | `new MuxerConfigBuilder().addProgram(...).build()` |
| Swift | `var b = MuxerConfigBuilder(); b.addProgram(...); b.build()` |
| Python | `b = MuxerConfigBuilder(); b.add_program(...); b.build()` |
| C | `tst_mux_config_t *cfg = tst_mux_config_new(); tst_mux_config_add_program(cfg, ...);` (the C config object is the builder) |

## Threading model

The threading guarantees split into two tiers — runtime handles vs.
opaque config builders. Read both before exposing tst-c objects as
thread-safe in your binding.

**Runtime handles** (the shells produced by `tst_*_open` /
`tst_*_open_listener`) are guarded by `CHandle<T>` inside `tst-c`, a
newtype over `tst_pipeline::binding::Owned` whose slot is a mutex (a spin
lock on a `no_std` build, which carries only the offline handles). Data-path entry points (`_send_*`, `_recv_*`, `_pull`,
`_get_stats`, `_push_*`, etc.) acquire that lock internally. Bindings
may safely call data-path functions on the same handle from any
thread — concurrent calls serialize through the inner mutex. Cancel
entry points (`_cancel`) intentionally do NOT acquire the mutex
(they're side-channels for unblocking a parked I/O thread), so they
can be invoked concurrently with data-path calls without
deadlocking. Each runtime handle is `Send` (ownership can transfer
between threads) and effectively `Sync` for data-path use through the
inner mutex.

**Opaque config builders** (`TstMuxConfig`, `TstDemuxConfig`,
`TstSenderConfig`, `TstRawSenderConfig`, `TstReconnectPolicy`) are
raw `Box<T>` values with unsynchronized mutable fields. Their setters
(`tst_mux_config_add_program`, `tst_demux_config_set_strict_mode`,
etc.) mutate through `&mut T` with NO internal locking. Bindings MUST
either:

- confine each builder pointer to a single thread for its entire
  lifetime (recommended — builders are short-lived by design), OR
- add a language-side lock (e.g. a Kotlin `synchronized`, a Swift
  `NSLock`, a Python `threading.Lock`) around every setter call on a
  shared builder.

Concurrent calls to two setters on the same builder pointer from
different threads without a language-side lock are a data race and
undefined behavior.

**Drop ordering.** Across two runtime shells that share state (e.g. a
sender + receiver pair that both reference the same underlying SRT
socket), `Drop` ordering is undefined. Consumers should drop senders
before receivers when both share state.

## Versioning

The Rust API uses pre-1.0 SemVer (`0.x.y`). The C ABI uses its own
three-tier scheme exposed through `tstrans.h`. The binding crates do not carry
`cargo public-api` baselines — their consumer contract is `tstrans.h` plus
the C-ABI ratchets; see
`docs/reference/public-api.md` § "Binding crates: no `cargo public-api`
baseline (by design)" for the full rationale.

- `TST_ABI_VERSION_MAJOR` — incremented on any source- or binary-
  incompatible change to the ABI shape. **0** today.
- `TST_ABI_VERSION_MINOR` — incremented on additive, source-compatible
  changes (new event kinds, new C entry points, new error codes).
  **24** today. History (additive bumps only — major stays at 0 pre-1.0):
    - `1`: receiver-surface initial drop.
    - `2`: `ManagedDemuxReceiver` wired into
      `tst-c`; `TST_EVENT_KIND_RECONNECT_DISCONTINUITY = 6` added; TS-bytes
      raw-receiver pull-loop hardening + C-ABI shape additions.
    - `3` (AU cell reassembly, 2026-05-24): `TstMultiCellAuReason` +
      `multi_cell_au_reason` field on `TstEventNonConformant`.
    - `4` (AU cell CFI tolerance, 2026-05-24):
      `TstNonConformantCode::CfiTolerated` (= 32) + `TstCellFragmentIndication`
      enum + `tst_demux_config_set_cfi_tolerance` setter. The new variant
      reuses the existing `cc_expected` + `cc_observed` field carriers
      to surface `observed_cfi` + `treated_as` without growing the struct.
    - `5` (demuxer-config parity, 2026-05-25):
      new C entry points `tst_demux_config_set_av1_carriage`,
      `tst_demux_config_set_au_cell_cap_per_pid`, and
      `tst_demux_config_set_lenient_psi_reassembly`, plus the
      `TstAv1CarriageMode` enum. Bridges Rust-only `DemuxerConfig`
      knobs through the C builder.
    - `6` (2026-05-26): RTP + RTSP C ABI surface.
      Cargo features `srt` + `rtp` (default-on through 2026-06-06, opt-in /
      default-**off** thereafter — like every other transport) gate the SRT
      and RTP/RTSP halves of the ABI; `TST_HAS_SRT` + `TST_HAS_RTP`
      `#define`s in tstrans.h let consumers `#if`-test feature presence.
      ~97 new C entry points: `tst_rtp_{sender,recv,mux_sender,demux_receiver}_*`
      open + close + data-path methods (~46 across 4 handle families) +
      `tst_rtsp_client_builder_*` + session methods (14) +
      `tst_rtsp_server_builder_*` + start + add_*_mount + mount push family
      + stats + cancel + stop (~37). 11 new error codes
      (`TST_E_RTP_TRANSPORT` through `TST_E_RTSP_MOUNT`, -15..-25).
    - `7` (2026-05-27): UDP + TCP (+ listener) + HLS publisher
      + RIST C ABI surface. Four new cargo features `udp` / `tcp` / `hls` /
      `rist` (all default-**off** — embedded `libtstrans.so` size stays
      unchanged for existing consumers); `TST_HAS_UDP` / `TST_HAS_TCP` /
      `TST_HAS_HLS` / `TST_HAS_RIST` `#define`s gate the surfaces. ~137 new
      C entry points: `tst_udp_*` (34) + `tst_tcp_*` (39, incl. listener) +
      `tst_hls_publisher_*` / `tst_publisher_*` / `tst_mux_publisher_*` (30,
      incl. the abstract `Publisher` trait projection + `MuxPublisher<P>`
      shell) + `tst_rist_*` (34) — each transport mirrors the RTP per-handle
      data-path surface (open/close/send_ts|recv_ts/push_*/next_event/stats)
      minus cancel (these transports had no `cancel_handle()` until `22`). 18 new
      error codes (`TST_E_UDP_IO` through `TST_E_RIST_IO`, -26..-43) +
      `TstPublisherKind` enum. **Note:** building with both `srt` and `rist`
      links two static mbedTLS copies; the cdylib build adds
      `-Wl,--allow-multiple-definition` (Linux) to collapse them onto one —
      see `bindings/c/build.rs`.
    - `8` — offline `tst_demuxer_*` byte-feeding demuxer surface: wraps
      `tst_core::Demuxer` directly (no transport URL); callers feed raw TS
      bytes and pull typed `TstEvent`s. Unconditional (no feature gate).
    - `9` — offline `tst_muxer_*` surface made unconditional (previously
      gated on the `srt` cargo feature; now lives alongside `tst_demuxer_*`
      with no feature gate). Non-SRT / no_std builds gain the offline muxer.
      Additive — no symbol removed, no signature changed.
    - `10` — two appended `TstMultiCellAuReason` values: `OverflowTotal`
      (= 4, aggregate AU-cell byte cap exceeded) and `TooManyPids` (= 5,
      too many in-flight AU PIDs). Both previously fell through to `Orphan`
      (0) via the forward-compat default.
    - `11` — `pmt_pid` field added to `TstEventProgramMap` (immediately after
      `pcr_pid`; `_pad` shrunk from 4 to 2 bytes to preserve total struct size).
      Exposes the PID carrying the PMT so C callers can reconstruct a muxer
      config from a `ProgramMap` event.
    - `12` — opaque private-data (`StreamSpec::Data`) stream surface:
      `tst_data_stream_handle_t` typedef plus seven new entry points —
      `tst_mux_config_add_data_stream`,
      `tst_mux_config_set_stream_descriptors_for_data`,
      `tst_mux_config_add_data_descriptor`, the offline muxer pair
      `tst_muxer_push_data` / `tst_muxer_push_data_to`, and the SRT-gated
      sender pair `tst_mux_sender_send_data` / `tst_mux_sender_send_data_to`.
      Carries opaque private payloads (PES `stream_id 0xBD`, arbitrary
      `stream_type`) alongside video/KLV, mirroring the Rust `push_data`
      family.
    - `13` — private-data push through managed-sender and RTSP-mount shells:
      the SRT-gated pair `tst_managed_mux_sender_send_data` /
      `tst_managed_mux_sender_send_data_to` (`TST_HAS_SRT`) and the
      RTP-gated pair `tst_rtsp_mount_push_data` / `tst_rtsp_mount_push_data_to`
      (`TST_HAS_RTP`). Completes data-stream surface parity with the
      video/klv/audio/subtitle push families on both shells.
    - `14` — AV1 carriage: `TstError::InvalidAv1Obu` (-44) guard
      error code; `av1_carriage` provenance byte on `TstEventSample`
      (repurposed pad byte — 0=`MPEG2_TS_BINDING`, 1=`INTEROP_RAW_OBU`,
      0xFF=N/A for non-AV1); `tst_muxer_push_video_wire` /
      `tst_muxer_push_video_wire_to` pass-through push for byte-faithful
      transmux; `tst_mux_config_set_av1_carriage` mux-side carriage setter.
    - `15` — `TstNonConformantCode::PmtProgramNumberMismatch`
      (= 33). PMT body `program_number` mismatch vs PAT assignment; surfaces
      on `TstEventNonConformant`, the mislabeled topology is NOT adopted;
      `pid` is the PMT PID; `programs[0]` = `pat_program`, `programs[1]` =
      `pmt_program` (reuses `programs_buf` carrier). No struct layout change.
    - `16` — demux trust-boundary diagnostics: four new
      `TstNonConformantCode` values (no struct layout change — all reuse
      existing carriers): `UnsupportedScrambling` (= 34),
      `AdaptationFieldMalformed` (= 35),
      `ZeroLengthPesNonVideo` (= 36),
      `PsiSyntax` (= 37). Carriers: `UnsupportedScrambling` has `pid` =
      the scrambled PID and `table_id` = the 2-bit
      transport_scrambling_control; `AdaptationFieldMalformed` has
      `table_id` = a kind discriminator (0 = ReservedControl,
      1 = BadLengthForControl, 2 = ShortPcr, 0xFF = unknown);
      `ZeroLengthPesNonVideo` has `table_id` = the PES stream_id;
      `PsiSyntax` has `table_id` = the PSI table_id, `obu_type` = the
      kind, and `cc_observed` = section_number for SectionNumberNonZero.
    - `17` — DTS-aware video push through the C ABI.
      `tst_muxer_push_video_to_with_dts` and
      `tst_muxer_push_video_wire_to_with_dts` add a `dts_90khz` parameter
      to the targeted video push, emitting PES with `PTS_DTS_flags = '11'`
      (ISO/IEC 13818-1 §2.4.3.6). Additive — no symbol removed, no
      signature or struct layout changed.
    - `18` — HLS promotion (tst-hls restructure): `tst_hls_publisher_finish_serving`
      returning the new opaque `TstHlsServerHandle` (`_local_addr` / `_shutdown` /
      `_free`) so VOD/EVENT playlists stay served after `finish`;
      `tst_hls_publisher_builder_max_segment_duration_ms` (wall-clock force-cut
      cap; `0` leaves the default) + `tst_hls_publisher_get_forced_cuts`
      (reads `HlsStats::forced_cuts`). The HLS surface moved from `tst-tcp`
      to the new `tst-hls` crate (link-level only; no C ABI effect). All
      gated `TST_HAS_HLS`; no struct growth (`TstHlsStats` stays 24 B — the counter rides a getter).
    - `19` — MISP timestamps (ST 0604): `tst_muxer_push_video_misp_to` /
      `tst_muxer_push_video_misp_to_with_dts` (SEI splice before the first VCL
      NAL) + `tst_misp_time_extract`, with error codes `TST_E_MISP_TIME` (−45)
      and `TST_E_MISP_TIME_MALFORMED` (−46). Additive — no symbol, signature,
      or struct layout changed.
    - `20` — bindings-parity bundle (2026-08-20). All additive —
      no existing symbol, signature, or struct layout changed:
      - **Background reconnect:** `TstReconnectMode` enum
        (`Blocking = 0`, `Background = 1`) + `tst_reconnect_policy_set_mode`
        setter on `tst_reconnect_policy_t` — both **unconditional**, like
        the rest of the reconnect-policy family (the builder module
        carries no feature gate; only the `Managed*` consumers are
        SRT-only). Also unconditional: the 48-byte
        `tst_managed_transport_stats_t` struct (5×`uint64_t` + `bool` +
        7-byte pad, size-pinned in the header trailer). `TST_HAS_SRT`
        gates only the three send-side getters that read it
        (`tst_managed_{sender,mux_sender,raw_sender}_get_reconnect_stats`).
      - **RTP stream-end reason:** `TstStreamEndReason` enum
        (`None = 0`, `CleanTeardown = 1`, `SessionExpired = 2`,
        `KeepaliveFailed = 3`, `TransportFailed = 4`, `ProtocolError = 5`,
        `Cancelled = 6`) is defined outside the `rtp` module — also
        **unconditional**, same reasoning as `TstReconnectMode` above (and
        avoids cbindgen re-emitting a per-variant `#if` guard on the
        enum). `TST_HAS_RTP` gates only the two getters that read it —
        `tst_rtp_{receiver,demux_receiver}_end_reason`. Last-error detail
        contract: an
        actually-recorded reason resets `tst_get_last_error[_str]` to
        `TST_E_SUCCESS` + the detail message (or `""` for msg-less
        reasons) on every call, overwriting any pending failure; `None`
        touches last-error not at all (see `tst_get_last_error`'s doc).
        **Wildcard aliasing:** a future Rust `EndReason` variant this
        binding doesn't know about yet maps to `TstStreamEndReason::None`
        until the C mapping is updated in a later release.
      - **Per-stream last-seen gauges:**
        `tst_*_get_stream_last_seen_micros` on all six demux-receiver
        families — plain + managed SRT (`TST_HAS_SRT`), RIST
        (`TST_HAS_RIST`), RTP (`TST_HAS_RTP`), TCP (`TST_HAS_TCP`), UDP
        (`TST_HAS_UDP`) — a `uint64_t` Unix epoch microsecond timestamp,
        `0` if the PID has never been observed.
      - **RTP receive-deadline parity — no new symbols:** the
        `?recv_timeout=<ms>` URL key is now honored by `tst_rtp_recv_open`
        / `tst_rtp_demux_receiver_open` (it previously reached only the
        RTSP-converted path); deadline expiry surfaces as the existing
        `TST_E_BUFFER_FULL` (-4), retryable — the same code SRT's
        recv-deadline expiry already returns to C consumers.
    - `21` — Apple/VideoToolbox-consumer surface (three independent
      additions, all additive — no existing symbol, signature, or struct
      layout changed):
      - **ST 0601 decode** (unconditional, no feature gate): opaque
        `TstSt0601` (owned decode result) +
        `tst_st0601_decode` / `_geometry` / `_get_f64` / `_get_u64` /
        `_state` / `_free`. `TstSt0601FieldState` enum (`Present = 0`,
        `Absent = 1`, `Sentinel = 2` for the MISB `INT_MIN` absent-value
        convention, `ImapbSpecial = 3` for an ST 1201.5 IMAPB special
        [±infinity / NaN / `BelowMin` / `AboveMax`], `WrongType = 4`
        returned only by the typed getters, never by `tst_st0601_state`
        itself) reports per-tag presence; `TstSt0601Geometry` is a
        `#[repr(C)]` struct bundling the common
        lat/lon/alt/heading/pitch/roll/hfov accessor group (a 23-tag
        curated set, each field paired with its own `_state` byte) behind
        one call. Two new
        error codes: `TstError::WrongType` (-47, a typed accessor called
        against a tag whose mapped field has a different native Rust
        type — the getter refuses a lossy cast) and `TstError::KlvDecode`
        (-48, `tst_st0601_decode` structural-parse failure; message
        carries `KlvDecodeError`'s `Display`, every variant collapsing
        to this one code).
      - **Annex-B / parameter-set helpers:** `tst_annexb_to_length_prefixed`
        (Annex-B start-code NAL stream → caller-provided length-prefixed
        output buffer, `length_size`-configurable) + opaque `TstParamSets`
        with `tst_param_sets_extract` / `_count` / `_get` / `_free`
        (SPS/PPS/VPS extraction from a parameter-set NAL stream). The
        conversion uses the two-call sizing idiom; no new error codes
        (`TST_E_BUFFER_FULL` / `TST_E_INVALID_CONFIG` / `TST_E_TOO_LARGE` /
        `TST_E_NOT_FOUND` are reused). Unconditional, like the ST 0601
        surface.
      - **Managed-receiver lifecycle:** `tst_managed_demux_receiver_end_reason`
        (`TST_HAS_SRT`) reuses the existing (ABI 20) `TstStreamEndReason`
        enum rather than adding a new one — `crate::receiver::demux_receiver::managed`
        gained a mapping of `tst_core::transport::EndReason` (now the one
        converter, `convert_end_reason`) onto the shared C enum.
        `tst_managed_demux_receiver_get_reconnect_stats` (`TST_HAS_SRT`)
        likewise reuses the existing `tst_managed_transport_stats_t`
        struct from ABI 20. `tst_demux_config_set_unwrap_timestamps`
        (unconditional) is a new demuxer-config setter.
    - `22` (2026-09) — additive; no
      existing symbol, signature, or struct layout changed, and no new C
      types or error codes:
      - **`tst_demux_config_set_sync_buf_cap`** (unconditional) — the
        pre-sync ingress ceiling (`DemuxerConfig::sync_buf_cap`);
        `0` restores the Rust default, the same sentinel
        `tst_demux_config_set_au_cell_cap_per_pid` uses.
      - **Cancel entry points for the three transports that had none:**
        `tst_tcp_{sender,mux_sender,receiver,demux_receiver,listener}_cancel`
        (`TST_HAS_TCP`), `tst_udp_{sender,mux_sender,receiver,demux_receiver}_cancel`
        (`TST_HAS_UDP`) and `tst_rist_{sender,mux_sender,receiver,demux_receiver}_cancel`
        (`TST_HAS_RIST`). Each fires the Rust `TcpCancelHandle` /
        `UdpCancelHandle` / `RistCancelHandle` through the shared
        `tst_pipeline::binding::Owned` slot without taking it, so it is
        callable from any thread while a data-path call is in flight, and
        the interrupted call returns `TST_E_CLOSED` (the
        one-cancel-outcome contract, see "Cancel handles"). `_cancel` never frees — the handle
        still takes its `_close` / `_free`. The tcp listener is the one
        exception to the mechanism: its handle holds a bare `TcpListener`,
        so `tst_tcp_listener_cancel` goes through
        `TcpListener::cancel_handle()` and a parked `_accept_*` returns
        NULL with `TST_E_CLOSED`.
      - **`MuxSender::finish` parity:** `tst_mux_sender_finish`,
        `tst_managed_mux_sender_finish` and
        `tst_{udp,tcp,rtp,rist}_mux_sender_finish` — drain the muxer's
        pending bytes to the live transport, report the first drain error,
        then close. Unlike `_close` they do NOT cancel first and do NOT
        free. See the `# capability:` lines of the surface manifest.
    - `23` (2026-10) — additive; no existing symbol, signature, or struct
      layout changed, and no new error codes. The RTSP server's publisher
      role, all `TST_HAS_RTP`:
      - **`tst_rtsp_server_builder_accept_unregistered_publishers`** —
        an ANNOUNCE to an unregistered path creates a publish mount on
        demand.
      - **`tst_rtsp_server_add_publish_mount`**,
        **`tst_rtsp_server_next_publisher`** (`TST_E_BUFFER_FULL` with
        `*out = NULL` on a timeout, `TST_E_CLOSED` once stopped; a
        `tst_rtsp_server_stop` on another thread wakes a parked call) and
        **`tst_rtsp_server_remove_mount`** (`TST_E_RTSP_MOUNT` when no
        mount is registered at the path).
      - **`tst_rtsp_server_local_addr`** — the bound listen address
        (`"ip:port"`, the kernel-picked port for a `:0` bind) into a
        caller buffer, NUL-terminated; returns the bytes written,
        `TST_E_INVALID_CONFIG` ("buffer too small") when the address plus
        its NUL does not fit, `TST_E_CLOSED` after `tst_rtsp_server_stop`.
        Every `*_local_addr` getter in the C ABI
        (`tst_hls_publisher_local_addr`, `tst_rtsp_server_local_addr`)
        shares this convention: byte count on success, and a too-small
        buffer is an error that writes nothing, never a silent
        truncation.
      - New opaque handle `tst_rtsp_publish_mount_t` with
        `tst_rtsp_publish_mount_{path,peer_count,generation,get_stats,
        publisher_info,cancel,into_demux_receiver,free}`. The transport is
        take-once (a second `_into_demux_receiver` is NULL with
        `TST_E_CLOSED`); `_free` never closes the mount.
      - New structs `tst_rtsp_publish_mount_stats_t` (every field of
        `PublishMountStats`) and `tst_rtsp_publisher_info_t`; new enums
        `tst_rtsp_publish_shape` and `tst_rtsp_clock_alignment`.
      - The server's publisher counters ride getters
        (`tst_rtsp_server_active_publishers`,
        `tst_rtsp_server_total_rtp_packets_received`,
        `tst_rtsp_server_total_rtp_bytes_received`): `tst_server_stats_t`
        keeps its layout.
    - `24` (`tls` cargo feature): `TST_HAS_TLS`. In a `tls` build,
      `tcps://` URLs, `tst_hls_publisher_builder_enable_tls`,
      `tst_rtsp_client_builder_tls_root_cert_pem` and
      `tst_rtsp_server_builder_tls_cert_pem` take effect (they were refused
      or ignored before). No symbol, layout, enum value or error code
      changed.
- `TST_ABI_VERSION_PATCH` — incremented on internal fixes that
  preserve both shape and behaviour.

Bindings should compile-time-assert the minor they require:

```c
#if TST_ABI_VERSION_MAJOR != 0 || TST_ABI_VERSION_MINOR < 17
#  error "this binding requires tst-c ABI ≥ 0.17"
#endif
```

`tst-jni` wraps the Rust crates directly (it has no binary dependency on the C ABI); the planned `tst-uniffi` does the same. The three bindings share a release cadence, not a binary contract.

## Thread-local last-error reset

`tst_clear_last_error()` (added in ABI 0.1) clears the thread-local
`(code, message)` slot read by `tst_get_last_error()` /
`tst_get_last_error_str()`. Bindings should call it at the start of
every public entry point that may return success but doesn't otherwise
overwrite the slot — without the clear, callers polling the error
string after a successful call see stale data from the previous call.

## Receiver-side reconnect (ABI 0.2)

`tst_managed_demux_receiver_*` mirrors the Rust `ManagedDemuxReceiver`
shell. When the underlying transport reconnects, the next
`tst_managed_demux_receiver_recv_event` emits a
`TST_EVENT_KIND_RECONNECT_DISCONTINUITY` event (kind = 6) so the language
binding can mark a hard discontinuity in any downstream state
(timestamp resets, PSI cache invalidation, decoder reset, etc.) instead
of guessing from the byte stream. The demuxer's `reset_sync()` is
called transparently before the next packet hits the syncer, and that
call **clears** the reassembly state — PAT/PMT, per-PID continuity
counters, last PCR/PTS, PES reassembly partials, and the dedupe sets
that gate `NonConformant` re-emission — rather than preserving it. Only
aggregate stats counters and the `DemuxerConfig` survive the reconnect.
The binding (and any downstream consumer) must treat post-reconnect PSI
and timestamp state as absent and rebuild it from the next PAT/PMT
cycle on the reconnected stream.

## Binding parity

Binding parity is the `bindings` column of
`tests/coverage/surface-manifest.toml`: each `[[surface]]` row lists the C,
Python, JVM, Swift and Kotlin twin of one Rust item, or says why there is none
(`deferred(reason)` for a gap that is real today, `n/a(reason)` for a gap by
design). A capability that spans several items is a `# capability:` comment
above its rows. The grammar is in `tests/coverage/README.md`, and the rail
`scripts/check/repo/surface-manifest.sh` resolves every named symbol against
the binding sources.

**Rule for new capabilities.** A ship-now cell must be under a day per binding
(a `with_ref_blocking`/`with_mut` body plus one error projection, with a red-first
test); anything larger is deferred WITH a trigger in
`docs/project/deferred-features.md`, never silently absent. The five-column
rule keeps the Swift and Kotlin columns from repeating this drift.

## C stats-getter naming

The C ABI stats getter symbols (`tst_mux_sender_get_stats`,
`tst_demux_receiver_get_stats`, etc.) keep the `get_` prefix as a frozen
ABI-stable convention. New bindings targeting the Rust API directly should
follow each language's idiomatic convention (no `get_` prefix in Rust/Kotlin;
`get` prefix in Java).
