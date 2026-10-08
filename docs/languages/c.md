# C bindings

> **Who this is for:** You write C (or any language that links against C) and want to embed ts-transformer in your application via a stable C ABI — for an embedded Linux target, a third-language wrapper (Go, Lua, C#, Erlang NIF), or a host application where Rust is not on the table.

> **You will learn:**
> - How to link against `libtstrans.so` (or `libtstrans.a` static) via pkg-config
> - How to write a hello-world that produces a `.ts` file with ~10 lines of C
> - How to push video + KLV through an SRT sender
> - How to drive the demux receiver on a live SRT stream
> - The C-specific gotchas: `_close` lifecycle, handle ownership, error code surface
> - Where the C surface differs from the Rust core (opaque pointers, flat enums, panic-to-error mapping)

## Install

The C ABI ships as the `tst-c` crate in the workspace; its build emits the artifacts a C consumer needs:

| Artifact | Path (after `cargo build`) | Purpose |
|---|---|---|
| `libtstrans.so` (Linux) / `libtstrans.dylib` (macOS) / `tstrans.dll` (Windows-MSVC) | `target/debug/` or `target/release/` | Shared library |
| `libtstrans.a` (`tstrans.lib` on MSVC) | same | Static library — libsrt + mbedTLS embedded; link the C++ runtime yourself (`tstrans.pc` lists it) |
| `tstrans.h` | `target/<profile>/include/` | Single-file C header (~450 KB), `cbindgen`-generated for the features you built. The committed `bindings/c/include/tstrans.h` is the `srt,rtp` rendering. |
| `tstrans.pc` | `target/<profile>/` | pkg-config file (substituted by `build.rs` from `tstrans.pc.in`) |

### From source

```sh
git clone --recurse-submodules https://github.com/aklofas/ts-transformer
cd ts-transformer
# Transports are opt-in features: srt, rtp, udp, tcp, hls, rist.
SRT_FORCE_VENDORED=1 cargo build -p tst-c --release --features srt
# Artifacts land in target/release/ (header: target/release/include/tstrans.h)
```

The build vendors libsrt 1.5.7 + mbedTLS 3.6.x statically — your `libtstrans.so` has no external dependencies beyond the C and C++ runtimes (libc, libm, libstdc++, libgcc_s; older glibc also splits out libpthread and libdl). Verify with `ldd target/release/libtstrans.so`.

### Compile + link

Direct gcc/clang:

```sh
gcc -I target/release/include \
    -L target/release \
    -Wl,-rpath,target/release \
    -Wall -Werror -o myapp \
    myapp.c -ltstrans
```

With pkg-config (recommended for build systems). The generated `tstrans.pc` assumes an install under `/usr/local` (`lib/` and `include/`), so copy the library and header there first:

```sh
export PKG_CONFIG_PATH=$PWD/target/release:$PKG_CONFIG_PATH
gcc -Wall -Werror -o myapp myapp.c $(pkg-config --cflags --libs tstrans)
```

### ABI versioning

Every release exposes two pairs of macros in `tstrans.h` plus matching runtime accessors:

```c
#define TST_VERSION_MAJOR        0   // package (Cargo.toml) version
#define TST_VERSION_MINOR        7
#define TST_VERSION_PATCH        0
#define TST_ABI_VERSION_MAJOR    0   // C ABI contract version
#define TST_ABI_VERSION_MINOR    23
```

The **ABI** pair is what binding consumers should pin against. Minor bumps are additive (new entry points / new enum variants); a future major bump will be breaking (none yet — sitting at `0` pre-1.0). Check at process startup:

```c
if (tst_get_abi_version_major() != TST_ABI_VERSION_MAJOR) {
    fprintf(stderr, "tstrans ABI major mismatch\n");
    return 1;
}
if (tst_get_abi_version_minor() < TST_ABI_VERSION_MINOR) {
    fprintf(stderr, "tstrans ABI minor too old\n");
    return 1;
}
```

See [`examples/getting-started/version_check.c`](../../bindings/c/examples/getting-started/version_check.c) for the canonical startup pattern.

## Hello world

Build MPEG-TS in memory using a synthetic H.264 payload and placeholder
KLV bytes. This demonstrates packet construction; the output is not
playable video or valid ST 0601 metadata. Save the code below as `hello.c`.
The full example, including error checks, is at
[`hello_world.c`](../../bindings/c/examples/getting-started/hello_world.c).

```c
#include "tstrans.h"
#include <stdio.h>

int main(void) {
    tst_mux_config_t *cfg = tst_mux_config_new();
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    tst_mux_config_add_video_stream(cfg, prog, 0x100, TST_VIDEO_CODEC_H264);
    tst_mux_config_add_klv_stream(cfg, prog, 0x101, TST_KLV_STREAM_TYPE_PRIVATE_DATA, /*carries_pts=*/false);

    tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);

    static const uint8_t aud_nal[] = { 0x00, 0x00, 0x00, 0x01, 0x09, 0x10 };
    tst_muxer_push_video(mux, aud_nal, sizeof(aud_nal), /*pts_90khz=*/0, /*key_frame=*/true);
    static const uint8_t klv[33] = { 0x06,0x0E,0x2B,0x34,0x02,0x0B,0x01,0x01, 0x0E,0x01,0x03,0x01,0x01,0x00,0x00,0x00, 0x10 };
    tst_muxer_push_klv(mux, klv, sizeof(klv), /*pts_90khz=*/0);

    uint8_t pkt[188];
    size_t total = 0;
    while (1) { size_t n = tst_muxer_pull(mux, pkt, 188); if (n == 0) break; total += n; }
    tst_muxer_close(mux);

    printf("built %zu bytes of MPEG-TS\n", total);
    return 0;
}
```

Run it:

```sh
gcc -I target/release/include -L target/release -Wl,-rpath,target/release \
    -o /tmp/hello hello.c -ltstrans
/tmp/hello
# built 752 bytes of MPEG-TS
```

The Rust twin is [`hello_world.rs`](../../examples/getting-started/hello_world.rs): the same PIDs and the same four packets (PAT, PMT, one video PES, one KLV PES); its KLV record is an encoded ST 0601 set rather than a zero-filled one.

## First send

The C twin of [Quickstart](/docs/start/quickstart.md). Connect an SRT sender, push one access unit + one KLV record, close cleanly:

```c
#include "tstrans.h"
#include <stdio.h>

int main(int argc, char **argv) {
    const char *url = argc > 1 ? argv[1] : "srt://127.0.0.1:9000";

    /* 1. Build the multiplex topology. */
    tst_mux_config_t *cfg = tst_mux_config_new();
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    tst_mux_config_add_video_stream(cfg, prog, 0x100, TST_VIDEO_CODEC_H264);
    tst_mux_config_add_klv_stream(cfg, prog, 0x101,
                                  TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA,
                                  /*carries_pts=*/true);

    /* 2. Open an SRT-backed mux sender. The open copies the config; free it. */
    tst_mux_sender_t *snd = tst_mux_sender_open(url, cfg);
    tst_mux_config_free(cfg);
    if (!snd) {
        fprintf(stderr, "open failed: %s\n", tst_get_last_error_str());
        return 1;
    }

    /* 3. Push one Annex-B framed NAL + one ST 0601 KLV blob. */
    static const uint8_t nal[] = { 0,0,0,1, 0x09, 0x10 };
    if (tst_mux_sender_send_video(snd, nal, sizeof(nal), 0, true) != 0) {
        fprintf(stderr, "send_video: %s\n", tst_get_last_error_str());
    }

    static const uint8_t klv[] = {
        0x06,0x0E,0x2B,0x34,0x02,0x0B,0x01,0x01, 0x0E,0x01,0x03,0x01,0x01,0x00,0x00,0x00,
        0x10,  0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0,
    };
    if (tst_mux_sender_send_klv(snd, klv, sizeof(klv), 0) != 0) {
        fprintf(stderr, "send_klv: %s\n", tst_get_last_error_str());
    }

    /* 4. Close. Auto-flushes any buffered TS packets and the SRT socket. */
    tst_mux_sender_close(snd);
    return 0;
}
```

For KLV: **pass raw MISB Local Set bytes** — the muxer auto-wraps the H.222.0 § 2.12.4.2 AU cell header for `SYNCHRONOUS_METADATA` streams. Don't pre-wrap.

Multi-stream variants (`tst_mux_sender_send_video_to(handle, ...)`, `tst_mux_sender_send_klv_to(handle, ...)`) target a specific elementary stream when you have more than one video or KLV stream configured. See [`examples/muxing/mux_dual_camera.c`](../../bindings/c/examples/muxing/mux_dual_camera.c) for the EO + IR + KLV fan-out shape.

### DTS-aware video push (offline muxer)

For B-frame-reordered streams you need to write both a presentation timestamp
(PTS) and a decode timestamp (DTS) into each PES header. Use the targeted
`_with_dts` variants on the offline `tst_muxer_t` to pass both:

```c
// Annex-B NAL with explicit DTS (handle-targeted):
int tst_muxer_push_video_to_with_dts(struct tst_muxer_t *p,
                                     tst_video_stream_handle_t handle,
                                     const uint8_t *nal, size_t len,
                                     int64_t pts_90khz,
                                     int64_t dts_90khz,
                                     bool key_frame);

// On-wire (byte-faithful) video AU with explicit DTS (handle-targeted):
int tst_muxer_push_video_wire_to_with_dts(struct tst_muxer_t *p,
                                          tst_video_stream_handle_t handle,
                                          const uint8_t *wire, size_t len,
                                          int64_t pts_90khz,
                                          int64_t dts_90khz,
                                          bool key_frame);
```

Both functions emit `PTS_DTS_flags = '11'` (ISO/IEC 13818-1 §2.4.3.6) in the
PES header, writing DTS as a 33-bit field immediately after the PTS field.
`handle` is obtained from `tst_mux_config_add_video_stream` at config time —
there is no single-stream DTS shorthand; use the targeted
`tst_muxer_push_video_to_with_dts` form even on a single-stream muxer.

> **B-frame note.** Most real-time EO/IR payloads use I/P-frame-only coding
> (no B frames) and need only PTS. The DTS variants are for sources that
> require a decode ordering different from presentation ordering — typically
> H.264/H.265 Baseline/Main with B frames, or AV1 with film-grain synthesis.
> When PTS and DTS are equal, prefer the non-DTS variants for a smaller PES
> header (4 bytes shorter — no DTS field written).

Both functions are added in **ABI 17** (additive; no existing symbol or struct
changed).

### Private/application data streams

For opaque payloads the demuxer would surface as `TST_STREAM_KIND_UNKNOWN` (vendor telemetry, application sidecar data), declare a data stream and push raw bytes:

```c
tst_data_stream_handle_t ds = tst_mux_config_add_data_stream(
    cfg, prog, 0x102, /*stream_type=*/0xF0, /*carries_pts=*/true);
/* ... after tst_mux_sender_open: */
tst_mux_sender_send_data(snd, payload, payload_len, pts_90khz);
```

- **Pass-through semantics.** No AU-cell wrap, no framing, no payload inspection — the bytes land verbatim as the payload of exactly one PES packet (`stream_id` `0xBD`, `private_stream_1`) on the configured PID. Record boundaries within a payload are your convention. Payloads are capped by the `PES_packet_length` ceiling: 65532 bytes without PTS, 65527 with.
- **PTS contract.** `pts_90khz` is written into the PES header only when the stream was configured with `carries_pts = true`; it is **always** used for PSI/PCR pacing decisions regardless. With `carries_pts = false` the PES omits the PTS field entirely (this library's demuxer surfaces such samples with `pts == 0`).
- **`stream_type` is the raw PMT byte** (e.g. `0xF0`/`0xF1` user-private, bare `0x06`) — no enum. The `(stream_type, descriptors)` pair must still classify as Unknown under the demux cascade (you can't masquerade as a typed video/KLV/audio stream); that's validated at `_open` time. Per-PID PMT descriptors go through `tst_mux_config_add_data_descriptor` / `tst_mux_config_set_stream_descriptors_for_data`, same contract as the video/KLV descriptor setters.
- **`_to` routing.** With more than one data stream configured, `tst_mux_sender_send_data_to(snd, ds, ...)` targets a specific one, mirroring `tst_mux_sender_send_klv_to`.

The config entry points and the offline `tst_muxer_push_data` / `tst_muxer_push_data_to` pair are unconditional; the `tst_mux_sender_send_data` / `tst_mux_sender_send_data_to` pair lives behind `TST_HAS_SRT` like the rest of the SRT mux-sender surface (build with `--features srt`).

## First receive

Bind an SRT listener, walk typed demux events:

```c
#include "tstrans.h"
#include <inttypes.h>
#include <stdio.h>

int main(void) {
    tst_demux_receiver_t *rx = tst_demux_receiver_open_listener("srt://:7000");
    if (!rx) {
        fprintf(stderr, "open_listener: %s\n", tst_get_last_error_str());
        return 1;
    }

    tst_event_t ev = {0};
    for (;;) {
        int rc = tst_demux_receiver_recv_event(rx, &ev);
        if (rc == 0) {
            switch (ev.kind) {
                case TST_EVENT_KIND_PROGRAM_MAP:
                    printf("PMT program=%u streams=%zu\n",
                           ev.u.program_map.program_number,
                           ev.u.program_map.stream_count);
                    break;
                case TST_EVENT_KIND_SAMPLE:
                    printf("SAMPLE pid=0x%04x pts=%" PRId64 " codec=%d len=%zu\n",
                           ev.u.sample.pid, ev.u.sample.pts,
                           ev.u.sample.codec, ev.u.sample.payload_len);
                    break;
                case TST_EVENT_KIND_METADATA:
                    printf("KLV pid=0x%04x pts=%" PRId64 " len=%zu\n",
                           ev.u.metadata.pid, ev.u.metadata.pts,
                           ev.u.metadata.payload_len);
                    break;
                default:
                    break;
            }
            continue;
        }
        if (rc == TST_E_END_OF_STREAM) break;   /* peer disconnected cleanly */
        if (rc == TST_E_CLOSED) break;          /* cancel_handle fired */
        fprintf(stderr, "recv_event rc=%d: %s\n", rc, tst_get_last_error_str());
        break;
    }
    tst_demux_receiver_close(rx);
    return 0;
}
```

The receiver is the higher-level of three concentric shapes. Pick by what you actually need:

| Shape | C type | Returns |
|---|---|---|
| Raw socket bytes | `tst_raw_receiver_t` | One SRT message per `_recv` call |
| 188-byte aligned TS packets | `tst_receiver_t` | One TS packet per `_recv_packet` call |
| Typed demux events | `tst_demux_receiver_t` | One `tst_event_t` per `_recv_event` call |

Add the `tst_managed_*` prefix for any of the three to get automatic reconnect — see [Pipeline guide](/docs/guides/pipeline.md). Full receiver examples in [`examples/receiving/`](../../bindings/c/examples/receiving/).

## HLS publisher (`TST_HAS_HLS`)

The HLS publisher segments MPEG-TS to `.ts` files and serves them (plus a rolling `.m3u8`) over an optional built-in HTTP server. It is a supported feature, opt-in at build time: `tstrans.h` exposes the surface only when built with `--features hls`, guarded by `#ifdef TST_HAS_HLS`. The surface (`tst_hls_publisher_builder_*`, `tst_mux_publisher_*`) mirrors the Rust `tst-hls` crate.

The ABI-18 additions harden the terminal-playlist story:

- `tst_hls_publisher_finish_serving` returns an opaque `TstHlsServerHandle` (`tst_hls_server_handle_local_addr` / `_shutdown` / `_free`) that keeps the built-in server up so a completed VOD/EVENT playlist and its segments stay fetchable after the stream ends.
- `tst_hls_publisher_builder_max_segment_duration_ms` sets the wall-clock force-cut cap for an overdue keyframe (`0` leaves the `2 × segment_duration` default); `tst_hls_publisher_get_forced_cuts` reads how often it fired.

The ABI-19 additions carry MISB ST 0604 MISP timestamps through the C ABI:

- `tst_muxer_push_video_misp_to` / `tst_muxer_push_video_misp_to_with_dts` push an access unit and splice a MISP Precision (or Nano Precision) Time Stamp SEI immediately before its first VCL NAL.
- `tst_misp_time_extract` scans an Annex-B access unit and returns the first MISP timestamp found.
- Error codes `TST_E_MISP_TIME` (−45, SEI build/splice failure) and `TST_E_MISP_TIME_MALFORMED` (−46, present-but-malformed timestamp).

On the decode side the C ABI carries one typed KLV set: ST 0601, through `tst_st0601_decode`, the curated `tst_st0601_geometry` getter and the per-tag `tst_st0601_get_f64` / `_get_u64` / `_state` accessors (ABI 21). Every other typed set (including the ST 1204 Core ID codec) and all typed encode stay out of the C ABI — C carries raw KLV bytes via the `push_klv` families; see the [STANAG 4609 reference](/docs/reference/stanag-4609.md).

See the [HLS guide](/docs/guides/hls.md) for serving guidance, the KLV ride-along carriage modes, and latency tuning.

## RTSP publisher ingest (`TST_HAS_RTP`)

The RTSP server's publisher role (ABI 23) lets an encoder push into the server with ANNOUNCE / SETUP `mode=record` / RECORD, and hands each pushed stream to the application as MPEG-TS through an ordinary `TstRtpDemuxReceiver`. The same mount keeps serving PLAY readers from the published bytes. A publish mount accepts one publisher at a time, in either of two shapes: one MPEG-TS-over-RTP track (`TST_RTSP_PUBLISH_SHAPE_MP2T`, bytes pass through), or elementary H.264 with an optional KLV track (`TST_RTSP_PUBLISH_SHAPE_ELEMENTARY`, re-muxed by the server into one program, video PID 0x100, KLV PID 0x101).

Server entry points:

```c
void tst_rtsp_server_builder_accept_unregistered_publishers(struct TstRtspServerBuilder *builder, bool accept);
struct tst_rtsp_publish_mount_t *tst_rtsp_server_add_publish_mount(struct TstRtspServer *server, const char *path);
int tst_rtsp_server_next_publisher(struct TstRtspServer *server, uint64_t timeout_ms, struct tst_rtsp_publish_mount_t **out);
int tst_rtsp_server_remove_mount(struct TstRtspServer *server, const char *path);
int tst_rtsp_server_local_addr(const struct TstRtspServer *server, char *buf, size_t len);
int tst_rtsp_server_active_publishers(struct TstRtspServer *server, uint64_t *out);
int tst_rtsp_server_total_rtp_packets_received(struct TstRtspServer *server, uint64_t *out);
int tst_rtsp_server_total_rtp_bytes_received(struct TstRtspServer *server, uint64_t *out);
```

Publish-mount handle:

```c
const char *tst_rtsp_publish_mount_path(const struct tst_rtsp_publish_mount_t *mount);
int tst_rtsp_publish_mount_peer_count(const struct tst_rtsp_publish_mount_t *mount, uint64_t *out);
int tst_rtsp_publish_mount_generation(const struct tst_rtsp_publish_mount_t *mount, uint64_t *out);
int tst_rtsp_publish_mount_get_stats(const struct tst_rtsp_publish_mount_t *mount, struct tst_rtsp_publish_mount_stats_t *out);
int tst_rtsp_publish_mount_publisher_info(const struct tst_rtsp_publish_mount_t *mount, struct tst_rtsp_publisher_info_t *out);
int tst_rtsp_publish_mount_cancel(struct tst_rtsp_publish_mount_t *mount);
struct TstRtpDemuxReceiver *tst_rtsp_publish_mount_into_demux_receiver(struct tst_rtsp_publish_mount_t *mount, const struct tst_demux_config_t *demux_cfg);
void tst_rtsp_publish_mount_free(struct tst_rtsp_publish_mount_t *mount);
```

Two ways to get a mount:

- **Registered names.** `tst_rtsp_server_add_publish_mount` registers a path up front (`TST_E_RTSP_MOUNT` for a duplicate or invalid path). An ANNOUNCE to any other path answers `404`.
- **On demand.** With `tst_rtsp_server_builder_accept_unregistered_publishers(builder, true)`, an ANNOUNCE to an unregistered path creates a publish mount and queues its handle. `tst_rtsp_server_next_publisher` hands each one out, in ANNOUNCE order, to exactly one caller. Up to 64 handles wait in that queue and the server holds up to 256 on-demand mounts; an ANNOUNCE past either bound answers `503`. Anyone who can reach the port can create mounts, so add `tst_rtsp_server_builder_auth_basic` or `_auth_digest_*` where that matters.

Return codes worth branching on:

| Code | Where | Meaning |
|---|---|---|
| `TST_E_BUFFER_FULL` (−4) | `tst_rtsp_server_next_publisher` | No mount arrived before the timeout; `*out` is NULL. Retry. Always the result when on-demand publishers are off. |
| `TST_E_CLOSED` (−7) | every server entry point | The server is stopped. A `next_publisher` call parked on another thread wakes with it when `tst_rtsp_server_stop` runs. |
| `TST_E_CLOSED` (−7) | `_into_demux_receiver`, then `tst_rtp_demux_receiver_next_event` | A second take of the mount's transport; or, on the receiver, an explicit cancel (`tst_rtsp_publish_mount_cancel` or `tst_rtp_demux_receiver_cancel`). |
| `TST_E_END_OF_STREAM` (−12) | `tst_rtp_demux_receiver_next_event` | The mount was closed by `tst_rtsp_server_remove_mount` or `tst_rtsp_server_stop`; what was already queued is delivered first. |
| `TST_E_RTSP_MOUNT` (−25) | `_add_publish_mount`, `_remove_mount` | Duplicate or invalid path, or no mount registered at the path. |

Rules the types do not enforce:

- **Take-once transport.** `tst_rtsp_publish_mount_into_demux_receiver` takes the mount's transport once across every handle to that mount; later calls return NULL with `TST_E_CLOSED`. The receiver outlives publisher churn: between publishers it stays open and silent, and the next publisher's bytes arrive as ordinary continuity discontinuities.
- **Freeing a handle never closes the mount.** `tst_rtsp_publish_mount_free` releases the handle only. An on-demand mount stays registered after its publisher leaves until `tst_rtsp_server_remove_mount` removes it, which also sends a live publisher RTSP Notice 5402 and disconnects it.
- **Stop, not the hard cancel, ends the mounts.** `tst_rtsp_server_stop` closes every publish mount, so each bound receiver reads `TST_E_END_OF_STREAM`, and it wakes a parked `next_publisher`. The hard cancel (`tst_rtsp_cancel_handle_cancel`) does neither: receivers stay parked and `next_publisher` keeps timing out. Shut down with stop, join the threads reading the receivers, then close the receivers, free the mount handles, and free the server last. Never call `tst_rtsp_server_free` while another thread is inside one of the server's calls.
- **Expired handles.** `next_publisher` can return a handle whose mount was removed while it waited in the queue. Its receiver reads `TST_E_END_OF_STREAM` at once.
- **One `next_publisher` caller at a time.** Concurrent callers are served one at a time, so a call made while another waits can overrun its own timeout.
- **Counters ride getters.** `tst_server_stats_t` keeps its layout; the publisher counters are out-parameter getters, and per-mount numbers are in `tst_rtsp_publish_mount_stats_t` (`alignment` is one of the four `tst_rtsp_clock_alignment` values). The mount getters keep working after the mount is closed.
- **Port 0.** `tst_rtsp_server_local_addr` writes the bound address, NUL-terminated and truncated to fit (64 bytes holds any address), so a server built on port 0 can report the port the kernel picked.

ffmpeg pushes elementary tracks and cannot push KLV over RTSP; GStreamer's `rtspclientsink` can push MPEG-TS or H.264 + KLV. The C example [`recv_rtsp_publish.c`](/bindings/c/examples/receiving/recv_rtsp_publish.c) runs the whole loop, with push commands in its header; the [publisher ingest recipe](/docs/cookbook/receiving/rtsp-publish-ingest.md) covers the same flow from Rust.

## Language-specific gotchas

**`_close` lifecycle contract.** Every handle has a `tst_<thing>_close()` function. The contract:

- Calling `_close(NULL)` is a safe no-op.
- After a successful close the pointer is invalid; **calling close again on the same non-null pointer is undefined behavior.**
- Concurrent close-from-multiple-threads on the same live pointer is also UB. Bindings must coordinate close against data-path use.

Treat handles as moved-into the close call: set your local variable to `NULL` immediately after, or wrap close in an `if (handle) { ...close...; handle = NULL; }` guard.

**Configs are copied, not consumed, by `_open`.** `tst_mux_sender_open(url, cfg)`, `tst_muxer_open(cfg)`, and their managed variants copy what they need from the config; the caller still owns it and frees it with `tst_mux_config_free(cfg)`, right after the open or after reusing it for further opens.

**Error surface.** Errors are flat negative `TST_E_*` integers returned directly by the function. The most recent error is also written to a thread-local slot:

```c
int rc = tst_mux_sender_send_video(snd, nal, len, pts, true);
if (rc != 0) {
    fprintf(stderr, "rc=%d (%s)\n", rc, tst_get_last_error_str());
    // tst_get_last_error() also returns rc; tst_clear_last_error() zeros the slot.
}
```

The full code table is in `tstrans.h` (search `TST_E_`). Key transient-vs-persistent distinction:

- `TST_E_NOT_AVAILABLE` (-13) — **transient**. The next call may succeed (e.g., a managed transport is mid-reconnect).
- `TST_E_NOT_FOUND` (-14) — **persistent**. The next call with the same key will return the same error (e.g., asking for stream stats on a PID the demuxer never saw).
- `TST_E_INVALID_USAGE` (-9) — **programmer bug**. The handle is in a fundamentally wrong state (e.g., calling `_send_video` after `_close`).

See [Binding-authors guide](/docs/reference/binding-authors.md#transient-vs-persistent-error-codes) for the full mapping recipe.

**Panics are mapped, not propagated.** Every `extern "C"` entry point wraps the Rust call in a `ffi_catch` shim. A Rust panic surfaces as `TST_E_PANIC_CAUGHT` (-11) with the panic message in `tst_get_last_error_str()` — never as a `std::abort` or a stack-unwind into your C runtime. A panic inside a MUTATING call also drops the handle's shell, so every later call returns `TST_E_CLOSED`; a panic inside a read-only accessor (`_get_stats`, `_is_alive`) reports the code and leaves the handle usable.

**Stream handles are `uint32_t` with packed metadata.** `tst_mux_config_add_video_stream` returns a `tst_video_stream_handle_t` whose high bits encode program/stream indices. The library validates these at every push-time call; **don't fabricate them by hand** — bit-twiddled values are rejected with `TST_E_INVALID_USAGE`. `TST_INVALID_STREAM_HANDLE` (`UINT32_MAX`) is the failure sentinel returned from the add-stream calls.

**No `#[non_exhaustive]` on C enums.** Enums are stable `int32_t` constants; new variants get assigned the next integer. Write switches with a safe `default:` arm for forward-compat:

```c
switch (ev.kind) {
    case TST_EVENT_KIND_PROGRAM_MAP:  /* ... */ break;
    case TST_EVENT_KIND_SAMPLE:       /* ... */ break;
    /* ... */
    default: /* future variant — log and skip */ break;
}
```

**Threading.** Pipeline shells (`tst_mux_sender_t`, `tst_demux_receiver_t`, etc.) are internally synchronized — the data-path methods (`_send_*`, `_recv_*`, `_pull`) are callable from multiple threads concurrently. **Configs (`tst_mux_config_t`, `tst_sender_config_t`, etc.) are NOT.** Build a config on one thread, hand it to `_open`, then never touch it again.

**Cancellation.** Every SRT and RTP shell has a `_cancel` (`tst_mux_sender_cancel`, `tst_demux_receiver_cancel`, …): lock-free, callable from any thread, idempotent. It closes the underlying socket (SRT) or flags the transport (RTP) so a thread parked in `_send` / `_recv` returns promptly. What that call returns: `TST_E_CLOSED` (-7) on every shell, managed or plain, for the call that observes the cancel and every call after it. libsrt reports the closed socket as a broken connection, but `SrtTransport` reads its own cancel latch afterwards and reports the cancel the caller asked for (0.7.0; through 0.6.x a plain SRT shell reported `TST_E_TRANSPORT` (-8) for the parked call and `TST_E_CLOSED` only for the ones after it). `_close` is cancel-first on every shell (SRT, RTP, UDP, TCP, RIST): it fires the same cancel, then frees. Since 0.7.0 every transport's cancel is a REAL handle, UDP and RIST included — calling `tst_udp_receiver_close` / `tst_udp_demux_receiver_close` (or the sender twins) from another thread ends a data-path call parked on the UDP 100 ms poll with `TST_E_CLOSED` within one tick, where before it waited for the next datagram. **RIST is the one family where a cross-thread `_close` is still not the answer:** a `tst_rist_*_recv_ts` / `_next_event` call does not park — it is a single ~100 ms librist poll that returns `TST_E_BUFFER_FULL` when nothing arrived, so callers poll in a loop, and because `_close` frees the handle, closing it from another thread while that loop runs is a use-after-free like any other post-free use. Close a RIST handle from the thread that polls it. The `tst_tcp_*_cancel` / `tst_udp_*_cancel` / `tst_rist_*_cancel` entry points — the non-freeing cross-thread cancel, and the only safe way to interrupt a RIST receive from another thread — are new symbols that ship with ABI 0.22. See [SRT cancel handle](/docs/reference/srt-cancel-handle.md) for the Rust-layer pattern.

**Process exit with a call still parked.** Cancel, join and `_close` before
the process exits — that is still the shape to write. If a thread is
nevertheless inside an SRT call when `exit()` runs (or `main` returns), the
library gets it out: its exit handler refuses every SRT call that has not
started yet, closes every SRT socket that is still open, waits for the calls
in flight to return — the whole call, not only the part that blocks — and
only then runs libsrt's own cleanup. Before 0.7.0 a thread left in an accept
made that cleanup wait forever and the process never terminated.

Two limits. The wait is bounded at 2 s; if a call is still in flight after
that, libsrt's cleanup runs anyway and that thread may crash or hang the
exiting process, which is what every such program risked before the guard
existed. And the 2 s does not bound exit as a whole: closing a connected
sender that still has unsent data takes up to that socket's linger time
(`?linger=` on the URL; 5 s by default on the sender opens), and the handler
closes the open sockets one after the other before the wait starts. A call
made after the handler has started — from a thread that is still running
while the process exits — returns `TST_E_CLOSED` without entering libsrt.

This matters most
for the listener-mode opens (`tst_demux_receiver_open_listener`,
`tst_managed_demux_receiver_open_listener`, …): they block in their first
accept BEFORE returning a handle, so there is nothing to `_cancel` while they
wait for a peer. The unparked call reports `TST_E_CLOSED` (a listener-mode
open returns `NULL` with that code), but the process is already on its way
out — treat it as a way to leave, not as an event to handle. A program that
closed everything pays nothing at exit.

## Where this binding differs from the Rust core

The C surface is `tst_pipeline` + `tst_srt` mechanically projected through `cbindgen`, with these structural deviations:

- **Opaque pointers, not typed references.** Rust uses `&mut MuxSender<SrtTransport>`; C uses `tst_mux_sender_t *` — a pointer to an opaque struct. You can't reach inside the struct or compose handles structurally.
- **Stable integer enums.** Rust's `#[non_exhaustive]` enums become flat `int32_t`-backed C enums; new variants land at the next integer. The Rust-side wildcard arm requirement is invisible at the C ABI.
- **No iterator types.** Rust's `Iterator<Item = DemuxEvent>` becomes a poll-style `tst_demux_receiver_recv_event(rx, &out_event)` — call in a loop, terminate on `TST_E_END_OF_STREAM`.
- **No generics.** Rust's `MuxSender<T: Transport>` collapses to one concrete `tst_mux_sender_t` (SRT-backed). No `RecvTransport` mock at the C ABI — wire-up tests use real SRT loopback.
- **Panic mapping.** Rust panics become `TST_E_PANIC_CAUGHT` rather than unwinding into your C runtime.
- **Explicit lifecycle.** Every handle needs an explicit `_close` / `_free` call — no `Drop` semantics. NULL-safe on the way in, UB on double-close of a non-null pointer.
- **Stream handles are validated `uint32_t`s.** Rust's `VideoStreamHandle` is a newtype enforcing program/stream indices at the type level; the C ABI smuggles the same metadata through the high bits of a `uint32_t` and validates at every push call.

If you're wrapping `tst-c` to build a higher-language binding (Java, Go, C#, Erlang NIF), start with [Binding-authors guide](/docs/reference/binding-authors.md) — it has per-language idiomatic-shape patterns and the full error-mapping contract.
