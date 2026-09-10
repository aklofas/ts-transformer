# Binding-author starter

This page is the entry point for authors writing language bindings on top of
`ts-transformer`. The Rust API is sync-blocking and generic over a
`Transport` / `RecvTransport`; binding code targets the **dyn-erased**
shape — one concrete type per pipeline shell — for cross-language stability.

## The dyn-erased shape

`tst_pipeline` re-exports six aliases that pin every shell to a boxed
trait object:

| Generic shell | Boxed alias |
|---------------|-------------|
| `MuxSender<T>` | `BoxedMuxSender` |
| `Sender<T>` | `BoxedSender` |
| `RawSender<T>` | `BoxedRawSender` |
| `DemuxReceiver<T>` | `BoxedDemuxReceiver` |
| `Receiver<T>` | `BoxedReceiver` |
| `RawReceiver<T>` | `BoxedRawReceiver` |

The aliases live at both `tst_pipeline::dyn_aliases::*` and the crate root.

Binding code constructs one of these by handing a `Box<dyn Transport>`
(or `Box<dyn RecvTransport>`) to `Xxx::new(transport, config)`:

```rust
use tst_pipeline::{BoxedMuxSender, MuxSender};
use tst_srt::SrtTransport;

let transport: Box<dyn tst_core::Transport> = Box::new(SrtTransport::connect(/* ... */)?);
let mut sender: BoxedMuxSender = MuxSender::new(transport, config)?;
```

## Per-language patterns

### Java / Kotlin (JNI)

Binding shape: opaque handle backed by `jlong` pointing at a `Box<BoxedMuxSender>`.

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

### Swift (UniFFI)

Binding shape: opaque struct with `deinit` calling close.

```swift
public class MuxSender {
    private var handle: OpaquePointer?

    public init(url: String, config: MuxerConfig) throws {
        self.handle = try tst_mux_sender_open_url(url, config.toNative())
    }
    public func sendVideo(payload: Data, pts: Int64) throws {
        try tst_mux_sender_send_video(handle, payload, pts)
    }
    deinit { tst_mux_sender_close(handle) }
}

// Swift idiomatic usage: defer { } block
let sender = try MuxSender(url: url, config: config)
defer { /* sender deinit closes automatically */ }
```

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
tst_mux_sender_t *sender = NULL;
if (tst_mux_sender_open_url(url, &config, &sender) != TST_OK) { /* error */ }
tst_mux_sender_send_video(sender, payload, payload_len, pts_ticks);
tst_mux_sender_close(sender);
```

### C ABI error-mapping contract

Every C-ABI error code surfaced through `tst_get_last_error()` and the
direct negative-return-value contract derives from one of two
explicit-coverage paths in `bindings/c/core/src/error.rs`:

**Shell-entry path (the common case).** Every C entry point that owns
a shell handle (`tst_mux_sender_*`, `tst_ts_sender_*`,
`tst_raw_sender_*`, `tst_demux_receiver_*`, `tst_ts_receiver_*`,
`tst_raw_receiver_*`) routes errors through one generic helper:

```rust
record_shell_error<E: ShellError>(e: &E) -> i32
// 1. e.kind() -> ShellErrorKind
// 2. tst_error_from_kind(kind) -> TstError
// 3. set_last_error(code, &e.to_string())
// 4. return negative code
```

Two CI ratchets guard this path against silent regressions:

- `scripts/check/rust/shell-error-kind-coverage.sh` — fails if a future
  `ShellErrorKind` variant is added without an explicit arm in
  `tst_error_from_kind` (before the `#[non_exhaustive]` wildcard).
- `scripts/check/rust/pipeline-kind-classification.sh` — fails if a future
  variant of `MuxError`, `TransportError`, `DemuxError`, or
  `TsFramingError` is added without an explicit arm in the
  corresponding `kind_from_*` helper in
  `crates/tst-pipeline/src/shell_error.rs`.

**Raw-mapper path (standalone-muxer + open helpers).** Two C-ABI paths
surface upstream errors before any shell wraps them and still go
through dedicated per-variant tables:

- `record_mux_error(&MuxError)` — used by `tst_muxer_*` (the
  standalone muxer, no transport).
- `record_transport_error(&TransportError)` — used by `tst_*_open_url`
  / `tst_*_open_addr` / `tst_*_listen_*` for connect/listen failures
  surfaced before a shell exists.

One CI ratchet guards this path:

- `scripts/check/c/raw-mapper-coverage.sh` — fails if a future
  `MuxError` or `TransportError` variant is added without an explicit
  arm in the corresponding `record_*_error` function.

Each path's wildcard `_ => ...` arm exists only to satisfy Rust's
`#[non_exhaustive]` requirement and is unreachable when the
corresponding ratchet is green. Binding authors can therefore assume
that every documented `TstError` code maps to a specific upstream
condition; no upstream variant silently degrades to `TST_E_INTERNAL`,
`TST_E_INVALID_CONFIG`, or `TST_E_TRANSPORT` without an explicit
choice by the tst-c maintainers.

If you encounter a `tst_get_last_error_str()` value beginning with
`"unhandled <Enum> variant: ..."`, that means one of the three
ratchets was bypassed or failed; please file an issue with the
variant name from the last-error string.

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

Every long-lived shell exposes `cancel_handle()` returning a `SrtCancelHandle`
that's `Send + Sync` and one-shot. Bindings should expose this as a
language-native shutdown primitive (e.g. Kotlin `Job.cancel()` analog,
Swift `Task.cancel()` analog, Python `threading.Event`-shaped). See
`docs/reference/srt-cancel-handle.md` for the full pattern.

## Builder shape

Every public builder (`MuxerConfigBuilder`, `MuxerProgramConfigBuilder`,
`SocketBuilder`, `ListenerBuilder`) uses `&mut self -> &mut Self`. This
shape translates directly to:

| Language | Builder usage |
|----------|---------------|
| Kotlin | `MuxerConfigBuilder().apply { addProgram(...); }.build()` |
| Java | `new MuxerConfigBuilder().addProgram(...).build()` |
| Swift | `var b = MuxerConfigBuilder(); b.addProgram(...); b.build()` |
| Python | `b = MuxerConfigBuilder(); b.add_program(...); b.build()` |
| C | `tst_muxer_config_builder_t *b = ...; tst_muxer_config_builder_add_program(b, ...);` |

## Threading model

The threading guarantees split into two tiers — runtime handles vs.
opaque config builders. Read both before exposing tst-c objects as
thread-safe in your binding.

**Runtime handles** (the shells produced by `tst_*_open` /
`tst_*_open_listener`) are guarded by `Handle<T> = Mutex<...>` inside
`tst-c`. Data-path entry points (`_send_*`, `_recv_*`, `_pull`,
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
  **21** today. History (additive bumps only — major stays at 0 pre-1.0):
    - `1` (plan #62): receiver-surface initial drop.
    - `2` (validate-1 Phase 2 wrap-up): `ManagedDemuxReceiver` wired into
      `tst-c`; `TST_EVENT_KIND_RECONNECT_DISCONTINUITY = 6` added; TS-bytes
      raw-receiver pull-loop hardening + F2 C-ABI shape additions.
    - `3` (AU cell reassembly, 2026-05-24): `TstMultiCellAuReason` +
      `multi_cell_au_reason` field on `TstEventNonConformant`.
    - `4` (AU cell CFI tolerance, 2026-05-24):
      `TstNonConformantCode::CfiTolerated` (= 32) + `TstCellFragmentIndication`
      enum + `tst_demux_config_set_cfi_tolerance` setter. The new variant
      reuses the existing `cc_expected` + `cc_observed` field carriers
      to surface `observed_cfi` + `treated_as` without growing the struct.
    - `5` (plan #96 demuxer-config parity, 2026-05-25):
      new C entry points `tst_demux_config_set_av1_carriage`,
      `tst_demux_config_set_au_cell_cap_per_pid`, and
      `tst_demux_config_set_lenient_psi_reassembly`, plus the
      `TstAv1CarriageMode` enum. Bridges Rust-only `DemuxerConfig`
      knobs through the C builder.
    - `6` (Phase 4 Stage 1, 2026-05-26): RTP + RTSP C ABI surface.
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
    - `7` (Plan A5a, 2026-05-27): UDP + TCP (+ listener) + HLS publisher
      + RIST C ABI surface. Four new cargo features `udp` / `tcp` / `hls` /
      `rist` (all default-**off** — embedded `libtstrans.so` size stays
      unchanged for existing consumers); `TST_HAS_UDP` / `TST_HAS_TCP` /
      `TST_HAS_HLS` / `TST_HAS_RIST` `#define`s gate the surfaces. ~137 new
      C entry points: `tst_udp_*` (34) + `tst_tcp_*` (39, incl. listener) +
      `tst_hls_publisher_*` / `tst_publisher_*` / `tst_mux_publisher_*` (30,
      incl. the abstract `Publisher` trait projection + `MuxPublisher<P>`
      shell) + `tst_rist_*` (34) — each transport mirrors the RTP per-handle
      data-path surface (open/close/send_ts|recv_ts/push_*/next_event/stats)
      minus cancel (these transports expose no `cancel_handle()`). 18 new
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
    - `13` — private-data push through managed-sender and RTSP-mount shells:
      the SRT-gated pair `tst_managed_mux_sender_send_data` /
      `tst_managed_mux_sender_send_data_to` (`TST_HAS_SRT`) and the
      RTP-gated pair `tst_rtsp_mount_push_data` / `tst_rtsp_mount_push_data_to`
      (`TST_HAS_RTP`). Completes data-stream surface parity with the
      video/klv/audio/subtitle push families on both shells.
    - `14` — AV1 carriage (WP-B): `TstError::InvalidAv1Obu` (-44) guard
      error code; `av1_carriage` provenance byte on `TstEventSample`
      (repurposed pad byte — 0=`MPEG2_TS_BINDING`, 1=`INTEROP_RAW_OBU`,
      0xFF=N/A for non-AV1); `tst_muxer_push_video_wire` /
      `tst_muxer_push_video_wire_to` pass-through push for byte-faithful
      transmux; `tst_mux_config_set_av1_carriage` mux-side carriage setter.
    - `15` — REF-PSI-01: `TstNonConformantCode::PmtProgramNumberMismatch`
      (= 33). PMT body `program_number` mismatch vs PAT assignment; `pid` is
      the PMT PID; `programs[0]` = `pat_program`, `programs[1]` =
      `pmt_program` (reuses `programs_buf` carrier). No struct layout change.
    - `16` — WP-D demux trust-boundary diagnostics: four new
      `TstNonConformantCode` values (no struct layout change — all reuse
      existing carriers): `UnsupportedScrambling` (= 34, REF-TS-01),
      `AdaptationFieldMalformed` (= 35, REF-TS-02),
      `ZeroLengthPesNonVideo` (= 36, REF-PES-01),
      `PsiSyntax` (= 37, REF-PSI-03).
    - `17` — BIND-01 (WP-I): DTS-aware video push through the C ABI.
      `tst_muxer_push_video_to_with_dts` and
      `tst_muxer_push_video_wire_to_with_dts` add a `dts_90khz` parameter
      to the targeted video push, emitting PES with `PTS_DTS_flags = '11'`
      (ISO/IEC 13818-1 §2.4.3.6). Additive — no symbol removed, no
      signature or struct layout changed.
    - `18` — HLS promotion (tst-hls restructure): `tst_hls_publisher_finish_serving`
      returning the new opaque `TstHlsServerHandle` (`_local_addr` / `_shutdown` /
      `_free`) so VOD/EVENT playlists stay served after `finish`;
      `tst_hls_publisher_builder_max_segment_duration_ms` (wall-clock force-cut
      cap) + `tst_hls_publisher_get_forced_cuts`. All gated `TST_HAS_HLS`; no
      struct growth (`TstHlsStats` stays 24 B — the counter rides a getter).
    - `19` — MISP timestamps (ST 0604): `tst_muxer_push_video_misp_to` /
      `tst_muxer_push_video_misp_to_with_dts` (SEI splice before the first VCL
      NAL) + `tst_misp_time_extract`, with error codes `TST_E_MISP_TIME` (−45)
      and `TST_E_MISP_TIME_MALFORMED` (−46). Additive — no symbol, signature,
      or struct layout changed.
    - `20` — bindings-parity bundle, items 6-9 (2026-08-20). All additive —
      no existing symbol, signature, or struct layout changed:
      - **Item 7, background reconnect:** `TstReconnectMode` enum
        (`Blocking = 0`, `Background = 1`) + `tst_reconnect_policy_set_mode`
        setter on `tst_reconnect_policy_t` — both **unconditional**, like
        the rest of the reconnect-policy family (the builder module
        carries no feature gate; only the `Managed*` consumers are
        SRT-only). Also unconditional: the 48-byte
        `tst_managed_transport_stats_t` struct (5×`uint64_t` + `bool` +
        7-byte pad, size-pinned in the header trailer). `TST_HAS_SRT`
        gates only the three send-side getters that read it
        (`tst_managed_{sender,mux_sender,raw_sender}_get_reconnect_stats`).
      - **Item 8, RTP stream-end reason:** `TstStreamEndReason` enum
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
        **Wildcard aliasing:** a future Rust `StreamEndReason` variant this
        binding doesn't know about yet maps to `TstStreamEndReason::None`
        until the C mapping is updated in a later release.
      - **Item 9, per-stream last-seen gauges:**
        `tst_*_get_stream_last_seen_micros` on all six demux-receiver
        families — plain + managed SRT (`TST_HAS_SRT`), RIST
        (`TST_HAS_RIST`), RTP (`TST_HAS_RTP`), TCP (`TST_HAS_TCP`), UDP
        (`TST_HAS_UDP`) — a `uint64_t` Unix epoch microsecond timestamp,
        `0` if the PID has never been observed.
      - **Item 6, RTP receive-deadline parity — no new symbols:** the
        `?recv_timeout=<ms>` URL key is now honored by `tst_rtp_recv_open`
        / `tst_rtp_demux_receiver_open` (it previously reached only the
        RTSP-converted path); deadline expiry surfaces as the existing
        `TST_E_BUFFER_FULL` (-4), retryable — the same code SRT's
        recv-deadline expiry already returns to C consumers.
    - `21` — Apple/VideoToolbox-consumer surface (three independent
      additions, all additive — no existing symbol, signature, or struct
      layout changed):
      - **ST 0601 decode:** opaque `TstSt0601` (owned decode result) +
        `tst_st0601_decode` / `_geometry` / `_get_f64` / `_get_u64` /
        `_state` / `_free`. `TstSt0601FieldState` enum (`Present = 0`,
        `Absent = 1`, `Sentinel = 2` for the MISB `INT_MIN` absent-value
        convention, `ImapbSpecial = 3` for an ST 1201.5 IMAPB special
        [±infinity / NaN / `BelowMin` / `AboveMax`], `WrongType = 4`
        returned only by the typed getters, never by `tst_st0601_state`
        itself) reports per-tag presence; `TstSt0601Geometry` is a
        `#[repr(C)]` struct bundling the common
        lat/lon/alt/heading/pitch/roll/hfov accessor group (each field
        paired with its own `_state` byte) behind one call. Two new
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
        (SPS/PPS/VPS extraction from a parameter-set NAL stream).
      - **Managed-receiver lifecycle:** `tst_managed_demux_receiver_end_reason`
        (`TST_HAS_SRT`) reuses the existing (ABI 20) `TstStreamEndReason`
        enum rather than adding a new one — `crate::receiver::demux_receiver::managed`
        gained a `convert_recv_end_reason` mapping `tst-pipeline`'s own
        `RecvEndReason` (new Rust-side type, `tst-pipeline`'s recv
        analogue of `tst_rtp::StreamEndReason`) onto the shared C enum.
        `tst_managed_demux_receiver_get_reconnect_stats` (`TST_HAS_SRT`)
        likewise reuses the existing `tst_managed_transport_stats_t`
        struct from ABI 20. `tst_demux_config_set_unwrap_timestamps`
        (unconditional) is a new demuxer-config setter.
- `TST_ABI_VERSION_PATCH` — incremented on internal fixes that
  preserve both shape and behaviour.

Bindings should compile-time-assert the minor they require:

```c
#if TST_ABI_VERSION_MAJOR != 0 || TST_ABI_VERSION_MINOR < 17
#  error "this binding requires tst-c ABI ≥ 0.17"
#endif
```

`tst-jni` and `tst-uniffi` track the C ABI for binary stability and the
Rust crate for feature parity.

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

## Muxer push surface parity matrix

The table below captures the full muxer push capability surface across all
four binding layers as of **v0.2.0 / ABI 17** (after BIND-01, WP-I).

| Capability | Rust core | C (`tst-c`) | Python (`tst-py`) | JVM (`tst-jni`) |
|---|---|---|---|---|
| Single-stream push (`push_video` / `push_klv` / `push_audio` / `push_subtitle` / `push_data`) | ✅ | ✅ | ✅ | ✅ |
| Targeted push (`push_video_to(handle, …)` etc.) | ✅ | ✅ | ✅ | ✅ |
| PTS + DTS (`push_video_to_with_dts`) | ✅ | ✅ | ✅ | ✅ |
| On-wire (AV1 carriage-aware) push (`push_video_wire` / `push_video_wire_to`) | ✅ | ✅ | ✅ | ✅ |
| Per-stream handle accessors (`video_handles()` / `video_stream_handle(i)` etc.) | ✅ | n/a (handles from config-time `add_*_stream`) | ✅ | ✅ |

**Notes on specific cells.**

- **C per-stream handle accessors** — the C ABI does not need runtime
  `video_handles()` accessors: the config-time `tst_mux_config_add_video_stream`
  returns the `tst_video_stream_handle_t` directly and the caller retains it.
  The "n/a" reflects that the accessor pattern is not applicable at the C ABI
  level, not that the information is unavailable.
- **AV1 mux carriage and the C targeted `*_to` family** — shipped in ABI 14
  (WP-B); Python `push_video_to_with_dts` and handle accessors — shipped in
  prior work. This PR (BIND-01/WP-I) completed DTS in C and targeted-push +
  handle-accessors + DTS in JVM to reach full parity.

A machine-checked version of this matrix — where a CI rail verifies that each
cell's claim matches the compiled binding — is a future enhancement noted in the
2026-06-15 codebase audit's "checked artifact" recommendation; it is deferred
pending a tooling decision on how to express cross-binding coverage assertions.

## Demux-config / managed-receiver-lifecycle parity matrix

The two receive-side capabilities added by ABI 0.21's managed-receiver
lifecycle parity (`tst_managed_demux_receiver_end_reason`) and demuxer-config
bridge (`tst_demux_config_set_unwrap_timestamps`) — see the ABI 21 history
entry above — reached full Python/JVM parity in the same-arc bindings
follow-up:

| Capability | Rust core | C (`tst-c`) | Python (`tst-py`) | JVM (`tst-jni`) |
|---|---|---|---|---|
| Continuous-timestamp unwrap across the 33-bit rollover (`DemuxerConfig::unwrap_timestamps`) | ✅ | ✅ `tst_demux_config_set_unwrap_timestamps` | ✅ `DemuxerConfig.unwrap_timestamps` | ✅ `DemuxerConfig.Builder.unwrapTimestamps(boolean)` |
| Managed-receiver end reason (`ManagedDemuxReceiver::end_reason_handle()` → `RecvEndReasonHandle` → `RecvEndReason`) | ✅ | ✅ `tst_managed_demux_receiver_end_reason` | ✅ `ManagedDemuxReceiver.end_reason() -> Optional[RecvEndReason]` | ✅ `ManagedDemuxReceiver.endReason()` → `RecvEndReason` or `null` |

**Notes on specific cells.**

- **C reuses the ABI 20 enum; Python/JVM define a distinct one.** The C
  getter maps `tst-pipeline`'s `RecvEndReason` (`EndOfStream` /
  `ReconnectExhausted` / `Cancelled`) onto the shared `TstStreamEndReason`
  enum the RTP surface already uses, to avoid adding a second wire enum
  (see the ABI 21 history entry's "Managed-receiver lifecycle" bullet).
  Python's `tstrans.srt.RecvEndReason` and JVM's `org.tstrans.srt.RecvEndReason`
  are each a standalone three-member enum (`END_OF_STREAM` /
  `RECONNECT_EXHAUSTED` / `CANCELLED`) rather than a reuse of
  `StreamEndReason` — the two concepts (RTP session-death reasons vs.
  SRT managed-receiver-lifecycle reasons) have different member sets and
  no binding shares one type between them.

## C stats-getter naming

The C ABI stats getter symbols (`tst_mux_sender_get_stats`,
`tst_demux_receiver_get_stats`, etc.) keep the `get_` prefix as a frozen
ABI-stable convention. New bindings targeting the Rust API directly should
follow each language's idiomatic convention (no `get_` prefix in Rust/Kotlin;
`get` prefix in Java).
