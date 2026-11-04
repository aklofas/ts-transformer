/*
 * recv_tcp.c — Bind a TCP listener, accept one connection, receive
 * raw 188-byte MPEG-TS packets, and log each packet's TS header fields.
 *
 * Demonstrates the TCP receive API with a listener:
 *   `tst_tcp_listener_t`  — binds the server socket and accepts connections.
 *   `tst_tcp_receiver_t`  — raw TS bytes out, one 188-byte packet per call.
 *
 * For the typed-event demux path (PMT / Sample / Metadata / KLV events) use
 * `tst_tcp_demux_receiver_t` + `tst_tcp_demux_receiver_next_event` instead.
 * This example stays at the raw-packet level so it works against any TCP
 * MPEG-TS sender (ffmpeg, VLC, or the sibling send_tcp.c example).
 *
 * Why TCP listener vs. TCP caller?
 *   The listener pattern is common for ground-station software waiting for
 *   platform-side senders to dial in. The platform (UAV, aircraft, EO/IR
 *   pod) is the initiator; the ground station binds a well-known port.
 *   The `tst_tcp_listener_t` wraps `std::net::TcpListener` and yields a
 *   `tst_tcp_receiver_t` per accepted connection. Only one connection is
 *   accepted in this single-threaded example; a multi-client server would
 *   loop calling `tst_tcp_listener_accept_receiver` repeatedly.
 *
 * What this example shows:
 *   1. CLI arg parsing: a TCP listener URL (tcp://0.0.0.0:7001) or
 *      address:port (0.0.0.0:7001) for bind.
 *   2. Binding via tst_tcp_listener_from_url and checking for NULL + last-error.
 *   3. Blocking accept via tst_tcp_listener_accept_receiver.
 *   4. A blocking receive loop driven by tst_tcp_receiver_recv_ts.
 *   5. Decoding and logging each TS packet's header fields (sync byte, PID,
 *      payload_unit_start_indicator, continuity counter).
 *   6. Clean shutdown on END_OF_STREAM (sender closed) or packet cap.
 *   7. Freeing the listener and receiver handles.
 *
 * Note on shutdown: tst_tcp_receiver_cancel exists and may be called from
 * another thread to end a parked recv, which then returns TST_E_CLOSED. This
 * single-threaded example needs none: it stops after a fixed packet count (or
 * when END_OF_STREAM fires) and closes the handle with no call in flight, so no cancel is needed.
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c --no-default-features --features tcp
 *   gcc -I target/debug/include \
 *       -L target/debug \
 *       -Wl,-rpath,target/debug \
 *       -Wall -Wextra -Werror \
 *       -o /tmp/recv_tcp \
 *       bindings/c/examples/receiving/recv_tcp.c \
 *       -ltstrans -lpthread -ldl
 *
 * Run (listen on port 7001, any NIC):
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_tcp tcp://0.0.0.0:7001?listen=1
 *
 * Pair with the sender example (in another terminal after this is running):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_tcp --dest 127.0.0.1:7001
 *
 * Also works with ffmpeg:
 *   ffmpeg -re -i input.ts -f mpegts tcp://127.0.0.1:7001
 *
 * Requires: TST_HAS_TCP == 1 (set when the `tcp` cargo feature is enabled).
 *
 * Closest sibling: receiving/recv_udp.c (UDP raw recv variant).
 */

#include "tstrans.h"

#if !defined(TST_HAS_TCP) || TST_HAS_TCP == 0
#error "This example requires TST_HAS_TCP. Rebuild tst-c with the tcp cargo feature enabled."
#endif

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/* MPEG-TS packet size: exactly 188 bytes (ISO/IEC 13818-1 §2.4.3.2). */
#define TS_PACKET_SIZE   188

/* MPEG-TS sync byte: 0x47, the first byte of every TS packet. */
#define TS_SYNC_BYTE     0x47

/*
 * Stop after this many packets so the example terminates deterministically
 * when paired with send_tcp (which sends 100). A real receiver would loop
 * until END_OF_STREAM or an external shutdown signal.
 */
#define MAX_PACKETS      100

/* ── Helper: decode + print one TS header ──────────────────────────────── */

/*
 * print_ts_header — decode the 4-byte MPEG-TS header from `pkt` and log it.
 *
 * TS header layout (ISO/IEC 13818-1 §2.4.3.2):
 *   byte[0]: sync_byte (0x47)
 *   byte[1]: transport_error_indicator(1b) | payload_unit_start_indicator(1b)
 *            | transport_priority(1b) | PID[12:8](5b)
 *   byte[2]: PID[7:0](8b)
 *   byte[3]: transport_scrambling_control(2b) | adaptation_field_control(2b)
 *            | continuity_counter(4b)
 *
 *   - PID is the 13-bit packet identifier ((byte[1] & 0x1F) << 8 | byte[2]).
 *     PID 0x0000 is the PAT; 0x1FFF is the null PID (discard); program PIDs
 *     are advertised in the PAT/PMT tables.
 *   - payload_unit_start_indicator (PUSI) = 1 marks the start of a PES or
 *     PSI section — the natural boundary for higher-level parsers.
 *   - continuity_counter (0..15) increments per PID; a gap signals packet loss.
 *     Over TCP, loss should not happen (TCP guarantees in-order delivery),
 *     but framing errors (byte shifts, sync loss) can cause gaps.
 */
static void print_ts_header(uint64_t index, const uint8_t *pkt) {
    uint8_t  sync = pkt[0];
    uint8_t  tei  = (pkt[1] >> 7) & 0x01;  /* transport_error_indicator */
    uint8_t  pusi = (pkt[1] >> 6) & 0x01;  /* payload_unit_start_indicator */
    uint16_t pid  = (uint16_t)(((pkt[1] & 0x1F) << 8) | pkt[2]);
    uint8_t  afc  = (pkt[3] >> 4) & 0x03;  /* adaptation_field_control */
    uint8_t  cc   = pkt[3] & 0x0F;         /* continuity_counter */

    if (sync != TS_SYNC_BYTE) {
        fprintf(stderr,
                "[tcp_recv] pkt %" PRIu64 ": BAD sync byte 0x%02x (expected 0x47)"
                " — framing error on TCP bytestream\n",
                index, sync);
        return;
    }

    fprintf(stdout,
            "[tcp_recv] pkt %" PRIu64 ": pid=0x%04x pusi=%u tei=%u afc=%u cc=%u\n",
            index, pid, pusi, tei, afc, cc);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
                "Usage: %s <url>\n"
                "\n"
                "  url  TCP listener URL, e.g.:\n"
                "         tcp://0.0.0.0:7001?listen=1   (any NIC, port 7001)\n"
                "         tcp://127.0.0.1:0?listen=1    (loopback, kernel port)\n"
                "\n"
                "Accepts one connection, receives up to %d TS packets, then exits.\n"
                "\n"
                "WHY a listener (server) here rather than a caller (client)?\n"
                "  In ISR/UAV deployments the ground station binds a well-known port\n"
                "  and waits for the sensor platform to dial in. The platform is the\n"
                "  initiator; the ground station is the passive listener.\n"
                "  For the caller (connect) shape, use tst_tcp_recv_open instead.\n",
                argv[0], MAX_PACKETS);
        return 1;
    }
    const char *url = argv[1];

    /*
     * ── Step 1: Bind the TCP listener ────────────────────────────────────
     *
     * tst_tcp_listener_from_url requires the URL to include ?listen=1.
     * This is the canonical way to create a listener: parse the URL,
     * bind the socket (SO_REUSEADDR), and return a handle.
     *
     * Alternative: tst_tcp_listener_bind("0.0.0.0:7001") for a raw
     * host:port without a URL scheme.
     *
     * Port 0 lets the kernel assign an ephemeral port (useful in tests
     * where you don't care which port is used).
     */
    TstTcpListener *listener = tst_tcp_listener_from_url(url);
    if (!listener) {
        fprintf(stderr,
                "tst_tcp_listener_from_url(\"%s\") failed: %s\n",
                url, tst_get_last_error_str());
        return 1;
    }
    fprintf(stderr, "[tcp_recv] listener bound: %s\n", url);

    /*
     * ── Step 2: Accept one connection ────────────────────────────────────
     *
     * tst_tcp_listener_accept_receiver blocks until a TCP client connects,
     * then returns a tst_tcp_receiver_t wrapping the accepted socket.
     *
     * The listener is NOT consumed — you can call accept_receiver again for
     * a second client. In this single-client example we accept once and
     * proceed to the receive loop.
     *
     * tst_tcp_listener_accept_sender returns a tst_tcp_sender_t instead —
     * use that when the listener side is the TS byte producer (push model).
     */
    fprintf(stderr, "[tcp_recv] waiting for connection...\n");
    TstTcpReceiver *rx = tst_tcp_listener_accept_receiver(listener);
    if (!rx) {
        fprintf(stderr,
                "tst_tcp_listener_accept_receiver failed: %s\n",
                tst_get_last_error_str());
        tst_tcp_listener_free(listener);
        return 1;
    }
    fprintf(stderr, "[tcp_recv] connection accepted. Waiting for TS packets (up to %d)...\n",
            MAX_PACKETS);

    /*
     * ── Step 3: Receive loop ─────────────────────────────────────────────
     *
     * tst_tcp_receiver_recv_ts blocks until one 188-byte TS packet is ready,
     * then copies it into our buffer and sets *out_n to the byte count
     * (always 188 on success). Over TCP, "one packet" means the library has
     * buffered 188 bytes of the bytestream — it synchronises on the 0x47
     * sync byte.
     *
     * The buffer must be at least 188 bytes or the call returns
     * TST_E_INVALID_CONFIG.
     *
     * Common return codes:
     *   0                     — success, *out_n = 188
     *   TST_E_END_OF_STREAM   — peer sent a FIN (connection closed cleanly)
     *   TST_E_CLOSED          — this handle was closed from another thread
     *   TST_E_TRANSPORT       — TCP I/O error (RST, network failure)
     *   TST_E_INVALID_CONFIG  — null pointer or too-small buffer
     */
    uint8_t buf[TS_PACKET_SIZE];
    uint64_t received = 0;
    int exit_code = 0;

    while (received < MAX_PACKETS) {
        size_t n = 0;
        int rc = tst_tcp_receiver_recv_ts(rx, buf, sizeof(buf), &n);

        if (rc == 0) {
            /* n is always TS_PACKET_SIZE on success; decode + log the header. */
            print_ts_header(received, buf);
            received += 1;
            fflush(stdout);
            continue;
        }

        if (rc == TST_E_END_OF_STREAM) {
            /*
             * The TCP sender closed its end (FIN received). On TCP this is
             * the normal terminal condition for a send-then-close sender.
             * Unlike UDP there is no concept of "multicast group empty" here.
             */
            fprintf(stderr,
                    "[tcp_recv] stream ended (sender closed). %" PRIu64 " packets received.\n",
                    received);
            break;
        }

        if (rc == TST_E_CLOSED) {
            /* Another thread closed this handle. Clean exit. */
            fprintf(stderr,
                    "[tcp_recv] receiver closed. %" PRIu64 " packets received.\n",
                    received);
            break;
        }

        /*
         * Any other negative rc indicates an unrecoverable transport error.
         * Log the code + the thread-local error string and exit non-zero.
         *
         * Common codes:
         *   TST_E_TCP_IO     (-30) — TCP socket error (RST, broken pipe)
         *   TST_E_TRANSPORT  (-8)  — generic transport failure
         */
        fprintf(stderr,
                "[tcp_recv] tst_tcp_receiver_recv_ts failed (rc=%d): %s\n",
                rc, tst_get_last_error_str());
        exit_code = 2;
        break;
    }

    if (received >= MAX_PACKETS) {
        fprintf(stderr,
                "[tcp_recv] reached packet cap. %" PRIu64 " packets received.\n",
                received);
    }

    /*
     * ── Step 4: Close handles ────────────────────────────────────────────
     *
     * tst_tcp_receiver_close closes the accepted TCP socket and frees the
     * handle. tst_tcp_listener_free closes the listening socket and frees
     * the listener. Both are safe to call with NULL (no-op).
     *
     * Order matters: close the per-connection handle before (or after) the
     * listener — they are independent resources. Here we close the receiver
     * first to signal the peer, then close the listener.
     */
    tst_tcp_receiver_close(rx);
    tst_tcp_listener_free(listener);
    fprintf(stderr, "[tcp_recv] closed.\n");

    return exit_code;
}
