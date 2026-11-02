/*
 * recv_rist.c — Minimal RIST receiver: pull TS packets, log their headers.
 *
 * Demonstrates the lowest-level RIST receive API: `tst_rist_receiver_t`.
 * This is the "raw TS bytes out" path — the library pulls 188-byte MPEG-TS
 * packets from a librist receiver and copies them one at a time to the
 * caller's buffer.
 *
 * WHY RIST receive vs plain UDP receive?
 *   Unlike plain UDP, RIST ARQ retransmission means out-of-order or
 *   initially lost packets may arrive via NACK + retransmit before the
 *   recovery buffer expires. This example's simple pull-loop works the
 *   same way for both transports because tst_rist_receiver_recv_ts blocks
 *   until one reassembled, in-order packet is ready, hiding ARQ latency.
 *   You only notice RIST's reliability benefit on a lossy link: null
 *   packets in the stream drop to near-zero on 1–5% link-loss paths
 *   (within the recovery buffer window), whereas plain UDP shows the
 *   raw loss rate directly.
 *
 * RIST receiver URL convention:
 *   RIST receiver URLs use the ffmpeg `@` bind-prefix convention:
 *     rist://@host:port   → bind on host:port and wait for a sender to connect
 *   This mirrors tst_udp_recv_open (`udp://@group:port` for multicast recv)
 *   and tst_tcp_recv_open (`tcp://@host:port` for TCP listen).
 *
 * RIST profiles recap:
 *   Simple Profile: rist://@0.0.0.0:8000                 (no encryption)
 *   Main Profile:   rist://@0.0.0.0:8000?profile=main    (no encryption)
 *   Encrypted Main: rist://@0.0.0.0:8000?aes-type=256&secret=<psk>&buffer=200
 *     Requires `mbedtls` cargo feature (default-on). Returns
 *     TST_E_RIST_ENCRYPTION_DISABLED (-41) if absent.
 *
 * Recovery buffer (`?buffer=N` ms):
 *   Sets the RIST ARQ receive window — how long the receiver waits for
 *   retransmitted packets before declaring them lost. Match this to the
 *   sender's `?buffer=` value. Both sides need the same window for ARQ
 *   to work correctly. Default: 200 ms (terrestrial links).
 *
 * What this example shows:
 *   1. CLI arg parsing: --bind host:port (defaults to 0.0.0.0:8000).
 *   2. Building a rist://@host:port URL with buffer + profile query params.
 *   3. Opening a tst_rist_receiver_t and checking for NULL + last-error.
 *   4. Pulling packets in a loop via tst_rist_receiver_recv_ts.
 *   5. Logging each 188-byte packet's PID from the 4-byte TS header.
 *   6. Exiting cleanly on EOF / peer close.
 *
 * Build (from the ts-transformer workspace root):
 *   RIST_FORCE_VENDORED=1 cargo build -p tst-c --no-default-features --features rist
 *   gcc -I target/debug/include \
 *       -L target/debug \
 *       -Wall -Wextra -Wpedantic \
 *       -o /tmp/recv_rist \
 *       bindings/c/examples/receiving/recv_rist.c -ltstrans -lpthread -ldl
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_rist
 *
 * Run (bind on all interfaces, port 8000):
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_rist
 *
 * Run with explicit bind address:
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_rist --bind 192.168.1.50:8000
 *
 * Send to this receiver from another terminal:
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rist --dest 127.0.0.1:8000
 *   or: ffmpeg -re -i input.ts -c copy -f mpegts rist://127.0.0.1:8000?buffer=200
 *
 * Requires: TST_HAS_RIST == 1 (set when the `rist` cargo feature is enabled).
 *
 * Mirrors: examples/receiving/recv_udp.c (UDP variant) and
 *          examples/receiving/recv_rtp.c (RTP variant).
 */

#include "tstrans.h"

#if !defined(TST_HAS_RIST) || TST_HAS_RIST == 0
#error "This example requires TST_HAS_RIST. Rebuild tst-c with the rist cargo feature enabled."
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * MPEG-TS packet size: exactly 188 bytes (ISO/IEC 13818-1 §2.4.3.2).
 * tst_rist_receiver_recv_ts always delivers exactly this many bytes per call.
 */
#define TS_PACKET_SIZE     188

/*
 * Maximum number of TS packets to print full-header details for.
 * After this limit the receiver continues pulling but only logs counts,
 * preventing the terminal from scrolling infinitely on a live stream.
 */
#define PRINT_LIMIT        20

/* Default RIST bind address. */
#define DEFAULT_BIND       "0.0.0.0:8000"

/*
 * Recovery buffer in milliseconds (must match the sender's ?buffer= value).
 * 200 ms is the standard terrestrial default. Increase for lossy RF links.
 */
#define DEFAULT_BUFFER_MS  "200"

/* ── Helper: decode the 13-bit PID from bytes 1-2 of a TS header ────────── */

/*
 * ts_pid — extract the 13-bit PID from a raw 188-byte MPEG-TS packet.
 *
 * TS header layout:
 *   byte[0]: sync_byte (should be 0x47)
 *   byte[1]: TEI(1b) | PUSI(1b) | priority(1b) | PID[12:8](5b)
 *   byte[2]: PID[7:0](8b)
 *
 * WHY mask byte[1] with 0x1F?
 *   The upper 3 bits are the transport_error_indicator, PUSI, and priority
 *   flags, not part of the PID. Masking isolates the 5 high PID bits.
 */
static uint16_t ts_pid(const uint8_t *pkt) {
    return (uint16_t)(((pkt[1] & 0x1Fu) << 8) | pkt[2]);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * ── Step 1: Parse CLI args ────────────────────────────────────────────
     *
     * Accept --bind host:port (or bare positional host:port).
     * Bind on 0.0.0.0 to accept connections from any sender on the network.
     * Bind on a specific address (e.g. 192.168.1.50) to restrict to one NIC.
     */
    const char *bind_addr = DEFAULT_BIND;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--bind") == 0 && i + 1 < argc) {
            bind_addr = argv[++i];
        } else if (argv[i][0] != '-') {
            bind_addr = argv[i];
        } else {
            fprintf(stderr,
                    "usage: recv_rist [--bind host:port]\n"
                    "  Default bind: %s\n"
                    "  Specific NIC: --bind 192.168.1.50:8000\n",
                    DEFAULT_BIND);
            return 1;
        }
    }

    /*
     * ── Step 2: Build the rist:// bind URL ───────────────────────────────
     *
     * RIST receiver URLs use the ffmpeg `@` convention: `rist://@host:port`.
     * The `@` prefix tells the library (and librist) to bind + listen on this
     * address, rather than connect to a remote sender.
     *
     * WHY `@` instead of a separate bind flag?
     *   The `@` convention is established by FFmpeg and adopted across the
     *   tst-c transport family (UDP, TCP, RIST) for consistency. Any code
     *   that handles RIST URLs can detect bind vs send from the URL alone.
     *
     * To add Main Profile encryption (requires mbedtls feature), append:
     *   &aes-type=256&secret=my-pre-shared-key&profile=main
     * Both sender and receiver must use the same AES type + secret.
     */
    char url[512];
    snprintf(url, sizeof(url), "rist://@%s?buffer=" DEFAULT_BUFFER_MS, bind_addr);
    fprintf(stderr, "[recv_rist] binding on: %s\n", url);
    fprintf(stderr, "[recv_rist] waiting for a RIST sender to connect...\n");

    /*
     * ── Step 3: Open the RIST receiver ───────────────────────────────────
     *
     * tst_rist_recv_open parses the URL, calls
     *   RistRecvTransportBuilder::new(url)?.listen()
     * to initialise a librist receiver context and bind the socket, then
     * wraps it in a Receiver<RistRecvTransport> pipeline shell.
     *
     * Returns NULL on failure. Common failures:
     *   TST_E_RIST_CONFIG (-39) — bad URL, missing `@` prefix, or bad AES type
     *   TST_E_RIST_FFI (-38)   — librist context creation failed (port in use?)
     *   TST_E_RIST_ENCRYPTION_DISABLED (-41) — AES requested, mbedtls absent
     *
     * NOTE: tst_rist_receiver_t is the "raw TS bytes out" shape. If you want
     * the library to also demux the TS into typed events (video samples, KLV
     * metadata, audio frames), use tst_rist_demux_receiver_open instead —
     * same URL format, event-driven API.
     */
    TstRistReceiver *receiver = tst_rist_recv_open(url);
    if (!receiver) {
        fprintf(stderr,
                "[recv_rist] tst_rist_recv_open failed (code=%d): %s\n",
                tst_get_last_error(), tst_get_last_error_str());
        return 2;
    }

    /*
     * ── Step 4: Pull TS packets in a loop ────────────────────────────────
     *
     * tst_rist_receiver_recv_ts blocks until one 188-byte MPEG-TS packet is
     * ready (accounting for RIST ARQ retransmission within the buffer window),
     * then copies it into `buf` and sets `*out_n` to 188.
     *
     * Return codes:
     *   0                    → success; buf[0..188) contains one TS packet.
     *   TST_E_END_OF_STREAM  → peer closed gracefully; stop pulling.
     *   TST_E_CLOSED         → handle was closed from another thread.
     *   TST_E_TRANSPORT      → transport-layer failure (librist I/O error).
     *   TST_E_INVALID_CONFIG → null buf or buf_len < 188 (programming error).
     *
     *   TST_E_BUFFER_FULL    → nothing arrived within one ~100 ms poll; retry.
     *
     * WHY is the RIST receive a poll rather than a blocking call?
     *   Each call waits about 100 ms for a packet and returns
     *   TST_E_BUFFER_FULL when none came, so a single-threaded loop stays
     *   responsive and a cancel from another thread ends it at the next
     *   tick. The loop below simply asks again. The session timeout (network
     *   silence) can be configured via `?session_timeout=N` ms on the bind URL.
     */
    uint8_t buf[TS_PACKET_SIZE];
    size_t out_n = 0;
    long long total_packets = 0;
    int exit_code = 0;

    while (1) {
        int rc = tst_rist_receiver_recv_ts(receiver, buf, sizeof(buf), &out_n);

        if (rc == TST_E_END_OF_STREAM) {
            /*
             * Sender closed the RIST session (sent RTCP BYE or dropped).
             * This is the normal loop-exit path for a finite sender.
             */
            fprintf(stderr,
                    "[recv_rist] stream ended (TST_E_END_OF_STREAM); "
                    "total packets: %lld\n",
                    total_packets);
            break;
        }

        if (rc == TST_E_BUFFER_FULL) {
            /*
             * An idle poll tick, not an error: the RIST receive waits about
             * 100 ms for a packet and reports TST_E_BUFFER_FULL when none
             * arrived. The handle is live, so ask again. The documented start
             * order (receiver first) lands here on every tick until the
             * sender appears.
             */
            continue;
        }

        if (rc != 0) {
            /*
             * Unexpected error: log the code + message, then exit. Common cases:
             *   TST_E_TRANSPORT (-8) — librist I/O error (network reset, etc.)
             *   TST_E_CLOSED    (-7) — handle closed while recv was blocking
             */
            fprintf(stderr,
                    "[recv_rist] recv_ts error (rc=%d): %s\n",
                    rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }

        total_packets++;

        if (total_packets <= PRINT_LIMIT) {
            /*
             * Decode and print the 13-bit PID from the TS header bytes [1:2].
             * WHY PID logging?
             *   The PID identifies the elementary stream carried in this packet
             *   (PAT at 0x0000, PMT at configurable PID, video at e.g. 0x0100,
             *   audio at 0x0101, KLV at 0x0102, null at 0x1FFF). Logging PIDs
             *   lets the operator quickly confirm the expected program structure
             *   is present in the incoming stream.
             * WHY check sync byte?
             *   The TS sync byte (0x47) must be the first byte of every valid
             *   packet. If it is wrong the stream is corrupted or mis-framed.
             */
            uint8_t sync = buf[0];
            uint16_t pid = ts_pid(buf);
            fprintf(stderr,
                    "[recv_rist] pkt %3lld: sync=0x%02x PID=0x%04x%s\n",
                    total_packets, sync, pid,
                    sync != 0x47 ? " [BAD SYNC]" : "");
        } else if (total_packets == PRINT_LIMIT + 1) {
            fprintf(stderr,
                    "[recv_rist] (further packets suppressed; counting only)\n");
        }
    }

    if (exit_code == 0 && total_packets > 0) {
        fprintf(stderr,
                "[recv_rist] received %lld TS packets (%lld bytes) total.\n",
                total_packets, total_packets * (long long)TS_PACKET_SIZE);
    }

    /*
     * ── Step 5: Close the receiver ───────────────────────────────────────
     *
     * tst_rist_receiver_close sends RTCP BYE, shuts down the librist context,
     * and frees all associated memory. Safe to call on any valid non-NULL
     * pointer, including after a recv_ts failure.
     */
    tst_rist_receiver_close(receiver);
    fprintf(stderr, "[recv_rist] receiver closed.\n");

    return exit_code;
}
