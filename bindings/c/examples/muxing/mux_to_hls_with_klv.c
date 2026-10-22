/*
 * mux_to_hls_with_klv.c — Mux H.264 video + MISB ST 0601 KLV into HLS.
 *
 * Demonstrates the *mux publisher* surface: `tst_mux_publisher_t`. Unlike
 * the raw `tst_publisher_t` (which takes pre-built TS packets — see
 * hls_publish.c), the mux publisher OWNS a full MPEG-TS muxer. You hand it
 * encoded elementary data — Annex-B H.264 NAL units and raw MISB KLV Local
 * Set bytes — and it muxes them into a single MPEG-TS, then pushes that TS
 * to an inner HLS publisher's rolling segments.
 *
 * Why this matters for gimbaled-platform ISR:
 *   The whole point of MISB / STANAG-4609 is that the sensor's positional
 *   KLV metadata travels IN THE SAME transport stream as the video, on its
 *   own PID, time-aligned by PTS. An HLS consumer that demuxes the `.ts`
 *   segments recovers BOTH the video frames AND the per-frame KLV — platform
 *   lat/lon, sensor pointing angles, slant range, etc. There is no separate
 *   metadata channel to correlate, and no WebVTT sidecar: the KLV stays
 *   inside the .ts (in-band) exactly as ST 1402 / ST 1910.1 specify. This is
 *   the canonical "FMV with embedded metadata over HTTP" delivery shape.
 *
 * Pipeline:
 *   builder → build → tst_publisher_t (HLS)
 *        ↓ (consumed by)
 *   tst_mux_publisher_with_config_hls(publisher, mux_cfg) → tst_mux_publisher_t
 *        ↓
 *   interleave send_video + send_klv (PTS-aligned)
 *        ↓
 *   finish_into_publisher → tst_publisher_t  (muxer flushed, HLS handed back)
 *        ↓
 *   tst_publisher_finish (writes #EXT-X-ENDLIST) → tst_publisher_free
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c --no-default-features --features hls
 *   cc -I target/debug/include \
 *      -L target/debug \
 *      -Wall -Werror \
 *      -o /tmp/mux_to_hls_with_klv \
 *      bindings/c/examples/muxing/mux_to_hls_with_klv.c -ltstrans -lpthread -ldl
 *   LD_LIBRARY_PATH=target/debug /tmp/mux_to_hls_with_klv
 *
 * Inspect the embedded KLV (segments persist under $TMPDIR):
 *   ffprobe -show_streams "$TMPDIR/segment_00000.ts"   # see the klv PID
 *
 * Requires: TST_HAS_HLS == 1 (set when the `hls` cargo feature is enabled).
 */

#include "tstrans.h"

#if !defined(TST_HAS_HLS) || TST_HAS_HLS == 0
#error "This example requires TST_HAS_HLS. Rebuild tst-c with the hls cargo feature enabled."
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

/* ── Synthetic elementary data ─────────────────────────────────────────── */

/*
 * Minimal Annex-B H.264 NAL: 4-byte start code (0x00000001) + IDR slice
 * header byte (0x65 = nal_ref_idc=3, nal_unit_type=5 IDR) + 3 filler bytes.
 * The muxer frames this into a video PES; the `key_frame` arg (not the NAL
 * contents) drives the TS random-access-indicator and the HLS segment cut.
 */
static const uint8_t NAL_H264[8] = {
    0x00, 0x00, 0x00, 0x01, 0x65, 0xBB, 0xBB, 0xBB,
};

/*
 * Minimal 17-byte MISB ST 0601 KLV blob:
 *   - 16 bytes: UAS Datalink LS Universal Label
 *   -  1 byte:  BER short-form length = 0 (empty Local Set)
 * The smallest spec-conformant ST 0601 envelope. We pass it as RAW Local
 * Set bytes — for a SYNCHRONOUS_METADATA stream the muxer auto-prepends the
 * 5-byte Metadata_AU_cell header (ITU-T H.222.0 V9 §2.12.4.2). DO NOT
 * pre-wrap the AU cell on the caller side.
 */
static const uint8_t KLV_BLOB[17] = {
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
    0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00, /* length = 0 */
};

/* ── PIDs + frame count ────────────────────────────────────────────────── */

#define PROGRAM_NUMBER   1
#define PMT_PID          0x1000
#define VIDEO_PID        0x1011
#define KLV_PID          0x1031
#define FRAME_COUNT      30      /* ~1 s at 30 fps */
#define BIND_ADDR        "127.0.0.1:8081"
#define SEGMENT_MS       2000

static const char *output_dir(void) {
    const char *t = getenv("TMPDIR");
    return (t && t[0] != '\0') ? t : "/tmp/";
}

int main(void) {
    int exit_code = 0;

    /*
     * ── Step 1: Build the HLS publisher ──────────────────────────────────
     *
     * Same builder chain as hls_publish.c. The resulting tst_publisher_t is
     * the sink the mux publisher will drain its TS into.
     */
    TstHlsPublisherBuilder *builder = tst_hls_publisher_builder_new();
    if (!builder) {
        fprintf(stderr, "[mux_to_hls] builder_new failed\n");
        return 2;
    }
    (void) tst_hls_publisher_builder_bind(builder, BIND_ADDR);
    (void) tst_hls_publisher_builder_output_dir(builder, output_dir());
    (void) tst_hls_publisher_builder_segment_duration_ms(builder, SEGMENT_MS);

    TstPublisher *pub = tst_hls_publisher_builder_build(builder);
    if (!pub) {
        fprintf(stderr, "[mux_to_hls] build failed: %s\n",
                tst_get_last_error_str());
        return 2;
    }

    /*
     * ── Step 2: Describe the MPEG-TS layout ──────────────────────────────
     *
     * The mux config is the SAME object used by the SRT / RTP / UDP mux
     * senders: one program with a video elementary stream + a KLV stream.
     *
     * The KLV stream is SYNCHRONOUS_METADATA (stream_type 0x15, carries_pts
     * true) — the ST 1402 / ST 1910.1 shape where each KLV unit is AU-cell
     * wrapped and PTS-aligned to the video. (Use PRIVATE_DATA / 0x06 for
     * asynchronous KLV that does not carry per-unit PTS.)
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        fprintf(stderr, "[mux_to_hls] mux_config_new failed\n");
        tst_publisher_free(pub);
        return 2;
    }
    tst_program_handle_t prog =
        tst_mux_config_add_program(cfg, PROGRAM_NUMBER, PMT_PID);
    (void) tst_mux_config_add_video_stream(cfg, prog, VIDEO_PID,
                                           TST_VIDEO_CODEC_H264);
    (void) tst_mux_config_add_klv_stream(cfg, prog, KLV_PID,
                                         TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA,
                                         /*carries_pts=*/true);

    /*
     * ── Step 3: Construct the mux publisher ──────────────────────────────
     *
     * tst_mux_publisher_with_config_hls CONSUMES `pub` (the tst_publisher_t)
     * — after this call do NOT free `pub`; ownership moved into the mux
     * publisher. The mux config is borrowed; we still own it and free it
     * below. Returns NULL on a config error (check last-error).
     */
    TstMuxPublisher *mp = tst_mux_publisher_with_config_hls(pub, cfg);
    /* cfg is borrowed by the call but still owned by us — free it now. */
    tst_mux_config_free(cfg);
    if (!mp) {
        fprintf(stderr, "[mux_to_hls] mux_publisher_with_config_hls failed: %s\n",
                tst_get_last_error_str());
        /* `pub` was consumed by the (failed) call — do not free it. */
        return 2;
    }

    /*
     * ── Step 4: Interleave video + KLV, PTS-aligned ──────────────────────
     *
     * One video NAL + one KLV unit per frame, sharing the same PTS so a
     * downstream consumer can correlate each KLV record with its video
     * frame. PTS advances by 3000 ticks per frame (90 kHz / 30 fps = 3000).
     * Every frame here is flagged key_frame=true, so each segment closes on
     * the first frame at or after SEGMENT_MS of media time and the next one
     * opens independently decodable. segment_duration is the maximum: a
     * real encoder's GOP longer than it is cut mid-GOP so the playlist's
     * target duration holds.
     */
    for (int i = 0; i < FRAME_COUNT && exit_code == 0; i++) {
        int64_t pts = (int64_t) i * 3000;

        int rc = tst_mux_publisher_send_video(mp, NAL_H264, sizeof(NAL_H264),
                                              pts, /*key_frame=*/true);
        if (rc != 0) {
            fprintf(stderr, "[mux_to_hls] send_video[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }

        /*
         * Pass RAW ST 0601 Local Set bytes. The muxer auto-wraps the 5-byte
         * Metadata_AU_cell header for this SYNCHRONOUS_METADATA stream —
         * caller must NOT pre-wrap. stream_index 0 selects the single KLV
         * stream configured above.
         */
        rc = tst_mux_publisher_send_klv(mp, KLV_BLOB, sizeof(KLV_BLOB),
                                        pts, /*stream_index=*/0);
        if (rc != 0) {
            fprintf(stderr, "[mux_to_hls] send_klv[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
    }

    /*
     * ── Step 5: Finish the muxer, recover the HLS publisher ──────────────
     *
     * finish_into_publisher CONSUMES `mp`, flushes the muxer, and hands the
     * inner HLS publisher back as a fresh tst_publisher_t. Do not free `mp`
     * afterward. On error it returns NULL.
     */
    TstPublisher *hls = tst_mux_publisher_finish_into_publisher(mp);
    if (!hls) {
        fprintf(stderr, "[mux_to_hls] finish_into_publisher failed: %s\n",
                tst_get_last_error_str());
        return 4;
    }

    /*
     * ── Step 6: Finish the HLS publisher + free ──────────────────────────
     *
     * tst_publisher_finish writes the terminating #EXT-X-ENDLIST and tears
     * down the HTTP server; tst_publisher_free reclaims the handle.
     */
    if (exit_code == 0) {
        int rc = tst_publisher_finish(hls);
        if (rc != 0) {
            fprintf(stderr, "[mux_to_hls] publisher_finish failed (rc=%d): %s\n",
                    rc, tst_get_last_error_str());
            exit_code = 4;
        } else {
            fprintf(stderr,
                    "[mux_to_hls] muxed %d video+KLV frames into HLS under %s\n",
                    FRAME_COUNT, output_dir());
        }
    }

    tst_publisher_free(hls);
    fprintf(stderr, "[mux_to_hls] done.\n");
    return exit_code;
}
