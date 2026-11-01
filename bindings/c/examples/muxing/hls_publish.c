/*
 * hls_publish.c — Publish pre-built MPEG-TS packets as an HLS stream.
 *
 * Demonstrates the *publisher* surface: `tst_publisher_t`. An HLS publisher
 * is NOT a transport — there is no peer to connect to. It is an outbound,
 * segment-aware sink that writes rolling `.ts` segments + an `.m3u8`
 * playlist to a directory AND serves them over an internal HTTP server.
 * A media player points its HLS client at `http://<bind>/playlist.m3u8`.
 *
 * This is the "raw TS bytes in" path — the caller hands the publisher
 * 188-byte-aligned MPEG-TS packets via tst_publisher_push_ts, exactly as a
 * hardware encoder, a file relay, or a libtstrans muxer drain would emit
 * them. If instead you have encoded NAL units / KLV / audio and want the
 * library to mux them for you, see the sibling mux_to_hls_with_klv.c example
 * (which owns a full muxer via tst_mux_publisher_t).
 *
 * Why HLS for gimbaled-platform video?
 *   HLS rides plain HTTP, so it traverses NATs, proxies, and CDNs that block
 *   UDP-based transports (SRT / RTP / plain UDP). The trade-off is latency:
 *   HLS buffers whole segments, so end-to-end delay is on the order of a few
 *   segment durations (seconds), not the sub-second of SRT. Use HLS when the
 *   consumer is a browser / off-the-shelf player on the far side of an HTTP
 *   boundary; use SRT/RTP for low-latency operator feeds. KLV metadata stays
 *   in-band inside the `.ts` segments (STANAG-4609 carriage) — an HLS client
 *   that demuxes the segments still sees the KLV PID.
 *
 * What this example shows:
 *   1. Building the publisher via the builder chain (bind, output_dir,
 *      segment_duration_ms).
 *   2. Building the publisher (binds the HTTP server immediately).
 *   3. Querying the bound address (useful when binding to an ephemeral :0).
 *   4. Synthesising structurally valid 188-byte TS null packets.
 *   5. Pushing 100 of those via tst_publisher_push_ts, cutting a segment.
 *   6. Finishing cleanly (writes #EXT-X-ENDLIST) and freeing.
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c --no-default-features --features hls
 *   cc -I target/debug/include \
 *      -L target/debug \
 *      -Wall -Werror \
 *      -o /tmp/hls_publish \
 *      bindings/c/examples/muxing/hls_publish.c -ltstrans -lpthread -ldl
 *   LD_LIBRARY_PATH=target/debug /tmp/hls_publish
 *
 * Watch the result (while it runs, or after — VOD/event playlists persist):
 *   ffplay http://127.0.0.1:8080/playlist.m3u8
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
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/* MPEG-TS packet size: exactly 188 bytes (ISO/IEC 13818-1 §2.4.3.2). */
#define TS_PACKET_SIZE   188
#define TS_SYNC_BYTE     0x47
#define TS_NULL_PID_HI   0x1F   /* high 5 bits of the 0x1FFF null PID */
#define TS_NULL_PID_LO   0xFF   /* low 8 bits of the 0x1FFF null PID  */

/* 100 packets × 188 bytes = 18,800 bytes — finishes instantly. */
#define PUSH_COUNT       100

/* Bind to a fixed loopback port so the playlist URL is predictable. */
#define DEFAULT_BIND     "127.0.0.1:8080"

/* Target 2 s segments — typical HLS live segment length. */
#define SEGMENT_MS       2000

/* ── Helper: synthesize one null TS packet ─────────────────────────────── */

/*
 * make_null_ts_packet — fill `buf` (exactly TS_PACKET_SIZE bytes) with a
 * valid MPEG-TS null packet (PID 0x1FFF, payload-only). Null packets carry
 * a PID every conformant receiver discards without parsing — a safe filler
 * for a transport-level example. push_ts requires 188-aligned buffers.
 */
static void make_null_ts_packet(uint8_t *buf) {
    buf[0] = TS_SYNC_BYTE;
    buf[1] = TS_NULL_PID_HI;
    buf[2] = TS_NULL_PID_LO;
    buf[3] = 0x10; /* adaptation_field_control=01 (payload only) */
    memset(buf + 4, 0xFF, TS_PACKET_SIZE - 4);
}

/* ── Helper: pick an output directory ──────────────────────────────────── */

/*
 * The output directory must be owned by this user and not writable by anyone
 * else, so the shared temp directory itself (root-owned, sticky, world
 * writable) is refused. Use a subdirectory of it instead: the publisher
 * creates it with mode 0700 when it does not exist.
 */
static const char *output_dir(void) {
    static char path[512];
    const char *t = getenv("TMPDIR");
    if (!t || t[0] == '\0') {
        t = "/tmp";
    }
    snprintf(path, sizeof path, "%s/tstrans-hls-example", t);
    return path;
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(void) {
    /*
     * ── Step 1: Configure the publisher via the builder chain ────────────
     *
     * The builder uses move-style chain setters on the Rust side; the C ABI
     * exposes each as a separate call that mutates the opaque builder in
     * place. Each setter returns 0 on success or a negative TST_E_* code.
     */
    TstHlsPublisherBuilder *builder = tst_hls_publisher_builder_new();
    if (!builder) {
        fprintf(stderr, "[hls_publish] builder_new failed (OOM)\n");
        return 2;
    }

    int rc = tst_hls_publisher_builder_bind(builder, DEFAULT_BIND);
    if (rc != 0) {
        fprintf(stderr, "[hls_publish] bind failed (rc=%d): %s\n",
                rc, tst_get_last_error_str());
        tst_hls_publisher_builder_free(builder);
        return 2;
    }

    const char *dir = output_dir();
    rc = tst_hls_publisher_builder_output_dir(builder, dir);
    if (rc != 0) {
        fprintf(stderr, "[hls_publish] output_dir failed (rc=%d): %s\n",
                rc, tst_get_last_error_str());
        tst_hls_publisher_builder_free(builder);
        return 2;
    }
    (void) tst_hls_publisher_builder_segment_duration_ms(builder, SEGMENT_MS);

    /*
     * ── Step 2: Build the publisher ──────────────────────────────────────
     *
     * build() binds the internal HTTP server immediately and CONSUMES the
     * builder — do not free the builder afterward (it is gone whether build
     * succeeds or fails). On failure the publisher is NULL; check last-error.
     */
    TstPublisher *pub = tst_hls_publisher_builder_build(builder);
    if (!pub) {
        fprintf(stderr, "[hls_publish] build failed: %s\n",
                tst_get_last_error_str());
        return 2;
    }
    fprintf(stderr, "[hls_publish] publisher kind: %u (0 == HLS)\n",
            tst_publisher_get_kind(pub));

    /*
     * ── Step 3: Report the bound address ─────────────────────────────────
     *
     * Useful when binding to an ephemeral port (":0"). With a fixed port
     * here it just echoes DEFAULT_BIND, but the pattern is the same.
     */
    char addr[64];
    int n = tst_hls_publisher_local_addr(pub, addr, sizeof(addr));
    if (n >= 0) {
        fprintf(stderr, "[hls_publish] serving HLS at http://%s/playlist.m3u8\n",
                addr);
    }
    fprintf(stderr, "[hls_publish] writing segments under: %s\n", dir);

    /*
     * ── Step 4: Push 100 TS packets ──────────────────────────────────────
     *
     * push_ts requires whole multiples of 188 bytes (one or more TS packets).
     * One packet per call here for clarity; batch for throughput in real code.
     * Returns 0 on success or a negative TST_E_* code (e.g. TST_E_HLS_CONFIG
     * for an unaligned buffer, TST_E_HLS_FINISHED after finish).
     */
    uint8_t ts_pkt[TS_PACKET_SIZE];
    make_null_ts_packet(ts_pkt);

    int exit_code = 0;
    for (int i = 0; i < PUSH_COUNT; i++) {
        rc = tst_publisher_push_ts(pub, ts_pkt, TS_PACKET_SIZE);
        if (rc != 0) {
            fprintf(stderr, "[hls_publish] push_ts[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
    }

    /*
     * ── Step 5: Cut a segment ────────────────────────────────────────────
     *
     * cut_segment hints that the next push starts a new segment. Call it on
     * keyframe boundaries so each .ts is independently decodable. Here we cut
     * once so the run produces at least one complete segment.
     */
    if (exit_code == 0) {
        (void) tst_publisher_cut_segment(pub);
    }

    /*
     * ── Step 6: Finish + free ────────────────────────────────────────────
     *
     * finish() flushes the open segment, writes the terminating
     * #EXT-X-ENDLIST tag (so VOD/event players know the stream ended), and
     * tears down the HTTP server. The handle is still allocated — free it.
     * Pushing after finish returns TST_E_HLS_FINISHED.
     */
    if (exit_code == 0) {
        rc = tst_publisher_finish(pub);
        if (rc != 0) {
            fprintf(stderr, "[hls_publish] finish failed (rc=%d): %s\n",
                    rc, tst_get_last_error_str());
            exit_code = 4;
        } else {
            fprintf(stderr,
                    "[hls_publish] published %d TS packets; playlist finalized.\n",
                    PUSH_COUNT);
        }
    }

    tst_publisher_free(pub);
    fprintf(stderr, "[hls_publish] publisher freed.\n");
    return exit_code;
}
