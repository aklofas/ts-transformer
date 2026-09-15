/*
 * recv_klv_to_stdout.c — bind a listener on srt://:7000, recv typed
 * demux events, filter TST_EVENT_KIND_METADATA, hex-dump each KLV
 * record to stdout with a PTS + sequence_number prefix.
 *
 * Why this example:
 *   Demonstrates the raw KLV byte flow without typed decode (for the
 *   typed ST 0601 path see the tst_st0601_* family, ABI 0.21, as used by
 *   receiving/recv_srt_events.c). Useful as a building
 *   block for: external KLV parsers, KLV-tap-and-forward shims,
 *   tools that pipe KLV records to a separate process for
 *   ST 0601 / ST 0903 decoding.
 *
 *   Each TST_EVENT_KIND_METADATA event surfaces one KLV record.
 *   ev.u.metadata.payload is the inner KLV LS bytes (no AU cell
 *   wrap; the demuxer strips the 5-byte H.222.0 §2.12.4.2 header
 *   for sync KLV per plan #25). The payload pointer borrows from
 *   the receiver's EventArena per design §4.5 — if you need to
 *   forward this elsewhere, memcpy before the next recv call.
 *
 * How to run:
 *   ./recv_klv_to_stdout > klv.hex
 *   (Then send any stream with KLV PIDs; KLV is typical for
 *    gimbaled-platform streams via the mux_with_klv example.)
 *
 * Build:
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror \
 *      -o /tmp/recv_klv_to_stdout \
 *      bindings/c/examples/receiving/recv_klv_to_stdout.c -ltstrans
 */

#include "tstrans.h"
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static void hex_dump(const uint8_t *p, size_t len) {
    /* Single-line hex dump — terse for KLV records that can be 100s
     * of bytes. Downstream parsers can ingest line-at-a-time. */
    for (size_t i = 0; i < len; i++) {
        printf("%02x", p[i]);
    }
    printf("\n");
}

int main(int argc, char **argv) {
    (void) argc;
    (void) argv;

    tst_demux_receiver_t *rx = tst_demux_receiver_open_listener("srt://:7000");
    if (!rx) {
        fprintf(stderr, "open_listener failed: %s\n", tst_get_last_error_str());
        return 1;
    }
    fprintf(stderr, "listening on srt://:7000; waiting for peer...\n");

    tst_event_t ev = {0};
    uint64_t klv_count = 0;

    for (;;) {
        int rc = tst_demux_receiver_recv_event(rx, &ev);

        if (rc == 0) {
            /* Filter — only print metadata events; ignore samples,
             * PMT updates, discontinuities, etc. */
            if (ev.kind == TST_EVENT_KIND_METADATA) {
                klv_count += 1;
                printf("# pid=0x%04x pts=%" PRId64
                       " kind=%d seq=%u len=%zu\n",
                       ev.u.metadata.pid,
                       ev.u.metadata.pts,
                       ev.u.metadata.metadata_kind,
                       ev.u.metadata.sequence_number,
                       ev.u.metadata.payload_len);
                hex_dump(ev.u.metadata.payload, ev.u.metadata.payload_len);
                /* fflush so streaming consumers see records promptly
                 * even when stdout is pipe-buffered. */
                fflush(stdout);
            }
            continue;
        }

        if (rc == TST_E_END_OF_STREAM) {
            fprintf(stderr,
                    "peer disconnected; %" PRIu64 " KLV records dumped\n",
                    klv_count);
            break;
        }
        if (rc == TST_E_CLOSED) {
            fprintf(stderr, "receiver was cancelled\n");
            break;
        }
        fprintf(stderr,
                "recv_event failed (rc=%d): %s\n",
                rc,
                tst_get_last_error_str());
        tst_demux_receiver_close(rx);
        return 2;
    }

    tst_demux_receiver_close(rx);
    return 0;
}
