# Changelog

All notable changes to this project are documented in this file.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style.
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

- **`tst_core::codec::nal_framing`** — Annex-B ↔ length-prefixed NAL
  conversion for AVCC/HVCC-style consumers (Apple VideoToolbox and
  similar decoder APIs expect length-prefixed framing, not the
  start-code-delimited Annex B the demuxer emits):
  `annexb_to_length_prefixed(annexb: &[u8], length_size: u8)` /
  `length_prefixed_to_annexb(data: &[u8], length_size: u8)` round-trip
  through `length_size`-byte (1/2/4) big-endian length prefixes, and
  `extract_parameter_sets(annexb: &[u8], codec: VideoCodec) ->
  ParameterSets { vps, sps, pps }` pulls complete parameter-set NALs
  (header byte(s) included, ready for
  `CMVideoFormatDescriptionCreateFrom{H264,HEVC}ParameterSets` and
  equivalents) directly out of a `Sample`'s raw Annex-B bytes — no
  manual header reconstruction needed. Two additive `CodecParseError`
  variants, `InvalidLengthSize { got }` and `NalLengthOverflow {
  nal_len, length_size }`.
- **`DemuxerConfig::unwrap_timestamps`** (default `false`) +
  `DemuxerConfigBuilder::unwrap_timestamps` — opt-in per-PID unwrap of
  the demuxer's raw 33-bit 90 kHz PTS/DTS into a monotonic `i64`
  timeline, applied uniformly to `DemuxEvent::Sample` and
  `DemuxEvent::Metadata`. The offset is never rebased to zero, so a
  video PID and a KLV PID sharing one wire clock stay directly
  comparable across the ~26.5 h rollover — this is what lets a consumer
  pair KLV to video frames by PTS on a long-running stream. DTS
  unwraps against its own PES's PTS rather than the shared per-PID
  offset, so a DTS that straddles the wrap boundary (DTS still
  pre-wrap while its PES's PTS has already wrapped) lands in the
  correct epoch instead of over-shifting by a full `1 << 33`. The
  accumulator resets alongside the rest of the per-PID parse state on
  `Demuxer::reset_sync` — a reconnect restarts the unwrap timeline.
- **`tst-pipeline` recv-side stream-end reason** — `RecvEndReason`
  (`EndOfStream` / `ReconnectExhausted` / `Cancelled`) +
  `RecvEndReasonHandle` (first-writer-wins, readable after the owning
  receiver is dropped) via `ManagedDemuxReceiver::end_reason_handle()`
  — the recv-side analogue of `tst-rtp`'s `StreamEndReasonHandle`.
  Distinguishes reconnect-budget exhaustion from a caller-initiated
  cancel/close, which today both surface indistinguishably as
  `TST_E_END_OF_STREAM` / `TST_E_CLOSED`. Obtain the handle before
  moving the receiver into an opaque handle (e.g. a C binding's box) so
  a watchdog thread can poll it independently of the thread driving
  `recv_event`. Also adds `ManagedDemuxReceiver::reconnecting()` +
  `ManagedRecvTransport::reconnecting_handle()` to expose whether the
  inner connection is currently absent (mid-reconnect, or permanently
  after budget exhaustion).
- **C ABI 0.20 → 0.21 (additive).** C-callable MISB ST 0601 KLV decode:
  opaque `tst_st0601_t` (`tst_st0601_decode` / `_geometry` / `_get_f64`
  / `_get_u64` / `_state` / `_free`), the `tst_st0601_field_state` enum
  (present / absent / MISB sentinel / IMAPB special / wrong-type), a
  23-tag curated `tst_st0601_geometry_t` one-call getter, and two new
  error codes `TST_E_WRONG_TYPE` (-47) / `TST_E_KLV_DECODE` (-48).
  Annex-B ↔ length-prefixed and parameter-set C helpers:
  `tst_annexb_to_length_prefixed` (two-call sizing idiom) and the
  opaque `tst_param_sets_t` extraction trio
  `tst_param_sets_extract` / `_count` / `_get` / `_free`, wrapping the
  `tst_core::codec::nal_framing` additions above. Managed SRT
  demux-receiver lifecycle parity: `tst_managed_demux_receiver_end_reason`
  and `tst_managed_demux_receiver_get_reconnect_stats` close the
  send/recv asymmetry left by the 0.20 background-reconnect and
  stream-end-reason surfaces (both reuse existing 0.20 C types — no new
  struct or enum), plus `tst_demux_config_set_unwrap_timestamps` wiring
  the `DemuxerConfig::unwrap_timestamps` knob above through the C
  builder. See `docs/reference/binding-authors.md`'s ABI-21 entry for
  the full breakdown.
- **`bindings/c/include/module.modulemap`** beside the committed header,
  so a Swift/SPM/XCFramework consumer can `import TSTrans` directly
  without a bridging header. `tstrans.pc`'s `Libs` line is now
  platform-aware (`-lc++`/no `-ldl` on macOS vs `-lstdc++ -ldl` on
  Linux) instead of hardcoding the Linux shape.
- **`bindings/c/examples/recv_srt_events.c`** — reference example for
  the managed (auto-reconnecting) SRT demux receiver, the behavioral
  reference the Apple/Swift wrapper is written against: the full
  `tst_event_t` kind switch including `RECONNECT_DISCONTINUITY`,
  inline ST 0601 KLV decode via the new `tst_st0601_*` surface, and a
  cancel-from-signal-handler-then-close SIGINT shutdown sequence.
- **Apple iOS build tooling** — `scripts/apple/build-ios.sh` cross-compiles
  the `tst-c` static library + vendored libsrt (encrypted via the vendored
  mbedTLS backend) for `aarch64-apple-ios` and `aarch64-apple-ios-sim`, and
  `scripts/apple/make-xcframework.sh` assembles `TSTrans.xcframework` (macOS +
  iOS + iOS-sim, with the header + `module.modulemap`). Enabled by
  `srt-sys`'s build script setting the iOS cmake toolchain
  (`CMAKE_SYSTEM_NAME=iOS`) for `*-apple-ios*` targets. Gated by the manual
  `apple-ios` GitHub Actions workflow (`macos-14`). C-ABI path; the UniFFI
  Swift binding + SPM package remain deferred to `tst-uniffi`.
- **Seven new C teaching examples** under `bindings/c/examples/`, the
  C twins of the sender-side Rust examples that had none:
  `sending/send_srt.c` (TS-bytes SRT sender from a URL + a walk through
  the URL error vocabulary — also fills the `send_<proto>` grid's missing
  SRT entry), `sending/send_srt_encrypted.c` (passphrase/AES-256 send +
  receive in one process, all via URL keys), `sending/send_srt_ts_file.c`
  (PCR-paced relay of a recorded `.ts` — the caller-mode counterpart of
  `srt_serve_ts_file.rs`, since the C sender family is caller-only),
  `operations/managed_reconnect.c` and
  `operations/managed_reconnect_background.c` (`tst_managed_mux_sender_t`
  against a deliberately flaky peer in Blocking and Background modes,
  with `tst_reconnect_policy_t` built knob by knob and the reconnect
  stats printed live), `muxing/mux_to_file.c` and
  `muxing/mux_h265_with_klv.c` (the standalone `tst_muxer_t` to a file;
  both produce output byte-identical to their Rust twins). Every example
  cites its Rust twin and documents the C-ABI differences that shape it.
- **`scripts/check/c/examples-compile.sh` + a CI step**: every
  `bindings/c/examples/**/*.c` is now compiled and linked with
  `-Wall -Werror` against the all-features cdylib on the linux-x86_64
  leg. Until now only the scenario adapter was built; the other examples
  could rot against a header change unnoticed.

### Changed

- **Maintainer tool binaries renamed to kebab-case.** The ten `[[bin]]`
  targets that regenerate or inspect fixtures — `gen-synthetic-fixtures`,
  `gen-subtitle-fixtures`, `gen-h266-fixtures`, `gen-av1-fixtures`,
  `gen-pts-rollover-fixture`, `measure-pcr-jitter`, `corpus-to-fixture`,
  `strip-conformance-parameter-sets`, `trace-h265-sps` (tst-core) and
  `gen-scenarios` (tst-integration) — were `snake_case`, which nightly
  cargo's `non_kebab_case_bins` lint now flags on every run. Source files
  and behavior are unchanged; only the `cargo run --bin <name>` spelling
  moves. Not part of the library API. Older entries below quote the
  snake_case spellings that were current at the time; substitute the
  kebab-case name when running one of those commands on current HEAD.
- **tst-jni builds an `rlib` alongside its `cdylib`** (as tst-py already
  did), so `cargo test --doc --workspace` no longer warns that doctests
  are unsupported for the crate. Nothing links the rlib; `libtstjni.so`
  is the same build.
- **CI logs are warning-free, and stay that way where the toolchain is
  pinned.** Every tst-c single-feature combo now also runs
  `clippy -D warnings`, so feature-subset dead code in the C core fails
  the build instead of accumulating as warnings (16 such bodies had). The
  RTSP server's session-cap reserve is a plain compare-exchange loop now
  (drops nightly's `fetch_update` deprecation without needing `try_update`,
  which the 1.85 toolchain lacks; behavior identical). The 11 published
  manifests drop their explicit `readme` keys (cargo infers them), and the
  GitHub Actions majors moved off the deprecated Node 20 runtime.

### Fixed

- **tst-rist: stats callback registered before `rist_start`.** Both
  `RistTransport` and `RistRecvTransport` registered the librist stats
  callback *after* starting the session. librist 0.2.20's protocol
  thread re-reads the stats interval lock-free on every tick, so that
  order was a data race against the already-running thread (caught by
  the nightly TSan job on the first run after the 0.2.20 bump; four
  tst-rist loopback tests). Registration now precedes `rist_start`, the
  order librist's own tools use; a `rist_start` failure now also
  reclaims the callback's leaked `Arc` ref instead of leaking it. No
  API or behavior change for callers; the callback was verified to fire
  from the new position by instrumenting it in a local loopback run
  (`RistStats` has no public observable for callback execution on a
  lossless loopback — its inline counters are exact regardless).
- **C docs: managed-receiver budget-exhaustion contract.** The module
  doc and `tst_managed_demux_receiver_recv_event` (plus the sibling
  `tst_managed_receiver_recv_packet` / `tst_managed_raw_receiver_recv`)
  claimed reconnect-budget exhaustion surfaces as `TST_E_TRANSPORT` —
  it surfaces as `TST_E_END_OF_STREAM` (a latched inner `Closed`, the
  same code path a clean peer close takes; libsrt can't distinguish the
  two at this layer). `TST_E_TRANSPORT` is reserved for the
  inner-cancel-mutex-poisoned path. The demux-receiver family's doc now
  also points callers at `tst_managed_demux_receiver_end_reason` to
  disambiguate a clean teardown from a give-up-after-retries; the
  plain/raw families have no `end_reason` getter, so their docs just
  state the `TST_E_END_OF_STREAM` reality.
- **C docs: managed demux-receiver threading + timeout contract.**
  Documents the family's threading rules (`_cancel` is
  lock-free/any-thread/idempotent and unblocks a blocked `recv_event`
  within one libsrt I/O cycle; `_close` is not safe concurrently with a
  blocked `recv_event` — cancel, join, then close; which getters are
  lock-free side-channel reads vs. mutex-gated) and the
  `?x-recvtimeout=<ms>` URL extension (`SRTO_RCVTIMEO`; retryable
  `TST_E_BUFFER_FULL` on expiry; survives reconnect; does not bound
  `Listener::accept`).

- **Docs: decoder-replay parameter-set reconstruction.** `codec.md`
  claimed `NalUnit::{H264,H265,H266}.payload` / `raw_rbsp` include the
  NAL header byte(s) — they don't; the demuxer strips them before
  parsing. The worked Annex-B-reconstruction examples now rebuild the
  header from the NAL's type/ref_idc (H.264) or
  type/layer_id/temporal_id_plus1 (H.265) fields, and point at the new
  `extract_parameter_sets` above as the shortcut for the common case.
- **Docs: reconnect cache behavior.** `binding-authors.md` claimed
  `tst_managed_demux_receiver_*`'s post-reconnect `reset_sync()`
  preserves PAT/PMT and per-PID reassembly state across a reconnect —
  it clears it (PAT/PMT, continuity counters, last PCR/PTS, PES
  reassembly partials, the `NonConformant` dedupe sets); only aggregate
  stats counters and `DemuxerConfig` survive. Also fixes the constant
  name in two places (`TST_EVENT_KIND_RECONNECT_DISCONTINUITY`, not
  `TST_EVENT_RECONNECT_DISCONTINUITY`, which never existed).
- **Docs: SRT URL keys.** `srt.md` claimed `x-recvtimeout` bounds a
  Listener's `accept` call — libsrt ignores `SRTO_RCVTIMEO` on
  `srt_accept`; the key only bounds recv on a connected/accepted socket
  (and sets what accepted sockets inherit). Also: `mode=listener` is
  accepted, not rejected — only `mode=rendezvous` is unsupported.
- `examples/sending/sender_from_url.rs`'s "unsupported key" row used
  `?conntimeo=`, which the parser has accepted since the connect-timeout
  URL key landed, so the example printed "unexpectedly parsed OK" for
  that case; it now uses `?transtype=`, a key that is still deliberately
  unexposed. The new C twin `send_srt.c` mirrors the corrected table.

---

## [0.6.0] — 2026-09-01

Post-v0.5.1: the cross-binding parity bundle (receive deadlines,
background reconnect, structured stream-end reasons, last-activity
gauges, a tracing bridge — C ABI 0.19 → 0.20, additive), a security
vendor bump (libsrt 1.5.7, librist 0.2.20), the embedded receiver-path
arc (no_std receiver shells, a RISC-V runtime gate, on-device SRT
ingress), and a workspace-wide simplification audit (net −3,339 lines)
that also fixed two long-standing correctness bugs. Several
compile-breaking struct-field additions and API removals make this
0.6.0 — a `"0.5"` requirement will not auto-update into it.

### Release highlights

The integrator-facing changes at a glance (full detail in the sections below):

- **Why this is 0.6.0 (compile-time breaks):** `ReconnectPolicy` gained a
  `mode` field, `RtpUrl` / `RtspUrl` gained `recv_timeout`, and
  `StreamStats` gained `last_seen` — full struct literals need the new
  fields (`..Default::default()` callers are unaffected). `TsFraming::push`
  is now fallible. Removals: `UdpUrlError::BadHost` completes the
  deprecation cycle started at 0.5.0 (match `HostResolve` instead), plus
  nine never-constructed error variants across the transport crates and a
  set of zero-caller APIs (`klv::pack::encode_pack`, the SRT socket
  bandwidth setters, `RtspUrl::is_server_bind`, duplicate RTP builder
  aliases, `MountError::PeerBackpressure`, the unused interleaved-framing
  module).
- **Behavior change: clean RTSP server teardown is now a clean end of
  stream** (`Ok(None)` / `EndOfStream`), not `TransportBroken` — a match
  arm that conflated "the server ended the session" with "the wire broke"
  needs an end-of-stream arm; the new `end_reason()` disambiguates why a
  session ended.
- **Security:** libsrt 1.5.6 → 1.5.7 — KM message length validation,
  KMRSP encryption-downgrade prevention, forged-ACK / DROPREQ / FEC
  hostile-wire hardening, and a bonding use-after-free fix — statically
  vendored, picked up by rebuilding. And `hlss://` with no
  `?cert=`/`?key=` now fails validation instead of silently serving
  plaintext HTTP.
- **Receive deadlines everywhere:** the `?recv_timeout=<ms>` URL key on
  `rtp://` / `rtsp(s)://` receivers plus
  `H264Receiver::set_recv_timeout`. Expiry is a typed, retryable timeout
  in every binding (`RtpErrorKind.TIMEOUT` in Python,
  `RtpException.Kind.TIMEOUT` on the JVM, `TST_E_BUFFER_FULL` in C) — a
  stalled stream can now be detected from any language without an
  external watchdog thread.
- **Background reconnect:** opt-in `ReconnectMode::Background` on
  `ManagedTransport` — send-side enqueue during an outage instead of
  blocking the producer — with reconnect/gap statistics via
  `stats_handle()`, exposed across C, Python, and JVM; cancel/close now
  interrupts a backoff wait promptly in both modes.
- **Observability parity across C / Python / JVM:** structured
  `StreamEndReason` (`end_reason()` / `end_detail()`), per-stream
  `last_seen` wall-clock gauges, and the opt-in `TSTRANS_LOG` stderr
  tracing bridge.
- **`Sender`'s `max_unsynced_bytes` watchdog now actually fires** — the
  released knob was a documented no-op; a persistently non-TS source now
  errors instead of scanning forever.
- **Embedded:** the receiver shells (`Receiver` / `DemuxReceiver` /
  `RawReceiver`) now build `no_std`; RISC-V is promoted from
  compile-proof to a full QEMU runtime gate; and a new on-device
  SRT-ingress + demux gate brings the embedded CI surface to 12 gates.

### Added

- **TCP `SO_KEEPALIVE` knob on the RTSP client control socket** —
  `RtspClientBuilder::tcp_keepalive(Duration)` or `?tcp_keepalive=N`
  (seconds) on `rtsp://` / `rtsps://` URLs. The kernel probes an idle
  control connection so a peer that died without FIN/RST eventually
  errors the socket, instead of leaving it silently open forever.
  Complements (does not replace) the application-level receive
  deadline `RtpRecvTransport::set_recv_timeout`: keepalive only
  detects a dead TCP peer, not a stalled-but-connected one. The query
  key is client-local — it is never forwarded on the RTSP request
  line. Because it is a URL knob, it works from every binding today.
  Default: off (OS default), matching the `tcp://` transport's
  `?keepalive=` precedent.

- **`ReconnectMode::Background` on `ReconnectPolicy`** — opt-in
  reconnect for `ManagedTransport` that doesn't wait on backoff or
  factory calls. A per-outage worker thread owns the
  factory/backoff/drain loop; while it's active (or the gap buffer is
  non-empty) `send_bytes` enqueues under `overflow_policy` (`Ok(())`
  means *accepted*, not *delivered*) — it can still block briefly on
  lock contention while the worker is mid-drain, bounded to at most
  one in-flight inner send. If the worker exhausts
  `max_attempts` — which bounds one continuous outage, resetting after
  every successful reconnect — the give-up surfaces exactly once, as a
  single `Broken` on the next `send_bytes` call, whose own bytes are
  not queued. The default, `ReconnectMode::Blocking`, is unchanged:
  reconnect still runs synchronously on the caller's thread. Send-side
  only — `ManagedRecvTransport` / `ManagedDemuxReceiver` log a warning
  and behave as `Blocking` if handed `Background`. See the
  [cookbook recipe](docs/cookbook/operations/managed-transport-reconnect.md#background-mode-never-stall-the-producer).
- **`ManagedTransport::stats_handle()` + `ManagedStatsHandle` +
  `ManagedTransportStats`** — a cloneable, `Send + Sync` observer
  (obtain before moving the transport into a sender shell, mirroring
  `cancel_handle()`) exposing `reconnect_attempts`,
  `reconnect_successes`, `gap_len`, `gap_messages_dropped`,
  `gap_bytes_dropped`, and `reconnecting`.
- Cancel/close during a reconnect backoff wait now interrupts promptly
  in both `Blocking` and `Background` modes — a Condvar-based wait
  replaces the previous `thread::sleep`, so `close()` / `cancel()` /
  `Drop` no longer wait out the full backoff period.
- **`?recv_timeout=<ms>` query key on `rtp://` and `rtsp(s)://` URLs**
  (`RtpUrl.recv_timeout` / `RtspUrl.recv_timeout`) — a configured
  receive deadline in milliseconds (must be nonzero), applied at
  construction time everywhere a URL builds a receiver:
  `RtpRecvTransport::listen`, `H264Receiver::listen`, and both
  `RtspSession::into_recv_transport` / `into_h264_receiver` (via the
  `RtspUrl` the session was built from). Equivalent to an explicit
  `set_recv_timeout` call, but reachable from a URL string alone — no
  binding-specific plumbing needed. Client-local on `rtsp(s)://` —
  never rendered onto the wire (same shape as `tcp_keepalive` above).
  Absent == no deadline (unchanged default).
- **`H264Receiver::set_recv_timeout(Option<Duration>)`** — the
  persistent-deadline knob `RtpRecvTransport` already had
  (`set_recv_timeout`, v0.5.1), now on the H.264 AU-level receiver: a
  `recv_au()` call that assembles no complete AU within the configured
  timeout returns `Backpressure` with the session left valid. The
  existing one-shot `recv_au_timeout` ignores the knob — its explicit
  argument always wins for that call.
- **`StreamEndReason`** (`CleanTeardown` / `SessionExpired` /
  `KeepaliveFailed { msg }` / `TransportFailed { msg }` /
  `ProtocolError { msg }` / `Cancelled`) + `RtpRecvTransport::{end_reason,
  end_reason_handle}` + `H264Receiver::end_reason` +
  `StreamEndReasonHandle::get` — a structured, first-writer-wins record
  of why an RTSP-backed receive session ended, recorded at the site
  that actually observed it (the interleaved pump's exit paths, the
  keepalive thread's failure paths) rather than reconstructed after the
  fact from whatever terminal error a caller happens to see. Two
  previously-silent keepalive failure sites (the header-injection
  encode guard, the control-TCP write) now also emit a
  `tracing::warn!`. `None` means the session hasn't ended yet, or ended
  through a path this arc doesn't instrument (e.g. a plain `rtp://`
  transport that was never closed or cancelled).
- **`StreamStats.last_seen: Option<SystemTime>`** — wall-clock gauge
  stamped every time a stream carries an item through a `Muxer` push or
  a `Demuxer` emit. `std`-only (the field doesn't exist under `no_std`
  — no wall clock there).
- **`tst-pipeline`'s receiver shells now build `#![no_std]` + `alloc`.**
  `Receiver` / `DemuxReceiver` / `RawReceiver` join the sender shells
  (`MuxSender` / `Sender` / `RawSender`, no_std since 2026-05-31) under
  `--no-default-features`, so the crate's no_std surface now covers
  both directions of the pipeline. None of the three hold an internal
  lock (their hot-path methods take `&mut self`), so unlike
  `MuxSender` there's no spin-lock swap and the existing
  one-sender-per-task concurrency caveat doesn't apply to them; they
  do eagerly allocate a `transport.max_payload()`-sized scratch buffer
  at construction, so on a fixed-heap device `max_payload()` is a
  heap-sizing decision as well as a wire-protocol ceiling. Verified
  against the same bare-metal targets (`thumbv7em-none-eabihf` /
  `riscv32imac-unknown-none-elf`) as the sender path.
- **`embedded/baremetal-qemu` gains a fourth on-device check, `udp_recv`,
  and a second architecture, RISC-V (`qemu-system-riscv32 -machine
  virt`).** `udp_recv` drives a `no_std` `DemuxReceiver` over its own
  dedicated smoltcp loopback `RecvTransport` and recovers the pushed
  video AU(s) byte-exact — the runtime counterpart to the receiver-path
  `no_std` compile gate above. `riscv32imac-unknown-none-elf` moves from
  a compile-gate-only Tier 2 platform to a full QEMU runtime gate (same
  four checks as `thumbv7em-none-eabihf`, the same source unmodified on
  both instruction sets). `cargo run --release --locked --target
  riscv32imac-unknown-none-elf` from `embedded/baremetal-qemu/`, or
  `bash embedded/scripts/check/qemu-runtime.sh` (both architectures,
  per-target skip if the matching QEMU binary is absent).
- **`embedded/freertos-srt` gains a `srt-recv` gate — SRT ingress with
  on-device demux, the reverse of `example`.** A bare-metal SRT listener
  on the real lan9118 NIC accepts an inbound connection from a host
  `tst-srt` caller (over QEMU SLIRP `hostfwd=udp`), receives the golden
  video-roundtrip stream, and demuxes it on-device via the offline
  `tst_demuxer_*` C ABI, asserting both the event census and the raw
  video payload bytes — the firmware itself is the authoritative
  verifier here, unlike `example`, where the host listener verifies.
  `bash embedded/scripts/check/freertos-srt.sh srt-recv`; the 12th
  embedded CI gate (3 standalone scripts + 9 `freertos-srt.sh` targets).

### Changed

- **Vendored dependency updates: libsrt v1.5.6 → v1.5.7 (security),
  librist v0.2.18 → v0.2.20 (security audit), FreeRTOS-Kernel V11.3.0 →
  V11.3.1.** libsrt 1.5.7 is a security-hardening release: KM message
  length validation before internal conversion/copy routines, prevention
  of post-establishment KMRSP encryption downgrade, minimum-MSS
  enforcement against heap corruption, forged-ACK send-buffer protection,
  DROPREQ range validation, FEC payload-size checks, and a use-after-free
  fix in the bonding BACKUP send path (Haivision/srt#3359,
  Haivision/srt#3323) — all
  hostile-wire-input surface. Every build of `tst-srt` / `tst-c` / the
  Python wheels / the JVM JAR vendors this statically, so consumers pick
  the fixes up by rebuilding/upgrading. librist 0.2.20 stays
  ABI-compatible with 0.2.18 and carries an upstream tool/API
  memory-safety audit (parser over-reads, short-datagram rejection in the
  ipv4-mux receiver and OOB message handler, key-material wiping) plus
  new opt-in features (`cbr-output` pacing, RTT-based bonded-leg muting)
  that `tst-rist` does not yet expose; no build-system changes (the LZ4
  and meson option story is unchanged from 0.2.18). FreeRTOS-Kernel
  V11.3.1 is a patch release (SVC privilege checks, timer command ID
  validation, queue-set type verification). mbedTLS (3.6.7 LTS), lwIP
  (2.2.1) and FreeRTOS-Plus-POSIX were already at their latest upstream
  versions.
- **`embedded/freertos-srt`'s deterministic test-entropy hooks
  (`_getentropy` / `mbedtls_hardware_poll` in
  `substrate/syscalls_stub.c`) are now emitted only under
  `-DTST_QEMU_TEST_ENTROPY=1`** (which `build.sh` sets for every gate
  target here). `freertos-srt/` is a *reference port* — copying
  `substrate/` into a downstream firmware tree is its intended reuse
  path — so a downstream integrator who does that and rebuilds outside
  `build.sh` now gets undefined-reference link errors on those two
  symbols, instead of the previous behavior of silently linking the
  fixed-seed LCG that makes the `ENCRYPT=1` QEMU/CI gates reproducible.
  Deliberate fail-loud: wire a real hardware RNG or your board's
  approved entropy source in their place before enabling SRT encryption
  in production firmware — see the production-crypto warning in
  `embedded/freertos-srt/README.md` and `docs/languages/embedded.md`.
- **`ReconnectPolicy` gained a `mode: ReconnectMode` field.** Full
  struct literals (not using `..Default::default()`) need the new
  field. This is compile-breaking for those callers, so this
  release is 0.6.0.
- **`RtpUrl` and `RtspUrl` both gained a `recv_timeout: Option<Duration>`
  field.** Full struct literals need the new field — same
  compile-breaking shape as `ReconnectPolicy`'s `mode` field above.
- **`StreamStats` gained a `last_seen: Option<SystemTime>` field**
  (std builds only). Full struct literals need the new field.
- **`TsFraming::push` (RECOVER mode) is now fallible.** Was `fn
  push(&mut self, bytes: &[u8]) -> (Vec<Vec<u8>>, &SenderStats)`; is
  now `fn push(&mut self, bytes: &[u8]) -> Result<(Vec<Vec<u8>>,
  &SenderStats), TsFramingError>`, carrying the new
  `NoSyncAfterLimit` (see `### Fixed`) up through the framing
  primitive. `TsFraming` is classified Stable in
  `docs/reference/api-stability.md`, but it is not re-exported at the
  crate root (only reachable via `tst_pipeline::sender::TsFraming`)
  and has no known consumer outside `Sender` itself — which already
  absorbs the new `Result` internally, so `Sender::send_ts`'s own
  signature is unchanged. **Migration:** code calling
  `TsFraming::push` directly needs to handle the new `Result`, e.g.
  `let (bundles, stats) = framing.push(bytes)?;` or `.unwrap()` if
  RECOVER-mode errors aren't expected in that context.
- **Clean RTSP server teardown now ends a receive session as a clean
  end of stream, not `ShellErrorKind::TransportBroken`.** A
  `DemuxReceiver` / `Receiver` / `RawReceiver` wrapping a transport from
  `RtspSession::into_recv_transport` used to surface a server-initiated
  TEARDOWN (clean TCP EOF on the interleaved control/data connection)
  identically to a wire failure — both looked like the pump's `mpsc`
  channel disconnecting, and the shell had no way to tell them apart.
  It now reports `Ok(None)` (`ShellErrorKind::EndOfStream`) for the
  clean case; a hard read error or reset (RST) still surfaces as
  `TransportBroken`, unchanged. **Migration:** a `TransportBroken` match
  arm that was catching BOTH "the server ended the session" and "the
  wire broke" needs to add an `Ok(None)` / `EndOfStream` arm for the
  first case. `RtpRecvTransport::end_reason()` (new, see Added)
  disambiguates further if you need to know *why* — `CleanTeardown` vs.
  `TransportFailed` vs. `SessionExpired` vs. `Cancelled`. Keep the
  `RtspClient` alive, or drain the receiver, until end-of-stream is
  observed: dropping the client races the classification, since its
  `Drop` cancels the pump, which may record `Cancelled` before the
  server's EOF is read — that race reproduces the old `TransportBroken`
  shape.

Together with `ReconnectPolicy`'s `mode` field, the `RtpUrl` /
`RtspUrl` / `StreamStats` field adds above are all compile-breaking for
full-struct-literal callers, confirming this release as 0.6.0.

**C ABI minor 19 → 20** (additive; no existing symbol, signature, or
struct layout changed) brings the C surface most of the way to parity
with the Rust-core work above:

- `TstReconnectMode` (`Blocking` / `Background`) +
  `tst_reconnect_policy_set_mode` on `tst_reconnect_policy_t`; the
  48-byte `tst_managed_transport_stats_t` (size-pinned in the header
  trailer) + `tst_managed_{sender,mux_sender,raw_sender}_get_reconnect_stats`
  (`TST_HAS_SRT` — send-side only, matching Rust).
- `TstStreamEndReason` + `tst_rtp_{receiver,demux_receiver}_end_reason`
  (`TST_HAS_RTP`). An actually-recorded reason resets
  `tst_get_last_error[_str]` to `TST_E_SUCCESS` plus the detail message
  (or `""`) on every call, overwriting any pending failure; `None`
  touches last-error not at all — see `tst_get_last_error`'s doc. A
  future Rust `StreamEndReason` variant this binding doesn't know about
  yet aliases to `TstStreamEndReason::None` until the C mapping catches
  up in a later release.
- `tst_*_get_stream_last_seen_micros` on all six demux-receiver handle
  families (plain + managed SRT, RIST, RTP, TCP, UDP).
- `?recv_timeout=<ms>` is now honored by `tst_rtp_recv_open` /
  `tst_rtp_demux_receiver_open` (previously only the RTSP-converted
  path applied it) — no new symbols; expiry surfaces as the existing
  `TST_E_BUFFER_FULL` (-4), retryable, documented on
  `tst_rtp_receiver_recv_ts` / `tst_rtp_demux_receiver_next_event`.
  Fixed alongside: `?pkt_size=`/`?pt=` on `rtp://` receive URLs are
  now rejected at open as documented (previously silently ignored —
  both open functions rebuilt the socket from individual URL fields
  rather than the parsed URL, so query keys outside that short list
  never reached the validation that was supposed to reject them).

**Python bindings** (`tstrans`, additive — no existing call breaks;
`recv()` / `recv_au()` gain a new optional `timeout_ms=` keyword, but
every pre-existing call site keeps working unchanged) close most of the
same gap:

- `RtpErrorKind.TIMEOUT` (new `IntEnum` member) — `TransportError::
  Backpressure` now maps to `TIMEOUT` instead of `TRANSPORT` across
  `rtp.Receiver.recv()`, `rtp.DemuxReceiver`, and
  `rtp.H264Receiver.recv_au()`. Only raised when a deadline is actually
  configured; a receiver with none still blocks indefinitely.
- Per-call `timeout_ms=` on `Receiver.recv()` / `H264Receiver.recv_au()`
  (the one-shot `recv_timeout`/`recv_au_timeout`, explicit argument
  wins over any configured persistent deadline for that call) plus the
  persistent `?recv_timeout=<ms>` URL query key on `rtp://` receiver
  URLs, honored by both `Receiver` and `DemuxReceiver` — no separate
  Python setter, the URL is the only knob. Either path's expiry raises
  `RtpError(TIMEOUT)`; the receiver stays open, call again to retry.
- `ReconnectMode` (`BLOCKING` / `BACKGROUND`) + a `mode=` keyword on
  `ReconnectPolicy` + `reconnect_stats() -> ManagedTransportStats` on
  `ManagedSender` / `ManagedMuxSender`; `ManagedTransportStats` is a
  frozen, `get_all`-shaped mirror of the six Rust fields. Send-side
  only, matching Rust: `ManagedReceiver` / `ManagedDemuxReceiver` log a
  warning and behave as `BLOCKING` if handed a `BACKGROUND` policy.
- `StreamEndReason` (pure-Python `IntEnum`, numeric values pinned
  against the Rust/C enums) + `end_reason()` / `end_detail()` on
  `Receiver`, `DemuxReceiver` (rtp), and `H264Receiver`. Unlike the C
  ABI, which reads a `KeepaliveFailed` / `TransportFailed` /
  `ProtocolError` message through the shared thread-local last-error
  channel, `end_detail()` reads the Rust enum's `msg` field directly —
  a recorded end reason is data, not a failure, so nothing routes
  through `tstrans.exceptions`.
- `last_seen_micros(pid)` on `DemuxReceiver` (rtp and srt) and
  `ManagedDemuxReceiver` (srt) — an epoch-microsecond `int`, or `None`
  if `pid` was never seen. Deliberately differs from the C ABI's
  `0`-sentinel convention (the C getters have no `Option`) — Python's
  `None` is the honest "never" value.
- `TSTRANS_LOG` environment variable — an opt-in stderr `tracing`
  bridge installed at `import tstrans` time (`EnvFilter` syntax, same
  as `RUST_LOG`, e.g. `TSTRANS_LOG=tst_rtp=debug`). Unset means zero
  subscriber overhead beyond the env lookup; `try_init` (not `init`)
  so an embedding process that already installed its own subscriber
  keeps it.

See the [Python guide](docs/languages/python.md) for recipes.

**JVM bindings** (`tstrans-jvm`, additive — no existing call breaks;
`recv()` / `recvAu()` gain new overloads, but every pre-existing call
site keeps working unchanged) close the same gap:

- `RtpException.Kind.TIMEOUT` (new enum constant) —
  `TransportError::Backpressure` now maps to `TIMEOUT` instead of
  `TRANSPORT` across `rtp.Receiver.recv()`, `rtp.DemuxReceiver`, and
  `rtp.H264Receiver.recvAu()`. Only raised when a deadline is actually
  configured; a receiver with none still blocks indefinitely. Adding
  the constant meant adding the bucket-count assertion `RtpException`
  was missing (`RtpErrorModelTest`, mirroring the sibling
  `*ErrorModelTest` classes' `Kind.values().length` pin) and updating
  every `@throws` doc that exhaustively enumerated `Kind` — both fixed
  in lockstep with the addition, not after.
- Per-call `recv(Integer timeoutMs)` / `recvAu(Integer timeoutMs)`
  overloads (the one-shot deadline; explicit argument wins over any
  configured persistent deadline for that call) plus the persistent
  `?recv_timeout=<ms>` URL query key on `rtp://` (and `rtsp(s)://`)
  URLs, honored by `Receiver`, `DemuxReceiver`, and `H264Receiver.listen`
  — and, for a URL-configured RTSP session, carried through
  automatically by `RtspSession.intoDemuxReceiver()` /
  `intoH264Receiver()` too — no separate JVM setter, the URL is the
  only knob. Either path's expiry throws `RtpException(TIMEOUT)`; the
  receiver stays open, call again to retry.
- Public `DemuxReceiver.recvEvent()` — a checked receive that
  surfaces `RtpException` / `DemuxException` directly (including
  `TIMEOUT`) rather than the iterator-style `next()`'s wrapped
  unchecked `RuntimeException`, so a demux-side timeout is a typed
  `TIMEOUT` and not an ambiguous EOS-shaped `null`.
- `ReconnectMode` (`BLOCKING` / `BACKGROUND`) + `ReconnectPolicy.Builder#mode(ReconnectMode)`
  + `reconnectStats() -> ManagedTransportStats` on `ManagedSender` /
  `ManagedMuxSender`; `ManagedTransportStats` mirrors the six Rust
  fields. Send-side only, matching Rust: `ManagedReceiver` /
  `ManagedDemuxReceiver` log a warning and reconnect as `BLOCKING`
  regardless of what the policy asks for.
- `StreamEndReason` (Java enum, wire-value-pinned against the Rust/C
  enums — the class deliberately never uses Java `ordinal()`) +
  `endReason()` / `endDetail()` on `Receiver`, `DemuxReceiver` (rtp),
  and `H264Receiver`. Like Python and unlike the C ABI (which reads a
  `KeepaliveFailed` / `TransportFailed` / `ProtocolError` message
  through the shared thread-local last-error channel), `endDetail()`
  reads the Rust enum's `msg` field directly — a recorded end reason
  is data, not a failure. Because a Java handle is freed on `close()`
  (unlike Python's object, which can still answer after the underlying
  resource is gone), the private native teardown call now computes and
  returns the close-time `EndReasonSnapshot`, which the Java wrapper
  caches at the moment it exclusively owns the resource — the public
  `close()` itself stays `void`. `endReason()` / `endDetail()` keep
  answering correctly after `close()` — see `Receiver`'s Javadoc for
  the read-after-close contract.
- `lastSeenMicros(pid)` on `rtp.DemuxReceiver`, `srt.DemuxReceiver`,
  and `srt.ManagedDemuxReceiver` — an epoch-microsecond boxed `Long`,
  or `null` if `pid` was never seen. Deliberately differs from the C
  ABI's `0`-sentinel convention (the C getters have no nullable type)
  — matches Python's `None` convention.
- `TSTRANS_LOG` environment variable — an opt-in stderr `tracing`
  bridge installed from `JNI_OnLoad` (the one guaranteed one-time
  native-library entry point; `System.load` calls it before any
  `Java_org_tstrans_*` native is reachable), same `EnvFilter` syntax
  as `RUST_LOG`/Python's bridge (e.g. `TSTRANS_LOG=tst_rtp=debug`).
  Unset means zero subscriber overhead beyond the env lookup;
  `try_init` (not `init`) so a host process that already installed its
  own subscriber keeps it. ANSI color codes are gated on `stderr`
  actually being a terminal, so a redirected/captured stderr (e.g. a
  Gradle test's captured output) gets plain text.

See the [JVM guide](docs/languages/jvm.md) for recipes. All four
binding-parity gaps this arc closes — background reconnect, the
recv-deadline knob, the last-activity gauge, and the tracing bridge /
structured stream-end reason — are now resolved across C, Python, and
JVM; see the corresponding entries in
[docs/project/deferred-features.md](docs/project/deferred-features.md)
for the full history. (The JVM null-argument fix under **Fixed** below
is a separate, JVM-only behavior change from an earlier arc.)

- **`TcpListenerBuilder::from_url` now rejects a URL without `?listen=1`
  at construction**, as its own doc has always promised
  (`TcpUrlError::NotAListenerUrl`, new `#[non_exhaustive]` variant).
  Previously this check didn't run at all — a caller-shaped URL like
  `tcp://0.0.0.0:5000` was silently accepted and coerced into a
  listener URL by the time `build()` re-derived one internally. Use
  `TcpTransportBuilder::from_url` for caller-side URLs instead.

### Deprecated

- **`tst_core::io_file::DemuxFromFile`** — silently coerces read and
  demux-feed failures to early EOF, making a truncated read
  indistinguishable from a clean end of stream. Use
  `TryDemuxFromFile` instead, which surfaces the same failures via
  `Iterator<Item = io::Result<DemuxEvent>>`. Not removed — Stable-tier
  deprecation cycle per `docs/reference/api-stability.md`; removal no
  earlier than 1.0.

### Removed

- **`tst_core::klv::universal_label::UniversalLabel::SMPTE_336M_LS_KEY`**
  — byte-identical duplicate of `ST_0601_LS`, kept under a documented
  naming error ("SMPTE 336M generic local set key prefix" was
  inaccurate — all 16 bytes are the ST 0601 canonical UL) with zero
  in-tree consumers. Use `ST_0601_LS`.
- **`tst_core::klv::pack::encode_pack` and `tst_core::klv::length::LengthEncoding`**
  — `encode_pack` had zero callers (every typed KLV encoder builds its
  own TLV stream via `emit_ber_oid_tlv`); only its `BerOid` branch was
  reachable (`Ber` errored immediately, `BerShort`/`BerLong`/`Fixed`
  were never constructed), and `LengthEncoding` existed solely to
  parameterize it. Callers building a generic KLV pack use
  `UniversalLabel::new` + `klv::length::{write_ber, write_ber_oid}`
  directly.
- **`tst_srt::socket::Socket::{set_max_bandwidth, set_input_bandwidth,
  set_overhead_bandwidth_pct}`** — zero callers anywhere in the
  workspace or docs. The equivalent values remain settable via
  `SocketConfig`/`ListenerConfig` at construction time.
- **`tst_srt::error::SrtErrno::Timeout`** — never constructed
  (`SrtErrno::from_raw`'s major-category mapping has no path to it;
  redundant with `Async`, which is what `SRT_ETIMEOUT`'s raw code
  actually maps to). `SrtErrno::Bad` is kept — it's named in C-side
  error-detail prose as the sentinel for an absent errno.
- **`tst_udp::url::UdpUrlError::BadHost`** — deprecated since 0.5.0
  ("removal no earlier than 0.6.0"); this is that release. Superseded
  by `HostResolve` (which carries the resolver's failure detail and
  covers hostname resolution, not just IP literals) and never
  constructed since. Match `HostResolve` instead.
- **Nine never-constructed error variants across the network-transport
  crates**: `tst_udp::UdpError::{HostNotLiteral, IfaceUnsupported,
  PayloadTooLarge, Closed}`, `tst_rist::RistError::{PayloadTooLarge,
  Closed, RecvTimeout, Io}`, `tst_tcp::TcpError::PayloadTooLarge` —
  none had a construction site; each transport surfaces the equivalent
  failure through the shared `tst_core::transport::TransportError`
  instead, which the C/Python bindings already mapped to the same
  exception `kind` name. The `*ErrorKind` enumerators and `kind()`
  match arms are removed alongside. (`tst_tcp::TcpError::Closed` is
  unaffected — it IS constructed, by the Python binding.) The frozen C
  `TstError` enumerators these used to map to (`UdpPayloadTooLarge`
  = -28, `UdpIfaceUnsupported` = -29, `RistPayloadTooLarge` = -40) stay
  declared, unchanged, for ABI compatibility — their doc comments now
  read "reserved; not currently produced." Python-visible: two of the
  nine had no other reachable path to their exception kind name and are
  genuinely gone from the surface — `tstrans.exceptions.UdpErrorKind`
  loses `HOST_NOT_LITERAL` and `IFACE_UNSUPPORTED` (7 → 5 members); a
  caller matching on either name by attribute gets `AttributeError`.
- **`tst_rtp::rtsp::interleaved` (`Frame`, `InterleavedReader`,
  `InterleavedWriter`, `MAX_BINARY_FRAME_LEN`) and
  `RtspError::InterleavedFraming`** — a second, unused RFC 7826 §14
  framing implementation; both real interleaved-frame paths
  (`RtspClient`'s pump, the RTSP server's session loop) parse `$`-frames
  inline and never called it. The one caller was the crate's own fuzz
  target, which fuzzed dead code — it now drives the pump's real
  boundary-detection logic via new `#[doc(hidden)]` helpers instead
  (not part of the public API).
- **`tst_rtp::rtsp::server::interleaved_pump`** (the empty-from-outside
  server-side counterpart to the client pump) — the RTSP server never
  expects client→server `$`-frames (no ANNOUNCE/RECORD support in this
  release), so nothing constructed it; the real request loop reads
  plain RTSP bytes in `rtsp::server::session`.
- **`tst_rtp::rtsp::server::builder` / `tst_rtp::rtsp::server::runtime`**
  — two empty placeholder modules left over from Phase 3 planning, with
  no contents and no callers.
- **`RtpSocketBuilder::connect` / `RtpRecvSocketBuilder::listen`** —
  byte-identical duplicates of `build`, kept for an internal Phase 1
  builder shape that was never a shipped contract. Use `.build()`.
- **`RtspUrl::is_server_bind`** — four of its five disjuncts were
  subsumed by the fifth (`host.parse::<IpAddr>().is_ok()`), and it
  contradicted its own doc by treating non-loopback `127.x` hosts as
  bindable. Real callers already use
  `RtspUrl::validate_for_server_bind`, which reports a typed error
  instead of a bool.
- **`MountError::PeerBackpressure`** — never constructed (nothing
  measures per-peer broadcast lag as a push-time condition); all three
  bindings mapped it regardless. A lagging peer's dropped-frame count
  is tracked in `MountStats::frames_dropped_total`, not surfaced as a
  push error.

### Fixed

- **JVM: MISB natives now throw `NullPointerException` on null
  arguments instead of silently returning `null`.** Affected:
  `Klv.platformPositionXml` / `Klv.sensorPointOfInterestXml` (null
  `record` or `config`), `Klv.platformUid` / `Klv.spiUid` /
  `Klv.validateMismms` (null `record`), and `Klv.encodeCoreId` /
  `Klv.coreIdText` (null `id`). Previously jni's own null guard
  surfaced as a Rust-side error with no pending Java exception, so
  these calls returned `null` with no diagnostic — a documented
  stopgap since the MISB full-tag arc. The NPE message names the
  offending parameter; javadocs updated to match.

- **`Sender`'s `max_unsynced_bytes` knob now does what it's documented
  to do.** Previously the RECOVER-mode watchdog tracked bytes consumed
  while scanning for TS sync but never acted on the threshold —
  `TsFramingError::NoSyncAfterLimit` was defined and reachable from
  the C ABI setter (`tst_sender_config_set_max_unsynced_bytes`) but
  never constructed. `send_ts` now returns `NoSyncAfterLimit { max }`
  once scanning exceeds the configured threshold without acquiring
  sync. The count accumulates across `send_ts` calls until sync is
  acquired OR the watchdog fires, resetting to zero either way, so a
  persistently non-TS source trips it again every `max_unsynced_bytes`
  — how the garbage is chunked across calls can affect exactly when it
  first fires. Set
  `max_unsynced_bytes` to `usize::MAX` to effectively disable the
  watchdog. C-visible: `tst_sender_send_ts` can now return
  `TST_E_INVALID_TS` where it previously always returned 0 for a
  stream that never finds sync; a C caller cannot distinguish this
  from a STRICT-mode `SyncLost` (both route through the same
  `InputMalformed` → `TST_E_INVALID_TS` mapping — only the last-error
  string differs). See the `TsFraming::push` entry under `### Changed`
  for the accompanying signature break.

- **Security: `hlss://` with no `?cert=`/`?key=` no longer silently
  serves plaintext HTTP.** `HlsUrl.tls` (set from the `hlss` scheme)
  was parsed but never consulted — `HlsConfig::validate()` only
  rejected a half-configured cert/key pair, so an `hlss://` bind with
  neither param validated clean and bound plain HTTP with no
  indication that TLS was never applied. `HlsConfig` gains a `tls`
  field, set by `merge_from_url`; `validate()` now rejects `tls: true`
  without both `tls_cert` and `tls_key`. **Migration:** an `hlss://`
  bind that previously validated and served plaintext HTTP now fails
  at `HlsPublisher::with_config` / `HlsPublisherBuilder::build` with
  `HlsError::InvalidConfig` — supply both `?cert=` and `?key=` (or
  call `.enable_tls(cert, key)` on the builder before `.build()`).

---

## [0.5.1] — 2026-08-18

A small, additive patch release: one new Rust API on the RTP receive
path, from an integrator field report. No breaking changes — `"0.5"`
consumers pick it up on a normal `cargo update`. C ABI unchanged at
0.19; Python/JVM surfaces unchanged (version bump only).

### Release highlights

- **Deadline-driven stall watchdogs on RTP/RTSP receive pumps, no
  cancel thread.** `RtpRecvTransport::set_recv_timeout` configures a
  persistent receive deadline that the blocking `recv_bytes` trait path
  honors, so `DemuxReceiver` / `Receiver` / `RawReceiver` surface a
  stalled-but-healthy session (peer stops sending; no error, no EOS)
  as their retryable `Backpressure`-kind error — the same shell-visible
  contract SRT's configured receive timeout has always had. Set it on
  the transport returned by `RtspSession::into_recv_transport` before
  wrapping it in a shell.

### Added

- **`RtpRecvTransport::set_recv_timeout(Option<Duration>)`** (from an
  integrator field report — the same stalled-but-healthy session class
  0.5.0's one-shot deadlines addressed). A persistent receive deadline
  honored by the blocking trait `recv_bytes` path: on expiry it returns
  `TransportError::Backpressure` with the transport left alive, so the
  receive shells (`DemuxReceiver`, `Receiver`, `RawReceiver`) surface
  the stall as their retryable `Backpressure`-kind error — the same
  shell-visible contract SRT's builder-configured `recv_timeout`
  (`SRTO_RCVTIMEO`) has always had. This makes a `for ev in
  DemuxReceiver` pump over an RTSP/RTP session deadline-drivable with
  no cancel thread: set the timeout on the transport returned by
  `RtspSession::into_recv_transport` before wrapping it. Default `None`
  keeps the existing infinite block; granularity is the ~100 ms
  cancel-poll interval; `Duration::MAX` saturates to "no deadline"; the
  one-shot `recv_timeout` method ignores the configured value (its
  explicit argument wins for that call). `ManagedRecvTransport`
  propagates the expiry unchanged — a recv timeout is not a reconnect
  trigger. Python/JVM/C mirrors ride the existing "RTP receive-deadline
  bindings parity" entry in `docs/project/deferred-features.md`.

---

## [0.5.0] — 2026-08-09

Post-v0.4.0: two integrator field-report remediation arcs, the interop
evidence arc's library-bug fixes, the first published 72-hour soak run,
client-only (tokio-free) RTSP builds behind a new feature split, and two
breaking renames. C ABI unchanged at 0.19.

### Release highlights

The integrator-facing changes at a glance (full detail in the sections below):

- **One breaking change, one deprecation** (compile-time only): the `tls`
  cargo feature no longer enables the RTSP **server's** TLS acceptor —
  server TLS moved to the new `rtsp-server-tls` feature, with the whole
  push server now behind a default-on `rtsp-server` feature (breaking for
  server-TLS builds; a `"0.4"` requirement will not auto-update into it).
  On the Stable `tst-udp` surface, `UdpUrlError::BadHost` is **deprecated,
  not removed** — new hostname-resolution failures report through the
  richer `HostResolve` (and a new `SendRecvMismatch` covers send/recv
  entry-point mixups); v0.4 code naming `BadHost` still compiles, and the
  variant is removed no earlier than 0.6 per the published Stable-tier
  deprecation policy.
- **Client-only RTSP builds are tokio-free.** `default-features = false`
  (+ `tls` for `rtsps://`) builds the RTSP/RTP client plane with zero tokio
  in the dependency tree — sized for mobile/embedded consumers — and a
  standing CI leg now pins both the build and the tokio-free invariant.
- **`DemuxReceiver` sessions on ≤ 0.4.0 drop the final buffered video
  access unit when the transport ends non-cleanly** (break, cancel, close —
  anything but a clean end-of-stream; the pending-AU flush only ran on
  `EndOfStream`). Found by the interop soak harness; the teardown path now
  flushes the last pending AU, with the truncated-sample tradeoff disclosed
  in the implementation docs. If you have seen a one-frame-short capture
  through `DemuxReceiver`, this was it.
- **A rarely-lost race in `Listener::accept_timeout`** could hand back a
  connection whose first receive failed fatally (non-blocking mode inherited
  from the accept probe). Fixed; only ever reproduced on loaded CI runners.
- **Field-report remediation, second and third integrators** — the full ask
  lists from both reports shipped: unconnected UDP sends (the spurious
  `ECONNREFUSED` failure class is structurally gone), RTSP-client
  poison-recovery (a panic mid-session no longer aborts the process from
  `Drop`), a new fallible
  `MuxSender::finish()` delivers the buffered tail on explicit shutdown
  (superseding the drop-don't-close workaround; `close()` stays the
  prompt cancel-first primitive, unchanged), deadline-bounded receives
  (`H264Receiver::recv_au_timeout`, `RtpRecvTransport::recv_timeout`),
  `SETUP` 400 diagnostics, `udp://` hostname resolution, DTS-capable mount
  pushes, `SecretString` re-export, `RtspClientBuilder::transport_preference`,
  and a documented+pinned `Send` guarantee on the client types.
- **`flow_window_packets` actually works for buffer sizing now**: both
  config-apply paths set `SRTO_RCVBUF` before `SRTO_FC`/`SRTO_MSS`, so libsrt
  silently clamped large receive buffers to the *default* flow-control window
  even when you raised it. Ordering fixed, and a silent clamp now emits a
  `tracing::warn!` naming the effective size and the lifting knob.
- **The first full 72-hour soak run is published** in
  `docs/project/validation-evidence.md`: overall PASS — zero process exits,
  12/12 outage-window reconnects, statistically clean drop rates on both
  legs, and flat memory on all six processes, at ~1.9 Mb/s of GOP-structured
  traffic per leg through a seeded impairment proxy.


### Added

- **`MuxSender::finish()` + `FileTransport::finish()` — fallible graceful
  shutdown pair** (release-gate audit). `MuxSender::finish` drains
  `pending_bytes` to the live transport and reports the drain outcome
  before closing (see the Fixed entry on explicit-shutdown tail delivery);
  `FileTransport::finish` flushes the userspace buffer and surfaces the
  I/O error the infallible `close()`/`Drop` paths must swallow — a
  flush-only failure (ENOSPC, EIO, removed device) previously lost the
  capture tail silently after every send had reported success.
  `FileTransport::bytes_sent` is now documented as accepted-not-flushed
  bytes. Bindings parity for both is deferred
  (`docs/project/deferred-features.md`).
- **`UdpUrlError::FamilyMismatch`**: `?localaddr=` and the peer address
  must agree on IP family; a literal mismatch is now a typed parse error
  and hostname resolution filters candidates to the local family (a
  dual-stack hostname with `?localaddr=<IPv6>` previously chose an IPv4
  peer, bound an IPv6 socket, and failed at the first send with an opaque
  OS error). `UdpTransport::with_config` applies the same construction-time
  check for direct `SocketConfig` users.
- **`SRTO_RCVBUF` silent-clamp warning** (tst-srt, integrator field ask):
  libsrt accepts any receive-buffer request but silently clamps the stored
  size to the flow-control window (`SRTO_FC`, default 25600 packets —
  ≈ 37.7 MB at default MSS) while still reporting success. Applying a
  config now reads the effective value back and emits a `tracing::warn!`
  naming requested vs. effective bytes and the lifting knob
  (`flow_window_packets`) when the shortfall exceeds the one-packet
  rounding libsrt's bytes→packets conversion always incurs. The kernel-side
  silent clamp on `SRTO_UDP_RCVBUF`/`SRTO_UDP_SNDBUF`
  (`net.core.{r,w}mem_max`) is documented on the config fields instead —
  that one is invisible to libsrt readback.

- **`H264Receiver::recv_au_timeout(Duration)`** (from an integrator field
  report). Deadline-bounded AU receive for stall watchdogs — a stalled-but-
  healthy session (server stops sending, no error, no EOS) previously
  blocked `recv_au` forever with no way for the caller to regain control.
  Returns `Err(TransportError::Backpressure)` on deadline expiry (the
  session stays valid; call again to keep waiting); deadline granularity is
  the internal cancel-poll interval (~100 ms).
- **`RtpRecvTransport::recv_timeout(&mut self, buf, deadline: Duration)`**
  (same field report). Deadline-bounded raw receive, aligning RTP with the
  UDP transport's periodic-return convention: `Ok(None)` on expiry,
  transport still alive. `recv_bytes`'s infinite-block behavior is
  unchanged. Bindings parity (Python/JVM/C) is deferred — see the "RTP
  receive-deadline bindings parity" entry in
  `docs/project/deferred-features.md`.
- **DTS-capable video push on RTSP server mounts** (PR #138, from an
  integrator field report). `MountHandle::push_video_to_with_dts(handle, nal,
  pts, dts, key_frame)` mirrors `MuxSender::send_video_to_with_dts` —
  targeted-only, like every DTS push in the workspace; obtain the handle from
  `video_handles()`. Streams served from a mount were previously always
  DTS == PTS, so a capture replayer could not reproduce real encoder timing
  (e.g. a constant PTS−DTS offset) through the RTSP server. The
  drain-and-broadcast contract is unchanged, and the muxer still does not
  enforce `dts <= pts` (same caller invariant as `MuxSender`). The Python,
  JVM, and C mount push surfaces remain PTS-only for now — see the "RTSP
  mount push-surface parity" entry in `docs/project/deferred-features.md`.
- **`udp://` URLs accept DNS hostnames** (PR #137, same field report).
  `UdpUrl::parse` — and therefore `UdpTransport::connect`,
  `UdpRecvTransport::listen`, the builders, and every binding — now resolves
  non-literal hosts via the system resolver instead of failing with a
  literal-address error, matching tst-srt and tst-tcp. Candidates are
  probe-walked with a local `bind` + `connect` (no packets are sent), so a
  resolver returning an unroutable-family address first (e.g. AAAA on a
  v6-disabled host) is skipped rather than breaking the send path; if every
  probe fails the first resolved address is kept so the real send surfaces
  the OS error. Multicast group addresses and the `?localaddr=` / IPv4
  `?iface=` selectors stay literal-only.
- **RTSP server SETUP rejections are now diagnosable from logs** (PR #136,
  same field report). A malformed `Transport:` header still gets the bare
  `400 Bad Request` on the wire, but the server logs the missing-header vs
  malformed-header distinction (target `tst_rtp::server`) and the shared
  range parser logs exactly which check failed — non-integer port/channel,
  non-ascending pair, companion overflow, or token count (target
  `tst_rtp::rtsp::transport_negotiation`, `debug!` level). Wire-supplied
  values are Debug-escaped before logging.
- **`TransportError::is_connection_refused()`** — portable refused-
  classification without hard-coding platform errnos (111 Linux / 61 BSD /
  10061 Windows); classifies via the std `io::ErrorKind` mapping over
  `Backpressure`/`Broken`'s `errno_code`. `std`-gated (the type stays
  no_std-buildable).
- **`MuxSender::into_inner()`** — recover the owned transport (best-effort
  pending drain, transport left open). Requested by an integrator so a
  `MuxSender` used for capture/setup can later hand its live transport off
  to different code without a full teardown/rebuild.
- **`ext::file_transport::FileTransport`** — write-to-file `Transport` for
  capture/debug sinks. Both binding consumers and integration ports had
  independently rebuilt this ad hoc; shipping one canonical version.
- **`tst_rtp::SecretString` re-export**, from an integrator field report.
  Consumers building `RtspClientBuilder` credentials no longer need a
  direct dependency on the `secrecy` crate just to name the type.
- **`RtspClientBuilder::transport_preference(RtspTransportPref)`**, same
  field report — a typed alternative to the `?transport=tcp|udp` URL query
  for forcing the SETUP transport. The builder value wins over the URL
  query when both are present.

### Changed

- **`UdpTransport` now sends on an unconnected socket (`send_to`)**, from
  an integrator field report reproduced against a live media server: a
  transient ICMP port-unreachable from a restarting or idle receiver no
  longer kills the sender with a fatal `Broken`/ECONNREFUSED. Fire-and-
  forget datagram semantics, matching every TS-over-UDP sender (ffmpeg,
  VLC, mediamtx). No knob — this is the only mode.
- **`udp://` hostname resolution now prefers IPv4 among reachable-family
  candidates.** A UDP connect-probe can reject an unconfigured/unroutable
  address family but cannot detect an absent listener, so `localhost`
  resolving `[::1, 127.0.0.1]` on a dual-stack host previously picked
  `::1` by resolver order and died against an IPv4-only listener.
- **Deprecated: `UdpUrlError::BadHost`** (softened from outright removal at
  the release gate — `tst-udp` is a Stable-tier surface, and the published
  policy in `docs/reference/api-stability.md` promises Stable breaking
  changes a deprecation cycle). The variant is never constructed since
  0.5.0: unresolvable or junk hosts now surface as
  `UdpUrlError::HostResolve { host, detail }`, and passing an
  `@`-prefixed (recv-bind) URL to the send-side builder — which previously
  misreported through `BadHost` — gets the purpose-specific
  `UdpUrlError::SendRecvMismatch`. **Migration:** replace `BadHost(host)`
  match arms with `HostResolve { host, .. }`; removal no earlier than 0.6.
  The C ABI and error-kind mappings are unchanged (URL errors ride the
  existing `Url` kind); C ABI minor stays 19.
- **`tst-rtp`: new default-on `rtsp-server` feature gates `RtspServer`
  (and tokio)**. `tst-rtp` was already a sync facade everywhere except
  `RtspServer`, which runs an internal tokio Runtime; that Runtime is now
  the only thing that pulls tokio into the crate's dependency tree, and it's
  behind this feature. Client-only consumers (e.g. an upcoming UniFFI
  mobile binding shipping RTP/RTSP client + SRT) build with
  `default-features = false` (`+ "tls"` for `rtsps://`) for a tokio-free
  dependency tree. **Breaking:** the `tls` feature no longer enables the
  server's TLS acceptor — server-TLS consumers enable the new
  `rtsp-server-tls` feature (implies both `rtsp-server` and `tls`) instead.
  An `rtsps://` server bind built with `rtsp-server` but not
  `rtsp-server-tls` fails `RtspServer::start` with the existing
  `RtspServerError::Tls` variant (message updated, no new variant). No C ABI
  change (`tst-c` always enables `rtsp-server`); the JVM and Python bindings
  now enable `rtsp-server-tls` explicitly (they ship the server's TLS
  acceptor already).

- **Docs:** **SETUP → PLAY → `into_h264_receiver` call order**, from an integrator
  field report. `RtspSession::into_h264_receiver` and `RtspClient::play`
  now spell out that PLAY must be issued before converting the session to
  a receiver, and what happens if it isn't — `H264Receiver::recv_au` just
  blocks waiting for AUs the server hasn't started sending yet. Previously
  discoverable only by reading the integration tests.
- **Docs:** **`Send` guarantee on `RtspClient` / `RtspSession` / `H264Receiver`**,
  same field report. Moving one of these to a dedicated receive/watchdog
  thread is now a documented, supported use — pinned with a compile-time
  regression test, not just an implementation accident.

### Fixed

- **`H264Receiver::recv_au_timeout` / `RtpRecvTransport::recv_timeout` no
  longer panic on extreme durations** (release-gate audit): the deadline
  arithmetic uses `checked_add` — a timeout too large to represent as an
  `Instant` (e.g. `Duration::MAX`) behaves as "no deadline" instead of
  panicking, `Duration::ZERO` expires at the first poll, and both
  behaviors are pinned by tests. `recv_timeout`'s parameter is renamed
  `deadline` → `timeout` (it is a relative duration; API introduced this
  release, so no compatibility impact).
- **`flow_window_packets` was a no-op for receive-buffer sizing** (tst-srt):
  socket and listener config application set `SRTO_RCVBUF` before `SRTO_FC`
  and `SRTO_MSS`, but libsrt clamps and converts the buffer against the
  window and MSS in effect *at set time* — so raising `flow_window_packets`
  together with a large `recv_buf_bytes` still left the buffer clamped to
  the default window. Options are now applied MSS → FC → buffers;
  regression-tested against the vendored libsrt.

- **Explicit shutdown can now deliver the buffered tail: new
  `MuxSender::finish()`** (interop-arc finding, reshaped at the release
  gate). `close()` cancels the transport before draining, so on SRT/RIST
  the pending-bytes drain sends failed and the tail of a stream was lost
  on an explicit close even though `Drop` (which drains first) delivered
  it. Rather than change `close()`'s Stable contract (prompt, cancel-first,
  ~3-10 ms wake — an emergency primitive must never block on a slow or
  dead network), the lossless path is a new, separately named
  `finish() -> Result<(), MuxSenderError>`: it drains `pending_bytes` to
  the still-live transport FIRST — reporting a drain failure instead of
  swallowing it — then closes; it may block like `Drop`, and a watchdog
  holding `cancel_handle()` can unblock it. `close()` behavior is
  unchanged from 0.4.0. The "drop, don't close" workaround from the
  interop-evidence arc is superseded by `finish()` — which is strictly
  better, because unlike `Drop` it tells you whether the tail actually
  made it. Bindings parity for `finish` is deferred (see
  `docs/project/deferred-features.md`).
- **`tst-srt`: a connection accepted via `Listener::accept_timeout` could
  permanently inherit non-blocking mode and die on its first read with
  zero delivery.** `accept_timeout`'s readiness probe briefly toggles the
  listener socket into non-blocking mode, and libsrt's own handshake
  thread clones the listener's option state into newly accepted sockets
  asynchronously — a connection completing its handshake inside that
  window picked up `SRTO_RCVSYN=false`, its first `recv` returned
  `SRT_EASYNCRCV` immediately, and the error was classified as fatal,
  killing the session before any data flowed. Timing-dependent (observed
  only on heavily loaded hosts). `Socket::from_accepted` now pins every
  accepted socket back to blocking mode before timeouts are applied, so
  the race is closed at the one construction point all accept paths share.
  No public API changes.
- **`DemuxReceiver`: the final access unit of a live receive session was
  silently dropped whenever the session ended any way other than a clean
  peer close.** H.264 video is muxed with `PES_packet_length = 0`
  ("unbounded" — the encoder doesn't know an access unit's length up front),
  so the demuxer only recognizes the last AU is complete when the next PES
  starts or the caller explicitly flushes. `DemuxReceiver::recv_event` only
  flushed on a clean `TransportError::Closed` (peer end-of-stream); a broken
  socket (`ShellErrorKind::TransportBroken` — the common case for a live SRT
  session ending) or a caller-initiated cross-thread `close()`/`cancel()`
  (`ShellErrorKind::Closed`) bypassed the flush entirely, so the buffered
  final AU was lost even though the bytes had fully arrived. Both paths now
  flush before surfacing the error, matching the existing clean-EOF
  behavior. `Demuxer::flush` is PID-agnostic, so any other stream (audio,
  KLV) with bytes still buffered when the transport ends benefits the same
  way; on a genuine mid-stream break the recovered sample can be a truncated
  access unit rather than a complete one, the same trade-off already
  accepted on the clean-EOF path. No public API changes.
- **RTSP server: the `active_sessions` stats gauge could transiently read
  `max_sessions + 1`** while an over-cap connection was being refused. The
  accept loop reserved a slot with an increment, bound-checked, then released
  on refusal — and `RtspServer::stats()` reads the same counter, so a poll
  landing between the two operations observed an impossible value. The
  reservation is now a compare-and-swap that only increments while below the
  cap, so the gauge can never exceed `max_sessions`, even transiently. Cap
  *enforcement* was never affected: accepted sessions never exceeded the
  configured limit, and refused connections were always dropped.
- **RTSP client: teardown, `Drop`, and request paths no longer panic when a
  background thread poisoned an internal mutex** (poison is recovered; a
  prior keepalive-thread panic can no longer abort the process via
  panic-in-panic during `Drop`), from an integrator field report.

---

## [0.4.0] — 2026-07-31

Post-v0.3.0: RTSP keepalive overhaul, MISB ST 0601 full-tag compliance,
crates.io publish readiness + API stability tiers, native-boundary sanitizer
coverage, vendored security updates.

### Release highlights

The integrator-facing changes at a glance (full detail in the sections below):

- **The 16.5-minute RTSP session drop is fixed** (PR #122). Every
  TCP-interleaved receive session (RTSPS included) died at ~16.5 minutes
  looking like a clean end of stream: the client's own keepalive responses
  overflowed an internal queue nothing drained. All prior releases are
  affected. Keepalive responses are now consumed, pings carry the `Session:`
  header, the cadence follows a server-advertised `timeout=`, and mid-session
  digest re-challenges refresh. If you worked around the drop with
  `keepalive=False`, remove the workaround on 0.4.0. A structured stream-end
  reason and a Python/JVM `tracing` diagnostics bridge are deferred —
  tracked in `docs/project/deferred-features.md`.
- **First crates.io publish** — all 11 Rust library crates are now on
  crates.io (`tst-core`, `tst-pipeline`, `tst-srt`, `tst-rtp`, `tst-udp`,
  `tst-tcp`, `tst-hls`, `tst-rist`, `tstrans-srt-sys`, `tstrans-rist-sys`,
  `tstrans-mbedtls-src`); git dependencies are no longer required. The two
  `-sys` packages are renamed (`tstrans-` prefix) but keep their library
  names, so `use srt_sys::` / `use rist_sys::` code compiles unchanged.
- **Public API classified into stability tiers** —
  `docs/reference/api-stability.md` maps every public module to
  Stable / Provisional / Experimental / Internal, with matching rustdoc
  headers, so pre-1.0 consumers can see what is safe to build on.
- **Send calls report input consumption** — `MuxSenderError` / `SenderError`
  now carry `input_consumed: Option<bool>`, replacing the discriminate-by-
  previous-call retry contract (bindings parity deferred).
- **Vendored security updates** — libsrt 1.5.5 → 1.5.6 (fixes
  hostile-wire-reachable KMREQ/KMRSP/LOSSREPORT/DROPREQ parsing bugs,
  including a CVE), librist 0.2.16 → 0.2.18, mbedTLS 3.6.6 → 3.6.7.
- **RIST on Windows is now fully supported** — librist 0.2.18 fixed the
  teardown hang and zero-delivery bugs; the Windows test gate is lifted
  (IPv6 multicast remains the only Windows-gated case).
- **MISB ST 0601 typed end to end** (PRs #116–#120) — 142 of 143 spec items
  typed (up from 52), plus `klv::st1010` SDCC-FLP, `klv::st0806` RVT, and
  `klv::st0805` KLV → Cursor-on-Target conversion, mirrored in Python + JVM.
- **RTSP hardening from the field** (PRs #110–#115) — the client
  authenticates every method (not just DESCRIBE/SETUP), `RtspServer::start()`
  surfaces bind/TLS errors instead of dying silently, TLS config on a
  plaintext bind is refused cross-surface, JVM gains full TLS parity, and
  Python wheels ship with TLS + RIST enabled.
- **Security policy** — `SECURITY.md` with private disclosure channels
  (GitHub private vulnerability reporting is enabled on the repo).

Landed as PRs #110–#132. The C ABI stayed frozen at minor **19** for the
whole span — no symbol, signature, or struct-layout changes; C consumers
rebuild against 0.4.0 without source changes.

### Rust crate packaging: crates.io publish readiness

#### Changed

- `srt-sys` renamed to `tstrans-srt-sys`; `rist-sys` renamed to
  `tstrans-rist-sys`. Both keep their library name (`srt_sys` / `rist_sys`,
  so every existing `use srt_sys::...` / `use rist_sys::...` call site
  compiles unchanged) and their Cargo dependency **key** inside the
  workspace (`srt-sys` / `rist-sys`, so internal `Cargo.toml` files and
  feature forwards like `mbedtls = ["srt-sys/mbedtls"]` are untouched) —
  only the published package name changed. Git consumers depending on
  either crate directly need to add the `package` key to keep resolving
  the same crate under the old dependency name:
  ```toml
  srt-sys  = { package = "tstrans-srt-sys",  git = "https://github.com/aklofas/ts-transformer" }
  rist-sys = { package = "tstrans-rist-sys", git = "https://github.com/aklofas/ts-transformer" }
  ```
- Both `-sys` crates now bundle their native source tree directly instead
  of resolving a shared workspace-root `vendor/`: the libsrt submodule
  moved to `crates/srt-sys/vendor/srt`, and the librist submodule moved to
  `crates/rist-sys/vendor/librist`. A new `tstrans-mbedtls-src` crate does
  the same for the shared vendored Mbed TLS tree (moved from the old
  workspace-root `vendor/mbedtls` to `crates/mbedtls-src/vendor/mbedtls`),
  and is now a build-dependency of both `-sys` crates instead of each
  resolving a shared relative path independently. This is what makes each
  crate a self-contained, publishable package — crates.io does not allow a
  package to reference files outside its own directory.

#### Added

- New crate `tstrans-mbedtls-src` — a build-time-only source provider (no
  bindings, no compiled code) exposing `source_dir()` so
  `tstrans-srt-sys`'s and `tstrans-rist-sys`'s build scripts can compile the
  bundled Mbed TLS sources with their own flags.
- `tst-core`, `tst-pipeline`, `tst-udp`, `tst-tcp`, `tst-hls`, `tst-rtp`,
  `tst-srt`, `tst-rist`, `tstrans-srt-sys`, `tstrans-rist-sys`, and
  `tstrans-mbedtls-src` (11 crates) are now packaged for crates.io
  publication (`readme`/`keywords`/`categories` metadata, per-crate
  `README.md`, versioned path dependencies) — starting **v0.4.0** they are
  resolvable directly from crates.io (`cargo add tst-core`, etc.) rather
  than only via a `git` dependency. See `docs/project/releasing.md`'s new
  "crates.io publish" section for the publish order and procedure.
- `publish = false` set explicitly on the 4 binding crates (`tst-c`,
  `tst-c-core`, `tst-py`, `tst-jni`) — none of these are meant to be
  depended on directly from crates.io; their consumer contract is the C
  ABI / PyPI wheel / Maven JAR, not a Rust `Cargo.toml` dependency.
- CI rail `publish-package-sanity`
  (`scripts/check/rust/publish-package-sanity.sh`) guards the bundled-vendor
  packaging: submodules actually checked out, no nested `Cargo.toml`
  silently dropping a subtree, required native-build/license file sentinels
  present, deliberately-excluded subtrees (nested dev-tool submodules,
  unused bundled fallback sources) absent, and compressed package size
  under crates.io's budget.

### Public API classified into stability tiers

#### Added

- `docs/reference/api-stability.md` classifies every top-level public
  module across the 10 `cargo public-api`-ratcheted crates into a
  Stable / Provisional / Experimental / Internal tier — the layer on top of
  `docs/reference/public-api.md`'s binding-canonical-workflow rule (that
  page decides what stays public at all; this one says how much churn to
  expect once it's public). No module is Experimental yet; the tier is
  reserved for future new surface.
- Every classified module gained a matching `Stability: <Tier>` line in
  its rustdoc header, so the tier is visible from `cargo doc` without
  cross-referencing the table.
- CI rail `scripts/check/rust/api-stability-coverage.sh` keeps the table
  in sync with the `cargo public-api` baselines bidirectionally: every
  top-level public module (and, under `tst_core::klv` /
  `tst_core::codec`, every second-level dialect/parser module) must have
  a stability row, and every row must still name a real module.

### MuxSender/Sender: explicit input-consumption reporting

#### Added

- `MuxSenderError` and `SenderError` gain a new `input_consumed:
  Option<bool>` field, reported on every error from the `send_*` family
  (`send_video`, `send_klv`, `send_audio_to`, etc.) and `Sender::send_ts`:
  - `Some(false)` — this call's input was **not** consumed (a mux/framing
    rejection, a closed transport, or a failure draining bytes retained
    by a *previous* call). Retrying the same input after fixing the
    cause cannot duplicate data.
  - `Some(true)` — this call's input **was** consumed: muxed/framed and
    retained in the pending queue, which drains exactly once on the next
    `send_*`/`send_ts` call. Do not push the same input again.
  - `None` — the error did not originate from a per-call input path
    (e.g. `Sender::flush`, or a poisoned internal lock).
  - `RawSender` gains no such field — it performs no pending retention,
    so a transport error always means the current input was not
    delivered and may be retried without ambiguity.

  This supersedes the previous retry contract on `MuxSender`
  (discriminate-by-the-previous-call's-outcome — callers had to remember
  whether the prior `send_*` also failed to tell consumed from
  not-consumed) and on `Sender` (which previously claimed every error
  meant the input was consumed, without accounting for drain-failure
  calls, where the failure happens before the new call's bytes are
  touched). Existing `match` arms on `MuxSenderError` / `SenderError`
  keep compiling unchanged — the field is additive.

### Native-boundary sanitizer coverage: `TST_NATIVE_SANITIZER` + nightly `asan-native` CI

#### Added

- The `srt-sys` and `rist-sys` build scripts accept
  `TST_NATIVE_SANITIZER=address|thread`, threading the matching
  `-fsanitize` instrumentation through every vendored native object they
  build — libsrt, librist, and both static mbedTLS copies (which must
  stay flag-identical for their first-definition-wins link collapse to
  be sound, hence one shared variable rather than per-crate ones).
  Requires `CC=clang CXX=clang++` and the vendored build path; unknown
  values or a system-library pkg-config resolution fail the build
  instead of silently producing uninstrumented libraries. With the
  variable unset, builds are byte-identical to before.
- CI: new nightly `asan-native` sanitizers job runs `tst-srt`,
  `tst-rist`, and `tst-c` (all six transport features) under
  AddressSanitizer with the instrumented native libraries — the first
  sanitizer coverage of the vendored C/C++ boundary.

### Vendored dependency updates: libsrt 1.5.6 (security), librist 0.2.18, mbedTLS 3.6.7

#### Security

- **libsrt bumped v1.5.5 → v1.5.6** — a security patch release fixing a
  KMREQ CVE (unchecked buffer size, Haivision/srt#3317 + #3345), a KMRSP
  wire-length stack overflow (#3319), out-of-bounds reads in LOSSREPORT
  (#3324) and DROPREQ (#3320) parsing, and a receive-buffer drop-range
  bug (#3322) — all reachable from hostile wire input. Every build of
  `tst-srt`/`tst-c`/the Python wheels/the JVM JAR vendors this statically,
  so consumers pick the fixes up by rebuilding/upgrading.

#### Changed

- **librist bumped v0.2.16 → v0.2.18.** Adds upstream Advanced Profile
  support (not yet exposed through `tst-rist` — the `RistProfile` surface
  is unchanged). librist now requires LZ4 for Advanced Profile payload
  compression; the vendored build compiles librist's bundled
  `contrib/lz4` (`-Dbuiltin_lz4=true`), so no system LZ4 dependency is
  introduced on any platform. The mbedTLS-3.x header workaround
  (staging `entropy_poll.h`) is gone — upstream removed the vestigial
  include this release. 0.2.18 also fixes the librist-on-Windows
  teardown hang (`rist_destroy` blocked ~14 s) and zero data-plane
  delivery that had kept the RIST runtime tests gated off Windows
  since 2026-05-29 — those tests now run (and gate) on windows-msvc CI.
- **mbedTLS bumped v3.6.6 → v3.6.7** (LTS patch release; both the libsrt
  and librist static copies move together, preserving the byte-identical
  dual-copy link invariant).
- **Embedded: FreeRTOS kernel V11.1.0 → V11.3.0**, FreeRTOS-Plus-POSIX
  advanced one upstream commit (semaphore value restored on timed-wait
  failure — relevant to the libsrt pthread shims). lwIP stays at 2.2.1
  (latest stable). The freertos-srt cross-build stamp covers submodule
  HEADs, so the QEMU gates rebuild automatically.

#### Fixed

- **`tst-srt` now registers `srt_cleanup()` via `atexit` at first libsrt
  use.** libsrt 1.5.6 changed its receive-queue worker to keep running
  through UDP-channel teardown errors (Haivision/srt#3327), so exiting a
  process without `srt_cleanup()` — previously our documented behavior —
  became a deterministic post-`main` segfault in any process that had
  opened an SRT socket. The `atexit` handler is registered right after
  `srt_startup()`, so it runs before libsrt's static destructors and
  stops the queue workers cleanly. Cleanup remains decoupled from value
  drops (unchanged design).
- `srt-sys`/`rist-sys` build scripts now watch the vendored submodules'
  version-bearing files (`cargo:rerun-if-changed`), so a submodule bump
  triggers a rebuild of the vendored static libraries on incremental
  local builds instead of silently reusing the previous version's
  artifacts. (CI was never affected — fresh checkouts always rebuilt.)

### Security policy + rustdoc staleness sweep

#### Added

- `SECURITY.md` — vulnerability disclosure policy: private reporting
  channels (GitHub private vulnerability reporting, email fallback),
  pre-1.0 supported-versions statement, and response expectations.
  Linked from the README and the docs landing page.

#### Documentation

- Rustdoc staleness sweep: references to `RtpTransport` and the RIST
  receiver as "future"/"forthcoming" surfaces corrected to describe the
  shipped implementations (`tst-core` transport traits, `tst-pipeline`
  crate docs, `tst-rist` internals), including the stale claim that
  `TransportError::ExplicitClose` is produced only by
  `ManagedRecvTransport` (the RTP transports produce it on cancel) and
  the wrong variant attribution for `RtpTransport`'s OS-`errno` codes
  (they ride `Broken`, not `Backpressure`). Two module docs no longer
  point at internal design documents that are not part of this
  repository.

### RTSP client keepalive overhaul: session-killing response overflow + frozen cadence (PR #122)

No public API changes — all fixes are internal to `tst-rtp`'s RTSP
client, and the Python/JVM/C bindings inherit them as built.

#### Fixed

- **Every TCP-interleaved receive session died at 16.5 minutes with what
  looked like a clean end of stream.** The keepalive thread never reads
  responses, and the interleaved pump routed every RTSP response — the
  keepalive's own 200 OKs included — into its bounded 32-deep control
  queue, which nothing drains between main-thread requests. On a
  receive-only session (SETUP/PLAY, then only data) the 33rd keepalive
  response overflowed the queue at the default 30 s ping cadence and the
  pump's control-flood policy failed the session, surfacing to receivers
  as a clean EOS. The pump now consumes keepalive responses (classified
  by the `CSeq ≥ 1_000_000` keepalive band) instead of queuing them.
  Affects every prior release; diagnosed from a field report of an
  RTSPS session dropping mid-flight at exactly this mark.
- **The keepalive cadence was frozen before SETUP.** The pinger spawns
  at connect time with its interval derived from the 60 s RFC 7826
  default; a server-advertised `Session: <id>;timeout=N` parsed at
  SETUP updated the client but never the running thread, so a server
  advertising `timeout < 60` expired the session between 30 s pings.
  The thread now re-reads a shared interval cell at every wake and
  SETUP retunes it in place; an explicit
  `RtspClientBuilder::keepalive_interval` override stays pinned.
- **Keepalive pings never carried a `Session:` header.** The session-id
  cell shared with the pinger was never written after SETUP, so pings
  were not bound to the session — and an un-bound OPTIONS refreshes
  nothing on a conforming server (RFC 7826 §10.5 defines keep-alive in
  terms of a request carrying the session identifier). SETUP now binds
  the running pinger to the negotiated session id.
- **A buffered keepalive response could be misattributed as the next
  request's response** in non-pump mode (UDP transport / pre-SETUP),
  where the read path returned the first complete message off the
  socket without CSeq classification — e.g. `options()` could return a
  keepalive 200 with no `Public:` header. The read path now consumes
  keepalive-band responses and keeps reading for the caller's actual
  response.
- **A mid-session 401 answering a keepalive ping now refreshes the
  shared challenge cache** (nonce rotation / RFC 7616 `stale=true`), so
  the next ping — at most one interval later — signs against the fresh
  challenge instead of every subsequent ping silently failing until the
  server timeout; a `454 Session Not Found` now flips the session-dead
  flag surfaced by `RtspClient::is_session_alive`.
- **Sub-200 ms keepalive intervals are honored** (previously any
  requested cadence below the thread's 200 ms cancel-poll floor was
  silently quantized up to it), and keepalive writes participate in the
  interleaved pump's write-gate hand-off — now a waiting-writers
  counter rather than a single flag — so a hot data stream cannot
  starve ping writes on the shared stream mutex.

### MISB full-tag-compliance arc: ST 0601 exhaustive coverage, `klv::st0806`, `klv::st0805` (PRs #116–#120)

MISB ST 0601 is now typed end to end: 142 of the 143 UAS Datalink LS spec
items model onto `UasDatalinkLs` as typed fields, up from 52 before this
arc; only Tag 66 (deprecated) remains permanently unknown-passthrough by
design. The arc also adds two new sibling KLV modules that round out the
surface: `klv::st0806` (Remote Video Terminal Local Set) and `klv::st0805`
(KLV → Cursor-on-Target conversion). Landed in five work packages across
PRs #116–#120; grouped below by topic rather than by landing PR.

#### Added — ST 0601 full-tag coverage (142 of 143 items typed, up from 52; `klv::st0601`)

The item table typed in five stages:

- **51 fixed-encoding items** (52→103 of 143): 30 fixed-linear-mapped
  scalars closing the tags 35–93 gap (target location, track-gate
  width/height, CE90/LE90 error estimates, weather/atmospheric fields,
  alternate-platform position/heading/height, sensor north/east velocity,
  and the full-range twins of platform angle-of-attack/sideslip — tags
  92/93); 11 raw scalar/string items (I8/U16/U64 raw encodings plus 6
  UTF-8 strings, including the first two typed tags whose own tag number
  is itself 2-byte BER-OID encoded — 129 Target ID, 135 Communications
  Method); 3 coded enums (`IcingDetected`, `SensorFovName`,
  `OperationalMode`, each keeping an `Other(code)` fallback that
  round-trips unrecognized wire codepoints byte-exact); and 7 named
  nested-local-set byte fields (see the *Changed* entry below). Also
  closes the `OutOfRangePolicy::Indicator` known gap from the v0.3.0
  field-feedback arc (PR #92): all 11 sentinel-eligible tags (6, 7, 50,
  51, 52, 79, 80, 90–93) are now encodable typed fields, not just 5.
- **25 extended-encoding IMAPB items** (103→128 of 143): 14 ST 1201.5
  IMAPB (variable-length float) items — 4 extended-range twins of
  existing restricted items (target width, density altitude, sensor and
  alternate-platform ellipsoid height — tags 96, 103–105 twinning items
  22, 38, 75, 76) and 10 new standalone items (range to recovery,
  platform course angle, altitude AGL, radar altimeter, sensor
  azimuth/elevation/roll rate, MI storage percent full, transmission
  frequency, zoom percentage — tags 109, 112–114, 117–120, 132, 134); 10
  MISB variable-length truncatable-int items (navsats in view,
  positioning method source, two new coded enums `PlatformStatus` and
  `SensorControlMode`, time airborne, propulsion unit speed, take-off
  time, on-board MI storage capacity, leap seconds, GPS/UTC correction
  offset — tags 110–111, 123–126, 131, 133, 136–137), plus tag 139
  (`active_payloads`, a raw multi-byte payload-active bitmask). A new
  `imapb_specials: Vec<(u32, ImapbSpecial)>` field (re-exported as
  `klv::ImapbSpecial`) carries out-of-band ST 1201.5 special values
  (`+/-Infinity`, NaN families, `BelowMin`/`AboveMax`) for the 14 new
  IMAPB items, mirroring the existing `sentinel_tags` mechanism: a
  populated typed field always wins over a same-tag `imapb_specials`
  entry on encode. `OutOfRangePolicy::Indicator` now covers these 14
  items too.
- **Pack/list substrate + 6 simple DLP items** (128→134 of 143): new
  `Encoding::Pack` dispatch in `tags.rs` plus a `klv::st0601::packs`
  module for items whose wire value is a small positional structure or
  flat list rather than one scalar. Tag 81 `image_horizon`
  (`ImageHorizonPixels`), Tag 115 `control_commands`
  (`Vec<ControlCommand>`, MULTI-INSTANCE per ST 0601.19 Table 1), Tag 116
  `control_command_verification` and Tag 121 `active_wavelengths`
  (BER-OID id lists), Tag 127 `sensor_frame_rate` (`SensorFrameRate`),
  Tag 143 `metadata_substream_id` (`MetadataSubstreamId`).
  `strict_body_walk`'s once-per-packet duplicate-tag check now exempts
  tags 115 and 102 — the spec's only two "Multiples Allowed" items. New
  general-purpose `klv::st1010` module (MISB ST 1010 SDCC-FLP
  parser/encoder, Mode 1 + Mode 2, sparse bit-vector support), used by
  Tag 102 below.
- **7 VLP series items** (134→141 of 143): Tag 122 `country_codes`
  (`CountryCodes`: coding method + up to three length-value UTF-8
  country codes, with per-§8.122.1 truncation — a trailing length-value
  pair drops entirely — distinct from the length-0 "unknown" marker,
  which still writes the length byte); Tag 128 `wavelengths_list`
  (`Vec<WavelengthRecord>`: BER-OID id + two IMAPB(0,1e9,4) bounds +
  UTF-8 name); Tag 130 `airbase_locations` (`AirbaseLocations`, sharing
  the new `Location` DLP with Tag 141: take-off + recovery WGS84 sites;
  a wire-absent recovery pair decodes to "same as take-off" per
  §8.130.1, distinguishable from an explicit length-0 "unknown"
  recovery); Tag 138 `payload_list` (`PayloadList`: Payload Count
  (BER-OID) plus a VLP of Payload Records — id, `PayloadType` enum,
  UTF-8 name); Tag 140 `weapons_stores` (`Vec<WeaponsStore>`: weapon
  physical address + packed 14-bit status, with Table 22 engagement-bit
  accessors `fuze_enabled`/`laser_enabled`/`target_enabled`/
  `weapon_armed`, + weapon type string — the whole-item spec vector has
  a known 1-byte length discrepancy, so the test pins the three
  per-record vectors and reconstructs the outer VLP directly); Tag 141
  `waypoint_list` (`Vec<Waypoint>`: waypoint id, signed 16-bit
  prosecution order, optional Mode/Source info bitfield, optional
  `Location`); Tag 142 `view_domain` (`ViewDomain`: up to three
  azimuth/elevation/roll `(start, range)` IMAPB pairs, each
  independently truncatable or explicitly marked "unknown"). All seven
  pinned against MISB ST 0601.19 §8 worked examples transcribed from the
  cached spec PDF (including the §8.138 63-byte Payload List example).
- **Tag 102 SDCC-FLP positional capture** (141→142 of 143, the arc's
  final item): Tag 102 (`sdcc_flps: Vec<SdccFlpField>`) is MULTI-INSTANCE
  with positional row-to-item semantics (the "Refined Source List"
  binding, ST 0601.19 §8.102) — each occurrence refines the accuracy of
  the `N` Local Set items immediately preceding it on the wire, where `N`
  is the occurrence's own SDCC-FLP matrix size. Decode maintains a
  running wire-order tag list and captures, per occurrence,
  `preceding_tags` (the refined items) plus the raw pack `bytes` (via
  `klv::st1010::decode_sdcc_flp`); encode re-emits every entry verbatim,
  grouped together in tag-ascending position — the original interleaving
  with `preceding_tags` is documented as NOT reproduced on re-encode
  (`SdccFlpField`'s adjacency caveat). **This is the arc's final item:
  only Tag 66 (deprecated, permanently unknown-passthrough by design)
  remains untyped.**
- All 142 items are mirrored field-for-field through the Python and JVM
  bindings, including every new enum. The WP-C pack/list dataclasses/
  records and the standalone `klv::st1010` mirror are large enough to
  warrant their own entries below.

#### Changed — nested local-set tags decode into named fields, not `unknown`

- Tags 73, 95, 97, 98, 99, 100, 101 (RVT / SAR Motion Imagery / Range
  Image / Geo-Registration / Composite Imaging / Segment / Amend local
  sets) now decode into their own named `Option<Vec<u8>>` field (`rvt`,
  `sar_mi_local_set`, `range_image_local_set`,
  `geo_registration_local_set`, `composite_imaging_local_set`,
  `segment_local_set`, `amend_local_set`) instead of the generic
  `unknown` bucket. The bytes are unchanged and wire output is
  byte-identical — interior typing of each nested set's contents is
  still future work; this only changes where a caller finds the bytes
  on the decoded struct.

#### Added — `st0601_sentinel_meaning` lookup in Python and JVM

- `tstrans.klv.st0601_sentinel_meaning(tag)` (Python) and
  `org.tstrans.klv.Klv.st0601SentinelMeaning(tag)` (JVM) expose the Rust
  `klv::st0601::st0601_sentinel_meaning` lookup (Out of Range / Reserved
  / Not Available per tag) to binding callers.

#### Added — ST 0601 restricted-vs-extended item precedence documented

- Four pairs of tags carry the same real-world quantity at two
  precisions: Item 22 / Item 96 (target width), Item 38 / Item 103
  (density altitude), Item 75 / Item 104 (sensor ellipsoid height), and
  Item 76 / Item 105 (alternate-platform ellipsoid height). Per ST
  0601.19 requirements 0601.9-20/-21, a decoder that understands the
  extended (IMAPB) item should prefer it over its restricted twin when
  both are present on the wire. `UasDatalinkLs` decodes both fields
  independently (it does not drop either), so this is now documented on
  each field's rustdoc and in the KLV guide (`docs/guides/klv.md`) as
  caller guidance, not enforced by the decoder.

#### Fixed — Python/JVM `unknown` predicate was stale against the newly-typed tags

- The Python and JVM bindings' internal `is_st0601_typed_tag` predicate
  gates the *encode* direction only: it drops a caller-supplied
  `unknown` entry before it reaches tst-core's encoder whenever the
  predicate considers the entry's tag typed (typed field wins). The
  predicate was stale relative to the 51 newly-typed tags above: Python
  was missing tags 3 and 4 (Mission ID, Platform Tail Number), and JVM
  was additionally missing tag 94 (MIIS Core Identifier). A
  caller-supplied `unknown` entry for one of those tags was *not*
  dropped by the filter, so it reached tst-core's encoder and was
  rejected with `KlvEncodeError::ReservedTagInUnknown` instead of being
  silently discarded per the documented "typed wins" collision policy.
  Fixed as part of widening the predicate for the 51 newly-typed tags
  above.
- **Behavior change:** the old predicate's over-broad `5..=91` span
  also covered tags 66 and 81, which remain untyped. A caller-supplied
  `unknown` entry at tag 66 or 81 was therefore silently dropped by the
  old filter before ever reaching the encoder. The corrected, exact
  predicate no longer covers 66/81, so `unknown` entries at those tags
  now encode onto the wire like any other forward-compat tag, instead
  of silently vanishing.
- `is_st0601_typed_tag` was widened again as each later stage above
  landed (WP-B's 25 extended items, WP-C's pack/list items including
  Tag 102), keeping the predicate's coverage exact rather than a range
  approximation. A caller-supplied `unknown` entry at any typed tag is
  silently dropped (typed wins) rather than surviving into the encoded
  record.

#### Fixed — JVM `precedingTags`/`controlCommandVerification`/`activeWavelengths` local-ref leak

- `read_sdcc_flp_field`'s `precedingTags` loop (and the same-shaped
  `controlCommandVerification`/`activeWavelengths` loops) called the list
  accessor once per iteration with no per-item local frame, while itself
  running inside the fixed 16-slot `with_local_frame` that wraps each
  `sdccFlps` list item — a caller-supplied list longer than that frame's
  spare capacity exhausted the JNI local-ref table and **aborted the JVM
  outright** (not a catchable exception). Fixed by routing all three call
  sites through a new shared `jutil::read_long_list` helper (mirrors the
  existing `build_long_list`), which applies its own per-item frame. Two
  new 64-entry regression tests
  (`sdccFlpFieldLongPrecedingTagsRoundTripsWithoutAborting`,
  `controlCommandVerificationLongListRoundTripsWithoutAborting`) exercise
  past the old ~13-entry abort threshold.

#### Fixed — `klv::st1010::decode_sdcc_flp` hostile Matrix Size abort

- A crafted Element 1 Matrix Size (the BER-OID `N`, attacker-controlled up
  to `u32::MAX`) could make `decode_sdcc_flp` size its correlation vectors
  by `N(N-1)/2` *before* reading any correlation byte — a 7-byte input
  claiming `N` &asymp; `u32::MAX` demanded a ~9.2 EiB allocation and
  **aborted the process** rather than returning an `Err`. `N` is now
  bounded against the wire bytes actually remaining (std-dev/correlation
  byte requirements, or the Bit Vector's bit capacity in sparse mode)
  before any allocation is sized by it. Reachable from all three typed
  entry points (Rust `decode_sdcc_flp`, Python, JVM `Klv.decodeSdccFlp`)
  via the ST 0601 Tag 102 workflow above. New fuzz target
  `klv_st1010_decode`.

#### Added — Python bindings: WP-C pack & list items + `klv::st1010` SDCC-FLP

- `tstrans.klv` gains frozen dataclasses mirroring every WP-C
  `UasDatalinkLs` pack/list field, following the existing `VTargetPack`
  nested-struct-list pattern: `ImageHorizonPixels`, `ControlCommand`,
  `SensorFrameRate`, `MetadataSubstreamId`, `CountryCodes`,
  `WavelengthRecord`, `Location`, `AirbaseLocations`, `PayloadType`
  (codepoint enum) plus `PayloadRecord`/`PayloadList`, `WeaponsStore`
  (with `general_status`/`fuze_enabled`/`laser_enabled`/
  `target_enabled`/`weapon_armed` properties), `Waypoint`,
  `ViewDomainPair`/`ViewDomain`, and `SdccFlpField` — all wired through
  `decode_uas_datalink`/`encode_uas_datalink`.
- New standalone `tstrans.klv.SdccFlp` dataclass plus `decode_sdcc_flp`/
  `encode_sdcc_flp_mode2` entry points mirroring the general-purpose
  `klv::st1010` module (usable independent of ST 0601 — see the module
  docstring).
- `is_st0601_typed_tag` (the internal predicate gating the Python
  binding's `unknown` collision-drop on encode) now covers every WP-C
  tag, including Tag 102 — whose predicate status was deferred pending
  its multi-instance modeling, which landed in Task C4. A
  caller-supplied `unknown` entry at any WP-C tag is now silently
  dropped (typed wins) rather than surviving into the encoded record.

#### Added — JVM bindings: WP-C pack & list items + `klv::st1010` SDCC-FLP

- `org.tstrans.klv` gains a Java record (or enum) mirroring every WP-C
  `UasDatalinkLs` pack/list field: `ImageHorizonPixels`, `ControlCommand`,
  `SensorFrameRate`, `MetadataSubstreamId`, `CountryCodes`,
  `WavelengthRecord`, `Location`, `AirbaseLocations`, `PayloadType`
  (codepoint enum, wire codes crossing as `long` — unlike `IcingDetected`'s
  narrow byte, `PayloadType::Other` carries the full BER-OID `u64` range)
  plus `PayloadRecord`/`PayloadList`, `WeaponsStore` (with
  `generalStatus`/`fuzeEnabled`/`laserEnabled`/`targetEnabled`/
  `weaponArmed` accessors), `Waypoint`, `ViewDomainPair`/`ViewDomain`, and
  `SdccFlpField` — all wired through `Klv.decodeUasDatalink`/
  `Klv.encodeUasDatalink`. Unlike `VTargetPack`'s many-optional-field
  Builder, most of these small (&le;8 field) types use a plain canonical
  constructor, matching the `CoreId`/`GeoPoint` precedent; `ImageHorizonPixels`
  and `WeaponsStore` are the two exceptions, each also gaining a `Builder`
  (named setters over 4 consecutive same-typed positional fields — a
  transposition hazard the review round called out for both: image-horizon
  percentages/lat-lon pairs and weapon station/hardpoint/carriage/store
  BER-OID ids). List fields (`controlCommands`, `wavelengthsList`,
  `weaponsStores`, `waypointList`, `sdccFlps`, and `payloadList`'s nested
  `records`) still follow the `VTargetPack` `with_local_frame`-per-item
  idiom on the JNI side.
- New standalone `org.tstrans.klv.SdccFlp` record plus
  `Klv.decodeSdccFlp`/`Klv.encodeSdccFlpMode2` entry points mirroring the
  general-purpose `klv::st1010` module; `SdccFlp.correlation(i, j)` is a
  pure-Java port of the Rust accessor (no JNI crossing).
- `is_st0601_typed_tag` (the internal predicate gating the JVM binding's
  `unknown` collision-drop on encode) now covers every WP-C tag, including
  Tag 102 — same decision as the Python binding. A caller-supplied
  `unknown` entry at any WP-C tag is now silently dropped (typed wins)
  rather than surviving into the encoded record.
- `read_uas_datalink` (the JNI encode-direction field reader) now calls
  `env.ensure_local_capacity(320)` — it had NO such call before this WP
  (pre-existing debt: every `read_nullable_*` accessor mints a local ref
  that lives until the function returns). `build_uas_datalink`'s own
  capacity bumped 224 &rarr; 320 for the same 14 new fields (a first-cut
  256 left effectively zero margin — bumped again after review to leave
  real headroom, verified empirically by
  `St0601PacksTest.fullyPopulatedWpcFieldsRoundTrip`).

#### Added — MISB ST 0806.4 Remote Video Terminal (RVT) Local Set typed layer (`klv::st0806`)

- New sibling-layer module `klv::st0806` types the RVT Local Set carried
  by ST 0601 Tag 73: both the nested body form (`decode`/`encode_to_vec`,
  no UL, embedded via `UasDatalinkLs::rvt`) and the standalone
  independent form (`decode_standalone`/`encode_to_vec_standalone`, own
  16-byte UL, timestamp-first Tag 2, checksum-last Tag 1 per
  ST 0806.4-02/-04). `UasDatalinkLs::rvt`'s rustdoc now points to the
  typed layer, mirroring the `vmti` field's pointer to `klv::st0903`.
- Two repeatable nested Local Sets — Point of Interest (`RvtPoi`, Tag
  12) and Area of Interest (`RvtAoi`, Tag 13) — and one repeatable User
  Defined LS (`RvtUserData`, Tag 11) round out the schema (ST 0806.4
  Tables 8-2/8-3/8-4); POI/AOI lat-lon/altitude use the same
  symmetric-mapped range + `0x80000000` "error"-sentinel machinery as
  ST 0601. POI Type and AOI Type share a wire key but diverge at value
  3 ("Target" vs. "Reserved"), so each gets its own `RvtPoiType`/
  `RvtAoiType` enum (plus `RvtUserDataType`).
- The standalone form's checksum (Tag 1) is CRC-32/MPEG-2 (ISO/IEC
  13818-1: poly `0x04C11DB7`, init `0xFFFFFFFF`, no reflection, no
  final XOR) — a new `klv::crc32` substrate, and a real divergence from
  the ST 0601 16-bit running-sum. A mismatch raises the new
  `KlvDecodeError::Crc32Mismatch { expected, found }` variant. An
  embedded RVT LS is not required to carry Tag 1 or Tag 2.
- Encoding a typed tag through the `unknown` pass-through bucket (RVT
  LS top level, or nested in `RvtPoi`/`RvtAoi`) is rejected with the
  existing `KlvEncodeError::ReservedTagInUnknown`, mirroring
  `klv::st0601::encode`'s guard.
- New fuzz target `klv_st0806_decode` (29 fuzz targets total across the
  workspace).

#### Added — Python bindings: `klv::st0806` RVT Local Set mirror

- `tstrans.klv` gains `RvtLs`/`RvtPoi`/`RvtAoi`/`RvtUserData` frozen
  dataclasses (plus `RvtPoiType`/`RvtAoiType`/`RvtUserDataType` enums)
  and `decode_rvt`/`decode_rvt_standalone`/`encode_rvt`/
  `encode_rvt_standalone` entry points. A CRC-32 mismatch on the
  standalone form raises `KlvError` with the existing
  `CHECKSUM_MISMATCH` kind (reused, not a new kind) and the declared/
  computed values in hex.

#### Added — JVM bindings: `klv::st0806` RVT Local Set mirror

- `org.tstrans.klv` gains `RvtLs`/`RvtPoi`/`RvtAoi`/`RvtUserData`
  records (plus `RvtPoiType`/`RvtAoiType`/`RvtUserDataType` enums) and
  `Klv.decodeRvt`/`decodeRvtStandalone`/`encodeRvt`/
  `encodeRvtStandalone`. A CRC-32 mismatch on the standalone form
  throws `KlvDecodeException` with the existing `CHECKSUM_MISMATCH`
  kind (reused, not a new kind).

#### Added — MISB ST 0805.1 KLV → Cursor-on-Target conversion (`klv::st0805`)

- New module `klv::st0805` converts a decoded ST 0601 UAS Datalink LS
  record (`UasDatalinkLs`) to Cursor-on-Target (CoT) XML: a **Platform
  Position** event (`platform_position_xml`, configurable `type`,
  default `a-f-A-M-F`) and a **Sensor Point of Interest** event
  (`sensor_point_of_interest_xml`, fixed `type="b-m-p-s-p-i"`), linked
  back to the platform event via `detail/link`. Both events use
  `how="m-p"` and `event/@version="2.0"` per ST 0805.1 §5.
- `uid` is a deterministic concatenation of KLV tags, never a UUID (a
  replayed file must reproduce byte-identical CoT): `platform_uid` =
  `"{tag10}_{tag3}"`, `spi_uid` = `"{tag10}_{tag3}_{tag11}"`. Platform
  point uses tags 13/14 + HAE, with the platform `hae` source order
  following ST 0601.19's extended-representation precedence — Tag 104
  (Sensor Ellipsoid Height Extended) first, then Tag 75 (Sensor Ellipsoid
  Height), then MSL Tag 15 + configurable `geoid_undulation_m`. SPI point
  prefers tags 40/41 (falling back to the frame-center tags 23/24), with
  `hae` paired to whichever position source was chosen: Tag 42 (MSL, no
  HAE-native twin exists for target location) + `geoid_undulation_m` with
  40/41; Tag 78 (HAE-native) then Tag 25 (MSL) + `geoid_undulation_m` with
  23/24 — only the frame-center branch has an HAE-native preference.
  `point/@ce`/`@le` are the fixed
  sentinel `9999999` for the Platform event, and for SPI are `tag45 /
  2.146` / `tag46 / 1.645` (falling back to the same sentinel when the
  source tag is absent). The Platform event's `sensor` sub-element maps
  `azimuth = (tag5 + tag18) mod 360`, `fov`=tag16, `vfov`=tag17,
  `model`=tag11, `range`=tag21. `<_flow-tags_>` generation time is a
  caller-supplied argument (`generated_us`), not wall-clock, keeping the
  module `no_std` and its output deterministic. New `CotConfig`
  (`platform_type`, `update_interval_us`, `producer`,
  `geoid_undulation_m`, `how`) and `CotError` (joins `KlvDecodeError`/
  `KlvEncodeError` in `error.rs`) types. ST 0805.1 defines no CoT→KLV
  reverse mapping — out of scope (see `docs/project/deferred-features.md`).
- Goldens validated against MITRE's CoT Base-Event Schema (PUBLIC
  RELEASE, Case #11-3895) and CoT Flow-Tags Schema (PUBLIC RELEASE,
  Case #11-3899), fetched from the DoD/DISA ATAK-CIV open-source mirror
  since `cot.mitre.org` does not resolve from the build environment.
  Validated with `lxml.etree.XMLSchema`; both event goldens and the
  isolated `_flow-tags_` fragment pass. XSDs not vendored into the
  repo.
- This is a pure conversion over an already-decoded record, not a KLV
  byte parser, so it adds no fuzz target.

#### Added — Python bindings: `klv::st0805` KLV → Cursor-on-Target conversion

- `tstrans.klv` gains `CotConfig`, `platform_position_xml`,
  `sensor_point_of_interest_xml`, `platform_uid`, and `spi_uid`,
  mirroring the Rust surface 1:1 (`config=` keyword-only, defaults
  matching `CotConfig::default()`).

#### Added — JVM bindings: `klv::st0805` KLV → Cursor-on-Target conversion

- `org.tstrans.klv.Klv` gains `platformPositionXml` /
  `sensorPointOfInterestXml` (each with a `CotConfig`-defaulted
  2-argument overload and an explicit-`CotConfig` 3-argument overload)
  and `platformUid` / `spiUid`; new `org.tstrans.klv.CotConfig` value
  type with a `Builder`.

#### Added — Fuzz coverage: `klv_st1204_decode` + `klv_st0605_decode` (31 targets total)

- Two new fuzz targets close the arc's fuzz-coverage riders: MIIS Core
  Identifier binary decode (`klv_st1204_decode`) and Precision Time
  Stamp Pack decode (`klv_st0605_decode`) — workspace total 29→31.

#### Fixed — binding-doc falsehoods and staleness left across the arc

- The ST 0601 encode-range table on the Python `.pyi` stub and the JVM
  Javadoc surface covered 39 of the 83 ranged fields; it now covers all
  83 (the 44 new rows: 30 linear-mapped + 14 IMAPB, plus twin-tag
  annotations), matching Rust rustdoc.
- `Klv.java` carried six false `@throws` claims that encoding an
  `unknown` entry at a reserved/typed tag raises an exception from
  those methods — unreachable, since both bindings' encode paths
  silently filter typed tags out of `unknown` before the Rust encoder
  ever sees them; the same false claim in `exceptions.py` is fixed too.
- The `klv::st0806`/JVM/Python "an embedded RVT LS never carries
  Tag 1/2" absolutes are corrected to ST 0806.4's actual "not required
  to carry" scope (the wording already used in the `klv::st0806` entry
  above) across Rust, Python, and JVM.
- A drifted `model.rs:80-86` file:line citation in the `klv::st0805`
  rustdoc is replaced with an intra-doc link.
- A JVM silent-null-on-null-input behavior shared by 5 JNI natives is
  now documented rather than left unstated.
- Stale typed-tag/pack/list enumerations that predated the
  st0605/st0805/st0806/st1010/st1204 additions are corrected across
  module docs.

#### Added — cookbook: long-tail KLV decode + KLV→CoT recipes

- `docs/cookbook/klv/decode-long-tail.md` ("Reading waypoint lists,
  weapons stores, and SDCC covariance") and
  `docs/cookbook/klv/klv-to-cot.md` ("Converting ST 0601 to
  Cursor-on-Target") join the cookbook index.

#### Added — deferred-features entries: SDCC-FLP encode adjacency, ST 0903 VTrack

- `docs/project/deferred-features.md` gains two entries: SDCC-FLP
  wire-adjacency preservation on encode (the ascending-order grouping
  caveat lives in ST 0601's Tag 102 field writer, not in
  `st1010::encode_sdcc_flp_mode2` itself, which has no adjacency
  concept — see the Tag 102 entry above) and ST 0903 VTrack, which
  MISB ST 0903.6 formally withdrew per its own revision history and is
  recorded as withdrawn rather than implied as a pending coverage gap.

### Changed — RTSP server refuses TLS config on a plaintext bind

- `RtspServer::start()` now fails with `RtspServerError::Tls` when
  TLS cert/key paths are configured (`tls_cert()` in Rust; `tls_cert` / `tls_key` in Python)
  but the bind URL scheme is
  plaintext `rtsp://` (previously the TLS material was silently ignored and
  the server came up unencrypted). Python inherits the guard through
  `RtspServerConfig.tls_cert` / `tls_key`; the JVM keeps its earlier
  fail-fast at `RtspServerConfig.Builder.build()` on top of it.
- The C ABI's `tst_rtsp_server_builder_tls_cert_pem` no longer
  accepts-and-discards PEM material: `tst_rtsp_server_builder_start` now
  fails (`TST_E_RTSP_SERVER`) when TLS bytes were supplied, because tst-c is
  built without TLS support and the bytes could never take effect. No new
  symbols or error codes — the C ABI minor stays 19.

### Fixed — RTSP client authenticates OPTIONS

- `RtspClient::options()` now routes through the shared authenticated send
  path (pre-emptive signing + one reactive 401 retry) like every other
  method. Servers that challenge OPTIONS used to surface a raw
  `Protocol { code: 401 }` error from credentialed clients.

### Added — Python wheels are fully featured: TLS + RIST encryption compiled in

- The Python binding gains a default-on `tls` cargo feature (shipped in the
  published wheels) that lights up every rustls-backed transport variant:
  - **`rtsps://` client** — verifies against platform native trust roots, or
    a private CA / self-signed camera cert via
    `RtspClientConfig.tls_root_certs_pem` (previously accepted but unread).
  - **`rtsps://` server** — `RtspServerConfig` takes `tls_cert` / `tls_key`
    PEM *file paths* (breaking, pre-1.0: replaces the never-functional
    `tls_cert_pem` / `tls_key_pem` bytes fields), matching the path-based
    convention of the HLS and TCP surfaces; unreadable paths raise
    `RtspError(TLS)` at `start()`.
  - **`tcps://` caller + listener** — callers verify via native roots or the
    `?ca=<pem path>` URL param; listeners serve TLS through the new
    `ListenerBuilder.tls(cert, key)` setter. (`tcps://` used to raise
    `TcpError(TLS_DISABLED)` unconditionally.)
  - **HTTPS HLS serving** — `HlsPublisherBuilder.enable_tls(cert, key)` now
    works in wheels instead of raising `HlsError(TLS_DISABLED)`.
- **RIST PSK encryption works in wheels**: tst-py's `rist` feature now builds
  tst-rist with its `mbedtls` feature — every `EncryptionKey` used to raise
  `RistError(ENCRYPTION_DISABLED)` in published wheels.
- Python end-to-end TLS test coverage: `tcps://` loopback round-trip +
  untrusted-cert fail-closed, `rtsps://` server↔client session over a custom
  trust anchor, HTTPS playlist fetch, and a hardened RIST encryption probe
  (`ENCRYPTION_DISABLED` is now a test failure, not a skip).

### Added — JVM gains TLS parity in `tstrans-jvm`

- JVM: TLS ships in `tstrans-jvm` — `rtsps://` client (custom-CA trust
  anchors via `RtspClientConfig.tlsRootCertsPem`, functional for the first
  time) and `rtsps://` server. **Breaking:** `RtspServerConfig.tlsCertPem`/
  `tlsKeyPem` (never-functional `byte[]` fields) are replaced by
  path-based `tlsCert`/`tlsKey` (PEM file paths, both-or-neither), the
  same swap the Python binding makes in this release (PR #111). Bad cert/key
  paths throw kind `TLS` from `RtspServer.start`.

### Fixed

- RTSP client: credentials are now attached to **every** RTSP method, not just
  DESCRIBE. Servers that auth-gate each request individually (gortsplib /
  MediaMTX, and this crate's own `RtspServer`) rejected the unauthenticated
  SETUP that followed an authenticated DESCRIBE, so credentialed `rtsp://` /
  `rtsps://` sessions against them could never reach PLAY. The client now
  caches the `WWW-Authenticate` challenge from the first 401 and attaches
  `Authorization` pre-emptively on DESCRIBE / SETUP / PLAY / PAUSE / TEARDOWN
  (with one reactive retry on a fresh challenge), hashes the Digest HA2
  against the exact request-URI of each method (SETUP signs its control URI,
  matching gortsplib/MediaMTX validation), keeps a strictly increasing
  `qop=auth` nonce-count while reusing a cached nonce (RFC 7616 §3.4), and
  authenticates the keepalive thread's OPTIONS pings. Unauthenticated servers
  are unaffected — pre-emptive auth stays inert until a challenge is seen.
- `tst-rtp`: `RtspServer::start()` now surfaces listener startup failures as
  typed errors — a bad TLS cert/key path returns `RtspServerError::Tls` and a
  bind conflict returns `BindAddrInUse` from `start()` itself. Previously the
  listener task died after `start()` had already returned `Ok`, leaving a
  server that looked started but never accepted (log-only). TLS cert/key are
  now loaded synchronously inside `start()`. A listener that fails to report
  startup within 5 s returns `Io(TimedOut)` — `start()` never returns
  `Ok(())` over an unbound listener (the old spin-wait's timeout fallback).

---

## [0.3.0] — 2026-07-13

Post-v0.2.0: audit remediation, API consistency, RFC 6184 H.264 ingest, HLS
promotion, STANAG 4609 / MISP conformance, hardening.

### Release highlights

The integrator-facing changes at a glance (full detail in the sections below):

- **HLS is a supported feature** in its own `tst-hls` crate — path traversal
  (CWE-22) closed, VOD/EVENT playlists stay served after finish, segments open
  on a decodable PAT → PMT → IDR boundary (segment 0 excepted when the first
  GOP outruns `segment_duration` — deferred fix tracked), loopback bind by
  default; ships in the Python wheels and the C ABI (minor 18).
- **H.264-over-RTP ingest (RFC 6184)** — `H264Depacketizer` + `H264Receiver` +
  RTSP auto-setup across Rust / Python / JVM, with a hardened `max_au_bytes`
  DoS cap.
- **STANAG 4609 / MISP conformance** — ST 0604 MISP SEI timestamps
  (`push_video_misp_to`, C ABI minor 19), ST 1204 Core ID + ST 0601 Tag 94,
  ST 0902 `validate_mismms`, and the `docs/reference/stanag-4609.md`
  conformance matrix.
- **Integrator field-report fixes** — `DemuxerConfig::sync_buf_cap` (the 4 MiB
  whole-file `feed()` ceiling is configurable and the error names the knob),
  ST 0601 out-of-range errors hint their full-range twin tags, opt-in
  `OutOfRangePolicy::Indicator`, and `tcp://` / `tcps://` hostname TLS.
- **Receive-side contract** — `RecvTransport::max_payload` is now the
  deliverable ceiling (full-MTU traffic from foreign senders no longer
  truncates or drops); the inert receive-side `pkt_size` knobs are removed.
- **Consistency renames** (breaking, pre-1.0): `SrtError`, `MuxErrorKind`,
  `is_cancelled`, `DemuxerConfigBuilder`, Python/JVM `send_*` live-push family,
  `DemuxEvent.Metadata` (`Klv` alias kept until 1.0), byte-valued
  `recv_buf_bytes` / `send_buf_bytes`, `TcpUrl::host`.

Remediation of the two 2026-07-01 audits (correctness/spec/safety and
simplification/refactor/dependency), landed as PRs #58–#84, plus the embedded
audit arc (WP-EMB-1–6, PRs #85/#86/#88–#90), the field-feedback hardening
(PR #87 / #91–#93), the RFC 6184 H.264-over-RTP depayloader arc (PRs #94–#96),
the receive-ceiling contract normalization (PRs #97 / #101), the tst-hls
restructure + promotion (PRs #98–#100), the STANAG 4609 / MISP conformance arc
(PRs #102–#105), and the post-v0.2.0 review hardening (PR #106). The C ABI
stayed frozen at minor **17** through the audit work and has bumped twice —
additively — since: to **18** (HLS promotion) and **19** (MISP timestamps). No
C symbol, signature, or struct layout was removed or changed.

### Changed (breaking, pre-1.0) — SRT buffer-size options are now byte-valued

- `SocketBuilder`'s `recv_buf_packets` / `send_buf_packets` are renamed to
  `recv_buf_bytes` / `send_buf_bytes`. The values were always passed straight
  through to libsrt's `SRTO_RCVBUF` / `SRTO_SNDBUF`, which are byte-valued —
  the old names implied a packet count they never had. Values above
  `i32::MAX` are now rejected instead of silently wrapping (PR #61).

### Changed (breaking, pre-1.0) — Rust type and builder renames for cross-binding consistency

- `tst_srt::Error` → `SrtError` (PR #79).
- `tst_core`'s `MuxSenderErrorKind` → `MuxErrorKind` (PR #79). Note: the
  `tst_pipeline::MuxSenderError` family is intentionally **not** renamed.
- `tst_rtp`'s cancel-handle predicate `is_canceled` → `is_cancelled`, matching
  the spelling already used elsewhere (also exposed through the Python
  binding; the Java binding was already correct) (PR #79).
- `DemuxerBuilder` → `DemuxerConfigBuilder`; its `build()` is now infallible
  and returns a `DemuxerConfig` (previously a `Result`). A new
  `DemuxerConfig::builder()` entry point is added (PR #79).
- `TcpUrl::addr: IpAddr` → `TcpUrl::host: String`. The field now holds the
  host as written in the URL (either an IP literal or a DNS hostname), deferring
  resolution to connect time. Callers that previously matched on `addr` should
  call `host.parse::<IpAddr>()` if they need the old `IpAddr` type (DA-NET-9, PR #91).

### Changed (breaking, pre-1.0) — Python and JVM binding surface

- Python and JVM `MuxSender` — and the srt `ManagedMuxSender` — rename their
  live-push methods from `push_*` to `send_*` (`send_video`, `send_audio`,
  `send_klv`, `send_data`, …), matching the underlying Rust `send_*` shape.
  `Muxer`, the RTSP `MountHandle`, and `MuxerFileSink` deliberately keep
  `push_*` (buffer/file operations, at Rust parity) (PR #79).
- Python `DemuxEvent.Klv` → `DemuxEvent.Metadata`. `DemuxEvent.Klv` remains as
  a deprecated same-object alias and will be removed at 1.0; the pandas
  adapter now reports these events under the `"Metadata"` kind (PR #79).
- JVM `DemuxEvent.Video` is now raw-first: its elementary-stream units are
  parsed lazily via `parse()` (which throws `DemuxException`) rather than
  eagerly on decode, so there is no eager `payload()` / units accessor
  (PR #75).
- JVM `DemuxEvent.Audio` is likewise raw-first, with lazy `parse()` /
  `parse(boolean strict)` (throwing `CodecParseException` /
  `DemuxException`) (PR #78).

### Changed (breaking, pre-1.0) — ST 0601 range-error messages name their full-range twins

- `KlvEncodeError::OutOfRange` gains a `hint: Option<&'static str>` field. ST 0601
  narrow-range encode rejections for Tags 6/7 (Platform Pitch/Roll, ±20°/±50°) and
  Tags 26–33 (Offset Corner Lat/Lon, ±0.075°) now append a hint to the error message
  naming the full-range twin fields (Tags 90/91 and 82–89 respectively). Tags without
  a twin continue to produce hints of `None` and no `;` in the message. Code that
  constructs `OutOfRange` directly must add `hint: None` (an integrator field report
  2026-07-08).

### Removed (breaking, pre-1.0)

- `PairerConfig::link_klv_to_video` (the dead pairing knob) is removed across
  Rust, Python, and JVM. It was never wired to behavior; no deprecated alias
  is kept. The demuxer's unrelated `link_klv` API is untouched (PR #69).
- `KlvFieldError::InvalidSentinel` is removed. ST 0601 sentinel values are now
  modeled explicitly rather than rejected — see the Added entry below (PR #79).

### Changed (breaking, pre-1.0) — HLS moved to its own `tst-hls` crate

- The HLS publisher moved out of `tst-tcp` into a new segmenter-first
  `tst-hls` crate. `tst-tcp` no longer carries the `hls` feature — callers
  that built `tst-tcp` with `--features hls` now depend on `tst-hls`
  directly (Rust import path `tst_hls::*` instead of `tst_tcp::hls::*`).
- The HTTP server now **binds loopback (`127.0.0.1:8080`) by default**
  instead of all interfaces; binding `0.0.0.0` is an explicit opt-in.
- The default `output_dir` is now a portable temp dir
  (`<temp_dir>/tstrans-hls`) instead of a hard-coded `/tmp/hls`.
- **Keyframes now begin segments.** Each segment opens on a decodable
  boundary — PAT → PMT → IDR — so a joining player can decode the first
  segment it fetches (previously a segment could start mid-GOP). One bounded
  exception remains: if the *first* GOP exceeds `segment_duration`, segment 0
  can still be wall-clock-cut mid-GOP (self-heals at the second keyframe);
  the closing keyframe-intent signal is deferred — see
  `docs/project/deferred-features.md`.

### Added — HLS promoted to a supported feature

- The HLS publisher is now a **supported** feature (no longer experimental
  or excluded from published artifacts). The `hls` feature is default-on in
  the Python wheels (`tstrans.hls` imports out of the box) and opt-in in the
  C binding (`TST_HAS_HLS`). The JVM binding does not yet expose HLS.
- **Path traversal (CWE-22) is closed** — the built-in HTTP server serves
  only files from a known set it wrote itself; request paths are not used to
  open arbitrary files under the output directory.
- **VOD / EVENT serving is closed** — `HlsPublisher::finish_serving` returns
  an `HlsServerHandle` that keeps the built-in server up so a completed VOD
  or EVENT playlist and its segments stay fetchable after the stream ends
  (Rust + C `tst_hls_publisher_finish_serving` + `TstHlsServerHandle` +
  Python `HlsPublisher.finish_serving()`).
- **`forced_cuts` stats + `max_segment_duration` cap** — a hard wall-clock
  ceiling force-cuts an overdue keyframe (stalled / very long GOP) so
  segments never grow unbounded; each force-cut increments the new
  `forced_cuts` counter. Exposed on the Rust builder / stats, the C ABI
  (`tst_hls_publisher_builder_max_segment_duration_ms` /
  `tst_hls_publisher_get_forced_cuts`), and Python.
- **C ABI minor 17 → 18** (additive; all new symbols gated `TST_HAS_HLS`;
  no existing symbol, signature, or struct layout changed).

### Added — H.264-over-RTP ingest (RFC 6184)

- **`tst-rtp`:** `H264Depacketizer` — a state machine that reassembles H.264
  Access Units from RFC 6184 RTP packets. Handles single-NAL unit (types 1–23),
  STAP-A aggregation (type 24), and FU-A fragmentation (type 28); packetization
  modes 0 and 1. Interleaved-mode (mode 2) packet types that reach the
  depacketizer are discarded into `H264DepayStats::packets_discarded` — mode 2
  is rejected earlier, at RTSP SETUP (see the RTSP path bullet below).
  Sequence-gap detection, SSRC-change recovery, oversize-AU protection
  (`max_au_bytes`, default 8 MiB), `ParameterSetInjection::BeforeIdr`
  (prepends cached SPS/PPS before every IDR), counters via `H264DepayStats`.
  Feed-next_au-flush idiom mirrors `Demuxer` (PRs #94–#95).

- **`tst-rtp`:** `H264Receiver` — a blocking I/O shell wrapping `H264Depacketizer`
  over a UDP socket or mpsc channel (TCP-interleaved). `listen("rtp://host:port?pt=N")`
  for direct UDP; `socket_stats()`, `rtp_stats()`, `depay_stats()`, `cancel_handle()`,
  `close()`. `recv_au()` → `Option<H264Au>` with the EOS drain contract (PRs #94–#95).

- **`tst-rtp`:** RTSP H.264 path — `RtspClient::setup_h264_auto(&sdp)` picks the
  unique H.264 m-line, decodes `a=rtpmap` and `a=fmtp` (including base64
  `sprop-parameter-sets`), rejects mode 2, and returns `(RtspSession, H264DepayConfig)`.
  `RtspSession::into_h264_receiver(config)` consumes the session into an
  `H264Receiver` that owns the control connection and performs TEARDOWN on drop.
  `rtpmap` must advertise `H264/90000` — non-90 kHz clocks are rejected
  with `RtspError::UnsupportedRtpmap` (PR #95).

- **Python (`tstrans.rtp`):** `H264Receiver`, `H264AccessUnit`, `H264DepayConfig`,
  `ParameterSetInjection`, `H264DepayStats`, `RtpStats`; `RtspClient.connect_h264(config)`
  → `RtspSession`; `RtspSession.into_h264_receiver()` (session stays usable for
  `pause()`/`play()` — differs from JVM). Ships in published wheels (PR #96).

- **JVM (`org.tstrans.rtp`):** `H264Receiver`, `H264AccessUnit`, `H264DepayConfig`,
  `ParameterSetInjection`, `H264DepayStats`, `RtpStats`; `RtspClient.connectH264(config)`
  → `RtspSession`; `RtspSession.intoH264Receiver()` (consuming — session is closed on
  return; `pause()`/`play()` unavailable afterward; differs from Python). Ships in
  the published JAR (PR #96).

- **Example:** `examples/receiving/recv_rtsp_h264.rs` — RTSP H.264 → Muxer →
  `.ts` file gateway with per-step `// why + how` commentary. Compile gate in CI;
  run locally against MediaMTX or any ONVIF camera.

- **Fuzz target:** `crates/tst-rtp/fuzz/fuzz_targets/rtp_h264_depacketize.rs` —
  exercises the depacketizer state machine against arbitrary byte sequences
  (arbitrary `RtpHeader` + payload, repeated feed/next_au cycles).

### Added

- **`tst-core`:** `codec::misp_time` — MISB ST 0604 MISP Precision / Nano Precision Time
  Stamp SEI build + extract (H.264/H.265).

- **`tst-core`:** `Muxer::push_video_misp_to` / `push_video_misp_to_with_dts` — targeted
  mux push variants that splice a MISP Precision or Nano Precision Time Stamp SEI NAL
  immediately before the first VCL NAL of the access unit. `MuxError::MispTime` is the
  new error variant if the SEI build or splice fails.

- **`tst-pipeline`:** `MuxSender::send_video_misp_to` / `send_video_misp_to_with_dts` and
  `MuxPublisher::send_video_misp` — pipeline-shell variants that
  forward to the muxer's MISP-splice push path.

- **C ABI minor 18 → 19** (additive; no existing symbol, signature, or struct layout
  changed): `tst_muxer_push_video_misp_to`, `tst_muxer_push_video_misp_to_with_dts`
  (push + splice on the muxer handle), `tst_misp_time_extract` (scan an Annex-B access
  unit and return the first MISP timestamp), error codes
  `TST_E_MISP_TIME = -45` and `TST_E_MISP_TIME_MALFORMED = -46`.

- **Python (`tstrans.codec`):** `MispTimestamp` type (`kind` / `time_status` / `value`
  properties, `micros()` / `nanos()` constructors), `extract_misp_timestamp(au_bytes,
  codec)` helper. `Muxer.push_video_misp_to(handle, nal, *, pts, dts=None, key_frame,
  misp)` push + splice variant.

- **JVM (`org.tstrans.codec`):** `MispTimestamp` record (`micros` / `nanos` factories),
  `MispTimestamp.extract(byte[], VideoCodec)` static helper. `Muxer.pushVideoMispTo`
  overloads (with and without DTS) push + splice variants.

- `tcp://` and `tcps://` caller URLs now accept DNS hostnames in addition to IP
  literals. Resolution happens at connect time (never at parse time). For
  `tcps://`, TLS presents the dialed name as the SNI and verifies the server
  certificate against it — dial a hostname for a `dnsName` SAN, or an IP
  literal for an `iPAddress` SAN. Listener URLs (`?listen=1`) still require IP
  literals (DA-NET-9, PR #91).

- ST 0601 INT_MIN "sentinel" values (out-of-range / reserved / not-available)
  are now modeled explicitly instead of being rejected. `UasDatalinkLs` gains a
  `sentinel_tags` field, plus `St0601SentinelMeaning` and
  `st0601_sentinel_meaning()` covering the full ST 0601.19 sentinel table.
  Encoding re-emits INT_MIN for listed-but-`None` fields (a present value still
  wins). Mirrored in the bindings as a Python tuple and a JVM `List<Long>`
  (PR #79).
- `tst_tcp` gains a `TcpCancelHandle` with `cancel_handle()`, giving the TCP
  transport a cooperative cancellation path; the Python TCP transport exposes
  it too (PR #62).
- `NativeLoader` now accepts a `-Dtstrans.native.lib=<path>` JVM system
  property as a developer override: when set to a non-empty path, the loader
  skips JAR extraction and calls `System.load` on that path directly, making
  it easy to point at a freshly-compiled debug build without repackaging the
  JAR (DA-JVM-2, PR #83).
- `DemuxerConfig::sync_buf_cap` (field) and `DemuxerConfigBuilder::sync_buf_cap(bytes)`
  make the demuxer's pre-sync ingress ceiling configurable (default 4 MiB).
  Previously a hidden constant, this ceiling silently rejected whole-file
  `feed()` calls over 4 MiB — including valid single-shot inputs from
  file-based pipelines — with an error message that pointed at `pes_cap_*`
  knobs (a dead end). The error message now names the actual knob
  (`DemuxerConfig::sync_buf_cap`) and the chunk-and-drain pattern. Callers
  feeding files larger than 4 MiB should either raise the ceiling or switch to
  chunked feeding. (an integrator field report 2026-07-08.)
- ST 0601 encode: opt-in `OutOfRangePolicy::Indicator` emits the spec's
  Out-of-Range special value (`0x8000`/`0x80000000`, ST 0601.19 §7.5) for
  Tags 6, 7, 50–52, 79, 80, 90–93 instead of erroring (of these, Tags 6, 7,
  50, 90, 91 are currently encodable typed fields); `encode_to_vec_with`;
  Python `out_of_range_policy=` kwarg; JVM `encodeUasDatalink(record, policy)`
  overload. Default behavior unchanged. (PR #92)

- **`tst-core` — ST 0601 Tag 94 (MIIS Core Identifier) and `klv::st1204`
  binary codec:** `UasDatalinkLs` gains a `miis_core_id: Option<Vec<u8>>`
  field (Tag 94). The new `tst_core::klv::st1204` module provides
  `CoreId` / `IdType` / `St1204Error`, with `decode(&[u8]) → Result<CoreId,
  St1204Error>`, `encode_to_vec(&CoreId) → Vec<u8>`, `CoreId::new(…)`,
  `check_value(…)` (Appendix B check-digit function), and `Display` for
  `CoreId` per ST 1204.3 §7.4.2 textual format. The textual form is
  verified against the Appendix B check-value vector.

- **`tst-core` — ST 0902 MISMMS record validator:** `st0601::validate_mismms`
  checks an encoded `UasDatalinkLs` for MISMMS (Minimum Interoperability
  Set for Metadata Management Systems) conformance and returns a
  `Vec<MismmsViolation>`. `MismmsViolation` (non_exhaustive) carries the
  violated item and a human-readable description. No C ABI change — KLV
  validation stays out of the C ABI, matching the encode-hardening precedent
  (WP-F, PR #51).

- **Python (`tstrans.klv`):** `decode_core_id(buf)` / `encode_core_id(core_id)`
  / `core_id_text(core_id)` functions; `IdType` / `CoreId` / `MismmsViolation`
  dataclasses; `validate_mismms(record)` validator; `UasDatalinkLs.miis_core_id`
  field (Tag 94, `bytes | None`).

- **JVM (`org.tstrans.klv`):** `Klv.decodeCoreId(byte[]) → CoreId` /
  `Klv.encodeCoreId(CoreId) → byte[]` / `Klv.coreIdText(CoreId) → String` /
  `Klv.validateMismms(UasDatalinkLs) → List<MismmsViolation>` static
  helpers; `IdType` enum; `CoreId` / `MismmsViolation` records;
  `UasDatalinkLs.miisCoreId()` accessor and `Builder.miisCoreId(byte[])`.
  No C ABI change (same as above).

- **Docs:** `docs/reference/stanag-4609.md` — STANAG 4609 / MISP conformance
  reference page (conformance matrix, per-language snippets for the MISP
  timestamp push and MISMMS validator, deferred-features cross-links). Cookbook
  Recipe 35 "Building a STANAG 4609-conformant stream" — H.264 + sync-KLV muxer
  with per-frame MISP timestamp splice, MISMMS validator gate, strict-compliance
  encode, and Tag 94 Core ID; Rust primary + Python variant.

### Changed — default features and transport behavior

- `tst-tcp`'s default features drop the experimental `hls` feature:
  `default = ["tls"]`. Consumers relying on HLS being on by default must now
  request it explicitly (PR #79).
- TCP transports now default `TCP_NODELAY` to `true` for low-latency streaming
  (PR #71).
- RTP/RTSP statistics now report `rtt_us` as `0` rather than a misleading
  cross-domain computation; a real RTT is not available on that path (PR #73).
- SRT statistics preserve sub-millisecond RTT precision in `perf_to_stats`
  instead of truncating to whole milliseconds (PR #61).

### Fixed — MPEG-TS mux/demux conformance

- Muxer: the PCR-only heartbeat packet now repeats the last payload continuity
  counter instead of advancing it, per H.222.0 (DA-MUX-1, PR #66).
- Demuxer: spec-legal duplicate TS packets (H.222.0 §2.4.3.3 — a packet
  repeated bit-for-bit, optionally with a refreshed PCR) are now recognized and
  suppressed instead of being reported as continuity-counter jumps. A
  same-CC-but-different-payload packet still routes through the discontinuity
  path (DA-DEMUX-1, PR #66).
- Demuxer: a stale PCR-tracking entry is dropped when a PMT moves the PCR PID,
  and PES/CC/PTS state is flushed for a PID that changes stream kind across a
  PMT update (DA-DEMUX-2 / DA-DEMUX-3, PR #66).
- Muxer: the PMT section-size estimator is now derived from the same
  descriptor-cache emitter that writes the section, structurally eliminating
  the estimator/emitter drift class (and the prior AC-3 under-estimate panic)
  (DA-MUX-2, PR #65).
- Muxer config: descriptor bodies of 254 or 255 bytes are no longer
  incorrectly rejected. H.222.0 §2.6 defines `descriptor_length` as an 8-bit
  field, so 0..=255 bytes are all valid; the old `declared > 253` guard was
  without documented basis. A 254- or 255-byte body necessarily exceeds the
  183-byte single-packet PMT limit, so such descriptors are always rejected by
  the separate `PmtTooLarge` check — the practical change is error
  classification only (DA-MUX-3, PR #81).

### Changed — pipeline send semantics

- The pre-muxed `Sender` now retains framed bundles across a mid-loop transport
  error and drains them first on the next call, so a transient failure no
  longer drops the framed prefix. This clarifies the send contract: an `Err`
  from `send_ts` means the input **was** consumed (framed and queued) — callers
  must not re-send the same bytes; recover by backing off and calling `flush()`
  or the next `send_ts` with new data (DA-PIPE-3, PR #69).
- `MuxSender`'s input-consumption / retry contract is now documented across the
  full `send_*` family: because a pre-push `drain_pending()` can fail before the
  push, "did the transport error consume my input?" is answered by the *previous*
  call's outcome (a success means the pending queue was empty, so the first
  error after it consumed the input; an error after an error may not have).
  Loss-sensitive callers are pointed at `ManagedTransport` (PR #78).

### Fixed — codec parsing (AC-3)

- `codec::ac3` now reads and classifies `bsid` before validating
  `fscod`/`frmsizecod`. An E-AC-3 frame (bsid 11..=16) whose `fscod` or
  `frmsizecod` bits happen to be invalid under AC-3 field constraints now
  correctly returns `UnsupportedProfile` rather than `Forbidden` or
  `ReservedValue` — the AC-3 field constraints are inapplicable once the
  bitstream is identified as E-AC-3 (DA-AV-3, PR #81).

### Fixed — codec parsing (H.264)

- `codec::h264` now enforces H.264 spec bounds on VUI fields:
  `log2_max_frame_num_minus4` / `log2_max_pic_order_cnt_lsb_minus4` are
  range-checked, `chroma_sample_loc_type` is bounded to `[0, 5]`, and a
  `num_units_in_tick == 0` VUI (which would divide by zero when deriving the
  frame rate) is rejected. Malformed or hostile bitstreams are now rejected
  rather than silently mis-parsed (PR #67).

### Fixed — KLV codec (IMAPB non-finite decode, fuzz-found)

- `decode_imapb` no longer returns a non-finite `Value` for degenerate IMAPB
  ranges. For parameters such as `min ≈ −f64::MAX, max = tiny subnormal`,
  the scale factor `sF` becomes extremely small, making `sR = 1/sF` enormous.
  The resulting `value = sR·(y−Zoffset) + min` could overflow to `±∞`; the
  existing lower-bound epsilon guard failed to catch this because the
  subtraction `p.min − epsilon` also overflowed to `−∞` (its magnitude exceeded f64::MAX), making the guard
  `value < −∞` always false. The fix guards non-finite arithmetic results
  immediately after computing `value`, returning `OutOfRange { decoded }` —
  the correct classification for any non-usable arithmetic result.
  Bug found by the `klv_imapb` libFuzzer target (PR #84).

### Fixed — KLV codec (IMAPB reserved-space detection)

- `decode_imapb` now detects inter-band reserved integers using an exact
  integer-domain comparison (`y > y_max`, where `y_max = floor(sF·(b−a) +
  Zoffset)` matches what the encoder produces for `value = max`) rather than
  a float epsilon. The old epsilon-based upper-bound admitted the integer
  immediately above `y_max` as `Value` at coarse grids (L=1), because the
  tolerance was exactly one quantization step and the comparison was not
  strictly greater. At the shipped ST 0903 shapes (L=2/L=3), exactly one
  reserved integer (`y_max + 1`) was likewise admitted as a `Value` decoding
  to `max + epsilon`; it is now classified `OutOfRange`. Legitimately-encoded
  values are unaffected at every length (audit finding F-02,
  `crates/tst-core/src/klv/imapb.rs`, PR #81).

### Fixed — KLV encoding conformance

- ST 0107.5 empty-string vs. absent handling (§6.3.3.2): decode maps `[0x00]`
  to `Some("")` and a zero-length value to `None`, and encode mirrors the
  distinction (PR #68).
- The strict-compliance encode path now trims leading/trailing whitespace
  (ST 0107.3-12) and strips control characters; the default (lenient) encode
  path is unchanged (DA-KLV-1 / DA-KLV-2, PR #68).
- ST 0903 `VTargetPack` encoding now enforces the wire-width caps on its
  fields (DA-KLVC-1, PR #68).

### Fixed — transport and bindings

- Python: the TCP transport no longer freezes the interpreter. Native TCP
  calls (`stats`, `close`, `peer_addr`, `repr`, recv) now release the GIL, and
  the GIL-held-lock that could block all Python threads is gone (DA-PY-1,
  PR #62). The srt `ManagedDemuxReceiver.socket_stats` / `ManagedMuxSender.stats`
  calls also release the GIL (DA-PY-2, PR #62).
- Python: byte-like inputs (`bytearray`, `memoryview`) are now accepted wherever
  `bytes` are, instead of being rejected (DA-PY-3, PR #74).
- JVM: decoding an ST 0903 VMTI packet with many targets no longer leaks/exhausts
  JNI local references — per-target local refs are reclaimed in `build_vmti`
  (DA-JVM-1, PR #63).
- SRT: a connect that times out is now classified as `ConnectError::TimedOut`
  instead of a generic setup failure, so that variant is actually reachable
  (DA-SRT-2, PR #61).
- UDP: `max_payload` now returns 65535 so a full-size datagram is not silently
  truncated; multicast receive binds set `SO_REUSEADDR` (and `SO_REUSEPORT`
  on BSD/macOS) so multiple receivers can join a group (DA-NET-7 / DA-NET-8,
  PR #71).
- RTSP: the server's Digest-auth checks are hardened against realm / nonce-count
  / qop-downgrade abuse (DA-RTP-4); the `PLAY` response reports the actual
  initial sequence number and RTP timestamp in `RTP-Info`; and IPv6 hosts are
  bracketed correctly in rendered request URIs (PR #73).
- RTSP: server credential checks are constant-time — Basic auth compares
  username and password unconditionally (no early-exit on username mismatch),
  and Digest auth compares the response attribute with constant-time equality,
  removing timing side-channels that could reveal whether credentials are
  partially correct (PR #82).
- RTSP: the DESCRIBE response now emits the conventional SDP shape for
  third-party client interop: `m=video <port> RTP/AVP 33` (RFC 2250 §2,
  replacing `m=application`), a session-level `a=control:*` (RFC 7826
  App. D), and a `Content-Base` header with trailing slash (RFC 7826 §D).
  The in-tree `RtspClient` accepts both the old and new shapes (selection
  is PT=33-based, not media-type-name-based) (DA-RTP-8, PR #82).
- RTCP: the reporter thread no longer panics when the OS RNG is unavailable
  (`getrandom` failure). It now warns once and falls back to a deterministic
  wrapping-counter jitter, keeping RTCP emissions at ~`RTCP_BASE_INTERVAL`
  (PR #82).
- RTSP: the server's auth-failure counter (3-strike session-close guard) no
  longer resets when the client sends an OPTIONS or GET_PARAMETER request.
  Previously, an attacker could interleave OPTIONS (always 200, never
  auth-gated) between bad-auth DESCRIBE/SETUP/PLAY requests to keep the
  counter ≤ 1 indefinitely. The counter now resets only on a successful (2xx)
  response from an auth-gated method; OPTIONS and GET_PARAMETER leave it
  untouched (PR #82).
- RTP: the recv path now validates RFC 2250 MP2T payload shape after a
  successful RTP header parse (PT=33 confirmed). Payloads that are empty,
  not an integral multiple of 188 bytes, or do not start with the TS sync
  byte `0x47` are silently dropped and counted in `RtpStats::malformed_packets`
  instead of being forwarded to the demuxer. The same check applies to the
  TCP-interleaved mpsc path. The demuxer's own resync logic is unchanged and
  remains defense-in-depth (DA-RTP-5, PR #82).
- RTP: `RtpRecvTransport`'s `bytes_received` / `packets_received` counters
  (surfaced via `socket_stats()`) are now incremented at wire-level — before
  RTP header or MP2T shape validation — on both the UDP and TCP-interleaved
  mpsc paths. Previously the mpsc path incremented only after the MP2T shape
  guard, so malformed-but-received chunks were undercounted relative to the
  UDP path. Both paths now consistently count every received datagram or
  channel chunk; malformed drops remain separately tracked in
  `RtpStats::malformed_packets` (PR #82).
- RTP: `RtpRecvTransport` no longer truncates full-size (7×188) MP2T-over-RTP
  datagrams. The receive buffer was sized to `pkt_size` (default 1316 bytes),
  which cannot hold a full 7×188 TS bundle plus the 12-byte RTP header
  (1328 bytes) — let alone CSRC- or extension-bearing headers —
  and `UdpSocket::recv` silently discards the excess: silent corruption in
  0.2.0, silent drops after the MP2T shape guard landed. The receive buffer
  is now sized to the 16-bit datagram ceiling (65535 bytes) on both the UDP
  and TCP-interleaved paths, so no legal datagram or interleaved frame can
  truncate. Only traffic from foreign senders was affected — the in-tree
  sender caps datagrams below the old buffer size. Note: this raised the
  transport-level scratch ceiling only; the pipeline receive shells'
  send-side-budget sizing (1304 bytes) has since been lifted too — see the
  receive-side `max_payload()` entries below (PR #95).
- RTSP: `extract_mount_path` now strips a single trailing `/` from the
  resolved path (except for the root `"/"`) so `rtsp://host/live/` and
  `rtsp://host/live` resolve to the same registered mount. Previously, a
  DESCRIBE with a trailing slash would get a 404 for a valid mount, and a
  client that DESCRIBE'd at `/live/` but then SETUP'd at `/live` would find
  different mounts. The normalization is applied consistently across DESCRIBE
  and SETUP (PR #82).
- JVM: the muxer config-enum ordinal decode helpers (`VideoCodec`, `AudioCodec`,
  `KlvStreamType`, `Av1CarriageMode`) and the SRT reconnect-policy ordinals
  (`BackoffStrategy`, `OverflowPolicy` in `build_reconnect_policy`) now throw
  `CONFIG_INVALID` instead of silently falling back to a default. In normal
  usage ordinals flow only through the typed Java enums, so the valid range is
  always respected; this rejection catches version-skew where a caller compiled
  against a newer binding passes an ordinal unknown to the native library
  (DA-JVM-3, PR #83).
- `NativeLoader` now extracts the native library to a stable,
  content-addressed directory (`<tmpdir>/tstrans-native-<hash>/`) instead of a
  randomly-named temp file. On load, stale sibling directories from previous
  JAR versions are swept best-effort. On Windows, `deleteOnExit` cannot remove
  a DLL that is still loaded; the stable layout means only one copy accumulates
  per JAR version, and the sweep removes copies from older versions once they
  are no longer locked. The stable-dir layout applies on all platforms; Linux
  and macOS never accumulated temp files across restarts, and their behavior is
  otherwise unchanged (DA-JVM-2, PR #83).

### Changed — Python demux event payload laziness (Subtitle / UnknownSample)

- `DemuxEvent.Subtitle` and `DemuxEvent.UnknownSample` now defer the
  `bytes` materialization to the first `.payload` access, matching the
  existing lazy pattern on `DemuxEvent.Video.raw` / `.Audio.raw` (WP-E
  PY-01). Both `SamplePayload::Subtitle.payload` and
  `SamplePayload::Unknown.raw` are `SharedBytes` in tst-core, so the
  Rust binding passes a cheap Arc clone to a `RawBytes` holder — no
  payload copy at event creation. `DemuxEvent.Metadata.payload` remains
  eager (`Vec<u8>` source, copy unavoidable at the `&DemuxEvent` call
  boundary) (DA-PERF-13, PR #83).

### Fixed — pipeline shell panic safety

- `MuxPublisher`'s fallible methods (`send_video`, `send_klv`, `send_audio`,
  `send_subtitle`, `send_data`, `cut_segment`) now return
  `MuxPublisherError::LockPoisoned` instead of panicking when the inner mutex is
  poisoned. Infallible methods (`stats`, `publisher_stats`) recover the poisoned
  guard and return the last observed value. `finish` recovers the poisoned guard
  and returns the owned publisher. Aligns `MuxPublisher` with the established
  crate-wide poison policy (DA-PIPE-5, PR #83).

### Changed — documentation

- README and the docs landing/overview pages were rewritten MPEG-TS-first:
  parse/build/inspect leads, transports follow; the "what it isn't / look
  elsewhere" sections were replaced by positive scope framing ("Works with
  your stack" / "Scope boundaries — and what to pair it with").
- The cookbook was reorganized for task-first browsing: recipes are no longer
  numbered (references are title links now), files carry descriptive slugs, and
  recipes live under seven task sections — `muxing/`, `sending/`, `receiving/`,
  `pairing/`, `klv/`, `codecs/`, `operations/`. Deep links into the old
  `docs/cookbook/<section>/<NN>-*.md` paths must be re-resolved via
  [the cookbook index](docs/cookbook/index.md).
- Added an embedded (`no_std` / bare-metal) entry page at `docs/languages/embedded.md` with index + README routing (WP-EMB-6).
- The MPEG-TS demux guide (`docs/guides/mpegts-demux.md`) now documents
  the sync-ingress ceiling (`DemuxerConfig::sync_buf_cap`, default 4 MiB),
  the chunk-and-drain loop for file replay, a new entry in the override-knobs
  table, and a concise prose paragraph describing each `DemuxEvent` variant
  (including the `Metadata` / `Klv` alias relationship).
- The Python guide (`docs/languages/python.md`) adds the same ceiling note
  with a Python chunk-and-drain snippet, a `DemuxerConfig(sync_buf_cap=…)`
  alternative, a full `DemuxEvent` variant reference with `match`/`isinstance`
  examples, and a `Muxer.pull()` drain-loop recipe for callers that need
  TS bytes in memory rather than writing to a file.
- HLS conformance wording qualified in `docs/project/deferred-features.md`
  and `docs/reference/compatibility.md`: the v0.2.0 fix covered the
  playlist-model (target-duration ceiling, duration-floor eviction, media-PTS
  `#EXTINF`); segment-initial decodability and HTTP-server hardening (path
  traversal) remain open.

### Changed — internal refactors and dependencies (behavior-preserving)

- Removed unused dependencies: `log`, `rtsp-types`, the `cc` dev-dependency, and
  the `tokio` `fs` feature (PR #58).
- Large behavior-preserving deduplication across the tree: mux/demux/codec/KLV
  helper consolidation (PRs #65–#69), pipeline `ShellSpan` and scaffolding
  cleanup (PRs #69, #70), JVM `NativeHandle` base class and handle/builder
  dedup (PRs #75–#76), and a ~2000-LOC C-binding transport-clone dedup behind
  byte-frozen forwarders plus a shared descriptor-slot resolver and zero-copy
  arena path (PR #77). Public API and generated C header were held byte-stable
  where surfaces were untouched.
- The CI `cargo public-api` toolchain is pinned to `nightly-2026-07-03` to stop
  a recurring `std::io` / `core::io` render false-drift; render baselines with
  `cargo +nightly-2026-07-03 public-api` (PR #78).

### Fixed — 32-bit C ABI layout protection (embedded audit WP-EMB-4/5)

- The `tstrans.h` ABI layout asserts are now pointer-width-aware: the
  pointer-bearing event-view structs pin per-width expected sizes (64-bit
  and 32-bit), so 32-bit consumers no longer need `-DTST_SKIP_ABI_ASSERTS`
  to build — and get layout-drift protection for the first time. Matching
  `target_pointer_width = "32"` const asserts added on the Rust side. The
  C-firmware QEMU gate now compiles with the asserts enabled and
  additionally demuxes its own output, validating `tst_event_t` /
  `tst_stream_info_t` / `tst_nal_t` field-by-field across the C↔Rust
  boundary on a 32-bit target (EMB-ABI32-1).
- Documented the `no_std` one-sender-per-task / one-handle-per-task
  concurrency contract on `MuxSender`, the pipeline `no_std` docs, the C
  handle layer, and `docs/languages/embedded.md`; `tst_get_last_error_str`
  now documents its process-global `no_std` storage. Corrected the
  portable-atomic story (native-atomics floor; `fallback` needs no embedder
  hook) and stale FreeRTOS substrate comments (EMB-MUTEX-1 + T-G;
  doc-comments only, no behavior change).

### Changed — receive-side `max_payload()` now reports the deliverable ceiling

- `RecvTransport::max_payload` is now contractually the *deliverable
  ceiling* of the protocol (the largest message a conformant remote
  sender can produce), decoupled from the send-side packet-size budget
  on `Transport::max_payload`. Per transport: RTP returns 65523
  (65535 − 12-byte RTP header, both UDP and TCP-interleaved arms), SRT
  returns at least the 1456 live-mode wire maximum, RIST returns 65535.
  UDP already reported 65535; TCP (stream, no message boundaries) is
  unchanged.

### Fixed

- Full-MTU foreign packets through the receive shells: `Receiver` /
  `RawReceiver` / `DemuxReceiver` sized their buffers from the send-side
  budget, so a conformant foreign sender's oversize message was lost or
  corrupted — a full-MTU RTP bundle (7×188 = 1316 B > 1304) surfaced as
  `Broken("recv buf too small")`, a 1456-byte SRT message was silently
  truncated to 1316 bytes, and an oversize RIST block was silently
  dropped. The direct RTP receive wrappers in the bindings — Python
  `tstrans.rtp.Receiver` and JVM `org.tstrans.rtp.Receiver` — had the
  same 1304-byte limit (foreign full-MTU bundles raised their
  `Broken`-kind errors) and are fixed by the same ceiling change.
  Completes the v0.2.0 recv-truncation fix, which had raised
  the transport-level scratch ceiling but not the shell sizing.
- `sprop-parameter-sets` entries without trailing `=` base64 padding
  (common camera/server spelling) are now decoded instead of being
  silently skipped, so out-of-band SPS/PPS injection works with such
  sources. Invalid characters are still rejected.

### Removed (breaking, pre-1.0)
- Receive-side `pkt_size` knobs, inert since the recv-ceiling change
  ([Unreleased] above): Rust `RtpRecvSocketBuilder::pkt_size` /
  `UdpRecvTransportBuilder::pkt_size` / `RistRecvTransportBuilder::pkt_size`,
  Python `tstrans.rtp.Receiver(pkt_size=)` kwarg and the udp/rist receive
  builders' `pkt_size` methods, and JVM
  `org.tstrans.rtp.Receiver.fromUrl(url, pktSize)`. Send-side `pkt_size`
  everywhere and TCP's receive-side read-granularity knob are unchanged.

### Changed (breaking, pre-1.0)
- Receive-side URLs now REJECT `?pkt_size=` with a teaching error
  ("pkt_size is a send-side knob…") instead of silently ignoring it —
  `rtp://` receive entry points (including the H.264 elementary path) and
  `udp://` receive entry points, across Rust, C, Python, and JVM. Strip the
  parameter from URLs shared verbatim with senders.

### Fixed
- `ManagedRecvTransport::max_payload` no longer reports a fixed 1316 while
  the inner transport is mid-reconnect; it reports the most recent live
  inner's deliverable ceiling (cached at construction, refreshed per
  successful rebuild). Direct-caller edge only — pipeline shells were
  unaffected.

### Fixed — post-v0.2.0 review hardening
- H.264-over-RTP depacketizer: cached SPS/PPS parameter-set injection
  (`ParameterSetInjection::BeforeIdr`) no longer bypasses the
  `H264DepayConfig::max_au_bytes` cap, on three levels: (1) an over-cap
  injected AU is dropped instead of emitted; (2) the injected AU length is
  now preflighted so the over-cap prefix is never allocated/copied (closing a
  memcpy-amplification vector where primed near-cap SPS+PPS made every later
  IDR rebuild and drop a large prefix); and (3) parameter sets whose framed
  length alone exceeds the cap are never retained in the SPS/PPS cache, whether
  in-band or seeded from `initial_parameter_sets` / SDP `sprop-parameter-sets`.
- H.264-over-RTP: `H264DepayConfig(max_au_bytes=0)` is now rejected by the
  Python binding (raises `ValueError`), matching the JVM builder. A zero cap
  drops every AU and is never useful; the raw Rust struct still accepts it
  (documented as degenerate).
- HLS `#EXTINF` duration: `MuxPublisher`'s media-span computation no longer
  overflows its intermediate `ticks * 1_000_000_000` product for PTS deltas
  beyond ~56.9 h (debug panic / release wrap). The math now decomposes the
  tick count into whole seconds (`ticks / 90_000`) plus sub-second nanoseconds
  (`(ticks % 90_000) * 1e9 / 90_000`), neither of which can overflow, so the
  span is exact with no clamp or truncation. Not reachable from conformant
  33-bit MPEG-TS PTS, only via large synthetic/mis-scaled PTS through the
  public publisher API.

### Fixed — documentation accuracy
- STANAG 4609 conformance matrix: the ST 0902 `validate_mismms` and ST 1204
  Core Identifier rows said "All four language bindings"; the typed KLV set
  APIs are not exposed in the C ABI (which carries raw KLV bytes only), so
  they now read "Rust, Python, and JVM".
- C `tst_hls_publisher_builder_new` rustdoc (and the generated `tstrans.h`)
  documented stale defaults (`0.0.0.0:0`, 6 s segments, no output dir); they
  now state the actual loopback bind `127.0.0.1:8080`, 4 s segments, and the
  `<system-temp>/tstrans-hls` output dir. The bind default is security-relevant.
- C `tst_misp_time_extract` rustdoc implied codec-gated UUID acceptance; the
  extractor accepts any known ST 0604 MISP UUID once an SEI payload is found,
  and the doc now says so.

---

## [0.2.0] — 2026-06-23

### Fixed — H.265 parameter-set IDs validated instead of silently truncated

- `codec::h265` now validates the four `ue(v)`-decoded values cast to `u8`
  during PPS and SPS parsing — PPS `pps_pic_parameter_set_id`, PPS
  `pps_seq_parameter_set_id`, SPS `sps_seq_parameter_set_id`, and the SPS VUI
  `chroma_sample_loc_type_top_field` — and returns
  `CodecParseError::ReservedValue` when any value exceeds the H.265 spec
  ceiling. Previously out-of-range values silently truncated or wrapped,
  potentially aliasing an unrelated parameter-set map entry on a malformed
  or hostile bitstream.

### Fixed — RIST receiver recovers from oversize and malformed datagrams

- A RIST receiver that encounters an oversize datagram (larger than the
  caller-provided receive buffer) or a malformed null-payload block no
  longer shuts down permanently. The offending datagram is now dropped and
  counted; the receive call returns `TransportError::Backpressure` so the
  caller can retry. Previously one such datagram closed the receiver forever.

### Changed — RIST transport statistics now populated

- `tst-rist` now wires librist's statistics callback and reports real
  counters through `socket_stats()` and `RistStats`: `packets_retransmitted`,
  `packets_dropped`, bandwidth (`RistStats::bandwidth_kbps`, also surfaced as
  `SocketStats::link_bandwidth_bps`), and `rtt` are populated from the live
  librist stats callback. Previously those four counters were always zero and
  `socket_stats()` returned `None` for RIST transports.

### Added — JVM data-stream surface (`org.tstrans`)

- **tst-jni** gains the data-stream mux surface, mirroring the Python
  binding below: `MuxerConfig.Builder.addData(pid, streamType, carriesPts)`
  + `streamDescriptorsForData(dataIndex, descriptors)`, the
  `DataStreamHandle` record, the offline `Muxer.pushData` / `pushDataTo`
  pair plus the `dataHandles()` / `dataStreamHandle(index)` accessors,
  `MuxerFileSink.pushData` / `pushDataTo`, `pushData` / `pushDataTo` /
  `dataHandle()` on the srt and rtp `MuxSender`s, the srt
  `ManagedMuxSender.pushData` / `pushDataTo` / `dataHandle()` pair, and the
  rtp `MountHandle.pushData` / `pushDataTo` / `dataHandle()` / `dataHandles()`
  family. Pass-through / PTS / size-ceiling semantics are those of the Rust
  `push_data` family (see the muxer + pipeline entry below). Not yet exposed:
  `data_handles_for_program` (the JVM `MuxerConfig` is single-program).

### Added — C data-stream surface (ABI minor 12 → 13)

- **tst-c** gains the data-stream mux surface, binding the Rust
  muxer/pipeline family below: `tst_data_stream_handle_t` plus seven new
  entry points — `tst_mux_config_add_data_stream`,
  `tst_mux_config_set_stream_descriptors_for_data`,
  `tst_mux_config_add_data_descriptor`, the offline
  `tst_muxer_push_data` / `tst_muxer_push_data_to` pair (unconditional),
  and the `tst_mux_sender_send_data` / `tst_mux_sender_send_data_to` pair
  (behind `TST_HAS_SRT`). Pass-through / PTS / size-ceiling semantics are
  those of the Rust `push_data` family (see the muxer + pipeline entry
  below). ABI minor 11 → 12; additive — no symbol removed, no signature or
  struct layout changed.
- **tst-c** further gains the managed-sender and RTSP-mount data push pairs:
  `tst_managed_mux_sender_send_data` / `_to` (behind `TST_HAS_SRT`) and
  `tst_rtsp_mount_push_data` / `_to` (behind `TST_HAS_RTP`), mirroring the
  `send_klv` / `push_klv` families. ABI minor 12 → 13; additive — no symbol
  removed, no signature or struct layout changed.

### Added — Python data-stream surface (`tstrans.mpegts` + srt/rtp `MuxSender`)

- **`tstrans.mpegts`** gains the data-stream mux surface, mirroring the Rust
  muxer below: `MuxerProgramConfigBuilder.add_data(pid, stream_type, *,
  carries_pts)` + `stream_descriptors_for_data(index, descriptors)`,
  `Muxer.push_data` / `push_data_to`, the `DataStreamHandle` type, and the
  handle accessors `data_handles()` / `data_stream_handle(index)` /
  `data_handles_for_program(program_number)`.
- **`tstrans.srt.MuxSender` / `tstrans.rtp.MuxSender`** gain `push_data` /
  `push_data_to` / `data_handle()` — push raw private-PES payloads through a
  live sender, following the `push_klv` family shape.
- **`tstrans.srt.ManagedMuxSender`** and **`tstrans.rtp.MountHandle`** gain the
  same `push_data` / `push_data_to` / `data_handle()` trio, completing data
  parity across every live sender / mount shell.

### Changed — `tio.transmux` passes private/application data streams through

- **`tio.transmux`** now routes data samples (demux
  `DemuxEvent.UnknownSample`) through `push_data_to`, so
  private/application data streams pass through byte-faithfully alongside
  video / audio / KLV (was: skipped/errored). DVB subtitling/teletext remain
  the strict-fail offenders; `drop=(StreamKindTag.UNKNOWN,)` remains the
  opt-out for excluding data streams. PTS nuance: re-muxed data streams
  always carry PTS and the demuxer substitutes 0 for a PTS-less source PES,
  so a source sample with no PTS re-emerges with a literal PTS of 0.

### Changed — tst-jni panic safety + handle-decode hardening (`org.tstrans`)

- An unexpected Rust panic inside any `org.tstrans` JVM native now surfaces as
  a `java.lang.RuntimeException` instead of aborting the JVM process — every
  native is wrapped in a `jni_catch` panic boundary (the JVM twin of the C
  binding's `ffi_catch`).
- `Muxer.pull`'s JNI-bridge failure cases now throw an unchecked
  `RuntimeException` rather than a checked `MuxException` (the hot drain path
  keeps no `throws` clause), and every handle-targeted `*To` push native now
  rejects out-of-range / negative stream handles instead of silently
  truncating them to a valid handle.

### Added — private/application data streams (muxer + pipeline)

Arbitrary private PES carriage: declare a data elementary stream with an
explicit PMT stream_type, push raw payload bytes through, and the demuxer
round-trips the stream as `StreamKind::Unknown(stream_type)` with
byte-identical payloads.

- **`tst_core::mpegts::mux::StreamSpec::Data { pid, stream_type, carries_pts }`**
  — new stream-spec variant: a pass-through PES `private_stream_1` (0xBD)
  stream with a caller-chosen PMT `stream_type`; the write-side dual of demux
  `StreamKind::Unknown(stream_type)`. Declared via
  `MuxerProgramConfigBuilder::add_data(pid, stream_type, carries_pts)`; raw
  PMT descriptor TLVs attach via `stream_descriptors_for_data` (the muxer
  never auto-emits a descriptor on a data stream). Mux-side `StreamKind`
  gains a `Data` variant for handle/error reporting.
- Validation: the classify-Unknown anti-masquerade rule — a Data stream's
  `stream_type` + descriptor set must classify as `Unknown` under the demux
  PMT cascade, so a Data spec cannot masquerade as a typed video / audio /
  KLV / subtitle stream (`0x06` is allowed only without classifying
  descriptors). Data streams are capped at 16 per program
  (`MuxError::TooManyDataStreams`) and are PCR-ineligible
  (`MuxError::DataPidUsedAsPcrPid`; the no-PCR-eligible-stream guard now
  covers data-only programs too).
- **`Muxer::push_data` / `push_data_to`** — pure pass-through emission: one
  push = one PES packet, no AU-cell framing, no payload inspection. New
  `DataStreamHandle` plus accessors `data_handles` / `data_stream_handle` /
  `data_handles_for_program`. New `MuxError::{NoDataStreamsConfigured,
  DataTooLarge}`; payloads are bounded by the PES_packet_length ceiling
  (65 527 bytes with PTS, 65 532 without). `carries_pts = false` omits the
  PES PTS field entirely; the demuxer surfaces such samples with `pts = 0`.
- **`tst_pipeline::MuxSender::{send_data, send_data_to, data_handles}`** and
  **`MuxPublisher::send_data`** — pipeline delegates to the muxer push
  family.

### Changed — `MuxerConfig::from_program_map` converts unknown stream types to Data specs

- **`MuxerConfig::from_program_map`** (Rust + Python) now maps
  `StreamKind::Unknown(stream_type)` streams to `StreamSpec::Data`
  pass-through entries instead of strict-failing the conversion naming them
  as offenders unless dropped: the raw PMT `stream_type` byte is kept and
  the stream's PMT descriptors are preserved verbatim (re-encoded
  TLV-for-TLV; the muxer never auto-emits descriptors on a data stream);
  `carries_pts` is always `true` (a PES-level property the PMT cannot
  declare); the PCR is never copied onto a data PID — data joins KLV in the
  PCR-ineligible class. `drop=[StreamKindTag::Unknown]` still excludes
  unknown streams entirely. Caveat: a `treat_as`-forced `Unknown` on a
  stream whose stream_type/descriptors classify as a typed kind converts to
  a Data spec that `validate()` rejects (the anti-masquerade rule), naming
  the stream and the classified kind.
- `tio.transmux` consumes the conversion: private/application data streams
  in a source pass through byte-faithfully — see the dedicated transmux
  entry above. DVB subtitling/teletext remain the only strict-fail
  offenders.
- `from_program_map`'s PCR copy rule: subtitle PIDs join KLV and data in the
  PCR-ineligible class — a source whose PCR rides a kept CEA-708/WebVTT
  subtitle PID now converts with the builder-default PCR (first video)
  instead of failing `validate()` with `SubtitlePidUsedAsPcrPid`.

### Added — `tstrans.mpegts.DataStreamSpec` (Python)

- New frozen dataclass in the `StreamSpec` hierarchy (`pid`, `stream_type`,
  `carries_pts`) — Python introspection of Data specs:
  `MuxerProgramConfig.streams` returns one per data stream, and
  `MuxerConfig.from_program_map` reconstructs one per unknown-type stream.
  PMT descriptors are not carried on the spec — they live on
  `MuxerProgramConfig.stream_descriptors`.

### Added — `tstrans.io.transmux` demux→edit→remux bridge (Python)

- New pure-Python context manager `tio.transmux(src, dst, *, drop=(),
  atomic=False)`: iterate a source file's demux events and write back the
  ones to keep — video/audio copied byte-for-byte via raw encoded AUs,
  KLV substitutable via `tx.write_klv(ev, new_bytes)` (pairs with
  `klv.patch_uas_datalink` for byte-faithful tag edits). The output muxer
  is constructed lazily from the first `ProgramMap` via
  `MuxerConfig.from_program_map`, reproducing the source program topology;
  `atomic=True` routes through the `write_file` temp-file + `os.replace`
  machinery. v1 scope: single-program sources; strict on unrepresentable
  streams with `drop=` opt-out. Completes the v0.2.0 transmux arc.

### Added — cross-binding video raw AU (C + JVM)

- **tst-c**: `TstEventSample.payload` / `payload_len` are now populated for
  Video samples with the exact encoded access unit (Annex-B for H.26x;
  on-wire PES payload for AV1) — they were null for video before. No
  struct or ABI change (the fields existed; ABI minor stays 11). The
  per-NAL/OBU `payload` pointers now point into the single raw-AU arena
  copy when the split units are subslices of the AU (always for H.26x;
  AV1 binding-mode falls back to per-unit copies). Pointer validity is
  unchanged: arena-owned, valid until the next `_recv_event` / `_close`.
- **JVM**: `DemuxEvent.Video` gains a `ByteBuffer raw` component — a heap
  (JVM-owned) copy of the exact encoded access unit, mirroring tst-py's
  `.raw`; feed it to `Muxer.pushVideo` for byte-faithful transmux. The
  record's canonical constructor changed (component order:
  `stream, pts, dts, codec, payload, raw, randomAccessIndicator,
  codecParseError`) — pre-1.0 breakage policy. `payload`
  (typed `List<VideoUnit>`) is unchanged.

### Changed (breaking) — `DemuxEvent::Sample` video/audio payloads are now raw-first

- The demuxer no longer eagerly parses video / audio elementary streams.
  `SamplePayload::Video` drops its `payload: VideoPayload` field and now carries
  **`raw: SharedBytes`** — the exact encoded access unit (Annex-B for H.26x;
  on-wire PES payload for AV1). `SamplePayload::Audio` is structurally unchanged
  — `frames` still carries the raw audio bytes; the opt-in parse path is new. In
  Python, `DemuxEvent.Video` / `DemuxEvent.Audio` drop the eager `payload`
  (NAL/OBU/frame lists) and `codec_parse_error` fields — use `.raw` + `.parse()`
  (or `tstrans.codec.split_units` / `parse_audio`) instead.
- **Parsing is opt-in.** Split a raw video AU with
  `tst_core::mpegts::demux::split_video(&raw, codec)` (returns
  `(VideoPayload, Vec<NonConformantIssue>)`) or `split_video_strict` (returns
  `Err` on the first ES-conformance issue). In Python, call `ev.parse()` on a
  `DemuxEvent.Video` / `DemuxEvent.Audio` event, or the free functions
  `tstrans.codec.split_units(raw, codec)` / `tstrans.codec.parse_audio(raw, codec)`.
- `NalUnit` / `Obu` payload bytes are now `tst_core::shared::SharedBytes`
  (refcounted zero-copy views; they still deref to `&[u8]`).
- The demuxer's `StrictMode` is now **TS-layer only** — it gates PSI / PES /
  timing conformance and no longer inspects or rejects video/audio ES content.
  Malformed-NAL/OBU rejection moved to the opt-in `split_video_strict`.
- The JVM binding still returns parsed `List<VideoUnit>` via `v.payload()`
  (it splits internally); the C `TstNal[]`/`TstObu[]` surface is likewise
  unchanged. Both additionally expose the raw AU since Wave 5 — see
  "cross-binding video raw AU" above.

### Fixed — `Muxer.write_file` drain contract documented + overflow hint (Python)

- Investigated the corrector-notebook `MuxError: muxer packet buffer is full`
  failure inside `Muxer.write_file(...)`: the drain proxy is correct — only
  pushes made on the proxy object the `with` statement yields drain per push;
  pushes on the original `Muxer` bypass the sink entirely and overflow once
  `buffer_packets` accumulate. Docstrings (`write_file`, `MuxerFileSink`,
  `MuxerDrainProxy`) and the Python language guide now state the
  push-on-the-proxy contract explicitly.
- The Python `MuxError(BACKPRESSURE)` raised on a full packet buffer now
  appends a hint pointing at the `write_file` proxy contract — the message a
  user actually sees when they hit the footgun mid-loop.
- New regression tests: a >10 000-AU push loop through the proxy (drains per
  push, never overflows) and the raw-muxer footgun itself (overflows, carries
  the hint).

### Added — Python ergonomics for the metadata-edit workflow

- `with_(**changes)` on the four frozen typed KLV sets (`UasDatalinkLs`,
  `SecurityLs`, `PrecisionTimeStampPack`, `VmtiLs`) — copy-update without
  `dataclasses.replace` boilerplate; construction-time validation re-runs.
- `tstrans.io.iter_uas_datalink(path, *, strict=False, config=None)` — typed
  ST 0601 iterator yielding `(pts, klv_index, UasDatalinkLs)`; `klv_index`
  counts every KLV event in file order so indices line up with a re-mux pass.
- `DemuxEvent.Klv.parse(*, strict=False)` — decode-on-event sugar mirroring
  `Video.parse()` / `Audio.parse()`; dispatches by universal label.
  `tstrans.klv.parse_klv_universal` now accepts `strict=`.
- Runtime `inspect.signature` on the `Muxer` / `Demuxer` / builder
  constructors is regression-locked (PyO3 already auto-generates
  `__text_signature__` from `#[new]` — no Rust changes were needed).

### Added — ST 0601 byte-faithful tag patcher

- `tst_core::klv::st0601::patch` + `tstrans.klv.patch_uas_datalink`: byte-faithful
  tag-level patching of raw ST 0601 local sets — edited tags re-encoded in place,
  every other TLV copied verbatim, checksum recomputed (new `KlvPatchError`).

### Added — `MuxerConfig::from_program_map` + `ProgramMap.pmt_pid`

Transmux bridge: convert a demuxed `ProgramMap` directly into a `MuxerConfig`
that reproduces the source program's topology — program number, PMT PID, stream
PIDs, codecs, and audio language — without hand-coding the builder. The
demux-kind ↔ mux-stream-type mapping that previously existed only as prose is
now in code, and documented as a table in the mux guide.

- **`tst_core::mpegts::mux::MuxerConfig::from_program_map(&pm, drop)`** — new
  constructor (Rust). Maps each demuxed `StreamKind` to its closest muxer
  equivalent; `drop: &[StreamKindTag]` lets callers exclude unrepresentable
  stream kinds (DVB sub/teletext; unknown stream types) rather than erroring.
  `carries_pts` is always `true` (PMT cannot declare it; STANAG 4609 norm).
  PCR PID is copied iff the demuxed `pcr_pid` matches a kept non-KLV stream.
  Audio language recovered from ISO 639 descriptor (tag `0x0A`) on
  `StreamInfo::raw_descriptors` when present and plausible (lowercase
  ISO 639-2 code; anything else falls back to language-less audio).
- **`tstrans.mpegts.MuxerConfig.from_program_map(pm, drop=None)`** — same
  constructor exposed as a Python `@staticmethod`. `drop` takes a list of
  `StreamKindTag` members. Raises `MuxError` (kind `CONFIG_INVALID`) for
  unrepresentable streams not excluded via `drop`; raises `ValueError` for
  malformed input.
- **`tst_core::mpegts::demux::ProgramMap::pmt_pid`** — new field on the core
  `ProgramMap` struct. The PAT-declared PID carrying this program's PMT;
  previously not surfaced on the demux event.
- **Python `tstrans.mpegts.ProgramMap.pmt_pid`** — same field surfaced on the
  Python `ProgramMap` frozen dataclass.
- **Python `tstrans.mpegts.StreamInfo.raw_descriptors`** — new field: tuple of
  `RawDescriptor(tag: int, data: bytes)` carrying the raw PMT per-stream
  descriptor TLVs in PMT loop order. New `RawDescriptor` type exposed.
- **`tst_core::mpegts::demux::StreamKindTag`** — new payload-free enum
  (`Video`, `Audio`, `Subtitle`, `KlvSync`, `KlvAsync`, `Unknown`);
  `StreamKind::tag()` returns the matching tag. Drives the `drop` filter in
  `from_program_map` without carrying the full `StreamKind` payload.
- Docs: new "Rebuilding a muxer config from a demuxed program" section in
  [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md#rebuilding-a-muxer-config-from-a-demuxed-program)
  with the complete stream-type mapping table (Rust + Python snippets).

### Changed (breaking, pre-1.0) — `ProgramMap` and `DemuxEvent.ProgramMap` gain new fields

- **C ABI minor 10 → 11**: `TstEventProgramMap`
  gains `uint16_t pmt_pid` immediately after `pcr_pid` (offset +4), consuming
  the first two bytes of the former `_pad[4]` (now `_pad[2]`) — the struct
  layout is unchanged (size, member alignment, and all prior fields are
  identical); existing binaries continue to work but a recompile picks up the
  new field. No symbols added or removed; the `TST_ABI_VERSION_MINOR`
  constant in `tstrans.h` bumped from `10` to `11`.
- **JVM `DemuxEvent.ProgramMap` record** gains `pmtPid` as the third record
  component (`int pmtPid` after `programNumber` and `pcrPid`). Callers
  constructing the record by positional arguments or using the generated
  `canonical constructor` must add the new argument. Pattern matches using
  named record accessors are unaffected.
- **Rust `ProgramMap` struct** gains `pmt_pid: u16`. Struct literal
  construction (`ProgramMap { program_number, pcr_pid, streams, klv_links }`)
  must add `pmt_pid`; field-update syntax (`..other`) is unaffected.

### Added — opt-in elementary-stream parsing surface

- `tst_core::shared::SharedBytes` — refcounted zero-copy byte buffer.
- `tst_core::mpegts::demux::split_video` / `split_video_strict` — opt-in split
  of a raw video AU into a `VideoPayload` (`Nals` / `Obus`).
- Python: `tstrans.codec.split_units` / `tstrans.codec.parse_audio`;
  `DemuxEvent.Video` / `DemuxEvent.Audio` gain `.raw` + `.parse()`.
- `push_video_to_with_dts(dts=None)` produces a PTS-only PES (equivalent to
  `push_video_to`).
- Python: type stubs (`.pyi`) for the core `tstrans.io`, `tstrans.codec`,
  `tstrans.klv`, and `tstrans.mpegts` modules, giving IDE autocomplete and
  `mypy` checking for the demux/mux/KLV/codec surface (previously only the
  transport submodules shipped stubs). The stubs reflect the raw-first sample
  model: `DemuxEvent.Video`/`.Audio` expose `.raw` + `.parse()`, and
  `codec.split_units`/`codec.parse_audio` are typed. A `mypy stubtest` CI
  ratchet keeps the stubs in sync with the runtime.

### Removed — macOS Intel (x86_64) Python wheel

- The best-effort `x86_64-apple-darwin` wheel build is gone: GitHub's
  `macos-13` Intel runners are scarce, and the queued leg held up the PyPI
  publish job (whose `needs:` waits for every matrix leg). Intel-Mac users
  install from the sdist, which builds from source. See
  `docs/project/deferred-features.md` ("macOS x86_64 (Intel)") for the
  revisit trigger.

---

## [0.1.0] — 2026-06-08

### tst-c-core split + offline muxer un-gated (2026-05-31)

#### Changed — tst-c ABI minor 8 → 9: offline `tst_muxer_*` un-gated (2026-05-31)

- tst-c ABI minor 8 → 9: the offline `tst_muxer_*` surface is now
  unconditional (previously gated behind the `srt` feature), matching the
  already-unconditional `tst_demuxer_*`. Additive — no symbol removed, no
  signature changed; SRT builds are unaffected, non-SRT builds gain the
  offline muxer.
- The C-ABI logic was split into a new `tst-c-core` rlib (embeddable,
  no_std-capable) re-exported by the `tst-c` cdylib/staticlib leaf. Consumer
  artifacts are unchanged: `cargo build -p tst-c` still emits
  `libtstrans.so` + `libtstrans.a` + `tstrans.h` exactly as before. A
  default-on `std` feature gates every transport-bearing surface
  (srt/rtp/udp/tcp/hls/rist); the offline core (muxer / demuxer / config /
  error / handle / panic) now compiles `#![no_std]`+`alloc` under
  `--no-default-features`, enabling the bare-metal embedded path.

---

### tst-py Phase 8: `tstrans.srt` — full SRT surface (2026-05-27)

#### Added — tst-py bindings for UDP / TCP / HLS / RIST (Plan A5b, 2026-05-27)

Four new Python submodules on the `tstrans` extension, all behind cargo
features default-**ON** (matching `rtp`/`srt`; PyPI wheels ship everything).
No C ABI change — this is tst-py-only (`tst-py` wraps the Rust crates
directly, not through `tst-c`).

- `tstrans.udp` — `Transport` (`send`) + `RecvTransport` (`recv`) + builders
  + `SocketStats`. Compose with `tstrans.mpegts` for muxing/demuxing.
- `tstrans.tcp` — dual-role `Transport` (`send`+`recv` on one handle) +
  `Listener` (`accept_blocking`) + `TransportBuilder`/`ListenerBuilder` +
  forward-compat `TlsConfig`/`ClientCert` (the `tcp` feature builds without
  TLS; `tcps://` raises `TcpError(TLS_DISABLED)`).
- `tstrans.hls` — `HlsPublisher` (builder → internal tokio HTTP server,
  `push_ts`/`cut_segment`/`finish`/`stats`/`hls_stats`/`local_addr`/
  `render_playlist`) + `MuxPublisher` (push video/KLV/audio/subtitle →
  HLS, `finish_into_publisher`) + a `Publisher` ABC mirroring the Rust
  `tst_core::publisher::Publisher` trait + `HlsMode`/`HlsStats`. KLV stays
  in-band in the `.ts` segments.
- `tstrans.rist` — `Transport`/`RecvTransport` + builders + `EncryptionKey`
  (AES-128/192/256 PSK; the secret is wrapped at the FFI boundary and never
  re-exposed) + `RistProfile` (SIMPLE/MAIN) + `RistStats`.
- 4 new exception classes (`UdpError`/`TcpError`/`HlsError`/`RistError`) +
  `*ErrorKind` IntEnums in `tstrans.exceptions`, raised Rust-side via the
  import-based `make_<x>_error` helpers (not `create_exception!`).
- 5 new bash ratchets (`check-py-{udp,tcp,hls,rist}-error-mapping-coverage.sh`
  + `check-py-publisher-class-mirror.sh`); 4 new `*.pyi` stubs (mypy
  `--strict` clean); ~80 new pytest tests.
- `tst-udp` gains an additive `recv_timeout(&mut buf, Duration)` (SO_RCVTIMEO)
  for the Python `RecvTransport.recv(timeout_ms=...)`; `tst-udp/public-api.txt`
  baseline updated. No other upstream crate API change.
- `python-wheels.yml` maturin feature list extended to
  `rtp,srt,udp,tcp,hls,rist` (workflow stays parked behind tst-jni).
- **SRT + RIST in one process:** the default wheel links libsrt + librist
  (two static mbedTLS copies); `bindings/python/build.rs` adds
  `-Wl,--allow-multiple-definition` (Linux, scoped srt&&rist) to collapse
  onto one mbedTLS — same fix as `tst-c`. Verified link + runtime.

#### Added — tst-c bindings for UDP / TCP / HLS / RIST (Plan A5a, 2026-05-27)

Four new cargo features on `tst-c`: `udp`, `tcp`, `hls`, `rist` — all
**default-OFF** (keeps embedded `libtstrans.so` size unchanged for existing
SRT-only / RTP-only consumers). Build the new transports explicitly, e.g.
`cargo build -p tst-c --features udp,tcp,hls,rist`.

- **~137 new C entry points**, each transport mirroring the RTP per-handle
  data-path surface (open / close / send_ts | recv_ts / push_{video,klv,audio,
  subtitle}[_to] / next_event / get_stats / get_socket_stats / reset_stats),
  **minus cancel** (these transports expose no `cancel_handle()`):
  - `tst_udp_*` — 34 (sender / receiver / mux_sender / demux_receiver).
  - `tst_tcp_*` — 39 (same four families + a `tst_tcp_listener_*` accept
    surface; the single `TcpTransport` impls both send + recv).
  - `tst_hls_publisher_*` / `tst_publisher_*` / `tst_mux_publisher_*` — 30:
    the HLS publisher (builder + concrete handle running an internal tokio
    HTTP server) + the abstract `Publisher` trait projection (`TstPublisher`,
    enum-dispatched) + the new `MuxPublisher<P>` shell. KLV stays in-band in
    the `.ts` segments.
  - `tst_rist_*` — 34 (RIST Simple + Main profiles via librist; AES-128/192/256
    encryption + profile + buffer carried in URL query params).
- 4 new `TST_HAS_UDP` / `TST_HAS_TCP` / `TST_HAS_HLS` / `TST_HAS_RIST`
  compile-time defines (always emitted; 0 when the feature is off).
- 18 new `TstError` codes (`TST_E_UDP_IO` … `TST_E_RIST_IO`, -26..-43) +
  the `TstPublisherKind` enum.
- 5 new bash ratchets (count 26 → 31): `check-{udp,tcp,hls,rist}-error-mapping-coverage.sh`
  + `check-publisher-trait-mirror.sh`.
- 8 new C examples (2 per protocol) + 4 new smoke-test integration tests.

#### Changed

- `tst-c` ABI version bumped: `TST_ABI_VERSION_MINOR` 6 → 7 (additive; no
  breaking C-side change — existing consumers do not need to rebuild).
- **Known limitation — SRT + RIST in one binary:** libsrt (`vendor/mbedtls`)
  and librist (its own `contrib/mbedtls`) each statically link mbedTLS, so a
  build with both the `srt` and `rist` features (e.g. `--all-features`) has
  duplicate `mbedtls_*` symbols. The cdylib build resolves this on Linux with
  `-Wl,--allow-multiple-definition` (collapses onto libsrt's copy; verified
  link + runtime). A clean single-mbedTLS build (cross-crate reuse) is a
  follow-up. Most consumers enable a single transport and are unaffected.

#### Added — new transport: RIST (Plan A4, 2026-05-27)

- **New crates `rist-sys` + `tst-rist`** — VideoLAN librist 0.2.10 bindings
  for the RIST (Reliable Internet Stream Transport) protocol. Mirrors the
  srt-sys + tst-srt two-crate split.
  - `rist-sys` is the raw FFI layer; bindings generated by bindgen from
    `librist/librist.h`. Vendored librist build driven by **meson + ninja**
    (librist is a meson project, not cmake — diverges from srt-sys's cmake
    workflow). `mbedtls` cargo feature (default-on) bundles librist's
    `contrib/mbedtls` for AES-128/192/256 PSK encryption.
  - `tst-rist` exposes safe wrappers `RistTransport` (sender) and
    `RistRecvTransport` (receiver) implementing `tst_core::transport::
    Transport` and `RecvTransport` respectively. Builders + URL parser
    follow the same pattern as `tst-srt` / `tst-rtp` / `tst-tcp` / `tst-udp`.
  - Supports RIST Simple Profile (VSF TR-06-1) and Main Profile (TR-06-2);
    Advanced / SRP-auth deferred via `#[non_exhaustive]`.
  - URL schemes: `rist://host:port` (sender), `rist://@host:port` (receiver
    bind, ffmpeg convention). Query params: `profile`, `bandwidth`,
    `buffer`, `aes-type`, `secret`, `cname`, `recovery_maxbitrate`,
    `session_timeout`, `compression`.
  - **Caveat:** Simple Profile receivers require an EVEN port (librist
    uses `port + port+1` for RTP + RTCP). Main Profile is unconstrained.
- New CI prerequisites on Linux: `apt install meson ninja-build` (in
  addition to the existing build-essential / cmake / pkg-config).
- New env var `RIST_FORCE_VENDORED=1` (mirror of `SRT_FORCE_VENDORED=1`)
  to skip the pkg-config probe and always use the vendored librist build.
- Cookbook recipes: 9d ([sending/rist.md](docs/cookbook/sending/rist.md)) +
  4c ([receiving/rist.md](docs/cookbook/receiving/rist.md)).
- Examples: `cargo run -p tst-examples --example send_rist` and `recv_rist`.

#### Added — second transport binding in Python (after RTP in Phase 4 Stage 2)

- New crate `tst-tcp` — raw MPEG-TS over TCP with optional TLS (rustls 0.23).
  Single `TcpTransport` impls both `Transport` and `RecvTransport`, supporting
  all 4 caller/listener × send/recv combinations. URL schemes: `tcp://`,
  `tcps://`.

- **New `tstrans.srt` submodule** behind cargo feature `srt = ["dep:tst-srt"]`,
  default-on. Published wheels always include SRT; source builds that don't
  need it can opt out via `maturin develop --no-default-features` for a
  smaller binary that drops the libsrt + mbedTLS link. No `[srt]` pyproject
  extra is added — there are no Python-side runtime deps to install.
- **18 PyClasses** spanning the full live-transport surface, organized into
  five layers that mirror the existing `tst-srt` Rust API:
  - Low-level construction (T3): `Builder`, `Socket`, `Listener`.
  - Raw byte transport (T2): `Sender`, `Receiver`, `SocketStats`, `SrtStats`,
    `CancelHandle`.
  - Mux/demux convenience (T5): `MuxSender`, `DemuxReceiver`.
  - Reconnect policy (T6): `ReconnectPolicy`, `BackoffStrategy`,
    `OverflowPolicy`.
  - Auto-reconnect wrappers (T7-T8): `ManagedSender`, `ManagedReceiver`,
    `ManagedMuxSender`, `ManagedDemuxReceiver`.
- **Exception classes**: `SrtError` + `SrtErrorKind` IntEnum-shaped PyClass
  in `tstrans.exceptions`, following the established kind-enum pattern
  (`MuxError` / `DemuxError` / `KlvError` / `RtspError` / `RtpError`).
  Kind variants: `CONFIG_INVALID`, `CONNECT_FAILED`, `TIMEOUT`, `CLOSED`,
  `BROKEN`, `IO`, `INTERNAL`.

#### Per-wave breakdown

- **T1 — bootstrap.** New `bindings/python/Cargo.toml` `srt` feature gated on
  `tst-srt`. Empty `bindings/python/python/tstrans/srt.py` shim with the
  conditional `_native.srt` import + friendly `ImportError` when the
  feature is off. `tstrans.exceptions.{SrtError, SrtErrorKind}` exception
  hierarchy + stub bash ratchet
  `scripts/check-py-srt-error-mapping-coverage.sh`.
- **T2 — transport layer.** `Sender(url)` + `Receiver(url)` PyClasses
  wrapping `tst_srt::SrtTransport` (which implements both `Transport` and
  `RecvTransport` — there is **no** separate `SrtRecvTransport` type, see
  drifts below). `SocketStats` mirrors the cross-transport abstract stats;
  `SrtStats` adds the libsrt-rich fields (`mbps_estimated_bandwidth`,
  `bytes_lost_send_side` / `bytes_lost_recv_side` symmetric byte-loss
  split, etc.). `CancelHandle` cancels parked recvs from another thread.
  **Cross-crate plumbing**: `Sender::transport` + `Receiver::transport`
  accessors added to `tst_pipeline` so the SRT-specific stats can be
  reached through the pipeline shells; `SrtTransport::stats` exposed as
  a top-level method on the Rust side.
- **T3 — low-level primitives.** Hybrid `Builder(url, *, kwargs...)`
  with fluent setters for all 12 SRT knobs. Q4-A URL precedence is
  preserved: URL values WIN over kwargs and setters, because the
  `UrlOverlay::apply_to_socket` unconditional overwrite runs **after**
  the kwarg-built config. `Socket` with `into_sender` / `into_receiver`
  / `into_mux_sender` / `into_demux_receiver` consume-and-move promotion.
  `Listener` with `accept(timeout_ms=...)`, iterator shape (yields
  `Socket` until `cancel_handle().cancel()` triggers `StopIteration`),
  `local_addr()` port readback.
- **T4 — errors.** Full `SrtError` mapping across every `tst-srt` error
  type (`ConnectError`, `AcceptError`, `IoError`, `BuildError`, etc.)
  with typed source preserved via PEP 3134 `from`. Real ratchet check
  (`scripts/check-py-srt-error-mapping-coverage.sh`) replaces the T1
  stub: every `SrtErrorKind` variant must have at least one literal
  `make_srt_error(py, "<VARIANT>", ...)` call site under
  `bindings/python/src/`.
- **T5 — convenience wrappers.** `MuxSender.from_url(url, program_config)`
  + `DemuxReceiver.from_url(url, *, demux_config=None)`. Same 16-method
  push family as `tstrans.rtp.MuxSender` and `tstrans.mpegts.Muxer`
  (`push_video` / `push_klv` / `push_audio` / `push_subtitle` + `_to`
  variants + 4 handle getters + `stats` + `close` + context manager).
  `DemuxReceiver` iterates `tstrans.mpegts.DemuxEvent` subclasses (no
  duplicated event type hierarchy across transports).
  `Socket.into_mux_sender` / `Socket.into_demux_receiver` finalizes the
  T3 promotion path (these were `NotImplementedError` stubs in T3 until
  T5 landed).
- **T6 — reconnect policy.** `ReconnectPolicy(max_attempts=..., backoff=
  BackoffStrategy.exponential(...), gap_buffer_capacity=..., overflow_policy
  =OverflowPolicy.DROP_OLDEST)`. `BackoffStrategy.{constant, exponential}`
  classmethod factories; `OverflowPolicy.{DROP_OLDEST, REJECT}` IntEnum-shaped
  PyClass. Defaults mirror `tst_pipeline::ReconnectPolicy::default()`:
  `max_attempts=10`, exponential `100ms..=10s`, `gap_buffer_capacity=256`,
  `DROP_OLDEST`. Constructor raises `ValueError` if `gap_buffer_capacity == 0`.
- **T7 — managed basic.** `ManagedSender.from_url(url, *, policy=None)` +
  `ManagedReceiver.from_url(url, *, policy=None)` wrapping
  `tst_pipeline::ManagedTransport<SrtTransport>` /
  `ManagedRecvTransport<SrtTransport>`. Expose `send_bytes` / `recv_bytes`
  on top of the underlying transport (no mux/demux on this layer).
  `reconnect_attempts()` reads 0 before any break — the initial bind+accept
  and initial connect do NOT count as reconnects.
- **T8 — managed convenience.** `ManagedMuxSender.from_url(url, program,
  *, policy=None)` + `ManagedDemuxReceiver.from_url(url, *, policy=None,
  demux_config=None)`. Same 16-method push family as T5's `MuxSender`,
  plus reconnect ergonomics. `DemuxEvent.ReconnectDiscontinuity` surfaces
  on the consumer iterator after the managed layer heals a break.
- **T9 — type stubs.** New `python/tstrans/srt.pyi` — 18 stub classes
  (matching the 18 PyClasses), `mypy --strict` clean. Continues the
  existing `py.typed` discipline established in Phase 4 Stage 2.
- **T10 — integration tests + README + CHANGELOG.** New
  `bindings/python/tests/test_srt_integration.py` with 4 end-to-end tests
  spanning multiple PyClasses (full Builder→Socket→MuxSender/DemuxReceiver
  pipeline, MuxSender/DemuxReceiver.from_url shortcut path, encrypted
  loopback with `passphrase`, ManagedSender+ManagedReceiver round-trip
  with `ReconnectPolicy`). README extended with a SRT section mirroring
  the RTP section structure. CHANGELOG entry (this entry).

#### Notable design decisions

- **Hybrid fluent + kwargs `Builder`.** All 12 SRT knobs are accepted as
  both `__init__` kwargs and as chainable setter methods. URL-provided
  values always win because the `UrlOverlay::apply_to_socket` unconditional
  overwrite runs after the kwarg-built config. This is the same precedence
  as the existing `tst-srt::Builder` Rust API. Decision Q4-A in the spec.
- **`secrecy::SecretString` passphrase wrapping.** The `passphrase` kwarg
  and the `.passphrase(...)` setter both accept a Python `str` but immediately
  wrap it in a `secrecy::SecretString` on the Rust side. The original Python
  `str` is never re-exposed via getters; `Builder.__repr__` redacts to
  `<redacted>` (validated by `test_srt_builder.py::test_passphrase_setter_redacts_in_repr`).
- **Q4-A URL precedence.** URL > setter > kwarg, in that order. Tests pin
  this in `test_srt_builder.py` (`test_url_wins_over_kwarg_passphrase` etc.).
- **16-method push family parity with `tstrans.rtp.MuxSender` and
  `tstrans.mpegts.Muxer`.** Single-stream `push_{video,klv,audio,subtitle}`
  + multi-stream `_to` variants + 4 handle getters + `stats` + `flush` /
  `close` / context manager. Keyword-only `pts` per the plan #96 Wave C
  normalization.
- **4 `Managed*` wrappers built on top of T6's `ReconnectPolicy`.**
  `ManagedSender` + `ManagedReceiver` for raw bytes (T7); `ManagedMuxSender`
  + `ManagedDemuxReceiver` for mux/demux + auto-reconnect (T8). All four
  consume the same `ReconnectPolicy` PyClass instance, so users can build
  a single policy and reuse it across all four roles.
- **No low-level `Socket.send_bytes` / `Socket.recv_bytes`.** Decision Q3-B
  in the spec — `Socket` is a promotion-only handle; bytes work goes through
  `into_sender()` / `into_receiver()` which produce the typed `Sender` /
  `Receiver` wrappers.

#### Drifts caught + resolved during execution

- **`SrtTransport` implements both `Transport` and `RecvTransport`.**
  The plan assumed a separate `SrtRecvTransport` type would exist (by
  analogy with `RtpTransport` / `RtpRecvTransport` in `tst-rtp`). Reality:
  `tst-srt` uses a single `SrtTransport` type that implements both traits.
  T2 wraps it twice — once as `Sender` (pushing via `Transport::send_bytes`),
  once as `Receiver` (pulling via `RecvTransport::recv_bytes`) — with no
  shared inner pyclass.
- **`OverflowPolicy` has 2 variants, not 3.** The spec sketched
  `DROP_OLDEST` + `DROP_NEWEST` + `BLOCK`; the real Rust enum
  `tst_pipeline::reconnect::gap_buffer::OverflowPolicy` only ships
  `DropOldest` (default) and `Reject`. T6 mirrors what Rust ships.
  Documented in the `OverflowPolicy` stub docstring + the spec was
  amended.
- **`ManagedSender.srt_stats()` is not directly achievable.** The
  `tst_pipeline::ManagedTransport` wrapper hides the inner
  `SrtTransport` behind a `Box<dyn Transport>` — there's no downcast
  path back to the SRT-specific stats. T7 surfaces this as
  `SrtError(IO)` when called; the user can re-enter the transport
  layer via `Sender.from_url` directly when they need
  `mbps_estimated_bandwidth` etc.
- **`ManagedDemuxReceiver.srt_stats()` returns `SocketStats`, not
  `SrtStats`.** Same root cause as above — the managed layer projects
  through the abstract `SocketStats`. Users who need the SRT-rich
  fields read them via the `Receiver.from_url` direct path.
- **`DemuxEvent.Metadata` is `DemuxEvent.Klv`.** The plan referenced
  `DemuxEvent.Metadata` in test pseudo-code; the actual Python class
  attribute is `DemuxEvent.Klv` (matching `MetadataKind::Klv*` in Rust).
  Caught + corrected during T10 integration test authoring. Note: the
  existing `test_srt_mux_demux.py::test_push_klv_via_loopback` has a
  pre-existing latent bug referencing `DemuxEvent.Metadata` — the
  isinstance check passes on the Video branch first so the bug is
  masked. Flagged for a follow-up fix outside this commit.

#### Numbers

- **pytest**: 818 → 928 (+110 across the phase: Wave A T2/T3/T4 added
  ~56 tests, Wave B T5/T6 added ~31, Wave C T7/T8 added ~18, T10
  integration added 5 tests + 1 skipped).
- **Bash ratchets**: 25 → 26 (new
  `scripts/check-py-srt-error-mapping-coverage.sh`, mirroring the
  existing per-error-kind ratchet pattern).
- **`#[non_exhaustive]` BASELINE**: stays at 198. Phase 8 adds new
  PyClasses and PyEnums but none are marked `#[non_exhaustive]` (the
  pattern is reserved for cross-binding-stable enums; PyO3 classes
  go through their own ABI mechanism).
- **`cargo-public-api` baselines**: no Rust-side public-API drift in
  the 4 ratcheted crates. Phase 8 only adds Python bindings on top of
  the existing `tst-srt` + `tst-pipeline` surfaces. (Verify with `cargo
  public-api -p tst-srt --simplified` before push.)

#### References

- Spec: `docs/specs/2026-05-27-tst-py-srt-design.md` (approved 2026-05-27,
  10 design decisions Q1-Q10).
- Plan: `docs/plans/2026-05-27-tst-py-srt.md` (~2800 lines, 11 tasks,
  4 waves of parallel subagents).
- Memory anchor: `project_phase_8_tst_py_srt_planned.md` (transitions to
  `project_phase_8_tst_py_srt_shipped.md` after this commit).

#### Phase 7 unblock

Phase 7 (PyPI wheels CI) was paused at false-start before Phase 8 —
the drafted `.github/workflows/python-wheels.yml` sits uncommitted on
disk with `--features rtp` baked into the maturin invocation. T11 Step 8
picks it up: sed `--features rtp` → `--features rtp,srt` in the workflow
and commits as Phase 8 closeout, so v0.1.0 PyPI wheels ship both
transports out of the box.

---

### tst-tcp HLS publisher with HTTP server (2026-05-27)

#### Added

- New `tst_core::publisher::Publisher` trait (sibling to `Transport`/`RecvTransport`) for
  outbound-only, segment-aware sinks. First impl: `HlsPublisher` in `tst-tcp`.
- New `tst_pipeline::MuxPublisher<P>` shell, mirroring `MuxSender<T>` but for the
  `Publisher` trait. `send_video(..., key_frame=true)` auto-calls `cut_segment()`.
- `tst-tcp`: new `hls` cargo feature (default-on). HLS publisher with built-in
  hyper 1.x HTTP server, LIVE/EVENT/VOD modes, rolling segmenter, Basic auth
  (RFC 7617), HTTPS via tokio-rustls 0.26. `hls://host:port` and `hlss://...`
  URL schemes. KLV stays inside the .ts segments.

---

### tst-udp: raw MPEG-TS over UDP (2026-05-27)

#### Added

- New crate `tst-udp` — raw MPEG-TS over UDP unicast + multicast
  (IPv4 + IPv6). `UdpTransport` (sender) + `UdpRecvTransport` (receiver)
  implement the existing `tst_core::transport::{Transport, RecvTransport}`
  traits and slot into `MuxSender<T>` / `DemuxReceiver<T>` shells.
  ffmpeg-compatible URL semantics (`udp://host:port`, `udp://@group:port`,
  `udp://group:port?iface=eth0&ttl=8`).
- New shared module `tst_core::net::udp_socket` — bind helpers + multicast
  knob helpers, extracted from previously-private code in `tst-rtp` and
  reused by `tst-udp`. Non-breaking refactor for `tst-rtp` callers.
- New workspace dependency `socket2 0.5` for cross-platform socket knobs
  (TOS, RCVBUF, SNDBUF) on `tst-udp`.

---

### tst-rtp Phase 4 Stage 3 follow-up: RtspServer::stop FIN on write halves (2026-05-26)

#### Fixed

- **`RtspServer::stop` now explicitly shuts down each per-session TCP
  write half.** Prior to this fix, `stop()` only signaled per-session
  cancellation; the per-session task dropped only its `OwnedReadHalf` on
  exit, while the `OwnedWriteHalf` stayed open through lingering
  `Arc<AsyncMutex<OwnedWriteHalf>>` clones held by `state.sessions` and
  the fanout task. No FIN reached the client, so client pumps blocked in
  `read()` until their own read timeout fired — and any in-Drop
  `RtspClient::teardown()` had to wait out the bounded 500 ms deadline
  before the pump's mpsc disconnected. After this fix, `stop()`'s final
  pass locks each `tcp_write` and calls `guard.shutdown().await` (bounded
  by 500 ms per session), sending FIN immediately. Client pumps observe
  EOF promptly; client teardown returns `Disconnected` instead of
  `TimedOut`.

  This closes the long-term root cause that Stage 3 T29's
  `teardown_with_deadline` sidestepped. The client-side deadline stays
  in place as defense-in-depth (peers that disappear without graceful
  shutdown still need it).

  Visible improvement: `tcp_interleaved_end_to_end_round_trips_ts_bytes`
  now runs in ~1.1 s wall (was 2.97 s with only the client-side fix in
  place).

#### Tests

- New regression test
  `rtsp_server_notice_5402::stop_shuts_down_per_session_write_half_so_client_sees_eof_promptly`
  pins the EOF-arrives-promptly behavior so any future regression
  (anyone removing the explicit shutdown loop in `RtspServer::stop`)
  trips immediately.

---

### tst-rtp Phase 4 Stage 3: TCP-aware RTCP ingest (2026-05-26)

#### Fixed

- **TCP-interleaved RTCP RR/SR now surface on `socket_stats()`.**
  Prior to this stage, peer RTCP arriving on the RTSP control channel
  (RFC 7826 §14 channel 1) was demuxed by the client interleaved pump but
  the receiver side of the mpsc just held the frames — `socket_stats().rtt_us`
  and `socket_stats().packets_lost_send` stayed at 0 regardless of peer
  feedback. `RtspSession::into_recv_transport` now calls a new
  `RtpRecvTransport::from_mpsc_with_rtcp` constructor which spawns a
  `rtsp-rtcp-ingest` std::thread that drains the RTCP mpsc, parses each
  frame via `SenderReport::decode` / `ReceiverReport::decode`, and feeds
  the shared `Arc<Mutex<RtcpStats>>` via the existing `ingest_rr` /
  `ingest_sr` free functions. `RtpRecvTransport::socket_stats()` projects
  `RtcpStats.cumulative_lost_send` into `packets_lost_send` and
  `RtcpStats.rtt_us` into `rtt_us` (UDP paths still project zeros — UDP
  RTCP ingest remains deferred).
- **`RtcpStats` gains three fields** (additive — pre-1.0): `cumulative_lost_send`,
  `rtt_us`, `last_sr_anchor`. `ingest_sr` now persists the anchor on stats
  (still returns it for callers); `ingest_rr` now also writes
  `cumulative_lost_send` (clamped to >=0) and calls `compute_rtt_us` against
  the stored anchor.
- **`RtspClient::Drop` no longer hangs after `RtspServer::stop`.** Root cause:
  after `server.stop()`, the server's per-session task exits and drops its
  read half, but the write half stays open via lingering `Arc` clones held
  by `state.sessions` and the fanout task — no FIN reaches the client. The
  client's pump keeps timing out on reads, and the client's in-Drop
  `teardown()` writes TEARDOWN into the kernel send buffer (succeeds, TCP
  isn't half-closed yet) then waits forever for a response that the
  defunct per-session task will never send. Fix: bound the in-Drop
  teardown via a new `teardown_with_deadline(Option<Instant>)` that
  short-circuits with `std::io::ErrorKind::TimedOut`. Drop uses a 500 ms
  deadline. The wire bytes still go out; we just stop waiting on the
  half-closed peer. Long-term, the server-side root cause (lingering
  write-half Arcs after graceful shutdown) deserves an explicit
  `shutdown().await` on each per-session write half in `RtspServer::stop`
  — tracked separately.
- **`tcp_interleaved_end_to_end_round_trips_ts_bytes`** un-ignored.
  Previously `#[ignore]`'d in plan #100 Wave H follow-up after T4's
  worktree reported pass but the merged state hung in the drop sequence.
  Now passes in ~3 s.

#### Added

- **`RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx)`** — `pub(crate)`
  constructor used by the TCP-interleaved RTSP client path. Spawns the
  `rtsp-rtcp-ingest` thread; thread exits cleanly when the pump's
  `Sender<Bytes>` drops.
- **`RtspClient::teardown_with_deadline(Option<Instant>)`** — `pub(crate)`
  bounded variant of `teardown()`. Wire bytes still go out; only the
  response wait is bounded.
- **`RtspClient::send_and_read_via_pump_with_deadline`** — `pub(crate)`
  underlying primitive supporting the bounded-teardown path.

#### Tests

- Three new transport unit tests verify the ingest pipeline end-to-end:
  `rr_on_rtcp_rx_populates_packets_lost_send` (RR → cumulative_lost_send →
  socket_stats.packets_lost_send), `sr_then_rr_populates_rtt_us`
  (SR-establishes-anchor → RR-with-matching-last_sr → rtt_us > 0), and
  `malformed_rr_increments_parse_error_counter` (truncated PT=201 →
  rr_parse_errors increment, no panic).

#### Public API

- `RtcpStats` field additions are additive — no removals; pre-1.0
  policy. cargo-public-api baseline regen needed for tst-rtp.

---

### tst-py Phase 4 Stage 2: `tstrans.rtp.*` (2026-05-26)

#### Added — first transport bindings in Python

- **New `tstrans.rtp` submodule** behind cargo feature `rtp = ["dep:tst-rtp"]`,
  default-on. Wheels published to PyPI always include RTP; source builds via
  `maturin develop --no-default-features` omit it. No `[rtp]` pyproject extra
  is added because RTP has no Python-side runtime deps to install via pip.
- **RTP transport surface** (T20): `Sender(url, *, pkt_size, ssrc)`,
  `Receiver(url, *, pkt_size)`, `SocketStats` (16-field frozen dataclass
  mirroring `tst_core::transport::SocketStats`), `CancelHandle` with
  `Arc`-shared cross-thread cancellation. `.send()` / `.recv()` release the
  GIL via `py.allow_threads`; `.stats()` / `.cancel_handle()` do not.
  Bytes-like inputs (`bytes` / `bytearray` / `memoryview` / NumPy `uint8`)
  via the audit-#10 two-path extraction pattern.
- **RTSP client surface** (T21): `RtspClient.connect(config) -> RtspSession`
  running OPTIONS/DESCRIBE/SETUP/PLAY in one call. `RtspSession.{play,
  pause, teardown, into_demux_receiver, cancel_handle, stats}`. Auth via
  `BasicAuth(user, password)` / `DigestAuth(user, password, algorithm)`
  PyClass dataclasses with `secrecy::SecretString` boundary wrapping (the
  Rust `RtspClientBuilder::auth` gets a `SecretString`; passwords never
  re-exposed to Python via getters, redacted in `__repr__`). Transport
  preference `TransportPref.{AUTO, UDP, TCP}` and `RtspVersion.{V1_0, V2_0}`
  IntEnum-shaped PyClasses. `RtspClientConfig` dataclass with
  `tls_root_certs_pem` passthrough (full TLS wiring deferred to a later
  task — bytes survive a round-trip, current `rtsps://` builds use the
  platform native trust roots).
- **RTSP server surface** (T22): `RtspServer.start(config) -> RtspServer`,
  `add_unicast_mount(path, program_config) -> MountHandle`,
  `add_multicast_mount(path, group, *, ttl, iface, program_config)`,
  `stats()`, `stop(*, drain_ms)` (Notice 5402 + graceful drain),
  `cancel_handle()`, context-manager protocol. `MountHandle` exposes the
  full 16-method push family (4 single-stream + 4 `_to` variants + 4
  handle getters + `stats` + `flush` + `reset_stats`) matching the Rust
  `tst_rtp::MountHandle` and the existing `tstrans.mpegts.Muxer.push_*`
  shape (keyword-only `pts` per plan #96 Wave C normalization). All push
  methods + `start` + `stop` release the GIL.
- **Convenience wrappers** (T23): `MuxSender(url, program_config, *, pkt_size)`
  wraps `tst_pipeline::MuxSender<RtpTransport>` for one-call construction;
  `DemuxReceiver(url, *, demux_config)` wraps
  `tst_pipeline::DemuxReceiver<RtpRecvTransport>` with `__iter__` / `__next__`
  emitting the existing `tstrans.mpegts.DemuxEvent` subclass hierarchy.
  `RtspSession.into_demux_receiver()` (previously a `NotImplementedError`
  stub from T21) is now wired through `RtspSession::into_recv_transport()`
  to return a real `DemuxReceiver`.
- **Type stubs** (T24): `python/tstrans/rtp.pyi` — 650 lines, 31 stub
  classes, `mypy --strict` clean. Continues the existing `py.typed`
  discipline.
- **2 new bash ratchets** (count 23 → 25):
  - `scripts/check-py-rtsp-error-mapping-coverage.sh` — every
    `RtspErrorKind` variant has at least one literal
    `make_rtsp_error(py, "<VARIANT>", ...)` call site in `bindings/python/src/`.
  - `scripts/check-py-rtp-error-mapping-coverage.sh` — same for
    `RtpErrorKind` (3 variants).
- **`RtspError`/`RtspErrorKind` + `RtpError`/`RtpErrorKind`** exception
  classes in `tstrans.exceptions`. Follow the established kind-enum
  pattern (`MuxError` / `DemuxError` / `KlvError`).
- **End-to-end integration tests** (T25): two new tests in
  `bindings/python/tests/test_rtp_integration.py` — a MuxSender ↔ DemuxReceiver
  loopback round-trip (data plane only) and a full RtspServer → RtspClient
  pipeline test that exercises the T23 `into_demux_receiver` bridge over
  UDP loopback. Both pass in ~1.6s wall on local hardware.
- **README updates**: top-level `README.md` Python row + `bindings/python/README.md`
  new "RTP + RTSP transport" section with 10-line client + server snippets.

#### Added — tst-rtp internal (T27, partial Stage 3)

- `RtspSession::activate_interleaved_pump` now returns the RTCP mpsc
  receiver alongside the RTP data receiver instead of black-holing RTCP
  via a `rtsp-rtcp-drain` background thread. New `rtcp_rx` field on
  `RtspSession` holds the receiver; `into_recv_transport` drops it for
  now (T28 will wire it into `RtcpReporterHandle` so TCP-interleaved
  RTCP RR/SR is observable on `socket_stats()`). No public-API delta —
  `activate_interleaved_pump` is `pub(crate)`. No C ABI delta.

#### Stats

- pytest: 813 passed, 6 skipped, 67 deselected — up from 707 + 2 at
  Stage 2 start.
- non_exhaustive BASELINE 193 → 198 (5 new Wave A/B PyClass enum-shaped
  types: `PyTransportPref`, `PyRtspVersion`, `PyDigestAlgorithm`, plus
  PyMuxSender + PyDemuxReceiver propagation).

#### Deferred — Stage 3 (T28-T30)

- T28: feed `rtcp_rx` into `RtcpReporterHandle` so socket stats report
  RTCP RR/SR-derived loss + RTT on the TCP-interleaved path.
- T29: un-ignore `tcp_interleaved_end_to_end_round_trips_ts_bytes` and
  debug the post-PLAY drop-sequence hang.
- T30: new RR-derived-loss test + CHANGELOG closeout.

---

### tst-c Phase 4 Stage 1: RTP + RTSP C ABI surface (2026-05-26)

#### Added — full Phase 4 Stage 1 C ABI

- **Cargo feature flags** `srt` + `rtp` on `tst-c` (both default-on); `mbedtls`
  retained. `--no-default-features` builds a file-I/O-only `tstrans` library.
  `tstrans.h` emits `TST_HAS_SRT` + `TST_HAS_RTP` `#define`s (always present;
  set to `1` when the matching feature is active, `0` otherwise) so consumer
  C code can `#if TST_HAS_RTP` without prior knowledge of build flavor.
- **~97 new C entry points** across 4 RTP handle families + RTSP client + server:
    - **RTP handles** (4 concrete opaque types: `TstRtpSender`,
      `TstRtpReceiver`, `TstRtpMuxSender`, `TstRtpDemuxReceiver`):
      `_open` + `_close` + data-path methods (send_ts / recv_ts / push_video
      / push_klv / push_audio / push_subtitle / push_*_to / next_event /
      get_*_stats / cancel / reset_stats / flush) — ~46 entry points total.
    - **RTSP client** (14 entry points): `tst_rtsp_client_builder_*` chain
      (`new`, `transport_pref`, `rtcp`, `keepalive`, `tls_root_cert_pem`,
      `auth_basic` / `auth_digest_md5` / `auth_digest_sha256`, `connect`,
      `free`) + `tst_rtsp_session_*` (`play`, `pause`, `teardown_and_free`,
      `cancel`, `into_demux_receiver` — bridges to `TstRtpDemuxReceiver`).
    - **RTSP server** (~37 entry points): `tst_rtsp_server_builder_*` chain
      (11: `new`, `bind`, auth × 3, `max_sessions`, `session_timeout`,
      `fanout_capacity`, `graceful_shutdown_drain_ms`, `tls_cert_pem`,
      `free`) + `tst_rtsp_server_builder_start` + `_add_unicast_mount` +
      `_add_multicast_mount` + `TstRtspMountHandle` push family
      (16: 4 single-stream + 4 `_to` variants + flush + cancel +
      reset_stats + 4 handle getters + get_stats) +
      `tst_rtsp_server_get_stats` + cancel-handle family + `_stop`
      (Notice 5402 + graceful drain) + `_free`.
- **11 new error codes** as `TstError` enum variants
  (`-15`..`-25`): `RtpTransport`, `RtspProtocol`, `RtspAuthFailed`,
  `RtspAuthRequired`, `RtspNotFound`, `RtspUnsupported`, `RtspTls`,
  `RtspIo`, `RtspTimeout`, `RtspServer`, `RtspMount`. Emitted as
  `TST_E_*` defines in `tstrans.h`.
- **4 new C examples**: `sending/rtp_basic.c`, `receiving/rtp_recv_basic.c`,
  `receiving/rtsp_client_camera.c`, `sending/rtsp_server_publish.c` —
  rich teaching comments per `feedback_examples_are_teaching_code.md`.
- **3 new bash ratchets**: `check-c-header-conditional-sections.sh` (cbindgen
  feature-gating regression guard), `check-rtsp-error-mapping-coverage.sh`
  (RtspError / MountError / RtspServerError → TstError variant coverage).
- **Upstream additions to `tst-rtp`**: `MountHandle::flush` + `reset_stats`
  surfaced (needed for C-side mount lifecycle parity); `RtspServerBuilder::with_url`
  added so tst-c can pass a pre-parsed `RtspUrl` from its accumulator builder.

#### Changed — tst-c ABI minor bump 5 → 6

`TST_ABI_VERSION_MINOR` 5 → 6 covering all Stage 1 additions. All changes
are additive (no breaking renames on existing symbols); existing consumers
continue to link unchanged. Per `docs/reference/binding-authors.md` policy,
any new C entry point triggers an ABI minor bump.

---

### tst-rtp Phase 3 Wave H follow-up (2026-05-26)

#### Fixed — Phase 3 Wave H deferrals closed (4 of 5)

- **Client-side `spawn_client_pump` wire-up into `RtspClient` (T4).**
  Activated at SETUP via `RtspClient::activate_interleaved_pump`: spawns
  the pump thread reading the TCP stream, demuxes `$`-framed RTP/RTCP
  binary into per-channel mpsc receivers, and parses RTSP responses out
  of the same byte stream into a control-message channel. Subsequent
  `send_and_read` requests poll the pump's `ctrl_rx` (matching by CSeq;
  keepalive responses with `CSeq >= 1_000_000` are silently discarded by
  the main thread). `RtspSession::into_recv_transport` now hands the
  pump's `data_rx` to `RtpRecvTransport::from_mpsc_placeholder` —
  TCP-interleaved transport now feeds the downstream
  `DemuxReceiver<RtpRecvTransport>` end-to-end. **RTCP routing is
  stubbed** for now: the pump pushes RTCP frames to a tiny named drain
  thread (`rtsp-rtcp-drain`) that discards them; a TODO marks the
  Phase 4+ landing site for TCP-aware RTCP ingest.
- **TCP-interleaved fanout on the server's per-session task (T1).**
  `handle_connection_inner` now splits the per-session TCP via
  `TcpStream::into_split()` at session start; the resulting
  `OwnedWriteHalf` is wrapped in `Arc<tokio::sync::Mutex<…>>` and stored
  on `ServerSessionState::tcp_write`. `handle_play`'s `TcpInterleaved`
  branch now clones that `Arc` + the SETUP-allocated `(rtp_channel, _)`
  pair and builds a `PeerTransport::Interleaved { writer, rtp_channel }`
  for `spawn_peer_fanout`. RTP frames flow over the interleaved channel.
  The per-frame mutex lock is fine for the RTSP-rate control plane vs
  the video-rate fanout traffic — both share the same write half via
  the same mutex.
- **RTSP Notice 5402 wire delivery in `RtspServer::stop()` (T3).** Per
  RFC 7826 §13.5.1, the server now sends an `ANNOUNCE rtsp://…/<mount>`
  request with `Notice: 5402 "Server-Initiated TEARDOWN"` over each
  active session's TCP write half before firing the per-session
  `CancellationToken`. Uses the same `Arc<Mutex<OwnedWriteHalf>>` that
  T1 introduced; serializes against any in-flight fanout frames so
  ANNOUNCE bytes can never interleave mid-binary-frame. Per-session
  write is bounded by a 1-second `tokio::time::timeout` to defend
  against a wedged peer holding the kernel send buffer.
- **IPv6 multicast iface binding by name (T2).** The `?iface=…` query
  parameter on a `rtp://[group]:port` URL now treats the value as an
  interface NAME (e.g. `"eth0"`, `"lo"`) on the IPv6 path, resolves it
  to a kernel ifindex via `libc::if_nametoindex(3)` (Unix-only,
  `#[cfg(unix)]`-gated), and applies the resulting 4-byte index to
  `IPV6_MULTICAST_IF` via raw `libc::setsockopt`. Previously the IPv6
  iface path rejected all input with "not implemented in v1". IPv4 path
  is unchanged (still takes an IP literal — `IP_MULTICAST_IF`'s wire
  shape).

#### Fixed — incomplete hotfix from `848f0b5f`

- **Windows clippy `unused_variable: iface_str` in
  `server/multicast.rs`.** The hotfix in `848f0b5f` gated the inner
  `iface_ip` parse under `#[cfg(unix)]` but left the outer `if let
  Some(iface_str) = iface` binding with no consumer on Windows — both
  v4 and v6 arms reference `iface_str` only inside `#[cfg(unix)]`
  blocks, and the `#[cfg(not(unix))]` early-return branches used static
  strings. Fix: format `iface_str` into the `#[cfg(not(unix))]` detail
  message of both arms (also useful diagnostic info — "requested iface
  'eth0'" vs the generic message).

#### Tests

- 3 new `#[cfg(unix)]` unit tests in `server/multicast.rs` covering
  `if_nametoindex("lo")` resolution success, bogus-name error path,
  and v6 IP-literal-as-iface-name error path.
- 2 newly-un-ignored integration tests:
  `rtsp_server_loopback_interleaved::client_setup_with_transport_tcp_round_trips_ts`
  and `rtsp_server_mixed_transports::udp_and_multicast_on_same_mount`.
- New integration test file `crates/tst-rtp/tests/rtsp_server_notice_5402.rs`
  (2 tests): drives a full OPTIONS → DESCRIBE → SETUP → PLAY handshake
  over a raw `TcpStream`, calls `stop()`, parses the ANNOUNCE bytes,
  asserts start-line + Notice header + reason phrase + round-trip
  Session header + mount path in URI.
- `rtsp_client_interleaved_e2e::tcp_interleaved_end_to_end_round_trips_ts_bytes`
  **stays `#[ignore]`** — the test was un-ignored briefly during the
  Wave H merge but surfaces a hang in the post-PLAY drop sequence (test
  runs >60s without returning). The wire-up infrastructure is verified
  by `rtsp_server_loopback_interleaved::client_setup_with_transport_tcp_round_trips_ts`
  (control-plane handshake + server-side fanout spawn) and
  `rtsp_server_notice_5402` (Notice 5402 wire delivery + cancel). The
  byte-level e2e assertion needs a follow-up debug pass to identify
  the interaction between T1's `Arc<Mutex<OwnedWriteHalf>>` + T4's
  pump thread + `Drop` ordering. Documented as a known issue inline in
  the test file.

#### Internal

- `RtspClient` and `RtspSession` lose the `Sync` auto-trait (they now
  hold `mpsc::Receiver<Bytes>`). `Send` retained. No `pub` items added;
  `cargo public-api` baseline regeneration absorbs the auto-trait
  delta only.
- `ServerSessionState` loses `UnwindSafe`/`RefUnwindSafe` (now holds
  `OwnedWriteHalf` + `tokio::sync::Mutex`, neither of which are
  unwind-safe). Pre-1.0 record-don't-block policy.
- `#[non_exhaustive]` BASELINE stays at 188 (no new variants).
- `cargo public-api` baselines for all 4 ratcheted crates (tst-core,
  tst-pipeline, tst-srt, tst-rtp) unchanged on net — T4's auto-trait
  flip on `RtspClient`/`RtspSession` already absorbed into the
  in-branch regen.
- `tst-c` ABI minor unchanged (no new C entry points; Phase 4 still
  lands the C ABI binding for the RTSP server).

#### Deferred — Phase 4

- **C ABI exposure of the RTSP server** (`tst_rtsp_server_*` symbols +
  `TST_HAS_RTP` define in cbindgen output) — the one remaining item
  from the original Wave H "5 deferrals" list. Phase 4 scope.

---

### tst-rtp Phase 3: RTSP server (2026-05-26)

#### Added — Phase 3 (RTSP server)

- `RtspServer` sync facade hiding an internal tokio Runtime — accepts
  client connections, manages sessions, fans out one Muxer's TS bytes
  to N connected peers. Public methods: `bind`, `bind_with`,
  `add_mount`, `add_multicast_mount`, `start`, `stop`, `local_addr`,
  `cancel_handle`, `stats`. Drop fires hard-cancel + `runtime.shutdown_timeout(5s)`.
- `RtspServerBuilder` chainable builder with `auth_basic`,
  `auth_digest_md5`, `auth_digest_sha256`, `max_sessions`,
  `session_timeout`, `fanout_capacity`, `graceful_shutdown_drain`,
  `tls_cert(cert_pem, key_pem)` (feature `tls`).
- `MountHandle` per-mount push surface mirroring `MuxSender::send_*`
  signatures: `push_video`, `push_klv`, `push_audio`, `push_subtitle`
  plus the `_to(handle, ...)` multi-stream variants and
  `video_handles` / `klv_handles` / `audio_handles` / `subtitle_handles`
  accessors. Internally: locks the inner Muxer, calls Muxer::push_*,
  drains TS bytes via `Muxer::pull` at 1316-byte chunks, broadcasts
  through a bounded `tokio::sync::broadcast` channel.
- `MountKind::Unicast` and `MountKind::Multicast { group, ttl, iface }`
  discriminant. Multicast mounts publish to a single shared UDP send
  socket fed by a per-mount sender task; the SDP advertises the
  group + per-client SETUP returns
  `Transport: RTP/AVP;multicast;destination=...;port=...;ttl=...`.
  TCP-interleaved against multicast → 461 per RFC 7826 §13.3.
- `MountStats` snapshot type (`bytes_pushed`, `packets_pushed`,
  `peer_count`, `frames_dropped_total`, `per_stream`).
- `ServerStats` aggregate snapshot type (`active_sessions`,
  `total_rtp_packets_sent`, `total_rtp_bytes_sent`, `mounts`).
- `RtspServerCancelHandle` — hard-cancel handle clone-able across threads.
- `RtspServerError` (10 `#[non_exhaustive]` variants), `MountError`
  (3 variants).
- `MulticastGroup::parse` + `RtspUrl::is_server_bind` /
  `validate_for_server_bind` helpers (URL validation entry points).
- Server-side TLS via tokio-rustls 0.26 (`feature = "tls"`); rustls
  `ServerConfig` loaded from PEM cert + key file paths at bind time.
- Server-side Basic + Digest (MD5 + SHA-256) auth acceptance —
  symmetric with Phase 2's client primitives (server reconstructs the
  same A1/A2 hash math + string-compares the `response=` attribute).
- Server-side `spawn_server_pump` primitive for TCP-interleaved
  transport — reads `$`-framed binary frames + RTSP requests off the
  client's TCP read half, demuxes by SETUP-allocated channels. Mirror
  of the client-side `spawn_client_pump` (Task 20).
- Graceful shutdown via `RtspServer::stop()` — flips a CancellationToken,
  iterates the active-session registry firing each per-session cancel,
  waits the configured drain window, then drops the runtime.
- `RtspClient::connect_with_roots(url, Option<RootCertStore>)` — new
  public method letting callers thread a custom rustls root store
  through to the TLS handshake (consumed by
  `RtspClientBuilder::tls_root_certs`).
- Rust example `examples/sending/rtsp_server_publish.rs` — chainable
  `RtspServerBuilder`, `add_mount`, `push_video` loop, graceful `stop()`.
- New bash ratchet `scripts/check-server-error-mapping-coverage.sh`
  (21st in `scripts/check-*.sh`) — verifies every `RtspServerError`
  variant is constructed somewhere under `rtsp/server/` or `builder.rs`.

#### Fixed — Phase 2 deferred items closed in Phase 3

- **Phase 2 deferred fix 2 (TLS-side keepalive):** `RtspClient::stream`
  refactored from raw `Stream` to `Arc<Mutex<Stream>>`. Phase 2's
  keepalive thread used `Stream::try_clone()` which silently failed for
  `rtsps://` because rustls `ClientConnection` isn't clonable. Now the
  main thread and keepalive thread lock the same Stream — TLS and
  plain TCP both work uniformly. `Stream::try_clone` removed.
- **Phase 2 deferred fix 1 (TCP-interleaved producer thread):** Server
  side via `spawn_server_pump` (`Task 19`) and client side via
  `spawn_client_pump` (`Task 20`) PRIMITIVES shipped. **The actual
  call-site wire-up of `spawn_client_pump` into `RtspClient::play()`
  remains deferred to a Wave H follow-up** — Phase 2's
  `RtpRecvTransport::from_mpsc_placeholder` still returns an unfed
  channel until that lands.

#### Fixed — bugs surfaced by Wave F integration tests

- Server `extract_mount_path` now strips trailing per-media control
  segments (`/trackID=N`, `/streamid=N`) so SETUP URIs built by
  `RtspClient::setup_mp2t_auto` from SDP `a=control:trackID=0` match
  the registered base mount path. Pre-fix, all client-to-server SETUP
  through `setup_mp2t_auto` returned 404.
- `RtspClientBuilder::auth(user, pass)` and `tls_root_certs(...)` are
  now actually applied to the constructed `RtspClient`. Pre-fix the
  builder stored both fields but `connect()` discarded them. Auth gets
  baked into `url.username/password` so the existing auth flow in
  `options_describe` picks them up; TLS roots threaded through to
  `TlsStream::connect` via the new `connect_with_roots` method.

#### Internal

- tokio + tokio-util promoted from dev-dep to production dep on `tst-rtp`
  for the `RtspServer` Runtime.
- tokio-rustls 0.26 added as feature-gated optional dep behind `tls`.
- rcgen 0.13 added as dev-dep for self-signed cert fixtures in the
  TLS integration tests.
- `time` crate pinned to 0.3.41 in `Cargo.lock` to keep MSRV 1.85
  (rcgen 0.13 pulled 0.3.47 which requires 1.88).
- `cargo public-api` baseline for `tst-rtp` grew ~590 lines across
  the wave (`MountHandle`, `RtspServer`, `RtspServerBuilder`,
  `ServerStats`, `MountStats`, `MountError`, `RtspServerError`,
  `MulticastGroup`, `RtspServerCancelHandle`, `MountKind`,
  `RtspClient::connect_with_roots`).
- `#[non_exhaustive]` BASELINE bumped 183 → 188.
- **`tst-c` ABI minor:** unchanged. No new C entry points; the C ABI
  bindings for the RTSP server land in Phase 4.

#### Deferred to Wave H follow-up / Phase 4

- ~~Client-side `spawn_client_pump` wire-up into `RtspClient::play()`~~
  **— Closed by Wave H T4 (2026-05-26).** See the Wave H entry above.
- ~~TCP-interleaved fanout on the server's per-session task~~ **— Closed
  by Wave H T1 (2026-05-26).**
- ~~RTSP Notice 5402 wire delivery in `RtspServer::stop()`~~ **— Closed
  by Wave H T3 (2026-05-26).**
- ~~IPv6 multicast interface binding by name~~ **— Closed by Wave H T2
  (2026-05-26).**
- C ABI exposure of the RTSP server (Phase 4 — `tst_rtsp_server_*`
  symbols + `TST_HAS_RTP` define in cbindgen output). **Remains open.**

---

### tst-rtp Phase 2: RTSP client + RTCP (2026-05-26)

#### Added

- `RtspClient` sync facade for `rtsp://` and `rtsps://` URLs with the
  full OPTIONS / DESCRIBE / SETUP / PLAY / PAUSE / TEARDOWN state
  machine (plus automatic `Drop`-driven TEARDOWN).
- `RtspClientBuilder` with builder-style options: `auth`,
  `no_auto_keepalive`, `keepalive_interval`, `connect_timeout`,
  `read_timeout`, `user_agent`, and `tls_root_certs` (feature `tls`).
- Authentication: RFC 7617 Basic + RFC 7616 Digest (MD5, SHA-256,
  MD5-sess, SHA-256-sess) + RFC 2617-flavored Digest for older
  cameras. 401-driven retry inside `describe()`.
- TCP-interleaved transport per RFC 7826 §14 (= RFC 2326 §10.12) with
  auto-fallback from UDP on `461 Unsupported Transport`. Explicit
  override via `?transport=tcp` / `?transport=udp` URL query params.
- `rtsps://` over sync rustls 0.23 behind cargo feature `tls` (ring
  backend; uses `rustls-native-certs` for the default root store).
- `?rtsp_version=1.0|2.0` URL query, default `1.0` for maximum camera
  interop (RFC 7826 §1.3 backward compatibility).
- Two SDP-picker APIs: `RtspClient::setup_mp2t_auto(&sdp)` picks the
  unique PT=33 m-line; `RtspClient::setup(&media)` is the explicit
  override.
- `RtspSession::into_recv_transport()` bridges into the existing
  `DemuxReceiver<RtpRecvTransport>` pipeline. UDP variant returns
  immediately; TCP-interleaved variant currently returns a placeholder
  transport (producer wiring deferred).
- RTCP RR/SR encode/decode (RFC 3550 §6.4) + `RtcpReporterHandle`
  background thread (randomized interval per §6.2/§6.3.1) + ingest
  (`compute_rtt_us` per §6.4.1 LSR/DLSR; RR fraction-lost feeds into
  `SocketStats::packets_lost_send`). New `RtcpStats` exposed via
  `RtpTransport::rtcp_stats()` and `RtpRecvTransport::rtcp_stats()`.
- Automatic keepalive thread sending OPTIONS at half the server's
  `Session:timeout=N` cadence (default 60 s → 30 s ping); opt-out via
  `RtspClientBuilder::no_auto_keepalive(true)`.
- New public types: `RtspClient`, `RtspClientBuilder`, `RtspSession`,
  `RtspCancelHandle`, `RtspError` (15 variants), `RtspMethod`,
  `RtspRequest`, `RtspResponse`, `RtspVersion`, `RtspScheme`,
  `RtspTransportPref`, `RtspTransportKind`, `TransportResponse`,
  `Sdp`, `SdpMedia`, `OptionsResponse`, `RtpInfo`, `Frame`,
  `InterleavedReader`, `InterleavedWriter`, `ReceiverReport`,
  `ReportBlock`, `SenderReport`, `SdesPacket`, `RtcpPacketType`,
  `RtcpReporterHandle`, `RtcpStats`, `SrAnchor`, `AuthChallenge`,
  `DigestAlgorithm`, `DigestChallenge`, `DigestContext`.
- Two new fuzz harnesses: `rtsp_message_decode` (RTSP wire-format) +
  `rtsp_interleaved_demux` (RFC 7826 §14 `$<ch><len><payload>`
  framing). Workspace fuzz count 19 → 21.
- Loopback RTSP server test fixture at
  `crates/tst-rtp/tests/fixtures/rtsp_loopback_server.rs` (tokio
  dev-dep). Supports `none`/`Basic`/`DigestMd5`/`DigestSha256` auth,
  configurable `force_461_on_udp`.
- Empirical interop matrix at `crates/tst-rtp/tests/INTEROP.md`
  (manual best-effort; 6 target rows).
- Teaching example at `examples/receiving/rtsp_client_camera.rs`.

#### Changed

- Phase 1's `RtpRecvSocketBuilder::build()` and `RtpSocketBuilder::build()`
  now open a second UDP socket for RTCP by default (RTP port + 1 per
  RFC 3550 §11). Opt-out via `.rtcp(false)` on either builder. This is
  a behavior change but additive on the public API.
- `RtspClient.stream` is now an internal `Stream` enum dispatching
  between plain `TcpStream` and `TlsStream` (gated on feature `tls`)
  rather than a bare `TcpStream`. Per-method code reads/writes through
  the `Read`/`Write` traits, transparent to callers.
- `RtpRecvTransport` now has an internal `Source` enum dispatching
  between UDP and mpsc (mpsc variant feeds the TCP-interleaved
  pipeline; producer wiring still deferred).

#### Phase 2 implementation notes

- 25 plan tasks shipped across 7 stages (Bootstrap + Waves A-F).
- Wave parallelism: up to 6 worktrees per wave; merge coordination
  done by main agent.
- `#[non_exhaustive]` baseline 172 → 183 (+11; within projection of
  +10..+15).
- `cargo-public-api` baseline `crates/tst-rtp/public-api.txt` grew
  from 597 to 1979 lines.

---

### tst-rtp Phase 1: RTP data plane (2026-05-25)

#### Added

- New crate `tst-rtp` ships the RTP-over-UDP data plane carrying MPEG-TS
  per RFC 2250 — sender + receiver, unicast + multicast (IPv4 + IPv6),
  behind the existing `tst_core::transport::{Transport, RecvTransport}`
  traits. `MuxSender<RtpTransport>` and `DemuxReceiver<RtpRecvTransport>`
  now work end-to-end on `rtp://host:port` URLs alongside the existing
  `srt://` flow.
- `RtpHeader` (RFC 3550 §5.1 fixed header), `RtpTransport`,
  `RtpRecvTransport`, `RtpStats` (protocol counters), `RtpUrl` (URL
  parser with `?ttl=`, `?iface=`, `?pkt_size=`, `?ssrc=` keys),
  `RtpSocketBuilder` + `RtpRecvSocketBuilder`. `tst_rtp::init()` is a
  no-op exposed for symmetry with `tst_srt::init()`.
- Pure Rust, no native deps. Direct dependencies: `getrandom` (SSRC +
  initial sequence-number randomization), `thiserror`, `tracing`, plus
  target-gated `libc` (cfg(unix)) for `setsockopt`-based multicast
  knobs that aren't yet stable on `std::net::UdpSocket` (Rust 1.85).
- New `cargo public-api` baseline at `crates/tst-rtp/public-api.txt`,
  bringing the ratchet to 4 crates (tst-core, tst-pipeline, tst-srt,
  tst-rtp). Fuzz harness `rtp_packet_decode` with 4 hand-built seeds;
  CI fuzz-smoke job compile-checks the new target.
- BASELINE `#[non_exhaustive]` count bumped 163 → 172 (observed at ship).
- Examples: `examples/sending/rtp_basic.rs`,
  `examples/receiving/rtp_recv_basic.rs`.

#### Scope

- **No RTSP yet** — Phase 2 lands `RtspClient` (DESCRIBE / SETUP / PLAY)
  and TCP-interleaved transport; the master design doc at
  `docs/specs/2026-05-25-tst-rtp-design.md` describes the 5-phase arc.
- **No RTCP yet** — `SocketStats.rtt_us` and `SocketStats.packets_lost_send`
  return 0 for `RtpTransport`; the RTP-specific
  `RtpRecvTransport::rtp_stats()` carries the malformed-packet counter
  (the only protocol-level signal Phase 1 surfaces).
- Timestamp source = system clock (`Instant::now()` → 90 kHz tick), M=0
  always. Per RFC 2250 §2, decoders use the inner PES PTS for content
  timing; the RTP timestamp is for jitter/RTCP and is not Phase 1's
  concern.
- Multicast send knobs (`IP_MULTICAST_TTL`, `IPV6_MULTICAST_HOPS`,
  `IP_MULTICAST_IF`) require `cfg(unix)` — the stable Rust 1.85 std
  `UdpSocket` doesn't expose them all yet. Windows MSVC target builds
  compile-and-link only (per plan #65); the multicast knobs return
  `ConnectError::IfaceUnsupported` on non-unix.

#### Compatibility

- No breaking change to `tst-core`, `tst-pipeline`, `tst-srt`, or
  `tst-c`. `tst-py` is unaffected (Phase 1 doesn't expose RTP to
  Python bindings — those land in Phase 4).
- Edition 2024, MSRV 1.85 (unchanged).

---

### tst-srt refactor groundwork (2026-05-25)

#### Refactor (no behavior change)
- Planned binding crates renamed: `srt-jni` → `tst-jni`,
  `srt-uniffi` → `tst-uniffi`. The crates remain PLANNED (not shipped);
  this is paperwork to align names with the `tst-*` workspace prefix
  ahead of adding RTP/RTSP transport.
- New `tst_core::url` module exposes scheme-neutral URL parsing helpers
  (`ParsedUrl`, `parse_url`, `parse_host_port`, `is_multicast_v4` /
  `_v6`) for shared use by `tst-srt`, the forthcoming `tst-rtp`, and
  downstream consumers. `tst-srt` no longer depends on the `url` crate.
- `SocketStats` + `TransportError::Backpressure`/`Broken.errno_code`
  rustdoc rewritten to be transport-neutral; the libsrt
  `CBytePerfMon` / `MJ_*` mapping now lives on `SrtTransport` itself.

#### Changed
- **Behavior change in `tst_srt::url::SrtUrl` URL parsing:** percent-encoding
  is now strict — malformed `%XY` (truncated or non-hex digits) returns
  `Err` rather than passing through, and percent-decoded bytes must be
  valid UTF-8. Additionally, `+` in query values is no longer decoded as
  space (the previous behavior came from the `url` crate's form-urlencoded
  semantics); literal `+` characters in passphrases now stay literal.
  Embed a `+` via percent-encoding (`%2B`) if it must be in a value.

  **Migration:** if you previously relied on `+` being decoded to a space
  in a URL query value (e.g., a passphrase containing a space), replace
  the `+` with `%20`. Values that already used `%2B` for a literal `+`
  are unaffected. Values without `+` are unaffected. Values with
  malformed percent-escapes that previously parsed successfully (rare)
  now return `UrlError::Syntax(BadPercentEncoding)`.

---

### docs/ framing pass (Phase 2 of polish) (2026-05-25)

Content + structure changes to the user-facing `docs/` surface. No code,
ABI, or API changes — BASELINE stays at 162, public-api baselines
unchanged, no ABI bump.

#### Docs

- **NEW: `docs/index.md`** — six-box landing routing 5 reader audiences
  (cold-domain reader, evaluator, language integrator, domain expert,
  binding author). Hero leads with reader outcome, not project mechanism.
  Names the four Diátaxis page types (Tutorials / How-to / Reference /
  Concepts) so readers learn the site's vocabulary.
- **NEW: `docs/start/overview.md`** — five-minute plain-English orientation:
  what ts-transformer streams, the three placements (source / middle /
  display), what's in the box, what's not.
- **NEW: `docs/start/concepts.md`** — cold-reader onramp explaining
  MPEG-TS / KLV / SRT vocabulary (PID, PES, AU, PAT/PMT/PCR/DTS/PTS,
  KLV/UL/BER) in plain terms with worked examples and a 22-row glossary.
- **NEW: `docs/languages/rust.md` and `docs/languages/c.md`** — per-language
  entry pages using an identical template (Install / Hello world / First
  send / First receive / Gotchas / Where this binding differs from the
  Rust core). Polyglot readers get cross-language muscle memory.
- **REFRESHED: `docs/languages/python.md`** — restructured to match the
  per-language template; pandas content demoted to a sub-section under
  "Language-specific gotchas".
- **REFRESHED: every `docs/guides/*.md` (6 files) and `docs/start/quickstart.md`** —
  each opens with a "Who this is for" line + a bulleted "You will learn"
  block (6–8 observable outcomes); first paragraph leads with a use case,
  not a type definition; every guide links to at least one runnable
  example in `tst-examples`. `quickstart.md` also re-framed to satisfy
  the 3-minute aha-time rule (theory + architecture moved to sibling
  reads).
- **SPLIT: `docs/cookbook.md` → `docs/cookbook/{sending,receiving,klv,codecs,operations}/*.md`** —
  33 recipes split into per-recipe grep-able files (11 sending, 10
  receiving incl. pairing, 4 klv, 3 codecs, 5 operations). New
  `docs/cookbook/index.md` catalogs them by section + by example program.
  Each recipe gets a "When to use this" header and "Related" cross-links
  to guides + runnable examples.
- **REFRESHED: README hero** — leads with reader outcome ("stream live
  H.264 / H.265 + KLV over an unreliable network in ~30 lines"), not
  project mechanism. "New here?" pointer to `docs/index.md`. Status
  block intact beneath the new hero.
- **Stale `docs/cookbook.md` references** in 12+ files (README, examples
  READMEs, source-comment refs in `crates/`, several intra-doc links)
  updated to the new `docs/cookbook/index.md` or specific recipe paths
  in the same commit that performed the split.

#### Process

- Phase 2 closes the docs polish initiated by spec
  `docs/specs/2026-05-24-docs-polish-design.md`. Both phases are
  reorganization + reframing; no code, public-API, or ABI changes.

---

### docs/ folder restructure (Phase 1 of polish) (2026-05-24)

**BREAKING (docs paths only).** Restructured `docs/` from a flat 18-file tree
into a folder hierarchy. No code, ABI, or API surface changes — BASELINE
stays at 162, public-api baselines unchanged, no ABI bump.

#### Docs

- `docs/getting-started.md` → `docs/start/quickstart.md`
- `docs/guide-*.md` → `docs/guides/*.md` (drops `guide-` prefix; 6 files)
- `docs/{guide-python,guide-python-pandas}.md` → `docs/languages/python.md`
  (mechanical merge; pandas content lives as a top-level section beneath
  the existing Python intro)
- `docs/{architecture,compatibility,conventions,public-api,binding-authors,srt-cancel-handle}.md`
  → `docs/reference/` (6 files)
- `docs/deferred-features.md` → `docs/project/deferred-features.md`
- `docs/troubleshooting.md` and `docs/cookbook.md` retain their paths this
  phase (cookbook splits in Phase 2; troubleshooting stays top-level by
  design — high-traffic, cross-audience)
- New empty `docs/cookbook/{sending,receiving,klv,codecs,operations}/`
  subfolder scaffold (`.gitkeep` placeholders) awaiting Phase 2 recipe
  split
- All in-repo references (README.md, rustdoc inside `crates/**/*.rs`,
  cbindgen-emitted `bindings/c/include/tstrans.h`, examples/READMEs,
  intra-`docs/` cross-links) updated to the new paths. Intra-`docs/`
  cross-links now use leading-slash absolute form (`/docs/...`) for
  GitHub-rendered portability from any nesting depth.
- External bookmarks to old paths will 404; no redirect shims (pre-1.0
  break-freely policy applies). Phase 2 will add the new landing page,
  per-language entry docs, and split the cookbook into per-recipe files.

---

### Phase 1 scoped test infrastructure (2026-05-25)

Tests-only / docs-only. No public API changes, no `#[non_exhaustive]` count
change (BASELINE stays at 162), no ABI bump.

#### Tests
- Closed 15 dead-weight Python skip sites in `tst-py` (10 `@pytest.mark.skipif`
  decorators + 5 runtime `pytest.skip` calls). Every gated fixture was already
  checked into `crates/tst-core/tests/fixtures/`; the skips were leftover
  defensive guards from when the fixtures were anticipated but not yet built.
  Module-level packaging assertions added so a fixture going missing in the
  future is a hard error, not a silent skip.
- Added tracked seed inputs for 7 previously-empty fuzz targets under
  `crates/*/fuzz/seeds/<target>/`: `demux_feed` (5), `demux_psi` (6,
  including real PAT/PMT extracted from `audio/aac-adts.ts`),
  `demux_pes_reassembly` (5), `mpegts_au_cell_read` (5),
  `klv_st0601_decode` (4), `parse_av1_sequence_header` (2), `url_parse` (7).
  New `scripts/seed-fuzz-corpora.sh` (idempotent) copies seeds into the
  gitignored `corpus/` tree before `cargo +nightly fuzz run`, preserving
  libFuzzer's accumulated runtime corpus. See
  `crates/tst-core/fuzz/seeds/README.md` for the convention.
- Wired `gen_pts_rollover_fixture` + `measure_pcr_jitter` into per-commit CI
  via `crates/tst-core/tests/timing_smoke.rs`. Both `[[bin]]` targets shipped
  for release-validation steps 8/9 (plan #83) had been unreachable from PR CI;
  a chained smoke test now exercises both via `CARGO_BIN_EXE_*`. The fixture
  straddles the 33-bit PTS boundary by ~3s on the post-wrap side and stays
  well under the jitter thresholds (median ≤ 67ms, p95 ≤ 100ms).

#### Docs
- Updated `docs/python-1/python-bindings-skip-backlog.md` — moved 15 closed
  rows to a "Closed by Phase 1" section. The 3 remaining runtime skips in
  `test_sample_payload_audio_fallback.py` require deterministic
  ADTS-syncword corruption fixtures and are deferred per plan
  `docs/plans/2026-05-25-scoped-test-infra-phase-1.md`.

---

### `cfi_tolerance` default flipped `false` → `true` (2026-05-24)

Behavior change: the `DemuxerConfig::cfi_tolerance` default flips from
`false` (strict-by-default per H.222.0 V9 §2.12.4.2 Table 2-157) to
`true` (tolerance-by-default — pragmatic for real-world STANAG 4609
traffic). The config knob and its semantics are unchanged; only the
default value moves. Callers who specifically need spec-strict
conformance behavior must now set `cfi_tolerance: false` explicitly.
This is recorded as a behavior change (not a hard BREAKING) because
the wire-format diagnostic (`NonConformantIssue::CfiTolerated`) still
fires under tolerance — producer malformation stays visible to
validators and telemetry; only the metadata-emission behavior
differs.

#### Rationale: this is an industry-wide encoder bug, not a vendor defect

Corpus-wide validation against the local 251-file / 37 GB STANAG 4609
local corpus (multiple platforms and captures) found ~99% of demuxer
`NonConformant` events under
tolerance mode are `MalformedAuCellCfiTolerated`. The producers ship
`cell_fragment_indication = 0b00` (Middle) on cells that are actually
single complete metadata Access Units (`0b11` Complete). Investigation
of the broader ecosystem confirmed this is industry-wide:

- **MISB ST 1402.2 Appendix B Table 2** lists the four CFI bit patterns
  (`'11', '10', '01' or '00'`) with **no semantic explanation** and
  misspells the field as `cell_fragmentation_indication` (the canonical
  H.222.0 name is `cell_fragment_indication`, no "ation"). ST 1402 has
  17 numbered normative requirements; **none mention CFI**.
- **FFmpeg `libavformat/mpegtsenc.c`** sets `stream_type=0x15` for
  `AV_PROFILE_KLVA_SYNC` but does **not** generate the 5-byte AU cell
  header itself — application code constructs it pre-mux. Zero CFI
  enforcement logic on the encode side.
- **GStreamer `gst-plugins-bad/.../tsdemux.c::parse_pes_metadata_frame`**
  reads the AU cell `flags` byte (which carries CFI in bits `[7:6]`)
  but **never inspects the CFI bits**, never reassembles multi-cell
  AUs, and never validates that single-cell AUs use `Complete`. Each
  cell becomes one output buffer regardless.
- **TSDuck, paretech/klvdata, jimcavoy/klvp, shacharmo/KlvOverMpegTSExtractor,
  Ghazanfar373/Video_klvdata_ffmpeg, tayre/klv-decoder**: no
  `cell_fragment_indication` references at all. GitHub-wide
  exact-token search for the canonical field name in C code returned
  zero hits outside `ts-transformer` itself.

Combine: encoder fields default-initialize to zero → CFI lands at
`0b00` (Middle) → no downstream decoder rejects it → producers ship
malformed streams indefinitely. The H.222.0 specification is
unambiguous on the bit table, but the implementer-facing MISB ST 1402
spec is silent on semantics and no public reference decoder enforces
the rule. Industry guides (ImpleoTV docs) consistently note that
"in the most common implementation, the packet payload consists of a
single metadata cell" — i.e., multi-cell fragmentation is essentially
unused, so every cell SHOULD be `Complete`, but encoders ship
`Middle` because of the default-zero pattern and nothing catches it.

`ts-transformer` is the only public reference decoder that enforces
CFI. Keeping strict-by-default would force every real-world consumer
to opt into tolerance to get their KLV — a UX trap with no upside,
since the `CfiTolerated` diagnostic still surfaces the malformation
under tolerance mode. Tolerance becomes the pragmatic default for
real-world traffic; strict mode remains available for conformance
testing of a producer against the wire spec.

#### Asymmetry with `lenient_psi_reassembly`

The sibling `lenient_psi_reassembly` knob still defaults `false`
(strict). The asymmetry is calibrated to corpus evidence — we have
empirical proof the CFI bug is dominant in real traffic (~99% of
events); we do not have equivalent evidence for PSI reassembly
violations. Per-knob defaults are set from empirical signal, not from
a uniform "strict-default" or "lenient-default" policy.

**Changed:**

- `tst_core::mpegts::demux::DemuxerConfig::cfi_tolerance`: default
  `false` → `true`. `#[derive(Default)]` replaced with a hand-written
  `impl Default` (a derived `Default` would give `bool` field a value
  of `false`). All other field defaults unchanged.
- `tst_core::mpegts::demux::DemuxerBuilder::new()` / `default()`:
  produces a tolerance-enabled demuxer. Tests exercising the strict
  orphan path must now call `.cfi_tolerance(false)` explicitly:
  `mpegts_au_cell_round_trip.rs::multi_cell_au_emits_non_conformant_issue_through_demuxer`
  + `mpegts_au_cell_tolerance.rs::strict_mode_orphan_middle_with_complete_klv_stays_orphan`
  updated to match.
- `tst_py::mpegts::DemuxerConfig.cfi_tolerance` dataclass default
  `False` → `True`. Docstring rewritten to lead with the new default
  and corpus-empirical rationale.
- `tst_py::io.parse_file` / `io.probe` / `io.extract_klv` docstring
  examples: `DemuxerConfig(cfi_tolerance=True)` (opt in) →
  `DemuxerConfig(cfi_tolerance=False)` (opt out for conformance).
- `tst_c::demux_config::tst_demux_config_new`: initialises
  `cfi_tolerance: true`. `tst_demux_config_set_cfi_tolerance` rustdoc
  rewritten to lead with default-true and the corpus-empirical
  rationale. Header regenerated; `tst-c::tests::header_drift`
  unchanged otherwise.
- `docs/guide-mpegts-demux.md` § "Malformed `cell_fragment_indication`
  tolerance (default on)" — rewritten from opt-in framing to
  default-on framing with industry-survey rationale.
- `docs/troubleshooting.md` — the "I see `MultiCellAu{Orphan}` events
  but zero typed KLV" entry now reads the inverse: default config
  already rescues these; check whether you've explicitly disabled
  tolerance. Added a new entry for users running conformance suites
  who want spec-strict CFI handling.

**ABI:** `TST_ABI_VERSION_MINOR` unchanged at 5. This is a
behavior-default change on an existing field, not an additive ABI
change — no new entry points, no new enum variants, no new error
codes, no struct-layout change. Per `docs/binding-authors.md`
policy, no minor bump is needed.

**Public API (`cargo public-api`):** no token-level change — default
values are not part of the public-api surface output. The 3 ratcheted
crates (tst-core / tst-pipeline / tst-srt) are unaffected.

**Tests:**

- `tst-core::default_cfi_tolerance_is_false` → `_is_true`; assertion
  flipped.
- `tst-c::cfi_tolerance_default_is_false` → `_is_true`; assertion
  flipped.
- `tst-py::test_demuxer_config_default_for_tolerance_is_false` →
  `_is_true`; `test_demuxer_config_accepts_tolerance_true` →
  `_accepts_tolerance_false`.
- `tst-py::test_extract_klv_strict_default_yields_zero_records_on_malformed`
  renamed to `_strict_config_yields_zero_records_on_malformed`; now
  passes `DemuxerConfig(cfi_tolerance=False)` explicitly. New test
  `test_extract_klv_default_yields_records_on_malformed` covers the
  new default's behavior.

**Cross-refs:**

- Corpus walk: outside-repo corpus-validation notebook
  (sensitive). 251 files / 37 GB, 0 parse errors,
  381,211 NonConformant events (~99% CFI tolerated).
- Industry survey memo (outside-repo; agent memory):
  `reference_cfi_industry_state.md`.
- ITU-T H.222.0 v9 (08/2023) Table 2-157 — canonical CFI bit mapping.
- MISB ST 1402.2 Appendix B Table 2 — the implementer-facing spec
  that omits CFI semantics.

---

### Plan #96 closeout-audit validation follow-ups (2026-05-25)

Static-validation review of the plan #96 closeout commits (see `docs/analysis/2026-05-25-closeout-fixes-validation.md`) surfaced 6 follow-up findings: 2 release-blocking (ABI minor not bumped despite new C entry points; stale `_close` lifecycle docs), 1 medium (subtitle config validation less strict than `DemuxerConfig`), 3 low (docstring + field-comment + test-helper-stability wording drifts). All 6 closed via 6-worktree parallel SDD; no Rust functional changes (Finding 6 was doc-only by design). BASELINE 162 unchanged. **C ABI minor bumped 4 → 5** per `docs/binding-authors.md` policy.

**Changed:**

- `TST_ABI_VERSION_MINOR` bumped **4 → 5**. The plan #96 Wave B commit (`dc923e4`) added 3 new C entry points (`tst_demux_config_set_av1_carriage`, `_set_au_cell_cap_per_pid`, `_set_lenient_psi_reassembly`) plus the `TstAv1CarriageMode` enum (cbindgen alias `tst_av1_carriage_mode`, macros `TST_AV1_CARRIAGE_MODE_MPEG2_TS_BINDING` / `_INTEROP_RAW_OBU`). Per `docs/binding-authors.md` policy, new C entry points trigger an additive minor bump; the v1 plan #96 changelog entry's "unchanged at 4 (no bump needed per project policy)" wording contradicted that policy and was corrected in the same commit that bumped the constant. Ratchet `scripts/check-doc-abi-and-st1910-currency.sh` regex extended to forbid `ABI version 0.[0-4]`.
- `docs/compatibility.md` row for `Lifecycle (_open / _close)`: dropped stale "`_close` is idempotent and NULL-safe; close-from-any-thread serializes through `Mutex<Option<...>>`" wording. Now matches the per-function rustdoc that Wave F established: NULL is a no-op; after a successful close the pointer is invalid and calling close again on the same non-null pointer is undefined behavior. Concurrent close-from-multiple-threads on the same live pointer is also UB.
- `tstrans.mpegts.DvbSubtitlingConfig.__post_init__` + `DvbTeletextConfig.__post_init__`: now reject `bool` for numeric fields (`bool` is a subclass of `int` in Python — silently passed through before) and tightened `language` from `bytes | bytearray` to `bytes` only (the dataclasses are `frozen=True, slots=True`; allowing mutable `bytearray` weakened the immutability/hashability story). Mirrors the stricter `DemuxerConfig.__post_init__` validation pattern shipped by Wave H.
- `tst_py::mux::Muxer::push_subtitle` docstring: corrected the misleading "Argument order follows the Rust API: `(payload, *, pts)`" claim. The Rust API is `push_subtitle(pts, payload)`; audit-2 #9 normalized the Python `push_*` family to a uniform `(payload, *, pts)` shape. The docstring now states this explicitly.
- `tst_core::io_file::TryDemuxFromFile::pending_error` field comment: now accurately describes the three-phase flow (stage error on read/feed failure → drain buffered events → emit staged error + set `done = true`). Previous comment incorrectly claimed `done = true` was set at the SAME time the error was staged.
- `tst_c::demux_config::test_build_options` rustdoc: added an explicit "Stability" note documenting that the `test_` prefix + `#[doc(hidden)]` follows the established convention in this crate (matches `lib.rs::test_clear_last_error` and siblings). The function is required to be `pub` because integration tests under `tests/` are separate crates that cannot see `pub(crate)` items.

**Tests added:**

- Python: 8 new tests in `bindings/python/tests/test_mpegts_muxer_push.py` covering bool-as-int rejection on all 6 numeric subtitle-config fields plus bytearray-as-bytes rejection on `language` for both `DvbSubtitlingConfig` and `DvbTeletextConfig`. pytest total: 708 (default; +8 vs plan #96 closeout's 698 → 700-ish + sibling diff). No Rust tests added (all other follow-ups are doc-only).

**`cargo public-api` drift:** none — all changes are doc comments, Python wrappers, docs, or const-value bumps. The 3 ratcheted crates (tst-core / tst-pipeline / tst-srt) are unaffected.

**Commits (in main order):**

- `2e527d1` — Fix #1: tst-c ABI minor 4 → 5
- `5671034` — Fix #2: docs/compatibility _close wording
- `4b38338` — Fix #3: tighten subtitle config validation
- `e32dd45` — Fix #4: push_subtitle docstring
- `34deca7` — Fix #5: TryDemuxFromFile::pending_error field comment
- `2f4a7f2` — Fix #6: test_build_options stability rustdoc

Validation report: `docs/analysis/2026-05-25-closeout-fixes-validation.md`.

---

### Core + C ABI + Python closeout audit fixes (2026-05-25)

Plan #96. 16 of 17 findings from `docs/analysis/2026-05-25-core-c-python-closeout-audit.md` closed (F13 invalidated during validation — no actual double-insert in `codec_parse_error_to_pyerr`'s `ReservedValue` arm). Shipped via 8-worktree parallel SDD (Waves A–H) + 1 sequential scaffold cleanup (Wave I) + 1 cross-wave fix + 1 H+B coordination follow-up. `#[non_exhaustive]` BASELINE stays at **162**; bash ratchet count goes from 14 to **20**.

**Added (cross-surface):**

- `tst_core::mpegts::demux::TryDemuxFromFile` — fallible streaming iterator with `Iterator<Item = io::Result<DemuxEvent>>`. Surfaces read errors AND `demuxer.feed()` errors as `Err(io::Error)` instead of collapsing them into `eof = true; return None` like `DemuxFromFile`. `try_demux_from_file_with_config(path, config) -> io::Result<TryDemuxFromFile>` is the constructor. The lossy `DemuxFromFile` keeps working but gains a doc note pointing at the fallible alternative. (Findings 6, 14)
- `tst_c::demux_config`: 3 new C setters bridging Rust-only knobs — `tst_demux_config_set_av1_carriage(cfg, mode)`, `tst_demux_config_set_au_cell_cap_per_pid(cfg, cap_bytes)`, `tst_demux_config_set_lenient_psi_reassembly(cfg, lenient)`. New `TstAv1CarriageMode` enum (mux side already had a mirror; demux side reuses it). (Finding 2)
- `tstrans.mpegts.DemuxerConfig`: 3 matching Python fields — `av1_carriage: Optional[Av1CarriageMode]`, `au_cell_cap_per_pid: Optional[int]`, `lenient_psi_reassembly: bool`. `build_demuxer()` rewritten to assemble `DemuxerConfig` directly (the Rust `DemuxerBuilder` has no setter for `lenient_psi_reassembly`). (Finding 2)
- `tstrans.mpegts`: 4 new mux-side subtitle codec dataclasses — `DvbSubtitlingConfig`, `DvbTeletextConfig`, `Cea708StandaloneConfig`, `WebVttInTsConfig` — mirroring the Rust `SubtitleCodec` struct variants exactly (`language: bytes` not `str` per Rust `[u8; 3]`; range validation in `__post_init__` for u16 page IDs, 5-bit teletext type, 3-bit magazine, BCD page numbers). New `MuxerProgramConfigBuilder.add_subtitle(codec_config)`; `push_subtitle` / `push_subtitle_to` now wrapped in `py.allow_threads` matching `push_video`'s GIL contract; signatures normalized to `(payload, *, pts)` per audit-2 #9 push_* shape. (Finding 3)
- `tst_core::mpegts::mux::{Video,Klv,Audio,Subtitle}StreamHandle::try_from_raw(raw: u32) -> Result<Self, MuxError>` — validating constructors that reject raw values with bits set outside the canonical 4-bit program + 4-bit within-program layout. New `handle_pack::try_unpack` underpins them. Existing `from_raw(raw: u32) -> Self` kept for in-process round-trip; doc'd with the trust-boundary caveat. (Finding 1)

**Changed (cross-surface):**

- `tst_c` ABI: every trust-boundary rewrap site now routes through `try_from_raw` and surfaces forged-high-bit handles as `TST_E_INVALID_USAGE` instead of silently aliasing a valid stream. **14 sites across 4 files** patched (`mux_sender.rs` ×4 + `managed_mux_sender.rs` ×4 + `muxer.rs` ×4 + `config/descriptors.rs` ×2) — the audit said 4; the actual surface was 14. (Finding 1)
- `tstrans.mpegts.<Kind>StreamHandle.from_raw(...)` (Python): same `try_from_raw` validation; forged-high-bit handles now raise `MuxError(INVALID_USAGE)` at construction instead of being accepted. (Finding 1)
- `tst_c` close/free safety wording: rewrote the 10 ambiguous "Idempotent — passing NULL is a no-op" comments. Only 1 of 10 (on `tst_muxer_close`) was actually wrong (the other 9 are on `_cancel` functions where "Idempotent" is correct). Added consistent "Safe to call with NULL. After this call the pointer is invalid; passing the same non-null pointer twice is undefined behavior." wording to that one plus 15 close/free functions that had no doc at all. (Finding 8)
- `docs/binding-authors.md` threading section: distinguished synchronized runtime handles (`Handle<T> = Mutex<...>`) from unsynchronized mutable config builders. Previous "Send + Sync everywhere" claim could mislead binding authors. (Finding 7)
- `tst_demux_config_free`: wrapped in `ffi_catch((), || { ... })` matching `tst_mux_config_free`. The 15th ratchet below would have caught this regression. (Finding 9)
- `tstrans.mpegts.DemuxerConfig.__post_init__`: fail-fast validation on all 7 fields (4 from Wave H, 3 from Wave B coordination follow-up). Invalid primitives now fail at construction instead of deep inside PyO3 extraction with opaque overflow errors. Excludes `bool` from `int` accepted-types since Python's `bool` is a subclass of `int`. (Finding 10)
- `tstrans.mpegts._drain_muxer_to_file`: replaced per-chunk `bytes(buf[:n])` allocation with a hoisted `memoryview` slice. Bumped `_DRAIN_CHUNK_PACKETS` from 4 to 7 to align with the 1316-byte SRT bundle size common in receivers. (Finding 11)
- `Demuxer` rustdoc per-language idiom table: C row no longer says "deferred to per-binding plan — receiver-surface C ABI is P0". Now correctly describes `tst_demux_receiver_recv_event(p, &out_event)` draining into arena-lifetime `tst_event_t` + `tst_demux_receiver_close(p)`. (Finding 15)
- `README.md` + `docs/binding-authors.md` + `docs/compatibility.md` + `bindings/c/src/lib.rs`: refreshed ABI version from stale 0.2 references to actual 0.4 in the generated header. Added history entries for the 2→3 (AU cell reassembly) and 3→4 (CFI tolerance) bumps to `binding-authors.md`. `compatibility.md` receiver-side stats + multi-program demux rows updated to "shipped" status (the audit said lines 460–482 contained the stale claim; actual stale text was at lines 463 + 466). (Finding 4)
- ST 1910 mis-cites scrubbed from MPEG-TS sync-metadata AU cell contexts. README lines 5/10/57/117 + `tst_py/src/mux.rs:1034` + `tests/test_mpegts_muxer_push.py:165` now use ST 1402 / H.222.0 §2.12.4.2 wording. ST 1910.1 kept correctly in `docs/compatibility.md:530` + `docs/deferred-features.md` CMAF/HLS context only. (Finding 5)
- Sweeping cleanup: 85 of 149 Phase N / Task N scaffold comments removed from public Rust + C ABI + Python wrapper sources (149 → 64). Remaining 64 are load-bearing ABI history entries, current-invariant comments, or test docstrings (out of authorized scope). Incidental: 3 stale `#[allow(dead_code)] // used in later Phase 3 tasks` attrs removed from `bindings/c/src/config/streams.rs::from_core` (those fns are heavily used today). (Finding 17)

**Added (CI ratchets, 6 new bash scripts — count goes from 14 to 20):**

- `scripts/check-doc-abi-and-st1910-currency.sh` — fails on `ABI version 0.[0-3]` in README/docs/crates, on bare `ST 1910` (not `ST 1910.1`) in `crates/` or `README.md`, and on "receiver-surface ... pending" / "demux event surface ... pending" in tst-c crate-level docs. (Finding 4 + 5 guard)
- `scripts/check-extern-c-ffi-catch-coverage.sh` — finds every `pub unsafe extern "C" fn` in tst-c (182 total); passes if body uses `ffi_catch(` OR `Handle::with_inner_{mut,ref}` (internally catches) OR is in a trivially-infallible allowlist (`tst_get_version_*`, `tst_get_abi_version_*`). 175 / 7 split today. (Finding 9 guard)
- `scripts/check-py-demux-error-mapping-coverage.sh` — extracts `DemuxError` variants (5) from Rust; verifies each appears in `demux_error_to_pyerr` before the wildcard arm. (Finding 12 guard)
- `scripts/check-py-klv-decode-error-mapping-coverage.sh` — same shape for `KlvDecodeError` (19 variants).
- `scripts/check-py-klv-encode-error-mapping-coverage.sh` — same shape for `KlvEncodeError` (8 variants); accepts the `as RustE` alias.
- `scripts/check-py-non-conformant-kind-coverage.sh` — extracts `NonConformantIssue` (33 variants) AND verifies all 32 distinct kind strings exist in the Python `NonConformantKind` enum.

All 4 Python-error ratchets use word-boundary matching (`grep -qE "Type::${v}\b"`); the existing `check-py-codec-error-mapping-coverage.sh` template uses plain substring grep — separate cleanup item.

**Removed:**

- `DemuxerConfig` "Other Rust-side knobs (link_klv, treat_as, av1_carriage, au_cell_cap_per_pid) are not yet bridged" warning. `link_klv` and `treat_as` remain Rust-only today (low demand); the other 3 are now bridged.

**Tests added (net):**

- Rust: ~13 new tests across `try_from_raw` (handle_pack), `TryDemuxFromFile` (io_file), AV1 demux config round-trip (tst-c integration), `tst_demux_config_set_*` parity (tst-c unit).
- C: 1 integration test for forged-handle rejection at C ABI boundary.
- Python: ~46 new tests across `test_handle_forge.py` (14), `test_demux_config_validation.py` (19), `test_demux_config_parity.py` (15), subtitle round-trip + GIL release + invalid-handle in `test_mpegts_muxer_push.py` (~14), memoryview file sink (4). pytest total: 691 → 698.

**`cargo public-api` drift:**

- `tst-core`: +14 lines (additive — `TryDemuxFromFile` + 2 fns + Iterator impl + marker traits).
- `tst-pipeline` / `tst-srt`: no drift.
- `tst-c` ABI minor: bumped **4 → 5**. Per `docs/binding-authors.md` minor-bump policy ("new C entry points" trigger an additive bump), the 3 new demux-config setters (`tst_demux_config_set_av1_carriage` + `_set_au_cell_cap_per_pid` + `_set_lenient_psi_reassembly`) plus the new `TstAv1CarriageMode` enum (cbindgen alias `tst_av1_carriage_mode`) require a minor bump. The original "unchanged at 4 (no bump needed)" wording in the v1 of this CHANGELOG entry contradicted policy and was corrected by this follow-up.

**Commits (in main order):**

- `8d4582d` — pre-wave: `klv::pack::Iter` tightened to `pub(crate)` (see prior section).
- `4c459e9` — pre-wave: `cargo fmt` cleanup that 8d4582d missed.
- `85962a8` — Wave G: 4 Python error-mapping ratchets.
- `1f72f8f` — Wave E: `TryDemuxFromFile`.
- `e6bebe4` — Wave A: forged handle rejection at trust boundaries.
- `dd34906` — Wave F: threading docs + close/free safety + 15th ratchet.
- `dc923e4` — Wave B: demux config parity (av1_carriage / au_cell_cap_per_pid / lenient_psi_reassembly).
- `26cf7e1` — Wave C: Python subtitle mux API.
- `059423e` — Wave H: DemuxerConfig fail-fast + memoryview file sink + NumPy caching docs.
- `dbff20d` — Wave D: ABI version + ST 1910 + Demuxer C row docs refresh.
- `37ff0ea` — cross-wave fix: narrow subtitle invalid-handle test post-Wave-A.
- `b752f39` — Wave H+B coordination: extend `__post_init__` to validate Wave B fields.
- `30f9059` — Wave I: Phase N / Task N scaffold sweep.

---

### `klv::pack::Iter` tightened to `pub(crate)` + Universal Set machinery retired (2026-05-24)

Closes the Phase 1 SemVer-ratchet deferral on `tst_core::klv::pack::Iter`
(`docs/deferred-features.md` entry removed). The iterator was already
`#[doc(hidden)] pub` and excluded from `cargo public-api --simplified`
baselines, so the visibility tightening has zero baseline drift. The
fuzz-target relocation precondition shipped in Phase 5 (`klv_iter.rs` was
in `tst-core/fuzz/` already); this commit drops the now-irrelevant fuzz
binary because its coverage is provided transitively by
`klv_st0601_decode` / `klv_st0102_decode` / `klv_st0903_decode`.

The tightening surfaced that `Iter::universal_set` + `Iter::remaining` +
`IterMode::UniversalSet` + `next_universal_set` had no callers outside the
deleted fuzz target and a pair of in-file tests. Per YAGNI those got
ripped out — `Iter` is now single-mode (`{ buf, offset, finished }`) and
local-set only. A future ST 0905 / universal-set typed decoder would add
back a separate iterator shape (the prior universal-set path was admitted
"included for completeness but the typed layer never calls it").

**Changed (internal — no public API impact):**

- `tst_core::klv::pack::Iter` is now `pub(crate)`. Removed from the
  `pub use pack::{Iter, OwnedRawField, RawField}` re-export in
  `crates/tst-core/src/klv/mod.rs`. `RawField` + `OwnedRawField` + the
  `pub fn encode_pack` surface are unchanged.
- `Iter::local_set` is now `pub(crate)`. `Iter::universal_set` +
  `Iter::remaining` + `IterMode` + `next_universal_set` deleted.
- `cargo public-api --simplified` baselines for tst-core, tst-pipeline,
  tst-srt: zero drift (verified). `#[non_exhaustive]` count unchanged at
  **162**.

**Removed (fuzz / oss-fuzz integration):**

- `crates/tst-core/fuzz/fuzz_targets/klv_iter.rs` deleted; matching
  `[[bin]]` entry removed from `crates/tst-core/fuzz/Cargo.toml`.
- `oss-fuzz/targets/klv_iter_seed_corpus/` deleted; `oss-fuzz/build.sh`
  no longer references `klv_iter` (3-target dict loop instead of 4, one
  fewer `zip_seeds` call). Expected next-rebuild figures: 15 targets /
  13 seed corpora / 3 dicts (down from 16 / 14 / 4). `oss-fuzz/
  VERIFICATION.md` gained a top-of-file note annotating the retirement
  rather than overwriting the 2026-05-15 historical snapshot.

**Docs:**

- Doc-link references to `Iter::local_set` stripped from `pub fn`
  comments in `crates/tst-core/src/klv/st0903/decode.rs` +
  `crates/tst-core/src/klv/st0102/decode.rs` (would have warned under
  `RUSTDOCFLAGS="-D warnings"` as "public docs link to private item").
- `docs/deferred-features.md`: the "`klv::pack::Iter` — tighten from
  `pub` to `pub(crate)`" entry deleted (no longer deferred).

**Background:** the prior deferred-features entry claimed that after
relocating the fuzz target into `crates/tst-core/fuzz/` the import would
"become crate-local (`crate::klv::pack::Iter`) and the `pub(crate)`
change is free." That claim was wrong — `cargo fuzz` sub-crates are
separate cargo packages that depend on the parent crate externally, so
`pub(crate)` would still have broken the import. The actual path to
closure was deleting the fuzz binary, not relocating it.

---

### Python bindings audit-2 closeout (2026-05-24)

10 findings from `docs/python-1/python-bindings-deep-dive-audit-2.md` + 1
hygiene item from the Codex CFI follow-up validation memo, shipped via
11-task parallel SDD (10 wave-1 worktrees + 1 wave-2 worktree). All
changes are Python-side (`tstrans`) or `tst-c`-side; Rust public API of
`tst-core` / `tst-pipeline` / `tst-srt` is unchanged (cargo public-api
baselines diff-clean). `#[non_exhaustive]` count stays at **162**. Bash
ratchet count goes from 13 to **14**.

**Added (`tstrans` 0.1.0):**

- `DemuxEvent.UnknownSample` — new event subclass carrying `stream`,
  `pts`, `dts`, raw `stream_type: int`, and `payload: bytes` for PMT
  entries the demuxer cannot classify as Video/Audio/Subtitle/KLV.
  Previously these collapsed into `NonConformant` events and the raw
  payload was discarded. `tstrans.pandas.events_to_dataframe` emits
  `kind="unknown_sample"` rows with `stream_type` + `payload_len`.
  (audit-2 #1)
- `DemuxErrorKind.STRICT_REJECTION` — Python-side variant for
  `tst_core::DemuxError::StrictRejection`. Previously mapped to
  `INTERNAL`, obscuring the difference between strict-mode policy
  outcomes and actual internal binding/library failures. (audit-2 #8)
- `py.typed` marker per PEP 561 — downstream type checkers can now see
  the inline annotations across `tstrans` instead of treating the
  package as untyped. Ships in the wheel via maturin's `python-source`
  mode. (audit-2 #6)

**Fixed (`tstrans` 0.1.0):**

- `MuxerFileSink(atomic=True)` no longer leaks `.partial` tempfiles
  when `_drain_muxer_to_file()` or `fh.close()` raises in `__exit__`.
  Restructured to an outer `try/finally` so cleanup always runs; the
  caller's exception is still propagated and never suppressed.
  (audit-2 #2)
- `Demuxer.next_event()` no longer pins the GIL during AAC/MP2 audio
  frame parsing. The AAC and MP2 arms of `convert_sample_event` now
  parse to owned Rust frames under `py.allow_threads`, then construct
  `Py<AdtsFramePy>` / `Py<Mpeg2AudioFramePy>` after re-acquiring the
  GIL. Mirrors the discipline already used in `Demuxer.feed()`.
  (audit-2 #3)
- `Pts90khz`, `TimeStatus`, `UasDatalinkLs.universal_label`, and
  `VTargetPack.target_color` now validate primitive shapes at
  `__post_init__` rather than deferring to PyO3 / Rust encoder
  failures. Catches typos like `Pts90khz(raw=2**63)` at the user's
  construction site. (audit-2 #4)
- `parse_klv_universal()` now rejects trailing bytes after the
  declared outer BER length for ST 0102 (Security) and ST 0903 (VMTI)
  — previously silently ignored. ST 0601 and ST 0605 paths already
  enforced this via their family-specific decoders. (audit-2 #5)
- `klv_to_dataframe(..., mode=...)` now raises `ValueError` on
  invalid mode strings instead of silently returning a
  `mode="summary"`-shaped DataFrame. Catches typos like
  `mode="target"` (missing 's'). (audit-2 #7)

**Changed (docs only):**

- `tstrans.mpegts` module header docstring + `DemuxerConfig` docstring
  + `tstrans.pandas` package comment refreshed to describe the
  current Phase 6+ surface (`UnknownSample`, `cfi_tolerance`, codec
  event payload types, multi-cell AU diagnostics, file sinks).
  Previously read like the Phase 2 / Phase 4 snapshots. (audit-2 #10)

**Test coverage (`tstrans` 0.1.0):**

- 6 previously-skipped tests now run via synthetic fixtures in
  `bindings/python/tests/_builders/` (`synthetic_klv_universal.py`) and
  `bindings/python/tests/fixtures/` (`aac_minimal.ts`, `audio_aac_large.ts`).
  Covers ST 0102 + ST 0903 pandas DataFrame paths, the GIL-release
  smoke test for audio, the GIL-progress assertion for audio-heavy
  captures, and a full mux→demux round-trip. The remaining 17
  fixture-dependent skips are cataloged with revisit triggers in
  `docs/python-1/python-bindings-skip-backlog.md` (outside the
  published repo). Default pytest count: 631 → 634 passed (1 → 1
  deferred-feature skip remaining); pandas pytest: 64 → 67 passed
  (0 skipped). (audit-2 #9)

**Tooling (`tst-c`):**

- `cbindgen` post-processor in `bindings/c/build.rs` strips the
  leading space cbindgen 0.29.x emits before single-line function
  declarations in the generated header — affected 55 declarations
  across `tstrans.h`, flagged by the Codex CFI follow-up validation
  memo as a hygiene item around `tst_demux_config_set_cfi_tolerance`.
  New `scripts/check-c-header-no-leading-space.sh` ratchet guards
  against regression — that's the **14th** bash ratchet (added to the
  pre-push list in `CLAUDE.md`).

---

### rename: `malformed_au_cell_cfi_tolerance` → `cfi_tolerance` (BREAKING) (2026-05-24)

Pre-1.0 ergonomic rename across the whole AU-cell-CFI tolerance API
surface. The original identifier (34 chars) read awkwardly in Python
kwarg form; renamed to the short noun form so the C ABI setter
(`tst_demux_config_set_<name>`) still reads cleanly.

**Breaking changes:**

- `DemuxerConfig::malformed_au_cell_cfi_tolerance` → `DemuxerConfig::cfi_tolerance`
- `DemuxerBuilder::malformed_au_cell_cfi_tolerance(enable)` → `DemuxerBuilder::cfi_tolerance(enable)`
- `NonConformantIssue::MalformedAuCellCfiTolerated { pid, observed_cfi, treated_as }` → `NonConformantIssue::CfiTolerated { pid, observed_cfi, treated_as }`

**Python (`tstrans` 0.1.0) breaking:**

- `tstrans.mpegts.DemuxerConfig(malformed_au_cell_cfi_tolerance=True)` → `DemuxerConfig(cfi_tolerance=True)`
- `tstrans.mpegts.NonConformantKind.MALFORMED_AU_CELL_CFI_TOLERATED` → `tstrans.mpegts.NonConformantKind.CFI_TOLERATED`

**C ABI (no minor bump — additive on top of unreleased state):**

- `tst_demux_config_set_malformed_au_cell_cfi_tolerance(...)` → `tst_demux_config_set_cfi_tolerance(...)`
- `TST_NONCONFORMANT_CODE_MALFORMED_AU_CELL_CFI_TOLERATED` → `TST_NONCONFORMANT_CODE_CFI_TOLERATED` (discriminator value `32` unchanged)

Discriminator value `32` is preserved on both sides — only the symbol
names change. `tst-core/public-api.txt` baseline rebumped accordingly.
`#[non_exhaustive]` count stays at 162.

---

### follow-up: codex CFI review fixes + python audit findings + 13th ratchet (2026-05-24)

Three small fixes from review feedback landing after the original work
shipped.

**Added:**

- `TstCellFragmentIndication` enum is now actually exported in `tstrans.h`.
  Defined in `bindings/c/src/event.rs` as the C mirror of Rust's
  `CellFragmentIndication`, but cbindgen's `[export] include` allowlist
  in `cbindgen.toml` was missing the entry — so C callers received the
  raw `cc_expected` / `cc_observed` bytes with no constants to compare
  against. Generated header now contains `enum tst_cell_fragment_indication`
  with `TST_CELL_FRAGMENT_INDICATION_{MIDDLE,LAST,FIRST,COMPLETE}` constants.
  (Caught by Codex review, `docs/analysis/2026-05-24-codex-review-au-cell-cfi-fix-validation.md`.)
- `scripts/check-c-header-mirror-enum-export.sh` (13th bash ratchet) —
  enumerates every `pub enum Tst*` in `bindings/c/src/` and asserts
  each is in the `cbindgen.toml [export] include` allowlist. Prevents
  the next mirror enum from regressing the same way. Wired into
  `.github/workflows/ci.yml`.

**Changed (Python audit #11 follow-up):**

- The list-returning audio parsers `parse_aac_frames`,
  `parse_aac_frames_with_resync`, `parse_mpeg2_audio_frames`, and
  `parse_mpeg2_audio_frames_with_resync` now release the GIL during
  heavy parse work via `py.allow_threads(...)`. Their `iter_*`
  counterparts already did, but the list-returning equivalents were
  missed by audit #11.

**Fixed (Python audit #7 follow-up):**

- `docs/guide-python-pandas.md` line 3 corrected from "NumPy zero-copy
  views" to "NumPy snapshot views (one Rust-to-Python `bytes` copy per
  access; see Snapshot vs zero-copy below)". The body section already
  explained the snapshot semantics correctly; only the leading
  paragraph contradicted it.

---

### au-cell CFI tolerance mode (opt-in receive-side compatibility) (2026-05-24)

**Added:**

- `DemuxerConfig::cfi_tolerance: bool` (default
  `false`) — opt-in tolerance for sync-metadata AU cells that arrive
  with `cell_fragment_indication` bits set to `0b00` (Middle) or
  `0b01` (Last) without a prior `First` cell. When enabled AND the
  orphan cell's inner payload independently validates as one complete
  KLV record (SMPTE 336M UL prefix + BER length match), the demuxer
  emits the cell as `MetadataKind::KlvSyncAuCell { cell_fragment_indication:
  Complete, .. }` plus a
  `NonConformantIssue::CfiTolerated { pid,
  observed_cfi, treated_as }` diagnostic so the malformation remains
  visible. Off by default keeps the spec-strict behavior: orphan
  cells surface only as `NonConformantIssue::MultiCellAu { reason:
  MultiCellAuReason::Orphan }`.
- `DemuxerBuilder::cfi_tolerance(enable)` — builder
  method for the new config field.
- `NonConformantIssue::CfiTolerated { pid: u16,
  observed_cfi: CellFragmentIndication, treated_as:
  CellFragmentIndication }` — new variant carrying the wire-observed
  and substituted CFI bytes for telemetry / diagnostics.

**Python (`tstrans` 0.1.0):**

- `tstrans.mpegts.DemuxerConfig.cfi_tolerance: bool`
  (default `False`) — mirrors the Rust config field.
- `tstrans.mpegts.CellFragmentIndication` — new `eq_int` enum
  (`MIDDLE=0`, `LAST=1`, `FIRST=2`, `COMPLETE=3`; discriminants match
  the H.222.0 V9 Table 2-157 wire bits exactly).
- `tstrans.mpegts.NonConformantKind.CFI_TOLERATED`
  — new enum member.
- `tstrans.mpegts._NonConformantEvent.observed_cfi` and
  `tstrans.mpegts._NonConformantEvent.treated_as` — new optional
  `CellFragmentIndication` fields set only on the new variant; `None`
  on every other issue kind.

**C (ABI minor 3 → 4):**

- `TST_NONCONFORMANT_CODE_CFI_TOLERATED` (= 32) —
  new `TstNonConformantCode` variant.
- `TstCellFragmentIndication` — new `repr(i32)` enum mirror.
  Discriminants match the wire bits: `Middle=0`, `Last=1`, `First=2`,
  `Complete=3`.
- `TstEventNonConformant.cc_expected` and
  `TstEventNonConformant.cc_observed` carry the observed and
  substituted CFI bytes (reusing the existing single-byte field
  carriers — no struct-size change).
- `tst_demux_config_set_cfi_tolerance(cfg, enable)`
  — new setter on the opaque `tst_demux_config_t` builder.
- `TST_ABI_VERSION_MINOR` bumped 3 → 4 (additive).

**Tests:**

- `crates/tst-core/tests/mpegts_au_cell_tolerance.rs` (NEW, 7
  integration tests) — strict mode + tolerance mode coverage matrix
  (orphan Middle/Last with valid KLV, invalid payload, BER-length
  mismatch, legitimate fragmentation unaffected, mid-buffer
  SequenceGap unaffected).
- `crates/tst-core/src/mpegts/demux/pes_emit.rs` (6 new validator
  unit tests + tests module).
- `crates/tst-core/src/mpegts/demux/types.rs` (3 new config-field
  unit tests).
- `crates/tst-core/src/mpegts/demux/event.rs` (1 new Display test).
- `bindings/python/tests/test_au_cell_tolerance.py` (NEW, 11 tests) —
  surface + end-to-end binding coverage.
- `bindings/c/src/demux_config.rs` (3 new setter unit tests).

**Background:** ITU-T H.222.0 V9 §2.12.4.2 Table 2-157 defines
`cell_fragment_indication = 0b11` as a single complete cell. Some
encoders mis-emit `0b00` or `0b01` for single complete KLV records.
The library now supports both spec-strict reception (default) and
opt-in tolerance for malformed producers — see
`docs/guide-mpegts-demux.md` and `docs/troubleshooting.md`.

### tst-py: audit #11 — release the GIL around heavy Rust work (2026-05-24)

**Performance:**

- `Demuxer.feed`, all `Muxer.push_*` methods (`push_video`,
  `push_video_to`, `push_video_to_with_dts`, `push_audio`,
  `push_audio_to`, `push_klv`, `push_klv_to`), and the codec
  eager-collect iterators (`iter_aac_frames`,
  `iter_aac_frames_with_resync`, `iter_mpeg2_audio_frames`,
  `iter_mpeg2_audio_frames_with_resync`) now release the Python GIL
  around their inner Rust work via `py.allow_threads`. This unblocks
  other Python threads (notebook UI, GUI event loops, sibling worker
  threads) during long parses or large pushes. Measured impact on a
  dev box:

  | Workload | Worker-thread throughput (vs solo) — before → after |
  |---|---|
  | `Muxer.push_video` (30 MB H.264 NAL) | 25% → ~100% |
  | `iter_aac_frames_with_resync` (500 MB) | 10% → ~85% |
  | `iter_mpeg2_audio_frames_with_resync` (500 MB) | 12% → ~85% |

  No behavior change for callers — public-API contract is identical.

**Intentionally NOT GIL-released:**

- `Muxer.pull(bytearray)` — the `PyByteArray` mutable-slice safety
  relies on the GIL preventing concurrent resize/mutation from another
  thread. Pull is also fast (memcpy-class, microseconds per call).
- KLV decode entry points (`decode_uas_datalink`, `decode_security`,
  `decode_vmti`, `decode_precision_timestamp`) — typical record sizes
  (20-200 bytes, ~5us Rust decode) are below the GIL transition
  breakeven (~50us); wrapping produces lock-contention pathology under
  hot batch loops.

**Tests:**

- `bindings/python/tests/test_gil_release.py` (NEW, 5 tests) — pure-Python
  worker-thread concurrency probes verifying the wrapped methods let
  other threads progress.

### tst-py: audit #9 — pythonic muxer arg order + keyword-only pts (2026-05-24)

**Changed (BREAKING — pre-1.0):**

- All `Muxer.push_*` methods now require `pts` (and `dts` where
  applicable) as a keyword-only argument. Positional `pts` raises
  `TypeError` at the PyO3 argument-extraction boundary. Affected
  methods: `push_video`, `push_video_to`, `push_audio`, `push_audio_to`,
  `push_klv`, `push_klv_to`. `push_video_to_with_dts` was already
  keyword-only and is unchanged.

- `Muxer.push_audio_to` argument order changed from
  `(handle, pts, frames)` to `(handle, frames, *, pts)` — `frames`
  moved before `pts`, and `pts` became keyword-only. This normalizes
  the `_to` form to match the single-stream `push_audio(frames, *, pts)`
  shape and the broader Python convention of `(target?, payload, *,
  pts)` across all `push_*` methods. The lower-level Rust API
  (`MuxerCore::push_audio_to`) is unchanged; the inconsistency was in
  the Python surface only.

**Migration:**

```python
# Before
muxer.push_video(nal, ts)
muxer.push_video(nal, ts, True)
muxer.push_video_to(handle, nal, ts)
muxer.push_audio(frames, ts)
muxer.push_audio_to(handle, ts, frames)   # note pts before frames
muxer.push_klv(klv, ts)
muxer.push_klv_to(handle, klv, ts)

# After
muxer.push_video(nal, pts=ts)
muxer.push_video(nal, pts=ts, key_frame=True)
muxer.push_video_to(handle, nal, pts=ts)
muxer.push_audio(frames, pts=ts)
muxer.push_audio_to(handle, frames, pts=ts)   # frames first, kw pts
muxer.push_klv(klv, pts=ts)
muxer.push_klv_to(handle, klv, pts=ts)
```

Closes audit #9. See
`docs/python-1/python-bindings-audit.md` §9 and
`docs/plans/2026-05-24-python-audit-backlog.md` §9.

---

### tst-py: audit #14 — remove unimplemented subtitle muxing surface (2026-05-24)

**Removed:**

- `MuxerProgramConfigBuilder.add_subtitle(pid, codec)` has been removed
  from the Python public API. It previously raised
  `NotImplementedError` unconditionally because the Rust mux-side
  `SubtitleCodec` is a struct-variant enum (carrying language, page
  IDs, and ancillary descriptors) that the flat Python `SubtitleCodec`
  enum doesn't yet model. A half-implemented placeholder is worse than
  a missing one — users built against it, then hit a runtime error.
  Demux-side subtitle decoding via `DemuxEvent.Subtitle` remains
  supported and unchanged. The `Muxer.push_subtitle` /
  `push_subtitle_to` / `Muxer.subtitle_handles()` /
  `MuxerProgramConfigBuilder.stream_descriptors_for_subtitle` surfaces
  remain wired so they begin working as soon as the construction gap
  closes. See `docs/deferred-features.md` "Python-side subtitle
  muxing" for the trigger to revisit. Closes audit #14.

---

### tst-py: audit #7 — NumPy "zero-copy" rewritten as "snapshot view" (2026-05-24)

**Docs:**

- Reworded the NumPy accessor documentation across
  `docs/guide-python-pandas.md`, `docs/guide-python.md`, and the
  `_make_np_property` docstring in
  `bindings/python/python/tstrans/codec.py` to describe
  `.payload_np` / `.raw_rbsp_np` / `.raw_np` as **snapshot views**
  rather than **zero-copy views**. Each accessor call materializes a
  fresh Python `bytes` from Rust-owned storage (one `O(payload_length)`
  copy), then NumPy views that bytes object with zero further copies.
  Added a new "Snapshot vs zero-copy" subsection to the pandas guide
  explaining the per-access cost, the manual-cache pattern for repeated
  reads, and a forward note that a future plan may implement the
  Python buffer protocol on the Rust types to eliminate the bytes copy.
  Extended `tests/test_pandas_numpy_views.py` with two new tests
  pinning the snapshot semantic: each access returns a distinct
  ndarray object (`a1 is not a2`), and the read-only guard + the
  fresh-snapshot guarantee together prevent any mutation from leaking
  to a subsequent access. Closes audit #7.

---

### tst-py: audit #5 — KLV unknown TLV round-trip preservation (2026-05-24)

**Fixed:**

- KLV inverse converters now forward the `unknown: tuple[tuple[int, bytes],
  ...]` field from each typed-set Python dataclass into the corresponding
  Rust struct's `Vec<OwnedRawField>`, so `decode -> encode -> decode` is
  losslessly round-trip-preserving for forward-compat tags. Previously
  ST 0102 `SecurityLs`, ST 0903 `VmtiLs` + `VTargetPack`, and ST 0601
  `UasDatalinkLs` decoded an unknown tag into `.unknown` but their
  inverse translators dropped it before encode — making decode-edit-encode
  workflows silently lossy for any vendor-private, ST 0107-only, or
  future-standard TLV. Closes audit #5.

**Added:**

- Collision precedence rule documented + enforced at the Python-Rust
  boundary: when a Python-supplied `unknown` entry's tag is already
  covered by the encoder's typed table for that set (e.g. tag 13 in
  ST 0601, tag 3 in ST 0102, tag 3 in ST 0903 VMTI, tag 5 in
  ST 0903 VTargetPack), the unknown entry is silently dropped — the
  typed field wins. This keeps the wire form free of duplicate TLVs
  and sidesteps ST 0601's `KlvEncodeError::ReservedTagInUnknown` for
  user-hand-constructed records. Real decode never produces such an
  `unknown` entry (the decoder routes typed tags to typed fields), so
  the drop only affects manual record construction.

- New `bindings/python/tests/test_klv_round_trip_unknown.py` (15 tests):
  per-set round-trip preservation, dataclass-construction preservation,
  the collision-typed-wins matrix, the nested VmtiLs-with-VTargetPack
  case, and Python-side `unknown` shape validation (raises on non-
  tuple / non-int tag / non-bytes value entries).

**Notes:**

- ST 0605 `PrecisionTimeStampPack` has no `unknown` field (it's a
  fixed 2-field pack) and is unaffected.
- No new Rust public-API surface; no `#[non_exhaustive]` BASELINE
  change (stays at 162).

---

### tst-py: audit #8 — Python docs refreshed to Phase 6 reality (2026-05-24)

**Docs:**

- Refreshed stale Python binding status text across `README.md`,
  `bindings/python/README.md`, and `docs/guide-python.md` to reflect the
  Phase 6 shipped reality (Demuxer + Muxer + typed KLV decode/encode
  for ST 0601 / ST 0102 / ST 0605 / ST 0903, codec parsers for
  H.264 / H.265 / H.266 / AV1 / AAC / MPEG-2 audio, optional pandas
  DataFrame + NumPy adapters). Replaced the "Once v1 ships" placeholder
  Quickstart in `bindings/python/README.md` with real `parse_file` /
  `probe` / `Muxer.write_file` examples. Updated the v1 roadmap
  to mark Phases 0-6 SHIPPED and Phase 7 (CI wheels + PyPI publish)
  as UP NEXT. Closes audit #8.

---

### tst-py: audit #3 — split `skip_unknown` from `skip_malformed` (2026-05-24)

**Changed:**

- `tstrans.io.extract_klv(parsed=True)` gains a new keyword-only
  `skip_malformed: bool = False` parameter and stops conflating it
  with `skip_unknown`. Previously `skip_unknown=True` (the default)
  silently swallowed *both* unknown universal labels and `KlvError`
  raised by the decoder on a recognized UL — masking real upstream
  corruption (truncated sets, bad checksums) as "skipped unknown".
  Now `skip_unknown` only suppresses payloads whose UL doesn't match
  any of the four supported sets, and `skip_malformed=False`
  propagates the `KlvError` so callers see data loss. The decoder
  exception is also caught specifically as `KlvError` (not bare
  `Exception`), so binding-shape regressions surface as `TypeError` /
  `AttributeError` rather than being absorbed. Closes audit #3.

  Migration: callers that want the old swallow-everything behavior
  should pass both `skip_unknown=True, skip_malformed=True`; callers
  who only ever fed well-formed KLV need no change.

---

### tst-py: Python bindings audit small wave (2026-05-24)

Small carry-forward batch from the 2026-05-24 Python bindings audit
(`docs/python-1/python-bindings-audit.md`). Three independent fixes
shipped as three commits; the larger #3 + #5-14 backlog gets its own
plan.

**Added:**

- `Muxer.write_file(path, atomic=True)` — opt-in atomic file sink.
  When `atomic=True`, `MuxerFileSink` writes to a `*.partial`
  tempfile in the same directory as `path` and `os.replace`s into
  place only on successful `__exit__`; on exception the tempfile
  is removed so nothing appears at the destination. Default
  `atomic=False` preserves the existing non-atomic
  drain-and-close-on-exception contract (the destination may exist
  as a valid TS prefix). Closes audit #13.

**CI:**

- New `python-core` GitHub Actions job runs the default (non-pandas)
  pytest suite after `maturin develop --release`. Closes audit #1
  (Python core tests previously had no CI coverage — only the pandas
  extra job ran, and only against pandas-marked tests).
- The 12th bash ratchet `scripts/check-py-codec-error-mapping-coverage.sh`
  now runs in CI (as the first step of `python-core`). Future
  `CodecParseError` variants can no longer silently miss Python
  exception mapping.

**Tests:**

- Pandas-marked test modules now use `pytest.importorskip("pandas")`
  / `pytest.importorskip("numpy")` so the default `-m 'not pandas'`
  run can collect without the optional extras installed. Latent bug
  surfaced by the new `python-core` job — collection failed on
  `import pandas as pd` in the 4 `test_pandas_*.py` files when the
  extras weren't installed.

**Docs:**

- `tstrans.codec.iter_aac_frames` / `iter_aac_frames_with_resync` /
  `iter_mpeg2_audio_frames` / `iter_mpeg2_audio_frames_with_resync`
  docstrings now state explicitly that all frames are collected
  upfront into a `Vec` at construction time — the returned object
  iterates that `Vec`, it is not a true streaming parser. Memory
  usage is O(num_frames); peak allocation occurs at construction.
  Behavior unchanged. Closes audit #12.

**Fixed:**

- `Pts90khz.ms` now truncates toward zero instead of flooring toward
  -infinity, matching Rust's `i64 / 90` semantics. Negative timestamps
  off-by-one for any raw value `-89 .. -1` (returned `-1`, should
  return `0`) and similar non-multiples-of-90 below zero. Closes
  audit #2.
- `tstrans.io.probe().packet_count` now reflects actual TS packets
  scanned (`bytes_read // 188`). Previously this field fell through
  to a sum of unrelated demuxer stats
  (`program_maps_seen + pmt_versions_seen + ...`) because neither
  `ts_packets_in` nor `packets_processed` exists on the Rust
  `DemuxerStats`. Existing callers relying on the field for
  bitrate / ingest validation / progress estimates were getting
  wrong numbers. Closes audit #4.
- KLV inverse conversion (`encode_uas_datalink` / `encode_vmti`) now
  raises `ValueError` when `UasDatalinkLs.universal_label` is not
  exactly 16 bytes or when `VTargetPack.target_color` is not exactly
  a 3-tuple. Previously both were silently dropped — `universal_label`
  fell back to the all-zero default UL and `target_color` was omitted
  from the encoded LS, producing a valid-looking-but-wrong KLV record
  with no error surfaced to the caller. Error messages name the field
  and the observed length. `target_color=None` (field absent) is still
  accepted. Closes audit #6.

**Changed:**

- `tstrans.mpegts.Demuxer.feed` now accepts any bytes-like input
  (`bytes`, `bytearray`, `memoryview`, NumPy `uint8` arrays), not
  only `bytes`. The previous signature required callers to pre-copy
  `bytearray` / `memoryview` via `bytes(...)` themselves. `bytes`
  callers stay on the existing zero-copy borrow path; other
  bytes-like inputs are coerced once through Python's `bytes()`
  builtin (one C-level copy) before being fed to the demuxer.
  `PyBuffer` would skip that coercion but is gated behind
  `not(Py_LIMITED_API)` in PyO3 0.22; `tst-py` builds with
  `abi3-py310` so the coercion is unavoidable on Python 3.10. Closes
  audit #10.

### mpegts: multi-cell AU cell reassembly (2026-05-24)

**`mpegts::demux` now reassembles fragmented Metadata AU cells per
H.222.0 V9 §2.12.4.2.** Two flavors covered: multiple AU cells back-to-back
within one PES (previously silently truncated to the first cell), and cells
of a single AU spread across multiple PES packets (`First` / `Middle` /
`Last`). Per-PID reassembly buffer capped at 1 MiB by default; failure
modes surface as typed `MultiCellAuReason` on
`NonConformantIssue::MultiCellAu`.

**Added:**

- `mpegts::demux` multi-cell Metadata AU cell reassembly
  (H.222.0 §2.12.4.2). Fragmented sync-metadata AUs (`First` / `Middle` /
  `Last`) now reassemble into a single demuxer event with
  `MetadataKind::KlvSyncAuCell::was_reassembled = true` and
  `cell_count = N`. Multiple AU cells packed into one PES (previously
  silently truncated to the first cell) now each emit their own event.
- `DemuxerConfig::au_cell_cap_per_pid` (default 1 MiB) caps the per-PID
  reassembly buffer; overflow drops with
  `NonConformantIssue::MultiCellAu { reason: Overflow, .. }`.
- `MultiCellAuReason` enum (`Orphan` / `SequenceGap` / `ConcurrentFirst` /
  `Overflow`) surfaces typed reassembly failure modes via
  `NonConformantIssue::MultiCellAu::reason`.
- `MetadataKind::KlvSyncAuCell` gains `was_reassembled: bool` +
  `cell_count: u32` fields.

**Changed:**

- `NonConformantIssue::MultiCellAu` Display reworded — describes the
  reassembly failure mode instead of the prior "not implemented" message.

**Bindings:**

- `tst-py`: `_KlvEvent` gains `was_reassembled: bool` + `cell_count: int`;
  new `MultiCellAuReason` PyO3 `eq_int` enum (`ORPHAN` / `SEQUENCE_GAP` /
  `CONCURRENT_FIRST` / `OVERFLOW`); `_NonConformantEvent` gains optional
  `multi_cell_au_reason` field.
- `tst-c`: `TstEventMetadata` gains `was_reassembled` + `cell_count` at
  the end of the struct (additive ABI change); new
  `tst_multi_cell_au_reason` enum mirrors `MultiCellAuReason`;
  `TST_ABI_VERSION_MINOR` bumped 2 → 3.

**Corpus validation (2026-05-24, 251 files):**

- _FMV captures (28 files): **60,089 typed KLV records** (previously 0
  before this plan — every cell past the first was silently dropped).
  175,121 NonConformant `MultiCellAu(Orphan)` events surface real
  wire-format issues that were previously invisible.
- _Raw captures (22 files): 88,635 typed KLV records, 92,691 Orphans.
- Other captures (201 files): 1,644,958 typed KLV records, 113,355
  Orphans.
- Total across 251 files: **1,793,682 typed KLV records.**
  `klv_reassembled = 0` indicates the corpus's actual fragmentation
  pattern is back-to-back Complete cells in one PES (the
  classify_klv-only-looked-at-first-cell bug) plus malformed
  `Middle`/`Last`-CFI orphans — not legitimate `First`+`Middle`+`Last`
  cross-PES fragmentation (which Task 5 integration tests prove the
  reassembler handles correctly).

---

### tst-py Phase 6 — pandas + NumPy adapters (2026-05-23)

**`tstrans.pandas` DataFrame adapters + zero-copy NumPy views via optional
[pandas] extra.** Python-only; no Rust changes. Existing `pip install
tstrans` workflow continues to work identically — adapters and accessors
appear only when `pip install 'tstrans[pandas]'` activates the extra.

**Added:**
- Optional `[pandas]` extra in `pyproject.toml` (pandas >= 2.0, numpy >= 1.24).
  Install: `pip install 'tstrans[pandas]'`. Without the extra, the
  adapters raise a friendly `ImportError` directing the user to install
  the extra.
- New `tstrans.pandas` submodule with 5 DataFrame adapters:
  - `klv_to_dataframe(records, *, mode="summary")` — polymorphic over
    `UasDatalinkLs` / `SecurityLs` / `PrecisionTimeStampPack` / `VmtiLs`.
    KLV DataFrames use `pd.DatetimeIndex(tz="UTC")` built from
    `timestamp_us` (microseconds since UTC epoch) when present; RangeIndex
    fallback when absent. ST 0903 supports `mode="summary"` (one row per
    VMTI LS) and `mode="targets"` (MultiIndex `[pts, target_id]`, one row
    per VTarget). Composite fields flatten to FLAT scalar columns (e.g.
    `frame_center_lat_deg`, `corner_lat_pt1_deg`).
  - `events_to_dataframe(events)` — union schema across all DemuxEvent
    kinds. Kind labels: `Pmt` / `ProgramMap` / `Sample` / `Klv` /
    `Discontinuity` / `NonConformant` / `EndOfStream`.
  - `nals_to_dataframe(nals, pts=None)` — H.264/H.265/H.266 NAL lists
    with spec-name lookup column.
  - `obus_to_dataframe(obus, pts=None)` — AV1 OBU lists with type-name
    lookup.
  - `audio_frames_to_dataframe(frames)` — polymorphic AdtsFrame /
    Mpeg2AudioFrame; enum-typed fields render as bare names (e.g. `LC`,
    `III`, `JOINT_STEREO`) for analyst-friendly grouping.
- Zero-copy NumPy accessors via Python monkey-patches on 15 byte-bearing
  classes:
  - `.payload_np` on `NalUnit` / `Obu` / `AdtsFrame` / `Mpeg2AudioFrame`.
  - `.raw_rbsp_np` on H.264/H.265/H.266 `Sps` / `Pps` / `Vps` /
    `SliceHeaderLight`.
  - `.raw_np` on `Av1SequenceHeader` / `Av1FrameHeaderLight`.
  - Returns `numpy.ndarray` (uint8 view) backed by the underlying `bytes`
    — no copy.
- `KlvFieldError` `field_errors` field renders as `|`-joined string of
  `tag<N>:<kind>:<message>` triples in the DataFrame (pipe-separator
  avoids ambiguity since `KlvFieldError.__str__` already contains commas).
- `nal_count` column is gated to video-only event rows; audio AdtsFrame
  lists do not populate it (avoids misleading `df.nal_count > N` filters
  on audio sample rows).
- Pytest marker `@pytest.mark.pandas` (default-skipped via
  `addopts = "-m 'not pandas'"` in `pyproject.toml`). Existing 513
  default unit tests remain unchanged.
- New CI job `python-pandas-extra` installs the extra and runs
  marker-only pytest (`pytest -m pandas --override-ini=addopts=`).
- 7 `test_pandas_missing_extra.py` tests (NOT marked) confirm every
  adapter raises a friendly `ImportError` when the extra is absent.
- User guide at `ts-transformer/docs/guide-python-pandas.md` covers
  install, adapters, NumPy views, common analyst recipes.

**Unchanged:**
- Core package `pip install tstrans` continues to work identically to
  Phase 5; no return types change.
- BASELINE non_exhaustive: 159 (Phase 6 = Python-only, no Rust changes).
- 12 bash ratchets: green.
- All 3 cargo public-api baselines (`tst-core` / `tst-pipeline` /
  `tst-srt`): clean.

**Internal:**
- pytest count: 513 → 520 default (added 7 missing-extra tests) +
  60 pandas-marker = ~582 total (520 default + 60 pandas, plus 5
  skipped across the two runs).
- No new Rust public API surface.
- No new fuzz targets.

---

### tst-py Phase 5 — Codec parsers (2026-05-23)

**`tstrans.codec` module fully populated; `Sample.payload` typed-replaced.**
(`b8aa957..a9d1634`). Exposes ~50 classes and ~25 functions covering
H.264 / H.265 / H.266 / AV1 / AAC / MPEG-2 audio parsers. Three new
Rust parsers (`parse_slice_header_light` for H.264 / H.265 / H.266) and
three fuzz harnesses ship alongside the Python surface. `Sample.payload`
changes from `bytes` to a typed list — first breaking change since Phase 2
(pre-1.0, per the project's break-freely policy).

**Added:**

- `tstrans.codec` module: `NalUnit`, `Obu`, `ObuExtension`,
  `H264Sps`, `H264SliceHeaderLight`, `H264SliceType`, `H264Pps`,
  `H264HrdParameters`, `H264VuiParameters`, `H264ColorPrimaries`,
  `H264MatrixCoefficients`, `H264TransferCharacteristics`,
  `parse_h264_sps`, `parse_h264_slice_header_light`,
  `H265Sps`, `H265SliceHeaderLight`, `H265SliceType`, `H265Vps`,
  `H265Pps`, `H265HrdParameters`, `H265VuiParameters`,
  `H265ColorPrimaries`, `H265MatrixCoefficients`,
  `H265TransferCharacteristics`, `parse_h265_sps`,
  `parse_h265_slice_header_light`,
  `H266Sps`, `H266SliceHeaderLight`, `H266SliceType`,
  `parse_h266_sps`, `parse_h266_slice_header_light`,
  `Av1SequenceHeader`, `Av1FrameHeaderLight`, `Av1FrameType`,
  `ObuType`, `parse_av1_sequence_header`,
  `parse_av1_frame_header_light`,
  `AdtsFrame`, `AacProfile`, `parse_adts_frame`,
  `Mpeg2AudioFrame`, `Mpeg2AudioVersion`, `Mpeg2AudioLayer`,
  `Mpeg2AudioChannelMode`, `parse_mpeg2_audio_frame`.
- New Rust public APIs: `tst_core::codec::{h264,h265,h266}::parse_slice_header_light`
  + `*SliceHeaderLight` structs + `*SliceType` enums (parity with AV1's
  existing `parse_frame_header_light`). H.266 returns sentinel `slice_type`
  / `pps_id` (deferred — full VVC syntax deferred per plan Task 4 note).
- 3 new fuzz harnesses in `crates/tst-core/fuzz/fuzz_targets/`
  (`fuzz_h264_slice_header_light`, `fuzz_h265_slice_header_light`,
  `fuzz_h266_slice_header_light`).
- `tstrans.exceptions.CodecError` + `CodecErrorKind` enum — 8-variant
  error hierarchy matching Rust `CodecParseError`.
- 12th bash ratchet `scripts/check-py-codec-error-mapping-coverage.sh`.

**Changed (BREAKING — pre-1.0):**

- `tstrans.mpegts.Sample.payload` changed type from `bytes` to one of
  `list[NalUnit]` (H.264 / H.265 / H.266) / `list[Obu]` (AV1) /
  `list[AdtsFrame]` (AAC) / `list[Mpeg2AudioFrame]` (MPEG-2 audio).
  Subtitle and AAC-LATM remain `bytes`. On audio parse failure mid-stream,
  payload falls back to `bytes` and `sample.codec_parse_error: CodecError`
  is populated.

**Internal:**

- `BASELINE` non_exhaustive count: 140 → 159 in `.github/workflows/ci.yml`.
- `tst-core` public-api baseline updated (+19 new entries for
  `*SliceHeaderLight` / `*SliceType` / `parse_slice_header_light` × 3 codecs).
- pytest count: 270 → 513 (2 skipped throughout).

---

### tst-py Phase 4 — Muxer wrap + KLV encoders (2026-05-23)

**Python bindings build path complete via 15 subagent-driven tasks**
(`8912bf5..aa09777`). Wraps the full `tst_core::mpegts::mux::Muxer`
surface (config family + push entries + handles + stats + draining
context-manager sink) and adds symmetric KLV encoders for ST 0601 /
0102 / 0605 / 0903. After Phase 4, a notebook can parse a `.ts` file
(Phase 2), modify KLV records (Phase 3), and re-mux to a new `.ts`
(Phase 4) — closing the round-trip use case from the parent spec.

**Added:**

- `tstrans.mpegts.{Muxer, MuxerConfig, MuxerConfigBuilder,
  MuxerProgramConfig, MuxerProgramConfigBuilder}` — 4-type config
  family mirroring Rust 1:1.
- `tstrans.mpegts.{KlvStreamType, Av1CarriageMode}` — pure-Python
  enums (`KlvStreamType.SYNCHRONOUS_METADATA / PRIVATE_DATA`;
  `Av1CarriageMode.MPEG2_TS_BINDING / INTEROP_RAW_OBU`, default
  `MPEG2_TS_BINDING`).
- `tstrans.mpegts.{StreamSpec, VideoStreamSpec, KlvStreamSpec,
  AudioStreamSpec, SubtitleStreamSpec}` — frozen dataclass tagged
  union over the streams in a program; Python 3.10+ match-statement
  compatible.
- `tstrans.mpegts.{VideoStreamHandle, AudioStreamHandle,
  KlvStreamHandle, SubtitleStreamHandle}` — opaque `u32`-backed
  PyO3 newtypes for handle-form pushes.
- `tstrans.mpegts.{MuxerStats, StreamCodecStats,
  VideoStreamCodecStats, KlvStreamCodecStats,
  AudioStreamCodecStats}` — stats accessors + per-stream tagged
  union (`Unknown` collapses to `None`).
- `tstrans.mpegts.{MuxerFileSink, MuxerDrainProxy}` +
  `Muxer.write_file(path)` — context manager that auto-drains
  after each push and flushes-and-closes on `__exit__`, never
  suppresses user exceptions.
- `tstrans.klv.{encode_uas_datalink,
  encode_uas_datalink_strict_compliance}` — ST 0601 encoders.
  `_strict_compliance` raises
  `KlvEncodeError(MISSING_MANDATORY_ITEM)` per ST 0601.8 §10.3.
- `tstrans.klv.encode_security` — ST 0102 encoder.
- `tstrans.klv.encode_precision_timestamp` — ST 0605 encoder
  (always returns 26 bytes).
- `tstrans.klv.{encode_vmti, encode_vmti_standalone}` — ST 0903
  encoders; `encode_vmti` emits LS body only,
  `encode_vmti_standalone` adds the SMPTE UL + BER-length prefix.
  `parse_klv_universal(encode_vmti_standalone(rec))` round-trips.
- `tstrans.exceptions.{MuxError, MuxErrorKind, KlvEncodeError,
  KlvEncodeErrorKind}` — 5-variant `MuxErrorKind` (`INPUT_MALFORMED
  / CONFIG_INVALID / INVALID_USAGE / BACKPRESSURE / INTERNAL`);
  8-variant `KlvEncodeErrorKind`.

**Push surface conventions (mirrors Rust 1:1 — arg order is
deliberately non-uniform across stream kinds):**

- `push_video(nal, pts, key_frame=False)`,
  `push_video_to(handle, nal, pts, key_frame=False)`,
  `push_video_to_with_dts(handle, nal, *, pts, dts, key_frame=False)`.
  No `pid` parameter — auto-resolves the lone video stream or
  raises `MuxError(INVALID_USAGE)` (Rust `AmbiguousTarget`).
- `push_audio(frames, pts)`,
  `push_audio_to(handle, pts, frames)`. **Arg order differs.**
- `push_klv(klv, pts, metadata_service_id=0)`,
  `push_klv_to(handle, klv, pts, metadata_service_id=0)`.
- `push_subtitle(pts, payload)`,
  `push_subtitle_to(handle, pts, payload)`. **Pts before payload.**
- Unknown handle → `MuxError(INVALID_USAGE)`; invalid payload →
  `MuxError(INPUT_MALFORMED)`; back-pressure →
  `MuxError(BACKPRESSURE)`.

**Verified:**

- Determinism sentinel: two fresh `Muxer` instances with the same
  config + same push sequence emit byte-identical output (4324
  bytes / 23 packets for the synthetic 1-video-1-klv fixture).
- 5/5 video + 5/5 klv input frames round-trip to 5 video + 5 klv
  events via `parse_file` (exact, not just within tolerance).
- 270 pytest tests + 2 skipped (was 178 after Phase 3 cleanup
  → +92 tests).

**Known follow-ups (NOT blocking Phase 5):**

- `add_subtitle` raises `NotImplementedError` — mux-side
  `SubtitleCodec` is a struct-variant Rust enum (per-variant
  fields for language / subtitling-type) incompatible with the
  Phase 2 flat `SubtitleCodec` Python string-enum. Deepening the
  Python representation is the work item.
- Full real-fixture round-trip needs config-from-probe
  reconstruction (currently the `tests/fixtures/local/`
  smoke-test is `pytest.skip`).
- NumPy zero-copy view of `pull()` output (Phase 6 pandas extra).
- `Muxer.write_to(io.BufferedWriter)` second sink shape (if a
  consumer asks).

**Build infrastructure:**

- BASELINE `#[non_exhaustive]` count bumped 135 → 140 in
  `.github/workflows/ci.yml`.

---

### Validate-1 act-now batch (plan #94, docs/plans/2026-05-22-validate-1-act-now-batch.md)

**Ten validate-1 carry-forward items closed via 7-worktree parallel SDD,
2026-05-22.** Shipped as 7 cherry-picks on `main` (`707f447..dc3a4b6`);
the other two of the act-now-12 list were verified-superseded at pre-flight
(ST0903-02 by Sprint 3 E5, M-05 by Sprint 2 B8). H264-RV2 was found
pre-shipped by Sprint 1 A8 `00bd703` during execution.

**Fixed:**

- **IMAPB `min < max` precondition guard** (commit `f36d533`, M-02). Encode
  and decode now reject `min >= max` up front with new variants
  `KlvEncodeError::InvalidImapbParams` and `KlvFieldError::InvalidImapbParams`
  rather than returning misleading bounds errors deeper in the call.
- **ST 0601 `encode_strict_compliance`** (commit `f36d533`, ST0601-NEW-01).
  Mirrors `decode_strict_compliance` on the encode side; new variant
  `KlvEncodeError::MissingMandatoryItem { tag, reason }` so producers can
  validate against ST 0601.8 §10.3 before emitting wire bytes.
- **Null PID skip in `cc_by_pid` tracking** (commit `656cecf`, Slice 06 M-02).
  Stuffing/null-PID packets no longer pollute the continuity-counter state
  machine, eliminating spurious `ContinuityJump` events under sparse PSI.
- **H.264 constraint flags consulted for `profile_idc=100` B-frame detection**
  (commit `dc3a4b6`, H264-RV4). `H264Sps::constraint_set_flags` is now a
  typed newtype; B-frame heuristics consult `constraint_set1_flag` per
  H.264 §A.2.1.
- **H.264 `frame_rate` `saturating_mul(2)` with None-on-saturation** (commit
  `dc3a4b6`, H264-RV7). Field-coded streams with extreme `num_units_in_tick`
  no longer panic on overflow; the iterator returns `None` for the affected
  frame and keeps walking.
- **H.265 `scaling_list` returns `EngineError` not `UnsupportedProfile`**
  (commit `46b2fd0`, H265-V1-M02). Scaling-list parse failures are
  surfaced as the parser-internal error type rather than misclassified as
  an unsupported codec profile; 3 conformance-fixture consumer sites
  updated.
- **AV1 `count_av1_obus` overflow safety** (commit `4b856a3`, TC-ROOT-V1-M2).
  `checked_add` replaces wrapping arithmetic; truncation surfaces as
  `None` rather than a wrap to zero.
- **`srt-sys` `SocketGuard` RAII for listener handle** (commit `707f447`,
  SS-V1-04). `encrypted.rs` listener tests no longer leak SRT sockets on
  early return; the guard wraps `srt_close` per-test.
- **`mock_transport` mutex poison recovery** (commit `738e9b2`, SS-V1-10).
  `unwrap_or_else(|e| e.into_inner())` lets a panicking test continue
  draining state from the shared mock instead of cascading the poison.

**Workspace updates:**

- BASELINE `#[non_exhaustive]` stays at 135 (additive variants on already-
  `#[non_exhaustive]` enums).
- `crates/tst-core/public-api.txt` baseline regenerated for
  `encode_strict_compliance` + 3 new error variants. `tst-pipeline` and
  `tst-srt` baselines unchanged.
- All 11 bash ratchets green.

**Follow-up (same day):** commit `46a454e` ported
`check-lifecycle-ffi-catch-coverage.sh` and `check-c-abi-rustdoc-coverage.sh`
to the portable `while IFS= read -r x; do arr+=("$x"); done < <(...)`
pattern (bash 3.2 / macOS), unblocking `macos-arm64` from gating
promotion (target 2026-05-30). First post-fix CI run had `macos-arm64`
pass cleanly for the first time since Sprint 3 D1.

### Validate-1 Phase 2 Sprint 4-5 follow-ups (docs/validate-1/15-sprint-4-5-review-codex.md)

**Five follow-up fixes from a 2026-05-20 Codex review of Sprints 4-5.**
Closed by 4 commits on `main` (`a0b0f8f`, `feffff8`, `361242a`, `d711ecb`).
Codex review at `docs/validate-1/15-sprint-4-5-review-codex.md`.

**Fixed:**

- **Stats wired to `frames_with_resync`** (commit `a0b0f8f`, follow-up #2).
  Sprint 4 G2 added the resync iterator but the `pes_emit` and
  `push_audio` stats sites were still calling strict `frames()`. The
  user-visible symptom Sprint 4 G2 named ("stats undercount on first
  parse error") is now actually fixed; this is the pattern documented in
  the `feedback_g2_pattern_plan_says_fix_symptom.md` memo.
- **`ManagedDemuxReceiver` data-loss budget rustdoc + no-dead-tail test**
  (commit `feffff8`, follow-up #3). Documents the reconnect drop budget
  (≤ `max_payload` bytes, typically ~7 TS packets, never an entire flow)
  and adds an integration test asserting no dead tail under repeated
  reconnect.
- **`tst-c` wires `ManagedDemuxReceiver` + `TST_EVENT_RECONNECT_DISCONTINUITY = 6`**
  (commit `361242a`, follow-up #1). New C entry points
  `tst_managed_demux_receiver_*` + new event kind exposed via the
  existing tagged-union event ABI. `TST_ABI_VERSION_MINOR` bumped 1 → 2
  (additive). Four pass-through delegates added on the managed wrapper:
  `stats`, `reset_stats`, `socket_stats`, `stream_codec_stats`.

**Workspace updates:**

- BASELINE `#[non_exhaustive]` stays at 135 (`TstEventKind` is not
  `#[non_exhaustive]` per existing convention).
- Docs-only follow-ups: 11-phase-2-plan.md Wave I SHIPPED + Sprint 5
  SHIPPED status blocks, 99-audit-summary.md H5/H6/H7/H8/H10/H11 cells
  closed, 13-interop-results.md acceptance-criterion correction.

### Validate-1 Phase 2 Sprint 5: Wave I empirical interop (docs/validate-1/11-phase-2-plan.md §2.6)

**Four empirical-interop fixtures from the Validate-1 audit's "validate
in the world" wave.** Shipped as 4 commits on `main`
(`774181c..e900779`) on 2026-05-20 via 4-worktree parallel SDD.

**Added (tests + harness only — no public-API delta):**

- **WebVTT / CEA-708 ignore-matrix** (commit `774181c`, I1). 32/32 ignore
  cells confirmed across ffprobe / tsp / tsanalyze / gst-launch tsdemux,
  empirically validating the H7 docs-only stance for these two subtitle
  codecs.
- **AV1 binding-conformant external decoder interop** (commit `ed80acd`,
  I2). ffmpeg / ffprobe accept the binding stream byte-identically to
  the legacy `Av1InteropRawObu` stream, validating the D-1 default
  (`Av1CarriageMode::Mpeg2TsBinding`).
- **KLV ST 1201.5 + ST 0903.6 spec vectors** (commit `f3fe8f9`, I3). 54
  tests covering 7/7 IMAPB substrate variants, `L ∈ 1..=8` bounds,
  Appendix A worked examples, and 22 BER-OID symmetry pairs across
  ST 0107.5 / 0601 / 0102 / 0903.
- **`scripts/cross-impl-byte-diff.sh`** (commit `e900779`, I5). 5-case
  content matrix comparing our output against tsduck `tsp` (byte-
  identical on all 5) and ffmpeg (diffs cosmetic only).

**Workspace updates:**

- BASELINE `#[non_exhaustive]` stays at 135 (tests + scripts only).
- No external receiver rejected any output under any test case.
- Plan-text gap surfaced (recorded in 13-interop-results.md): ST 0903.6
  §10.1.11/12 are Horizontal_FOV / Vertical_FOV worked examples, not
  incremental-update flows. If a different MISB doc was intended, file
  as a follow-up plan.

### Validate-1 Phase 2 Sprint 4: Waves F + G + H pipeline/codec/docs sweep (docs/validate-1/11-phase-2-plan.md §2.4 + §2.5)

**Twenty-one fixes (5 F + 3 G + 13 H docs sweep) from the Validate-1
audit's pipeline-correctness + codec-conformance + documentation
slices.** Shipped as 10 commits on `main` (`7275ae8..6182f02`) on
2026-05-20 via 8-worktree parallel SDD.

**Fixed (High):**

- **`ManagedDemuxReceiver` shell + `Demuxer::reset_sync` +
  `DemuxEvent::ReconnectDiscontinuity`** (commits `987c230` + `c969129`,
  F2; BREAKING — new public API). Sibling to `ManagedTransport` for the
  demux pipeline. On underlying-transport reconnect, the demuxer's
  syncer is reset and a `ReconnectDiscontinuity` event is emitted so
  consumers can mark a hard discontinuity in any downstream state.
  Reassembly tables (PAT/PMT, per-PID CC, last PTS) are preserved
  across reconnect.
- **`*_with_options(*Config)` → `*_with_config` rename** (commit
  `834a651`, F5; BREAKING). Workspace-wide constructor convention sweep:
  `Demuxer::with_options(DemuxerConfig)` → `Demuxer::with_config(DemuxerConfig)`,
  same pattern on `Pairer` and `io_file`. `DemuxReceiver::with_demux_options`
  is intentionally kept as a deferred rename (see `docs/conventions.md`
  outliers table).
- **`CodecParseError::UnsupportedFreeFormat` + `frames_with_resync()`**
  (commit `df51bdf`, G1 + G2). `bitrate_index == 0` (legal per ISO
  11172-3 but unsupported here) now surfaces a distinct variant rather
  than `ReservedValue`. New `frames_with_resync()` iterators on both
  `codec::mpegaudio` and `codec::aac::adts` walk past unparsable bytes
  to recover stream-wide stat accuracy.
- **`F2+F5` cross-worktree integration fix** (commit `6182f02`).
  `ManagedDemuxReceiver` was constructing the demuxer via the
  post-F5-renamed `Demuxer::with_config` after the rename landed on a
  separate worktree; integrated fix-up applied per
  `feedback_cherry_pick_build_between_parallel_worktrees.md`.

**Fixed (Medium):**

- **ADTS `profile = 3` gated on MPEG-2/-4 ID bit** (commit `12fb3d4`, G3).
  `profile = 3` is the SSR (Scalable Sample Rate) profile for MPEG-2 AAC
  but reserved for MPEG-4 AAC; the iterator now consults the `ID` bit
  and rejects vs. accepts accordingly.
- **`MuxSender::Drop` gated on `!closed`** (commit `5869573`, F4). The
  drop impl no longer double-cancels an already-closed sender; idempotent
  by construction.

**Documentation:**

- **`Transport::close` vs `RecvTransport::close` asymmetry** (commit
  `7275ae8`, F3). Rustdoc clarifies that the send-side `close` waits for
  outbound queue drain while the receive-side `close` is immediate;
  asymmetry was real but undocumented.
- **`max_unsynced_bytes` is diagnostic-only** (commit `c942d6d`, F1).
  Clarifies the threshold is a warning, not a fail-fast — a long
  unsynced run is logged but doesn't terminate the receive loop.
- **Wave H one-shot docs sweep (H1-H13)** (commit `ceb54a5`). Thirteen
  small docs corrections / consistency fixes across guides and examples;
  scrub-guard regex extended to forbid `srt-c` (which would catch a
  regression to the inner-workspace shape).

**Workspace updates:**

- BASELINE `#[non_exhaustive]` bumped 134 → 135 (one new
  `ManagedDemuxReceiverConfig` in `tst-pipeline`).
- `crates/tst-pipeline/public-api.txt` baseline regenerated (new
  `ManagedDemuxReceiver` + `ManagedDemuxReceiverConfig` + new variant on
  `DemuxEvent`).

### Validate-1 Phase 2 Sprint 1-3 review follow-ups (docs/validate-1/14-sprint-1-3-review-codex.md)

**Three follow-up fixes from a 2026-05-20 Codex review of Sprints 1-3.**
Codex re-reviewed the response and corrected the Sprint 3 BASELINE wave
attribution per `feedback_baseline_attribution_verify_via_ci_yml_diff.md`.

**Fixed:**

- **A4 bounded-PES residual-discard rationale clarified** (commit
  `c351b1f`). Rustdoc now records why tail residual is dropped along
  with per-PID state (Option B per Sprint 1 plan) rather than emitted as
  a malformed-PES diagnostic.
- **Descriptors module docs cover Result-returning builders** (commit
  `1631123`). `mpegts::descriptors` rustdoc previously documented only
  the parser path; now covers the build-side error subset.
- **AV1 per-OBU framing fix** (commit `9f83250`, extending Sprint 2 C8
  chain). The first AV1 OBU in a Frame carrier was getting the spec
  3-byte start code but subsequent OBUs were not; per-OBU framing now
  unconditionally emits the 3-byte prefix.

**Workspace updates:** BASELINE `#[non_exhaustive]` stays at 134 (no
new variants).

### Validate-1 Phase 2 Sprint 3: Waves D + E FFI hardening + KLV strict-compliance (docs/validate-1/11-phase-2-plan.md §2.4)

**Twelve FFI + KLV-encode fixes from the Validate-1 audit.** Shipped as
19 commits on `main` (`5813c72..566789b`) on 2026-05-20 via 11-worktree
3-phase parallel SDD.

**Fixed (High):**

- **C ABI lifecycle entries wrapped in `ffi_catch`** (commits `94cdfaf`
  + `feff2ff`, D1). `_close` and `_cancel` entries previously bypassed
  panic-catch; a panicking shell on close could unwind across the FFI
  boundary as undefined behaviour. New (11th) bash ratchet
  `check-lifecycle-ffi-catch-coverage.sh` ratchets this against
  regression.
- **`TransportError` carries optional typed errno source** (commits
  `76e89fa` + `4f3a1e0`, D5). `TransportError::*` now embed an
  `Option<TypedErrnoSource>` so FFI bindings can propagate the libsrt /
  POSIX errno code distinctly from the message string. New trait method
  `ShellError::errno_code()`.
- **DTS + PTS migrated to `Option<Pts90khz>`** (commits `07641e0` +
  `d169771`, D4; BREAKING). Previously both lived as `Option<i64>` on
  `PesPayload` — now typed at the API boundary.
- **`sizeof` guards for 7 `#[repr(C)]` stats structs** (commits `a3ce423`
  + `6c6c87a`, D2). New `static_assert(sizeof(struct) == EXPECTED)` per
  struct in the cbindgen-generated header, catching silent layout drift
  before bindings link.

**Fixed (KLV encode strictness — Wave E):**

- **ST 0601 strict-mode duplicate-tag + canonical BER walker** (commit
  `76361ed`, E1 + E2). Strict decode now rejects duplicate tags and
  non-canonical BER length encodings.
- **ST 0601 encode reserved-tag filter** (commit `5f6ddd9`, E3). New
  variant on the unknown-tag enum; reserved tags filtered before
  serialisation.
- **ST 0102 BER-OID encode for unknown tags** (commit `20d1038`, E4).
  Symmetric with the BER-OID decode path added in Sprint 2.
- **ST 0903 BER-OID local-set walker + `VTargetPack` inner walk**
  (commits `031b3c4` + `566789b`, E5). VMTI local sets and per-target
  packs now traverse BER-OID encoded tags correctly; nested LSes still
  pass-through.

**Fixed (other Wave D items):**

- **`SrtCancelHandle` module docstring** (commit `489eaf6`, D3). Clarifies
  that the handle is the canonical cross-thread shutdown primitive and
  that `is_cancelled()` is advisory.
- **`MockRecvTransport` with `FailMode` fixtures** (commits `1197b61` +
  `11cc28f` + `c27d2ff`, D7). New test helper with deterministic failure
  injection (Closed, Broken, Backpressure) for receive-side reconnect
  tests.
- **`srt-sys` cdylib `--exclude-libs=ALL` on Linux** (commit `5813c72`,
  D6). Symbol-hygiene fix preventing vendored mbedTLS / libstdc++
  symbols from leaking into `libtstrans.so`'s dynamic symbol table.

**Workspace updates:**

- BASELINE `#[non_exhaustive]` bumped 131 → 134 (+3: Wave D additions;
  Wave E additions ride on already-`#[non_exhaustive]` enums).
- `crates/tst-core/public-api.txt` baseline regenerated (`Pts90khz` on
  DTS, new `ShellError::errno_code` trait method, new KLV-encode
  variants, new `MockRecvTransport`).
- 11 bash ratchets (10th was added in plan #93; 11th
  `check-lifecycle-ffi-catch-coverage.sh` added here).

---

### Validate-1 Phase 2 Sprint 2: Waves B + C demux & mux conformance (docs/validate-1/11-phase-2-plan.md §2.2 + §2.3)

**Sixteen demux-correctness, mux-conformance, and FFI-hardening fixes from the
Validate-1 Phase 1 audit (Codex + Claude reports at `docs/validate-1/`).**
Shipped as 20 commits on `main` (`43545ef..2d73294`) on 2026-05-19/20 via
parallel `superpowers:subagent-driven-development` background controllers
across 13 isolated git worktrees, with sequential rebase-and-merge to keep
linear history.

**Fixed (High):**

- **Multi-program PCR global tracking** (commit `43545ef`, B1, Codex TS-TIME-01).
  Replaced single `last_pcr_27mhz: Option<u64>` field with
  `last_pcr_by_pid: HashMap<u16, u64>` so each program's PCR PID has its own
  time base. Multi-program TS no longer produces false `PcrAnomaly` events.
- **Mux PSI multi-program backpressure** (commit `577abb1`, B2). New
  `Muxer::psi_packets_due` helper centralises reservation math (was hardcoded
  `2` in all 4 push paths; correct shape is `1 + programs.len()`).
  `maybe_emit_psi` now writes `psi_last` for ALL programs on emit.
- **PUSI pointer_field continuation + N-of-M sync re-acquisition**
  (commits `07bbed8` + `b2ceab3`, B3+B7 + followup). PSI assembler now
  honours `pointer_field` continuation bytes (prior section completed first,
  then new section started). Sync re-acquisition uses ffmpeg's 5-of-7
  188-byte-stride validation, no longer false-syncs on isolated `0x47` bytes.
  Mid-stream-join scenario regression-test added in followup.
- **DVB-sub data_identifier strict mode** (commit `ac4db93`, C10,
  Codex 02 #6). Strict mode rejects `data_identifier != 0x20`; lenient
  emits sample + `NonConformantIssue::DvbSubDataIdentifier { observed }`.
- **SRT payload size threading** (commit `a0d7d24`, C1, Codex SRT-01).
  New `Socket::payload_limit() -> usize` returns post-handshake
  `SRTO_PAYLOADSIZE`. `SrtTransport::new` queries it instead of hardcoding
  `1316`. URL `payloadsize=1456` now actually takes effect.
- **OverflowPolicy::Reject surface** (commit `3be1096`, C2, Codex PIPE-01).
  `GapBufferError::Full` now maps to `TransportError::Backpressure("gap buffer full")`
  instead of silent `let _ = gap.enqueue(...)`. Counter-test guards
  `DropOldest` continues silent-evict per its contract.
- **PES header validation + PTS anomaly + subtitle alignment**
  (commit `1cc0653`, B4+B5+B6). `NonConformantIssue::{PtsAnomaly,
  MissingRequiredPts, PesHeaderMalformed, SubtitleAlignmentMissing}`
  variants + `PesHeaderMalformedKind` enum. PTS no longer poisons
  `last_pts_by_pid` when absent. Forbidden `PTS_DTS_flags = 0b01` rejected.
  DVB-sub/teletext PES missing `data_alignment_indicator` surfaces issue.
- **PCR-only adaptation-field injection** (commit `a2445f8`, C3,
  Codex TS-TIME-02). New `Muxer::maybe_emit_pcr_only` injects PCR-only
  TS packets on the PCR PID when no payload arrives within
  `pcr_interval_ms`. Honours H.222.0 Annex D 100ms cap when video/audio
  frame intervals exceed it. CC of PCR-only packets does NOT increment.
- **PesPtsField::PtsAndDts + Annex-B AU validation** (commit `8938ca7`,
  C4+C13). New `Muxer::push_video_to_with_dts(handle, nal, pts, dts, key_frame)`
  API emits `PTS_DTS_flags = 0b11` + correct 5-byte PTS + DTS with marker
  prefixes (`0b0011` PTS, `0b0001` DTS). B-frame reordered video now muxes
  correctly. `validate_annex_b` rewritten as structural NAL walker
  (rejects empty NALs + malformed start-code structure).
- **AC-3 mandatory audio descriptor + syncframe parser** (commit `0ead2f9`,
  C6+C12, Codex AUDIO-01/04). New `codec::ac3` module + `Ac3SyncInfo`
  struct + `parse_syncframe` API per A/52 §5.4.1. Muxer auto-emits
  `ac3_audio_stream_descriptor` (tag 0x81) for `AudioCodec::Ac3`. Demuxer
  emits `NonConformantIssue::Ac3SyncMissing` when
  `data_alignment_indicator=1` but payload doesn't start at `0x0B77`.
- **AAC PCE channel layout + LATM/LOAS sync** (commit `c9835b9`, C7+C11,
  Codex AUDIO-02/03). New `AacChannelLayout::{PceDefined, Channels(u8)}`
  enum — `decode_channels(0)` returns `PceDefined` instead of error.
  Iterator no longer terminates on PCE-prefixed frames. New
  `codec::aac::latm` module validates LOAS syncword
  (`0x2B7` 11-bit pattern) + audioMuxLengthBytes per ISO/IEC 14496-3 §1.7.
- **AV1 binding-conformant mode + AV01 reg first** (commits `5394c00`
  + `78d9b8e` + `2d73294`, C8+C9). New `Av1CarriageMode::{Mpeg2TsBinding,
  InteropRawObu}` enum, default `Mpeg2TsBinding`. Mux emits
  `stream_id=0xBD` + `ts_open_bitstream_unit` framing with spec-correct
  3-byte `[0x00, 0x00, 0x01]` start code + emulation prevention bytes
  (escape rule covers `b ∈ {0x00, 0x01, 0x02, 0x03}` after `0x00 0x00`).
  Demux unwraps binding bytes + surfaces `Av1WrongStreamId` /
  `Av1MissingTsObuFraming` diagnostics. PMT descriptor cache reorders
  caller-supplied AV01 registration descriptor to position 0.

**Fixed (Medium):**

- **AV1 implicit color_range bit** (commit `dd42c33`, B11). `ColorInfo`
  populated unconditionally on well-formed sequence headers — the
  `color_range` wire bit was being read but discarded when
  `color_description_present_flag=0`.
- **NAL/OBU header validation + AV1 uvlc cursor fix** (commit `5d47391`,
  B9+B10). `NonConformantIssue::{NalHeader, Av1ObuHeader}` variants +
  `NalHeaderKind` + `Av1ObuHeaderKind` enums. H.264/265/266 `forbidden_zero_bit`,
  H.265/266 `temporal_id_plus1!=0`, H.266 `reserved_zero_bit` and
  `layer_id ∈ 0..=55` constraints enforced. H.266 `ReservedBit` and
  `LayerIdOutOfRange { id > 55 }` NALs unconditionally dropped per spec
  mandate. AV1 `uvlc()` now consumes the trailing 1-bit marker even on
  the 32-leading-zeros sentinel path.
- **PAT cleanup + PCR field validation** (commit `7752ec8`, B8+B12).
  On PAT change, all per-PID state (cc_by_pid, last_pts_by_pid,
  last_pcr_by_pid, reassembly state, stream_kind_by_pid, pid_to_program,
  PSI assemblers) cleared for removed programs' PIDs. PCR validation:
  reserved 6 bits + extension ≤ 299 per H.222.0 §2.4.3.5.
  `NonConformantIssue::PcrMalformed { kind }` + `PcrMalformedKind` enum.

**Fixed (Medium, breaking — pre-1.0 per `feedback_break_freely_prerelease.md`):**

- **Descriptor builders return Result** (commit `f88f036`, C5,
  Codex 02 #5). `descriptors::{registration, user_private,
  user_private_with_tag, component}` now return
  `Result<Vec<u8>, DescriptorError>` instead of silently truncating via
  `body_len as u8`. `DescriptorError::TooLarge { tag, len, max }`
  variant added. `registration()` body cap corrected 251→255 (additional
  cap 247→251) per spec H.222.0 §2.6. 10 caller sites updated in
  `tst-core`, examples, docs.

**Public API changes (pre-1.0, recorded — see `crates/tst-core/public-api.txt`):**

- New `NonConformantIssue` variants (12 total across the sprint).
- New `tst-core` modules: `codec::ac3` (with `Ac3SyncInfo` + `parse_syncframe`),
  `codec::aac::latm` (with `parse_latm` + `LatmFramingKind`).
- New `tst-core` enums: `AacChannelLayout`, `PesHeaderMalformedKind`,
  `PcrMalformedKind`, `NalHeaderKind`, `Av1ObuHeaderKind`,
  `Av1CarriageMode`, `DescriptorError` (and its `TooLarge` variant).
- New `MuxerConfig::av1_carriage` + `DemuxerConfig::av1_carriage` config
  fields with corresponding builder setters.
- New `Muxer::push_video_to_with_dts` + `MuxSender::send_video_to_with_dts`
  methods for B-frame-reordered video.
- `Socket::payload_limit() -> usize` exposed on `tst-srt`.
- `AdtsFrame.channels: u8` field renamed to
  `AdtsFrame.channel_layout: AacChannelLayout` with `.channels() -> Option<u8>`
  accessor.

**Infrastructure / CI:**

- `#[non_exhaustive]` BASELINE bumped 114 → 131 across the sprint
  (`.github/workflows/ci.yml`). 9 new `#[non_exhaustive]` types contributed:
  `PesHeaderMalformedKind`, `PcrMalformedKind`, `NalHeaderKind`,
  `Av1ObuHeaderKind`, `AacChannelLayout`, `LatmFramingKind`,
  `Av1CarriageMode`, `Ac3SyncInfo`, `DvbSubStripResult`. The remaining
  +8 are comment/rustdoc mentions counted by `rg -c` per
  `feedback_baseline_count_projection_undercount.md`.
- New `.gitignore` entry `/.worktrees/` (commit `145c46b`) enables
  parallel-subagent worktree isolation per
  `feedback_per_subagent_worktree_for_parallel_code_changes.md`.
- C ABI variant codes 21-31 assigned to new `TstNonConformantCode`
  entries; `tstrans.h` regenerated.

**Sprint 2 execution shape:** Phase 1 (5 parallel worktrees) → Phase 2
(4 parallel) → Phase 3 (4 parallel) → 2 follow-up fixes (B3+B7 mid-stream-join
bug + C8+C9 wire-format spec-conformance fix in 2 commits). Per-item
two-stage review (spec compliance + code quality) before merge; 4 items
landed APPROVED_WITH_NOTES with minor polish deferred; 2 items required
implementer-iteration fix cycles (B3+B7 critical bug, C8+C9 wire format).

Closeout memory: `project_validate_1_sprint_2_shipped.md`.
**Sprints 3-5 (Waves D/E/F/G/H/I) remain pending** — see
`docs/validate-1/11-phase-2-plan.md` for the per-wave dispositions.

---

### Validate-1 Phase 2 Sprint 1: Wave A wire-format & UB fixes (docs/validate-1/11-phase-2-plan.md §2.1)

**Eight wire-format / UB / parser-correctness fixes from the Validate-1
Phase 1 audit (20 Claude slices + 8 Codex reports at `docs/validate-1/`).**
Shipped as 8 commits on `main` (`3cd175e..9c29400`) on 2026-05-19/20.

**Fixed (Critical):**

- **DVB teletext PES_packet_length truncation** (commit `dbd0cbb`, Phase 2
  plan §A1, Codex 02 #1). For payloads near the previous cap (65490),
  the writer padded to `N*184` then emitted `(N*184 - 6) as u16` which
  silently wrapped modulo 65536 — conformant demuxers mis-framed the PES
  and the downstream subtitle stream corrupted. New
  `dvb_teletext_total_pes_bytes(payload_len, auto_prepend)` helper
  pre-validates against `u16::MAX`; max payload tightened to 65458
  (auto-prepend) / 65459 (caller-supplied data_identifier). 6 boundary
  tests.

**Fixed (High):**

- **C ABI `slice::from_raw_parts(null, 0)` UB safety** (commit `3cd175e`,
  Phase 2 plan §A3, Codex CABI-01). The pre-existing `(NULL, len > 0)`
  guard missed the `(NULL, 0)` case — Rust requires non-null pointer
  even for zero-length slices. New `tst-c/src/ffi_slice.rs`
  with `pub(crate) ffi_slice(ptr, len, name) -> Result<&[u8], i32>`
  applied to 28 sender-side data-path call sites. 4 contract tests.

- **C event arena lifetime correctness** (commit `e2958be`, Phase 2 plan
  §A2, Codex CABI-02 material omission). Event-payload pointer fields on
  `TstEvent` (audio/subtitle/unknown sample, metadata, NAL/OBU, PMT
  descriptor) referenced the input `DemuxEvent` Vec's storage instead of
  the arena's — dangling after `_recv_event` returned. Extended
  `EventArena` with `payload_buf: Vec<u8>` + two-pass collect-then-
  resolve for multi-payload events. 7 inline tests assert C pointers do
  NOT alias input Vec pointers.

- **Bounded PES reassembly tail bytes** (commit `c6acf84`, Phase 2 plan
  §A4, Codex VIDEO-03). Length-driven completion was taking the whole
  reassembly buffer via `std::mem::take`, including bytes past the
  declared `PES_packet_length`. Replaced with
  `part.buf.drain(..total).collect()`; residual dropped along with
  per-PID state (option B per plan). 2 new regression tests.

- **H.266 `walk_ref_pic_list_struct` AbsDeltaPocSt predicate** (commit
  `568c6cf`, Phase 2 plan §A5, Claude slice 11 H266-V1-H1). Walker used
  an `inter_layer_ref_pic_flag`-shaped predicate falsely attributed to
  §7.4.9; spec per H.266 V4 §7.4.9 eq.(150) +
  ffmpeg `vvc/refs.c:522-526` is
  `!((sps_weighted_pred_flag || sps_weighted_bipred_flag) && i != 0)`.
  Cascaded into 1-bit cursor drift on streams with multi-entry RPS where
  `abs_delta_poc_st == 0` at `i ≥ 1`. Helper signature gains the two
  flags; dead `prev_use_ref_pic_list` tracking removed. RED test:
  `TruncatedRbsp { offset_bits: 240 }` (the exact drift).

- **ST 0601 high-numbered tag narrowing** (commit `3600fd3`, Phase 2 plan
  §A6, Codex 03 #3). `apply_typed_tag` called `lookup(tag as u8)` where
  `tag: u32`; future BER-OID tag 258 narrowed to tag 2 (Precision Time
  Stamp Pack) and overwrote `record.timestamp_us`. Option B fix:
  `u8::try_from(tag)` at call site, matching the ST 0102 precedent at
  `klv/st0102/decode.rs:117-128`. 3 regression tests.

- **H.264 PPS `seq_parameter_set_id` range + PPS→SPS cross-validation**
  (commit `00bd703`, Phase 2 plan §A8, Claude slice 09 H264-RV1). PPS
  parse accepted SPS-ID ∈ [0, 255] vs H.264 V15 §7.4.2.2 mandate
  [0, 31]. Adds `CodecParseError::ReservedValue` for out-of-range.
  `parse_parameter_sets` now drops orphan PPS with `tracing::warn!` if
  the referenced SPS isn't in the map. 4 new tests.

- **IMAPB decode special values + decoded bounds check** (commit
  `9c29400`, Phase 2 plan §A7, Claude slice 01 H2 + H3). Decoder didn't
  implement ST 1201.5 §7.2.2 step 1 special-value detection
  (`0xC8...` +∞, `0xD0...` NaN, `0xE0...` BelowMin, `0xE1...` AboveMax
  silently decoded as garbage normal floats) and didn't bounds-check
  against `[min, max]` (`IMAPB(0,100,3)` wire `0x800000` decoded ~128.0).
  New `pub enum DecodedImapb { Value(f64), PositiveInfinity,
  NegativeInfinity, NaN, BelowMin, AboveMax, ReservedSpecial { raw },
  OutOfRange { decoded } }` with `#[non_exhaustive]`. `value()`
  ergonomic accessor returns `Some(f64)` only for `Value`. Cascade:
  3 KLV consumers + 1 proptest. 9 new tests.

**Workspace updates:**

- BASELINE non_exhaustive in `.github/workflows/ci.yml` bumped 113 → 114
  (one new `#[non_exhaustive]` on `DecodedImapb`).
- `crates/tst-core/public-api.txt` baseline regenerated (additive change
  for `DecodedImapb` enum + variants + impls + `decode_imapb` return
  type).

**Note on remaining Wave A items:** None. Sprint 1 closed all 8 Wave A
items end-to-end. Sprints 2-5 (Waves B-I) cover the remaining ~62
Medium + ~110 Low findings + the empirical interop test suite.

### Codex Waves 1-6 re-review fixes (docs/plans/2026-05-19-codex-waves-1-6-rereview-fixes.md)

**Three follow-up fixes from a 2026-05-19 Codex comprehensive re-review of
Waves 1-6** (`docs/refactor-1/_codex-waves-1-6-comprehensive-rereview-report.md`),
performed after plan #92 closed the first round of Codex Wave 6 findings:

**Fixed:**

- **C ABI `TstError::NotAvailable` / `TstError::NotFound` now record fresh
  last-error state before returning.** 17 C ABI sites in `bindings/c/src/`
  (12 NotAvailable socket_stats accessors + 5 NotFound per-PID codec_stats
  accessors) returned the negative code via `TstError::Foo as i32` without
  calling `set_last_error()` first, leaving stale message visible to
  `tst_get_last_error()`. Each site now uses `record_not_available(msg)` or
  `record_not_found(msg)` — new `pub(crate)` helpers in
  `bindings/c/src/error.rs` paired with the existing `record_shell_error`
  / `record_mux_error` / `record_eos` family. 4 new unit tests prove the
  helpers overwrite prior unrelated last-error state.

- **Single-stream KLV C ABI entry points now document the raw-LS-bytes /
  no-AU-cell-pre-wrap contract.** `tst_muxer_push_klv`,
  `tst_mux_sender_send_klv`, and `tst_managed_mux_sender_send_klv` had
  thin or absent rustdoc, leaving binding authors at risk of pre-wrapping
  the 5-byte `Metadata_AU_cell` header (which the muxer auto-prepends for
  `SynchronousMetadata` streams per ITU-T H.222.0 V9 §2.12.4.2 —
  double-wrapping produces unparseable metadata). New rustdoc on each
  entry mirrors the contract documented in
  `memory/reference_klv_au_cell_caller_responsibility.md`. Regenerated
  `tstrans.h` propagates the new blocks into the MUX SENDER section.

- **User-facing docs refreshed for Waves 2-4 API renames.** `guide-mpegts-mux.md`'s
  `push_video` / `push_klv` signature box updated from raw `i64` PTS to
  `Pts90khz` (Wave 2 typed boundary); its `push_klv` example updated to
  include the now-required `metadata_service_id: u8` 3rd arg.
  `guide-klv.md` updated from `EncodeOptions` to `EncodeConfig` (Wave 2's
  `*Options→*Config` rename). `architecture.md`, `guide-pipeline.md`, and
  `guide-mpegts-demux.md` updated from `pipeline::pairing` to
  `tst_pipeline::ext::pairing` (Wave 4 module move). Plus a bonus stale
  `pts_to_duration(pts_90khz: i64)` signature in `guide-mpegts-demux.md`
  caught and updated to `Pts90khz`. Historical references in
  `deferred-features.md` and the "potential" cross-reference at
  `guide-mpegts-demux.md:409` intentionally left as-is — these describe
  the deferral itself.

**New CI ratchet:**

- `scripts/check-no-direct-not-available-not-found-cast.sh` (the **10th**
  bash ratchet) forbids the `TstError::NotAvailable as i32` /
  `TstError::NotFound as i32` direct-cast pattern in `bindings/c/src/`.
  Excludes `bindings/c/src/error.rs` (where the helpers' own bodies
  legitimately contain the cast paired with `set_last_error`). Wired
  into `.github/workflows/ci.yml` alongside the existing 9.

**Public API impact:**

- Zero public Rust API delta on all 3 ratcheted crates (`tst-core`,
  `tst-pipeline`, `tst-srt`). New helpers are `pub(crate)`.
- `#[non_exhaustive]` BASELINE in CI: unchanged at 113.
- `tstrans.h` byte delta: +~90 lines from the 3 new KLV docstring blocks
  (cbindgen propagates Rust rustdoc into the header), 0 symbol changes.

**Test coverage:** 4 new unit tests in `bindings/c/src/error.rs` covering
the 2 new helpers' code-and-message overwrite behavior. All 10 bash
ratchets green. All 3 cargo-public-api baselines clean.

---

### Codex Wave 6 validation fixes (docs/plans/2026-05-19-codex-wave-6-validation-fixes.md)

**Three follow-up fixes to Wave 6 sign-off**, surfaced by a 2026-05-19 Codex static
review of the shipped Wave 5.B + 6.A + 6.D implementations
(`docs/refactor-1/_codex-wave-6-implementation-validation.md`):

**Fixed:**

- **C header section dividers no longer repeat.** Wave 5.B's `add_section_dividers`
  post-process in `bindings/c/build.rs` walked cbindgen's name-sorted output
  line-by-line and emitted a divider on every classified-section transition.
  With `cbindgen.toml` `sort_by = "Name"`, alphabetic symbol order interleaved
  domains (`tst_clear_*` → INTROSPECTION, `tst_demux_*` → DEMUX RECEIVER,
  `tst_get_*` → INTROSPECTION again, etc.), producing 16 dividers with 7
  domain sections each appearing twice. Replaced with **chunk-then-group-then-emit**:
  pass 1 buffers each doc-comment + declaration block classified by section;
  pass 2 emits header content verbatim, then iterates 7 required sections +
  2 conditional catch-alls in declared order, emitting each at most once.
  Result: `bindings/c/include/tstrans.h` now has 9 dividers (7 required +
  LIFETIME + OTHER), matching the original Wave 5.B spec. Implementation
  required two adaptations beyond the plan's sketch: multi-line declaration
  absorption (cbindgen wraps long parameter lists) and a trailer bucket
  (`} // extern "C"` + `#endif` + `_TST_ABI_ASSERT` block must emit AFTER
  sections, not before). `bindings/c/tests/header_drift.rs` carries a
  mirror copy of `add_section_dividers` (intentional — build.rs runs
  pre-compile and cannot import from `tst_c::`); both copies updated in
  lock-step and enforced byte-identical by the existing drift test.

- **`record_mux_error` wildcard for unknown future `MuxSenderErrorKind`
  variants now maps to `TstError::Internal` (was `InvalidConfig`).** Aligns
  with the adjacent `record_shell_error` wildcard at `tst-c/src/error.rs:180`
  (`Internal`) and with `MuxError::kind()`'s wildcard at
  `tst-core/src/error.rs:631` (`Internal`). Rationale: an unknown future
  coarse kind is more truthful surfaced as a library/runtime failure than
  as caller-side `InvalidConfig`. Behavior change is in the future-only
  path — no current variant takes the wildcard.

**Changed:**

- **`mpegts::mux::mod.rs` shrunk from 629 LoC to 320 LoC** (Wave 6.A
  follow-up). `Muxer::new` is now a thin ~50-LoC coordinator; per-program
  state collection, PCR PID resolution, PMT descriptor cache construction,
  and per-stream stats initialization moved to 4 new `pub(super)` helpers
  in `mpegts::mux::state`:
  - `collect_stream_states(prog) -> (Vec<Video>, Vec<Klv>, Vec<Audio>, Vec<Sub>)`
  - `resolve_pcr_pid(prog) -> u16`
  - `build_pmt_descriptor_cache(prog) -> Vec<Vec<u8>>`
  - `initialize_stats(prog, &video, &klv, &audio, &subtitle, &mut into)`

  Final `state.rs`: 445 LoC (was 96). **Zero public Rust API delta**
  (`cargo public-api -p tst-core --simplified` baseline byte-identical).
  **Zero `#[non_exhaustive]` BASELINE delta** (stays 113). Zero behavior
  change — mechanical extraction with all 761 tst-core tests + workspace
  suite green.

**New CI ratchet:**

- `scripts/check-c-header-section-uniqueness.sh` (the **9th** bash ratchet)
  asserts `tstrans.h` has 7-9 dividers AND all section names are unique.
  Guards against regression to the pre-fix line-by-line transition-emission
  shape. Wired into `.github/workflows/ci.yml` alongside the existing 8.

**Public API impact:**

- `cargo public-api` baselines for `tst-core` / `tst-pipeline` / `tst-srt`:
  byte-identical to pre-plan.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml`: unchanged at 113.
- `tstrans.h` byte delta: cbindgen output reordering (sort-by-name groups
  now travel as section blocks instead of interleaved) + 9 divider lines
  emitted in canonical order instead of 16 in transition order.

**Test coverage:** no new tests added — the byte-identity header drift test
(`bindings/c/tests/header_drift.rs`) covers the post-process change end-to-end;
existing muxer roundtrip + descriptor + per-stream-class test suites cover
the `Muxer::new` extraction behavior-equivalence. All 9 bash ratchets green;
all 3 cargo-public-api baselines clean.

---

### Wave 6.D `MuxError` two-tier reshape (docs/plans/2026-05-19-wave-6-muxerror-reshape.md)

**Breaking change (tst-core / tst-c — new public surface, C routing simplified):**

- **`MuxSenderErrorKind` enum added** (`tst_core::error::MuxSenderErrorKind`,
  `#[non_exhaustive]`) — 5 coarse categories for the inner (muxer-specific)
  error tier: `InputMalformed`, `ConfigInvalid`, `InvalidUsage`, `Backpressure`,
  `Internal`. Complements the outer `tst_pipeline::ShellErrorKind` (6 variants,
  shell-agnostic) without overlapping it.

- **`MuxError::kind()` method added** — `pub fn kind(&self) -> MuxSenderErrorKind`
  with an exhaustive 32-arm match over every `MuxError` variant, categorizing
  each to its canonical inner-tier kind. Bindings that need coarse routing
  (e.g. "is this a caller bug or a data problem?") can call `kind()` rather than
  matching the full 32-variant set.

- **`mpegts::mux::_detail` module added** — `pub mod _detail { pub use
  crate::error::MuxError; }`. The underscore prefix signals spec-domain tier:
  bindings that need to match individual `MuxError` variants for diagnostic
  output import via `use tst_core::mpegts::mux::_detail::MuxError;`, making
  the non-default, high-specificity import path legible at the use site.

- **`record_mux_error` rewritten** (`tst-c/src/error.rs`, 189 → ~75 LoC) —
  two per-variant overrides kept explicit (`InvalidNal` → `TST_E_INVALID_NAL`,
  `KlvTooLarge` → `TST_E_KLV_TOO_LARGE`); all remaining 30 variants routed via
  `e.kind()` pattern match. Error messages now come from `e.to_string()` (the
  `#[error]` attribute) rather than duplicated per-arm format strings.

- **New CI ratchet** — `scripts/check-mux-error-kind-coverage.sh` verifies
  every `MuxError` variant is matched explicitly in `MuxError::kind()` before
  the `_ => Internal` wildcard arm. Registered in `.github/workflows/ci.yml`
  alongside the existing 3 error-coverage ratchets.

**Public API impact:**

- `cargo public-api -p tst-core --simplified` baseline refreshed: +1 enum
  (`MuxSenderErrorKind`, 5 variants + trait impls), +1 method
  (`MuxError::kind`), +1 module (`mpegts::mux::_detail` with all 32 `MuxError`
  re-exports).
- `cargo public-api -p tst-pipeline --simplified` byte-identical to pre-plan.
- `cargo public-api -p tst-srt --simplified` byte-identical to pre-plan.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` bumped **105→111**
  (empirical; `rg -c` counts attribute instances + comment-line mentions).

**Test coverage:** 34 new integration tests in
`crates/tst-core/tests/mux_error_kind_routing.rs` — one assertion per `MuxError`
variant routing plus 2 kind-property tests. All 8 bash ratchets green.

---

### Wave 6.B `mpegts/demux/demuxer.rs` god-module split (docs/plans/2026-05-19-wave-6b-demuxer-split.md)

**Refactor (purely internal — zero public API change, zero `#[non_exhaustive]` BASELINE delta):**

- `demuxer.rs` 3584 → ~2312 LoC. The coordinator now contains only the
  public surface methods (`new`, `with_options`, `feed`, `feed_aligned`,
  `next_event`, `flush`, `stats`, `reset_stats`, `stream_codec_stats`), the
  `Demuxer` struct definition, and the thin private dispatch helpers
  (`process_packet`, `handle_process_packet_result`, `lookup_stream`,
  `program_number_for_pid`). All struct fields are `pub(super)`.

- **5 new sibling submodules** extracted from `demuxer.rs`:
  - `sync_ingress.rs` — byte-stream sync recovery, PCR gap tracking,
    continuity-counter validation.
  - `pmt_classify.rs` — PMT stream-type classification and `StreamKind`
    derivation helpers (including `classify_0x06`, `classify_0x06_with_ambiguity`,
    `classify_klv`, `stream_type_from_kind`).
  - `psi_topology.rs` — PSI section dispatch, PAT/PMT topology tracking,
    `build_program_map`, `klv_mismatch_insert`.
  - `pes_emit.rs` — PES reassembly dispatch and complete-PES-to-`DemuxEvent`
    conversion (`handle_pes_packet`, `handle_complete_pes`).
  - `stats_recorder.rs` — stats accounting and nonconformant event queueing
    (`queue_nonconformant`, `bump_video_counters`, `bump_klv_counters`,
    `bump_audio_counters`).

  All sibling submodules are `mod` (not `pub mod`) — private to the `demux`
  tree. Each uses `impl super::demuxer::Demuxer { pub(super) fn ... }` per
  Decision DB3, keeping the coordinator struct in one place.

- Binding-canonical-workflow audit: zero items promoted to `low_level` —
  all extracted helpers are classification/accounting internals with no
  documented FFI or binding-consumer demand.

**No public API impact:**

- `cargo public-api -p tst-core --simplified` byte-identical to pre-plan.
- `cargo public-api -p tst-pipeline --simplified` byte-identical to pre-plan.
- `cargo public-api -p tst-srt --simplified` byte-identical to pre-plan.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` stays at **87**.

---

### Wave 6.C-KLV typed-set module reorg (docs/plans/2026-05-19-wave-6-klv-reorg.md)

**Refactor (purely internal — zero public API change, zero `#[non_exhaustive]` BASELINE delta):**

- `klv::st0601` fan-out — 1711-line `mod.rs` god-file extracted into:
  `model.rs` (`UasDatalinkLs`, `EncodeConfig`, `GeoPoint`, `Attitude`,
  `FieldOfView`, `Corners`), `decode.rs` (4 decode entry points + inner
  helpers), `encode.rs` (5 encode / len functions). `mod.rs` becomes a
  ~35-line thin facade; all canonical re-exports preserved byte-identically
  at `klv::st0601::*`.
- `klv::st0102` fan-out — 1242-line `mod.rs` extracted into `model.rs`
  (`SecurityLs` + `pub(super)` UTF-16 helpers), `decode.rs`, `encode.rs`.
  `mod.rs` becomes a ~25-line thin facade.
- `klv::st0903` fan-out — 1572-line `mod.rs` + 1012-line `vtarget_pack.rs`
  extracted into `model.rs` (`VmtiLs`), `decode.rs`, `encode.rs`, `tests.rs`,
  and a nested `vtarget_pack/` subdirectory (`mod.rs`, `model.rs`, `decode.rs`,
  `encode.rs`, `tests.rs`). `mod.rs` becomes a ~90-line thin facade.
- `klv::st0605` directory conversion — 219-line `st0605.rs` single-file
  converted to `st0605/{mod.rs, model.rs, decode.rs, encode.rs}` for shape
  uniformity. Tests stay inline in `mod.rs` per Decision K5.
- `## Spec coverage` rustdoc blocks added to all 4 typed-set `mod.rs` files,
  listing parsed tags/fields, `unknown`-preservation policy, decode/encode
  modes, and deferred items. Closes audit `04-documentation.md` Finding 4
  and `08-test-infrastructure.md` Finding 4 (spec-coverage docstring scope).

**No public API impact:**

- `cargo public-api -p tst-core --simplified` baseline refreshed (re-export
  path resolution churn for some impl blocks; zero callable-symbol delta).
- `cargo public-api -p tst-pipeline --simplified` byte-identical to pre-plan.
- `cargo public-api -p tst-srt --simplified` byte-identical to pre-plan.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` unchanged by this
  plan (Wave 6.C-codec already bumped 87→105 — that plan's entry was
  inadvertently omitted from CHANGELOG during its ship; covered by memory
  entry `project_plan_87_wave_6_C_codec_reorg_shipped.md`).

**Test coverage:** 761 `tst-core` lib tests pass (unchanged count). All 4
KLV-touching fuzz targets (`klv_iter`, `klv_st0601_decode`, `klv_st0102_decode`,
`klv_st0903_decode`) compile clean under `cargo +nightly fuzz check`. All 6
bash ratchets green.

---

### Wave 6.A `mpegts/mux/mod.rs` god-module split (docs/plans/2026-05-19-wave-6-mux-mod-split.md)

**Refactor (purely internal — zero public API change, zero `#[non_exhaustive]` BASELINE delta):**

- `mpegts/mux/mod.rs` broken from ~4300 LoC into 8 focused modules:
  - `mux/state.rs` — stream-state structs (`VideoStreamState`, `KlvStreamState`,
    `AudioStreamState`, `SubtitleStreamState`), `validate_annex_b`,
    `caller_has_recognized_subtitle_descriptor`, `ts_packets_for`.
  - `mux/scheduling.rs` — `psi_due`, `pcr_due`, `maybe_emit_psi` (all `pub(super)`).
  - `mux/stats_accounting.rs` — `MuxerStats` struct and `stats()`, `reset_stats()`,
    `stream_codec_stats()`, `bump_*_counters()`.
  - `mux/push_video.rs` — `Muxer::push_video` and `push_video_to`.
  - `mux/push_klv.rs` — `Muxer::push_klv` and `push_klv_to`.
  - `mux/push_audio.rs` — `Muxer::push_audio`, `push_audio_to`,
    `audio_handles`, `audio_handles_for_program`, `audio_stream_handle`.
  - `mux/push_subtitle.rs` — `Muxer::push_subtitle_to`, `subtitle_handles`,
    `subtitle_handles_for_program`, `subtitle_stream_handle`.
  - `mux/tests/` — 6 test files (`config.rs`, `handles.rs`, `push.rs`,
    `stats.rs`, `subtitle.rs`, `validation.rs`) declared via `#[path]`
    as direct children of `mux` so `use super::*` scope is preserved.
- `mod.rs` reduced from ~4300 LoC to ~590 LoC (coordinator: struct definition,
  `new`, `pull`, `pending_packets`, `capacity_packets`, `pcr_pid_for_program`,
  and module declarations).
- Decision D7 applied: `emit.rs` extraction skipped — the emit loop is
  tightly coupled to per-push adaptation-field state and extracting it
  would require a behavioral-change-risking refactor. Per-push modules
  (`push_video.rs` etc.) own their emit loops directly.

**No public API impact:**

- `cargo public-api -p tst-core --simplified` byte-identical to pre-plan.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` unchanged by this
  plan (Plan C-codec already bumped 87→105).
- All 760 `tst-core` lib tests pass without modification.

---

### Wave 6.F mechanical / hygiene sweep (docs/plans/2026-05-19-wave-6-mechanical-sweep.md)

**Refactor (purely internal — zero public API change, zero `#[non_exhaustive]` BASELINE delta):**

- Mutex policy sweep — 23 sites. Applies the Wave 4.B hybrid mutex policy
  (plan #79) to every remaining `.lock().unwrap()` production site in
  `tst-pipeline`:
  - **19 sites in `mux_sender.rs`**: 10 fallible-return methods (`send_*`,
    `*_handles_for_program`) → `.lock().map_err(...)?` with site-specific
    diagnostic string mapped to `MuxSenderError::Broken` (via
    `From<TransportError>`) / `MuxError::ProgramNotFound`; 9 infallible-return
    methods (`*_handles`, `stats`, `socket_stats`, `stream_codec_stats`,
    `reset_stats`, `is_alive`) → `if let Ok(...) { ... } else { <safe default> }`
    matching the `socket_stats` precedent (reconnect/mod.rs:419-422).
  - **4 sites in `reconnect/mod.rs`** (`<ManagedTransport as Transport>`'s
    `max_payload`, `is_alive`, `close` + the multi-line site in `send_managed`'s
    pre-check size guard) → safe-default shape via
    `.lock().ok().and_then(...).unwrap_or(...)` for the trait methods, and
    `.lock().map_err(...)?` for the fallible pre-check site.
  - **Zero new BUG: panic sites** — per Plan F Decision F1, every site is
    recoverable (no in-flight queued bytes that would be silently lost on
    lock recovery).
- `apply_query_pair` split — `tst-srt/src/url.rs:343-523`. Decomposes the
  180-line `match`-arm-soup into 22 free-function helpers grouped by
  query-parameter family + a slim ~30-line routing match (24 arms after
  collapsing the latency trio + ffmpeg-alias trio). Per audit
  `01-structure-and-size.md` Finding 8 (Option (b) — "smallest change").
- `#[allow(clippy::field_reassign_with_default)]` tightening — per-site
  evaluation of the 16 sites in `tst-srt/src/config.rs` (4) and
  `tst-core/src/klv/st0601/mod.rs` (12). 13 converted to
  `..Default::default()` struct-update syntax; 3 kept on intentional
  spec-style `UasLs` round-trip construction. Per Plan F Decision F7 +
  audit `07-internal-hygiene.md` Finding 6.
- `#[allow(clippy::unnecessary_cast)]` verification — single site in
  `tst-srt/src/error.rs`. The existing 5-line cross-platform comment was
  re-verified as current; no edit needed. Per audit Finding 7.

**Tests (new regressions, in-file in `crates/tst-pipeline/src/{mux_sender,reconnect/mod}.rs`'s `#[cfg(test)]` mods):**

- `mux_sender_inner_lock_poisoned_returns_broken_error` — covers the 10
  fallible-return mutex sweep sites.
- `mux_sender_inner_lock_poisoned_returns_safe_default` — covers the 9
  infallible-return mutex sweep sites.
- `managed_transport_inner_lock_poisoned_returns_safe_default` — covers
  the 3 `Transport` trait impl sites in `reconnect/mod.rs`.
- Plan #79's `successful_reconnect_does_not_deadlock` regression stays
  passing — Plan F doesn't change `send_managed`'s scoped-guard discipline.

**No public API impact:**

- `cargo public-api -p tst-core --simplified` byte-identical to pre-plan.
- `cargo public-api -p tst-pipeline --simplified` byte-identical to pre-plan.
- `cargo public-api -p tst-srt --simplified` byte-identical to pre-plan.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` stays at **87**.

**Lock-policy rustdoc updates:** Both `MuxSender` and `ManagedTransport`
struct-level `# Panics` / "Lock poisoning policy" rustdoc sections updated
to reflect the now-complete hybrid policy across all transport-facing
methods.

**Wave 6 status after Plan F ships:** Plan F is the first of Wave 6's 5
Phase-1 plans (parallel with A, B, C-KLV, C-codec). Plans D and E (Phase 2)
wait on A and B respectively. Once all 7 land, refactor-1 is **complete**
and the project moves to `tst-jni` binding work.

---

### Wave 5.C C examples retrofit + tst-c structural reorg + sender-side audio/subtitle C ABI (docs/plans/2026-05-21-c-abi-examples-and-tst-c-reorg.md)

**Added (purely additive — no breaking changes):**

- Sender-side audio + subtitle C ABI exposure (gap left when plans #21
  and #22 deferred their C-side exposure "to the future receiver-surface
  plan," but receiver-surface plans #59/#60/#62 never picked them up).
  New entry points:
  - `TstAudioCodec` enum (Mp2/Aac/AacLatm/Ac3) reused from the
    pre-existing demux-event-side definition.
  - 2 audio constructors: `tst_mux_config_add_audio_stream` +
    `tst_mux_config_add_audio_stream_with_language` (3-byte ISO 639-2
    language tag).
  - 4 per-variant subtitle constructors:
    `tst_mux_config_add_subtitle_stream_dvb_subtitling`,
    `_dvb_teletext`, `_cea708`, `_webvtt`. Per-variant (not tagged
    union) for JNI/UniFFI binding ergonomics.
  - 4 muxer push: `tst_muxer_push_audio[_to]` +
    `tst_muxer_push_subtitle[_to]`.
  - 8 mux_sender send: `tst_mux_sender_send_audio[_to]` +
    `tst_mux_sender_send_subtitle[_to]` plus matching
    `tst_managed_mux_sender_send_*` wrappers (full pattern symmetry
    with the existing video/klv send surface).
  - 15 new integration tests in `bindings/c/tests/audio_subtitle.rs`.
- New C example
  `bindings/c/examples/c/muxing/mux_with_audio_klv_subtitles.c` —
  first C example covering all four user-visible stream-handle types
  (`TstVideoStreamHandle` + `TstAudioStreamHandle` +
  `TstKlvStreamHandle` + `TstSubtitleStreamHandle`) in one mux program.
  H.264 + AAC-ADTS + ST 0601 KLV + DVB subtitles with synthetic
  payloads; output verified end-to-end with `ffprobe`.

**Improved (zero ABI delta):**

- Retrofitted `bindings/c/examples/c/muxing/send_synthetic.c` from
  88 LoC / 19% comment density to 249 LoC / 64% density. Aligned with
  the teaching-code convention bar set by `mux_dual_camera.c` per
  `feedback_examples_are_teaching_code.md`: multi-line header banner,
  WHY comments on every non-obvious API call, explicit error-check
  pattern using `tst_get_last_error_str()`, label-based `goto fail`
  cleanup.

**Internal (zero callable-ABI delta — same symbols, same signatures,
same struct layouts, same sizeof asserts):**

- Split `bindings/c/src/config.rs` (1649 LoC) into `config/{mod,
  programs, streams, descriptors, builders}.rs`.
- Split `bindings/c/src/demux_receiver.rs` (1054 LoC) into
  `demux_receiver/{mod, events, stats, managed}.rs`.
- Reorganized `bindings/c/src/` from 17 flat sibling files into
  `sender/` (5 files: muxer, mux_sender, ts_sender, raw_sender,
  connect) + `receiver/` (4 entries: raw_receiver, ts_receiver,
  demux_receiver/, listen) subfolders. Cross-cutting files preserved
  at the root: `lib.rs`, `error.rs`, `panic.rs`, `handle.rs`,
  `event.rs`, `stats.rs`, `demux_config.rs`, `config/`. Plan A keeps
  version code inline in `lib.rs` (Decision D2); no `version.rs` file
  exists.
- Split `bindings/c/tests/url_open.rs` (1421 LoC) into
  `tests/url_open/{mod, mux_sender, ts_sender, raw_sender,
  demux_receiver, ts_receiver, raw_receiver}.rs`. Cargo's
  folder-shaped integration-test discovery (via explicit `[[test]]
  path = "..."` in `Cargo.toml`) treats `url_open/mod.rs` as the
  test binary entry point; `cargo test -p tst-c --test url_open`
  still runs all 31 tests as one binary.
- Added `sort_by = "Name"` to `bindings/c/cbindgen.toml` so
  generated items in `tstrans.h` are alphabetically ordered by symbol
  name. Closes a known Plan #83 follow-up. Decouples header layout
  from Rust source-file layout so future reorgs don't churn the
  header. One-time mechanical re-baseline of the committed
  `tstrans.h` (~3184-line diff, all mechanical reordering — same
  callable surface, same struct layouts, same sizeof asserts).
- Added workspace-level `rustfmt.toml` with `reorder_modules = false`.
  Several `tst-c/src/*/mod.rs` files use deliberate non-alphabetical
  `pub mod` declaration order; this config declares the intent so
  rustfmt leaves them alone.

**Verification:**

- All `tst-c` tests pass on default features,
  `--no-default-features`, and `--all-features` (214+ tests, 31 of
  which are the url_open split).
- `cargo public-api` baselines for `tst-core`, `tst-pipeline`,
  `tst-srt` byte-identical (Plan C touches none of those crates'
  Rust public surface; the audio/subtitle work promoted handle
  helpers to `pub #[doc(hidden)]` matching the existing video/klv
  precedent with zero baseline impact).
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml`
  unchanged at 87 (Plan C adds zero `#[non_exhaustive]` decorations).
- All 6 pre-push bash ratchets green (10 new entries added to
  `check-c-abi-rustdoc-coverage.sh` allowlist for the new
  audio/subtitle entry points).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
  --all-features` clean.
- `cargo fmt --all -- --check` clean.
- All 3 muxing C examples (`send_synthetic.c`, `mux_dual_camera.c`,
  `mux_with_audio_klv_subtitles.c`) compile cleanly with
  `-Wall -Werror`. Comment density: 64% / 61% / 60%.

---

### Wave 5.A C ABI versioning + last-error clear (docs/plans/2026-05-21-c-abi-versioning-and-last-error-clear.md)

**Added (purely additive — no breaking changes):**

- 3-tier C ABI version model: package + ABI + header.
  - **Package version** (tracks `Cargo.toml`):
    - `pub const TST_VERSION_MAJOR/MINOR/PATCH` already existed; emitted
      as `#define TST_VERSION_MAJOR 0` in `tstrans.h` since plan #1.
    - **NEW** runtime accessors: `tst_get_version_major()`,
      `tst_get_version_minor()`, `tst_get_version_patch()`,
      `tst_get_version_packed()` (returns `(M<<16)|(m<<8)|p` matching
      libsrt convention), `tst_get_version_string()` (returns a
      process-lifetime NUL-terminated `"M.m.p"` C string).
  - **ABI contract version** (bumped only on breaking C-ABI change):
    - **NEW** `pub const TST_ABI_VERSION_MAJOR/MINOR = 0/1` (initial
      `0.1` pre-1.0 value). Emitted as `#define TST_ABI_VERSION_MAJOR 0`
      / `#define TST_ABI_VERSION_MINOR 1`.
    - **NEW** runtime accessors `tst_get_abi_version_major()`,
      `tst_get_abi_version_minor()`.
- **NEW** `tst_clear_last_error()` C entry — resets the thread-local
  last-error slot to `(TST_E_SUCCESS, "")`. Mirrors libsrt's
  `srt_clearlasterror()`. Caller-driven; idempotent.
- **NEW** C smoke test
  `bindings/c/examples/c/getting-started/version_check.c`.
  Cross-validates every (runtime, header) pair; the canonical pattern
  for binding-author load-time SO/header consistency checks.
- **NEW** Rust integration test `bindings/c/tests/version_check.rs`
  (7 tests asserting each runtime accessor returns the expected const).
- **NEW** in-file last-error-clear tests in `bindings/c/src/error.rs`
  (`tst_clear_last_error_resets_to_success_state` +
  `tst_clear_last_error_idempotent_when_already_clear`).

**Internal:**

- Decision D1 (see plan): macro prefix is `TST_*` not `TSTRANS_*` for
  consistency with the existing `TST_VERSION_*` / `TST_INVALID_*` /
  `TST_STATS_*` precedent.
- Decision D2: version entries live inline in `bindings/c/src/lib.rs`
  rather than a new `version.rs`. Plan C's tst-c reorg owns any
  future extraction.
- Cbindgen mechanism: `pub const FOO: <integer-type> = N;` automatically
  emits as `#define FOO N` (verified by the existing `TST_VERSION_*`
  precedent on HEAD). No `[defines]` config block needed.

**Out of scope (deferred per Wave 5.A scope):**

- ABI-bump CI ratchet (relies on maintainer discipline +
  `header_drift.rs` to catch silent breakage).
- Per-entry-point versioning (`*_added_in` accessors).
- Domain-grouping comments in `tstrans.h` (Plan B).
- Symbol-script restriction of exports (Plan B).
- C ABI `_Static_assert` for `tst_socket_stats_t` (Plan B).

---

### Wave 5.B C ABI symbol hygiene + layout asserts + release-validation (docs/plans/2026-05-21-c-abi-symbol-hygiene-and-release-validation.md)

**Tooling / build:**

- **Symbol hygiene.** `bindings/c/build.rs` now emits per-OS linker args to restrict
  `libtstrans` dynamic exports to `tst_*`/`TST_*`:
  - Linux: `-Wl,--exclude-libs=ALL` (hides all static-library symbols, including
    libsrt's `srt_*`/`SRT_*` and mbedTLS's `mbedtls_*`).
  - macOS: `-Wl,-exported_symbols_list,exports.txt` (Apple ld whitelist with the
    Mach-O leading-underscore convention).
  - Windows: documented no-op pending plan #65 runtime-test deferral.

  The original plan specified a Linux `-Wl,--version-script=...` mechanism, but
  that conflicts with rustc's auto-emitted anonymous version-script for cdylib
  targets (GNU BFD ld rejects mixing named and anonymous version tags). The
  `--exclude-libs=ALL` pivot achieves the same outcome (0 `srt_*`/`SRT_*` in
  the dynamic export table) without touching the auto-emitted script.

  New file: `bindings/c/exports.txt`. Closes audit `09-c-abi.md` Finding 3.

- **Layout assertion.** `bindings/c/cbindgen.toml` trailer gains a 9th
  `_TST_ABI_ASSERT(sizeof(tst_socket_stats_t) == 120, ...)` line. Catches
  Rust-side `SocketStats` reorders that change the struct size at C-consumer
  build time. Closes audit `09-c-abi.md` Finding 2.

- **Domain-grouping section dividers.** `bindings/c/build.rs` runs a
  post-process step after cbindgen that inserts prefix-keyed section dividers
  in `tstrans.h` (`// ─── INTROSPECTION ───`, `// ─── MUX SENDER ───`, etc.).
  7 required sections (INTROSPECTION, MUX SENDER, TS SENDER, RAW SENDER,
  DEMUX RECEIVER, TS RECEIVER, RAW RECEIVER) + 2 conditional catch-alls
  (LIFETIME, OTHER). Symbol-name-based grouping is independent of source-file
  layout. Closes audit `09-c-abi.md` Finding 5.

- **CI ratchet.** New `scripts/check-no-srt-symbol-leak.sh`; runs
  `nm -D -g --defined-only` against `target/debug/libtstrans.so` + fails on any
  `srt_*`/`SRT_*` match. Wired into `.github/workflows/ci.yml` after the
  existing 6 ratchets. Linux-only (same gate as `symbol_audit.rs`).

- **`bindings/c/tests/symbol_audit.rs` update.** Removed the `srt_*`
  allowlist (no longer needed after Task 4); added `srt_symbols_not_exported`
  test for defense-in-depth (clearer failure message naming the specific
  leaked symbol).

**Release-validation:**

- **Step 6 (ffmpeg muxer differential).** `release-validation.sh:138-200` was a
  TODO stub; now extracts H.264 NALs via `ffmpeg -c:v copy`, re-muxes through
  ffmpeg, diffs `tsdump --psi` of `$BASELINE` vs ffmpeg-remux. Soft-fail on diff.
  Skips cleanly if ffmpeg or tsdump are missing.

- **Step 7 (player decode matrix).** `release-validation.sh` Step 7 had a
  partial-stub that only ran `$player --version`; now invokes each of
  `ffplay` / `vlc` / `mpv` / `gst-play-1.0` with player-specific headless flags
  + 10s timeout wrapper; greps stderr for error markers. Soft-fail per Tier-B
  convention. SKIPs missing players.

- **Step 8 (PTS rollover).** New test tool `gen_pts_rollover_fixture` at
  `crates/tst-core/tests/tools/gen_pts_rollover_fixture.rs` emits a synthetic
  .ts file with initial PTS 5 seconds below 2^33; the 10s stream straddles the
  MPEG-TS PTS wraparound. `release-validation.sh` Step 8 invokes the tool +
  probes with `tsdump` to confirm the demux side handles the wrap cleanly.

- **Step 9 (PCR jitter).** New test tool `measure_pcr_jitter` at
  `crates/tst-core/tests/tools/measure_pcr_jitter.rs` walks PCR samples +
  computes inter-PCR delta median + p95 (in milliseconds). Thresholds:
  median > 67 ms or p95 > 100 ms → fail (per
  `reference_ts_corpus_cadence.md` baseline). PCR extraction inlined per
  ISO/IEC 13818-1 §2.4.3.4-5 because `parse_ts_packet` is `pub(super)`.

**Internal (no public-API surface delta):**

- New files: `bindings/c/exports.txt`,
  `crates/tst-core/tests/tools/gen_pts_rollover_fixture.rs`,
  `crates/tst-core/tests/tools/measure_pcr_jitter.rs`,
  `scripts/check-no-srt-symbol-leak.sh`.
- Modified: `bindings/c/build.rs` (link args + post-process function),
  `bindings/c/cbindgen.toml` (9th layout assert),
  `bindings/c/include/tstrans.h` (regenerated +1 trailer line + section
  dividers), `bindings/c/tests/symbol_audit.rs` (allowlist removal + new
  test), `bindings/c/tests/header_drift.rs` (mirrored
  `add_section_dividers` to keep the drift check in sync; kept in lock-step
  with `build.rs` by convention).
- `cargo public-api` baselines for tst-core / tst-pipeline / tst-srt:
  unchanged.
- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml`: unchanged.

---

### Wave 4.C CancelHandle rename + pairing relocate + polish (docs/plans/2026-05-20-cancelhandle-pairing-and-polish.md)

**Breaking (pre-1.0):**

- Renamed `CancelHandle` → `SrtCancelHandle` to telegraph SRT-specificity
  (the type wraps a libsrt `SRTSOCKET` integer handle with `i64::MIN` as
  the cancelled sentinel — non-SRT transports that arrive later will add
  their own cancel primitives). Re-exports updated at `tst_pipeline::`
  and `tst_srt::` paths. C ABI unchanged (the type is internal to Rust;
  the `tst_*_cancel()` C function family stays the same).
- Relocated `tst_pipeline::pairing` → `tst_pipeline::ext::pairing`. The
  top-level `tst_pipeline::Pairer` (and `PairerMode`, `PairerConfig`,
  `PairerOutput`, `PairerStats`, `VideoSample`, `KlvSample`) re-exports
  are removed — callers spell out `tst_pipeline::ext::pairing::Pairer`.
  Signals "opt-in extension, not first-class shell" by withholding the
  convenience re-export.
- Removed `pub type tst_srt::Result<T> = std::result::Result<T, Error>;`
  alias (zero workspace consumers; workspace standard is to spell out
  the error type).

**Added:**

- Discriminating test files: `crates/tst-core/tests/demux_error_discrimination.rs`
  (3 tests for `DemuxError` variants — `SyncBufExhausted` + strict-mode
  `MalformedPes`/`StrictRejection` + a documented `MalformedPsi` smoke
  fallback since that variant has no public-API trigger path) and
  `crates/tst-pipeline/tests/transport_error_discrimination.rs`
  (5 tests covering `TransportError` variants flowing through shell
  errors: Backpressure, Broken, Closed→EndOfStream on receiver,
  ExplicitClose, TooLarge). Per
  `feedback_audit_test_not_always_discriminating.md`, these assert on
  the specific variant via `matches!` — not on `is_err()`.
- `docs/binding-authors.md` gained a new "Transient vs persistent error
  codes" subsection clarifying the contract on `TST_E_NOT_AVAILABLE` (-13)
  vs `TST_E_NOT_FOUND` (-14) for binding-language retry policies.
- New `tst_pipeline::ext` module with module-level rustdoc codifying
  the opt-in-extension contract for current and future extensions.

**Improved:**

- Upgraded rustdoc on `TstError::NotAvailable` (-13) and `TstError::NotFound`
  (-14) to lead with the transient-vs-persistent contract verb and to
  cross-reference each other. The cbindgen-generated C header
  `bindings/c/include/tstrans.h` regenerates with the new rustdoc.
- `tst_pipeline::lib.rs` and `tst_srt::lib.rs` comment blocks for the
  re-exported `SrtCancelHandle` updated from the now-misleading
  "transport-agnostic primitive" framing to the accurate "SRT-shaped
  primitive defined in tst-core for layering reasons" framing.

**Internal:**

- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` unchanged at
  87 (Plan C adds zero `#[non_exhaustive]` decorations).
- Renamed `docs/cancel-handle.md` → `docs/srt-cancel-handle.md` with
  page-header + intro-paragraph rewrites to drop the "universal
  cross-thread shutdown" framing.
- New `scripts/check-raw-c-mapper-coverage.sh` ratchet closes a Wave 1.3
  coverage gap noticed during static closeout review of Wave 4.A. The
  Wave 4.A split of the old `check-tst-c-error-coverage.sh` into
  `check-shell-error-kind-coverage.sh` + `check-pipeline-kind-classification.sh`
  covered the new shell-layer routing but left the raw `record_mux_error`
  / `record_transport_error` mappers (used by standalone-muxer paths and
  by `connect_srt`/`listen_srt` open helpers) unratcheted against
  upstream variant additions. New script restores per-variant coverage
  with one documented exclusion: `TransportError::ExplicitClose`, which
  the raw paths cannot construct.

---

### Wave 4.A shell error kind fold (docs/plans/2026-05-20-shell-error-kind-fold.md)

**Breaking (pre-1.0):**

- All 6 pipeline shells now return `struct { kind: ShellErrorKind, source: <Shell>ErrorSource }`
  instead of variant enums (`MuxSenderError`, `SenderError`, `DemuxReceiverError` already
  had enum-shaped errors; `RawSender::send`, `Receiver::next_packet`, `RawReceiver::recv_one`
  now return new shell error types instead of bare `TransportError`). Three new public types
  added: `RawSenderError`, `ReceiverError`, `RawReceiverError`.
- Six new public source enums: `MuxSenderErrorSource`, `SenderErrorSource`,
  `DemuxReceiverErrorSource`, `RawSenderErrorSource`, `ReceiverErrorSource`,
  `RawReceiverErrorSource`. Each `#[non_exhaustive]` with typed `#[from]` variants.
- Callers that matched on `MuxSenderError::Mux(_)` / `SenderError::Transport(_)` / etc.
  switch to `match err.source` or `match err.kind`. Pattern:
  ```rust
  // Old:
  Err(MuxSenderError::Transport(TransportError::Broken(_))) => { /* reconnect */ }
  // New (kind-based — recommended for binding-portable code):
  Err(err) if err.kind == ShellErrorKind::TransportBroken => { /* reconnect */ }
  // OR (source-based — preserves inner-variant discrimination):
  Err(err) if matches!(err.source, MuxSenderErrorSource::Transport(TransportError::Broken(_))) => { /* reconnect */ }
  ```
- New `TransportError::ExplicitClose` variant distinguishes caller-initiated close
  from peer-EOS (the existing `Closed` variant). Runtime wiring lands in Wave 4.B.
- `TsFramingError` is now `#[non_exhaustive]` (workspace convention sweep).
- C ABI numeric TST_E codes unchanged, but the **kind→code mapping consolidates several
  triggers**. The `tst_get_last_error_str()` content preserves the full inner Display
  output so callers reading the string get the full diagnostic:
  - `MuxError::InvalidNal` (was `TST_E_INVALID_NAL = -2`) → `TST_E_INVALID_TS = -3`
  - `MuxError::KlvTooLarge` (was `TST_E_KLV_TOO_LARGE = -5`) → `TST_E_INVALID_TS = -3`
  - `TransportError::TooLarge` (was `TST_E_TOO_LARGE = -6`) → `TST_E_INVALID_TS = -3`
  - `MuxError::InvalidStreamHandle`/`AmbiguousTarget`/etc. (was `TST_E_INVALID_USAGE = -9`) → `TST_E_INVALID_CONFIG = -1`
  - `TransportError::Backpressure` (was `TST_E_TRANSPORT = -8`) → `TST_E_BUFFER_FULL = -4`

**Added:**

- New `tst_pipeline::ShellErrorKind` enum with 6 variants 1:1 with TST_E codes:
  `ConfigInvalid` (-1), `InputMalformed` (-3), `Backpressure` (-4),
  `TransportBroken` (-8), `Closed` (-7), `EndOfStream` (-12). `#[non_exhaustive]`.
- New `tst_pipeline::ShellError` trait: `fn kind(&self) -> ShellErrorKind`. Implemented
  by all 6 shell error types.

**Internal:**

- `bindings/c/src/error.rs` collapses from 4 per-variant `record_*_error` functions
  (~270 lines of per-variant translation) to one `record_shell_error<E: ShellError>(e: &E) -> i32`
  helper plus `tst_error_from_kind(kind: ShellErrorKind) -> TstError`. Inline match
  routing at 2 recv-path sites in `bindings/c/src/demux_receiver.rs` also
  collapsed to `record_shell_error` (the open-path sites still use
  `record_transport_error` for raw `TransportError` from connect helpers).
- `scripts/check-tst-c-error-coverage.sh` (plan #70, 134 lines) split into two new
  scripts: `check-shell-error-kind-coverage.sh` (kind→code routing in tst-c) and
  `check-pipeline-kind-classification.sh` (inner-variant→kind routing in tst-pipeline).
- `#[non_exhaustive]` BASELINE bumped 72 → 87 in `.github/workflows/ci.yml`.
- The +5 delta over Plan A's projected `72 → 82` baseline comes from
  net new rustdoc-comment-line mentions of `#[non_exhaustive]` (7 comment
  lines added, 2 deleted = +5 net), not from additional public-API attribute
  decorations. The 10 actual attribute additions match Plan A's projection
  (ShellErrorKind + 6 shell error structs + 3 source enums). Discriminating
  attribute-only count today: 67 (`rg -c` inflated count: 87; difference: 20
  comment-line mentions across `crates/`). Comment sites include the
  `kind_from_transport` pattern notes, the `ShellErrorKind` variant rustdoc,
  and shell-error-source struct docs. See memory entry
  `feedback_baseline_count_projection_undercount.md` for the systemic root
  cause; the CI `BASELINE` constant continues to track the inflated `rg -c`
  count for compatibility with the existing guard expression.

---

### Wave 4.B Transport semantics + mutex policy (docs/plans/2026-05-20-transport-semantics-and-mutex-policy.md)

**Breaking (pre-1.0):**

- `ManagedRecvTransport::recv_bytes` returns `TransportError::ExplicitClose`
  on caller-initiated paths (entry check + cross-thread cancel signal),
  replacing `TransportError::Closed`. The reconnect-budget-exhausted exit
  continues to return `TransportError::Closed` (peer-EOS-ish). The
  receive-side shell's `kind_from_transport` (Plan A) maps `ExplicitClose`
  → `ShellErrorKind::Closed` → `TST_E_CLOSED` (-7) and `Closed` →
  `ShellErrorKind::EndOfStream` → `TST_E_END_OF_STREAM` (-12), fixing the
  long-standing peer-EOS-vs-caller-close conflation (03-architecture.md
  Finding 5).

**Internal:**

- Mutex poison sweep in `tst-pipeline`: 4 recoverable-path sites now route
  to `TransportError::Broken` with site-specific messages
  (`managed_receive.rs:179`, `reconnect/mod.rs:193/222/287`); 2
  invariant-critical gap-accumulator sites now panic with `BUG: ...`
  prefix caught by `tst-c`'s `ffi_catch` as `TST_E_PANIC_CAUGHT` (-11)
  (`reconnect/mod.rs:214/226`). Plan #45's `.lock().ok()` cancel-path
  precedent extended to all 6 audit-enumerated sites
  (05-error-handling.md Finding 2). The 17 `.lock().unwrap()` sites in
  `mux_sender.rs` and 4 additional sites in `reconnect/mod.rs`
  (size-precheck inside send_managed, plus `Transport::max_payload`,
  `Transport::is_alive`, `Transport::close`) are out of scope for Plan B
  (not in audit enumeration).

- Three booleans (`closed`, `explicit_close`, `cancelled`) now disambiguate
  ManagedRecvTransport state. The original 2-bool design (`closed` +
  `cancelled`) couldn't distinguish caller-close from budget-exhausted
  (both set `closed=true`); added `explicit_close` set only by caller
  paths so the re-entry gate routes correctly.

- New `# Panics` rustdoc on `ManagedTransport::send_managed` and
  `ManagedTransport::drain_gap_if_alive` documenting the `BUG: gap lock
  poisoned` panic contract.

- New `crates/tst-pipeline/tests/poison_policy.rs` with 4 discriminating
  tests covering Tasks 2-4 behaviors.

---

### Wave 3.2 naming consistency + Stats typing (docs/plans/2026-05-19-naming-renames-and-stats-typing.md)

**Breaking (pre-1.0):**

- Renamed `ManagedReceiveTransport` → `ManagedRecvTransport` (symmetric
  with `ManagedTransport` and the underlying `RecvTransport` trait).
  C ABI type names (`tst_managed_demux_receiver_t` etc.) unchanged.
- Renamed `RawSenderStats` → `RawSendStats` and `RawReceiverStats` →
  `RawRecvStats` (the one confusable Stats pair in the workspace).
- Renamed C ABI mirror types `TstRawSenderStats` → `TstRawSendStats`,
  `TstRawReceiverStats` → `TstRawRecvStats`. Header typedefs
  `tst_raw_sender_stats_t` → `tst_raw_send_stats_t` and
  `tst_raw_receiver_stats_t` → `tst_raw_recv_stats_t`. Affects
  `tst_raw_*_get_stats` / `tst_managed_raw_*_get_stats` function
  signatures.
- Changed `StreamStats.stream_type: u8` → `StreamStats.stream_type:
  StreamTypeCode`. C ABI `tst_stream_stats_t.stream_type: uint8_t`
  unchanged (Rust→C bridge calls `.as_byte()` at conversion).
- Removed deprecated `AddrError::Ipv6Unsupported` variant (deprecated
  since plan #29 / 2026-05-06 when IPv6 shipped).

**Added:**

- New `tst_core::mpegts::common::StreamTypeCode` enum:
  `Known(StreamType)` for codes this library recognizes,
  `Unknown(u8)` for codes seen in real-world streams outside the
  typed `StreamType` set. `#[non_exhaustive]`. Methods: `from_byte`,
  `as_byte`, `known`.

**Internal:**

- `#[non_exhaustive]` BASELINE bumped 71 → 72 in `.github/workflows/ci.yml`.

---

### Wave 2.3 config conventions and symmetry (plan #72)

#### Added

- New `docs/conventions.md` codifies workspace-wide policies for
  Config/Options naming, constructor naming, builder-vs-Default, public
  field policy, and where invariants are enforced.
- New `tst_pipeline::ReceiverConfig` + `tst_pipeline::RawReceiverConfig`
  empty `#[non_exhaustive]` structs. Future receive-side knobs can land
  non-breakingly. Mirror the send-side `SenderConfig`/`RawSenderConfig`
  shape.
- New `tst_pipeline::Pairer::new(video_pid, klv_pid) -> Self` primary
  constructor that delegates to `with_options` with default config.
- New `tst_core::error::MuxError::ConfigInvalid { reason: String }`
  variant for richer `validate()` diagnostics that need formatted
  reasons. Maps to `TstError::InvalidConfig` at the C ABI (same code
  as flat-string `MuxError::InvalidConfig`).
- New `tst_core::mpegts::mux::MuxerProgramConfig::new(program_number,
  pmt_pid) -> Self` in-crate constructor for external callers (now
  required because `MuxerProgramConfig` gained `#[non_exhaustive]` and
  has no `Default` impl).

#### Changed (BREAKING — pre-1.0)

- `tst_core::mpegts::demux::DemuxerOptions` renamed to
  `tst_core::mpegts::demux::DemuxerConfig`; also gained
  `#[non_exhaustive]`. Construction via struct literal outside the
  crate no longer permitted; use `DemuxerConfig::default()` and assign
  overrides.
- `tst_pipeline::PairerOptions` renamed to `tst_pipeline::PairerConfig`.
- `tst_core::klv::st0601::EncodeOptions` renamed to
  `tst_core::klv::st0601::EncodeConfig`; also gained
  `#[non_exhaustive]`.
- `tst_pipeline::Receiver::new(transport)` → `Receiver::new(transport,
  ReceiverConfig)`. The config is currently empty; pass
  `ReceiverConfig::default()`.
- `tst_pipeline::RawReceiver::new(transport)` → `RawReceiver::new(
  transport, RawReceiverConfig)`. Same.
- `MuxerConfig::validate()` now raises `MuxError::ConfigInvalid { reason }`
  (richer diagnostic) instead of `MuxError::InvalidConfig(static)` for
  `stream_descriptors` length mismatches. Pattern matches on
  `InvalidConfig` no longer catch this specific case.
- `tst_pipeline::SenderConfig`, `tst_pipeline::RawSenderConfig`,
  `tst_srt::SocketConfig`, `tst_srt::ListenerConfig`,
  `tst_core::mpegts::mux::MuxerConfig`, and
  `tst_core::mpegts::mux::MuxerProgramConfig` ALL gained
  `#[non_exhaustive]`. Cross-crate callers using struct literal syntax
  (incl. `Foo { field, ..Default::default() }`) must migrate to
  default-and-assign: `let mut cfg = Foo::default(); cfg.field = ...;`.
  See `docs/conventions.md` § "Public field policy for `*Config`
  structs" for the canonical construction patterns.

#### CI

- `#[non_exhaustive]` BASELINE in `.github/workflows/ci.yml` bumped
  from 58 to 71 (+13 observed by the `rg -c` count CI uses; reflects
  4 new annotations from Tasks 2 + 4 — `DemuxerConfig`,
  `EncodeConfig`, `ReceiverConfig`, `RawReceiverConfig` — plus 6 from
  the codex-required policy sweep — `SenderConfig`, `RawSenderConfig`,
  `SocketConfig`, `ListenerConfig`, `MuxerConfig`,
  `MuxerProgramConfig` — plus 3 doc-comment mentions of
  `#[non_exhaustive]` that the regex naturally captures).
- `cargo public-api` baselines refreshed for `tst-core`,
  `tst-pipeline`, AND `tst-srt` (`tst-srt` now changes because Task 9
  sweeps `SocketConfig` + `ListenerConfig`).

---

### demux event fixes (plan #69)

#### Changed (BREAKING — pre-1.0)

- **`tst_core::mpegts::demux::StreamId` gained `program_number: u16`
  field.** All construction sites must supply it. The `Demuxer`
  populates it from the PMT via an internal `program_number_for_pid()`
  lookup; falls back to sentinel `0` only for pre-PMT contexts where
  no PMT has been seen yet for the PID. Outside-crate construction of
  `StreamId` literals must add the field; pattern matches on
  `StreamId { .. }` are unaffected.
- **`tst_event_sample_t.program_number` (C ABI)** is now populated
  from the actual stream's owning program. Previously hardcoded to
  `0` (TODO from earlier ABI work). Multi-program demux consumers
  that depended on the always-zero behavior must update.
- **`tst_event_metadata_t.program_number` (C ABI)** same as above.

#### Added

- **`NonConformantIssue::MalformedPes { pid, reason }` (Rust API)** —
  malformed PES headers now surface as a non-conformant event in
  lenient demux mode instead of propagating as a fatal error. Strict
  demux modes still escalate. Applies symmetrically to both
  `Demuxer::feed` and `Demuxer::feed_aligned` via a shared internal
  handler so the byte-aligned and packet-aligned feed paths report
  identical issue counts on the same input.
- **`tst_event_sample_t.random_access_indicator` (C ABI, `uint8_t`)** —
  exposes the TS adaptation-field RAI bit on video sample events.
  Zero for non-video sample events. Companion to the Rust-side
  `SamplePayload::Video::random_access_indicator` field added in plan
  #68; this entry plumbs it through the C ABI.
- **`tst_event_sample_t.stream_type` (C ABI, `uint8_t`)** — exposes
  the raw PMT `stream_type` byte on sample events with unknown or
  vendor-specific codecs so C-side consumers can inspect / route
  them. Zero for known stream types that map to a typed payload.
- **`tst_event_discontinuity_t.variant_pid` (C ABI, `uint16_t`)** —
  carries the discontinuity-variant-specific PID. Currently used
  only by `PesOversize` (the offending stream's PID); the existing
  `pid` field continues to mean the parent stream PID. Zero for
  variants that don't have a variant-specific PID.
- **`tst_nonconformant_code_t::TST_NCC_MALFORMED_PES = 19` (C ABI)** —
  C-side discriminator for the new `NonConformantIssue::MalformedPes`
  Rust variant; surfaces in `tst_event_nonconformant_t.code`.

#### Fixed

- **`Muxer::push_video()` and `Muxer::push_klv()` now route to the
  correct program in multi-program configs** when the lone stream of
  that kind is not in program-index 0. Previously both hardcoded
  `pack(0, 0)`, so a config with a single video stream in
  program-index 1 (or any non-zero program) silently mis-routed the
  pushed AU. Extracted shared `single_video_handle()` /
  `single_klv_handle()` helpers matching the working
  `push_audio()` / `push_subtitle()` pattern.
- **`mpegts::demux::pes::Reassembler::push` previously dropped the
  new PUSI's payload** if the prior buffer hit `MalformedPes` during
  `parse_complete`. Restructured to insert the new `Partial` state
  up-front and defer the prior-parse error, so lenient-mode recovery
  actually emits subsequent `Sample` events from the same PID after a
  malformed PES instead of stalling.

---

### codec-specific per-stream stats (plan #68)

#### Added

- **`tst_core::stats::StreamCodecStats`** — `#[non_exhaustive]` tagged
  enum carrying per-PID codec-specific counters. Variants (each
  `#[non_exhaustive]`): `Video { nals_or_obus, random_access_aus }`
  (H.264/H.265/H.266 NAL counts or AV1 OBU counts + random-access AU
  count), `Klv { records }` (BER-TLV record count when a PES carries
  multiple records), `Audio { frames }` (MP2 + AAC-ADTS frame counts;
  LATM + AC-3 fall through to `Unknown` — see deferred-features.md),
  `Unknown` (codec not classified or no codec-specific counters
  defined yet).
- **`Muxer::stream_codec_stats(pid)` and
  `Demuxer::stream_codec_stats(pid)`** — `Option<StreamCodecStats>`
  accessors; return `None` when the PID was never observed on this
  handle. Codec kind is determined eagerly from the configured /
  parsed `stream_type`; counters reset alongside the existing
  `stats_per_stream` reset path.
- **Pipeline-level `stream_codec_stats(pid)` on `MuxSender` and
  `DemuxReceiver`** (both plain and managed-transport variants share
  the same method via the `Transport` generic).
- **`tst-c`: 5 new entry points** —
  `tst_muxer_get_stream_codec_stats`,
  `tst_mux_sender_get_stream_codec_stats`,
  `tst_managed_mux_sender_get_stream_codec_stats`,
  `tst_demux_receiver_get_stream_codec_stats`,
  `tst_managed_demux_receiver_get_stream_codec_stats`.
  All take `(handle, pid, *out tst_stream_codec_stats_t)` and return
  the standard `tst_error_t` discriminant.
- **`tst-c`: `TstStreamCodecStats` `repr(C)` tagged-union (24 B)** —
  `kind` discriminator (one of `TST_CODEC_KIND_UNKNOWN`,
  `TST_CODEC_KIND_VIDEO`, `TST_CODEC_KIND_KLV`,
  `TST_CODEC_KIND_AUDIO`) + arm-specific payload structs
  `tst_codec_video_stats_t` / `tst_codec_klv_stats_t` /
  `tst_codec_audio_stats_t`. `_Static_assert` ABI size guards trip
  consumer-side builds on accidental layout drift.
- **`tst-c`: `TST_E_NOT_FOUND = -14` error code** — returned by the
  `_get_stream_codec_stats` family when the PID was never observed on
  this handle. Distinct from `TST_E_NOT_AVAILABLE` (transient
  managed-reconnect mid-flight) so callers can branch on the
  "PID-typo / wrong-PID" vs "wait and retry" distinction.
- **TS adaptation-field `random_access_indicator` (bit 0x40) now
  extracted** and propagated through PES assembly to
  `SamplePayload::Video::random_access_indicator`. Receiver-side
  `random_access_aus` codec counter uses this signal.
- **`tst_core::codec::util::count_nal_units(buf, codec)`** —
  cross-codec helper for counting NAL units
  (H.264/H.265/H.266 Annex-B start-code scan) or OBUs (AV1 LEB128
  walk) inside a single AU buffer. Shared between the muxer-side
  push-time count and the demuxer-side parse-time count.

#### Changed

- **BREAKING** — `SamplePayload::Video` gains a new field
  `random_access_indicator: bool`. `SamplePayload` is already
  `#[non_exhaustive]` at the variant level, but the `Video` payload
  struct is not — outside-crate pattern matches on
  `SamplePayload::Video { ... }` need a `..` rest binding to absorb
  the new field; struct construction at test sites must add
  `random_access_indicator: false` (or the relevant value).
- **BREAKING** — `mpegts::demux::pes::PesPayload` gains a new field
  `random_access_indicator: bool`.
- **BREAKING** — `mpegts::demux::ts::TsPacket` gains a new field
  `random_access_indicator: bool` (struct already `#[non_exhaustive]`
  so pattern-match consumers absorb via `..`; struct-construction
  consumers must add the field).
- **BREAKING** — `mpegts::demux::pes::Reassembler::push` gains a 4th
  parameter `random_access_indicator: bool` (RAI gets latched on the
  first TS packet of an AU at the reassembler level so PES payload
  emission carries it through).
- **`#[non_exhaustive]` workspace count guard `BASELINE`** bumped
  from 52 to 54 in `.github/workflows/ci.yml` (absorbs
  `StreamCodecStats` enum + 3 variants worth of `#[non_exhaustive]`
  attributes).

See the plan at `docs/plans/2026-05-16-codec-specific-stats.md`.
Closes the P1 "codec-specific stats on `StreamStats`" backlog entry
(deferred from plan #16).

---

### tst-srt Windows MSVC port (plan #65)

#### Changed
- **`tst-srt`: internal sockaddr handling switched from
  `libc::sockaddr_*` to `os_socketaddr::OsSocketAddr`.** Public API
  surface unchanged — `Socket::connect` / `Socket::peer_addr` /
  `Socket::local_addr` / `Listener::bind` / `Listener::accept` all
  still take or return `std::net::SocketAddr`. Internal substitution
  unblocks `*-pc-windows-msvc` builds (`libc` doesn't expose
  `sockaddr_storage` / `sockaddr_in` / `sockaddr_in6` / `linger` /
  `AF_INET6` on that target). `Socket::set_linger`-related code uses
  a hand-rolled `#[repr(C)] LingerOpt` POD (2-field struct, same
  layout on every platform; POSIX `SO_LINGER` predates the BSD /
  Win32 split). `addr::to_sockaddr` is now infallible (was
  `Result<_, AddrError>` with a Result that was vestigial); callers
  in `socket.rs` / `listener.rs` simplified accordingly.

#### Added
- **`os_socketaddr = "0.2"` workspace dep** for cross-platform
  sockaddr abstraction. Used only by `tst-srt` today. Tiny crate
  (~400 LoC); deps are `libc` on Unix + `winapi` cfg-gated on
  Windows.
- **`crates/srt-sys/build.rs`: Windows MSVC build support.**
  Three additions cfg-gated on `target.contains("msvc")`:
  - `/EHsc` cxxflag for the libsrt cmake build (MSVC requires
    explicit C++ exception unwind semantics; gcc/clang have it on
    by default). Originally shipped under plan #64 (commit
    `dcd04d6`); referenced here for completeness.
  - Link `srt_static` instead of `srt` (libsrt's CMakeLists names
    the static lib `srt_static.lib` on MSVC to avoid colliding with
    the shared-lib import lib also called `srt.lib`; see
    `vendor/srt/CMakeLists.txt:1169-1181`).
  - Link `bcrypt` (mbedTLS on Windows uses `BCryptGenRandom` from
    `bcrypt.dll` for entropy collection; on Linux it uses
    `/dev/urandom`).
- **`docs/deferred-features.md`: "Windows MSVC runtime test
  stabilization" entry.** SRT loopback tests hang on Windows (at
  least `tst-c::demux_receiver_loopback` observed at 18+ min before
  cancellation; likely the whole loopback test family affected).
  Most plausible root cause: `srt_close` peer-EOS propagation
  semantics differ on winsock vs BSD sockets. Diagnosis requires
  Windows hardware on hand to iterate. Memory note with full
  diagnostic plan at
  `project_plan_65_windows_runtime_test_deferral.md`.

#### Skipped on windows-msvc (pending deferred follow-up)
- **`cargo test --doc`, `cargo test` (default / no-default /
  all-features) gated on `if: matrix.name != 'windows-msvc'`**
  in `.github/workflows/ci.yml`. Windows MSVC matrix entry now
  runs `cargo build` (default + no-default-features) to gate
  compile + link regressions; runtime test coverage falls to
  Linux x86_64 + Linux aarch64 + macOS arm64.

#### Fixed
- **`tst-srt/tests/socket_stats::socket_stats_returns_none_after_close`:
  50ms pause before close to win the accept/close race.** Same
  fast-hardware race fixed for `lifecycle.rs::explicit_close_succeeds`
  in plan #66 (commit `40eb7f9`); surfaced on linux-aarch64 mid-
  plan-#65 once Windows compile errors stopped masking other
  matrix-entry failures. Order-swap fix from plan #66 doesn't
  apply here because the accept closure calls `recv()` (blocks
  until peer-close), so `accept.join()` before close would
  deadlock; 50 ms pause covers the connect/accept window instead.

#### Allow
- **`#[allow(clippy::unnecessary_cast)]` on the
  `crates/tst-srt/src/error.rs` tests module.** bindgen emits the
  `SRT_REJECT_REASON_*` and `SRT_REJX_*` constants as `u32` on
  Linux but `i32` on `*-pc-windows-msvc`. The `as i32` casts in
  the reject-reason ordinal-pinning tests are necessary on Linux
  (u32→i32) but redundant on Windows (i32→i32 → clippy error
  under `-D warnings`). Module-level allow with explanatory
  comment rather than 25+ per-callsite cfg gates.

---

### macOS arm64 phase-in stabilization (plan #66)

#### Changed
- **Loopback integration tests stabilized for Darwin scheduling.**
  Six tests across `bindings/c/tests/` and `crates/tst-srt/tests/`
  had hardcoded `thread::sleep` drain pauses (100-500 ms) that worked
  on Linux loopback but raced on the GHA `macos-14` (Apple Silicon)
  runner. All bumped to 1 s — comfortably covers SRT's 120 ms
  latency budget plus Darwin scheduling jitter on every platform.
  Affected tests: `tst-c::raw_receiver_loopback`,
  `tst-c::ts_receiver_loopback`, `tst-c::stats`,
  `tst-srt::pipeline_sender`, `tst-srt::pipeline_receiver_live`,
  `tst-srt::pipeline_receiver_live_corpus`. (Continues the pattern
  established by the post-plan-#64 hotfix to
  `tst-c::demux_receiver_loopback`.)
- **`bindings/c/tests/smoke.rs`: cross-platform cdylib name +
  dylib-search env var.** Was hardcoding `libtstrans.so` +
  `LD_LIBRARY_PATH`; macOS uses `.dylib` + `DYLD_LIBRARY_PATH`,
  Windows uses `tstrans.dll` + `PATH`. Refactored to use
  `std::env::consts::DLL_{PREFIX,SUFFIX}` for the name + a compile-
  time `cfg!`-evaluated const for the env var name. Windows PATH
  handling prepends (not replaces) so basic C runtime DLLs stay
  reachable.
- **`crates/tst-srt/tests/lifecycle::explicit_close_succeeds`:
  deterministic accept/close ordering.** Latent race surfaced by
  plan #67's linux-aarch64 gating promotion — on fast ARM hardware
  `socket.close()` could win against listener `accept()` returning,
  leaving `accept` to panic with "Connection was broken." Swapped
  order: `accept.join()` first, then `socket.close()`. Same
  verification intent; no race.

#### Cfg-gated
- **`bindings/c/tests/symbol_audit`: `#[cfg_attr(not(all(target_os =
  "linux", target_env = "gnu")), ignore = "..."]`.** The test uses
  GNU nm with ELF-specific flags and filters ELF housekeeping
  symbols (`_init`, `_fini`, `__bss_start`, etc.). macOS (Mach-O)
  and Windows (COFF) have entirely different symbol formats. Linux
  GNU coverage (x86_64 + aarch64, both gating) is sufficient for the
  no-Rust-symbol-leak invariant; porting the test would require
  three separate platform-specific implementations of the same
  invariant. Documented as Linux-GNU-only by design in the module
  rustdoc.

---

### Linux aarch64 promoted to gating (plan #67)

#### Changed
- **`.github/workflows/ci.yml`: linux-aarch64 flipped from
  `continue-on-error: true` to `continue: false`.** Aarch64 was
  green on every post-ship run since the plan #64 matrix expansion
  (2026-05-16), so the conservative 14-day phase-in window is no
  longer warranted. Aarch64 build/test failure now blocks PR merge
  alongside Linux x86_64.
- **`docs/compatibility.md`: Linux aarch64 row** updated from
  "Tier 1, phase-in" to "Tier 1, gating". macos-arm64 and
  windows-msvc remain "Tier 1, phase-in" pending their own fix
  plans (#66 macOS loopback stabilization, #65 tst-srt Windows
  port).

---

### tst-c Tier 1 multi-platform (plan #64)

#### Added

- **CI: Tier 1 multi-platform matrix.** `.github/workflows/ci.yml`
  refactored from a single `test-linux` job into a matrix-strategy
  `build` job with 4 entries: Linux x86_64 (gating, unchanged),
  Linux aarch64 (phase-in informational), macOS arm64 (phase-in
  informational), Windows x86_64 MSVC (phase-in informational).
  Native runners on all 4 — no cross-compilation. After ~14
  consecutive green nightly days on the 3 new platforms a separate
  follow-up plan (P2) flips `continue-on-error: true` to `false`,
  converting them to gating.
- **`docs/compatibility.md` build-targets table** documenting Tier 1
  (Linux x86_64 / Linux aarch64 / macOS arm64 / Windows MSVC) +
  Tier 2 (Linux musl) + Deferred (iOS, Android, MinGW, macOS Intel)
  status per platform, with phase-in semantics explained inline.
- **`docs/deferred-features.md` entries** for iOS (device + simulator),
  Android (arm64 + x86_64 emulator + armv7), macOS x86_64 Intel, and
  Windows MinGW — each with concrete consumer-driven triggers and a
  scope-when-added note. iOS + Android gated on the future
  `tst-uniffi` plan; macOS Intel + MinGW gated on specific consumer
  asks.
- **`README.md` Platform support subsection** under `## Building`
  listing the 4 Tier 1 platforms and cross-linking
  `docs/compatibility.md` + `docs/deferred-features.md`. Stale
  "multi-platform builds … next on the roadmap" sentence near the
  C example removed (multi-platform ships today; only `tst-jni` /
  `tst-uniffi` remain on the roadmap).

---

### libsrt wire-stats at the C ABI (plan #63)

#### Added

- **`tst_core::transport::SocketStats`** — abstract wire-level transport
  stats (RTT µs, send/recv/link bandwidth bps, sent/received bytes +
  packets, recv-side byte+packet loss, send-side packet loss,
  retransmits, send/recv drops, send/recv buffer depths in packets).
  16-field `#[non_exhaustive]` struct so growing the field set in
  future libsrt releases is not a breaking change.
- **`Transport::socket_stats()` / `RecvTransport::socket_stats()`** —
  new trait method, defaulted to `None`. `SrtTransport` /
  `SrtRecvTransport` implement it by mapping
  `crate::socket::Stats` (libsrt `CBytePerfMon` snapshot) through a
  `map_stats` free function. `ManagedTransport` /
  `ManagedRecvTransport` forward through
  `inner.as_ref().and_then(...)`, returning `None` mid-reconnect.
- **`MuxSender::socket_stats()`, `Sender::socket_stats()`,
  `Receiver::socket_stats()`, `RawReceiver::socket_stats()`,
  `DemuxReceiver::socket_stats()`** — pipeline-shell pass-throughs.
  `RawSender` reaches through the existing `transport()` accessor.
- **`tst-c`: 12 new entry points `tst_*_get_socket_stats(p, out)`**
  across all 6 sender + 6 receiver handle families (mux_sender,
  ts_sender, raw_sender, receiver, demux_receiver, raw_receiver —
  each plain + managed). Reads from the underlying libsrt socket and
  copies the snapshot into the caller's `tst_socket_stats_t`.
- **`tst-c`: `TstSocketStats` `repr(C)` struct (120 B)** — 16 fields
  (3 u32 + 1 u32 pad + 13 u64). Const-assert pins size at 120 B.
  Layout documented field-by-field for binding authors.
- **`tst-c`: `TST_E_NOT_AVAILABLE = -13` error code** — returned by
  the `_get_socket_stats` family when the inner transport has no
  live socket (closed, or for managed: mid-reconnect). Distinguished
  from `TST_E_INVALID_USAGE` so callers can branch on the transient
  vs. fundamental distinction.
- **C teaching example `examples/c/operations/socket_stats_poll.c`**
  — 5-second send loop with periodic socket-stats print every 500 ms
  (RTT, bytes_sent, packets_sent, loss, retransmits, send-buffer
  depth). First entry under the new C-side `operations/` subfolder.

#### Changed

- **`#[non_exhaustive]` workspace count guard `BASELINE`** bumped from
  42 to 47 in `.github/workflows/ci.yml` (absorbs `SocketStats` +
  4 prior post-plan-#62 additions).
- **`cargo public-api` baselines** refreshed for `tst-core`,
  `tst-pipeline`, `tst-srt` (additions: `SocketStats` struct +
  `socket_stats()` methods on the trait + 6 shells + 2 transport
  impls + `Box<T>` blanket forwarding).
- **Mid-flight catch: `#[non_exhaustive]` outside-crate construction**
  — Rust E0639 blocks `SocketStats { ... }` struct-literal even with
  the `..Default::default()` update-syntax tail (no escape hatch as
  of Rust 1.85). The `map_stats` site in `tst-srt` uses the
  default-and-assign pattern instead.

See the wire-stats plan at
`docs/plans/2026-05-16-tst-c-libsrt-wire-stats.md`.

---

### Phase 3 of tst-c receiver surface (plan #62)

#### Added

- **`tst-c` receiver surface Phase 3** — `tst_demux_receiver_t` and
  `tst_managed_demux_receiver_t` opaque handles wrapping
  `tst_pipeline::DemuxReceiver<SrtTransport>`. Surface the full typed-
  event API to non-Rust consumers: `tst_event_t` tagged union over
  PROGRAM_MAP / SAMPLE / METADATA / DISCONTINUITY / NONCONFORMANT,
  with subordinate `tst_nal_t`, `tst_obu_t`, `tst_descriptor_t`,
  `tst_stream_info_t`, `tst_klv_link_t` list elements. Pointer fields
  borrow from a per-handle EventArena (zero-alloc steady state) —
  valid until the next `_recv_event` / `_close` call.
- **`tst_demux_config_t` opaque builder** — caller-side knobs:
  strict mode (4 levels), KLV PID→video PID link overrides,
  per-PID stream-kind overrides, PES reassembly caps.
- **Bundled send-side descriptor API** — `tst_mux_config_add_video_descriptor`,
  `_add_klv_descriptor`, `_add_audio_descriptor`,
  `_add_subtitle_descriptor` close the previously-deferred
  per-stream PMT descriptor construction at the C ABI. Shares the
  receive-side `tst_descriptor_t` struct from day one.
- **Per-PID stats** — `tst_demux_receiver_get_stream_stats` returns
  a borrowed `(*const tst_stream_stats_t, size_t)` array per design §4.5
  lifetime convention (valid until next get_stream_stats /
  reset_stats / close call).
- **Two new C examples** — `recv_demux_to_console.c` (flagship
  Phase 3 example printing all 5 event kinds) and
  `recv_klv_to_stdout.c` (KLV byte-flow tap, building block for
  external typed-ST 0601 decoders).
- **`_Static_assert` ABI size guards** on all public Phase 3 structs
  (`tst_nal_t` 24 B, `tst_obu_t` 24 B, `tst_descriptor_t` 24 B,
  `tst_stream_info_t` 40 B, `tst_klv_link_t` 8 B,
  `tst_demux_receiver_stats_t` 48 B, `tst_event_t` ≤256 B) — trip
  consumer-side builds on accidental layout drift.

See the Phase 3 plan at `docs/plans/2026-05-16-tst-c-demux-receiver.md`
and the design doc at `docs/specs/2026-05-15-tst-c-receiver-surface-design.md`.
This ships the complete tst-c receiver surface (Phases 1, 2, 3 all
shipped); next-up is `tst-jni` / `tst-uniffi` cross-language bindings.

---

### Phase 2 of tst-c receiver surface

#### Added

- **`tst-c` receiver surface Phase 2** — `tst_receiver_t` and
  `tst_managed_receiver_t` opaque handles wrapping
  `tst_pipeline::Receiver<SrtTransport>`. 14 new C entry points
  (open / open_listener / recv_packet / cancel / close / get_stats /
  reset_stats × plain + managed). `tst_receiver_recv_packet`
  delivers one 188-byte aligned MPEG-TS packet per call with sync
  recovery already done; the syncer counters
  (`bytes_skipped_for_sync`, `resync_events`) reach the C consumer
  via the new `tst_receiver_stats_t` struct.
  See `examples/c/receiving/recv_ts_to_file.c` for the teaching
  example. Phase 3 (`tst_demux_receiver_t` + typed events) remains
  on the P0 backlog.

---

### Phase 1 of tst-c receiver surface

#### Added

- New `TstError::EndOfStream = -12` error code distinguishing peer
  graceful disconnect (FIN) from caller-side `Closed = -7` cancel/close.
- New `tst_raw_receiver_t` opaque handle with the following C entry
  points: `tst_raw_receiver_open(url)`,
  `tst_raw_receiver_open_listener(url)`, `tst_raw_receiver_recv`,
  `tst_raw_receiver_cancel`, `tst_raw_receiver_close`,
  `tst_raw_receiver_get_stats`, `tst_raw_receiver_reset_stats`.
- New `tst_managed_raw_receiver_t` opaque handle with the same 7 entry
  points (managed sibling).
- New `TstRawRecvStats` `repr(C)` struct mirroring
  `tst_pipeline::RawRecvStats`.
- New `tst_*_cancel` entry points for all six sender families
  (`tst_raw_sender_cancel`, `tst_managed_raw_sender_cancel`,
  `tst_sender_cancel`, `tst_managed_sender_cancel`,
  `tst_mux_sender_cancel`, `tst_managed_mux_sender_cancel`) — closes
  the P1 sender-side cancellation deferral.
- New `Mode { Caller, Listener }` enum + `SrtUrl::mode` field on
  `tst-srt`; URL parser now accepts `?mode=listener` and allows empty
  host in listener mode. `tst_*_open_listener` C entry points also
  accept `srt://:port` (empty host) without requiring the explicit
  `?mode=listener` query parameter.
- New C example `recv_raw_to_file.c` (`bindings/c/examples/c/receiving/`).

#### Fixed

- `tst_raw_receiver_recv` now maps `TransportError::Broken` on a
  non-cancelled handle to `TST_E_END_OF_STREAM` (was incorrectly
  surfacing as `TST_E_TRANSPORT`). SRT peer disconnect surfaces as
  `Broken` at the transport layer to support managed-reconnect; the
  plain C ABI semantically translates this to "end of stream".

#### Internal

- Sender handle structs (`TstRawSender`, `TstManagedRawSender`,
  `TstSender`, `TstManagedSender`, `TstMuxSender`, `TstManagedMuxSender`)
  gain a side-channel `Arc<dyn TransportCancel>` + `Arc<AtomicBool>`
  field captured at `_open` time to support thread-safe `_cancel`
  without acquiring the handle's `Mutex`.
- C ABI rustdoc coverage allowlist extended for the 19 new entry
  points; proper `# C ABI` rustdoc backfill on corresponding Rust
  methods deferred to a P2 follow-up.

---

### Rust quality + DX + FFI refactor, phases 1-6 (plans #39-#50 ride along)

Phase 1 (SemVer ratchet), Phase 2 (DX + observability), Phase 3
(FFI-readiness), Phase 4 (performance hot paths), Phase 5
(internal hygiene), and Phase 6 (test infrastructure) of the Rust
quality + DX + FFI refactor. Plan #39 (examples reorganization),
plan #44 (KLV wire-format critical fixes from the 2026-05-10
spec-validation audit), plan #45 (pipeline close-flush and pairer
PTS saturation fixes from the same audit), plan #46 (KLV
follow-up: VMTI checksum ordering + Security LS UL constant),
plan #47 (MPEG-TS PSI multi-section reject + AV1 binding docs),
plan #48 (video codec parser robustness fixes), plan #49
(SRT RejectReason mapping fix), and plan #50 (tst-c FFI panic
isolation) also ride this release.

#### Added

- **OSS-Fuzz onboarding artifacts** (`oss-fuzz/`): `project.yaml`, `Dockerfile`,
  `build.sh`, and `README.md` configure continuous Google-compute fuzzing for
  the 16 cargo-fuzz harnesses (15 in `tst-core`, 1 in `tst-srt`). Includes
  a shared `klv.dict` libFuzzer dictionary, per-target `.options` files for
  the 4 demux/parser targets, and seed corpora for 14 of 16 targets sourced
  from existing fixtures + committed synthetic seeds. The PR to
  `google/oss-fuzz` is a separate manual step documented in
  `oss-fuzz/README.md`.

#### Fixed

- **`parse_pat` / `parse_pmt` OOB on short section_length** — surfaced by
  OSS-Fuzz local smoke (plan #53). Both parsers now reject `section_length`
  below the structural minimum (9 for PAT, 13 for PMT) with the new
  `PsiParseError::SectionTooShort` variant, instead of underflowing the
  CRC slice extraction.

- H.265 SPS parser: fixed bit-cursor misalignment in
  `walk_short_term_ref_pic_sets` that caused `parse_sps` to return
  `ReservedValue { field: "delta_idx_minus1" }` on valid Main10
  conformance vectors. The inter-prediction arm was unconditionally
  reading `delta_idx_minus1` (ue), but per H.265 §7.3.7 that field is
  only signaled when `stRpsIdx == num_short_term_ref_pic_sets` — true
  only in slice-header context, never in SPS context. The bug was
  surfaced by the `DBLK_A_MAIN10_VIXS_4` fixture (plan #55); its entry
  is now removed from the test runner's `KNOWN_PARSER_BUGS` allow-list.

#### Testing

- `scripts/release-validation.sh` steps 3-5 (`tsanalyze` / `tspsi` / `ffprobe`)
  now diff against committed golden files at `tests/golden/baseline-*.{txt,json}`
  instead of printing "no golden yet". A new `--update-goldens` flag refreshes
  the goldens in place when a behavior change is intentional. The script exits
  1 on unexpected divergence. Goldens are produced from a baseline generated by
  `cargo run -p tst-examples --example mux_to_file -- baseline.ts 5`.

- New maintainer tool `corpus_to_fixture` at
  `crates/tst-core/tests/tools/corpus_to_fixture.rs` extracts minimal
  TS-packet sub-sequences from corpus `.ts` files (filtered by PID and/or
  packet-index range) into committed regression fixtures at
  `crates/tst-core/tests/fixtures/regression/`. Optional `--emit-shim`
  generates a Cargo integration test that `include_bytes!`s the fixture
  and smoke-tests it through `Demuxer`. Modeled after TSDuck's
  `ts2headers.sh` capture-then-commit pattern. Invoke via
  `cargo run -p tst-core --bin corpus_to_fixture -- --help`.

#### Internal

- Test infrastructure: new `common::Loopback` + `AcceptHandle<R>` helper
  in `crates/tst-srt/tests/common/mod.rs` consolidates the 15-line
  "bind / spawn accept / signal ready" boilerplate into a 3-line
  builder + closure shape. 18 of 20 integration tests now use the
  helper (net −90 lines across the sweep). Two files don't fit:
  `ipv6_loopback.rs` (helper hardcodes `127.0.0.1:0`) and
  `listener_accept_timeout.rs` (tests `accept_timeout` itself; spawns
  a connector thread — inverse pattern). Pattern from GStreamer's
  `tests/check/elements/srt.c`. Survey item #5.
- CI: new nightly `sanitizers` workflow (`.github/workflows/sanitizers.yml`)
  runs `cargo test -p tst-core -p tst-pipeline` under AddressSanitizer
  and ThreadSanitizer (separate jobs; sanitizers can't combine). Trigger:
  `schedule: '0 3 * * *'` + `workflow_dispatch`. Scope intentionally
  pure-Rust; libsrt + mbedTLS instrumentation is deferred to a follow-up
  plan that threads sanitizer flags into the vendored cmake build.
  Suppression files at `.sanitizer-suppressions/{asan,tsan}.txt`.
  `continue-on-error: true` for the first 2-3 weeks; tighten once stable.
  Survey item #8.

---

#### tst-c FFI panic isolation (2026-05-11) — plan #50

CABI-04 from the 2026-05-10 audit
(`docs/analysis/2026-05-10-audit-slices/15-tst-c-abi.md`). Phase 0
(plan #36, 2026-05-08) wrapped the data path via
`Handle::with_inner_{mut,ref}`; this plan completes the coverage
for the open path, the config-builder setters, and the last-error
accessors. Rust's `extern "C"` panic behavior is implementation-defined
under `panic="unwind"` and aborts under `panic="abort"`; either is
unacceptable for a stable C ABI. After this fix, every panic
inside `tst-c`'s extern "C" boundaries is caught, recorded as
`TstError::PanicCaught` (-11) in the thread-local last-error, and
translated to a sentinel return for the entry point's return type.

##### Fixed (panic-safety hardening)

- **New `bindings/c/src/panic.rs` module** with `pub(crate) fn
  ffi_catch<R, F>(default: R, f: F) -> R` helper. Wraps
  `catch_unwind(AssertUnwindSafe(f))`; on `Err` records `PanicCaught`
  via the existing `record_panic_caught` in `error.rs` (extracts a
  best-effort detail string from the panic payload) and returns the
  caller-supplied default sentinel.

- **All 7 `_open` entry points wrapped**: `tst_muxer_open`,
  `tst_mux_sender_open`, `tst_managed_mux_sender_open`,
  `tst_sender_open`, `tst_managed_sender_open`, `tst_raw_sender_open`,
  `tst_managed_raw_sender_open`. Previously bare against panics in
  `Socket::connect_with` / `MuxerConfig::validate` / `MuxSender::new` /
  URL parsing / `Box::new` allocation. Reconnect-time panics on
  managed senders are already covered by `Handle::with_inner_*`
  wrapping (the factory runs from the data path).

- **All 25 config-builder entry points in `config.rs` wrapped**:
  4 `_new` constructors (default `null_mut`), 4 `_free` destructors
  (default `()`), `tst_mux_config_add_program` (default
  `TST_INVALID_PROGRAM_HANDLE`), `tst_mux_config_add_video_stream` /
  `_add_klv_stream` (default `TST_INVALID_STREAM_HANDLE`), and 14
  `c_int` setters covering PCR/PSI/buffer + 3 descriptor setters +
  2 sender setters + 5 reconnect setters (default `TstError::Internal
  as i32`). Vec::push, Vec::with_capacity, parse_tlv_list slice
  arithmetic, and Box::new are all panic surfaces; previously a panic
  in any of them would unwind through extern "C".

- **Both last-error accessors wrapped**: `tst_get_last_error`
  (default `TstError::Internal as i32`) and `tst_get_last_error_str`
  (default: pointer to a static `b"\0"` byte slice). The static
  fallback on `_str` preserves the never-NULL contract documented
  in the rustdoc — a reentrant Drop double-borrow of the
  thread-local `RefCell` previously could panic out of `borrow()`.

- **6 inline unit tests** in `panic::tests` pin the contract:
  closure value passes through on success; panic returns the
  default and records `PanicCaught`; payload detail captured for
  both `&'static str` and formatted `String` payloads; null-ptr
  default works; void default works; open-path simulated panic
  (regression test for the architectural property).

No API change; no symbol changes. The `cargo public-api` baseline
stays byte-identical (the new `panic` module is private and the
`ffi_catch` helper is `pub(crate)`). The fix is binary-compatible
with existing linked C consumers.

---

#### SRT RejectReason mapping fix (2026-05-11) — plan #49

SRT-01 from the 2026-05-10 audit
(`docs/analysis/2026-05-10-audit-slices/14-srt-bindings.md`).
`tst_srt::error::RejectReason::from_raw` previously mapped raw codes
`1001..=1014` as if they were the internal `SRT_REJECT_REASON` enum
offset by 1000 — they're actually the `SRT_REJX_*` HTTP-style
extension codes from `access_control.h`, which are set by remote
services via `srt_setrejectreason` and live in a different code
category entirely. Effects:

- Every libsrt-emitted handshake reject (bad passphrase, version
  mismatch, backlog, timeout, …) was reported as `Other(raw)`
  instead of its typed variant. The `tests/handshake.rs` integration
  test for passphrase mismatch was hitting its `eprintln!` "log it
  but don't fail" fallback because raw 10 (`SRT_REJ_BADSECRET`) was
  never matching the `BadSecret` arm.
- Conversely, an extension code 1001 (`SRT_REJX_KEY_NOTSUP` —
  StreamID key not supported) was being reported as `BadSecret`.

##### Fixed (breaking — `tst-srt`)

- **`tst_srt::error::RejectReason`** rewritten per `srt.h:535-558`
  (internal `SRT_REJC_INTERNAL`, ordinals 0..=17) and
  `access_control.h:21-71` (predefined `SRT_REJC_PREDEFINED`,
  1000..=1999):
  - **Removed** `ValueLearn` and `UnknownStreamId` — never existed
    in libsrt; mid-design guesses.
  - **Added** internal-category variants `Unknown` (0), `System` (1),
    `Peer` (2), `MessageApi` (12), `Congestion` (13), `Filter` (14),
    `Group` (15), `Timeout` (16), `Crypto` (17).
  - **Added** extension-category variants `Fallback` (1000),
    `KeyNotSupported` (1001), `Filepath` (1002), `HostNotFound`
    (1003), `Unauthorized` (1401), `Overload` (1402), `BadMode`
    (1405), `Unacceptable` (1406), `Conflict` (1409),
    `NotSupportedMedia` (1415), `Locked` (1423), `FailedDependency`
    (1424), `InternalServerError` (1500), `Unimplemented` (1501),
    `Gateway` (1502), `Down` (1503), `VersionUnsupported` (1505),
    `NoRoom` (1507).
  - **Behavioral rename** of existing variants: `BadSecret`,
    `Unsecure`, `Version`, `Resource`, `Rogue`, `Backlog`, `Ipe`,
    `Close`, `RdvCookie`, `BadRequest`, `Forbidden`, `NotFound`
    keep their identifiers but now map to the spec-correct raw
    codes (mostly small ordinals, not 1000+). Match-arms in
    downstream code still compile but the runtime category they
    catch shifts.
  - The enum remains `#[non_exhaustive]`; `Other(i32)` covers
    `SRT_REJC_USERDEFINED` (2000+) and unknown codes within either
    typed range.

- **`tst_srt::error::ConnectError::Rejected` sentinel check**
  shifted from `reason != RejectReason::Other(0)` to
  `reason != RejectReason::Unknown`. libsrt's `SRT_REJ_UNKNOWN`
  is raw 0, not 1000-and-something — the previous sentinel was
  consistent with the (broken) enum mapping and stops being
  meaningful after the fix.

- **`crates/srt-sys/wrapper.h`** now `#include`s
  `<srt/access_control.h>` so the 21 `SRT_REJX_*` constants are
  exposed as `pub const SRT_REJX_*: u32` in the generated
  bindings. `tst-srt` uses these to drift-detect upstream
  renumbering via the new
  `reject_reason_extension_named_constants` test.

---

#### Video codec parser robustness fixes (2026-05-11) — plan #48

Three decoder-side robustness fixes from the 2026-05-10 audit
(`docs/analysis/2026-05-10-audit-slices/07-codec-h264.md` H264-01,
`.../16-codec-h266-vui-h274.md` H274-01 + H274-02). Library does not
encode H.264 or H.266, so the only behavior change is decoder-side:
malformed input that previously surfaced as `Ok(garbage)` now surfaces
as a typed `CodecParseError`.

##### Fixed (decoder behavior on malformed input)

- **`codec::h264::parse_sps`** — When the underlying `h264-reader`
  decoder surfaces `chroma_format_idc` outside the spec range (H.264
  V15 §7.4.2.1.1: shall be in 0..=3), `parse_sps` now returns
  `CodecParseError::ReservedValue { field: "chroma_format_idc", value }`
  instead of silently coercing the value to `Yuv420` and continuing.
  The previous behavior produced a `H264Sps` with crop offsets computed
  against the (different) original `chroma_format` and a fabricated
  chroma bit-depth.

- **`codec::h266::parse_sps`** — Now correctly consumes the optional
  `vui_payload(payloadSize)` tail per H.266 V4 §7.3.2.21 —
  `vui_parameters()` (H.274 §7.2) may not consume all `8 * payloadSize`
  bits, and the SPS caller must advance the cursor to the declared
  payload end before reading `sps_extension_flag`. Previously the
  parser mis-framed `sps_extension_flag` for any encoder that emitted
  the optional `vui_reserved_payload_extension_data` + marker +
  zero-pad tail. The `parse_h266_vui` `pub(super)` function signature
  also drops the unused `_payload_size_bytes` argument —
  tail-consumption is now correctly placed in the SPS caller.

- **`codec::h266` VUI parser** — `vui_chroma_sample_loc_type_frame`,
  `vui_chroma_sample_loc_type_top_field`, and
  `vui_chroma_sample_loc_type_bottom_field` are now validated against
  the H.274 V4 §7.3 (p. 20) range 0..=6 inclusive. Previously the
  parser used `read_ue()? as u8`, which silently accepted out-of-range
  values up to 255 and silently truncated values ≥ 256 to a valid
  in-range value (e.g. 256 → 0). All three sites now return
  `CodecParseError::ReservedValue` with the original `u32` value
  preserved.

##### Docs

- **`ColorInfo::chroma_loc`** rustdoc — H.274 V4 §7.3 (p. 20)
  inference rule documented: when `vui_chroma_loc_info_present_flag
  = 0` AND `ChromaFormatIdc == 1`, the spec infers
  `vui_chroma_sample_loc_type_frame = 6`. The parser leaves
  `chroma_loc = None` to preserve the "absent" vs "absent and inferred"
  distinction; callers needing the inferred value substitute 6
  themselves.

---

#### MPEG-TS PSI multi-section reject + AV1 binding docs (2026-05-11) — plan #47

Two audit-driven fixes at the MPEG-TS layer from
`docs/analysis/2026-05-10-audit-slices/05-mpegts-demux.md` (DEMUX-01)
and `docs/analysis/2026-05-10-audit-slices/10-codec-av1.md` (AV1-05).

##### Public API

- **`klv::st0903`** unchanged. New variants additive on
  `#[non_exhaustive]` enums:
  - `mpegts::demux::event::NonConformantIssue::PsiMultiSectionUnsupported { pid, table_id, last_section_number }`
  - `mpegts::demux::psi::PsiParseError::MultiSectionUnsupported { table_id, last_section_number }`

##### Fixed

- **`mpegts::demux::psi`** — multi-section PSI tables
  (`last_section_number > 0` per H.222.0 §2.4.4.5) are now rejected
  with a new `PsiParseError::MultiSectionUnsupported` and surfaced as
  a typed `NonConformantIssue::PsiMultiSectionUnsupported` event.
  Prior behavior silently overwrote sibling sections via the
  version-dedup path, so a multi-section PMT delivered streams from
  only the last-arriving section. Full §2.4.4.5 reassembly is
  deferred until a consumer needs it (the corpus has zero
  multi-section captures — MISB-shaped ISR streams pack everything
  into a single section well under the 1021-byte short-form cap).
  Audit slice 05 finding DEMUX-01.

##### Docs

- **AV1 binding deviations** — `docs/deferred-features.md` AV1
  binding-§3.2/§3.4 carriage entry corrected: prior entry claimed
  `data_alignment_indicator=1` was not set; the muxer in fact does
  set it correctly for AV1 video PES. Entry reduced from three
  deviations to two (§3.2 framing + §3.4 stream_id). Inline rustdoc
  added at the AV1 PES writer site
  (`crates/tst-core/src/mpegts/mux/mod.rs`) pointing back to the
  deferred-features entry for the binding-deviation rationale.
  Audit slice 10 finding AV1-05.

---

#### KLV follow-up — VMTI checksum + Security LS UL (2026-05-10) — plan #46

Two High-severity audit findings from `docs/analysis/2026-05-10-audit-slices/03-klv-other-sets.md` closed in this slice.

##### Public API

- **`klv::universal_label::UniversalLabel::SECURITY_LS_UL`** — new
  16-byte UL constant per MISB ST 0102.12 §6.7
  (`06.0E.2B.34.02.03.01.01.0E.01.03.03.02.00.00.00`, CRC 40980).
- **`klv::st0102::SECURITY_LS_UL`** — raw `[u8; 16]` re-export mirroring
  the `klv::st0903::VMTI_LS_UL` precedent. Used by consumers detecting
  the standalone (non-Tag-48-nested) Security LS carriage path.
- **`klv::st0903::encode_standalone(&VmtiLs, &mut [u8]) -> Result<usize, _>`**
  — new self-checksumming entry for standalone-VMTI carriage. Emits
  `[VMTI_LS_UL:16][outer BER length][body][Tag 1 checkSum TLV]` per
  ST 0903.4-17 / ST 0903.6-119. The Tag 1 value is the running 16-bit
  unsigned summation per §10.1.1.
- **`klv::st0903::encode_to_vec_standalone(&VmtiLs) -> Result<Vec<u8>, _>`**
  — convenience over `encode_standalone` allocating a fresh buffer.
- **`klv::st0903::encoded_len_standalone(&VmtiLs) -> usize`** — sizing
  helper for the standalone path.

##### Changed (wire-format)

- **`klv::st0903::encode` / `encode_to_vec`** is now exclusively the
  **embedded-VMTI body** entry — Tag 1 (checkSum) is silently dropped
  per ST 0903.6-120 ("where the VMTI LS is embedded-VMTI, the VMTI LS
  checkSum (Item 1) shall be omitted"). Any value the caller stored in
  `VmtiLs::checksum` is ignored. Callers who want a self-checksummed
  standalone-VMTI wire record use `encode_to_vec_standalone`. Decode is
  unchanged: `VmtiLs::checksum` still captures the Tag 1 value when
  present on the wire.

##### Tests

- Eight new regression tests pin the new contracts: `encode_omits_tag1_checksum_per_st0903_6_120`, `encode_drops_caller_supplied_checksum`, `encode_standalone_emits_tag1_last_per_st0903_4_17`, `encode_standalone_checksum_matches_running_sum_16`, `encode_standalone_round_trips_via_decode`, `encoded_len_standalone_matches_encode_standalone`, `security_ls_ul_canonical_bytes`, `security_ls_ul_reexport_matches_universal_label`.

---

#### Pipeline close-flush and pairer PTS saturation fixes (2026-05-10) — plan #45

Three High-severity correctness fixes in `tst-pipeline` from the 2026-05-10
spec-validation audit (slice 13). No wire-format change — behavioral +
arithmetic fixes only.

##### Fixed (lifecycle / arithmetic)

- **`tst_pipeline::Sender::close`** — now best-effort flushes the
  buffered partial bundle before marking closed, matching `Drop`
  semantics (PIPE-01). Pre-fix, callers using the AutoCloseable /
  `__exit__` / `.use { }` / `tst_sender_close(...)` idioms could
  silently drop 1–6 partial TS packets sitting in `TsFraming::buffer`.

- **`tst_pipeline::MuxSender::close`** — now best-effort drains
  `pending_bytes` before marking closed, matching `Drop` semantics
  (PIPE-02). Pre-fix, queued back-pressure-buffered chunks were
  silently abandoned on explicit close. Cancel-first ordering is
  preserved (the `close_unblocks_parked_sender_thread` test continues
  to pass). Also: `close()` now gracefully handles a poisoned inner
  mutex via `if let Ok` instead of `.unwrap()`, parity with Drop.

- **`tst_pipeline::pairing::Pairer`** — nearest-mode arithmetic now
  uses `saturating_add` / `saturating_sub` at the three flagged sites
  in `nearest.rs` (PIPE-03 item 1). Pre-fix, PTS values approaching
  `i64::MAX` (theoretical, or from misconfigured sources) would
  overflow — panic in debug, silent wrap in release. The
  `pairing/mod.rs` module-doc has been rewritten (PIPE-03 item 2) to
  accurately describe the demuxer's PTS shape: per-event values
  bounded `0..(2^33 − 1)` per H.222.0 §2.4.3.7, with explicit
  semantics across the 33-bit rollover boundary.

##### Tests

- `close_flushes_buffered_partial_packets` pins the PIPE-01 contract
  via a Recorder transport.
- `close_drains_pending_bytes` pins the PIPE-02 contract via a
  `BackpressureOnce` transport with an external `Arc<Mutex<Vec<u8>>>`
  snoop slot.
- `close_does_not_panic_on_poisoned_lock` pins the poisoned-lock parity
  ride-along via a `PanicOnSend` transport.
- `near_i64_max_pts_does_not_overflow_buffered_drain` and
  `near_i64_max_pts_does_not_overflow_realtime_match` pin the PIPE-03
  arithmetic contract; both panic pre-fix in debug, both pass post-fix.

---

#### KLV wire-format critical fixes (2026-05-10) — plan #44

Two wire-format-incompatible KLV defects from the 2026-05-10 spec-validation
audit. Both defects predate any external consumer; pre-1.0 break per the
break-freely policy.

##### Fixed (wire-format breaking)

- **`klv::imapb`** — encoder now writes unsigned big-endian per ST 1201.5
  §7.2.3 Table 1; previously emitted signed two's-complement, MSB-flipping
  every value. Also: truncate (not round) per §7.2.1 step 4a; proper
  `Zoffset = sF·a − floor(sF·a)` per §7.1.2 step 6 when the range straddles
  zero. Length cap widened from `1..=7` to `1..=8`. Affects every ST 0903
  VMTI emit and the VTargetPack IMAPB-encoded tags 10-16. Internal
  round-trips were previously consistent (encode + decode agreed on the
  wrong algorithm), masking the wire-format break. Supersedes the
  `length: 1..=7` claim from the Phase 6 entry below — Phase 6 introduced
  the typed error variants; plan #44 widens the cap to spec.

- **`klv::st0601::UasDatalinkLs`** — Tag 50 is now correctly typed as
  Platform Angle of Attack (int16 mapped to ±20°, sentinel `0x8000`) per
  ST 0601.19 §8.50; the previous "Platform Call Sign" typing was a
  misidentification. Platform Call Sign moves to Tag 59 (utf8 ≤ 127 B)
  per §8.59. New struct field: `platform_angle_of_attack_deg: Option<f64>`.
  Existing `platform_call_sign: Option<String>` field is preserved by
  name but now serializes to Tag 59. The `KlvEncodeError::StringTooLong`
  emitted for an over-length call sign now reports `tag: 59`.

##### Tests

- New ST 1201.5 spec-vector tests in `klv::imapb`: Appendix A Tests 2 + 3,
  ST 0903.6 §10.1.11 worked example (FOV 12.5° / 10.0° / 90.0°), and an
  L=8 round-trip.
- New ST 0601 wire-pin tests: `tag_50_is_platform_angle_of_attack_int16_per_spec`
  and `tag_59_is_platform_call_sign_utf8_per_spec`.
- Synthetic fixture `synthetic_full.klv` regenerated to exercise both
  Tag 50 and Tag 59 in the integration-test fixture-decode path.

##### Substrate cleanup

- `klv::imapb::ImapbParams` lost private `scale()` + `signed_offset()`
  methods; gained `sf()` + `z_offset()` per ST 1201.5 §8.9 Summary.
- Dead `write_signed_be` + `read_signed_be` helpers removed (audit
  finding KLV-SUB-09 — `1u64 << 64` UB risk at n=8 obviated by deletion).

---

#### Phase 6 — Test infrastructure (2026-05-10)

Test-infrastructure improvements and two latent-bug fixes surfaced
by the new property tests. 12 commits `c94881d..016c3e4`.

##### Public API

Two latent substrate issues surfaced by Phase 6's new property tests
fixed at the type level (pre-1.0 break per the break-freely policy):

- **KLV ST 0102 / ST 0903 PartialEq:** `SecurityLs::eq`, `VmtiLs::eq`,
  and `VTargetPack::eq` no longer compare `field_errors`. Two LSes
  that produced identical field values are now semantically equal
  regardless of which fields failed strict decode. `field_errors`
  is a decoder-side diagnostic, not part of the LS value.
  `PartialEq` trait surface unchanged; `StructuralPartialEq` impls
  removed (auto-generated by derive but not by manual impl).
- **IMAPB length cap:** `encode_imapb` and `decode_imapb` now reject
  `length` not in `1..=7` with new typed variants
  `KlvEncodeError::UnsupportedImapbLength` and
  `KlvFieldError::UnsupportedImapbLength`. Previously, `length >= 8`
  caused `i64` overflow in `ImapbParams::signed_offset` — panic in
  debug, silent wrap in release. ST 1201.5 defines IMAPB for any
  L-byte width; this is an internal-arithmetic limitation. Both
  error enums are `#[non_exhaustive]`; the addition is
  forwards-compatible.

##### Test infrastructure

- **Loopback probe + atomic-signal helpers** in `crates/tst-srt/tests/common/mod.rs`:
  `loopback_probe()`, `require_loopback!()` macro, `wait_for_ready(&AtomicBool)`.
- **28 default-running loopback tests probe-gated** via `require_loopback!()`
  across 19 files. Sandbox/restricted CI environments now emit
  `SKIP: loopback unavailable` instead of failing dozens of tests.
- **7 listener-settle sites migrated** from `thread::sleep(50ms)`
  to `wait_for_ready(&AtomicBool)` atomic-signal poll. Matches the
  `accept_done` precedent from `cancellation_loopback.rs`. Bonus:
  fixes a previously-flaky `accept_timeout_succeeds_when_peer_connects`
  test by eliminating the race window.
- **H.266 parameter-set parsers added** to the existing
  `parse_parameter_sets` fuzz target (target count stays at 16).
- **KLV ST 0102 + ST 0903 fuzz targets** upgraded from panic-only
  to decode→encode→decode round-trip identity. ST 0102 filters
  inputs containing multi-byte BER-OID unknown tags (documented
  encoder limitation). Codec parameter sets (H.264/H.265/H.266) and
  AV1 sequence header stay panic-only — no encoder counterpart.
- **6 new property tests:**
  - `tst-core/tests/klv_proptest.rs`: BER round-trip, BER-OID round-trip,
    IMAPB round-trip (lerp value generation, scale-factor tolerance
    with f64 ULP floor).
  - `tst-core/tests/mpegts_psi_proptest.rs`: PSI mux→demux round-trip,
    descriptor build/parse round-trip, `Demuxer::feed` chunking
    invariance.
- **3 new CI rails:**
  - `cargo test --workspace --all-features` (closes feature-matrix gap).
  - `linux-musl-x86_64` (tst-core + tst-pipeline scope; libsrt-bound
    crates need a deeper rework for musl-native libsrt).
  - Nightly fuzz compile smoke (`cargo +nightly fuzz check` for both
    fuzz crates).

Test count: 1320 → 1326 default-features (+6: 3 KLV proptests + 3 PSI
proptests).

---

#### Phase 5 — Internal hygiene (2026-05-10)

God-module splits, test-helper de-duplication, fuzz-target relocation,
focused dead-code sweep. 15 commits `7ab2ffb..2709572`.

##### Public API

The four moved-types' canonical paths shifted (user-facing re-exports
preserved):

- `tst_core::mpegts::mux::*` types: `VideoCodec`, `KlvStreamType`,
  `AudioCodec`, `SubtitleCodec`, `StreamKind`, `TeletextField`,
  `StreamSpec`, `VideoStreamHandle`, `KlvStreamHandle`,
  `AudioStreamHandle`, `SubtitleStreamHandle` now resolve via
  `mpegts::mux::types::*` (cargo-public-api visible canonical path).
  User-facing `tst_core::mpegts::mux::*` re-exports unchanged.
- `tst_core::mpegts::mux::*` configuration types: `MuxerConfig`,
  `MuxerConfigBuilder`, `MuxerProgramConfig`, `MuxerProgramConfigBuilder`
  now resolve via `mpegts::mux::config::*`. User-facing paths unchanged.
- `tst_core::mpegts::demux::*`: `DemuxerStats`, `DemuxerOptions`,
  `DemuxerBuilder`, `ProgramTracker` now resolve via
  `mpegts::demux::types::*`. User-facing paths unchanged.
- `tst_core::codec::h265::bitreader` → `tst_core::codec::bitreader`
  (codec-agnostic; Annex-B reader is consumed by both H.265 and H.266
  parsers). `BitReader` is `#[doc(hidden)]` since Phase 3.6.1; not
  user-facing.

`klv::pack::Iter` retained as `#[doc(hidden)] pub` (the audit's claim
that fuzz-target relocation enables a `pub → pub(crate)` tightening was
structurally incorrect — `cargo-fuzz` creates a separate crate; tightening
remains gated on either a `#[cfg(fuzzing)] iter_for_fuzz` entry point or
deletion of the `klv_iter` fuzz target).

##### Internal restructure (Phase 5)

- **`mpegts::mux::types`** (NEW, ~485 LoC): codec/stream-class enums,
  `StreamSpec`, four opaque stream-handle types extracted from
  `mpegts/mux/mod.rs`. Re-exported via `pub use types::*;`.
- **`mpegts::mux::config`** (NEW, ~873 LoC): `MuxerProgramConfig`,
  `MuxerConfig`, `MuxerConfigBuilder`, `MuxerProgramConfigBuilder`
  extracted; private `validate_language_code` helper migrated alongside.
- **`mpegts::common::handle_pack`** (NEW): four byte-near-identical
  `pack` / `unpack` impls on `Video`/`Klv`/`Audio`/`SubtitleStreamHandle`
  collapse to one shared substrate. Defensive `& WITHIN_MASK` form
  applied uniformly (was already present in Audio/Subtitle; behavior
  identical on valid inputs, slightly safer in release on out-of-range).
- **`mpegts::demux::types`** (NEW, ~152 LoC): `DemuxerStats`,
  `DemuxerOptions`, `ProgramTracker`, `DemuxerBuilder` extracted from
  `mpegts/demux/demuxer.rs`. Private `DEFAULT_PES_CAP_*` constants
  consolidated into the existing `pub(crate) const fn` accessors.
- **`codec::bitreader`** (PROMOTED from `codec::h265::bitreader`):
  Annex-B Exp-Golomb reader is consumed by both `codec::h265::*` and
  `codec::h266::*`. File-level `#[allow(dead_code)]` removed.
  `BitReader::bit_cap` field and `BitReader::at_end()` method deleted
  (no consumers).
- **`tst-test-helpers`** (NEW workspace member, `publish = false`):
  consolidates `synthetic_nal` (47 LoC) + `ts_parser` (218 LoC) +
  `mock_transport` (82 LoC) — three modules previously byte-identical
  across `tst-core/tests/common/`, `tst-pipeline/tests/common/`, and
  `tst-srt/tests/common/`. ~10 consumer test files swap their import
  paths to `tst_test_helpers::*`. `tst-core` and `tst-pipeline` gain
  the dev-dep; `tst-srt` already had it from Task 8.
- **`crates/tst-core/fuzz/`** (NEW cargo-fuzz crate): hosts 15 of 16
  fuzz targets. `tst-srt/fuzz/` retains only `url_parse` (URL parsing
  lives in `tst-srt`); the `tst-core` dep dropped from
  `tst-srt/fuzz/Cargo.toml`. Six corpus subdirectories moved alongside.
  `mux_push_klv` arity fix landed during the relocation
  (`push_klv(data, 0)` → `push_klv(data, 0, 0)` — pre-existing
  breakage from plan #30's `metadata_service_id` addition).
- **Dead-code annotations**: 8 module-level `#![allow(dead_code)]`
  before; 4 after. 3 file-level annotations removed; 1 confirmed-dead
  item deleted (`Av1BitReader::buf_len_bits`); 2 narrow per-item
  allows added (`Av1BitReader::byte_align`, `bit_pos` — used only by
  inline `#[cfg(test)]` blocks; clippy can't see test consumers). The
  `tst-srt/tests/common/mod.rs` and bindgen-generated `srt-sys/src/lib.rs`
  annotations left in place. (Audit estimated 43 annotations; reality
  was 8 — most already swept by prior phases incidentally.)
- **Orphan dirs deleted**: `crates/tst-core/tests/fixtures/{h266,av1}/`
  (each contained only a stale `regen.sh` producing files no test
  reads; canonical fixtures at `tests/fixtures/codec/{h266,av1}/`),
  and `crates/tst-srt/fuzz/corpus/klv_st1910_unwrap/` (target deleted
  in plan #25's AU cell rework; corpus subdir was missed).

##### File size deltas

- `mpegts/mux/mod.rs`: 5489 → 4151 LoC (-1338, -24%).
- `mpegts/demux/demuxer.rs`: 3290 → 3158 LoC (-132).
- `codec/h265/bitreader.rs` (219 LoC) → `codec/bitreader.rs` (211 LoC,
  -8 from dead-item deletions).

##### Tests

- 1320 passing on default features (matches pre-Phase-5 baseline).
- 1319 passing on `--no-default-features` (matches prior Phase 3-Phase 4
  numbers).
- All 3 Phase 3 CI ratchets clean
  (`check-c-abi-rustdoc-coverage`, `check-close-contract-presence`,
  `check-no-public-usize`).
- Public-API baselines refreshed for `tst-core` and `tst-pipeline` to
  reflect the moved-types canonical-path renames; `tst-srt` baseline
  unchanged.
- `cargo public-api` surface is unchanged at user-facing-paths level
  (re-exports via `mpegts::mux::*` / `mpegts::demux::*` preserve every
  caller-visible import).

---

#### Phase 4 — Performance hot paths (2026-05-10)

Bench-driven receiver + sender optimizations. 6 bench targets / 21
sub-benches established (Tasks 1–7); 5 optimization candidates committed
(Tasks 8–10, 12–13); 3 dropped per decision rule (Tasks 11, 14, 15).

##### Added (Phase 4)

- **`Demuxer::feed_aligned(&[u8; 188]) -> Result<...>`** — fast path
  that skips the sync-search buffer entirely when the caller guarantees
  the input is already a valid 188-byte packet aligned on the 0x47 sync
  byte. `DemuxReceiver` is wired to use this path internally.
  Eliminates the slice-copy-into-sync-buf round-trip on the common
  in-sync case. **-12 to -13%** on the `demux_feed_per_188` bench.

##### Performance (Phase 4)

- **Syncer ring buffer** (`tst-pipeline::pipeline::syncer`): replaced
  `buf: Vec<u8>` + `to_vec() + drain()` per-packet pattern with a
  hand-rolled ring (`head: usize` cursor, compaction at the 1316-byte
  SRT-datagram threshold). `Receiver::next_packet` now returns
  `[u8; 188]` by value (drops the `try_into().unwrap()` at call sites).
  **-60%** on `syncer_aligned_steady_1000`. (Audited estimate was 2–4%;
  the measured gain reflects that the original path paid both a heap
  allocation and a full memmove of the trailing buffer on every emit.)

- **`pid_to_program` HashMap** (`tst-core::mpegts::demux`): replaces
  the O(programs × streams) linear scan in `program_number_for_pid`
  with a `HashMap<u16, u16>` populated at PMT-handle time and cleared
  on PAT version bumps. **-9 to -10%** on `demux_feed_per_188`.

- **Muxer `pes_scratch` field**: single `Vec<u8>` reused across the 4
  PES build sites (audio / subtitle / video / KLV) instead of separate
  `Vec::with_capacity` per call. Drops 4 heap allocations per AU.
  **-5 to -8%** on `mux_end_to_end_30frames`, **-5 to -10%** on
  `push_klv_1kb`.

- **Continuity counters flat array**: `BTreeMap<u16, u8>` continuity
  counter table replaced with `Box<[u8; 8192]>` indexed by the 13-bit
  PID field; 4-bit CC masking retained. Drops the now-stale "≤4 PIDs"
  comment that rationalized the original map. **-6 to -11%** on
  `mux_end_to_end_30frames`.

- **Dropped: PMT cache + CC patch** (Task 11): -1.3% / -4.5% across
  two passes — both below the 5% host-noise-adjusted threshold;
  invalidation surface area not justified by gain.

- **Dropped: adaptation-field stuffing `fill(0xFF)`** (Task 14):
  codegen inspection showed LLVM already lowers the existing
  `for byte in &mut out[..] { *byte = 0xFF }` loop to `memset@plt`.
  No change needed.

- **Dropped: profile-guided `#[inline]` sweep** (Task 15): `perf` not
  installed on the build host; profiling was blocked. Zero `#[inline]`
  additions — valid outcome per plan.

---

#### Phase 3 — FFI-readiness (2026-05-09)

Final phase of the quality + DX + FFI refactor. Six sub-phases:
3.1 (pipeline shell aliases + `Box<dyn>` blanket impls + binding-author
starter doc), 3.2 (audio frame `Owned` siblings + AV1 panic
inventory), 3.3 (stream handle opacity), 3.4 (builder reshape to
`&mut self -> &mut Self`), 3.5 (targeted API reshape: `PairerOptions`,
`usize → u64`, `CancelHandle` relocation), 3.6 (visibility + close
contracts + Rust↔C ABI cross-references + three CI ratchets).

##### Added (Phase 3 / sub-phase 3.1 — pipeline shell aliases)

- **Six `BoxedXxx` dyn-erased aliases** in `tst-pipeline` for binding
  generators (UniFFI / JNI / PyO3) that need a single concrete type
  per shell shape regardless of the underlying transport:
  `BoxedMuxSender`, `BoxedSender`, `BoxedRawSender`,
  `BoxedDemuxReceiver`, `BoxedReceiver`, `BoxedRawReceiver`. Collected
  into the new `tst_pipeline::dyn_aliases` module with crate-root
  re-exports.
- **Blanket `Transport` and `RecvTransport` impls for `Box<T: ?Sized>`**
  in `tst-core/src/transport.rs`. Without these, `Box<dyn Transport>`
  doesn't satisfy `T: Transport`, making the dyn-erased aliases
  type-level landmines. Added mid-execution after the Phase 3 plan was
  found to miss this prerequisite.
- [`docs/binding-authors.md`](./docs/binding-authors.md) — ~150-line
  starter guide for `tst-jni` / `tst-uniffi` / `tst-pyo3` authors.
  Worked Kotlin/Swift/Python/C examples plus builder + cancel-handle +
  threading + versioning sections.

##### Added (Phase 3 / sub-phase 3.2 — audio frame `Owned` siblings)

- **`codec::aac::AdtsFrameOwned`** — 11-field owned mirror of
  `AdtsFrame<'a>` with symmetric `to_owned()` / `as_ref()` round-trip.
  FFI-shaped collect-pattern doctest verifies the borrow→own→reborrow
  cycle works for binding consumers that need to retain frames
  across calls.
- **`codec::mpegaudio::FrameOwned`** — 10-field owned mirror of
  `mpegaudio::Frame<'a>` with the same symmetric round-trip and
  doctest pattern.
- **AV1 panic inventory was clean** — Phase 0 had already done the
  hardening (35 panic-shaped sites, all in `#[cfg(test)]` blocks).
  Closed Phase 0 deferral 3.2f with 10 regression tests at
  `tests/av1_no_panic.rs` exercising the production paths under
  truncation / oversized-leb128 / bit-overflow inputs.

##### Changed (Phase 3 / sub-phase 3.3 — stream handle opacity)

- **`VideoStreamHandle::{pack, unpack, raw, from_raw}` and
  `KlvStreamHandle::{pack, unpack, raw, from_raw}` are now
  `#[doc(hidden)]`.** They remain `pub` (load-bearing for the `tst-c` C
  ABI, which converts handles to/from `uint32_t` across crate lines), but
  they no longer appear in rustdoc. Binding generators (UniFFI / JNI /
  PyO3) that scan the public surface won't surface them; the Java `int` /
  Swift `UInt32` paths to construct invalid handles are eliminated.
  Full `pub(crate)` demotion of these two handle types is deferred to a
  future plan that reshapes `tst-c` to use opaque handles internally.
  Direct Rust callers should obtain handles via `Muxer::add_video_stream`,
  `Muxer::video_handles`, `Muxer::add_klv_stream`, or
  `Muxer::klv_handles` — those are the stable API entry points.

- **`AudioStreamHandle::{pack, unpack}` and
  `SubtitleStreamHandle::{pack, unpack}` are now `pub(crate)`.** No
  external consumers exist (tst-c does not bind audio or subtitle handles
  at the C ABI boundary yet). The `from_raw` / `raw` helpers on these
  types were test-only; they are now `#[cfg(test)] pub(crate)`.

##### Changed (Phase 3 / sub-phase 3.4 — builder reshape)

- **Breaking:** Every public builder converted to `&mut self -> &mut Self`
  chainable shape:
  - `MuxerConfigBuilder` (all methods + `build()` → `&self`)
  - `MuxerProgramConfigBuilder` (all methods + `build()` → `&self`); also
    restructured to be standalone (no longer owns parent),
    `MuxerConfigBuilder::add_program` now takes a `MuxerProgramConfig`
    value, `end_program()` removed
  - `SocketBuilder` (all methods + `connect()` / `config()` → `&self`);
    `try_stream_id` now `Result<&mut Self, _>`
  - `ListenerBuilder` (all methods + `bind()` / `config()` → `&self`)
  - Descriptor-setter methods (`stream_descriptors_for_video`/`klv`/
    `audio`/`subtitle`/`stream`) on `MuxerProgramConfigBuilder` switched
    from deferred-error semantics to immediate-error
    `Result<&mut Self, MuxError>`.

  Migration: replace `Builder::new().method(x).method(y).build()` with
  `let mut b = Builder::new(); b.method(x); b.method(y); b.build()`.
  For `MuxerProgramConfigBuilder`: build the program standalone, then
  pass the value:
  `let prog = { let mut p = MuxerProgramConfigBuilder::new(num, pid); p.add_video(...); p.build() }; b.add_program(prog);`.

  Rationale: closes audit theme H5; required for clean Kotlin
  `.apply { }`, Swift `var b`, Java chaining, Python step-wise, and C
  opaque-handle binding patterns. See
  [`docs/binding-authors.md`](./docs/binding-authors.md).

##### Changed (Phase 3 / sub-phase 3.5 — targeted API reshape)

- **Breaking:** `Pairer::nearest_pts(video_pid, klv_pid, tolerance_ticks,
  max_klv_history, mode)` removed. Use
  `Pairer::with_options(video_pid, klv_pid, PairerOptions { ... })`
  instead — field-style construction with explicit `Duration` units
  composes cleanly across binding languages.
- **Breaking:** `Pairer::last_before_pts`: the third argument changed
  from `Option<i64>` (90 kHz ticks) to `Option<Duration>`. Same upgrade
  rationale: explicit units, idiomatic across language boundaries.
- **Breaking:** `MatchMode` enum removed. `PairerMode` is the path
  forward (`Realtime` and `Buffered { max_lag: Duration }`); marked
  `#[non_exhaustive]` so future variants don't break the SemVer ratchet.
- **Breaking:** `MuxError::BufferFull::capacity_packets`: `usize` →
  `u64`. JNI / UniFFI / cbindgen don't have a stable mapping for `usize`
  (32-bit on 32-bit targets, 64-bit on 64-bit targets); `u64` is
  unambiguous across architectures.
- **Breaking:** `Muxer::pending_packets()` and
  `Muxer::capacity_packets()` return `u64` (were `usize`).
- **Breaking:** `TsFramingError::SyncLost::offset` and
  `TsFramingError::NoSyncAfterLimit::max`: `usize` → `u64`.
- **Breaking:** `CancelHandle` type relocated from `tst-srt` to
  `tst-core`. The pipeline-layer cancel mechanism now lives in
  `tst_core::cancel`; `tst-pipeline` and `tst-srt` re-export it as
  `tst_pipeline::CancelHandle` and `tst_srt::CancelHandle` so binding
  authors have a single import path. Removes the libsrt-drag concern
  from the no-SRT `tst-pipeline` crate while preserving the established
  import sites.

##### Added (Phase 3 / sub-phase 3.5)

- `PairerOptions` struct (`#[non_exhaustive]`) — field-style
  construction with explicit `Duration` units; `Default` impl exposes
  the previous defaults from `Pairer::nearest_pts`.
- `Pairer::with_options(video_pid, klv_pid, PairerOptions)` — the
  replacement constructor for the removed `nearest_pts`.
- `tst_pipeline::CancelHandle` and `tst_srt::CancelHandle` re-exports —
  single import path for binding authors regardless of which crate they
  pull in.
- [`docs/cancel-handle.md`](./docs/cancel-handle.md) — universal
  cross-thread shutdown pattern with per-language idiom table
  (Kotlin `Job.cancel()`, Swift `Task.cancel()`, Python
  `threading.Event`, C `tst_cancel_handle_cancel`).
- Cookbook recipe (Operations section): graceful shutdown from another
  thread via `CancelHandle`.
- Architecture doc section: cross-thread shutdown via `CancelHandle`.

##### Changed (Phase 3 / sub-phase 3.6 — visibility + close contracts)

- **`klv::pack::Iter` is now `#[doc(hidden)]`.** The iterator is still
  `pub` (a downstream fuzz target depends on it — see Phase 1 Task
  1.3.4 deferral), but it no longer appears in rustdoc. Public iteration
  over KLV packs goes through `klv::pack::iter()` and the typed
  `klv::st0601` / `klv::st0102` / `klv::st0903` decoders.

##### Added (Phase 3 / sub-phase 3.6)

- **Close-contract rustdoc on 11 long-lived public types.** Each type
  now carries a `# Closing` section spelling out the resource-cleanup
  contract for binding authors, plus a per-language idiom table
  covering Rust `Drop`, Kotlin `use { }` / `AutoCloseable`, Swift
  `defer`, Python `__exit__` / `with`, and C explicit-free pairing.
  Coverage: `MuxSender`, `Sender`, `RawSender`, `DemuxReceiver`,
  `Receiver`, `RawReceiver`, `Pairer`, `Socket`, `Listener`, `Muxer`,
  `Demuxer`. (Tasks 3.6.2 + 3.6.3.)
- **Rust ↔ C ABI cross-references on public methods.** Sender shells'
  10 most-used methods (Tasks 3.6.4) plus tst-srt and tst-core public
  methods (Task 3.6.5) now carry `# C ABI` rustdoc sections naming the
  matching `tst_*` C entry point, and the C header carries reverse
  references to the Rust path. Binding authors no longer need to
  hand-trace the mapping.
- **Three new CI ratchets** under `scripts/`:
  - `check-c-abi-rustdoc-coverage.sh` (Task 3.6.6) — verifies every
    `tst_*` C ABI export has a matching `# C ABI` rustdoc reference
    and vice-versa. Bidirectional: catches both Rust→C drift (new C
    entry point not surfaced in Rust docs) and C→Rust drift (new Rust
    method not annotated). Currently locks in 74 C ABI exports.
  - `check-close-contract-presence.sh` (Task 3.6.7) — verifies all
    11 long-lived public types still carry a `# Closing` rustdoc
    section. Catches accidental removals during refactors.
  - `check-no-public-usize.sh` (Task 3.6.8) — guards against `usize`
    sneaking back into the public API surface. Sub-phase 3.5
    eliminated all public `usize` (replaced with `u64` for FFI
    portability); this keeps the surface clean.

---

#### Examples reorganization (2026-05-09)

##### Changed (examples)

- **Examples now live in a workspace-level `tst-examples` crate** at
  `examples/`, organized into 8 task-oriented subfolders
  (`getting-started/`, `sending/`, `muxing/`, `receiving/`,
  `klv-metadata/`, `pairing/`, `codec-parsing/`, `operations/`). The
  per-crate `examples/` directories under `crates/tst-srt/`,
  `crates/tst-pipeline/`, and `crates/tst-core/` are gone.

- **Invocation lines change.** Run any example with
  `cargo run -p tst-examples --example <name>`. The previous forms —
  `cargo run -p tst-srt --example <name>`,
  `cargo run -p tst-pipeline --example <name>`,
  `cargo run -p tst-core --example <name>`, and bare
  `cargo run --example <name>` — no longer resolve. README, cookbook,
  guide-*.md, getting-started.md, architecture.md, and troubleshooting.md
  are all updated; downstream consumers with their own scripts need to
  update theirs.

- **Fixture generators are now `[[bin]]` targets in `tst-core`, not
  examples.** `gen_synthetic_fixtures`, `gen_subtitle_fixtures`,
  `gen_h266_fixtures`, and `gen_av1_fixtures` moved from
  `crates/tst-srt/examples/` to `crates/tst-core/tests/tools/`.
  Invocation: `cargo run -p tst-core --bin <name>`. They're maintainer
  tooling, not learner code; relocating them clarifies the boundary.

- **C examples mirror the same taxonomy** under
  `bindings/c/examples/c/{getting-started,muxing}/`. Build commands
  in each file's header updated to the new paths.

##### Added (examples)

- **`getting-started/hello_world.rs`** (Rust) and
  `bindings/c/examples/c/getting-started/hello_world.c` (C) — the
  smallest possible mux + KLV round-trip showing what this library
  does. Both produce byte-identical output (752 bytes / 4 packets).
  Designed as the first example a new contributor runs.

- **9 READMEs** — top-level `examples/README.md`, 8 per-category
  READMEs, and `bindings/c/examples/c/README.md`. Numbered curriculum
  per category with cookbook backlinks; "diffs from previous"
  call-outs on the cumulative h264 → h265 → h266 → av1 muxing
  progression.

---

#### Phase 2 — DX + observability (2026-05-09)

##### Added (Phase 2)

- **CI rail: broken intra-doc links block PRs.** New
  `cargo doc --workspace --no-deps --all-features` step with
  `RUSTDOCFLAGS="-D warnings"`. Warnings of all four classes
  (`broken_intra_doc_links`, `private_intra_doc_links`,
  `invalid_html_tags`, `redundant_explicit_links`) fail the build.

- **CI rail: `cargo test --doc --workspace`.** Doctests now run on
  every PR alongside unit/integration tests.

- **Doctests on 15 top-level public APIs.** `lib.rs` quick-starts on
  `tst-core` + `tst-pipeline` + `tst-srt` (3); sender shells
  `MuxSender` / `Sender` / `RawSender` (3); receiver shells
  `DemuxReceiver` / `Receiver` / `RawReceiver` (3); top-level builders
  `SocketBuilder` / `MuxerConfigBuilder` / `MuxerProgramBuilder` (3);
  KLV typed decoders `klv::st0601::decode` / `klv::st0102::decode` and
  `Pairer::nearest_pts` (3).

- **`# Errors` rustdoc sections on 30 fallible public APIs.** Each
  block names concrete typed error variants and links them via
  intra-doc syntax. Coverage spans 7 `MuxSender::send_*` siblings,
  `Sender::flush`, 8 `Muxer::push_*` methods, 3 `Passphrase`
  constructors, 5 `klv::st0601` entry points, 2 `klv::st0102`
  encoders, and 4 `klv::st0903` entry points.

- **`# Panics` rustdoc sections on 9 caller-observable panic
  surfaces.** Three categories: stream-handle `pack()` debug-asserts
  on out-of-range indices (Video / Klv / Audio / Subtitle, 4 sites);
  internal Mutex poison on `MuxSender` / `ManagedTransport` /
  `ManagedRecvTransport` (documented at the struct level, 3 sites);
  libsrt startup failure on `Socket::connect_with` /
  `Listener::bind_with` (2 sites). Internal `.unwrap()` /
  `debug_assert!` sites that are unreachable invariants are
  intentionally not documented.

- **`tracing` instrumentation on `tst-pipeline` runtime events.**
  Sender-side reconnect attempts (target `tst_pipeline::reconnect`):
  INFO per attempt with `attempt#` / `max_attempts` / `backoff_ms`;
  DEBUG on non-zero backoff sleep; WARN on terminal give-up.
  Receiver-side reconnect (target
  `tst_pipeline::managed_receive`): mirrors the sender shape.
  `MuxSender` back-pressure threshold crossings: WARN on first
  crossing of 80% (approaching cap) and 100% (cap reached, push will
  return `BufferFull`); recovery transitions are silent.
  Per-shell lifetime spans: `info_span!` opened in `new()`, entered
  on `Drop`, target `tst_pipeline::*`. Per-call `trace_span!`
  deferred to Phase 4 perf-measurement work.

- **`Muxer::pending_packets()` and `::capacity_packets()` accessors**
  on `tst_core::mpegts::mux::Muxer`. Needed by the back-pressure
  threshold-crossing instrumentation above to compute the
  pending-vs-capacity ratio without wiring an extra field through
  `MuxerStats`.

- **README "Hello, MPEG-TS" snippet above the fold.** ~20-line
  copy-paste-runnable that exercises the muxer shape without needing
  an SRT peer or a 2-terminal setup. Cross-references
  `docs/getting-started.md` for the SRT-side walkthrough.

- **Cookbook section grouping (30 recipes → 7 sections + ToC).** The
  flat list of recipes is grouped under Sending / Muxing / Receiving
  / KLV metadata / Pairing video + KLV / Codec parsing / Operations
  with anchor-linked table of contents. Recipe numbers stay stable
  (every existing inbound link is preserved). New recipe 0:
  minimal-shape "Send a single TS packet to any Transport" using
  `RawSender` + an in-memory `Sink`.

- **Tracing quick-start in `getting-started.md`.** Copy-paste
  subscriber wiring + `RUST_LOG` filter target reference table
  documenting all the targets introduced in sub-phase 2.4.

##### Changed (Phase 2)

- **`tst-srt::init` migrated from `log` to `tracing`.** Single
  facade workspace-wide. libsrt-internal syslog levels now flow into
  `tracing` macros with `target="srt"` matching the existing
  `tst_core::*` targets — consumers wire one tracing-subscriber.

- **`tst-srt`: dropped `doctest = false`.** The lib.rs
  `SocketBuilder` quick-start is now compile-checked via `cargo test
  --doc`. Snippet rewritten with `no_run` + hidden `main` wrapper so
  CI doesn't need an SRT peer.

- **`tst-pipeline`: dropped unused `log` dep, added `tracing` dep.**

- **3 thin examples retrofitted to the rich-comment convention.**
  `mux_to_file.rs` / `pipeline_send_to_socket.rs` /
  `ts_relay_from_file.rs` — each now opens with a header banner and
  comments the why behind every non-obvious choice (PID assignments,
  PCR cadence, latency knob, NAL header byte layout, key-frame
  semantics, drain-loop pattern, `metadata_service_id` no-op for
  PrivateData, cancel-first close). Density matches
  `mux_h265_with_klv.rs`. The CLAUDE.md self-flag is now resolved.

- **Cookbook recipes 24 / 29 / 30** (`pair_klv_pipeline`,
  `parse_audio_frames`, `decode_vmti_metadata`) and **15 / 20 / 21**
  swept for consistency: `cargo run --example` invocations now
  include `-p <crate>` qualifiers (brittle to future workspace
  layout changes without). Top-of-file "Run any example with" line
  rewritten to point readers at per-recipe Runnable lines.

- **23 broken intra-doc links fixed workspace-wide** to clear the
  `cargo doc -D warnings` rail. Categories: 4 wrong-scope
  qualifications (use `[..][Self::..]` / `[..][crate::..]` form),
  1 renamed/moved item, 4 cross-crate references converted to
  inline code (cannot link upward across the crate graph), 5
  square-bracket text mistaken for links escaped with backticks
  (`buf[0]`, `programs[0]`, `Box<T>`), 4 `KlvStreamType` wrong-scope
  qualifications in `tst-pipeline`, 3 self-method backtick forms.
  `tstrans.h` regenerated to match the rustdoc edits cbindgen
  propagates into the C header.

##### Breaking (Phase 2)

- **`tst-srt`: optional `log` Cargo feature removed.** Replaced by
  unconditional `tracing` facade. Consumers wiring `log` should
  switch to `tracing-log` for compatibility.

- **4 pipeline shells gained explicit `Drop` impls.** `RawSender`,
  `DemuxReceiver`, `Receiver`, and `RawReceiver` previously had no
  `Drop` impl. The new lifetime-span instrumentation requires one
  to enter the span on shutdown. `Sender` and `MuxSender` already
  had `Drop` and are unaffected.

- **Auto-trait propagation preserved via `AssertUnwindSafe<Span>`
  wrapper.** The new private `_span: Span` field on the four shells
  above contains a `Mutex` internally, which would have flipped
  them from `RefUnwindSafe` / `UnwindSafe` to `!RefUnwindSafe` /
  `!UnwindSafe`. The wrapper preserves consumer auto-traits at zero
  runtime cost (Span is only entered in `new()` and `Drop`, never
  hot-path). `MuxSender` stays `!RefUnwindSafe` from its existing
  `Mutex<Inner<T>>`; `DemuxReceiver` stays `!RefUnwindSafe` from
  its inner `Demuxer`.

---

---

#### Phase 1 — SemVer ratchet (2026-05-08)

##### Breaking (Phase 1)

- **`MuxError` field-tag retypes:** `AmbiguousTarget.kind` and
  `InvalidStreamHandle.kind` changed from `&'static str` to `StreamKind`;
  `InvalidTeletextField.field` changed from `&'static str` to `TeletextField`.
  Display output is unchanged — both new enums implement `Display` with the
  same human-readable strings as before.

- **Two new `MuxError` variants:** `DescriptorIndexOutOfRange { kind:
  StreamKind, index: u32, program_number: u16 }` and `AbsIndexOutOfRange {
  abs_idx: u32, len: u32, program_number: u16 }`. The five
  `MuxerConfigBuilder::stream_descriptors_for_*` / `ProgramBuilder`
  out-of-range paths previously panicked; they now store a deferred typed
  error and surface it from `MuxerConfigBuilder::build()`. First-error-wins.

- **`#[non_exhaustive]` added to 37 public error enums** across `tst-core`
  (10 enums), `tst-pipeline` (5 enums), and `tst-srt` (12 newly attributed +
  `UrlError` which already had it = 13). Future variants on these enums will
  not be SemVer-breaking, but external `match` arms now require a wildcard
  (`_`) arm. Full list: `MuxError`, `DemuxError`, `AuCellError`,
  `DescriptorError`, `DescriptorParseError`, `PsiParseError`, `TsParseError`,
  `CodecParseError`, `TransportError` (tst-core, 10 total including
  `VTargetPackError` which already had it); `MuxSenderError`, `SenderError`,
  `DemuxReceiverError`, `TsFramingError`, `GapBufferError` (tst-pipeline);
  `PassphraseError`, `StreamIdError`, `PacketFilterError`, `AddrError`,
  `OptionError`, `IoError`, `ConnectError`, `BindError`, `AcceptError`,
  `SendError`, `RecvError`, `Error`, `UrlError` (tst-srt).

- **`Pts90khz` / `Pcr27mhz` inner field is now private.** The public tuple
  field (`Pts90khz(pub i64)`) allowed bypassing the typed-time invariant.
  Use `::new(ticks)` to construct and `.as_ticks()` to read raw ticks.
  Existing call sites using `Pts90khz::from_millis` / `from_pts` /
  arithmetic operators are unaffected.

- **Mux config type rename cascade:** `mpegts::mux::Config` →
  `MuxerConfig`; `ConfigBuilder` → `MuxerConfigBuilder`; `ProgramConfig` →
  `MuxerProgramConfig`; `ProgramBuilder` → `MuxerProgramBuilder`. The old
  names are gone with no aliases.

- **`MuxSender::new` arg order swapped** from `(config, transport)` to
  `(transport, config)`, matching `Sender::new` and `RawSender::new`.

- **`Role` enum renamed variants and default changed:** `Role::DemuxReceiver`
  → `Role::Receiver`; `Role::MuxSender` → `Role::Sender`. The dead
  `Role::Unspecified` alias is removed. `Role` now `Default`s to
  `Role::Receiver` (was `Role::Unspecified`; the libsrt-level socket mode
  behavior is unchanged — `merge_receiver_defaults` / `merge_sender_defaults`
  still select the right SRTO option set). `Role` is now `#[non_exhaustive]`.

- **`ParseError` types disambiguated:** `mpegts::descriptors::ParseError` →
  `DescriptorParseError`; `codec::ParseError` → `CodecParseError`.
  `DescriptorError` (build-side) remains distinct from `DescriptorParseError`
  (parse-side).

- **Cancel-handle return shape unified.** All nine
  `Transport::cancel_handle()` call sites now return
  `Option<Arc<dyn TransportCancel + Send + Sync>>` consistently. Previous
  shape was a mix of `Box<dyn …>`, shared references, and bare
  `Arc<…>` without the `dyn` bound.

- **Stats return shape unified.** `Sender::stats()` and the inner
  `framing.stats()` now return an owned `SenderStats` snapshot. Callers that
  stored a `&SenderStats` reference must switch to owning the value.

- **`tst-core` crate root re-exports are now explicit.** Wildcard glob
  imports of `tst_core::*` previously escaped every future addition to
  `error.rs` and `transport.rs` implicitly. The exports are now a finite,
  documented list; future internal additions no longer appear automatically.

- **New `tst-srt` crate-root re-exports:** `RecvError`, `SendError`,
  `ConnectError`, and `BindError` are now reachable as `tst_srt::RecvError`
  etc. (previously required the full `tst_srt::srt::error::*` path).

- **`UasDatalinkLs` re-exported at `tst_core` crate root** (previously
  `tst_core::klv::st0601::UasDatalinkLs` only).

- **`Demuxer::programs_for_test` scoped to `pub(crate)` + `#[cfg(test)]`.**
  Was `pub`. White-box PAT/PMT diffing tests are now unit tests inside
  `demuxer.rs`'s `mod tests` block instead of integration tests.

- **`VideoStreamHandle::for_test` and `KlvStreamHandle::for_test` deleted.**
  For valid handles use `pack(prog, within)`; for out-of-range sentinel values
  use `from_raw(u32::MAX)`.

- **`klv::pack::Iter` visibility tightened to `pub(crate)`** — deferred to
  Phase 5 (the public-facing iterator surface requires the god-module split
  to settle first; see `docs/plans/2026-05-08-phase-1-semver-ratchet.md`
  Task 1.3.4 for rationale). External consumers iterating KLV packs should
  use the higher-level typed-set decoders (`klv::st0601`, `klv::st0102`,
  `klv::st0903`).

##### Internal (Phase 1)

- 44 `#[allow(dead_code)]` annotations swept across 15 files. 40 were
  cascade-pattern artifacts (helpers landed before consumers in earlier plans;
  consumers shipped but the annotations were never removed). `cargo clippy -D
  warnings` stays green post-removal. 7 newly-flagged items deleted as
  genuinely dead (including `Handle<T>::into_raw`, `Handle<T>::from_raw`,
  `MemTransport::taken`, and two `_unused()` sentinel functions in
  `klv/pack.rs` and `klv/imapb.rs`); 2 gated correctly under `#[cfg(test)]`.

- Internal codec-parser substrate types (`BitReader` in `codec::h265`,
  `Av1BitReader` in `codec::av1`) marked `#[doc(hidden)]`. Their public
  visibility was a cross-module necessity, not consumer surface. Phase 5's
  god-module split will relocate them.

- Three hand-rolled `Display` + `std::error::Error` implementations migrated
  to `thiserror` derives: `CodecParseError`, `AuCellError`, `DescriptorError`.
  Externally-observable `Display` strings are unchanged (verified by
  regression tests).

##### CI (Phase 1)

- **`cargo public-api` baselines committed** for `tst-core`, `tst-pipeline`,
  and `tst-srt` (`crates/<name>/public-api.txt`). A new CI step diffs the
  current surface against the baseline and fails on any unintended drift.
  Intentional SemVer-breaking changes must update the baseline file in the
  same commit so reviewers can audit the delta. `tst-c` is excluded: its
  public surface is a C ABI tracked by `tstrans.h` (cbindgen), not a Rust
  API; `cargo-public-api` cannot handle the `lib "tstrans"` / `package
  "tst-c"` name mismatch.

- **`#[non_exhaustive]` count guard added.** CI asserts the count of
  `#[non_exhaustive]` attributes across all crate source files never
  decreases below the Phase 1 baseline of 37. New error enums must bump the
  `BASELINE` constant in the same commit.
