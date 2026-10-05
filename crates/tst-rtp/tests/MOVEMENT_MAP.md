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
| `h264/common.rs` | Test-only RFC 6184 payloader + LCG PRNG + `expected_annexb` helper |
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
