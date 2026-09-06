# tst-c examples (C)

Runnable C examples for the `tst-c` C ABI. Linux x86_64 only by build
convention (cbindgen-generated header + cdylib / staticlib).

If you're new to the project and want to start in Rust, the equivalent
curriculum lives at [`../../../examples/`](../../../examples/) — the C
examples here mirror that taxonomy.

## Build

```sh
cd bindings/c
# Transports are opt-in. The offline mux/demux examples build with a bare
# `cargo build`; the transport examples (sending/, receiving/, the rtp/srt
# scenarios) need their feature: e.g. --features srt,rtp (or udp/tcp/hls/rist).
cargo build --features srt,rtp   # produces target/debug/libtstrans.{so,a} + target/debug/include/tstrans.h
gcc -I ../../target/debug/include -L ../../target/debug \
    -Wall -Werror -o /tmp/<name> \
    examples/<category>/<name>.c -ltstrans
LD_LIBRARY_PATH=../../target/debug /tmp/<name>
```

Use the header cbindgen generated for the feature set you just built
(`target/debug/include/tstrans.h`). The committed `include/tstrans.h` is the
`--features srt,rtp` rendering: it only defines `TST_HAS_SRT` / `TST_HAS_RTP`,
so the udp / tcp / hls / rist examples stop at their `#error` guard against it.
One trap: that generated header reflects the LAST tst-c build, and cargo only
reruns cbindgen when the build is not fresh — so if you build with fewer
features and then re-run the all-features build from cache, the narrower
header stays on disk. `touch bindings/c/cbindgen.toml` before rebuilding
forces it to regenerate (the rail below does this for you). Examples that
spawn threads add `-lpthread`.

To compile every example in one go (what CI does on the linux leg):

```sh
bash scripts/check/c/examples-compile.sh    # from the workspace root
```

For pkg-config-using build systems, `tstrans.pc` is emitted alongside
the cdylib once `tstrans.pc.in` has been substituted by the build —
add `target/debug` to `PKG_CONFIG_PATH` and use
`pkg-config --cflags --libs tstrans`.

## Read in this order

### 1. `getting-started/hello_world.c` — smallest possible mux + KLV

The C twin of the Rust [`hello_world.rs`](../../../examples/getting-started/hello_world.rs).
Builds 1 MPEG-TS frame containing 1 video AU + 1 KLV record using the
`tst_muxer_t` muxer-only handle, drains the bytes, prints the count
(byte-identical to the Rust version: 752 bytes / 4 packets).

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/hello_world
```

### 2. `getting-started/version_check.c` — verify loaded library matches header

Cross-validates every `(runtime accessor, header macro)` pair at process
startup. Queries `tst_get_version_major/minor/patch/packed/string` and
`tst_get_abi_version_major/minor`, compares each against the corresponding
`TST_*_VERSION_*` compile-time macro from `<tstrans.h>`, and exits 0 only
when all values agree.

The canonical pattern for binding authors to copy into their own startup
checks (e.g. `JNI_OnLoad` for `tst-jni`, the UniFFI init hook for
`tst-uniffi`):

```c
if (tst_get_abi_version_major() != TST_ABI_VERSION_MAJOR) {
    fprintf(stderr, "tstrans ABI major mismatch\n");
    exit(1);
}
if (tst_get_abi_version_minor() < TST_ABI_VERSION_MINOR) {
    fprintf(stderr, "tstrans ABI minor too old\n");
    exit(1);
}
```

Expected output (versions will match your build):

```text
Package version (matches Cargo.toml):
  TST_VERSION_MAJOR          runtime=0  header=0  [OK]
  TST_VERSION_MINOR          runtime=1  header=1  [OK]
  TST_VERSION_PATCH          runtime=0  header=0  [OK]
  packed (M<<16|m<<8|p)      runtime=256  header=256  [OK]
  version string             runtime="0.1.0"

ABI contract version (breaking-change cadence):
  TST_ABI_VERSION_MAJOR      runtime=0  header=0  [OK]
  TST_ABI_VERSION_MINOR      runtime=1  header=1  [OK]

After tst_clear_last_error():
  tst_get_last_error()     = 0  (expect 0 = TST_E_SUCCESS)
  tst_get_last_error_str() = ""  (expect empty)

OK: all runtime/header pairs match. Loaded libtstrans is consistent with the tstrans.h compiled into this binary.
```

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/version_check
```

### 3. `muxing/mux_synthetic_srt.c` — sender + synthetic frames over SRT

Open a `tst_mux_sender_t` against an SRT URL, push 5 synthetic H.264 +
KLV frames, close. The C analogue of the Rust
[`send_pipeline_to_socket.rs`](../../../examples/sending/send_pipeline_to_socket.rs).

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/mux_synthetic_srt 127.0.0.1:9000
```

### 4. `muxing/mux_dual_camera.c` — multi-stream within one program

Diff from §3: two video streams (EO + IR) sharing one program + KLV.
Demonstrates `tst_mux_config_add_video_stream` returning per-stream
handles and `tst_muxer_push_video_to(handle, ...)` for fan-out.
Mirrors the Rust [`mux_dual_camera.rs`](../../../examples/muxing/mux_dual_camera.rs).

### 5. `muxing/mux_two_programs.c` — multi-program

Diff from §4: two PMTs in one PAT, each with its own video + KLV
streams. Shows the `tst_program_handle_t` flow and the
`tst_*_to(prog_handle, ...)` siblings. No Rust twin yet — the equivalent
recipe lives in the cookbook ([Repack two single-program inputs into one multi-program TS](../../../docs/cookbook/muxing/repack-multi-program.md))
under a different shape (demux + re-mux instead of synthetic frames).

### 6. `operations/poll_socket_stats.c` — live libsrt wire-stats polling

Push 5 seconds of synthetic video through a `tst_mux_sender_t` and
print RTT + bandwidth + loss + retransmits every 500 ms using
`tst_mux_sender_get_socket_stats`. Shows the operational-telemetry
counterpart to the app-level `tst_mux_sender_get_stats` — wire-level
visibility into what the network actually did (vs what you asked
libsrt to do). The C-side analogue of the
[`operations/`](../../../examples/operations/) Rust examples.

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/poll_socket_stats srt://127.0.0.1:9000
```

### 7. `sending/send_rtp.c` — RTP unicast/multicast sender (raw TS bytes)

Open a `tst_rtp_sender_t` to an `rtp://` URL, push 100 synthetic 188-byte
MPEG-TS null packets via `tst_rtp_sender_send_ts`, then close. The
lowest-level RTP send API — caller supplies pre-built TS packets; the
library handles RTP framing (RFC 2250), UDP packetisation, and SSRC/sequence
management. Use `tst_rtp_mux_sender_t` instead when you need the library to
also mux encoded video/KLV/audio into TS for you.

```sh
# unicast (loopback default)
LD_LIBRARY_PATH=../../target/debug /tmp/send_rtp

# explicit unicast destination
LD_LIBRARY_PATH=../../target/debug /tmp/send_rtp --dest 192.168.1.100:5000

# multicast (org-local scope, RFC 2365)
LD_LIBRARY_PATH=../../target/debug /tmp/send_rtp --dest 239.1.2.3:5000
```

### 8. `sending/send_rtsp_server.c` — RTSP server: unicast mount + push loop

Start an RTSP server, register a `/live` mount, and push synthetic H.264 +
MISB ST 0601 KLV frames in a 30 fps loop. Multiple RTSP clients can connect
simultaneously — each gets its own view of the same fanout channel.

Key concepts demonstrated:
- Builder chain: `_max_sessions`, `_session_timeout`, `_fanout_capacity`,
  `_graceful_shutdown_drain_ms`.
- Unicast mount registration (`tst_rtsp_server_add_unicast_mount`) and how
  it differs from the SRT point-to-point sender.
- Signal-safe shutdown: `tst_rtsp_server_cancel_handle` + SIGINT handler,
  followed by `tst_rtsp_server_stop` (graceful drain + RFC 7826 Notice 5402)
  from the main thread.
- Per-mount stats polling (`tst_rtsp_mount_get_stats`) and server stats
  (`tst_rtsp_server_get_stats`).

```sh
# Terminal 1 — start the server
LD_LIBRARY_PATH=../../target/debug /tmp/send_rtsp_server \
    --bind rtsp://0.0.0.0:8554 --mount /live
# Terminal 2 — play the stream
ffplay rtsp://127.0.0.1:8554/live
```

### 9. `sending/send_srt.c` — SRT TS-bytes sender from a URL + the URL error vocabulary

The SRT entry the `send_<proto>` grid was missing. Opens a `tst_sender_t`
from an `srt://host:port?key=value` URL, pushes 100 null TS packets (14 full
7-packet bundles + 2 flushed explicitly), then walks a table of ten
malformed URLs and prints the code + detail string each one produces —
every URL-shaped mistake is `TST_E_INVALID_CONFIG` before any socket
exists; an unreachable peer is `TST_E_TRANSPORT`. The C twin of the Rust
[`sender_from_url.rs`](../../../examples/sending/sender_from_url.rs), and
the place to read about the one structural difference: the C ABI has no
standalone parse step, `_open` is parse + connect, and the URL is the
only per-connection socket-option surface.

```sh
# Terminal 1 — a listener (recv_ts_to_file binds srt://:7000)
LD_LIBRARY_PATH=../../target/debug /tmp/recv_ts_to_file /tmp/out.ts
# Terminal 2
LD_LIBRARY_PATH=../../target/debug /tmp/send_srt
# -> "sent 100 TS packets (18800 bytes), flushed, closed." then the 10-row table
```

Verified 2026-09-06: 18,800 bytes land in `/tmp/out.ts`; the table's last
row (`srt://:9000?mode=listener` handed to a sender) fails at connect
time, not validation — the C sender family is caller-only, see §11.

### 10. `sending/send_srt_encrypted.c` — passphrase-encrypted send + receive in one process

A listener thread (`tst_raw_receiver_open_listener`) and a caller on
main (`tst_raw_sender_open`), both configured entirely through
`?passphrase=...&pbkeylen=32&latency=120` (the caller adds `streamid`).
Sixteen messages over AES-256, count asserted, exit 0. Shows the RAW
message-oriented handle pair, that the listener-side open is bind +
accept in one blocking call with the key exchange inside the handshake,
and the free-port + ready-flag choreography for a two-thread demo. The C
twin of [`encrypted_send_recv.rs`](../../../examples/sending/encrypted_send_recv.rs).

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/send_srt_encrypted
# -> 16 "listener: recv 20 bytes" lines, "OK: 16 encrypted messages round-tripped"
```

A passphrase mismatch never yields a connected handle on either side: the
caller's open fails with `TST_E_TRANSPORT` ("peer rejected connection:
BadSecret") and the listener keeps waiting (verified 2026-09-06).

### 11. `sending/send_srt_ts_file.c` — relay a recorded `.ts` over SRT, PCR-paced

Reads a `.ts` file 188 bytes at a time and pushes it through a
`tst_sender_t` at real-time pace: the first PCR is anchored to the wall
clock and every later PCR-bearing packet waits until the clock has moved
as far as the PCR has — what `ffmpeg -re` does, so a player's jitter
buffer is never flooded. Re-anchors on a backwards PCR jump (rollover or
`--loop` rewind), flushes + drains one latency window before close,
`--no-pace` to blast, SIGINT-safe. The caller-mode counterpart of the Rust
[`srt_serve_ts_file.rs`](../../../examples/sending/srt_serve_ts_file.rs):
that example LISTENS so VLC can dial in; the C ABI's sender family only
dials out (see `docs/project/deferred-features.md`, "SRT URL mode
dispatch"), so this one dials a listening receiver instead.

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/mux_to_file /tmp/in.ts 5        # any .ts works
LD_LIBRARY_PATH=../../target/debug /tmp/recv_ts_to_file /tmp/copy.ts    # terminal 1
LD_LIBRARY_PATH=../../target/debug /tmp/send_srt_ts_file /tmp/in.ts 127.0.0.1:7000   # terminal 2
cmp /tmp/in.ts /tmp/copy.ts && echo identical
```

Verified 2026-09-06 with the 5 s file from §12: byte-identical copy on the
receiver in both modes; the paced run takes the file's PCR span plus the
connect and drain windows.

### 12. `muxing/mux_to_file.c` — the standalone muxer to a file, no transport

The simplest muxer pipeline: config → `tst_muxer_open` → push one
synthetic H.264 AU + one async-KLV record per frame at 30 fps →
`tst_muxer_pull` after every push → `fwrite`. Reproduces the Rust
[`mux_to_file.rs`](../../../examples/muxing/mux_to_file.rs) config, cadence,
AU sizes and KLV bytes exactly, so the two produce **byte-identical**
files — the proof that the C config builder drives the same muxer with
the same defaults as the Rust API.

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/mux_to_file /tmp/out.ts 5
cargo run -p tst-examples --example mux_to_file -- /tmp/rust.ts 5 && cmp /tmp/out.ts /tmp/rust.ts
```

Verified 2026-09-06: 150 frames = 196,836 bytes = 1,047 TS packets from
both; `ffprobe` reports h264 + KLV.

### 13. `muxing/mux_h265_with_klv.c` — H.265 + synchronous KLV

Diffs from §12 to show exactly which knobs flip: `TST_VIDEO_CODEC_H265`,
`TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA` with `carries_pts = true` (the
pair is mandatory together; the muxer rejects sync KLV without PTS at
open), and the two-byte H.265 NAL header. Explains the caller
responsibility with sync KLV — pass raw KLV bytes, the muxer prepends the
5-byte Metadata AU cell itself. Byte-identical to the Rust
[`mux_h265_with_klv.rs`](../../../examples/muxing/mux_h265_with_klv.rs).

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/mux_h265_with_klv /tmp/h265.ts
```

Verified 2026-09-06: 150 frames = 228,044 bytes = 1,213 TS packets from
both; `ffprobe` reports hevc + KLV.

### 14. `operations/managed_reconnect.c` — the managed sender vs a flaky peer

`tst_managed_mux_sender_t` in the default Blocking mode against a peer
thread that accepts, reads 5 messages, closes, and re-listens — twice —
then drains to a clean close. The `tst_reconnect_policy_t` is built knob
by knob (20 attempts, exponential 50 ms → 2 s, gap 256, DROP_OLDEST) with
the rationale for each value; the sender's loop treats send errors as
informational and never sees the outages. Peer mechanics worth reading
before copying: the listener socket is released at accept, so closing
the accepted handle IS the disconnect simulation; a thread blocked in
`_open_listener` cannot be cancelled through the C ABI, so main joins
with a deadline. The C twin of
[`managed_reconnect.rs`](../../../examples/operations/managed_reconnect.rs).

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/managed_reconnect
# -> peer rounds 0/1 "dropping after 5 messages", round 2 clean close,
#    "OK: completed run with reconnects (sent_ok=30, sent_err=0, successes=2)"
```

Verified 2026-09-06: 5 of 5 consecutive runs, ~2 s each.

### 15. `operations/managed_reconnect_background.c` — Background mode + eviction stats

Same shape, one knob flipped: `TST_RECONNECT_MODE_BACKGROUND`, so the
producing thread keeps its 10 ms cadence straight through a ~300 ms
outage while a worker thread reconnects. Gap capacity is deliberately 4,
so the live stats line (polled without blocking — a Background-mode
property) shows `DROP_OLDEST` evicting ~200 messages: a `0` from
`send_video` means "accepted", not "delivered". The header explains the
two things the printout shows that surprise people — `attempts=1` for
the whole gap (the single re-dial rides libsrt's handshake retry) and the
peer's second round closing after 0 messages (prompt close on the
just-recovered link) — both identical in the Rust twin
[`managed_reconnect_background.rs`](../../../examples/operations/managed_reconnect_background.rs).

```sh
LD_LIBRARY_PATH=../../target/debug /tmp/managed_reconnect_background
# -> "OK: completed run with a background reconnect (sent_ok=120, ... successes=1, dropped_msgs=~200)"
```

## Receiver-side C examples

The C ABI covers both sender (mux + raw/TS sender) and receiver (demux +
raw/TS receiver) surfaces. Receiver-side C examples ship under
[`receiving/`](receiving/):

- [`receiving/recv_ts_to_file.c`](receiving/recv_ts_to_file.c) — TS-bytes
  receiver: drain TS packets straight to a file via the C ABI's Receiver
  shape.
- [`receiving/recv_demux_to_console.c`](receiving/recv_demux_to_console.c)
  — DemuxReceiver event walk: print one line per demux event to stdout.
- [`receiving/recv_klv_to_stdout.c`](receiving/recv_klv_to_stdout.c) — KLV
  extraction: dump KLV payloads as hex / file on the side of a Demuxer.
- [`receiving/recv_raw_to_file.c`](receiving/recv_raw_to_file.c) — raw
  socket bytes (no TS framing): the lowest-level RawReceiver shape.
- [`receiving/recv_rtp.c`](receiving/recv_rtp.c) — RTP
  multicast demux receiver: join an IP multicast group, walk the full
  `tst_event_t` event-kind switch, and shut down gracefully on SIGINT via
  `tst_rtp_demux_receiver_cancel`. The RTP twin of
  `recv_demux_to_console.c`. Requires `TST_HAS_RTP`.
- [`receiving/recv_rtsp_camera.c`](receiving/recv_rtsp_camera.c) —
  full RTSP client lifecycle: builder chain with Digest MD5 auth and
  transport preference (UDP / TCP-interleaved / auto), `_connect`,
  `_play`, `into_demux_receiver` bridge to typed event loop, SIGINT
  cancel, and cleanup. Canonical pattern for consuming a gimbaled-platform
  camera stream over RTSP.
- [`receiving/recv_srt_events.c`](receiving/recv_srt_events.c) — the
  MANAGED (auto-reconnecting) SRT demux receiver reference example: full
  `tst_event_t` kind coverage including `TST_EVENT_KIND_RECONNECT_DISCONTINUITY`
  (the boundary marker `recv_demux_to_console.c`'s switch has no case
  for), inline MISB ST 0601 KLV decode via `tst_st0601_decode` /
  `tst_st0601_geometry`, and the documented cancel-then-close SIGINT
  shutdown ordering. Supersedes `recv_demux_to_console.c` for the
  managed+caller+KLV-decode case, and is the behavioral reference the
  Apple/Swift wrapper is written against.
