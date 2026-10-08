# tst-rtp integration-test movement map

The 27 former top-level `tests/*.rs` integration binaries were consolidated
into **4 domain harnesses** (`rtp`, `rtcp`, `rtsp_client`, `rtsp_server`),
same `#[path] mod` pattern as the other crates: `tests/<domain>.rs` includes
its members from `tests/<domain>/`.

## What changed (and what did not)

- **Test bodies are unchanged.** Pure relocation.
- The `rtcp_` / `rtsp_client_` / `rtsp_server_` filename prefixes (redundant
  with the domain dir) were dropped; the `rtp` members kept their names.
- Fully-qualified paths gained a `<domain>::<file>::` prefix. Filtering still
  works: `cargo test -p tst-rtp --test rtsp_server mount::`.
- The shared fixtures (`fixtures/rtsp_loopback_server.rs`,
  `fixtures/tls_certs.rs`) are now declared once at each domain binary's root
  (`#[path = "fixtures/mod.rs"] mod fixtures;`) instead of per-file; member
  imports changed from `use fixtures::…` to `use crate::fixtures::…`. (The
  `rtp` domain doesn't use fixtures, so it has none. `tls_certs` stays
  `#[cfg(feature = "tls")]`, so no-default-features builds are unaffected.)

## Equivalence check

No test added/dropped/renamed: tst-rtp's `cargo test -- --list` count is
unchanged (310) and the test leaf-name multiset is byte-identical before/after
(active + `--ignored`, both feature modes).

## Movement table

### `rtp/` — raw RTP-over-UDP unicast + multicast loopback

| old `tests/…` | new `tests/…` |
| --- | --- |
| `loopback_multicast.rs` | `rtp/loopback_multicast.rs` |
| `loopback_unicast.rs` | `rtp/loopback_unicast.rs` |

### `rtcp/` — RTCP receiver/sender reports over RTP and RTSP-interleaved transports

| old `tests/…` | new `tests/…` |
| --- | --- |
| `rtcp_interleaved.rs` | `rtcp/interleaved.rs` |
| `rtcp_loopback.rs` | `rtcp/loopback.rs` |
| `rtcp_via_rtsp.rs` | `rtcp/via_rtsp.rs` |

### `rtsp_client/` — RTSP client: SETUP/PLAY/TEARDOWN, auth, fallback, TLS, keepalive, interleaved

| old `tests/…` | new `tests/…` |
| --- | --- |
| `rtsp_client_auth.rs` | `rtsp_client/auth.rs` |
| `rtsp_client_fallback.rs` | `rtsp_client/fallback.rs` |
| `rtsp_client_interleaved_e2e.rs` | `rtsp_client/interleaved_e2e.rs` |
| `rtsp_client_keepalive.rs` | `rtsp_client/keepalive.rs` |
| `rtsp_client_setup_play.rs` | `rtsp_client/setup_play.rs` |
| `rtsp_client_teardown.rs` | `rtsp_client/teardown.rs` |
| `rtsp_client_tls.rs` | `rtsp_client/tls.rs` |
| `rtsp_client_tls_keepalive.rs` | `rtsp_client/tls_keepalive.rs` |

### `h264/` — RFC 6184 H.264-over-RTP: UDP loopback round-trips + RTSP session

| new `tests/…` | description |
| --- | --- |
| `h264/common.rs` | LCG PRNG for the loss soak; re-exports the RFC 6184 payloader + `expected_annexb` from `fixtures/h264_payloader.rs` |
| `h264/udp_loopback.rs` | Multi-AU roundtrip + randomized-loss soak (p=0.2, 200 AUs, fixed seed) |
| `h264/rtsp_session.rs` | `setup_h264_auto` mode-1 roundtrip + mode-2 pre-SETUP rejection |

### `rtsp_server/` — RTSP server: mounts, auth, multicast, TLS, shutdown, interleaved/UDP transports

| old `tests/…` | new `tests/…` |
| --- | --- |
| `rtsp_server_auth_basic.rs` | `rtsp_server/auth_basic.rs` |
| `rtsp_server_auth_digest.rs` | `rtsp_server/auth_digest.rs` |
| `rtsp_server_bind.rs` | `rtsp_server/bind.rs` |
| `rtsp_server_concurrent.rs` | `rtsp_server/concurrent.rs` |
| `rtsp_server_lagging_peer.rs` | `rtsp_server/lagging_peer.rs` |
| `rtsp_server_loopback_interleaved.rs` | `rtsp_server/loopback_interleaved.rs` |
| `rtsp_server_loopback_udp.rs` | `rtsp_server/loopback_udp.rs` |
| `rtsp_server_mixed_transports.rs` | DELETED — was an empty no-op placeholder for a mixed UDP+multicast assertion; both legs are covered in isolation (`loopback_udp.rs`, `multicast.rs`) and the scenario adds no new server behavior |
| `rtsp_server_mount.rs` | `rtsp_server/mount.rs` |
| `rtsp_server_multicast.rs` | `rtsp_server/multicast.rs` |
| `rtsp_server_notice_5402.rs` | `rtsp_server/notice_5402.rs` |
| `rtsp_server_session_keepalive.rs` | `rtsp_server/session_keepalive.rs` |
| `rtsp_server_shutdown.rs` | `rtsp_server/shutdown.rs` |
| `rtsp_server_tls.rs` | `rtsp_server/tls.rs` |

### `rtsp_server/publish/` — RTSP publisher role (ANNOUNCE / SETUP `mode=record` / RECORD), MP2T

New members (no former top-level file), registered at the `rtsp_server.rs`
root as `#[path = "rtsp_server/publish/<file>.rs"] mod publish_<file>;` — one
`#[path] mod` per file, no `publish/mod.rs`. Filter with
`cargo test -p tst-rtp --test rtsp_server publish_`.

| file | covers |
| --- | --- |
| `rtsp_server/publish/mp2t_interleaved.rs` | TCP-interleaved publisher → app transport + PLAY reader, byte-identical |
| `rtsp_server/publish/mp2t_udp.rs` | UDP publisher (source latched from the first datagram) → app transport |
| `rtsp_server/publish/second_publisher.rs` | second ANNOUNCE on a live mount is 403 |
| `rtsp_server/publish/republish.rs` | mount outlives publishers (TEARDOWN, dropped connection), `generation` |
| `rtsp_server/publish/rtsps_record.rs` | interleaved RECORD over `rtsps://` (`rtsp-server-tls`) |
| `rtsp_server/publish/lifecycle.rs` | cancel, publisher TEARDOWN, server stop Notice 5402, idle reaping vs media liveness |
| `rtsp_server/publish/es_h264.rs` | ffmpeg-shaped H.264 publisher (sprop SDP, `Range: npt=0.000-`, SR first), interleaved → app `DemuxReceiver` + PLAY reader; UDP → app |
| `rtsp_server/publish/es_h264_klv.rs` | GStreamer-shaped H.264 + KLV publisher: KLV PTS from sender reports (spec §2 formula, ±1 tick), `Provisional` fallback without them |
| `rtsp_server/publish/conformance.rs` | shared receive-side conformance kit on the app transport |
| `fixtures/raw_rtsp_publisher.rs` | raw-socket publisher (`RawPublisher`, plain + TLS), `SDP_MP2T`, `rtp_wrap`, `ts_fixture_packets`; elementary shapes `SDP_H264`, `SDP_H264_KLV`, `h264_rtp_packets`, `klv_rtp_packet`, `klv_unit`, `sr_packet`, `demux_until_quiet` |
| `fixtures/h264_payloader.rs` | test-only RFC 6184 payloader (`packetize`, `expected_annexb`, `build_rtp_packet`), re-exported by `h264/common.rs` |
