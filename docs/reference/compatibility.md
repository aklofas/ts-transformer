# Compatibility matrix

What `ts-transformer` actually implements today, mapped to the upstream specs and
deployed protocol surfaces. Statuses are deliberate and conservative — items
not listed below are intentionally **not yet implemented**.

**Legend**

| Mark | Meaning |
| --- | --- |
| ✅ Full | Implemented and exercised by tests (synthetic + real-world fixtures). |
| ⚙️ Partial | Implemented for the curated subset listed; other parts not modelled. |
| 🔁 Pass-through | Not typed; preserved verbatim through the parser and re-emitted. |
| 🟡 Permissive | Wire format accepted; spec-mandated strictness is opt-in. |
| ⏳ Planned | On the roadmap, not yet implemented. |
| ❌ Out of scope | Deferred indefinitely. |

The `ts-transformer` workspace scopes to **MPEG-TS + MISB ST 0601 / 0102 / 0605 / 0903
KLV** over **UDP, raw TCP / TLS, RTP (incl. RTSP client + server), SRT, and RIST**.
An HLS publisher (`tst-hls`) is now **supported and ships in the Python wheels**
(default-on `hls` feature in `tst-py`). The C binding exposes HLS via the opt-in
`hls` Cargo feature; the crate is segmenter-first — the built-in HTTP server
and TLS are gated behind the `serve` and `tls` features respectively. The
built-in server serves only a known set of files it wrote (no path traversal),
binds loopback by default, and can keep serving a completed VOD/EVENT playlist
via `finish_serving`. See the [HLS guide](/docs/guides/hls.md); fMP4/CMAF and
LL-HLS remain deferred (`deferred-features.md`).
The JVM binding has no HLS surface (deferred). Other
containers (MP4 / CMAF), other transports (RTMP / WebRTC), and raw elementary
streams remain out of scope until a consumer asks. See
`tests/coverage/TEST_CORPUS.md` for the parsing-side compliance ledger
that this document summarises.

---

## Build targets

The `ts-transformer` workspace supports the following platforms. Tier 1
platforms are verified in CI on every PR; non-Tier-1 platforms are
deferred — see `deferred-features.md` for triggers to revisit.

| Target                       | Status                  | CI scope                          | Notes                                          |
|------------------------------|-------------------------|-----------------------------------|------------------------------------------------|
| Linux x86_64 (GNU)           | Tier 1, gating          | Every PR + scrub/ratchet scripts  | Reference platform                             |
| Linux aarch64 (GNU)          | Tier 1, gating          | Every PR + cargo build/test       | GHA `ubuntu-24.04-arm`; native build           |
| macOS arm64 (Apple Silicon)  | Tier 1, gating          | Every PR + cargo build/test       | GHA `macos-14`; native build; Intel not supported |
| Windows x86_64 (MSVC)        | Tier 1, gating          | Every PR + cargo build/test       | GHA `windows-latest`; MSVC toolchain only      |
| Linux x86_64 (musl)          | Tier 2                  | `tst-core` + `tst-pipeline` only  | libsrt-bound crates not supported under musl   |
| Bare-metal `thumbv7em-none-eabihf` | Tier 2 (runtime-gate) | `tst-core`+`tst-pipeline` (sender+receiver) build + QEMU runtime | Cortex-M4F/M7F (STM32F4/F7/H7); `#![no_std]`+`alloc` |
| Bare-metal `riscv32imac-unknown-none-elf` | Tier 2 (runtime-gate) | `tst-core`+`tst-pipeline` (sender+receiver) build + QEMU runtime | RISC-V (e.g. ESP32-P4 bare-metal); `#![no_std]`+`alloc` |
| iOS / Android                | Deferred                | —                                 | See `deferred-features.md`                     |
| Windows MinGW (gcc)          | Deferred                | —                                 | See `deferred-features.md`                     |
| macOS x86_64 (Intel)         | Deferred                | —                                 | See `deferred-features.md`                     |

**"Gating" status meaning:** the platform is built + tested in CI on every
PR and build or test failures DO block merge. All four Tier 1 platforms
(Linux x86_64 + aarch64, macOS arm64, Windows MSVC) are gating.

**Bare-metal `no_std` (compile- and runtime-gate):** `tst-core` builds under
`#![no_std]` + `alloc` with `--no-default-features` — the MPEG-TS
mux/demux engine, KLV, the in-crate codec parameter-set parsers, and the
transport traits. The `std` feature (default-on) gates the `net` helpers,
the thread/`Barrier` cancel path, and `serde_json`/`toml` export. CI
compiles the crate for `thumbv7em-none-eabihf` and
`riscv32imac-unknown-none-elf`; a `*-none-*` target has no `std`, so the
build itself is an unconditional guard. Beyond the compile gate, the
embedding binary supplies a `#[global_allocator]`, and on 32-bit cores
`portable-atomic` backs the 64-bit cancel atomic via its `fallback`
implementation (a lock shim over the target's native atomics — no
embedder-supplied hook needed; atomics-less cores such as `thumbv6m`
are not supported). The
bare-metal compile is also exercised at runtime under QEMU on both
architectures (`qemu-system-arm -machine mps2-an386`, thumbv7em; and
`qemu-system-riscv32 -machine virt`, riscv32imac) in CI as end-to-end mux
**and demux** round-trips. `embedded/freertos-srt/` is a separate, shipped
reference product: a full port of libsrt + the MPEG-TS muxer/demuxer onto
FreeRTOS + FreeRTOS-Plus-POSIX + lwIP, with SRT video egress and ingress
CI-hard-gated under QEMU — see
[`/docs/languages/embedded.md`](/docs/languages/embedded.md).

`tst-pipeline`'s sender shells (`MuxSender`/`Sender`/`RawSender`) and
receiver shells (`Receiver`/`DemuxReceiver`/`RawReceiver`) also build
`#![no_std]` + `alloc` under `--no-default-features` and are verified
against the same bare-metal targets. The `Managed*` reconnect wrappers,
`MuxPublisher`, and `ext::pairing` require the default-on `std` feature.

---

## Versions

| Component | Pinned at |
| --- | --- |
| Rust edition | 2024, MSRV **1.85** (`rust-toolchain.toml`) |
| `libsrt` (Haivision) | **v1.5.7** (`crates/srt-sys/vendor/srt`, git submodule) |
| `mbedTLS` | **v3.6.7** LTS (`crates/mbedtls-src/vendor/mbedtls`, git submodule) |
| `bindgen` | 0.72 (FFI; `srt-sys` build, published as `tstrans-srt-sys`) |
| `cbindgen` | 0.29 (C header generation; `tst-c` build) |
| `cc` | 1.0 (compiles + links the C smoke test in `tst-c` integration tests) |
| `cmake` | upstream-compatible; `pkg-config` discovery first, vendored fallback |

The vendored builds disable libsrt's `ENABLE_HEAVY_LOGGING` (Debug+static
init crash), and force `USE_ENCLIB=mbedtls` when the `mbedtls` feature is
on (default).

---

## SRT transport (`tst-srt`)

### Wire protocol

| Spec / Feature | Status | Notes |
| --- | --- | --- |
| `draft-sharabayko-srt` (IETF SRT v1.5) | ✅ Full | via libsrt 1.5.7; we don't reimplement the wire protocol. |
| `draft-sharabayko-srt-over-quic` | ❌ Out of scope | Upstream is exploratory. |
| Caller / Listener / Rendezvous handshake | ⚙️ Partial | Caller + Listener exposed; Rendezvous reachable via raw `tstrans-srt-sys` only. |
| Live congestion controller (`SRTO_CONGESTION=live`) | ✅ Full | `Congestion::Live` (default). |
| File congestion controller (`SRTO_CONGESTION=file`) | ✅ Full | `Congestion::File`. |
| Message API (datagram-style send/recv) | ✅ Full | `Socket::send` / `Socket::recv`. |
| Stream API (`SRTO_TSBPDMODE=false`, `SRTO_MESSAGEAPI=false`) | ⏳ Planned | Reachable only via `tstrans-srt-sys` today. |
| Bonded sockets / groups (`SRT_GROUP_*`) | ❌ Out of scope | No consumer demand. |
| Async / poll API (`srt_epoll_*`) | ⏳ Planned | Today's API is sync blocking. Reactor is on the roadmap. |

### Encryption (AES-CTR over SRT)

| Item | Status | Notes |
| --- | --- | --- |
| Pre-shared passphrase (10–79 chars) | ✅ Full | `Passphrase::new` enforces the spec range. |
| AES-128 key length | ✅ Full | `KeyLength::Aes128` (default). |
| AES-192 key length | ✅ Full | `KeyLength::Aes192`. |
| AES-256 key length | ✅ Full | `KeyLength::Aes256`. |
| mbedTLS backend | ✅ Full | Default; statically linked from `crates/mbedtls-src/vendor/mbedtls`. |
| OpenSSL backend | ❌ Out of scope | Add only on consumer ask. |
| Built-in (libsrt's "internal" cryptolib) | ❌ Out of scope | Not used. |
| Encryption disabled (`--no-default-features`) | ✅ Full | Builds without mbedTLS; `ENABLE_ENCRYPTION=OFF`. |

**`mbedtls` vs `tls` — two distinct security mechanisms.** The workspace uses
two Cargo features with similar-sounding names that operate at different layers
and must not be confused:

- **`mbedtls`** (`srt-sys`/`tstrans-srt-sys` feature, default-on) — links the vendored mbedTLS
  3.6.x LTS library into libsrt to provide AES-128/192/256-CTR encryption of
  the SRT channel itself (data-plane wire encryption via `SRTO_PASSPHRASE` /
  `SRTO_PBKEYLEN`). Also used by `tst-rist` for RIST channel encryption. This
  is compile-time link-time crypto built into the SRT/RIST stack; it has
  nothing to do with TLS certificates or the TCP transport layer.

- **`tls`** (`tst-tcp` / `tst-rtp` / `tst-hls` feature) — wires in rustls 0.23
  to provide TLS 1.2/1.3 on top of the raw TCP transport layer (`tcps://`,
  `rtsps://`, and HTTPS HLS serving). This is a *transport-layer* security feature — it authenticates
  the TCP connection and encrypts the TCP byte stream. It is independent of any
  SRT or RIST encryption. In `tst-tcp` the `tls` feature is **default-on**
  because TCP transport security is the crate's core value; in `tst-rtp` the
  `tls` feature is **default-off** because RTSP commonly runs plaintext
  (`rtsps://` is an explicit opt-in). The Python wheels enable all of them via
  tst-py's default-on `tls` feature. The two feature names are deliberately
  distinct; no alias links them.

**`tst-rtp`'s `rtsp-server` / `rtsp-server-tls` features — client vs. server,
tokio or not.** `tst-rtp` is a sync facade throughout, with one exception:
`RtspServer` runs an internal tokio Runtime. That Runtime is gated behind
its own feature so client-only consumers (e.g. a mobile UniFFI build
shipping RTP/RTSP client + SRT) get a tokio-free dependency tree.

| Feature | Default | Enables |
| --- | --- | --- |
| `rtsp-server` | ✅ on | `RtspServer` + mounts (the RTSP push server); pulls `tokio` + `tokio-util`. |
| `tls` | ❌ off | Client `rtsps://` (`RtspClient`) via sync rustls — no tokio either way. |
| `rtsp-server-tls` | ❌ off | The server's `rtsps://` TLS acceptor (`tokio-rustls`); implies both `rtsp-server` and `tls` — Cargo has no other way to express "`tls` AND `rtsp-server`". |

`cargo build -p tst-rtp --no-default-features --features tls` is the
tokio-free client-only build (`cargo tree -e normal` has no `tokio` entry).
The C ABI (`tst-c`) always enables `rtsp-server` (no TLS surface at all);
the JVM and Python bindings enable `rtsp-server-tls` (they ship the server's
TLS acceptor).

### Tunables (`SocketConfig` / `ListenerConfig`)

Each row maps a libsrt option to its safe-Rust accessor. Accessors that
aren't yet wrapped are reachable via `tstrans-srt-sys`.

| libsrt option | Safe field / setter | Status |
| --- | --- | --- |
| `SRTO_PASSPHRASE` | `passphrase: Option<Passphrase>` | ✅ Full |
| `SRTO_PBKEYLEN` | `key_length: KeyLength` | ✅ Full |
| `SRTO_LATENCY` | `latency: Option<Duration>` | ✅ Full |
| `SRTO_PEERLATENCY` | `peer_latency` (caller) | ✅ Full |
| `SRTO_RCVLATENCY` | `recv_latency` | ✅ Full |
| `SRTO_RCVBUF` (bytes) | `recv_buf_bytes` | ✅ Full |
| `SRTO_SNDBUF` (bytes) | `send_buf_bytes` (caller) | ✅ Full |
| `SRTO_MAXBW` | `max_bandwidth: MaxBandwidth` | ✅ Full |
| `SRTO_INPUTBW` | `input_bandwidth` (caller) | ✅ Full |
| `SRTO_OHEADBW` | `overhead_bandwidth_pct` | ✅ Full |
| `SRTO_MSS` | `mss: Option<u16>` | ✅ Full |
| `SRTO_PAYLOADSIZE` | `payload_size: Option<u16>` | ✅ Full |
| `SRTO_STREAMID` | `stream_id: Option<StreamId>` | ✅ Full |
| `SRTO_LOSSMAXTTL` | `loss_max_ttl` | ✅ Full |
| `SRTO_TLPKTDROP` | `too_late_packet_drop` | ✅ Full |
| `SRTO_FC` | `flow_window_packets` | ✅ Full |
| `SRTO_PACKETFILTER` | `packet_filter: Option<PacketFilter>` | ✅ Full |
| `SRTO_CONGESTION` | `congestion: Option<Congestion>` | ✅ Full |
| `SRTO_SNDTIMEO` / `SRTO_RCVTIMEO` | `send_timeout` / `recv_timeout` | ✅ Full |
| `SRTO_REUSEADDR` (listener) | `reuse_addr: bool` | ✅ Full |
| Backlog (`srt_listen` arg) | `backlog: u32` | ✅ Full |
| `SRTO_CONNTIMEO` | `connect_timeout: Option<Duration>` | ✅ Full |
| `SRTO_LINGER` | `linger: Option<Duration>` | ✅ Full |
| `SRTO_SENDER` | `role: Role` (`Receiver` / `Sender`) | ✅ Full — `Sender` sets `SRTO_SENDER=1` for HSv4-peer compatibility. |
| Sender / receiver socket presets | `SocketConfig::sender_defaults()` / `::receiver_defaults()` + `merge_sender_defaults()` / `merge_receiver_defaults()` + `SocketBuilder::sender_defaults()` / `::receiver_defaults()` chain methods | ✅ Full — domain-tuned bundle (`connect_timeout=15s`, `linger=5s` sender / off receiver, `role=Sender` / `Receiver`). Merge-if-default semantics preserve explicit caller values. |
| Per-socket statistics (`srt_bistats`) | `Socket::stats() -> Stats` | ⚙️ Partial — flat counter snapshot; finer windowing TBD. |
| All other `SRTO_*` options | — | 🔁 Pass-through via `tstrans-srt-sys` (raw FFI). |

### Packet filters (`SRTO_PACKETFILTER`)

| Filter | Status | Notes |
| --- | --- | --- |
| `fec` (Reed–Solomon ARQ) | ✅ Full | Filter spec passed verbatim; libsrt evaluates it. |
| Custom-name pass-through | ✅ Full | Any value accepted by libsrt 1.5.7. |
| Filter parameter validation | 🟡 Permissive | Length ≤ 512 bytes + ASCII charset enforced; semantics handed to libsrt. |

---

## Container — MPEG-TS

| Spec / Feature | Status | Notes |
| --- | --- | --- |
| MPEG-TS muxer | ✅ Full | `mpegts::mux::Muxer` — multi-program (≤16 programs), multi-stream (≤16 video + ≤16 audio + ≤16 KLV + ≤16 subtitle PIDs per program), H.264/H.265/H.266/AV1 video + MP2/AAC/AC-3 audio + DVB/teletext/CEA-708/WebVTT subtitles + ST 0601 KLV (sync + async per ST 1402), VBR. |
| MPEG-TS demuxer | ✅ Full | `mpegts::demux::Demuxer` — multi-program, lenient by default, four-tier `StrictMode` ladder. Required TS/PSI syntax (scrambling, adaptation-field, zero-length PES, PSI syntax) is validated under the ladder; multi-section PAT is reassembled, multi-section PMT is unsupported. See `mpegts::demux` block below. |
| Single PES/packet KLV embedding (ST 1402.2 Asynchronous) | ✅ Full | Default in `mpegts::mux::MuxerConfig` (`klv_stream_type = PrivateData`, `klv_carries_pts = false`). |
| ST 1402.3 Synchronous metadata stream | ✅ Full | `KlvStreamType::SynchronousMetadata` + `klv_carries_pts = true` in `MuxerConfig`. |
| H.222.0 § 2.12.4.2 sync metadata AU cell wrapping | ✅ Full | `mpegts::au_cell::write_metadata_au_cell` / `read_metadata_au_cell`. Auto-wrapped inside `Muxer::push_klv` for `KlvStreamType::SynchronousMetadata` streams; PTS in PES header per § 2.12.4.1. |
| Variable-length PES splitting | ⏳ Planned | Required for ≥ 65 000-byte KLV records (rare in practice). |
| KLVA registration descriptor (`stream_type 0x06` + `0x05 "KLVA"`) | ✅ Full | Detected/recognised on decode side; emitted by muxer on the KLV PID. |
| Audio carriage (mux side) | ✅ Full | MP2 / AAC ADTS / AAC LATM / AC-3; `Muxer::push_audio` / `push_audio_to`; PTS-only PES headers. AC-3 streams auto-emit `registration_descriptor` `format_identifier="AC-3"` per ATSC A/53 Part 3 §5.1 (caller-Registration suppression). Optional `iso_639_language_descriptor` auto-emit via `MuxerConfigBuilder::add_audio_with_language(pid, codec, lang)` per ISO/IEC 13818-1 §2.6.18 (caller-tag-0x0A suppression). |
| Audio carriage (demux side) | ✅ Full | Typed `AudioCodec`; raw PES bytes in `SamplePayload::Audio { frames }`. |

---

## Subtitle / caption carriage

Four subtitle / caption codecs ride alongside video / KLV / audio.
All four share PMT `stream_type = 0x06` and disambiguate via
auto-emitted PMT descriptors (`subtitling_descriptor`,
`teletext_descriptor`, or `registration_descriptor` with
`format_identifier` `"VTTC"` / `"GA94"`).

| Codec | Sender | Receiver | Verification |
|---|---|---|---|
| DVB subtitling | ✅ Full | ✅ Full | round-trip + ffprobe |
| DVB teletext | ✅ Full | ✅ Full | round-trip + ffprobe |
| CEA-708 standalone | ✅ Best-effort | ✅ Full | round-trip-only (ffmpeg's CEA-708 path is SEI-embedded) |
| WebVTT-in-TS | ✅ Full | ✅ Full | round-trip + ffprobe |
| CEA-608/708 in SEI | ❌ — | ❌ — | future SEI parsing plan |
| ARIB STD-B24 | ❌ — | ❌ — | deferred (Japan-only) |

Per-program cap: ≤16 subtitle streams (`MAX_SUBTITLE_STREAMS_PER_PROGRAM`).
Subtitle PIDs cannot serve as the PCR PID (too sparse for PCR pacing).
`push_subtitle` payload max: 65527 bytes (PES packet length budget).

### Wire-envelope and descriptor conformance

| Spec / Feature | Status | Notes |
| --- | --- | --- |
| DVB-sub PES envelope (ETSI EN 300 743 §6.2) | ✅ Full | Auto-wrapped inside `Muxer::push_subtitle_to` for `DvbSubtitling` (`data_identifier=0x20` + `subtitle_stream_id=0x00` + segments + `end_of_PES_data_field_marker=0xFF`). Caller passes raw segment bytes. |
| DVB-teletext stuffed PES header (ETSI EN 300 472 §4.2) | ✅ Full | Auto-emitted inside `Muxer::push_subtitle_to` for `DvbTeletext` (45-byte header, `PES_header_data_length=0x24`); PES tail padded with `0xFF` stuffing to `(N × 184) − 6` `PES_packet_length`. |
| Multi-language single-PID `subtitling_descriptor` (ETSI EN 300 468 §6.2.41) | ✅ Full | `mpegts::descriptors::subtitling_descriptor_multi`. |
| Multi-language single-PID `teletext_descriptor` (ETSI EN 300 468 §6.2.43) | ✅ Full | `mpegts::descriptors::teletext_descriptor_multi`. |
| Subtitle auto-emit suppression on caller-supplied descriptor | ✅ Full | When `MuxerConfigBuilder::stream_descriptors_for_subtitle` supplies a recognized codec marker (`subtitling_descriptor` 0x59 / `teletext_descriptor` 0x56 / VBI teletext 0x46 / `registration_descriptor` with `VTTC` or `GA94` format_identifier), the muxer suppresses its codec-driven auto-emit (mirrors KLVA / AV01 suppression). |
| ISO 639 language code casing (EN 300 468) | ✅ Full | `validate_language_code` accepts both lowercase and uppercase 3-letter ASCII alphabetic codes. |
| No-PCR-eligible-stream rejection | ✅ Full | `MuxerConfig::validate` rejects programs with no video or audio stream (`MuxError::NoPcrEligibleStream`); KLV/data/subtitle cannot carry PCR — their cadence is too sparse to anchor it. |
| Multi-descriptor `stream_type 0x06` ambiguity (demux) | ✅ Full | `NonConformantIssue::SubtitleDescriptorAmbiguous` surfaced when ≥2 distinguishing descriptors co-exist (subtitling / teletext / `VTTC` / `GA94`); cascade picks first match. |

---

## KLV substrate (`tst-core::klv`)

The generic SMPTE/MISB substrate underneath the typed layers.

| Spec / Feature | Status | Notes |
| --- | --- | --- |
| **SMPTE ST 336** — KLV encoding rules | ✅ Full | Universal Label, BER short/long, BER-OID, local-set / universal-set iterators. |
| **MISB ST 0107.5** — KLV Metadata in Motion Imagery | ✅ Full | Future-proof skip rule (`unknown: Vec<OwnedRawField>`); UL family-versioning helpers. |
| BER short-form length (≤ 127) | ✅ Full | `length::write_ber` / `read_ber`. |
| BER long-form length (1–8 octets) | ✅ Full | Indefinite-length form rejected (not allowed in KLV). |
| BER-OID length (base-128 self-terminating) | ✅ Full | `length::write_ber_oid` / `read_ber_oid`. |
| 16-bit running-sum checksum (ST 0601 §6.8) | ✅ Full | `checksum::checksum_running_sum_16`. |
| **MISB ST 1201.5** — IMAPB integer↔float mapping | ✅ Full | `imapb` matches §7.1.2 / §7.2 forward and inverse; §7.1.3 / §7.2.3 special values (±∞, quiet / signaling NaN, IMAP_BELOW_MINIMUM / IMAP_ABOVE_MAXIMUM, user-defined) modelled both directions via `DecodedImapb::Special(ImapbSpecial)` + `encode_imapb_special`. Typed ST 0903 fields read normal-range values via `DecodedImapb::value()` (specials surface as field-unavailable). |
| Universal-label introspection | ✅ Full | `UniversalLabel::oid` / `category` / `registry` / `structure` / `version_byte` / `is_st0601_family`. |
| `Iter::local_set` / `Iter::universal_set` | ✅ Full | Universal-set iterator currently exposes `tag: 0` synthetic ID; UL bytes accessor is a future tightening. |
| `OwnedRawField` pass-through | ✅ Full | Unknown / non-typed tags preserved verbatim, round-trip safe. |

### Real-world recovery shapes

Recognised "shape" wrappers the parser handles when KLV PES payloads aren't
plain `[UL][len][body]`:

| Shape | Status | Notes |
| --- | --- | --- |
| Plain ST 0601 record | ✅ Full | `decode` / `decode_unchecked` / `decode_strict` / `decode_strict_compliance`. |
| Wrapped Precision Time Stamp Pack + ST 0601 LS (TRM 0909.4 §7) | ✅ Full | Phase A of the spec-compliance plan; first record decoded via `klv::st0605::decode`, rest via record-iter. |
| ST 1402.2 Synchronous Method 5-byte AU cell header | ✅ Full | Spec-conformant 5-byte parser ships in `mpegts::au_cell::read_metadata_au_cell` per H.222.0 V9 § 2.12.4.2 Tables 2-155+2-156. |
| Broken-checksum captures | ✅ Full | `decode_unchecked` accepts; `decode` rejects with `ChecksumMismatch`. |

---

## KLV — typed MISB ST 0601 (`tst-core::klv::st0601`)

| Spec | Status | Notes |
| --- | --- | --- |
| **MISB ST 0601.19** — UAS Datalink Local Set | ✅ Full | 142 of 143 items typed or structured (see table below); Item 66 (deprecated) stays `unknown`-passthrough by design (ST 0107.5 future-proof skip). |
| ST 0601.19 §6.4 UAS LS UL | ✅ Full | `UniversalLabel::ST_0601_LS`; family check honours version-byte evolution. |
| ST 0601.8-09 (Tag 2 first) | ⚙️ Partial | Wire format accepts any order; `decode_strict_compliance` enforces. |
| ST 0601.8-11 (Tag 1 last) | ⚙️ Partial | Wire format accepts any order; `decode_strict_compliance` enforces. |
| ST 0601.8-12 (Tag 65 required) | ⚙️ Partial | Wire format treats as `Option<u8>`; `decode_strict_compliance` enforces. |
| ST 0601 ISO 646 / 7-bit ASCII string fields (Tags 3, 4, 10–12) | 🟡 Permissive | UTF-8 accepted; corpus is plain ASCII. |
| Linear-range int↔float mapping (§7.5) | ✅ Full | INT_MIN special values produce `Ok(None)`; tag recorded in `sentinel_tags`, not `field_errors`. |
| Big-endian byte / bit ordering (§6.5.1) | ✅ Full | Throughout. |

### Typed ST 0601 items (142 of 143)

Grouped by field section — see `tst_core::klv::st0601` module docs for the
full per-tag breakdown (source of truth; this table tracks it at a
coarser grain to avoid drifting per-tag detail out of sync).

| ID(s) | Group | Status |
| --- | --- | --- |
| 1–4, 10–12, 59, 65 | Identity / time (checksum, UTF-8 identity strings, version, timestamp) | ✅ Full |
| 5–9, 50, 90–91 | Platform state (heading/pitch/roll/airspeed, angle of attack, full-range pitch/roll twins) | ✅ Full |
| 13–20, 75 | Sensor pose & position (lat/lon/altitude, ellipsoid height, FOV, relative azimuth/elevation/roll) | ✅ Full |
| 21–25, 78 | Ranging & frame center | ✅ Full |
| 26–33, 82–89 | Image corners (offset from frame center, and full lat/lon) | ✅ Full |
| 40–46 | Target location & tracking (location, track-gate width/height, CE90/LE90) | ✅ Full |
| 35–38, 49, 53–55 | Weather / atmospheric | ✅ Full |
| 51–52, 56–58, 64, 92–93 | Extended platform state (vertical speed, sideslip, ground speed/range, fuel, magnetic heading, full-range AoA/sideslip twins) | ✅ Full |
| 67–69, 71, 76 | Alternate platform | ✅ Full |
| 79–80 | Sensor velocity | ✅ Full |
| 34, 63, 77 | Coded enums (`IcingDetected`, `SensorFovName`, `OperationalMode`) | ✅ Full — `Other(code)` fallback round-trips unrecognized wire codepoints |
| 39, 60–62, 70, 72, 106–108, 129, 135 | Raw scalar & string items | ✅ Full |
| 96, 103–105, 109, 112–114, 117–120, 132, 134 | Extended-range items (ST 1201.5 IMAPB) | ✅ Full — out-of-range values under `OutOfRangePolicy::Indicator` and producer specials round-trip via `imapb_specials` |
| 110–111, 123–126, 131, 133, 136–137, 139 | Var-length int/enum items (incl. `PlatformStatus`, `SensorControlMode`) | ✅ Full |
| 47 | Generic flag bitfield | ✅ Full |
| 81, 102, 115–116, 121–122, 127–128, 130, 138, 140–143 | Pack & list items (Appendix Table C1 — `ImageHorizonPixels`, `SdccFlpField`, `ControlCommand`, id lists, `CountryCodes`, `WavelengthRecord`, `AirbaseLocations`, `PayloadList`, `WeaponsStore`, `Waypoint`, `ViewDomain`, `MetadataSubstreamId`) | ✅ Full — item 102's `SdccFlpField.bytes` is a sibling-decoded raw pack, parsed via `klv::st1010::decode_sdcc_flp` |
| 48, 73, 74, 94 | Sibling-decoded nested local sets | ✅ Bytes pass-through; typed via `klv::st0102`/`klv::st0806`/`klv::st0903`/`klv::st1204` |
| 95, 97–101 | Named nested-set byte fields (SAR MI / Range Image / Geo-Registration / Composite Imaging / Segment / Amend) | 🔁 Pass-through — dedicated struct field, interior bytes not yet decoded |
| 66 | (deprecated placeholder) | 🔁 Pass-through via `unknown` (permanently, by design) |
| any other tag | forward-compat | 🔁 Pass-through via `unknown` (ST 0107.5 future-proof skip) |

Composite views layered on top: `GeoPoint`, `Attitude`, `FieldOfView`,
`Corners`. They surface `None` if any constituent typed field is missing.

### Decode entry points

| Entry point | Checksum | UL gating | Structural rules |
| --- | --- | --- | --- |
| `klv::st0601::decode` | ✅ verified | accepts any UL | ❌ |
| `klv::st0601::decode_unchecked` | ❌ skipped | accepts any UL | ❌ |
| `klv::st0601::decode_strict` | ✅ verified | requires ST 0601 family | ❌ |
| `klv::st0601::decode_strict_compliance` | ✅ verified | requires ST 0601 family | ✅ -09/-11/-12 |

---

## KLV — typed MISB ST 0605 (`tst-core::klv::st0605`)

| Spec / Feature | Status | Notes |
| --- | --- | --- |
| **MISB ST 0605.10 §7** Precision Time Stamp Pack | ✅ Full | `klv::st0605::decode` / `encode`; 26-byte fixed layout. |
| **MISB ST 0807.27** registry row 1061 (UL CRC 23259) | ✅ Full | `UniversalLabel::PRECISION_TIMESTAMP_PACK_UL`. |
| **MISB ST 0603.5 §7.1** microsecond timestamp | ✅ Full | `timestamp_us: u64` (BE). |
| **MISB ST 0603.5 §7.4** Time Status byte | ✅ Full | `TimeStatus(u8)` newtype with `is_locked` / `has_discontinuity` / `is_reverse_jump` / `reserved_bits_valid` accessors. |
| Reserved-bits enforcement (4-0 = `0b11111`) | 🟡 Permissive | `decode` does not enforce; check via `time_status.reserved_bits_valid()`. The `KlvDecodeError::ReservedBitsInvalid` variant is present but not produced by `decode`. |

---

## KLV — typed MISB ST 0903 VMTI (`tst-core::klv::st0903`)

| Spec / Feature | Status | Notes |
| --- | --- | --- |
| VMTI Local Set decode/encode (ST 0903.6) | ✅ Full | Typed top-level `VmtiLs` + per-target `VTargetPack`; 7 nested/sibling LSes (`VMask`, `VTracker`, `VChip`, `VChipSeries`, `VObjectSeries`, Algorithm Series, Ontology Series) pass-through. Universal Set form deferred. |
| Sibling-layer composition with `klv::st0601` | ✅ Full | ST 0601 Tag 74 stays `Option<Vec<u8>>`; consumers call `klv::st0903::decode` on inner bytes (no coupling between parent and inner decoders). |
| Standalone-PID dispatch | ✅ Full | `VMTI_LS_UL` 16-byte UL constant exposed for VMTI on its own KLV PID (separate from any ST 0601 stream). |
| `decode` (lenient) | ✅ Full | Tolerates missing tags, malformed sub-records (in `field_errors`), unknown tags (in `unknown` per ST 0107.5 §6). |
| `decode_strict` | ✅ Full | Rejects missing required tags (Tag 4 / Tag 6), duplicates, malformed UTF-8, malformed packs (`KlvDecodeError::St0903InvalidVTargetPack`). Conditional-required tags (1, 2, 11, 12, 13) NOT enforced — carriage-aware validation is consumer-side. |

---

## MPEG-TS demuxer (`tst-core::mpegts::demux`)

| Feature / Type | Status | Notes |
| --- | --- | --- |
| `Demuxer` | ✅ Full | Stateful TS demuxer; `feed` bytes in, `next_event` typed events out, `flush` drains trailing PES on stream end. Bytes need not be 188-aligned. |
| `DemuxerConfigBuilder` / `DemuxerConfig` | ✅ Full | Fluent builder (via `DemuxerConfig::builder()`) + plain-struct config form. |
| `DemuxEvent::ProgramMap` | ✅ Full | Emitted on PAT/PMT discovery and version-bump; carries `program_number`, `pcr_pid`, `pmt_pid`, `streams`, `klv_links`. |
| `DemuxEvent::Sample` | ✅ Full | Generic ES sample; payload typed for video / audio / subtitle, `Unknown` for unrecognized stream_types. |
| `DemuxEvent::Metadata` | ✅ Full | Standalone metadata events; `MetadataKind::KlvSyncAuCell { metadata_service_id, sequence_number, cell_fragment_indication, decoder_config_flag, random_access_indicator, was_reassembled, cell_count }` (7 fields per H.222.0 § 2.12.4.2 Table 2-156 + multi-cell reassembly state), `KlvAsync` (bare LS), `Unknown(u8)`. |
| `DemuxEvent::Discontinuity` | ✅ Full | `ContinuityJump`, `PesOversize`, `PesTotalOversize`, `AdaptationFieldFlag`. |
| `DemuxEvent::NonConformant` | ✅ Full | Lenient-mode signal for spec violations; converts to fatal in strict modes. |
| H.264 NAL split (stream_type 0x1B) | ✅ Full | Annex-B start codes stripped; `NalUnit::H264 { nal_type, ref_idc, payload }`. Emulation-prevention bytes preserved. |
| H.265 NAL split (stream_type 0x24) | ✅ Full | `NalUnit::H265 { nal_type, layer_id, temporal_id_plus1, payload }`. |
| H.266 NAL split (stream_type 0x33) | ✅ Full | `NalUnit::H266 { nal_type, layer_id, temporal_id_plus1, payload }`. |
| AV1 OBU split (stream_type 0x06 + AV01 registration) | ✅ Full | LEB128 `obu_size` consumed; `Obu { obu_type, extension, payload }`. Disambiguates from KLV-async via `format_identifier`. |
| Async KLV (stream_type 0x06 + KLVA) | ✅ Full | Detected via registration descriptor `KLVA`; emitted as `KlvAsync`. |
| Sync KLV (stream_type 0x15) | ✅ Full | H.222.0 § 2.12.4.2 5-byte `Metadata_AU_cell` header parsed; PES PTS surfaced on the parent event (per § 2.12.4.1). |
| Stream type / shape mismatch (sync↔async fallback) | ✅ Full | When PMT and wire shape disagree (e.g. 0x15 PID with bare KLV), demuxer classifies on actual shape and emits `StreamTypeMismatch{Sync,Async}On*Pid` non-conformance. PES PTS preserved. |
| `metadata_descriptor` parser | ✅ Full | KLV→video link emitted in `ProgramMap.klv_links` as `LinkSource::Declared`. |
| KLV link inference (single video + single KLV) | ✅ Full | `LinkSource::Inferred` when no descriptor but topology is unambiguous. |
| `DemuxerConfigBuilder::link_klv` override | ✅ Full | Caller-supplied klv→video PID link (`LinkSource::Override`). |
| `DemuxerConfigBuilder::treat_as` override | ✅ Full | Caller-supplied `StreamKind` override per PID — overrides the PMT-derived kind. |
| PES reassembly cap (`pes_cap_per_pid`) | ✅ Full | Default 4 MiB per-PID; overflow surfaces as `Discontinuity::PesOversize`. |
| PES reassembly cap (`pes_cap_total`) | ✅ Full | Default 64 MiB aggregate; overflow surfaces as `Discontinuity::PesTotalOversize`. |
| Sync recovery (HUNT/VERIFY/LOCKED) | ✅ Full | Internal syncer state machine; ~6 KiB search window, then `DemuxError::Unrecoverable`. |
| `StrictMode::Off` (lenient default) | ✅ Full | `NonConformant` events surface as data; receive loop continues. |
| `StrictMode::TimingOnly` | ✅ Full | Hard-fail on `PcrAnomaly`, `PusiMidPes` (vestigial — not currently emitted), `PsiChecksumMismatch`. |
| `StrictMode::DescriptorsOnly` | ✅ Full | Hard-fail on `MissingMetadataDescriptor`, `StreamTypeMismatch{Sync,Async}OnPid`. |
| `StrictMode::Full` | ✅ Full | Hard-fail on every `NonConformantIssue` variant including future-added ones; except PAT multi-section (table_id 0x00), which degrades to a surface-only event ("don't blame the sender" — REF-PSI-02). |
| KLV-mismatch event coalescing | ✅ Full | One `StreamTypeMismatch*` event per (PID, PMT version); avoids flooding. |
| PSI version-bump detection | ✅ Full | Re-emits `ProgramMap` only on PMT/PAT version change. |
| PSI multi-section PAT (H.222.0 §2.4.4.5) | ⚙️ Partial | Multi-section PAT reassembled per §2.4.4.5; multi-section PMT is not supported (rejected with `PsiMultiSectionUnsupported`). Under `StrictMode::Full` a multi-section PAT degrades to a surface-only event (never a hard fail); PMT still hard-fails. |
| `pts_to_duration` helper | ✅ Full | 90 kHz ticks → `std::time::Duration`. |
| Multi-program TS | ✅ Full | Multi-PMT; one `ProgramMap` event per program + on PAT/PMT version bumps; `StreamInfo.program_number` on every `Sample`/`Metadata` event; PAT version diffing drops disappeared programs; `NonConformantIssue::PidReusedAcrossPrograms` on cross-program PID collision. |
| Subtitle classification on `stream_type 0x06` | ✅ Full | Cascade: subtitling/teletext/`VTTC`/`GA94` descriptors → `Subtitle` payload; KLV cases unchanged when no subtitle descriptor present. |
| AV1 / H.266 codec variants on `VideoCodec` | ✅ Full | Recognized and tagged on `SamplePayload::Video.codec`. The opt-in `split_video(&raw, codec, av1_carriage.unwrap_or_default())` returns `VideoPayload::Nals(_)` for `H266` (`stream_type=0x33`) and `VideoPayload::Obus(_)` for `Av1` (`stream_type=0x06` + AV01 registration). |
| Typed SPS/VPS/PPS payload parser | ✅ Full | `codec::h264` / `codec::h265` / `codec::h266` for NAL-shaped codecs; `codec::av1` for OBU-shaped. See `codec` block below. |
| Sync-KLV ↔ video AU pairing helper | ❌ Out of scope | Pairing is a consumer-domain decision; cookbook recipes 12–14 are the canonical patterns. |

## Pipeline composition (`tst-pipeline`)

### Send side

| Feature / Type | Status | Notes |
| --- | --- | --- |
| `MuxSender<T>` | ✅ Full | Composes `Muxer` + `Transport` for the canonical NAL+KLV → TS → SRT path. Internally synchronized; transient transport failures buffer in-flight bytes, delivered by the next successful send or a fallible `finish()` (prompt `close()` and final `Drop` drains are best-effort). |
| `Sender<T>` | ✅ Full | Pre-muxed TS bytes → SRT with sync framing/recovery. 3-byte sync verify, 7-packet bundling, RECOVER + STRICT modes. |
| `RawSender<T>` | ✅ Full | Byte-blind one-shot sender. One `send` call = one SRT message; size-cap validation at construction. |
| `ManagedTransport<T>` | ✅ Full | Reconnect + gap-buffer decorator over any `Transport`. Reconnect on the caller's thread by default (`ReconnectMode::Blocking`), or opt-in to a per-outage background worker thread (`ReconnectMode::Background`) so `send_bytes` doesn't wait on backoff or factory calls; drop-oldest-message overflow policy; single-thread receiver. |
| Multi-stream / multi-program `MuxerConfig` | ✅ Full | ≤16 programs, ≤16 video + ≤16 KLV streams per program; standalone-sub-builder shape — `MuxerProgramConfigBuilder::new(N, pmt_pid)` + `add_video(...)` / `add_klv(...)` + `build()`, then bound onto `MuxerConfigBuilder::add_program(...)`; opaque handles from `video_handles_for_program(N)` / `klv_handles_for_program(N)`; `push_video_to(handle, …)` / `push_klv_to(handle, …)` on `Muxer`; `send_video_to(handle, …)` / `send_klv_to(handle, …)` on `MuxSender`. Single-program single-stream callers keep the old flat API unchanged. |

### Receive side

| Feature / Type | Status | Notes |
| --- | --- | --- |
| `DemuxReceiver<R>` | ✅ Full | `RecvTransport → Receiver → Demuxer` shell. `recv_event` → `DemuxEvent`; auto-flushes demuxer on `Closed`. Implements `Iterator<Item = Result<DemuxEvent, DemuxReceiverError>>`. |
| `Receiver<R>` | ✅ Full | Pull bytes from a `RecvTransport`, run TS sync recovery, emit 188-byte aligned packets via `next_packet`. |
| `RawReceiver<R>` | ✅ Full | One `recv_one` call returns one owned byte vec — no TS framing or demux. |
| `ManagedRecvTransport<R>` | ✅ Full | Reconnect decorator for the receive direction. No gap buffer (recv-side bytes that never arrived can't be replayed); restarts on `Closed` / `Broken` per `ReconnectPolicy`. |
| `RecvTransport` trait | ✅ Full | Receive-side counterpart to `Transport`: `recv_bytes`, `max_payload`, `is_alive`, `close`. Implemented by `SrtTransport` and any consumer-side mock. |
| `SrtTransport` impl `RecvTransport` | ✅ Full | Same `SrtTransport` wrapper handles both send and receive directions on a connected SRT `Socket`. |
| `DemuxReceiver::add_byte_sink` fan-out | ✅ Full | Register `Box<dyn FnMut(&[u8]) + Send>` callbacks; each sink sees every 188-byte TS packet in registration order before the demuxer parses it. |
| `DemuxReceiver::with_demux_options` | ✅ Full | Construct a receiver around a custom `DemuxerConfig` (e.g. strict mode, PES caps, link overrides). |
| Stream-end contract | ✅ Full | `TransportError::Closed` → iterator termination after `Demuxer::flush`. `Broken` → `DemuxReceiverError::Transport(Broken(_))`. `Demux` → strict-mode rejection or malformed PES. |
| Receive-side gap buffer | ❌ Out of scope | Receive-side bytes that never arrived can't be replayed; no symmetric counterpart to `ManagedTransport`'s gap buffer. |

### Pairing (`tst_pipeline::ext::pairing`)

| Surface | Status |
|---|---|
| Rust API (`Pairer::with_config`, `Pairer::last_before_pts`, `feed`/`flush`/`stats`) | Shipped 2026-05-07 |
| C ABI exposure | Deferred to future receiver-surface plan |
| JNI exposure | Shipped — `org.tstrans.pipeline.Pairer` |
| UniFFI exposure | Deferred to future `tst-uniffi` plan |

### Receive-side entry points

The five transports split into two models: **connectionless** transports expose
a single one-shot factory (`listen`) that binds a socket and serves as the
receive transport for its lifetime, while **connection-oriented** transports
expose a `Listener` that loops over `accept` / `accept_blocking` to hand back
one dedicated transport per inbound peer. There is deliberately no
`TcpRecvTransport::listen` convenience wrapper: such a wrapper would accept
only the first connection and discard the listener, which is almost never the
right behaviour for a TCP server. `TcpListener::from_url` is the URL-style
constructor for the TCP listener; callers call `accept_blocking` once per peer.

| Transport | Model | Constructor | Argument | Yields |
|-----------|-------|-------------|----------|--------|
| UDP | Connectionless | `UdpRecvTransport::listen` | `url: &str` (`udp://@host:port`) | `UdpRecvTransport` |
| RTP | Connectionless | `RtpRecvTransport::listen` | `url: &str` (`rtp://host:port`) | `RtpRecvTransport` |
| RIST | Connectionless | `RistRecvTransport::listen` | `url: &str` (`rist://@host:port`) | `RistRecvTransport` |
| TCP | Connection-oriented | `TcpListener::from_url` / `TcpListener::bind` → `accept_blocking` | url (`tcp://…?listen=1`) / `SocketAddr` | one `TcpTransport` per accepted peer |
| SRT | Connection-oriented | `ListenerBuilder::bind` / `Listener::bind_with` → `accept` → `SrtTransport::new` | addr / config+addr | one `SrtTransport` per accepted peer |

---

## Codec parameter set parsing (`tst-core::codec`)

Stateless typed parsers for codec parameter sets. The demuxer event surface
is unchanged — NAL bytes surface as `NalUnit` with raw RBSP. Consumers call
these parsers explicitly when they need typed fields (resolution, profile,
level, color, frame rate). See [`guide-codec.md`](/docs/guides/codec.md).

| Codec | Rust core | C ABI |
| --- | --- | --- |
| H.264 SPS / PPS (`codec::h264`) | ✅ Full | ❌ Deferred (rides with receiver C ABI) |
| H.265 VPS / SPS / PPS (`codec::h265`) | ⚙️ Partial | ❌ Deferred |
| H.266 VPS / SPS / PPS (`codec::h266`) | ⚙️ Partial (VPS+SPS+PPS) | ❌ Deferred |
| AV1 Sequence Header + Frame Header light (`codec::av1`) | ⚙️ Partial | ❌ Deferred |

**H.264 notes:** wraps `h264-reader` 0.8; `parse_sps` / `parse_pps` /
`parse_parameter_sets`; partial-success-tolerant on combined call; strict on
per-set functions. 13/13 corpus fixtures matched ffprobe.

**H.265 notes:** hand-rolled per spec; `parse_vps` / `parse_sps` / `parse_pps` /
`parse_parameter_sets`. Full short-term RPS walker per H.265 §7.3.7 / §7.4.8
(mirrors ffmpeg's `ff_hevc_decode_short_term_rps`) walks past
`num_short_term_ref_pic_sets > 0` SPSes; tracks `NumDeltaPocs[]` for
`inter_ref_pic_set_prediction_flag` inheritance. VUI timing (§E.2.1) is
fully walked — `frame_rate` is recovered from `num_units_in_tick` /
`time_scale` when `vui_timing_info_present_flag = 1`. Known limitation:
bails with `CodecParseError::EngineError` on
`sps_scaling_list_data_present_flag = 1` SPSes — a parser gap (the
`scaling_list_data()` syntax at H.265 §7.3.4 is not yet walked), not a
profile-level rejection; conformant Main 10 HDR streams routinely set
this flag.

**H.266 notes:** hand-rolled per H.266 V4 §7.3 / §7.4; `parse_vps` /
`parse_sps` / `parse_pps` / `parse_parameter_sets`. Full SPS body walk
(entropy_coding_sync, POC config, partition constraints, ref_pic_list_struct,
virtual boundaries, general_timing_hrd_parameters) + `parse_h266_vui` per
§E.2.1 recover `frame_rate` (from `num_units_in_tick` + `time_scale` —
H.266 moves timing OUT of VUI vs. H.265) and `ColorInfo` (primaries +
transfer + matrix via H.273). APS NALs (types 17 / 18), Picture Header
NALs (type 19), and multi-layer streams (`nuh_layer_id != 0`) pass through
unparsed. Bails `UnsupportedProfile` on
`sps_subpic_info_present_flag = 1` / `sps_scaling_list_data_present_flag = 1`
(rare; not in reference encoder defaults). Empirical note: real VVenC
default-preset output does NOT emit a VUI block — `color_info` stays `None`
on those fixtures (recovery happens via the SPS body, not VUI).

**AV1 notes:** OBU-shaped (not NAL-shaped); `parse_sequence_header` /
`parse_frame_header_light` / `parse_obu_stream`. `Av1FrameHeaderLight`
surfaces `frame_type` + `show_frame` + `show_existing_frame` only;
`frame_size` is always `None` (full per-frame decode requires reference-
frame management beyond this parser's scope). Operating points beyond 0
walked but not surfaced. Tile Group / Metadata / Padding OBUs pass through
as `Obu { obu_type, payload, .. }` without further parsing.

### Audio frame parsing

| Codec | Status | Module |
|---|---|---|
| MPEG-1/2/2.5 Layer I/II/III | ✅ Full (Rust core) | `codec::mpegaudio` |
| AAC ADTS | ✅ Full (Rust core) | `codec::aac` |
| AAC LATM | ⚙️ Partial | `codec::aac::latm` — LOAS/LATM sync validator (`validate_latm_sync`); full `audioMuxElement` decode deferred. |
| AC-3 | ✅ Full (Rust core) | `codec::ac3` — syncframe parser (`parse_syncframe` / `Ac3SyncInfo`). E-AC-3 (Annex E, `bsid ≥ 11`) rejected as unsupported. |

The frame parsers surface header-level metadata (sample rate, channel
count, layer/profile, frame length, samples per frame, has-CRC). They
do not decode audio content or verify CRCs. C ABI exposure is deferred
to ride with the future receiver-surface plan.

---

## C ABI (`tst-c`)

| Feature | Status | Notes |
| --- | --- | --- |
| `tst_muxer_t` standalone utility | ✅ Full | open/push_video/push_klv/pull/close. Internally synchronized; data-path callable from multiple threads. |
| `tst_mux_sender_t` (plain L1) | ✅ Full | NAL+KLV in, TS+SRT out via `SrtTransport`. Internally synchronized. |
| `tst_managed_mux_sender_t` (managed L2) | ✅ Full | reconnect + gap buffer over `ManagedTransport<SrtTransport>`; synchronous retries on caller's thread. |
| `tst_ts_sender_t` / `tst_managed_ts_sender_t` | ✅ Full | pre-muxed TS bytes in; sync framing/recovery (RECOVER auto-resync + STRICT fail-fast); `_get_stats` accessor. |
| `tst_raw_sender_t` / `tst_managed_raw_sender_t` | ✅ Full | one `_send` call = one outbound SRT message. `TST_E_TOO_LARGE` on `len > SRTO_PAYLOADSIZE`. |
| Opaque builder configs | ✅ Full | `tst_mux_config_t`, `tst_ts_sender_config_t`, `tst_raw_sender_config_t`, `tst_reconnect_policy_t`. Internally cloned by `_open`; caller frees independently. |
| Thread-local last-error idiom | ✅ Full | `tst_get_last_error()` + `tst_get_last_error_str()`; ten `TST_E_*` codes covering all `tst_core` failure shapes. |
| `TST_VERSION_MAJOR` / `MINOR` / `PATCH` macros | ✅ Full | Compile-time `#define`s in `tstrans.h`. |
| Lifecycle (`_open` / `_close`) | ✅ Full | `_open` returns NULL on failure with last-error set; `_close` is NULL-safe (no-op on NULL); after a successful close the pointer is invalid and calling close again on the same non-null pointer is undefined behavior. Concurrent close-from-multiple-threads on the same live pointer is also UB — bindings must coordinate close against data-path use. |
| URL parsing | ✅ Full | `srt://host:port?key=value&...` — IPv4 / DNS / bracketed IPv6 hosts plus the libsrt-URL Group 1 vocabulary (`streamid` / `passphrase` / `latency` / `payloadsize` / `congestion` / `conntimeo` / `linger` / `udprcvbuf` / `udpsndbuf` / etc.) plus a handful of ffmpeg-style aliases (`pkt_size`, `payload_size`, `srt_streamid`, `tsbpddelay`, `smoother`, `ffs`, `connect_timeout`, `recv_buffer_size`, `send_buffer_size`). See "FFmpeg URL interop quirks" below for unit divergence. |
| Stats accessors | ✅ Full | Sender side: `tst_muxer_get_stats` / `_reset_stats`; `tst_mux_sender_*` and `tst_managed_mux_sender_*` (get + reset); `tst_ts_sender_*` and `tst_managed_ts_sender_*` (get + reset); `tst_raw_sender_*` and `tst_managed_raw_sender_*` (get + reset). Receiver side: `tst_raw_receiver_get_stats`, `tst_ts_receiver_get_stats`, `tst_receiver_get_stats`, `tst_demux_receiver_get_stats` + `tst_demux_receiver_get_stream_codec_stats`, plus `_get_socket_stats` for SRT-level counters. `tst_sender_stats_t` + `tst_muxer_stats_t` + `tst_raw_send_stats_t` + `tst_raw_recv_stats_t` + `tst_receiver_stats_t` + `tst_demux_receiver_stats_t` + `tst_stream_codec_stats_t` + `tst_socket_stats_t` `repr(C)` types. |
| Multi-stream `mpegts::mux` fan-out | ✅ Full | `tst_video_stream_handle_t` / `tst_klv_stream_handle_t` typedefs (transparent `uint32_t`); `tst_mux_config_add_video_stream` / `_add_klv_stream` return handles at config time; `_video_to(handle, ...)` / `_klv_to(handle, ...)` siblings on `tst_muxer_t`, `tst_mux_sender_t`, and `tst_managed_mux_sender_t` (≤16 video + ≤16 KLV streams per program — same caps as the Rust core). Single-target entry points (`tst_*_send_video` / `_send_klv`) keep their signatures and surface `MuxError::AmbiguousTarget` as `TST_E_INVALID_USAGE` on multi-stream muxers. The `Sender` / `RawSender` variants don't carry a `Muxer`, so multi-stream is N/A there. |
| Multi-program `mpegts::mux` config | ✅ Full | `tst_program_handle_t` (transparent `uint32_t`); `tst_mux_config_add_program` returns a handle; `tst_mux_config_add_video_stream_to_program` / `_add_klv_stream_to_program` scope streams to a program; ≤16 programs per muxer config. |
| Multi-program demux at the C ABI | ✅ Full | `tst_demux_receiver_t` ships today (`tst_demux_receiver_open` / `_recv_event` / `_get_stats` / `_get_stream_codec_stats` / `_close`); typed `tst_event_t` carries the same `DemuxEvent` shapes the Rust API emits, including multi-program PAT/PMT events. Pairs with `tst_raw_receiver_*` and `tst_ts_receiver_*` for callers that want bytes or TS packets without the demux step. |
| cbindgen-generated `tstrans.h` | ✅ Full | Committed at `bindings/c/include/tstrans.h`; CI verifies no drift via `tests/header_drift.rs`. |
| Symbol-prefix audit | ✅ Full | `tests/symbol_audit.rs` runs `nm -D` and asserts every exported symbol matches `^(tst_|TST_|srt_)` (`srt_*` allowlisted because libsrt is statically linked). |
| `pkg-config` metadata | ✅ Full | `tstrans.pc` generated by `build.rs` from `tstrans.pc.in`; substitutes `@VERSION@` and `@PREFIX@`. |
| Static-link discipline | ✅ Full | libsrt + mbedTLS + libstdc++ statically embedded into `libtstrans.so` and `libtstrans.a`; `ldd` shows only libc / libpthread / libstdc++ / libdl / libm. |
| Distribution artifacts | ✅ Full | `libtstrans.so` + `libtstrans.a` + `tstrans.h` + `tstrans.pc`. Tarball staged manually; GitHub Releases publishing not automated today. |
| End-to-end C smoke test | ✅ Full | `tests/smoke.c` compiled by `cc` and linked against the cdylib at test time; exercises muxer push/pull + every NULL-close path + invalid-URL last-error. |
| Live-socket roundtrip test | ✅ Full | `tests/live_pair.rs` binds a real `Listener` on 127.0.0.1, connects `tst_mux_sender_t`, sends a NAL, asserts the listener receives a TS sync byte. |
| Multi-platform Tier 1 | ✅ Full — Linux x86_64 + aarch64 + macOS arm64 + Windows MSVC all gating | See "Build targets" section at top of this document. |
| Pre-emptive close cancellation while parked in libsrt | ✅ Full | `Sender::close()` (and the underlying `Socket::cancel_handle()`) atomically closes the SRT handle from any thread, unblocking a peer thread parked in `srt_sendmsg`/`srt_recvmsg`. See [`pipeline.md`](/docs/guides/pipeline.md). |

---

## FFmpeg URL interop quirks

`ts-transformer` follows the libsrt-URL canonical conventions
(`srt-live-transmit`, OBS, mediamtx, gstreamer's `srtsink`/`srtsrc`,
Haivision Connect). FFmpeg's `srt://` protocol diverges in a few unit
conventions; users copying URLs between tools should be aware.

| URL key | FFmpeg unit | ts-transformer unit | Notes |
| --- | --- | --- | --- |
| `latency` | µs | ms | ts-transformer warns when value ≥ 10 s (likely paste from ffmpeg URL). |
| `rcvlatency` | µs | ms | Same warning. |
| `peerlatency` | µs | ms | Same warning. |
| `snddropdelay` | µs | (deferred) | Currently rejected as unsupported. |

When migrating an ffmpeg pipeline URL to ts-transformer, divide the latency
values by 1000.

ffmpeg-style key aliases honored by ts-transformer (zero new functionality —
just alternate spellings of existing keys): `pkt_size` / `payload_size`
(→ `payloadsize`), `srt_streamid` (→ `streamid`), `tsbpddelay` (→
`latency`), `smoother` (→ `congestion`), `ffs` (→ `fc`),
`recv_buffer_size` / `send_buffer_size` (→ `udprcvbuf` / `udpsndbuf`),
`connect_timeout` (→ `conntimeo`).

---

## Standards reference — what we cite vs. what we implement

The MISB / SMPTE / IETF documents that bear on `ts-transformer`. The
"Implemented?" column reflects what `ts-transformer` does, not what the spec
covers.

| Spec | Title | Implemented? |
| --- | --- | --- |
| **SMPTE ST 336** | Data Encoding Protocol Using Key-Length-Value | ✅ KLV substrate in `klv::pack` / `klv::length` / `klv::universal_label` |
| **MISB ST 0102.12** | Security Metadata Universal & Local Sets | ✅ LS form — typed decode + encode (`klv::st0102`); Universal Set form deferred |
| **MISB ST 0107.5** | KLV Metadata in Motion Imagery | ✅ Future-proof skip rule, UL family helpers |
| **MISB ST 0601.19** | UAS Datalink Local Set | ✅ 142 of 143 items typed or structured (see table above) |
| **MISB ST 0603.5** | Time Stamping Motion Imagery | ✅ For ST 0605 Time Status byte |
| **MISB ST 0604.6** | Time Stamping & Transport in MISB Motion Imagery | ⚙️ Opt-in — `Muxer::push_video_misp_to` / `MuxSender::send_video_misp_to` splice the MISP SEI (H.264 + H.265); `codec::misp_time::extract` recovers it. Commercial Time Stamp (UTC wall-clock SEI) and H.262/AV1/H.266 variants deferred. See [`stanag-4609.md`](/docs/reference/stanag-4609.md). |
| **MISB ST 0605.10** | Encoding & Inserting Time Codes / Stamps | ✅ Precision Time Stamp Pack |
| **MISB ST 0607.5** | UAS Datalink LS Time-Stamped Records | 🔁 Pass-through; not exercised by corpus |
| **MISB ST 0805.1** | KLV to Cursor-on-Target (CoT) Conversions | ✅ `klv::st0805::{platform_position_xml, sensor_point_of_interest_xml}` — Platform Position + Sensor Point of Interest CoT events from a decoded ST 0601 LS; deterministic `uid`s (`platform_uid`/`spi_uid`) and fixed attribute order for byte-stable replay. Rust, Python, and JVM (the C ABI carries raw KLV but exposes no typed KLV APIs). |
| **MISB ST 0806.4** | Remote Video Terminal (RVT) Local Set | ✅ `klv::st0806::{decode, decode_standalone, encode_to_vec, encode_to_vec_standalone}` — RVT LS + POI/AOI/User-Defined nested LSs; embedded via ST 0601 Tag 73 and standalone UL + CRC-32/MPEG-2 form. Rust, Python, and JVM (the C ABI carries raw KLV but exposes no typed KLV APIs). |
| **MISB ST 0807.27** | KLV Metadata Registry | ⚙️ Used as canonical source for UL constants |
| **MISB ST 0902.8** | Motion Imagery Sensor Minimum Metadata Set | ⚙️ Opt-in validator — `klv::st0601::validate_mismms` returns a `Vec<MismmsViolation>` per record; stream-level cadence tracker deferred. See [`stanag-4609.md`](/docs/reference/stanag-4609.md). |
| **MISB ST 0903.6** | Video Moving Target Indicator (VMTI) | ✅ LS form — typed top-level (`VmtiLs`) + per-target (`VTargetPack`) decode + encode (`klv::st0903`); 7 nested/sibling LSes pass-through (typed layers deferred); Universal Set form deferred |
| **MISB ST 1010.3** | Standard Deviation and Correlation Coefficient, Floating-Point (SDCC-FLP) | ✅ `klv::st1010::{decode_sdcc_flp, encode_sdcc_flp_mode2}` — Mode 1 (foreign/defensive parse) and Mode 2 (encode) + sparse bit-vector; general-purpose, also used by ST 0601 Tag 102 (`sdcc_flps`) |
| **MISB ST 1201.5** | IMAPB / IMAPA Floating-Point Mapping | ✅ §7.1.2 / §7.2 + §7.1.3 / §7.2.3 special values (`ImapbSpecial`, encode + decode); IMAPA form deferred |
| **MISB ST 1204.3** | Motion Imagery Identification System (MIIS) Core Identifier | ✅ `klv::st1204::{decode, encode_to_vec}` + ST 0601 Tag 94 (`miis_core_id`). Rust, Python, and JVM (the C ABI carries raw KLV but exposes no typed KLV APIs). See [`stanag-4609.md`](/docs/reference/stanag-4609.md). |
| **MISB ST 1303.2** | Multi-Dimensional Array Pack (MDAP) | ❌ Out of scope (no ST 0903 consumer) |
| **MISB ST 1402.2** | KLV in MPEG-2 Transport Streams | ✅ Async (0x06) + sync (0x15) modes in both `mpegts::mux` (encode) and `mpegts::demux` (decode) |
| **MISB ST 1607.2** | Constructs to Amend / Segment KLV | ❌ Out of scope (no multi-PES KLV in corpus) |
| **MISB ST 1910.1** | KLV in CMAF emsg boxes (HLS/DASH delivery) | ❌ Deferred — unrelated to MPEG-TS carriage; trigger on first CMAF/HLS consumer ask |
| **MISB TRM 0909.4** | Motion Imagery Quality Metadata | ⚙️ §7 multi-record PES pattern handled |
| **MISB RP 0802.2** | UAS Streaming Pipeline Recommendation | 📖 Reference reading |
| **MISB RP 1011.1** | Local Set Inheritance Recommendation | 📖 Reference reading |
| **MISP-2025.1** | Motion Imagery Standards Profile | 📖 Roadmap reference |
| **MISP-2025.1 Handbook** | MIS Handbook (companion to MISP) | 📖 Reference reading |
| **MISP-2023.2** | Motion Imagery Standards Profile (prior) | 📖 Reference reading |
| `draft-sharabayko-srt` (IETF) | Secure Reliable Transport Protocol | ✅ via `crates/srt-sys/vendor/srt` |
| `draft-sharabayko-srt-over-quic` | SRT over QUIC | ❌ Out of scope |

📖 = read for context; nothing to implement directly.

---

## Bindings & FFI presentation

| Crate | Status | Target |
| --- | --- | --- |
| `tstrans-srt-sys` | ✅ Full | Bindgen-generated FFI to libsrt 1.5.7; encryption via mbedTLS. |
| `tst-core` | ✅ Full | Safe Rust API — MPEG-TS mux/demux, KLV substrate + typed sets (ST 0601 / 0102 / 0605 / 0903 / 0806 / 1010 / 1204) + the ST 0805 KLV→CoT conversion layer, codec parsers (H.264 / H.265 / H.266 / AV1 / AAC / MPEG-2 audio), `Transport` + `RecvTransport` traits. No SRT dependency. |
| `tst-srt` | ✅ Full | SRT-specific safe wrapper — `Socket`, `Listener`, `SocketBuilder`, `SrtTransport`, `SrtRecvTransport`, `SrtCancelHandle`, URL parsing. Wraps libsrt 1.5.7. |
| `tst-pipeline` | ✅ Full | Composition layer — `MuxSender<T>` / `Sender<T>` / `RawSender<T>` / `DemuxReceiver<R>` / `Receiver<R>` / `RawReceiver<R>` shells; `ManagedTransport` reconnect wrapper; `Pairer` KLV↔video alignment. Decoupled from libsrt via the `Transport`/`RecvTransport` traits. |
| `tst-c` | ✅ Full | cdylib + staticlib + cbindgen-generated `tstrans.h` + pkg-config. ABI version **0.20** (additive minor bumps). Multi-platform Tier 1 (Linux x86_64 + aarch64 + macOS arm64 + Windows MSVC all gating). |
| `tst-py` | ✅ Full | PyO3 bindings, published to PyPI as **`tstrans`** (0.6.0). File I/O (inspect + offline build of `.ts`); typed KLV decode/encode for the core MISB sets (ST 0601 / 0102 / 0605 / 0903) plus ST 0806 / 1010 / 1204 and the ST 0805 KLV→CoT conversion layer; codec parsers; live UDP / TCP / RTP (incl. RTSP) / SRT / HLS / RIST transports + Pairer; TLS variants ship in the wheels (`tcps://` caller + listener, `rtsps://` client + server, HTTPS HLS) as does RIST PSK encryption; optional `[pandas]` extra for DataFrame + NumPy adapters. (RIST excluded from the Windows wheel.) |
| `tst-jni` | ✅ Full | JVM JAR for JDK 17+ consumers, distributed as `org.tstrans:tstrans-jvm` on Maven Central. Mirrors the Python surface package-for-package (`org.tstrans.{io,codec,klv,mpegts,rtp,srt,pipeline}`); RTP (incl. RTSP client + server) + SRT transports. |
| `tst-uniffi` | ⏳ Planned | iOS / Android via UniFFI (Swift / Kotlin). |

For full build-target / CI gating coverage see "Build targets" at the top of this document.

### H.264-over-RTP ingest (RFC 6184) — per-binding status

| Binding | Status | Notes |
|---|---|---|
| Rust (`tst-rtp`) | ✅ Full | `H264Depacketizer` / `H264Receiver` / `H264DepayConfig` / `H264Au` / `H264DepayStats`. Single-NALU, STAP-A, FU-A (modes 0 and 1). RTSP path via `setup_h264_auto` → `into_h264_receiver`. |
| Python (`tstrans.rtp`) | ✅ Full | `H264Receiver`, `H264AccessUnit`, `H264DepayConfig`, `ParameterSetInjection`, `H264DepayStats`, `RtpStats`. RTSP path via `RtspClient.connect_h264` → `session.into_h264_receiver()`. Ships in published wheels. |
| JVM (`org.tstrans.rtp`) | ✅ Full | `H264Receiver`, `H264AccessUnit`, `H264DepayConfig`, `ParameterSetInjection`, `H264DepayStats`, `RtpStats`. RTSP path via `RtspClient.connectH264` → `session.intoH264Receiver()`. Note: `intoH264Receiver()` consumes the session (differs from Python). Ships in the published JAR. |
| C (`tst-c`) | ❌ Deferred | No C-ABI `H264Receiver` family yet. Would be the next additive ABI minor bump. Trigger: a C/embedded consumer asks. See [`/docs/project/deferred-features.md`](/docs/project/deferred-features.md). |

**Intentional per-language conveniences.** A small set of APIs exist only in
one binding by design: `transmux` (Python-only convenience that bridges a
`Demuxer` and `Muxer` in one call; Rust callers compose them directly);
`split_video` (Rust/C API, not exposed in Python/JVM — callers there receive
`VideoSample.raw` bytes); `Pts90khz` wrapper type (Rust + Python, not in C/JVM
where ticks are passed as plain integers). These are not parity gaps but
deliberate ergonomic adaptations for each language's idiom.

---

## Sanitizers

Nightly GitHub Actions workflow `.github/workflows/sanitizers.yml`
(trigger: `schedule: '0 3 * * *'` UTC + on-demand `workflow_dispatch`)
runs four sanitizer jobs, all hard-gating. The workflow file is the
source of truth for this section; the current scope is:

- **AddressSanitizer + ThreadSanitizer, pure-Rust crates:** the six
  pure-Rust crates — `tst-core`, `tst-pipeline`, `tst-rtp`, `tst-udp`,
  `tst-tcp`, `tst-hls` — run their test suites under ASan and under
  TSan in separate jobs (sanitizers can't combine in a single binary).
- **Native AddressSanitizer + ThreadSanitizer, native-linking crates:**
  `tst-srt`, `tst-rist`, and `tst-c` run under ASan and under TSan with
  the vendored native libraries (libsrt, librist, mbedTLS) themselves
  compiled `-fsanitize=address` / `-fsanitize=thread` via the
  `TST_NATIVE_SANITIZER` build hook. The native ASan job has been
  hard-gating since 2026-08-19 and the native TSan job since
  2026-09-08, each after a dated phase-in window of job-level-green
  nightlies. Known librist-internal races are suppressed by exact
  function name in `.sanitizer-suppressions/tsan.txt`, with the
  harvest evidence recorded inline.
- JVM and Python memory-sanitizer coverage is deferred; see
  [deferred-features](/docs/project/deferred-features.md).

## Fuzzing

32 cargo-fuzz harnesses live in-tree (27 in `tst-core`, 4 in `tst-rtp`,
1 in `tst-srt`; inventory ground truth is
`tests/coverage/fuzz-targets.toml`). CI compile-checks the harnesses
nightly; deeper runs are local (`cargo +nightly fuzz run <target>`).

OSS-Fuzz status: **onboarding artifacts exist under `oss-fuzz/`
(Dockerfile + build script bundling all 32 harnesses), but the project
is not yet enrolled upstream — there is no continuous OSS-Fuzz fleet
coverage today.** Enrollment is a planned maintainer-driven PR to
`google/oss-fuzz`; the bundle requires a fresh local verification run
before submission (see `oss-fuzz/VERIFICATION.md`).

---

## Out of scope

These items appear in nearby specs but are explicitly **not** on the
roadmap. They are revisitable on consumer ask — not philosophical refusals.

- Containers other than MPEG-TS (MP4 / fMP4 / CMAF, Matroska / WebM).
- Metadata sets other than the typed MISB family already shipped (ST 1303 MDAP; ST 0902 MISMMS is an opt-in validator — see [`stanag-4609.md`](/docs/reference/stanag-4609.md)).
- WebRTC / RTMP transports (UDP, raw TCP / TLS, RTP, SRT, and RIST all ship — see top-of-document scope note).
- ST 1607 segmented multi-PES KLV reassembly.
- Async / reactor SRT API.
- Bonded / grouped sockets.
