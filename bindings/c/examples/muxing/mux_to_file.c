/*
 * mux_to_file.c — mux a synthetic H.264 + async-KLV stream to a `.ts` file
 * with NO transport: the standalone `tst_muxer_t` handle, drained by hand.
 *
 * This is the simplest possible muxer pipeline in C — useful for verifying
 * the muxer shape (config → open → push → pull → write) without bringing up
 * SRT or any other transport, and for producing a `.ts` you can inspect
 * with `ffprobe`, `tsp` (TSDuck), or the receiver examples in
 * `../receiving/`.
 *
 * The C twin of the Rust `examples/muxing/mux_to_file.rs`. It reproduces the
 * Rust example's config, frame cadence, synthetic AU sizes and KLV bytes
 * EXACTLY, so the two programs produce byte-identical output for the same
 * duration. That equivalence is the point: it is proof that the C ABI's
 * config builder + muxer handle drive the same muxer with the same defaults
 * as the Rust API. Verified 2026-09-06 with `cmp` (5 s = 150 frames =
 * 196,836 bytes = 1,047 TS packets from both).
 *
 * Handle lifecycle shown here (the same three-step shape every muxing
 * example in this directory uses):
 *   1. tst_mux_config_t — heap config builder; freed by us right after
 *      open (the muxer copies what it needs).
 *   2. tst_muxer_t      — the muxer; owns no socket, so `pull` is the only
 *      way bytes leave it.
 *   3. tst_muxer_close  — frees the muxer. Bytes not pulled before close
 *      are discarded, which is why the drain loop runs after EVERY push.
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c                     # offline surface: no transport feature needed
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/mux_to_file bindings/c/examples/muxing/mux_to_file.c -ltstrans
 *
 * Run:
 *   LD_LIBRARY_PATH=target/debug /tmp/mux_to_file /tmp/out.ts 5
 *
 * Verify:
 *   ffprobe /tmp/out.ts            # 1 video stream (h264) + 1 data stream (KLV)
 *   tsp -I file /tmp/out.ts -P analyze -O drop
 *   cargo run -p tst-examples --example mux_to_file -- /tmp/rust.ts 5 && cmp /tmp/out.ts /tmp/rust.ts
 *
 * See `mux_h265_with_klv.c` for the H.265 + synchronous-KLV variant of this
 * same shape, and `mux_synthetic_srt.c` for the same data flowing over SRT
 * through a `tst_mux_sender_t` instead of a file.
 */

#include "tstrans.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * These are the PIDs `MuxerConfig::default()` picks on the Rust side. The C
 * config builder has no "default program" — every stream is added
 * explicitly — so we spell the same values out here. Any PIDs in
 * 0x0010..=0x1FFE that don't collide would work; these particular ones are
 * what makes the output byte-identical to the Rust twin.
 *
 * WHY 0x1000 / 0x1011 / 0x1031?
 *   0x1000 for the PMT is a widely used single-program convention. Video at
 *   0x1011 and KLV at 0x1031 are spaced 0x20 apart so they read as distinct
 *   in a TSDuck / Wireshark PID column, and both sit inside the 0x1000 block
 *   most ground-station tooling expects elementary streams in.
 */
#define PROGRAM_NUMBER  1
#define PMT_PID         0x1000
#define VIDEO_PID       0x1011
#define KLV_PID         0x1031

/*
 * 30 fps live-video shape. PTS runs on the 90 kHz MPEG-TS clock, so
 * 90000 / 30 = 3000 ticks per frame; `pts = i * PTS_PER_FRAME` walks the
 * clock cleanly at 30 fps cadence. A real encoder passes through whatever
 * PTS its upstream produced.
 */
#define FRAME_RATE      30
#define PTS_PER_FRAME   (90000 / FRAME_RATE)

/*
 * One IDR every 2 seconds (every 60th frame at 30 fps) — a common GOP
 * cadence for live encoders. The key flag drives the TS adaptation field's
 * random_access_indicator bit, which receivers use to identify seek points.
 */
#define GOP_FRAMES      (FRAME_RATE * 2)

/*
 * 1316 = 7 × 188 — the default SRT payload (`SRTO_PAYLOADSIZE`) and thus the
 * natural "one datagram" unit for MPEG-TS-over-SRT. The muxer itself doesn't
 * care about this size; it only bounds how many whole TS packets one `pull`
 * returns. Sized this way so the drain loop below is exactly what an
 * SRT-bound caller would write.
 */
#define PULL_BUF_BYTES  1316

/* Synthetic KLV record length. Small, and well under the 128-byte BER
 * short-form ceiling the Rust twin's helper also stays inside. */
#define KLV_BYTES       50

/* ── Synthetic payload builders ────────────────────────────────────────── */

/*
 * make_h264_au — fill `buf` with a synthetic Annex-B H.264 access unit of
 * `len` bytes and return `len`.
 *
 * `tst_muxer_push_video` expects an Annex-B byte stream (start code +
 * NAL units), NOT length-prefixed AVCC. The muxer is opaque to NAL
 * contents — it packs whatever bytes you give it into PES packets — so a
 * start code, a plausible NAL header byte, and filler is all the shape a
 * downstream tool needs to see.
 *
 * NAL header byte layout (H.264 §7.3.1):
 *   forbidden_zero_bit(1) | nal_ref_idc(2) | nal_unit_type(5)
 * Key frames use nal_unit_type 5 (IDR slice) with nal_ref_idc 3 (0b11,
 * highest priority); other frames use type 1 (non-IDR slice) with
 * nal_ref_idc 2 (0b10, a reference picture). Encoded:
 *   key    → (0b11 << 5) | 5 = 0x65
 *   non-key→ (0b10 << 5) | 1 = 0x41
 *
 * `len` varies per frame (800..1000 bytes, see main) to mimic real
 * bitstream variability so downstream tooling sees a believable shape.
 */
static size_t make_h264_au(uint8_t *buf, size_t len, bool key) {
    const uint8_t nal_type = key ? 5 : 1;
    const uint8_t nri      = key ? 0x3 : 0x2;
    buf[0] = 0x00; buf[1] = 0x00; buf[2] = 0x00; buf[3] = 0x01;  /* Annex-B start code */
    buf[4] = (uint8_t)((nri << 5) | nal_type);
    memset(buf + 5, 0xA5, len - 5);                              /* filler "slice data" */
    return len;
}

/*
 * make_klv — fill `buf` with KLV_BYTES bytes: byte j = (frame + j) mod 256.
 *
 * WHY not a real ST 0601 record?
 *   The muxer is opaque to KLV contents on the send path — it wraps the
 *   bytes in a metadata-stream PES and emits them. Real MISB ST 0601 KLV is
 *   built with the `tst_st0601_*` encode surface (see the Rust
 *   `klv_encode_minimal` example); for a muxer demo any bytes will do, and a
 *   frame-dependent pattern makes each record distinguishable in a hex dump.
 */
static size_t make_klv(uint8_t *buf, uint32_t frame) {
    for (uint32_t j = 0; j < KLV_BYTES; j++) {
        buf[j] = (uint8_t)(frame + j);   /* wraps at 256, same as the Rust twin's wrapping_add */
    }
    return KLV_BYTES;
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * ── Step 1: Parse args ───────────────────────────────────────────────
     *
     * argv[1] = output path (default out.ts in the cwd, matching the Rust
     * twin), argv[2] = duration in seconds (default 5). strtoul with a
     * fallback keeps a typo from turning into a zero-frame run.
     */
    const char *out_path = (argc > 1) ? argv[1] : "out.ts";
    uint32_t duration_s = 5;
    if (argc > 2) {
        char *end = NULL;
        unsigned long v = strtoul(argv[2], &end, 10);
        if (end == argv[2] || *end != '\0' || v == 0 || v > 3600) {
            fprintf(stderr, "usage: mux_to_file [out.ts] [duration_secs 1..3600]\n");
            return 1;
        }
        duration_s = (uint32_t)v;
    }

    /*
     * ── Step 2: Build the mux config ─────────────────────────────────────
     *
     * One program, one H.264 video stream, one KLV stream carried as
     * PrivateData (PMT stream_type 0x06, "async KLV"): no AU-cell wrapping,
     * no per-record PTS in the PES (carries_pts = false). This is the
     * simplest KLV carriage and what `MuxerConfig::default()` selects on the
     * Rust side. PCR interval (40 ms) and PSI interval (100 ms) are left at
     * their defaults — both ETSI TR 101 290 §5 conformant — by simply not
     * calling `tst_mux_config_set_pcr_interval_ms` / `_set_psi_interval_ms`.
     *
     * The PCR PID is also left unset: the muxer resolves it to the first
     * video stream of the program, which is the conventional choice
     * (`tst_mux_config_set_pcr_pid` exists for the rare case where PCR must
     * ride a different PID).
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        /* tst_mux_config_new fails only on out-of-memory; no last-error is recorded. */
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        return 2;
    }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, PROGRAM_NUMBER, PMT_PID);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 2;
    }
    /*
     * The add_*_stream calls return per-stream handles; we don't keep them
     * because the single-stream `tst_muxer_push_video` / `_push_klv`
     * shorthands below resolve the target automatically when exactly one
     * stream of that kind exists. `mux_dual_camera.c` shows the handle-taking
     * `_to` variants for the multi-stream case.
     */
    tst_mux_config_add_video_stream(cfg, prog, VIDEO_PID, TST_VIDEO_CODEC_H264);
    tst_mux_config_add_klv_stream(cfg, prog, KLV_PID,
                                  TST_KLV_STREAM_TYPE_PRIVATE_DATA,
                                  /*carries_pts=*/ false);

    /*
     * ── Step 3: Open the muxer ───────────────────────────────────────────
     *
     * tst_muxer_open validates the config (duplicate PIDs, PSI interval
     * below 10 ms, sync-KLV without PTS, ...) and returns NULL with the
     * thread-local last-error set if anything is wrong. It copies the config
     * by value, so `cfg` is ours to free immediately — on BOTH branches.
     */
    tst_muxer_t *mux = tst_muxer_open(cfg);
    tst_mux_config_free(cfg);
    cfg = NULL;
    if (!mux) {
        fprintf(stderr, "tst_muxer_open failed: %s\n", tst_get_last_error_str());
        return 3;
    }

    FILE *out = fopen(out_path, "wb");
    if (!out) {
        perror(out_path);
        tst_muxer_close(mux);
        return 4;
    }

    /*
     * ── Step 4: Push frames, draining after every push ───────────────────
     *
     * WHY drain after every push instead of at the end?
     *   The muxer buffers emitted TS packets internally (default cap 10,000
     *   packets). Pulling as we go keeps that memory bounded and mirrors a
     *   live sender, which must ship bytes as soon as they exist. `pull`
     *   returns 0 when nothing more is ready right now, otherwise a multiple
     *   of 188 — one or more whole TS packets that fit in the buffer.
     */
    const uint32_t total_frames = duration_s * FRAME_RATE;
    uint8_t au[1000];            /* max AU size below is 800 + 199 = 999 */
    uint8_t klv[KLV_BYTES];
    uint8_t pull_buf[PULL_BUF_BYTES];
    int exit_code = 0;

    for (uint32_t i = 0; i < total_frames; i++) {
        const int64_t pts = (int64_t)i * PTS_PER_FRAME;
        const bool key = (i % GOP_FRAMES) == 0;

        /* Size varies 800..999 bytes with the frame index — see make_h264_au. */
        const size_t au_len = make_h264_au(au, 800 + (i % 200), key);
        if (tst_muxer_push_video(mux, au, au_len, pts, key) != 0) {
            fprintf(stderr, "push_video[%u] failed: %s\n", i, tst_get_last_error_str());
            exit_code = 5;
            break;
        }

        /*
         * One KLV record per frame with the same PTS. For an async
         * (PrivateData) stream the PTS is NOT written to the wire — the
         * argument is accepted for API uniformity and used only by
         * synchronous-metadata streams (see mux_h265_with_klv.c).
         */
        const size_t klv_len = make_klv(klv, i);
        if (tst_muxer_push_klv(mux, klv, klv_len, pts) != 0) {
            fprintf(stderr, "push_klv[%u] failed: %s\n", i, tst_get_last_error_str());
            exit_code = 5;
            break;
        }

        for (;;) {
            size_t n = tst_muxer_pull(mux, pull_buf, sizeof(pull_buf));
            if (n == 0) {
                break;
            }
            if (fwrite(pull_buf, 1, n, out) != n) {
                perror("fwrite");
                exit_code = 6;
                break;
            }
        }
        if (exit_code != 0) {
            break;
        }
    }

    /*
     * ── Step 5: Close ────────────────────────────────────────────────────
     *
     * Everything the muxer emitted has already been pulled (the drain loop
     * runs to `pull == 0` after every push), so close discards nothing.
     * Order matters only for the file: flush+close it AFTER the last pull.
     */
    tst_muxer_close(mux);
    if (fclose(out) != 0) {
        perror("fclose");
        return 6;
    }

    if (exit_code == 0) {
        printf("wrote %u frames (%u s) to %s\n", total_frames, duration_s, out_path);
    }
    return exit_code;
}
