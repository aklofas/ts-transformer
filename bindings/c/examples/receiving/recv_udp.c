/*
 * recv_udp.c — receive raw 188-byte MPEG-TS packets over plain UDP and
 * log each packet's TS header fields.
 *
 * Demonstrates the lowest-level UDP receive API: `tst_udp_receiver_t`. This is
 * the "raw TS bytes out" path — the library hands you complete 188-byte
 * MPEG-TS packets one at a time, exactly as they arrived in the UDP datagram
 * (no RTP de-sequencing, no demux). This is the twin of `tst_udp_sender_t`.
 *
 * For the typed-event demux path (PMT / Sample / Metadata / KLV events) use
 * `tst_udp_demux_receiver_t` + `tst_udp_demux_receiver_next_event` instead —
 * see the RTP sibling recv_rtp.c for that style. This example stays at
 * the raw-packet level so it works against any MPEG-TS-over-UDP source
 * (ffmpeg, VLC, a hardware encoder) with no assumptions about program
 * structure.
 *
 * Why plain UDP for gimbaled-platform video?
 *   Plain UDP/MPEG-TS is what `ffmpeg -f mpegts udp://...` emits and what
 *   many legacy ground stations and recorders consume. It has no
 *   retransmission (use SRT for that) and no RTP header (use RTP for jitter
 *   reordering). On a co-located tactical LAN where loss is rare it
 *   minimizes overhead and latency.
 *
 * What this example shows:
 *   1. Parsing a CLI URL argument (udp://0.0.0.0:5000 or
 *      udp://@239.1.2.3:5000?iface=eth0 for multicast).
 *   2. Opening a tst_udp_receiver_t and checking for NULL + last-error.
 *   3. A blocking receive loop driven by tst_udp_receiver_recv_ts.
 *   4. Decoding and logging each TS packet's header fields (sync byte, PID,
 *      payload_unit_start_indicator, continuity counter).
 *   5. Clean shutdown on END_OF_STREAM / CLOSED, and on reaching a packet cap.
 *
 * Note on shutdown: tst_udp_receiver_cancel exists and may be called from
 * another thread to end a parked recv, which then returns TST_E_CLOSED. This
 * single-threaded example needs none: it stops after a fixed packet count (or
 * when END_OF_STREAM fires) and closes the handle with no call in flight, so no cancel is needed.
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c --no-default-features --features udp
 *   cc -I target/debug/include \
 *      -L target/debug \
 *      -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror \
 *      -o /tmp/recv_udp \
 *      bindings/c/examples/receiving/recv_udp.c \
 *      -ltstrans -lpthread -ldl
 *
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_udp udp://0.0.0.0:5000
 *
 * Pair with the sender example (in another terminal):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_udp --dest 127.0.0.1:5000
 *
 * Requires: TST_HAS_UDP == 1 (set when the `udp` cargo feature is enabled).
 *
 * Closest sibling: receiving/recv_rtp.c (RTP + typed demux events).
 */

#include "tstrans.h"

#if !defined(TST_HAS_UDP) || TST_HAS_UDP == 0
#error "This example requires TST_HAS_UDP. Rebuild tst-c with the udp cargo feature enabled."
#endif

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/* MPEG-TS packet size: exactly 188 bytes (ISO/IEC 13818-1 §2.4.3.2). */
#define TS_PACKET_SIZE   188

/* MPEG-TS sync byte: 0x47, the first byte of every TS packet. */
#define TS_SYNC_BYTE     0x47

/*
 * Stop after this many packets so the example terminates deterministically
 * when paired with send_udp (which sends 100). A real receiver would loop
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
 *     PID 0x0000 is the PAT; 0x1FFF is the null PID; program-specific PIDs
 *     are advertised in the PAT/PMT.
 *   - payload_unit_start_indicator (PUSI) = 1 marks the first TS packet of a
 *     PES packet or PSI section — the natural boundary for parsing.
 *   - continuity_counter increments 0..15 per PID; a gap signals packet loss.
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
                "[udp_recv] pkt %" PRIu64 ": BAD sync byte 0x%02x (expected 0x47)\n",
                index, sync);
        return;
    }

    fprintf(stdout,
            "[udp_recv] pkt %" PRIu64 ": pid=0x%04x pusi=%u tei=%u afc=%u cc=%u\n",
            index, pid, pusi, tei, afc, cc);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr,
                "Usage: %s <url>\n"
                "\n"
                "  url  UDP source URL, e.g.:\n"
                "         udp://0.0.0.0:5000                 (unicast bind, any NIC)\n"
                "         udp://@239.1.2.3:5000?iface=eth0   (multicast join)\n"
                "\n"
                "Receives up to %d TS packets, then exits.\n",
                argv[0], MAX_PACKETS);
        return 1;
    }
    const char *url = argv[1];

    /*
     * ── Step 1: Open the raw UDP receiver ────────────────────────────────
     *
     * tst_udp_recv_open binds the socket (unicast) or joins the multicast
     * group on the requested interface. Port 0 lets the kernel assign an
     * ephemeral port. Returns NULL on failure with the last-error set.
     */
    TstUdpReceiver *rx = tst_udp_recv_open(url);
    if (!rx) {
        fprintf(stderr,
                "tst_udp_recv_open(\"%s\") failed: %s\n",
                url, tst_get_last_error_str());
        return 1;
    }

    fprintf(stderr, "Opened: %s\n", url);
    fprintf(stderr, "Waiting for MPEG-TS packets (up to %d)...\n", MAX_PACKETS);

    /*
     * ── Step 2: Receive loop ─────────────────────────────────────────────
     *
     * tst_udp_receiver_recv_ts blocks until one 188-byte TS packet is ready,
     * then copies it into our buffer and sets *out_n to the byte count
     * (always 188 on success). It is NOT a spin loop — it parks on the UDP
     * receive path, so idle CPU is effectively zero.
     *
     * The buffer must be at least 188 bytes or the call returns
     * TST_E_INVALID_CONFIG.
     */
    uint8_t buf[TS_PACKET_SIZE];
    uint64_t received = 0;
    int exit_code = 0;

    while (received < MAX_PACKETS) {
        size_t n = 0;
        int rc = tst_udp_receiver_recv_ts(rx, buf, sizeof(buf), &n);

        if (rc == 0) {
            /* n is always TS_PACKET_SIZE on success; decode + log the header. */
            print_ts_header(received, buf);
            received += 1;
            fflush(stdout);
            continue;
        }

        if (rc == TST_E_END_OF_STREAM) {
            /*
             * The sender closed the stream or the multicast group lost all
             * active senders. On UDP this is the normal terminal condition
             * (there is no caller-cancel path that would surface TST_E_CLOSED
             * here unless another thread closed the handle).
             */
            fprintf(stderr,
                    "Stream ended (no more packets). %" PRIu64 " packets received.\n",
                    received);
            break;
        }

        if (rc == TST_E_CLOSED) {
            /* Another thread closed the handle. Clean exit. */
            fprintf(stderr,
                    "Receiver closed. %" PRIu64 " packets received.\n",
                    received);
            break;
        }

        /*
         * Any other negative rc indicates an unrecoverable transport error.
         * Log the code + the thread-local error string and exit non-zero.
         *
         * Common codes:
         *   TST_E_UDP_IO         (-26) — UDP socket I/O error
         *   TST_E_TRANSPORT      (-8)  — generic transport failure
         *   TST_E_INVALID_CONFIG (-1)  — NULL/too-small buffer (programmer error)
         */
        fprintf(stderr,
                "tst_udp_receiver_recv_ts failed (rc=%d): %s\n",
                rc, tst_get_last_error_str());
        exit_code = 2;
        break;
    }

    if (received >= MAX_PACKETS) {
        fprintf(stderr,
                "Reached packet cap. %" PRIu64 " packets received.\n",
                received);
    }

    /*
     * ── Step 3: Close the receiver ───────────────────────────────────────
     *
     * tst_udp_receiver_close leaves any joined multicast group, closes the
     * socket, and frees all internal state. Safe to call with NULL (no-op),
     * but we know rx is non-NULL here.
     */
    tst_udp_receiver_close(rx);
    return exit_code;
}
