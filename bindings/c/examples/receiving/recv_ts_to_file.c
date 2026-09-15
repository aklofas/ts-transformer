/*
 * recv_ts_to_file.c — bind a listener on srt://:7000, recv 188-byte
 * aligned MPEG-TS packets, write each packet to the file path given
 * in argv[1], dump stats every 1000 packets, exit cleanly on peer
 * disconnect.
 *
 * Why this example:
 *   The TS-aligned receiver is one layer up from the raw byte stream
 *   (recv_raw_to_file.c). The library hides the sync-recovery state
 *   machine (HUNT → VERIFY → LOCKED) — you ask for "the next 188-byte
 *   packet" and the library handles re-locking after a network gap
 *   or corruption. The periodic stats dump shows the sync-recovery
 *   counters; on a clean loopback they stay at zero. On a lossy
 *   network or after a sender restart, bytes_skipped_for_sync grows
 *   and resync_events increments each time the syncer re-locks.
 *
 *   Use this shape when you want aligned TS packets but don't want
 *   to pay for full demux (e.g., a relay that forwards packets to
 *   another transport, a probe that counts PIDs, or a third-party
 *   decoder that expects raw TS packets).
 *
 * Differences from recv_raw_to_file.c:
 *   - Protocol unit is a fixed 188-byte TS packet (not a
 *     variable-length SRT message), so there is no heap-fallback
 *     buffer and no TST_E_TOO_LARGE branch in the recv loop.
 *   - Periodic stats dump exposes the syncer counters
 *     (bytes_skipped_for_sync, resync_events) — visible only via
 *     tst_receiver_t, not tst_raw_receiver_t.
 *
 * How to run:
 *   1. In one terminal (receiver first, so the port is ready):
 *        ./recv_ts_to_file out.ts
 *   2. In another terminal (any sender pushing TS bytes to srt://127.0.0.1:7000):
 *        cargo run -p tst-examples --example ts_relay_from_file -- input.ts 127.0.0.1:7000
 *      (input.ts can come from mux_to_file.rs, or any pre-muxed .ts file —
 *       see examples/sending/ for the full catalogue).
 *   3. When the sender disconnects, recv_ts_to_file exits automatically
 *      after printing a final summary.
 *   4. Inspect the output:
 *        ffprobe out.ts
 *        tsp -I file out.ts -P analyze   # TSDuck PSI/SI walk
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror \
 *      -o /tmp/recv_ts_to_file \
 *      bindings/c/examples/receiving/recv_ts_to_file.c -ltstrans
 *
 * Run:
 *   /tmp/recv_ts_to_file out.ts
 *
 * Closest Rust analog: examples/receiving/srt_listener_to_file.rs
 * (Rust uses a Receiver to surface bytes over a SrtTransport; the C
 * side here drives the equivalent shape through the C ABI).
 * The C version is more verbose because there is no RAII,
 * and because C readers may have less context about what the safe
 * Rust wrappers underneath are doing on their behalf.
 */

#include "tstrans.h"
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/*
 * MPEG-TS packet size — fixed by the standard at 188 bytes.
 *
 * WHY exactly 188?
 *   Defined by ITU-T H.222.0 §2.4.3. Every TS packet starts with the
 *   0x47 sync byte and contains 187 bytes of payload + header. The
 *   tst_receiver_recv_packet contract requires the caller to pass
 *   a uint8_t[188] buffer — there's no "small" or "large" packet to
 *   worry about, unlike the raw receiver where SRT message sizes
 *   vary up to ~1316 bytes (the default).
 */
#define TS_PACKET_SIZE 188

/*
 * Stats dump cadence.
 *
 * WHY every 1000 packets?
 *   At a typical 5 Mb/s video bitrate, 1000 packets = ~300 ms of
 *   data — frequent enough to observe a sync-recovery event soon
 *   after it happens, sparse enough that the example's stderr
 *   doesn't drown out the user's terminal. Tune to taste.
 */
#define STATS_DUMP_EVERY 1000

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <output-file>\n", argv[0]);
        return 1;
    }

    /* Open the output file before binding the network socket so that
     * file-path errors fail fast, before accepting a peer. */
    FILE *out = fopen(argv[1], "wb");
    if (!out) {
        perror("fopen");
        return 2;
    }

    /*
     * Bind a listener on port 7000 and accept the first peer.
     *
     * tst_receiver_open_listener blocks until the first peer
     * connects, then returns the accepted handle. The listening
     * socket is dropped after accept — single-peer shape. For a
     * multi-peer accept loop, use tst_managed_receiver_open_listener
     * which re-binds and re-accepts on disconnect.
     */
    fprintf(stderr, "listening on srt://:7000; waiting for peer...\n");
    tst_receiver_t *rx = tst_receiver_open_listener("srt://:7000");
    if (!rx) {
        fprintf(stderr, "open_listener failed: %s\n", tst_get_last_error_str());
        fclose(out);
        return 3;
    }
    fprintf(stderr, "peer connected; receiving packets...\n");

    /*
     * ── Recv loop ─────────────────────────────────────────────────────
     *
     * Stack-allocated 188-byte buffer. No heap fallback — the protocol
     * is fixed at 188 bytes per packet so the buffer can never be too
     * small.
     */
    uint8_t pkt[TS_PACKET_SIZE];
    uint64_t total_packets = 0;

    for (;;) {
        int rc = tst_receiver_recv_packet(rx, pkt);

        /* ── rc == 0: normal packet received ── */
        if (rc == 0) {
            if (fwrite(pkt, 1, TS_PACKET_SIZE, out) != TS_PACKET_SIZE) {
                perror("fwrite");
                tst_receiver_close(rx);
                fclose(out);
                return 4;
            }
            total_packets += 1;

            /* Periodic stats dump — shows the syncer counters in
             * action. On a clean loopback these stay at zero; on a
             * lossy network you'll see bytes_skipped_for_sync grow. */
            if (total_packets % STATS_DUMP_EVERY == 0) {
                tst_receiver_stats_t stats;
                int sc = tst_receiver_get_stats(rx, &stats);
                if (sc == 0) {
                    fprintf(stderr,
                            "packets=%" PRIu64
                            " bytes=%" PRIu64
                            " skipped_for_sync=%" PRIu64
                            " resync_events=%" PRIu64 "\n",
                            stats.packets_received,
                            stats.bytes_received,
                            stats.bytes_skipped_for_sync,
                            stats.resync_events);
                }
            }
            continue;
        }

        /* ── rc == TST_E_END_OF_STREAM: peer disconnected cleanly ── */
        if (rc == TST_E_END_OF_STREAM) {
            fprintf(stderr,
                    "peer disconnected; %" PRIu64 " packets received\n",
                    total_packets);
            break;
        }

        /* ── rc == TST_E_CLOSED: our side cancelled ── */
        if (rc == TST_E_CLOSED) {
            fprintf(stderr, "receiver was cancelled\n");
            break;
        }

        /* ── any other return code is a transport or library error ── */
        fprintf(stderr,
                "recv_packet failed (rc=%d): %s\n",
                rc,
                tst_get_last_error_str());
        tst_receiver_close(rx);
        fclose(out);
        return 5;
    }

    /*
     * Normal teardown. _close frees the handle's heap allocation
     * regardless of internal state (closed, cancelled, or still-open).
     * Double-close is a no-op.
     */
    tst_receiver_close(rx);
    fclose(out);

    fprintf(stdout,
            "wrote %" PRIu64 " packets (%" PRIu64 " bytes) to %s\n",
            total_packets,
            total_packets * TS_PACKET_SIZE,
            argv[1]);
    return 0;
}
