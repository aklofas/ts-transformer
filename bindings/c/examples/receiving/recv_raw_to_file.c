/*
 * recv_raw_to_file.c — bind a listener on srt://:7000, recv raw TS
 * bytes from the first peer, write every message chunk to the file
 * path given in argv[1], exit cleanly on peer disconnect.
 *
 * Why this example:
 *   The simplest receiver-side teaching code. Demonstrates:
 *     1. Listener-mode bind via tst_raw_receiver_open_listener — no
 *        connect-handshake; the library waits for the remote peer to
 *        call connect() to us.
 *     2. The recv loop shape: pull one message at a time, write it to
 *        disk, loop.
 *     3. The four-way exit-code interpretation for tst_raw_receiver_recv:
 *        success, EOS (peer FIN), cancelled (our side), transport error,
 *        and message-too-large.
 *     4. Correct teardown order: always call _close to free the handle,
 *        even on error paths.
 *
 * How to run:
 *   1. In one terminal (receiver first, so the port is ready):
 *        ./recv_raw_to_file out.ts
 *   2. In another terminal (any sender targeting srt://127.0.0.1:7000 —
 *      tst_raw_receiver_recv treats every inbound message opaquely, so
 *      any caller-mode sender works, e.g.):
 *        cargo run -p tst-examples --example ts_relay_from_file -- input.ts 127.0.0.1:7000
 *   3. When the sender disconnects, recv_raw_to_file exits automatically.
 *   4. Inspect the output:
 *        ffprobe out.ts
 *        tsp -I file out.ts -P analyze   # TSDuck PSI/SI walk
 *
 * Build (from the ts-transformer workspace root). The committed header's
 * config declares no transport, so including bindings/c/include directly
 * leaves the SRT declarations out; build against a packaged copy instead:
 *   bash scripts/dev/c-package.sh --features srt --out /tmp/tstrans-pkg
 *   cc -I /tmp/tstrans-pkg/include -L /tmp/tstrans-pkg/lib \
 *      -Wl,-rpath,/tmp/tstrans-pkg/lib -Wall -Wextra -Werror \
 *      -o /tmp/recv_raw_to_file \
 *      bindings/c/examples/receiving/recv_raw_to_file.c -ltstrans -lpthread
 *
 * Run:
 *   /tmp/recv_raw_to_file out.ts
 *
 * Closest Rust analog: examples/receiving/srt_listener_to_file.rs
 * (listener bind, recv loop, write to file). The C version is more
 * verbose because there is no RAII, and because C readers may have less
 * context about what the safe Rust wrappers underneath are doing on
 * their behalf.
 */

#include "tstrans.h"
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/*
 * Recv buffer sizing.
 *
 * WHY 1500 bytes?
 *   libsrt's default maximum payload per message in live mode is 1316
 *   bytes (7 × 188, seven MPEG-TS packets per SRT message, at the
 *   IP-1500 MTU).  Senders can configure a larger payload, but 1500
 *   covers every default-configured sender.  If a sender is configured
 *   with a custom (larger) payload size and sends a message that exceeds
 *   RECV_BUF_LEN, tst_raw_receiver_recv returns TST_E_TOO_LARGE instead
 *   of truncating — and that message is gone: the call consumed it, so a
 *   retry cannot recover it.  The recv loop below counts the loss and
 *   grows a heap buffer so LATER messages of that size fit.
 *
 * WHY not stack-allocate a much larger buffer "just in case"?
 *   Large stack buffers can overflow the default 8 MB stack on Linux
 *   (ulimit -s).  The heap buffer the loop allocates after a
 *   TST_E_TOO_LARGE only serves the messages that follow.  The real fix
 *   is to size the buffer from the sender's configured payload size
 *   (its pkt_size) before the first receive, so nothing is ever dropped.
 */
#define RECV_BUF_LEN 1500

/*
 * Fallback heap buffer when a single message exceeds RECV_BUF_LEN.
 *
 * WHY 16 * 1024 * 1024 (16 MiB)?
 *   SRT's documented maximum packet payload is bounded by the underlying
 *   UDP socket's receive buffer.  In practice, custom configurations rarely
 *   exceed a few hundred KB.  16 MiB is a generous ceiling that covers any
 *   realistic sender while still being small compared to typical available
 *   RAM.  Exceeding this would indicate a misconfigured or adversarial peer.
 */
#define RECV_HEAP_MAX (16u * 1024u * 1024u)

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <output-file>\n", argv[0]);
        return 1;
    }

    /* Open the output file before binding the network socket.
     *
     * WHY open the file first?
     *   If the file path is invalid (bad directory, permissions, etc.) we
     *   want to fail immediately, before accepting a peer connection.
     *   Accepting a connection and then failing to open the file forces an
     *   abrupt socket close that looks like a transport error to the sender.
     *   Opening the file first gives a clean, human-readable error instead.
     */
    FILE *out = fopen(argv[1], "wb");
    if (!out) {
        perror("fopen");
        return 2;
    }

    /*
     * Bind a listener on port 7000 and accept the first peer.
     *
     * WHY tst_raw_receiver_open_listener rather than tst_raw_receiver_open?
     *   tst_raw_receiver_open is the URL-dispatch entry point: the URL's
     *   `?mode=caller` (the default) dials out, while `?mode=listener`
     *   routes through the listener path.  tst_raw_receiver_open_listener
     *   forces listener mode regardless of any mode= value in the URL.
     *   For unambiguous listener-mode code, the _listener suffix is clearer
     *   than `?mode=listener` buried in the URL string.
     *
     * WHY "srt://:7000" (empty host, no ?mode=listener)?
     *   With _open_listener entry points, the function name is the
     *   authoritative listener-mode signal.  You can write "srt://:7000"
     *   (clean form) or "srt://:7000?mode=listener" (explicit form) — both
     *   work.  The clean form is preferred here for brevity.  The URL's
     *   empty host is interpreted as the wildcard address (0.0.0.0 for
     *   IPv4, :: for IPv6 when dual-stack is enabled), so the listener
     *   accepts connections from any interface.  For a loopback-only
     *   listener use "srt://127.0.0.1:7000".
     *
     * WHY no ?passphrase=... or ?pbkeylen=... here?
     *   Encryption parameters would require the connecting peer to supply
     *   the same passphrase.  This example pairs with a default sender and
     *   omits encryption for brevity.  For encrypted production code, add
     *   ?passphrase=<key>&pbkeylen=32 to both ends.
     *
     * tst_raw_receiver_open_listener blocks until the first peer connects,
     * then returns the accepted connection as the receiver handle.  The
     * listening socket is discarded after the first accept (single-peer
     * shape).  For a multi-peer accept loop use tst_managed_raw_receiver_t.
     */
    fprintf(stderr, "listening on srt://:7000; waiting for peer...\n");
    tst_raw_receiver_t *rx = tst_raw_receiver_open_listener("srt://:7000");
    if (!rx) {
        fprintf(stderr, "open_listener failed: %s\n", tst_get_last_error_str());
        fclose(out);
        return 3;
    }
    fprintf(stderr, "peer connected; receiving messages...\n");

    /*
     * ── Recv loop ─────────────────────────────────────────────────────────
     *
     * Stack-allocated fast path buffer.  Heap fallback is allocated only
     * if we encounter a message that exceeds RECV_BUF_LEN.
     */
    uint8_t stack_buf[RECV_BUF_LEN];
    uint8_t *heap_buf = NULL;
    size_t heap_buf_len = 0;

    uint64_t total_msgs  = 0;
    uint64_t lost_msgs   = 0;  /* messages dropped by TST_E_TOO_LARGE */
    uint64_t total_bytes = 0;

    for (;;) {
        size_t got = 0;
        /* Use the grown heap buffer once a TST_E_TOO_LARGE has allocated it. */
        uint8_t *rbuf = heap_buf ? heap_buf : stack_buf;
        size_t   rlen = heap_buf ? heap_buf_len : sizeof(stack_buf);
        int rc = tst_raw_receiver_recv(rx, rbuf, rlen, &got);

        /* ── rc == 0: normal message received ── */
        if (rc == 0) {
            if (fwrite(rbuf, 1, got, out) != got) {
                perror("fwrite");
                /* File write error — tear down and exit. */
                tst_raw_receiver_close(rx);
                free(heap_buf);
                fclose(out);
                return 4;
            }
            total_bytes += (uint64_t)got;
            total_msgs  += 1;
            continue;
        }

        /* ── rc == TST_E_END_OF_STREAM: peer disconnected cleanly ── */
        if (rc == TST_E_END_OF_STREAM) {
            /*
             * The peer sent a graceful SRT close (analogous to TCP FIN).
             * This is the normal exit path: log stats and break.
             *
             * WHY distinguish TST_E_END_OF_STREAM from TST_E_CLOSED?
             *   TST_E_END_OF_STREAM means the remote side closed.
             *   TST_E_CLOSED means our side called _cancel.
             *   A real application may want to log a "peer disconnected"
             *   event for telemetry, or reconnect via a managed receiver,
             *   while a "cancelled" event might be a SIGINT handler unwinding
             *   cleanly.  Keeping them separate lets callers branch on intent.
             */
            fprintf(stderr,
                    "peer disconnected; "
                    "%" PRIu64 " messages, %" PRIu64 " bytes\n",
                    total_msgs, total_bytes);
            break;
        }

        /* ── rc == TST_E_CLOSED: our side cancelled ── */
        if (rc == TST_E_CLOSED) {
            /*
             * tst_raw_receiver_cancel was called (e.g. from a signal
             * handler on another thread).  Not triggered in this single-
             * threaded example, but documented so the recv loop pattern
             * is complete and portable to multi-threaded applications.
             *
             * After TST_E_CLOSED the handle is dead; _close still frees
             * it, so we call _close unconditionally in the teardown below.
             */
            fprintf(stderr, "receiver was cancelled\n");
            break;
        }

        /* ── rc == TST_E_TOO_LARGE: message exceeded the buffer ── */
        if (rc == TST_E_TOO_LARGE) {
            /*
             * The sender transmitted a single message larger than the
             * buffer.  That message has been DEQUEUED and is lost: a
             * retry receives the NEXT message, not this one.  Count the
             * loss, then grow the heap fallback so later messages of
             * this size fit.
             *
             * WHY not just use a 64 KB stack buffer everywhere?
             *   Stack space is a finite, per-thread resource.  Reserving
             *   a large stack buffer for a "never happens in practice"
             *   case would be wasteful and could cause stack overflow on
             *   threads with a smaller stack limit.  The heap fallback
             *   allocates only when needed.
             *
             * WHY double the heap buffer size on each growth?
             *   Repeated small growths (realloc by +1 KB each time) are
             *   O(n²) in the number of retries.  Doubling amortises the
             *   growth cost to O(log n) retries.  We cap at RECV_HEAP_MAX
             *   to bound memory usage against adversarial senders.
             */
            size_t new_len = (heap_buf_len == 0) ? 65536u : heap_buf_len * 2u;
            if (new_len > RECV_HEAP_MAX) {
                fprintf(stderr,
                        "message exceeds heap cap (%u bytes); aborting\n",
                        RECV_HEAP_MAX);
                tst_raw_receiver_close(rx);
                free(heap_buf);
                fclose(out);
                return 5;
            }
            uint8_t *tmp = realloc(heap_buf, new_len);
            if (!tmp) {
                fprintf(stderr, "realloc(%zu): out of memory\n", new_len);
                tst_raw_receiver_close(rx);
                free(heap_buf);
                fclose(out);
                return 6;
            }
            heap_buf     = tmp;
            heap_buf_len = new_len;
            lost_msgs   += 1;

            continue;
        }

        /* ── any other return code is a transport or library error ── */
        fprintf(stderr, "recv failed (rc=%d): %s\n", rc, tst_get_last_error_str());
        tst_raw_receiver_close(rx);
        free(heap_buf);
        fclose(out);
        return 7;
    }

    /*
     * Normal teardown.
     *
     * WHY call _close even after TST_E_END_OF_STREAM?
     *   tst_raw_receiver_close frees the handle's heap allocation
     *   regardless of the handle's internal state (closed, cancelled,
     *   or still-open).  Skipping it leaks memory.  The Rust backing
     *   code is resilient — double-close is a no-op.
     *
     * WHY free(heap_buf) before fclose(out)?
     *   Order doesn't matter here (neither depends on the other), but
     *   freeing in reverse allocation order (heap_buf allocated after rx,
     *   freed before fclose which was opened before rx) is the idiomatic
     *   pattern and makes code review easier.
     */
    tst_raw_receiver_close(rx);
    free(heap_buf);
    fclose(out);

    fprintf(stdout,
            "wrote %" PRIu64 " bytes (%" PRIu64 " messages) to %s"
            " (%" PRIu64 " oversized messages lost)\n",
            total_bytes, total_msgs, argv[1], lost_msgs);
    return 0;
}
