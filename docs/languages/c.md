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
| `libtstrans.a` (`tstrans.lib` on MSVC) | same | Static library — libsrt + mbedTLS + libstdc++ statically embedded |
| `tstrans.h` | `bindings/c/include/` | Single-file C header (~350 KB), `cbindgen`-generated |
| `tstrans.pc` | `target/<profile>/` | pkg-config file (substituted by `build.rs` from `tstrans.pc.in`) |

### From source

```sh
git clone https://github.com/aklofas/ts-transformer
cd ts-transformer
SRT_FORCE_VENDORED=1 cargo build -p tst-c --release
# Artifacts land in target/release/ + bindings/c/include/tstrans.h
```

The build vendors libsrt 1.5.7 + mbedTLS 3.6.x statically — your `libtstrans.so` has no external dependencies beyond libc, libpthread, libstdc++, libdl, and libm. Verify with `ldd target/release/libtstrans.so`.

### Compile + link

Direct gcc/clang:

```sh
gcc -I bindings/c/include \
    -L target/release \
    -Wl,-rpath,target/release \
    -Wall -Werror -o myapp \
    myapp.c -ltstrans
```

With pkg-config (recommended for build systems):

```sh
export PKG_CONFIG_PATH=$PWD/target/release:$PKG_CONFIG_PATH
gcc -Wall -Werror -o myapp myapp.c $(pkg-config --cflags --libs tstrans)
```

### ABI versioning

Every release exposes two pairs of macros in `tstrans.h` plus matching runtime accessors:

```c
#define TST_VERSION_MAJOR        0   // package (Cargo.toml) version
#define TST_VERSION_MINOR        6
#define TST_VERSION_PATCH        0
#define TST_ABI_VERSION_MAJOR    0   // C ABI contract version
#define TST_ABI_VERSION_MINOR    21
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

See [`examples/getting-started/version_check.c`](../../bindings/c/examples/getting-started/version_check.c) for the canonical startup pattern (matches what `tst-jni` and `tst-uniffi` will do in `JNI_OnLoad` / the UniFFI init hook).

## Hello world

Build one MPEG-TS frame in memory containing one H.264 access unit + one KLV record — no SRT, no files. The full example is at [`examples/getting-started/hello_world.c`](../../bindings/c/examples/getting-started/hello_world.c); the core is ten lines:

```c
#include "tstrans.h"
#include <stdio.h>

int main(void) {
    tst_mux_config_t *cfg = tst_mux_config_new();
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    tst_mux_config_add_video_stream(cfg, prog, 0x100, TST_VIDEO_CODEC_H264);

    tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);

    static const uint8_t aud_nal[] = { 0x00, 0x00, 0x00, 0x01, 0x09, 0x10 };
    tst_muxer_push_video(mux, aud_nal, sizeof(aud_nal), /*pts_90khz=*/0, /*key_frame=*/true);

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
gcc -I bindings/c/include -L target/release -Wl,-rpath,target/release \
    -o /tmp/hello hello.c -ltstrans
/tmp/hello
# built 752 bytes of MPEG-TS
```

The output is byte-identical to the Rust [`hello_world.rs`](../../examples/getting-started/hello_world.rs) example.

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

    /* 2. Open an SRT-backed mux sender. The config is consumed; free it. */
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

Typed KLV set encode/decode (including the ST 1204 Core ID codec) intentionally stays out of the C ABI — C carries raw KLV bytes via the `push_klv` families; see the [STANAG 4609 reference](/docs/reference/stanag-4609.md).

See the [HLS guide](/docs/guides/hls.md) for serving guidance, the KLV ride-along carriage modes, and latency tuning.

## Language-specific gotchas

**`_close` lifecycle contract.** Every handle has a `tst_<thing>_close()` function. The contract:

- Calling `_close(NULL)` is a safe no-op.
- After a successful close the pointer is invalid; **calling close again on the same non-null pointer is undefined behavior.**
- Concurrent close-from-multiple-threads on the same live pointer is also UB. Bindings must coordinate close against data-path use.

Treat handles as moved-into the close call: set your local variable to `NULL` immediately after, or wrap close in an `if (handle) { ...close...; handle = NULL; }` guard.

**Configs are consumed by `_open`.** `tst_mux_sender_open(url, cfg)`, `tst_muxer_open(cfg)`, and their managed variants consume the config internally. You should still call `tst_mux_config_free(cfg)` afterward (it's a free-the-shell call, not a free-the-data call — internally idempotent against the consumed contents).

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

**Cancellation.** Every SRT and RTP shell has a `_cancel` (`tst_mux_sender_cancel`, `tst_demux_receiver_cancel`, …): lock-free, callable from any thread, idempotent. It closes the underlying socket (SRT) or flags the transport (RTP) so a thread parked in `_send` / `_recv` returns promptly. What that call returns: managed SRT shells and every RTP shell report `TST_E_CLOSED` (-7); a **plain** SRT shell reports `TST_E_TRANSPORT` (-8) for the call that was parked when the cancel landed (libsrt reports the closed socket as broken) and `TST_E_CLOSED` for every call after it — 0.7.0 makes the parked call report `TST_E_CLOSED` too. `_close` is cancel-first on every shell (SRT, RTP, UDP, TCP, RIST): it fires the same cancel, then frees. On TCP that cancel is the transport's real handle, so a parked call is woken; on **UDP and RIST** it is only a flag latch — it records the intent (so the call reports `TST_E_CLOSED` rather than end-of-stream) but wakes nothing, because those transports expose no cancel handle yet. UDP/TCP/RIST gain their own `_cancel` entry points, and UDP/RIST a real wake-up, with ABI 0.22. See [SRT cancel handle](/docs/reference/srt-cancel-handle.md) for the Rust-layer pattern.

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
