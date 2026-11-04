/*
 * mux_two_programs.c — Two-program MPEG-TS mux via the tst-c C ABI.
 *
 * ── USE CASE ─────────────────────────────────────────────────────────────────
 *
 * Multi-program MPEG-TS (MP-TS) lets an aggregator ground station combine
 * streams from two independent platforms into a single TS multiplex. Each
 * "program" is a logically independent bundle of elementary streams (video,
 * KLV, audio) that carries its own PCR clock and its own PMT describing the
 * bundle. A receiver picks the program of interest with `-program N` in ffmpeg
 * or with a program filter in TSDuck.
 *
 * WHY multi-program instead of multiple UDP ports?
 *   - One SRT socket, one connection, one flow-controlled pipe. The aggregator
 *     doesn't need to manage N parallel sockets and their reconnect/backoff
 *     policies independently.
 *   - Multi-program is also the repacking model: if your input comes from two
 *     single-program sources (e.g., two separate SRT feeds), you demux each
 *     source with a Demuxer instance, then route the ES payloads into programs
 *     1 and 2 of a single Muxer. The Rust analogue of that workflow is at
 *     examples/muxing/repack_two_programs.rs — this C example
 *     demonstrates the STRUCTURAL API only (stub NALs/KLV).
 *
 * ── PID UNIQUENESS RULE ───────────────────────────────────────────────────────
 *
 * MPEG-TS PIDs must be unique across the entire multiplex — not just within
 * a program. The muxer rejects config with colliding PIDs at open time with
 * TST_E_INVALID_CONFIG. When repacking two sources into one multiplex, the
 * caller is responsible for renumbering the second source's PIDs if they
 * collide with the first source's. This example avoids collisions by choosing
 * a clearly distinct PID range for each program (0x1000-range vs 0x1100-range).
 *
 * ── BUILD ─────────────────────────────────────────────────────────────────────
 *
 *   From the ts-transformer workspace root:
 *
 *   This example writes a file and needs no transport feature, so the
 *   packaged copy is built with the default feature set:
 *
 *   bash scripts/dev/c-package.sh --out /tmp/tstrans-pkg
 *   gcc bindings/c/examples/muxing/mux_two_programs.c \
 *       -I /tmp/tstrans-pkg/include \
 *       -L /tmp/tstrans-pkg/lib -Wl,-rpath,/tmp/tstrans-pkg/lib \
 *       -ltstrans \
 *       -o /tmp/mux_two_programs
 *
 * ── RUN + VERIFY ─────────────────────────────────────────────────────────────
 *
 *   /tmp/mux_two_programs /tmp/two-program.ts
 *
 *   ffprobe -show_programs /tmp/two-program.ts 2>/dev/null | grep program_num
 *   # Expected output:
 *   #   program_num=1
 *   #   program_num=2
 *
 *   tsp -I file /tmp/two-program.ts -P analyze
 *   # TSDuck: should report PAT with 2 program entries and 2 distinct PMTs.
 *
 * ── MIRRORS ──────────────────────────────────────────────────────────────────
 *
 *   examples/muxing/repack_two_programs.rs — full Rust example
 *   feeding real ES data from demuxed sources.
 *   bindings/c/examples/muxing/mux_dual_camera.c — single-program multi-stream
 *   example (EO + IR + KLV in one program).
 */

#include "tstrans.h"
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/*
 * Stub Annex-B IDR NAL: 4-byte start code + nal_unit_type=0x65 (IDR slice) +
 * minimal payload.
 *
 * WHY a stub NAL rather than a real encoded frame?
 *   ffprobe and downstream demuxers identify the video codec from the PMT's
 *   stream_type byte (0x1B for H.264, 0x24 for H.265) — NOT from parsing the
 *   NAL contents. A tiny stub is sufficient to make the muxer emit structurally
 *   correct PAT + PMT + PCR + PES packets, which is what this example
 *   demonstrates. For a real ES round-trip, see repack_two_programs.rs.
 *
 *   NAL type 0x65 = IDR slice. We use it here because the muxer needs at
 *   least one IDR to begin a PES sequence. The payload bytes (0x88 0x82 ...) are
 *   arbitrary fillers that don't affect the structural TS output.
 */
static const uint8_t NAL_IDR[] = {
    0x00, 0x00, 0x00, 0x01,   /* Annex-B start code */
    0x65,                      /* nal_unit_type = IDR slice */
    0x88, 0x82, 0x00, 0x00,   /* filler payload */
};

/*
 * Minimal 17-byte ST 0601 KLV blob.
 *
 * WHY exactly 17 bytes?
 *   A conformant ST 0601 packet requires:
 *     - 16 bytes: UAS Datalink Local Set Universal Label (the SMPTE/MISB UL)
 *     -  1 byte:  BER short-form length = 0 (zero-length value body)
 *   Total: 17 bytes — the smallest legal ST 0601 KLV envelope.
 *   "Zero-length value" means no tags inside the Local Set; this is spec-
 *   conformant and is enough for the muxer to produce a well-formed KLV PES.
 *
 * WHY PrivateData KLV (stream_type 0x06) rather than SynchronousMetadata (0x15)?
 *   PrivateData KLV is the async shape: the PES carries no PTS, so no AU cell
 *   wrapping is required. A receiver treats it as best-effort side-channel KLV
 *   aligned by arrival rather than timestamp. This is the simpler shape — and
 *   the correct choice for any workflow that doesn't need sub-frame KLV timing.
 *
 *   If you need synchronous (PTS-stamped) KLV, use
 *   TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA — the muxer auto-wraps each
 *   push in a 5-byte Metadata_AU_cell header per ITU-T H.222.0 V9 § 2.12.4.2
 *   before TS-framing. Pass raw KLV LS bytes to tst_muxer_push_klv_to;
 *   PTS lives in the PES header (per § 2.12.4.1).
 */
static const uint8_t KLV_BLOB[] = {
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,  /* UL bytes 1-8  */
    0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,  /* UL bytes 9-16 */
    0x00,                                              /* BER length = 0 */
};

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <output.ts>\n", argv[0]);
        return 1;
    }
    const char *output_path = argv[1];

    /* ── Step 1: Build the mux config ─────────────────────────────────────
     *
     * tst_mux_config_t is an opaque heap-allocated builder. Populate it
     * via the tst_mux_config_add_program + tst_mux_config_add_*_stream
     * entry points, then hand it to tst_muxer_open. The muxer clones what
     * it needs at open time, so you own the config pointer and must free it
     * yourself — but freeing immediately after open is the correct pattern
     * (signals "config is consumed, not aliased").
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        /* tst_mux_config_new fails only on OOM — no last-error to print. */
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        return 2;
    }

    /* ── Program 1 ─────────────────────────────────────────────────────────
     *
     * tst_mux_config_add_program must be called BEFORE adding streams.
     * The returned tst_program_handle_t is a zero-based ordinal used as the
     * `program` argument to all subsequent stream-add and descriptor-set calls.
     *
     * WHY program_number=1 and pmt_pid=0x1000?
     *   MPEG-TS PAT reserves program_number=0 for the NIT pointer; programs
     *   carry numbers 1..=65535. PMT PID 0x1000 is the common convention for
     *   the first program in a single-program TS; we reuse it here. PMT PIDs
     *   must also be unique across all programs — do not use the same PMT PID
     *   for program 2.
     *
     * TST_INVALID_PROGRAM_HANDLE (UINT32_MAX) signals failure; check
     * tst_get_last_error_str() for the reason.
     */
    tst_program_handle_t p1 = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (p1 == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program p1 failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 3;
    }

    /*
     * Add one H.264 video stream and one async KLV stream to program 1.
     *
     * WHY 0x1011 for video and 0x1031 for KLV?
     *   PIDs are arbitrary in the range 0x0010–0x1FFE (outside the reserved
     *   range 0x0000–0x000F and the null-packet PID 0x1FFF). We spread our
     *   PIDs by 0x20 hex to make them visually distinct in Wireshark / TSDuck
     *   captures. 0x1031 is far enough from 0x1011 that an accidental swap
     *   is obvious during debugging.
     *
     * The returned tst_video_stream_handle_t is a packed uint32_t encoding
     * (program_index, within_program_stream_index). You MUST use handle-
     * targeted push calls (tst_muxer_push_video_to / tst_muxer_push_klv_to)
     * on any muxer with more than one stream of the same kind; the bare
     * tst_muxer_push_video / tst_muxer_push_klv calls return
     * TST_E_INVALID_USAGE when the count is ambiguous (≠1).
     *
     * TST_INVALID_STREAM_HANDLE (UINT32_MAX) on any failure — rare at config
     * time; the cap is 16 video + 16 KLV per program.
     */
    tst_video_stream_handle_t v1 =
        tst_mux_config_add_video_stream(cfg, p1, 0x1011, TST_VIDEO_CODEC_H264);

    tst_klv_stream_handle_t k1 =
        tst_mux_config_add_klv_stream(cfg, p1, 0x1031,
                                       TST_KLV_STREAM_TYPE_PRIVATE_DATA,
                                       /*carries_pts=*/false);

    if (v1 == TST_INVALID_STREAM_HANDLE || k1 == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "stream add failed for p1: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 4;
    }

    /* ── Program 2 ─────────────────────────────────────────────────────────
     *
     * A second independent program. We pick H.265 for the video codec here
     * to demonstrate that programs are codec-independent — the muxer emits
     * the correct stream_type byte (0x24 for H.265, 0x1B for H.264) in each
     * program's PMT without any additional configuration.
     *
     * WHY pmt_pid=0x1100?
     *   PMT PIDs must be unique across all programs. We choose 0x1100 — it's
     *   in the valid range (0x0010–0x1FFE), clearly distinct from 0x1000, and
     *   doesn't collide with any elementary stream PID we're assigning.
     *
     * WHY 0x1111 and 0x1131?
     *   These are in the 0x1100-range, visually pairing with program 2's PMT
     *   at 0x1100. The muxer validates that these PIDs don't collide with any
     *   PID from program 1 (0x1011, 0x1031, or 0x1000). If you accidentally
     *   used 0x1011 here, tst_muxer_open would fail with TST_E_INVALID_CONFIG
     *   and the error string would name the duplicate PID.
     */
    tst_program_handle_t p2 = tst_mux_config_add_program(cfg, 2, 0x1100);
    if (p2 == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program p2 failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 5;
    }

    tst_video_stream_handle_t v2 =
        tst_mux_config_add_video_stream(cfg, p2, 0x1111, TST_VIDEO_CODEC_H265);

    tst_klv_stream_handle_t k2 =
        tst_mux_config_add_klv_stream(cfg, p2, 0x1131,
                                       TST_KLV_STREAM_TYPE_PRIVATE_DATA,
                                       /*carries_pts=*/false);

    if (v2 == TST_INVALID_STREAM_HANDLE || k2 == TST_INVALID_STREAM_HANDLE) {
        fprintf(stderr, "stream add failed for p2: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 6;
    }

    /* ── Step 2: Open the muxer ────────────────────────────────────────────
     *
     * tst_muxer_open validates the full config (PID uniqueness, program
     * numbers, etc.) and then builds the internal muxer state. It clones
     * what it needs from cfg — free cfg right after open to signal that
     * the config is consumed and not aliased.
     *
     * On failure (e.g., colliding PIDs, empty program), returns NULL and
     * sets the thread-local last-error. Check tst_get_last_error_str().
     */
    tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    cfg = NULL;  /* prevent accidental use after free */

    if (!mux) {
        fprintf(stderr, "tst_muxer_open failed: %s\n", tst_get_last_error_str());
        return 7;
    }

    /* ── Step 3: Open output file ─────────────────────────────────────────
     */
    FILE *out = fopen(output_path, "wb");
    if (!out) {
        perror("fopen");
        tst_muxer_close(mux);
        return 8;
    }

    /* ── Step 4: Push 30 ticks to both programs and drain to file ─────────
     *
     * WHY 30 iterations?
     *   The default PSI interval is 100ms (PAT + PMT emitted every 100ms of
     *   PTS time). At ~33ms per tick (3003 PTS units at 90kHz ≈ 33.37ms),
     *   30 ticks covers ~1 second — well past the PSI interval, so at least
     *   one full PAT and both PMTs are guaranteed to appear in the output.
     *
     * WHY pts = 90000 + tick * 3003?
     *   MPEG-TS uses a 90kHz clock (90,000 ticks per second). The starting
     *   value 90000 is an arbitrary initial PTS offset — using 0 works too
     *   but offset-90000 is a common convention that avoids exact-zero edge
     *   cases in some demuxers. 3003 ticks per frame ≈ 29.97fps (NTSC
     *   drop-frame cadence). Common alternatives: 3600 for 25fps, 3000 for
     *   exactly 30fps.
     *
     * WHY drain inside the push loop rather than one big pull at the end?
     *   tst_muxer_pull has an internal ring buffer (default 10,000 TS
     *   packets ≈ 1.88 MB). A short 30-frame run won't overflow it, so a
     *   single post-loop pull would also work here. The in-loop drain pattern
     *   is shown because it's the correct idiom for long-running or high-
     *   bitrate workflows where the ring could fill. tst_muxer_pull returns
     *   0 when no complete packet batch is ready — that's normal, not an
     *   error, so the inner while-loop exits immediately on most iterations.
     *
     * WHY push to both programs on every tick?
     *   In a real aggregator both platforms send at their own cadence. Here
     *   we keep them in lockstep for simplicity. The PCR for each program
     *   is driven independently — program 1's PCR counter doesn't affect
     *   program 2's. You can push to each program at different rates; the
     *   muxer interleaves them in PTS order.
     */
    uint8_t drain_buf[188 * 64];

    for (int tick = 0; tick < 30; tick++) {
        int64_t pts = 90000LL + (int64_t)tick * 3003;

        /* Push video and KLV to program 1. key_frame=true on every push for
         * this stub — in production, set true only on IDR frames. */
        if (tst_muxer_push_video_to(mux, v1, NAL_IDR, sizeof(NAL_IDR),
                                     pts, /*key_frame=*/true) != 0) {
            fprintf(stderr, "push v1 at tick %d: %s\n", tick, tst_get_last_error_str());
            goto fail;
        }
        if (tst_muxer_push_klv_to(mux, k1, KLV_BLOB, sizeof(KLV_BLOB), pts) != 0) {
            fprintf(stderr, "push k1 at tick %d: %s\n", tick, tst_get_last_error_str());
            goto fail;
        }

        /* Push video and KLV to program 2 (same PTS — they happen to be
         * frame-synchronised in this synthetic example). */
        if (tst_muxer_push_video_to(mux, v2, NAL_IDR, sizeof(NAL_IDR),
                                     pts, /*key_frame=*/true) != 0) {
            fprintf(stderr, "push v2 at tick %d: %s\n", tick, tst_get_last_error_str());
            goto fail;
        }
        if (tst_muxer_push_klv_to(mux, k2, KLV_BLOB, sizeof(KLV_BLOB), pts) != 0) {
            fprintf(stderr, "push k2 at tick %d: %s\n", tick, tst_get_last_error_str());
            goto fail;
        }

        /* Drain all ready TS packets to the output file.
         * tst_muxer_pull returns the number of bytes written into the buffer
         * (always a multiple of 188), or 0 if nothing is ready yet. */
        size_t n;
        while ((n = tst_muxer_pull(mux, drain_buf, sizeof(drain_buf))) > 0) {
            /* Structural sanity: packet count must be exact, first byte 0x47. */
            if ((n % 188) != 0 || drain_buf[0] != 0x47) {
                fprintf(stderr, "alignment check failed: n=%zu first=0x%02x\n",
                        n, (unsigned)drain_buf[0]);
                goto fail;
            }
            fwrite(drain_buf, 1, n, out);
        }
    }

    fclose(out);
    tst_muxer_close(mux);

    printf("Wrote %s\n", output_path);
    printf("Verify: ffprobe -show_programs %s 2>/dev/null | grep program_num\n",
           output_path);
    printf("        # Expected: program_num=1  program_num=2\n");
    return 0;

fail:
    /*
     * WHY close muxer after file?
     *   cfg was freed right after tst_muxer_open returned. The only live
     *   resource here is the muxer. Close the file first (partial output is
     *   fine to leave on disk for post-mortem inspection), then close the
     *   muxer so the Rust side releases its ring buffer and internal state.
     */
    fclose(out);
    tst_muxer_close(mux);
    return 9;
}
