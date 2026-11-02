/*
 * send_tcp.c — Minimal raw TCP sender: pre-built TS packets over plain TCP.
 *
 * Demonstrates the lowest-level TCP send API: `tst_tcp_sender_t`. This is
 * the "raw TS bytes" path — the caller provides 188-byte-aligned MPEG-TS
 * packets and the library writes them to the TCP socket directly (no RTP
 * header, unlike `tst_rtp_sender_t`). No mux config, no encoder state, no
 * stream handles.
 *
 * This is distinct from `tst_tcp_mux_sender_t` (which owns a full MPEG-TS
 * muxer and accepts encoded NAL / KLV / audio frames). Use `tst_tcp_sender_t`
 * when your upstream already produces TS packets — for example a hardware
 * encoder, a relay from a file, or a downstream repackager.
 *
 * Why TCP instead of UDP or SRT?
 *   Plain TCP/MPEG-TS is appropriate when:
 *   - You are on a reliable LAN or loopback and do not need retransmission.
 *   - Your receiver is a legacy system (e.g., an embedded decoder) that speaks
 *     raw TCP bytestream rather than UDP datagrams or SRT.
 *   - You want byte-stream framing without the overhead of RTP sequencing.
 *   TCP provides in-order, loss-free delivery at the cost of head-of-line
 *   blocking. For mission-critical UAV / ISR video over unreliable links,
 *   prefer SRT (retransmission + ARQ) or RTP over UDP (jitter reordering).
 *
 * What this example shows:
 *   1. CLI arg parsing: --dest host:port (e.g. 127.0.0.1:7001).
 *   2. Building a tcp:// URL with an optional nodelay query param.
 *   3. Opening a tst_tcp_sender_t and checking for NULL + last-error.
 *   4. Synthesising structurally valid 188-byte TS null packets as payload.
 *   5. Pushing 100 of those packets via tst_tcp_sender_send_ts.
 *   6. Flushing the buffered tail with tst_tcp_sender_flush, then closing via
 *      tst_tcp_sender_close.
 *
 * No cancel API on TCP handles (the TCP transport does not expose a cancel
 * handle). A single-threaded sender like this one simply runs its loop to
 * completion and calls tst_tcp_sender_close. To interrupt a blocked send from
 * another thread, close the handle from the sending thread or rely on the
 * socket's send-side behavior (e.g., a TCP RST from the peer).
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c --no-default-features --features tcp
 *   gcc -I target/debug/include \
 *       -L target/debug \
 *       -Wall -Werror \
 *       -o /tmp/send_tcp \
 *       bindings/c/examples/sending/send_tcp.c \
 *       -ltstrans -lpthread -ldl
 *
 * Run (you need a listening receiver — see recv_tcp.c or use nc -l 7001):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_tcp --dest 127.0.0.1:7001
 *
 * Receive with nc (in a separate terminal):
 *   nc -l 7001 | xxd | head
 *
 * Receive with the sibling example:
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_tcp tcp://0.0.0.0:7001
 *
 * Requires: TST_HAS_TCP == 1 (set when the `tcp` cargo feature is enabled).
 *
 * Mirrors: examples/sending/send_udp.c (UDP variant) and the sibling
 *          send_rtp.c C example.
 */

#include "tstrans.h"

#if !defined(TST_HAS_TCP) || TST_HAS_TCP == 0
#error "This example requires TST_HAS_TCP. Rebuild tst-c with the tcp cargo feature enabled."
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * MPEG-TS packet size: exactly 188 bytes (ISO/IEC 13818-1 §2.4.3.2).
 * Over TCP the library writes all 188 bytes per `send_ts` call. The
 * receiver re-synchronises on the 0x47 sync byte if framing is lost.
 */
#define TS_PACKET_SIZE   188

/*
 * MPEG-TS sync byte: 0x47. The first byte of every TS packet.
 */
#define TS_SYNC_BYTE     0x47

/*
 * Null PID (0x1FFF): every conformant receiver silently discards null
 * packets per H.222.0 §2.4.4.4 — safe synthetic payload for this example.
 */
#define TS_NULL_PID_HI   0x1F
#define TS_NULL_PID_LO   0xFF

/*
 * Number of TS packets to send. 100 × 188 = 18,800 bytes.
 */
#define SEND_COUNT       100

/* Default destination — connect to a loopback listener on port 7001. */
#define DEFAULT_DEST "127.0.0.1:7001"

/* ── Helper: synthesize one null TS packet ─────────────────────────────── */

/*
 * make_null_ts_packet — fill `buf` (exactly TS_PACKET_SIZE bytes) with a
 * valid MPEG-TS null packet (PID 0x1FFF, no adaptation field, payload-only).
 *
 * TS header layout (4 bytes):
 *   byte[0]: sync_byte                         = 0x47
 *   byte[1]: transport_error_indicator(1b)=0
 *            payload_unit_start_indicator(1b)=0
 *            transport_priority(1b)=0
 *            PID[12:8](5b) = 0x1F (null PID high 5 bits)
 *   byte[2]: PID[7:0](8b) = 0xFF (null PID low 8 bits)
 *   byte[3]: transport_scrambling_control(2b)=00
 *            adaptation_field_control(2b)=01  (payload only, no adaptation)
 *            continuity_counter(4b)=0000
 */
static void make_null_ts_packet(uint8_t *buf) {
    buf[0] = TS_SYNC_BYTE;
    buf[1] = TS_NULL_PID_HI;
    buf[2] = TS_NULL_PID_LO;
    buf[3] = 0x10;             /* adaptation_field_control=01 (payload only) */
    memset(buf + 4, 0xFF, TS_PACKET_SIZE - 4);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * ── Step 1: Parse CLI args ────────────────────────────────────────────
     *
     * Accept --dest host:port. The host must be a plain TCP listener address
     * (unicast only — TCP is point-to-point; there is no multicast).
     */
    const char *dest = DEFAULT_DEST;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dest") == 0 && i + 1 < argc) {
            dest = argv[++i];
        } else if (argv[i][0] != '-') {
            dest = argv[i];
        } else {
            fprintf(stderr,
                    "usage: send_tcp [--dest host:port]\n"
                    "  Default dest: %s\n"
                    "  Requires a TCP listener at the given address.\n",
                    DEFAULT_DEST);
            return 1;
        }
    }

    /*
     * ── Step 2: Build the tcp:// URL ─────────────────────────────────────
     *
     * The URL scheme routes to the raw-TCP caller path. `?nodelay=1`
     * disables Nagle's algorithm so small TS frames are sent immediately
     * without waiting for more data — important for low-latency streaming.
     *
     * WHY nodelay for MPEG-TS?
     *   Each tst_tcp_sender_send_ts call writes exactly 188 bytes. Without
     *   TCP_NODELAY, Nagle would buffer these until either the ACK for the
     *   previous batch arrives or a full MSS is accumulated. For real-time
     *   video where individual TS packets carry PTS/DTS timing, Nagle adds
     *   latency. Setting nodelay=1 avoids this at the cost of slightly more
     *   network frames on high-bandwidth streams.
     */
    char url[256];
    snprintf(url, sizeof(url), "tcp://%s?nodelay=1", dest);
    fprintf(stderr, "[send_tcp] connecting to: %s\n", url);

    /*
     * ── Step 3: Open the TCP sender ──────────────────────────────────────
     *
     * tst_tcp_sender_open creates a TCP socket and connects to the
     * destination synchronously (blocking until the 3-way handshake
     * completes or the connect timeout fires). Default timeout is 10s;
     * override via ?connect_timeout=Ns.
     *
     * Returns NULL on failure. The last-error thread-local is set; call
     * tst_get_last_error_str() immediately to retrieve the human-readable
     * message.
     *
     * Common failure codes:
     *   TST_E_TCP_IO     (-30) — connection refused or I/O error
     *   TST_E_TCP_CONFIG (-31) — malformed URL
     *   TST_E_TCP_CONNECT_TIMEOUT (-32) — connect timed out
     *
     * NOTE: tst_tcp_sender_t is the "raw TS bytes in" shape. If you want
     * the library to mux NAL units / KLV / audio into TS for you, use
     * tst_tcp_mux_sender_open instead — same URL format, richer push API.
     */
    TstTcpSender *sender = tst_tcp_sender_open(url);
    if (!sender) {
        fprintf(stderr, "[send_tcp] tst_tcp_sender_open failed: %s\n",
                tst_get_last_error_str());
        return 2;
    }
    fprintf(stderr, "[send_tcp] connected; pushing %d TS packets\n", SEND_COUNT);

    /*
     * ── Step 4: Build a synthetic TS packet ──────────────────────────────
     *
     * One null TS packet reused for every iteration. In a real application
     * this buffer would be filled by a hardware encoder's DMA output, a
     * libtstrans muxer drain, or a file relay. send_ts copies the bytes
     * before returning, so mutating the buffer after the call is safe.
     */
    uint8_t ts_pkt[TS_PACKET_SIZE];
    make_null_ts_packet(ts_pkt);

    /*
     * ── Step 5: Push 100 TS packets ──────────────────────────────────────
     *
     * tst_tcp_sender_send_ts writes all `len` bytes to the TCP socket
     * before returning (or returns an error). TCP is a reliable bytestream:
     * the receiver gets exactly these bytes in order. Returns 0 on success,
     * a negative TST_E_* code on failure.
     *
     * Passing exactly TS_PACKET_SIZE per call is correct; for throughput-
     * oriented code, batch multiple packets per call (up to pkt_size, which
     * defaults to 64 KiB).
     */
    int exit_code = 0;
    for (int i = 0; i < SEND_COUNT; i++) {
        int rc = tst_tcp_sender_send_ts(sender, ts_pkt, TS_PACKET_SIZE);
        if (rc != 0) {
            fprintf(stderr,
                    "[send_tcp] tst_tcp_sender_send_ts[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
    }

    if (exit_code == 0) {
        fprintf(stderr,
                "[send_tcp] sent %d TS packets (%d bytes) successfully.\n",
                SEND_COUNT, SEND_COUNT * TS_PACKET_SIZE);
    }

    /*
     * ── Step 6: Close the sender ─────────────────────────────────────────
     *
     * Flush first. The sender sends one datagram per seven-packet bundle,
     * so 100 packets leave as 14 bundles and the last two stay buffered.
     * tst_tcp_sender_close is cancel-first and abortive: it discards that
     * buffered tail, so tst_tcp_sender_flush must run before it for a
     * lossless end of stream. close then closes the socket and frees all
     * associated memory; it is safe on a valid non-NULL pointer at any time,
     * including after a send failure.
     */
    if (exit_code == 0) {
        int flush_rc = tst_tcp_sender_flush(sender);
        if (flush_rc != 0) {
            fprintf(stderr, "[send_tcp] tst_tcp_sender_flush failed (rc=%d): %s\n",
                    flush_rc, tst_get_last_error_str());
            exit_code = 1;
        }
    }
    tst_tcp_sender_close(sender);
    fprintf(stderr, "[send_tcp] sender closed.\n");

    return exit_code;
}
