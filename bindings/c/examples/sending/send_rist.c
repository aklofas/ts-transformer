/*
 * send_rist.c — Minimal RIST sender: pre-built TS packets over RIST.
 *
 * Demonstrates the lowest-level RIST send API: `tst_rist_sender_t`. This is
 * the "raw TS bytes" path — the caller provides 188-byte-aligned MPEG-TS
 * packets and the library hands them to librist, which applies ARQ
 * (Automatic Repeat Request) retransmission and RTCP feedback automatically.
 *
 * WHY RIST (vs plain UDP, vs RTP, vs SRT) for gimbaled-platform video?
 *   RIST (Reliable Internet Stream Transport, VSF TR-06-1/2, SMPTE 2022-5/6)
 *   adds ARQ-based reliability to UDP without the proprietary stack of SRT.
 *   It is a published open standard — multiple vendors interoperate at the
 *   RIST receiver end. For tactical ISR (Intelligence, Surveillance,
 *   Reconnaissance) over RF links where the ground station may be a COTS
 *   product, RIST gives you reliability + interoperability without vendor
 *   lock-in. Use RIST when:
 *     - The far end is a third-party product that speaks RIST (not SRT).
 *     - Link loss is 1–5% and you need clean-decoded video.
 *     - You need a standards-body specification (VSF TR-06-1 / RFC 8083).
 *   For proprietary ground-station infrastructure where both ends are under
 *   your control, SRT is equally valid and has wider GStreamer/ffmpeg support.
 *
 * RIST profiles:
 *   Simple Profile (RIST SP, VSF TR-06-1): UDP + RTCP-based ARQ retransmit.
 *     URL: rist://host:port (no auth, no encryption).
 *   Main Profile (RIST MP, VSF TR-06-2): adds PSK encryption (AES-128/192/256
 *     via DTLS + libsrtp or mbedTLS), GRE tunnelling, and extended headers.
 *     URL: rist://host:port?profile=main&aes-type=256&secret=<psk>&buffer=200
 *     NOTE: Main Profile encryption requires librist built with mbedTLS
 *     (the `mbedtls` cargo feature, default-on). Building without mbedtls
 *     returns TST_E_RIST_ENCRYPTION_DISABLED (-41).
 *
 * Recovery buffer (`?buffer=N` ms):
 *   The buffer parameter sets the ARQ retransmission window on the sender
 *   side — how long the sender caches sent packets for potential NACK-driven
 *   retransmit. The receiver uses the same window to request missing packets.
 *   Larger values tolerate higher-RTT links; typical values:
 *     200 ms — terrestrial LAN / Wi-Fi
 *     400 ms — lossy RF (C-band, Ku-band)
 *     800 ms — high-latency satellite
 *   Minimum latency is approximately `buffer` ms (ARQ window = playout delay).
 *
 * What this example shows:
 *   1. CLI arg parsing: --dest host:port (unicast).
 *   2. Building a rist:// URL with buffer + profile query params.
 *   3. Opening a tst_rist_sender_t and checking for NULL + last-error.
 *   4. Synthesising structurally valid 188-byte TS null packets as "payload".
 *   5. Pushing 100 of those packets via tst_rist_sender_send_ts.
 *   6. Flushing the buffered tail with tst_rist_sender_flush, then closing via
 *      tst_rist_sender_close.
 *
 * Encrypted variant — use this URL instead (requires mbedtls feature):
 *   rist://host:8000?aes-type=256&secret=my-pre-shared-key&buffer=200&profile=main
 *   Both sender and receiver must use the same aes-type + secret.
 *
 * Build (from the ts-transformer workspace root):
 *   RIST_FORCE_VENDORED=1 cargo build -p tst-c --no-default-features --features rist
 *   gcc -I target/debug/include \
 *       -L target/debug \
 *       -Wall -Wextra -Wpedantic \
 *       -o /tmp/send_rist \
 *       bindings/c/examples/sending/send_rist.c -ltstrans -lpthread -ldl
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rist
 *
 * Run (unicast sender, loopback):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rist
 *
 * Run with explicit destination:
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rist --dest 192.168.1.100:8000
 *
 * Receive with ffplay (RIST receiver, separate terminal):
 *   ffplay rist://@192.168.1.100:8000   # ffmpeg 5.x+ supports RIST
 *
 * Receive with a tst-c RIST receiver (see examples/receiving/recv_rist.c).
 *
 * Requires: TST_HAS_RIST == 1 (set when the `rist` cargo feature is enabled).
 *
 * Mirrors: examples/sending/send_udp.c (UDP variant) and
 *          examples/sending/send_rtp.c (RTP variant).
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
 * MPEG-TS packet size: exactly 188 bytes per ISO/IEC 13818-1 §2.4.3.2.
 * RIST sends arbitrary-length payloads but MPEG-TS consumers expect
 * 188-byte-aligned datagrams; keep payloads as multiples of 188.
 */
#define TS_PACKET_SIZE   188

/* 0x47 — the MPEG-TS sync byte at the start of every TS packet. */
#define TS_SYNC_BYTE     0x47

/*
 * Null PID (0x1FFF): receivers discard null packets per §2.4.4.4.
 * Safe synthetic payload for testing without a real encoder attached.
 */
#define TS_NULL_PID_HI   0x1F
#define TS_NULL_PID_LO   0xFF

/* Number of TS packets to send in this example. */
#define SEND_COUNT       100

/* Default RIST destination (unicast, loopback). */
#define DEFAULT_HOST_PORT "127.0.0.1:8000"

/*
 * Recovery buffer in milliseconds.
 * 200 ms is appropriate for low-latency terrestrial links. Increase to
 * 400–800 ms for high-latency or lossy links (satellite, RF, LTE).
 * The ARQ window determines minimum RIST-introduced latency.
 */
#define DEFAULT_BUFFER_MS "200"

/* ── Helper: synthesize one null TS packet ─────────────────────────────── */

/*
 * make_null_ts_packet — fill `buf` (exactly TS_PACKET_SIZE bytes) with a
 * valid MPEG-TS null packet (PID 0x1FFF, payload-only).
 *
 * TS header layout (4 bytes):
 *   byte[0]: sync_byte                          = 0x47
 *   byte[1]: error(1b)=0 | PUSI(1b)=0 | pri(1b)=0 | PID[12:8](5b) = 0x1F
 *   byte[2]: PID[7:0](8b)                       = 0xFF
 *   byte[3]: scrambling(2b)=00 | AFC(2b)=01     (payload only, no adaptation)
 *            | continuity_counter(4b)=0000
 */
static void make_null_ts_packet(uint8_t *buf) {
    buf[0] = TS_SYNC_BYTE;
    buf[1] = TS_NULL_PID_HI;  /* high 5 bits of 0x1FFF */
    buf[2] = TS_NULL_PID_LO;  /* low  8 bits of 0x1FFF */
    buf[3] = 0x10;             /* adaptation_field_control=01 (payload only) */
    memset(buf + 4, 0xFF, TS_PACKET_SIZE - 4);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * ── Step 1: Parse CLI args ────────────────────────────────────────────
     *
     * Accept --dest host:port (or a bare positional host:port argument).
     * The default sends to loopback port 8000, which is the RIST conventional
     * default port and matches the recv_rist.c example's bind address.
     */
    const char *host_port = DEFAULT_HOST_PORT;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dest") == 0 && i + 1 < argc) {
            host_port = argv[++i];
        } else if (argv[i][0] != '-') {
            host_port = argv[i];
        } else {
            fprintf(stderr,
                    "usage: send_rist [--dest host:port]\n"
                    "  Default dest: %s\n"
                    "  Encrypted:    --dest host:8000  (configure URL below for AES)\n",
                    DEFAULT_HOST_PORT);
            return 1;
        }
    }

    /*
     * ── Step 2: Build the rist:// URL ────────────────────────────────────
     *
     * Simple Profile URL: rist://host:port?buffer=200
     *
     * WHY no `profile=` param?
     *   Simple Profile is the default. Explicitly adding `?profile=simple`
     *   is equivalent and harmless but redundant.
     *
     * Encrypted Main Profile URL (uncomment and replace the URL below):
     *   rist://host:8000?aes-type=256&secret=my-pre-shared-key&buffer=200&profile=main
     *   REQUIREMENTS:
     *     - librist built with mbedTLS (`mbedtls` cargo feature, default-on)
     *     - Both sender and receiver must use the same aes-type + secret
     *     - Main Profile forces ARQ + auth; Simple Profile is rejected
     *     - Returns TST_E_RIST_ENCRYPTION_DISABLED (-41) if mbedTLS is absent
     *
     * WHY `?buffer=200`?
     *   The recovery buffer (in ms) is the ARQ window: how long the sender
     *   caches packets for NACK-driven retransmit. It also determines the
     *   minimum playout latency at the receiver. 200 ms is a standard
     *   starting point for terrestrial links (doubles to 400 ms for RF).
     */
    char url[512];
    snprintf(url, sizeof(url), "rist://%s?buffer=" DEFAULT_BUFFER_MS, host_port);
    fprintf(stderr, "[send_rist] sending to: %s\n", url);

    /*
     * ── Step 3: Open the RIST sender ─────────────────────────────────────
     *
     * tst_rist_sender_open parses the URL (including query-param config),
     * initialises a librist context, and registers the peer endpoint.
     * RIST sender construction uses a move-style builder internally:
     *   RistTransportBuilder::new(url).connect()
     * Both steps are collapsed into this single C function.
     *
     * Returns NULL on failure. Call tst_get_last_error_str() immediately
     * (before any other TST call on this thread) for the human-readable
     * diagnostic. Common failures:
     *   TST_E_RIST_CONFIG (-39) — bad URL / unsupported AES type
     *   TST_E_RIST_FFI (-38)   — librist context or peer creation failed
     *   TST_E_RIST_ENCRYPTION_DISABLED (-41) — AES requested, mbedtls absent
     *
     * NOTE: tst_rist_sender_t is the "raw TS bytes in" shape (pre-muxed path).
     * If you want the library to mux NAL units / KLV / audio into TS for you,
     * use tst_rist_mux_sender_open — same URL format, richer push API.
     */
    TstRistSender *sender = tst_rist_sender_open(url);
    if (!sender) {
        fprintf(stderr, "[send_rist] tst_rist_sender_open failed (code=%d): %s\n",
                tst_get_last_error(), tst_get_last_error_str());
        return 2;
    }
    fprintf(stderr, "[send_rist] sender opened; pushing %d TS packets\n", SEND_COUNT);

    /*
     * ── Step 4: Build a synthetic TS packet ──────────────────────────────
     *
     * Null TS packets (PID 0x1FFF) are silently discarded by every
     * conformant receiver, making them a safe test payload. In a real
     * application this buffer comes from a hardware encoder, a muxer
     * drain, or a file relay. tst_rist_sender_send_ts copies the bytes
     * before returning, so reusing/mutating the buffer after the call is safe.
     */
    uint8_t ts_pkt[TS_PACKET_SIZE];
    make_null_ts_packet(ts_pkt);

    /*
     * ── Step 5: Push 100 TS packets ──────────────────────────────────────
     *
     * tst_rist_sender_send_ts accepts any non-zero buffer length; the
     * library recommends (but does not require) multiples of 188. Passing
     * exactly 188 here means one TS packet per call. For throughput-oriented
     * code, batch multiple packets per call (up to pkt_size bytes — default
     * 1316 = 7 × 188).
     *
     * WHY no sleep?
     *   This example sends as fast as possible to verify correctness, not
     *   streaming timing. For real video, pace output to match the encoder
     *   clock to prevent receiver-side buffer overflow.
     */
    int exit_code = 0;
    for (int i = 0; i < SEND_COUNT; i++) {
        int rc = tst_rist_sender_send_ts(sender, ts_pkt, TS_PACKET_SIZE);
        if (rc != 0) {
            fprintf(stderr,
                    "[send_rist] tst_rist_sender_send_ts[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
    }

    if (exit_code == 0) {
        fprintf(stderr,
                "[send_rist] sent %d TS packets (%d bytes) successfully.\n",
                SEND_COUNT, SEND_COUNT * TS_PACKET_SIZE);
    }

    /*
     * ── Step 6: Close the sender ─────────────────────────────────────────
     *
     * Flush first. The sender sends one datagram per seven-packet bundle,
     * so 100 packets leave as 14 bundles and the last two stay buffered.
     * tst_rist_sender_close is cancel-first and abortive: it discards that
     * buffered tail, so tst_rist_sender_flush must run before it for a
     * lossless end of stream. close then closes the socket and frees all
     * associated memory; it is safe on a valid non-NULL pointer at any time,
     * including after a send failure.
     */
    if (exit_code == 0) {
        int flush_rc = tst_rist_sender_flush(sender);
        if (flush_rc != 0) {
            fprintf(stderr, "[send_rist] tst_rist_sender_flush failed (rc=%d): %s\n",
                    flush_rc, tst_get_last_error_str());
            exit_code = 1;
        }
    }
    tst_rist_sender_close(sender);
    fprintf(stderr, "[send_rist] sender closed.\n");

    return exit_code;
}
