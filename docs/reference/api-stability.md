# API Stability

This page classifies every top-level public module across the workspace's
publishable crates into a stability tier. It is the source of truth for
"can I depend on this without it moving under me" — read it alongside
[`/docs/reference/public-api.md`](/docs/reference/public-api.md) (which
governs *what stays public at all*, the binding-canonical-workflow rule)
and [`/docs/reference/conventions.md`](/docs/reference/conventions.md)
(naming and shape conventions). This page governs *how much churn to
expect*, not what's public.

## Tiers

Four tiers, applied uniformly pre-1.0 and restated for what they become
once the workspace reaches 1.0:

- **Stable** — before 1.0, a Stable item may be removed or renamed directly;
  every such change is announced under the CHANGELOG's BREAKING heading in
  the release that ships it, with migration notes. From 1.0, a breaking
  change requires a deprecation cycle (≥1 minor release carrying a
  deprecation warning) and is semver-major.
- **Provisional** — may change in any minor release; every change lands
  with CHANGELOG migration notes; no deprecation cycle promised.
- **Experimental** — may change or vanish in any release without notice.
- **Internal** — public solely for the binding-canonical workflow or
  documented extension points (`demux::low_level`); no stability promise.

No workspace module is currently classified Experimental — the tier
exists for future use (a new module can land there before earning
Provisional status).

## Classification table

One row per (crate, top-level public module), plus explicit override rows
for named exceptions that live below the top level (`mpegts::demux::low_level`)
or that don't warrant module-by-module granularity (`tst-rtp::rtsp`, which
covers both the client and server code paths — see its Why cell). Level-2
KLV dialects (`klv::st0601`, etc.) and codec parsers (`codec::h264`, etc.)
get their own rows because their tiers diverge from their parent module;
other crates' submodules are covered by their top-level module's row.

`(crate)` in the Module column means every top-level module in that crate
shares the row's tier — matching that crate's README whole-crate
stability line (`tst-srt`, `tst-udp`, `tst-tcp`, `tst-hls`, `tst-rist`,
and the three `tstrans-*-sys`/`-src` crates). Where a narrower row exists
for a path inside a crate that also has a broader row (`mpegts` vs.
`mpegts::demux::low_level`; the fully-enumerated `tst-rtp` modules vs.
its `rtsp` row), the more specific row wins for that path.

The Before Stable column says, for each Provisional row, what would let
it move to Stable; Stable and Internal rows carry `—`. Which rows 1.0
needs is in the [1.0 exit criteria](/docs/project/1.0-exit-criteria.md).

| Package | Module | Tier | Why | Before Stable |
|---|---|---|---|---|
| tst-core | mpegts | Stable | core mux/demux engine; `mux::StreamKind` is now `MuxStreamKind` and stays as a deprecated alias, removed in 0.10.0 | — |
| tst-core | mpegts::demux::low_level | Internal | extension surface, binding-canonical workflow | — |
| tst-core | binding_schema | Internal | the crossing-record schema the binding generator reads; `binding_record!` and `BINDING_SCHEMA` exist for the generator, not for consumers | — |
| tst-core | klv | Stable | KLV substrate (encode/decode machinery) | — |
| tst-core | klv::checksum | Stable | KLV substrate machinery, same tier as `klv` itself | — |
| tst-core | klv::imapb | Stable | KLV substrate machinery, same tier as `klv` itself | — |
| tst-core | klv::length | Stable | KLV substrate machinery, same tier as `klv` itself | — |
| tst-core | klv::pack | Stable | KLV substrate machinery, same tier as `klv` itself | — |
| tst-core | klv::universal_label | Stable | KLV substrate machinery, same tier as `klv` itself | — |
| tst-core | klv::st0601 | Stable | primary metadata dialect of the project scope | — |
| tst-core | klv::st0102 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer |
| tst-core | klv::st0605 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer |
| tst-core | klv::st0903 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer, with the typed nested VMTI sets either added or confirmed out of the surface |
| tst-core | klv::st0805 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer |
| tst-core | klv::st0806 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer |
| tst-core | klv::st1010 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer |
| tst-core | klv::st1204 | Provisional | typed set, recent surface | one minor with no signature change and a binding consumer |
| tst-core | codec | Stable | shared parameter types (`ChromaFormat`/`ColorInfo`/`Rational`/etc.) used by every codec parser below | — |
| tst-core | codec::util | Stable | shared codec utility (NAL-unit counting), same tier as `codec` itself | — |
| tst-core | codec::h264 | Stable | primary video codec path | — |
| tst-core | codec::h265 | Stable | primary video codec path | — |
| tst-core | codec::h266 | Provisional | newer codec, less field exposure | parameter-set parsing exercised by an outside encoder's capture with no signature change |
| tst-core | codec::av1 | Provisional | newer codec, less field exposure | carriage-mode decision exercised by an outside producer's capture |
| tst-core | codec::misp_time | Provisional | MISB ST 0604 MISP timestamp helpers, recent surface | MISP timestamps from an outside producer's capture parsed with no signature change |
| tst-core | codec::annexb | Provisional | the Annex-B start-code scanner every NAL consumer shares; new public surface | one minor with no signature change while every NAL consumer in the workspace uses it |
| tst-core | codec::nal | Provisional | NAL header type table (H.264/H.265/H.266); new public surface | one minor with no change to the NAL type tables or their names |
| tst-core | codec::aac | Provisional | audio parser | the LATM decision made, either parsed or confirmed out of the surface, and one minor with no signature change |
| tst-core | codec::ac3 | Provisional | audio parser | the E-AC-3 and DVB decision made, either parsed or confirmed out of the surface, and one minor with no signature change |
| tst-core | codec::mpegaudio | Provisional | audio parser | one minor with no signature change and a binding consumer |
| tst-core | codec::nal_framing | Provisional | new module, Annex B ↔ length-prefixed NAL conversion for VideoToolbox-style consumers | a length-prefixed consumer such as VideoToolbox driven through it with no shape change |
| tst-core | transport | Stable | trait contract all transports implement; the executable form (`transport::conformance`) sits behind the non-default `conformance` feature; `EndReason` and `EndReasonHandle` are Provisional (new; surfaces will add variants); the `*ErrorKind` enums are exhaustive (`conventions.md`). | — |
| tst-core | error | Stable | error taxonomy; the `*ErrorKind` enums are exhaustive (`conventions.md`) | — |
| tst-core | cancel | Stable | cancellation plumbing shared across transports; spec-silent, defaults to tst-core's tier. `FlagCancel` and `ResourceCancel` (formerly `SrtCancelHandle`, kept as a deprecated alias for one minor) are Stable. `CancelSlot` (re-exported by `tst-pipeline` as the `FactoryCancel` alias) is Provisional — same divergence shape as `tst-rtp`'s `h264` row below | — |
| tst-core | io_file | Stable | file I/O convenience layer over mux/demux (`file` feature); spec-silent, defaults to tst-core's tier | — |
| tst-core | net | Stable | shared socket-setup plumbing consumed by transport crates; spec-silent, defaults to tst-core's tier | — |
| tst-core | publisher | Provisional | `Publisher` trait + stats for segment-publishing transports (HLS); its only implementor (`tst-hls`) and only consumer (`tst-pipeline::mux_publisher`) are both Provisional — no Stable-tier evidence backs the trait yet, unlike `transport`, whose implementors are mostly Stable | a Stable implementor, which in practice means `tst-hls` moving to Stable |
| tst-core | shared | Stable | `SharedBytes` zero-copy buffer used throughout demux event payloads; spec-silent, defaults to tst-core's tier | — |
| tst-core | url | Stable | shared URL-parsing plumbing used by every transport crate; spec-silent, defaults to tst-core's tier | — |
| tst-pipeline | mux_sender | Stable | canonical send shell | — |
| tst-pipeline | sender | Stable | TS-bytes send shell | — |
| tst-pipeline | raw_sender | Stable | TS-bytes send shell, raw variant; `RawSendStats` is now `RawSenderStats` and stays as a deprecated alias, removed in 0.10.0 | — |
| tst-pipeline | demux_receiver | Stable | canonical receive shell | — |
| tst-pipeline | receiver | Stable | TS-bytes receive shell | — |
| tst-pipeline | raw_receiver | Stable | TS-bytes receive shell, raw variant; `RawRecvStats` is now `RawReceiverStats` and stays as a deprecated alias, removed in 0.10.0 | — |
| tst-pipeline | managed_demux_receiver | Stable | `Managed*` reconnect wrapper | — |
| tst-pipeline | managed_receive | Stable | `Managed*` reconnect wrapper | — |
| tst-pipeline | reconnect | Stable | reconnect config | — |
| tst-pipeline | shell_error | Stable | shell error taxonomy | — |
| tst-pipeline | mux_publisher | Provisional | HLS-adjacent, newer | the HLS publisher's segment contract unchanged for one minor |
| tst-pipeline | ext | Provisional | extensions (pairing, file transport) — newer surface | pairing and the file transport unchanged for one minor, with the pairing C exposure either built or confirmed out of the surface |
| tst-pipeline | binding | Provisional | binding-shared handle layer (`Owned`, `HandleState`, panic helpers, `BindingErrorKind`, `view`, `opened`, `ManagedHandles`); `no_std` + `alloc` except `ManagedHandles` (`std`); shape may still move before 1.0; kept a module of tst-pipeline, not a crate (see `architecture.md`) | the planned mobile bindings built on it without a shape change |
| tst-srt | (crate) | Stable | primary transport of the project scope; the `Srt`-prefixed type names (`SrtSocket`, `SrtListener`, `SrtSocketBuilder`, `SrtListenerBuilder`, `SrtSocketConfig`, `SrtListenerConfig`, `SrtStats`, `SrtConnectError`, `SrtUrlError`) replace the unprefixed ones (`Socket`, `Listener`, `SocketBuilder`, `ListenerBuilder`, `SocketConfig`, `ListenerConfig`, `Stats`, `ConnectError`, `url::UrlError`), which stay as deprecated aliases, removed in 0.10.0 | — |
| tst-srt | opener | Provisional | URL → type-erased transport for the language bindings' shells | the opener signature unchanged while a second binding family adopts it |
| tst-srt | shells | Provisional | URL → managed-shell open path returning `tst_pipeline::binding::ManagedHandles`; new surface, may still move before 1.0 | the managed-shell open path unchanged while the planned mobile bindings adopt it |
| tst-udp | (crate) | Stable | small, settled; `SocketConfig` is now `UdpConfig` and stays as a deprecated alias, removed in 0.10.0 | — |
| tst-udp | opener | Provisional | URL → type-erased transport for the language bindings' shells | the opener signature unchanged while a second binding family adopts it |
| tst-tcp | (crate) | Stable | small, settled; `SocketConfig` is now `TcpConfig` and stays as a deprecated alias, removed in 0.10.0 | — |
| tst-tcp | opener | Provisional | URL → type-erased transport for the language bindings' shells | the opener signature unchanged while a second binding family adopts it |
| tst-rtp | cancel | Stable | RTP cancellation handle | — |
| tst-rtp | depay | Stable | the access unit, configuration and counters the H.264 and H.265 depacketizers share (formerly `H264Au`/`H264DepayConfig`/`H264DepayStats`, still aliased under those names) | — |
| tst-rtp | error | Stable | RTP error taxonomy | — |
| tst-rtp | h264 | Stable | RFC 6184 H.264 packetizer, depacketizer and receiver; `H264Receiver::set_recv_timeout` and `end_reason()` are Provisional — same divergence shape as `transport`'s row below | — |
| tst-rtp | h265 | Provisional | RFC 7798 H.265 packetizer and depacketizer; new surface | H.265 RTP from an outside sender depacketized with no signature change |
| tst-rtp | opener | Provisional | URL opener for the language bindings' shells (type-erased transport + cancel + domain) | the opener signature unchanged while a second binding family adopts it |
| tst-rtp | packet | Stable | RTP packet parse/build | — |
| tst-rtp | rtcp | Stable | RTCP SR/RR/SDES packet codecs and `RtcpStats` | — |
| tst-rtp | sdp | Stable | SDP parsing (`Sdp`, `SdpMedia`) | — |
| tst-rtp | transport | Stable | RTP transports (unicast/multicast, v4/v6); `ConnectError` is now `RtpConnectError` and stays as a deprecated alias, removed in 0.10.0; `RtpRecvTransport::{end_reason, end_reason_handle}` are Provisional — they return tst-core's `EndReason`/`EndReasonHandle` | — |
| tst-rtp | url | Stable | `rtp(s)://`/`rtsp(s)://` URL parsing; `UrlError` is now `RtpUrlError` and stays as a deprecated alias, removed in 0.10.0 | — |
| tst-rtp | rtsp | Provisional | server surface (`RtspServer`) still evolving; the client API (`RtspClient`) is stable in practice, but the module's tier is set by the server surface — the rail is module-granular, not sub-module. `RtspClientBuilder`, `RtspServerBuilder` and `RtspServerCancelHandle` (in `rtsp::client` and `rtsp::server`, formerly in `builder` and `cancel`) keep the Stable tier they had | the server builder, mount and publisher-role surface unchanged for one minor, with publisher resource budgets settled |
| tst-hls | (crate) | Provisional | crate split out of `tst-tcp` in 0.3.0 | the segment and playlist contract unchanged for one minor, with low-latency HLS either added or confirmed out of the surface |
| tst-rist | (crate) | Provisional | profile coverage still evolving | the supported profiles and URL keys fixed, and one minor with no signature change |
| tst-rist | opener | Provisional | URL → type-erased transport for the language bindings' shells | the opener signature unchanged while a second binding family adopts it |
| tstrans-srt-sys | (crate) | Internal | raw FFI, no direct-consumer promise; documentation-only row — no `public-api.txt` baseline exists for this crate | — |
| tstrans-rist-sys | (crate) | Internal | raw FFI, no direct-consumer promise | — |
| tstrans-mbedtls-src | (crate) | Internal | build-time source provider | — |

### Root-level re-exports

`tst-core` and `tst-rtp` re-export a handful of high-traffic types and
functions at the crate root for ergonomics — for example
`tst_core::Transport` (from `tst_core::transport::Transport`),
`tst_core::UasDatalinkLs` (from `tst_core::klv::st0601::UasDatalinkLs`), and
`tst_rtp::RtspClient` (from `tst_rtp::rtsp::client::RtspClient`).
These root items do not get their own table rows: **a root-level item's
tier is whatever the module row that defines it says.**

## Bindings

The Python (`tstrans`), JVM (`tstrans-jvm`), and C (`tst-c`) binding
surfaces inherit the stability tier of the Rust module they wrap. A
Python function backed by a Provisional Rust module carries the same
Provisional promise as the Rust API, independent of the binding
package's own version number. Binding-specific mechanics (C ABI minor
version bumps, the `#[non_exhaustive]` count ratchet, the
binding-canonical-workflow rule that keeps an item public in the first
place) are covered by
[`/docs/reference/public-api.md`](/docs/reference/public-api.md) and
[`/docs/reference/binding-authors.md`](/docs/reference/binding-authors.md),
not restated here. How the C ABI rules are checked in CI is in the
"Versioning" section of the binding-authors page.

## Decision log

### 2026-07-30 — MuxSender consumption-info fix: rejected alternatives

Transport-source `MuxSenderError`s carry an `input_consumed: Option<bool>`
field, set at the
failure site, so callers always know whether a failed `send_*` call
touched this call's input: `Some(true)` — consumed (muxed and retained
in the pending queue, draining on the next `send_*`) — do not resend;
`Some(false)` — not consumed (a mux-side rejection, a closed transport,
or a failure draining bytes retained by a *previous* call) — safe to
retry the same input; `None` — not a per-call input-path error (e.g. a
poisoned lock). Two alternative designs were considered and rejected:

- **`SendOutcome` return enum.** Rejected because it changes the `Ok`
  path of all 14 `send_*` methods and every existing caller, merely
  restating what `Ok` already meant. The failure-site information
  belongs on the error, not on the success path.
- **Atomic enqueue** (input is always consumed into `pending`, never
  rejected up front). Rejected because it requires a cap + overflow
  policy + age telemetry on the bare shell — duplicating exactly what
  `ManagedTransport` already provides one layer up. The bare shell
  stays a thin primitive by design; callers who want lossless behavior
  reach for `Managed*`.
