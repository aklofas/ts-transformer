/*
 * mux_h265_with_klv.c — H.265 + SYNCHRONOUS-KLV flavour of `mux_to_file.c`.
 *
 * Diffs against `mux_to_file.c` to show exactly which config knobs flip when
 * you switch codec and KLV carriage mode — everything else (handle
 * lifecycle, drain loop, file writing) is the same shape:
 *
 *   TST_VIDEO_CODEC_H264                  → TST_VIDEO_CODEC_H265
 *   TST_KLV_STREAM_TYPE_PRIVATE_DATA      → TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA
 *   carries_pts = false                   → carries_pts = true
 *   1-byte H.264 NAL header               → 2-byte H.265 NAL header
 *
 * The C twin of the Rust `examples/muxing/mux_h265_with_klv.rs`, reproducing
 * its config and synthetic payloads exactly so both produce byte-identical
 * output. Verified 2026-09-06 with `cmp` (150 frames = 228,044 bytes =
 * 1,213 TS packets from both).
 *
 * WHY synchronous KLV?
 *   PrivateData (async, stream_type 0x06) hands the receiver a stream of
 *   KLV records with no wire-level timestamp, so it cannot say WHICH video
 *   frame a record belongs to. SynchronousMetadata (stream_type 0x15, MISB
 *   ST 1402.2 §9.4.1 / ITU-T H.222.0 §2.12.4) carries a PTS in each PES
 *   header, which is what lets a receiver align every metadata record to
 *   the matching frame — the STANAG 4609 "platform position at the instant
 *   of this frame" contract gimbaled-platform consumers rely on. The cost is
 *   one extra rule: sync KLV REQUIRES carries_pts = true (the muxer rejects
 *   the combination SYNCHRONOUS_METADATA + carries_pts = false at open).
 *
 * Caller responsibility with sync KLV — pass RAW KLV LS bytes:
 *   For SYNCHRONOUS_METADATA streams the muxer itself prepends the 5-byte
 *   Metadata_AU_cell header (H.222.0 §2.12.4.2 Tables 2-155/2-156:
 *   metadata_service_id + sequence_number + flags + AU_cell_data_length)
 *   before every record. Do NOT pre-wrap — the record would end up wrapped
 *   twice and decoders would see the inner header as garbage KLV. The C
 *   push takes no metadata_service_id argument; it always writes the spec
 *   default 0x00 (the Rust API exposes the byte for the rare
 *   multi-service-per-PID case).
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c                     # offline surface: no transport feature needed
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/mux_h265_with_klv bindings/c/examples/muxing/mux_h265_with_klv.c -ltstrans
 *
 * Run:
 *   LD_LIBRARY_PATH=target/debug /tmp/mux_h265_with_klv /tmp/h265.ts
 *
 * Verify:
 *   ffprobe /tmp/h265.ts           # 1 video stream (hevc) + 1 data stream (KLV, stream_type 0x15)
 *   cargo run -p tst-examples --example mux_h265_with_klv -- /tmp/rust_h265.ts && cmp /tmp/h265.ts /tmp/rust_h265.ts
 */

#include "tstrans.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * Same PIDs as `mux_to_file.c` (and `MuxerConfig::default()` on the Rust
 * side) — kept identical on purpose so a diff between the two examples
 * shows only the codec / KLV-mode knobs, not unrelated PID rearrangement.
 */
#define PROGRAM_NUMBER  1
#define PMT_PID         0x1000
#define VIDEO_PID       0x1011
#define KLV_PID         0x1031

/* 30 fps for 5 s = 150 frames; 90000 / 30 = 3000 PTS ticks per frame. */
#define FRAME_RATE      30
#define DURATION_S      5
#define TOTAL_FRAMES    (FRAME_RATE * DURATION_S)
#define PTS_PER_FRAME   (90000 / FRAME_RATE)
#define GOP_FRAMES      (FRAME_RATE * 2)   /* one IDR every 2 s */

#define PULL_BUF_BYTES  1316               /* 7 × 188, see mux_to_file.c */
#define KLV_BYTES       50

/* ── Synthetic payload builders ────────────────────────────────────────── */

/*
 * make_h265_au — fill `buf` with a synthetic Annex-B H.265 access unit of
 * `len` bytes and return `len`.
 *
 * H.265's NAL unit header is TWO bytes (H.265 §7.3.1.2), versus H.264's one:
 *   byte 0: forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id[5](1)
 *   byte 1: nuh_layer_id[4:0](5)  | nuh_temporal_id_plus1(3)
 *
 * Key frames use nal_unit_type 19 (IDR_W_RADL); other frames use type 1
 * (TRAIL_R, a non-IDR reference picture). `(type << 1)` packs the 6-bit
 * type into the top of byte 0 with forbidden_zero = 0 and the high
 * layer-id bit = 0. Byte 1 = 0x01 encodes nuh_layer_id = 0 and
 * nuh_temporal_id_plus1 = 1 (temporal_id 0 — the base layer).
 *
 * As with H.264, the muxer never parses NAL contents; the header bytes are
 * for the benefit of tools that inspect the output.
 */
static size_t make_h265_au(uint8_t *buf, size_t len, bool key) {
    const uint8_t nal_type = key ? 19 : 1;
    buf[0] = 0x00; buf[1] = 0x00; buf[2] = 0x00; buf[3] = 0x01;  /* Annex-B start code */
    buf[4] = (uint8_t)(nal_type << 1);
    buf[5] = 0x01;
    memset(buf + 6, 0xA5, len - 6);                              /* filler "slice data" */
    return len;
}

/* Same frame-dependent pattern as mux_to_file.c: byte j = (frame + j) mod 256. */
static size_t make_klv(uint8_t *buf, uint32_t frame) {
    for (uint32_t j = 0; j < KLV_BYTES; j++) {
        buf[j] = (uint8_t)(frame + j);
    }
    return KLV_BYTES;
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * Default output path: $TMPDIR/h265.ts, falling back to /tmp — the C
     * analogue of the Rust twin's `env::temp_dir().join("h265.ts")`. (C
     * examples are Linux-only by build convention, so a POSIX temp path is
     * fine here where the Rust examples must stay cross-platform.)
     */
    char default_path[4096];
    const char *tmpdir = getenv("TMPDIR");
    snprintf(default_path, sizeof(default_path), "%s/h265.ts",
             (tmpdir && *tmpdir) ? tmpdir : "/tmp");
    const char *out_path = (argc > 1) ? argv[1] : default_path;

    /*
     * ── Step 1: Build the mux config — the two knobs that differ ─────────
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        return 2;
    }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, PROGRAM_NUMBER, PMT_PID);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 2;
    }
    /* Knob 1: codec. Selects PMT stream_type 0x24 (HEVC) instead of 0x1B (AVC). */
    tst_mux_config_add_video_stream(cfg, prog, VIDEO_PID, TST_VIDEO_CODEC_H265);
    /*
     * Knob 2: KLV carriage. SYNCHRONOUS_METADATA (stream_type 0x15) with
     * carries_pts = true — the pair is mandatory together; the muxer
     * validates it at open (see the header comment for why).
     */
    tst_mux_config_add_klv_stream(cfg, prog, KLV_PID,
                                  TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA,
                                  /*carries_pts=*/ true);

    /*
     * ── Step 2: Open the muxer (config validated here) ───────────────────
     *
     * This is where a SYNCHRONOUS_METADATA + carries_pts = false mistake
     * would surface: NULL return, TST_E_INVALID_CONFIG in the thread-local
     * last-error, detail string naming the rule.
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
     * ── Step 3: Push 150 frames, draining after each ─────────────────────
     */
    uint8_t au[1200];            /* max AU size below is 1000 + 199 = 1199 */
    uint8_t klv[KLV_BYTES];
    uint8_t pull_buf[PULL_BUF_BYTES];
    int exit_code = 0;

    for (uint32_t i = 0; i < TOTAL_FRAMES; i++) {
        const int64_t pts = (int64_t)i * PTS_PER_FRAME;
        const bool key = (i % GOP_FRAMES) == 0;

        /* Size varies 1000..1199 bytes with the frame index. */
        const size_t au_len = make_h265_au(au, 1000 + (i % 200), key);
        if (tst_muxer_push_video(mux, au, au_len, pts, key) != 0) {
            fprintf(stderr, "push_video[%u] failed: %s\n", i, tst_get_last_error_str());
            exit_code = 5;
            break;
        }

        /*
         * Sync KLV: the muxer prepends the 5-byte AU-cell header to these
         * raw bytes, then writes a PES whose header carries `pts` — the
         * same PTS as the video frame pushed just above, which is the
         * whole point: the receiver can pair this record with that frame.
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

    /* ── Step 4: Close (nothing left unpulled — see mux_to_file.c) ─────── */
    tst_muxer_close(mux);
    if (fclose(out) != 0) {
        perror("fclose");
        return 6;
    }

    if (exit_code == 0) {
        printf("wrote %u H.265 frames (sync KLV, %u s) to %s\n",
               (unsigned)TOTAL_FRAMES, (unsigned)DURATION_S, out_path);
    }
    return exit_code;
}
