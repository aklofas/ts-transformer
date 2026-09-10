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

- **Stable** — breaking change requires a deprecation cycle (≥1 minor
  release carrying a deprecation warning) + CHANGELOG migration notes.
  Post-1.0: semver-major only.
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

| Package | Module | Tier | Why |
|---|---|---|---|
| tst-core | mpegts | Stable | core mux/demux engine |
| tst-core | mpegts::demux::low_level | Internal | extension surface, binding-canonical workflow |
| tst-core | klv | Stable | KLV substrate (encode/decode machinery) |
| tst-core | klv::checksum | Stable | KLV substrate machinery, same tier as `klv` itself |
| tst-core | klv::imapb | Stable | KLV substrate machinery, same tier as `klv` itself |
| tst-core | klv::length | Stable | KLV substrate machinery, same tier as `klv` itself |
| tst-core | klv::pack | Stable | KLV substrate machinery, same tier as `klv` itself |
| tst-core | klv::universal_label | Stable | KLV substrate machinery, same tier as `klv` itself |
| tst-core | klv::st0601 | Stable | primary metadata dialect of the project scope |
| tst-core | klv::st0102 | Provisional | typed set, recent surface |
| tst-core | klv::st0605 | Provisional | typed set, recent surface |
| tst-core | klv::st0903 | Provisional | typed set, recent surface |
| tst-core | klv::st0805 | Provisional | typed set, shipped 2026-07 |
| tst-core | klv::st0806 | Provisional | typed set, shipped 2026-07 |
| tst-core | klv::st1010 | Provisional | typed set, shipped 2026-07 |
| tst-core | klv::st1204 | Provisional | typed set, shipped 2026-07 |
| tst-core | codec | Stable | shared parameter types (`ChromaFormat`/`ColorInfo`/`Rational`/etc.) used by every codec parser below |
| tst-core | codec::util | Stable | shared codec utility (NAL-unit counting), same tier as `codec` itself |
| tst-core | codec::h264 | Stable | primary video codec path |
| tst-core | codec::h265 | Stable | primary video codec path |
| tst-core | codec::h266 | Provisional | newer codec, less field exposure |
| tst-core | codec::av1 | Provisional | newer codec, less field exposure |
| tst-core | codec::misp_time | Provisional | shipped with the ST 0604 arc |
| tst-core | codec::aac | Provisional | audio parser |
| tst-core | codec::ac3 | Provisional | audio parser |
| tst-core | codec::mpegaudio | Provisional | audio parser |
| tst-core | codec::nal_framing | Provisional | new module, Annex B ↔ length-prefixed NAL conversion for VideoToolbox-style consumers |
| tst-core | transport | Stable | trait contract all transports implement |
| tst-core | error | Stable | error taxonomy |
| tst-core | cancel | Stable | cancellation plumbing shared across transports; spec-silent, defaults to tst-core's tier. `CancelSlot` (shipped 2026-09-09, re-exported by `tst-pipeline` as the `FactoryCancel` alias) is Provisional — same divergence shape as `tst-rtp`'s `h264` row below |
| tst-core | io_file | Stable | file I/O convenience layer over mux/demux (`file` feature); spec-silent, defaults to tst-core's tier |
| tst-core | net | Stable | shared socket-setup plumbing consumed by transport crates; spec-silent, defaults to tst-core's tier |
| tst-core | publisher | Provisional | `Publisher` trait + stats for segment-publishing transports (HLS); its only implementor (`tst-hls`) and only consumer (`tst-pipeline::mux_publisher`) are both Provisional — no Stable-tier evidence backs the trait yet, unlike `transport`, whose implementors are mostly Stable |
| tst-core | shared | Stable | `SharedBytes` zero-copy buffer used throughout demux event payloads; spec-silent, defaults to tst-core's tier |
| tst-core | url | Stable | shared URL-parsing plumbing used by every transport crate; spec-silent, defaults to tst-core's tier |
| tst-pipeline | mux_sender | Stable | canonical send shell |
| tst-pipeline | sender | Stable | TS-bytes send shell |
| tst-pipeline | raw_sender | Stable | TS-bytes send shell, raw variant |
| tst-pipeline | demux_receiver | Stable | canonical receive shell |
| tst-pipeline | receiver | Stable | TS-bytes receive shell |
| tst-pipeline | raw_receiver | Stable | TS-bytes receive shell, raw variant |
| tst-pipeline | managed_demux_receiver | Stable | `Managed*` reconnect wrapper |
| tst-pipeline | managed_receive | Stable | `Managed*` reconnect wrapper |
| tst-pipeline | reconnect | Stable | reconnect config |
| tst-pipeline | shell_error | Stable | shell error taxonomy |
| tst-pipeline | dyn_aliases | Stable | boxed-transport type aliases (plumbing/re-export); spec-silent, defaults to tst-pipeline's tier |
| tst-pipeline | mux_publisher | Provisional | HLS-adjacent, newer |
| tst-pipeline | ext | Provisional | extensions (pairing, file transport) — newer surface |
| tst-srt | (crate) | Stable | primary transport of the project scope |
| tst-udp | (crate) | Stable | small, settled |
| tst-tcp | (crate) | Stable | small, settled |
| tst-rtp | builder | Stable | RTP transport builder / URL-connect surface |
| tst-rtp | cancel | Stable | RTP cancellation handle |
| tst-rtp | clock | Stable | RTP clock/timestamp helper |
| tst-rtp | error | Stable | RTP error taxonomy |
| tst-rtp | h264 | Stable | RFC 6184 H.264 depacketizer/receiver; `H264Receiver::set_recv_timeout` and `end_reason()` (shipped 2026-08-20) are Provisional — same divergence shape as `transport`'s row below |
| tst-rtp | init | Stable | one-time RTP init |
| tst-rtp | packet | Stable | RTP packet parse/build |
| tst-rtp | rtcp | Stable | RTCP sender/receiver report handling |
| tst-rtp | sdp | Stable | SDP parsing/media selection |
| tst-rtp | transport | Stable | RTP transports (unicast/multicast, v4/v6); `RtpRecvTransport::{end_reason, end_reason_handle}` (shipped 2026-08-20) are Provisional — they return `StreamEndReason`/`StreamEndReasonHandle`, which live in and inherit the tier of the `rtsp` row below |
| tst-rtp | url | Stable | `rtp(s)://`/`rtsp(s)://` URL parsing |
| tst-rtp | rtsp | Provisional | server surface (`RtspServer`) still evolving; the client API (`RtspClient`) is stable in practice, but the module's tier is set by the server surface — the rail is module-granular, not sub-module. Also covers `rtsp::client::end_reason` (`StreamEndReason`, `StreamEndReasonHandle`, re-exported at the crate root) |
| tst-hls | (crate) | Provisional | crate restructured 2026-07 |
| tst-rist | (crate) | Provisional | profile coverage still evolving |
| tstrans-srt-sys | (crate) | Internal | raw FFI, no direct-consumer promise; documentation-only row — no `public-api.txt` baseline exists for this crate |
| tstrans-rist-sys | (crate) | Internal | raw FFI, no direct-consumer promise |
| tstrans-mbedtls-src | (crate) | Internal | build-time source provider |

### Root-level re-exports

`tst-core` and `tst-rtp` re-export a handful of high-traffic types and
functions at the crate root for ergonomics — for example
`tst_core::Transport` (from `tst_core::transport::Transport`),
`tst_core::UasDatalinkLs` (from `tst_core::klv::st0601::UasDatalinkLs`), and
`tst_rtp::compute_rtt_us` (from `tst_rtp::rtcp::ingest::compute_rtt_us`).
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
not restated here.

## Decision log

### 2026-07-30 — MuxSender consumption-info fix: rejected alternatives

The fix for external-review triage item 7 gives transport-source
`MuxSenderError`s an `input_consumed: Option<bool>` field, set at the
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
