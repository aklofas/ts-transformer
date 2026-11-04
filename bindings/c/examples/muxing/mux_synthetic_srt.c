/*
 * mux_synthetic_srt.c — Minimal SRT sender: synthetic H.264 video + KLV → MPEG-TS over SRT.
 *
 * The shortest learnable C example for the libtstrans send-side API.
 * Builds a single-program multiplex with one H.264 video stream and one
 * KLV metadata stream, then pushes 5 synthetic frames to an SRT URL.
 *
 * This example demonstrates:
 *   - Building a tst_mux_config_t from C.
 *   - Opening a tst_mux_sender_t to an SRT caller URL.
 *   - Push-side error handling via tst_get_last_error_str().
 *   - The minimum 5-frame loop that produces a structurally valid TS.
 *
 * Build (from the ts-transformer workspace root). The committed header's
 * config declares no transport, so including bindings/c/include directly
 * leaves the SRT declarations out; build against a packaged copy instead:
 *   bash scripts/dev/c-package.sh --features srt --out /tmp/tstrans-pkg
 *   cc -I /tmp/tstrans-pkg/include -L /tmp/tstrans-pkg/lib \
 *      -Wl,-rpath,/tmp/tstrans-pkg/lib -Wall -Werror \
 *      -o /tmp/mux_synthetic_srt \
 *      bindings/c/examples/muxing/mux_synthetic_srt.c -ltstrans -lpthread
 *
 * Run (sender):
 *   /tmp/mux_synthetic_srt 127.0.0.1:9000
 *
 * Run (receiver, separate terminal — receives and writes to a .ts file):
 *   srt-live-transmit srt://:9000 file://con > /tmp/out.ts
 *
 * Verify the output:
 *   ffprobe /tmp/out.ts
 *     # Should report 1 video stream (h264) + 1 data stream (KLV).
 *   tsp -I file /tmp/out.ts -P analyze
 *     # TSDuck — confirms PMT enumerates both PIDs and PCR cadence is sane.
 *
 * Mirrors examples/sending/send_pipeline_to_socket.rs (Rust).
 */

#include "tstrans.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/*
 * Synthetic H.264 NAL — 4-byte Annex-B start code + nal_unit_type 0x65 (IDR slice)
 * + 0xAA filler.
 *
 * WHY 0x65 (IDR slice) not 0x67 (SPS)?
 *   For a sender-only example with no actual decoder downstream, either works
 *   structurally. 0x65 chosen for parallelism with real H.264 IDR slices that
 *   downstream tools display as such in hex dumps; the muxer trusts the
 *   `key_frame` parameter regardless of NAL contents (it does not parse the
 *   bitstream to set random_access_indicator — see Step 4 below).
 *
 * WHY 500 bytes?
 *   A real H.264 IDR slice is much larger; 500 bytes is enough to land in
 *   multiple TS packets (188 - 14 = 174 useful bytes per PES-carrying TS
 *   packet, so 500 / 174 ≈ 3 packets per video frame). Demonstrates the
 *   muxer's PES-fragmentation logic without being slow to build.
 */
static void make_nal(uint8_t *buf, size_t len) {
    buf[0] = 0; buf[1] = 0; buf[2] = 0; buf[3] = 1; buf[4] = 0x65;
    memset(buf + 5, 0xAA, len - 5);
}

/*
 * Synthetic ST 0601 KLV — 16-byte UAS Datalink LS Universal Label + BER
 * short-form length (16) + 16 payload bytes = 33 bytes total.
 *
 * WHY 33 bytes (not 17 as in the smallest legal KLV)?
 *   17 bytes is the smallest spec-conformant ST 0601 packet (16-byte UL +
 *   1 length byte of value 0). This example uses 33 bytes to demonstrate a
 *   non-empty payload so the receiver can hex-dump and see the changing
 *   `seq` byte across frames (each frame: payload is 16 copies of the byte
 *   value `seq`).
 *
 * WHY the specific UL bytes?
 *   0x06 0x0E 0x2B 0x34 ... is the SMPTE/MISB UL prefix; the full 16 bytes
 *   spell out the UAS Datalink LS key. Any conformant KLV parser will
 *   recognize this stream as MISB ST 0601.
 *
 * Returns total bytes written (always 33).
 */
static size_t make_klv(uint8_t *buf, uint8_t seq) {
    static const uint8_t ul[16] = {
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
        0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    };
    memcpy(buf, ul, 16);
    buf[16] = 16;            /* BER short-form length = 16 */
    memset(buf + 17, seq, 16);
    return 33;
}

int main(int argc, char **argv) {
    /*
     * ── Step 1: Build the SRT URL ────────────────────────────────────────
     *
     * Accept "host:port" as argv[1] (default 127.0.0.1:9000) and prepend
     * the srt:// scheme. The libtstrans URL parser reads the scheme to
     * route to the SRT transport — other schemes (file:// etc.) are
     * accepted by some entry points but not by tst_mux_sender_open.
     */
    const char *host_port = (argc > 1) ? argv[1] : "127.0.0.1:9000";
    char url[256];
    snprintf(url, sizeof(url), "srt://%s", host_port);
    fprintf(stderr, "sending to %s\n", url);

    /*
     * ── Step 2: Build the mux config ─────────────────────────────────────
     *
     * tst_mux_config_t is the opaque heap-allocated config builder.
     * Populate it via the tst_mux_config_add_* family, then hand it to
     * tst_mux_sender_open (which clones the inner — you may free cfg
     * immediately after a successful open).
     *
     * WHY program_number=1 and pmt_pid=0x1000?
     *   MPEG-TS reserves program_number=0 for the NIT pointer in the PAT;
     *   our data program is program 1. PMT PID 0x1000 is a widely-used
     *   convention for single-program TS — most decoders accept it
     *   without configuration.
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        /* tst_mux_config_new fails only on OOM; no last-error to print. */
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        return 1;
    }

    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 2;
    }

    /*
     * Add one video stream and one KLV stream.
     *
     * WHY 0x1011 (video) and 0x1031 (KLV)?
     *   PIDs 0x0000-0x000F are reserved (PAT, CAT, NIT, etc.); 0x1FFF is
     *   the null packet PID; 0x1000 is our PMT. Spreading our elementary
     *   stream PIDs by ≥16 (0x10) makes them visually distinct in a
     *   Wireshark / TSDuck capture and avoids accidental adjacency.
     *
     * WHY TST_KLV_STREAM_TYPE_PRIVATE_DATA (async)?
     *   Async KLV (stream_type 0x06) carries no PTS in the PES; downstream
     *   parsers treat it as a best-effort side-channel keyed by arrival
     *   order, not timestamp. Simpler shape than synchronous KLV (which
     *   requires caller-side AU-cell wrapping). For typical "metadata
     *   alongside video" use cases, async is the right default.
     *
     * On a single-stream sender these add_* calls return a handle, but
     * since we'll use the no-handle `tst_mux_sender_send_video` /
     * `tst_mux_sender_send_klv` entry points below, we don't need to
     * capture or check the handles individually here — tst_mux_sender_open
     * will fail downstream if any stream-add went wrong.
     */
    tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    tst_mux_config_add_klv_stream(cfg, prog, 0x1031,
                                   TST_KLV_STREAM_TYPE_PRIVATE_DATA,
                                   /*carries_pts=*/false);

    /*
     * ── Step 3: Open the sender ──────────────────────────────────────────
     *
     * tst_mux_sender_open copies what it needs from cfg and connects to the
     * SRT peer. On failure, sets the thread-local last-error and returns
     * NULL.
     *
     * The cfg pointer remains owned by us — tst_mux_sender_open does NOT
     * free it on either success or failure. We free it ourselves after
     * either branch (matching Step 2's note that cfg may be freed
     * immediately after a successful open).
     */
    tst_mux_sender_t *s = tst_mux_sender_open(url, cfg);
    if (!s) {
        fprintf(stderr, "tst_mux_sender_open failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 3;
    }
    tst_mux_config_free(cfg);
    cfg = NULL;  /* prevent accidental use after free */

    /*
     * ── Step 4: Push 5 synthetic frames ──────────────────────────────────
     *
     * WHY 33000 PTS ticks per frame?
     *   MPEG-TS PTS uses a 90 kHz clock. At 30 fps: 90000/30 = 3000 ticks/frame.
     *   Using 33000 here (≈ one frame at 2.72 fps) makes the example slow
     *   enough to observe in a packet capture without rushing the receiver.
     *   For a real 30 fps sender, use 3000.
     *
     * WHY usleep(33ms)?
     *   At 30 fps wall-clock cadence. Real-world senders pace their pushes
     *   to match the encoder's output rate so the SRT sender's bandwidth
     *   estimator settles correctly.
     *
     * WHY keyframe = (i == 0)?
     *   Only the first frame is marked as an IDR/keyframe; the muxer uses
     *   this to set the random_access_indicator bit in the adaptation
     *   field for that TS packet, so a downstream demuxer can seek to it.
     *   Subsequent frames are non-key (P-frames in a real encoder).
     */
    uint8_t nal[500];
    uint8_t klv[64];
    for (int i = 0; i < 5; i++) {
        make_nal(nal, sizeof(nal));
        size_t klv_len = make_klv(klv, (uint8_t)i);
        int64_t pts = (int64_t)i * 33000;
        bool keyframe = (i == 0);

        int rc = tst_mux_sender_send_video(s, nal, sizeof(nal), pts, keyframe);
        if (rc != 0) {
            fprintf(stderr, "send_video[%d] failed: %s\n", i, tst_get_last_error_str());
            goto fail;
        }

        rc = tst_mux_sender_send_klv(s, klv, klv_len, pts);
        if (rc != 0) {
            fprintf(stderr, "send_klv[%d] failed: %s\n", i, tst_get_last_error_str());
            goto fail;
        }

        usleep(33 * 1000);
    }

    /*
     * ── Step 5: Drain and close ──────────────────────────────────────────
     *
     * WHY the 200ms post-close sleep?
     *   tst_mux_sender_close flushes the muxer's internal buffer and waits
     *   for the SRT close handshake. On a fast loopback the close returns
     *   immediately; the 200ms sleep is belt-and-suspenders for the case
     *   where the receiver hasn't drained its socket yet (rare but
     *   observable on slow CI hardware).
     */
    fprintf(stderr, "done. closing.\n");
    tst_mux_sender_close(s);
    usleep(200 * 1000);
    return 0;

fail:
    /*
     * Cleanup path: close the sender (cfg is already freed above).
     */
    tst_mux_sender_close(s);
    return 4;
}
