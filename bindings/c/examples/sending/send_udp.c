/*
 * send_udp.c — Minimal raw UDP sender: pre-built TS packets over plain UDP.
 *
 * Demonstrates the lowest-level UDP send API: `tst_udp_sender_t`. This is
 * the "raw TS bytes" path — the caller provides 188-byte-aligned MPEG-TS
 * packets and the library hands them to the UDP socket directly (no RTP
 * header, unlike `tst_rtp_sender_t`). No mux config, no encoder state, no
 * stream handles.
 *
 * This is distinct from `tst_udp_mux_sender_t` (which owns a full MPEG-TS
 * muxer and accepts encoded NAL / KLV / audio frames). Use `tst_udp_sender_t`
 * when your upstream already produces TS packets — for example a hardware
 * encoder, a relay from a file, or a downstream repackager.
 *
 * Why plain UDP (vs RTP, vs SRT) for gimbaled-platform video?
 *   Plain UDP/MPEG-TS is the simplest possible push: one datagram carries
 *   N×188-byte TS packets with no RTP sequencing layer. It is the format
 *   ffmpeg emits for `udp://host:port` and what many legacy ground stations
 *   and recorders consume. It has no retransmission (use SRT for that) and
 *   no RTP timestamp/sequence header (use RTP when the receiver needs jitter
 *   reordering). For a co-located tactical LAN where loss is rare and the
 *   receiver tolerates raw TS, plain UDP minimizes overhead and latency.
 *
 * What this example shows:
 *   1. CLI arg parsing: --dest host:port (unicast) or multicast address.
 *   2. Building a udp:// URL with an optional pkt_size query param.
 *   3. Opening a tst_udp_sender_t and checking for NULL + last-error.
 *   4. Synthesising structurally valid 188-byte TS null packets as "payload".
 *   5. Pushing 100 of those packets via tst_udp_sender_send_ts.
 *   6. Flushing the buffered tail with tst_udp_sender_flush, then closing via
 *      tst_udp_sender_close.
 *
 * Note on shutdown: tst_udp_sender_cancel exists and may be called from
 * another thread to end a parked send. This single-threaded example needs
 * none: it runs its loop to completion and calls tst_udp_sender_close
 * directly, which is cancel-first and abortive.
 *
 * Build (from the ts-transformer workspace root):
 *   cargo build -p tst-c --no-default-features --features udp
 *   cc -I target/debug/include \
 *      -L target/debug \
 *      -Wall -Werror \
 *      -o /tmp/send_udp \
 *      bindings/c/examples/sending/send_udp.c -ltstrans -lpthread -ldl
 *   LD_LIBRARY_PATH=target/debug /tmp/send_udp
 *
 * Run (unicast, default):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_udp
 *
 * Run with explicit destination (unicast):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_udp --dest 192.168.1.100:5000
 *
 * Run (multicast):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_udp --dest 239.1.2.3:5000
 *
 * Receive with ffplay (in a separate terminal):
 *   ffplay udp://@239.1.2.3:5000      # multicast (note ffmpeg's @ prefix)
 *   ffplay udp://127.0.0.1:5000       # unicast
 *
 * Requires: TST_HAS_UDP == 1 (set when the `udp` cargo feature is enabled).
 *
 * Mirrors: examples/sending/rtp_basic_sender.rs (Rust, RTP variant) and the
 *          sibling send_rtp.c C example.
 */

#include "tstrans.h"

#if !defined(TST_HAS_UDP) || TST_HAS_UDP == 0
#error "This example requires TST_HAS_UDP. Rebuild tst-c with the udp cargo feature enabled."
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * MPEG-TS packet size: exactly 188 bytes, standardised in ISO/IEC 13818-1
 * (H.222.0) §2.4.3.2. Every cell of the bitstream is a multiple of this
 * length; receivers use it to re-synchronise on byte-stream boundaries.
 * UDP senders SHOULD align datagram payloads to multiples of 188.
 */
#define TS_PACKET_SIZE   188

/*
 * MPEG-TS sync byte: 0x47. The first byte of every TS packet. A receiver
 * that loses sync scans forward byte by byte looking for 0x47 at offset 0,
 * then 188, then 376, etc. to reacquire framing.
 */
#define TS_SYNC_BYTE     0x47

/*
 * The null PID (0x1FFF) is used for "null packets" — TS cells carrying no
 * useful payload. Every conformant receiver silently discards them per
 * §2.4.4.4, so they make a safe synthetic payload for this example.
 *
 * PID is a 13-bit field: 0x1FFF encodes as 0x1F in byte[1] (high 5 bits)
 * plus 0xFF in byte[2] (low 8 bits).
 */
#define TS_NULL_PID_HI   0x1F
#define TS_NULL_PID_LO   0xFF

/*
 * Number of TS packets to send. 100 packets × 188 bytes = 18,800 bytes —
 * small enough to finish instantly, large enough to span several UDP
 * datagrams (pkt_size defaults to 7×188 = 1316 bytes, so 100/7 ≈ 15 sends).
 */
#define SEND_COUNT       100

/* Default destination for unicast loopback testing. */
#define DEFAULT_HOST_PORT "127.0.0.1:5000"

/* ── Helper: synthesize one null TS packet ─────────────────────────────── */

/*
 * make_null_ts_packet — fill `buf` (exactly TS_PACKET_SIZE bytes) with a
 * valid MPEG-TS null packet (PID 0x1FFF, no adaptation field, payload-only).
 *
 * WHY a null packet rather than random bytes?
 *   Null packets have a defined PID that every conformant receiver discards
 *   without parsing the payload — so ffplay won't emit spurious parse errors
 *   when it reads these datagrams. 0xFF stuffing after the 4-byte header is
 *   the standard filler per H.222.0 §2.4.3.8.
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
    buf[1] = TS_NULL_PID_HI;  /* 0x1F — high 5 bits of 0x1FFF */
    buf[2] = TS_NULL_PID_LO;  /* 0xFF — low 8 bits of 0x1FFF  */
    buf[3] = 0x10;             /* adaptation_field_control=01 (payload only) */
    memset(buf + 4, 0xFF, TS_PACKET_SIZE - 4);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * ── Step 1: Parse CLI args ────────────────────────────────────────────
     *
     * Accept --dest host:port (or a bare host:port positional). The host may
     * be a unicast address (192.168.1.100) or an IPv4 multicast group
     * (224.0.0.0/4 — e.g. 239.1.2.3). The library auto-detects multicast
     * from the destination address range and configures IP_MULTICAST_TTL /
     * IP_MULTICAST_IF accordingly.
     */
    const char *host_port = DEFAULT_HOST_PORT;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dest") == 0 && i + 1 < argc) {
            host_port = argv[++i];
        } else if (argv[i][0] != '-') {
            host_port = argv[i];
        } else {
            fprintf(stderr, "usage: send_udp [--dest host:port]\n"
                    "  Default dest: %s\n"
                    "  Multicast:    --dest 239.1.2.3:5000\n",
                    DEFAULT_HOST_PORT);
            return 1;
        }
    }

    /*
     * ── Step 2: Build the udp:// URL ─────────────────────────────────────
     *
     * The URL scheme routes to the raw-UDP sender path. The host:port becomes
     * the socket destination (unicast) or group address + port (multicast).
     *
     * WHY pkt_size=1316?
     *   1316 = 7 × 188. Each UDP datagram carries exactly 7 TS packets, well
     *   below the 1500-byte Ethernet MTU (after the 20-byte IP + 8-byte UDP
     *   headers there are 1472 usable bytes). This is the de-facto industry
     *   standard for MPEG-TS-over-UDP used by VLC, FFmpeg, and most IPTV
     *   middleware. Larger values risk IP fragmentation; smaller values waste
     *   header overhead. The library default is also 1316, so this param just
     *   makes the value visible and tweakable.
     */
    char url[256];
    snprintf(url, sizeof(url), "udp://%s?pkt_size=1316", host_port);
    fprintf(stderr, "[send_udp] destination: %s\n", url);

    /*
     * ── Step 3: Open the UDP sender ──────────────────────────────────────
     *
     * tst_udp_sender_open creates a UDP socket and resolves the destination.
     * For multicast destinations it configures IP_MULTICAST_TTL and (if
     * ?iface= was given) IP_MULTICAST_IF.
     *
     * Returns NULL on failure. The last-error thread-local is set; call
     * tst_get_last_error_str() immediately (before any other TST call on
     * this thread) to retrieve the human-readable message.
     *
     * NOTE: tst_udp_sender_t is the "raw TS bytes in" shape. If you want the
     * library to also mux NAL units / KLV / audio into TS for you, use
     * tst_udp_mux_sender_open instead — same URL format, richer push API.
     */
    TstUdpSender *sender = tst_udp_sender_open(url);
    if (!sender) {
        fprintf(stderr, "[send_udp] tst_udp_sender_open failed: %s\n",
                tst_get_last_error_str());
        return 2;
    }
    fprintf(stderr, "[send_udp] sender opened; pushing %d TS packets\n",
            SEND_COUNT);

    /*
     * ── Step 4: Build a synthetic TS packet ──────────────────────────────
     *
     * We synthesise one null TS packet and reuse it for every iteration. In a
     * real application this buffer would be filled by a hardware encoder's
     * DMA output, a libtstrans muxer drain, or a file relay reading 188-byte
     * chunks. send_ts copies the bytes before returning, so reusing/mutating
     * the buffer after the call is safe.
     */
    uint8_t ts_pkt[TS_PACKET_SIZE];
    make_null_ts_packet(ts_pkt);

    /*
     * ── Step 5: Push 100 TS packets ──────────────────────────────────────
     *
     * tst_udp_sender_send_ts accepts any non-zero buffer length; the library
     * encourages (but does not require) multiples of 188. Passing exactly 188
     * here means one TS packet per call. For throughput-oriented code, batch
     * multiple packets per call up to the configured pkt_size.
     *
     * Returns 0 on success, a negative TST_E_* code on failure.
     *
     * WHY no sleep between iterations?
     *   Null packets carry no timing, so a receiver that discards them has no
     *   basis for rate complaints. For real video, pace output to match the
     *   encoder clock so the receiver's jitter buffer is not overrun.
     */
    int exit_code = 0;
    for (int i = 0; i < SEND_COUNT; i++) {
        int rc = tst_udp_sender_send_ts(sender, ts_pkt, TS_PACKET_SIZE);
        if (rc != 0) {
            fprintf(stderr,
                    "[send_udp] tst_udp_sender_send_ts[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
    }

    if (exit_code == 0) {
        fprintf(stderr,
                "[send_udp] sent %d TS packets (%d bytes) successfully.\n",
                SEND_COUNT, SEND_COUNT * TS_PACKET_SIZE);
    }

    /*
     * ── Step 6: Close the sender ─────────────────────────────────────────
     *
     * Flush first. The sender sends one datagram per seven-packet bundle,
     * so 100 packets leave as 14 bundles and the last two stay buffered.
     * tst_udp_sender_close is cancel-first and abortive: it discards that
     * buffered tail, so tst_udp_sender_flush must run before it for a
     * lossless end of stream. close then closes the socket and frees all
     * associated memory; it is safe on a valid non-NULL pointer at any time,
     * including after a send failure.
     */
    if (exit_code == 0) {
        int flush_rc = tst_udp_sender_flush(sender);
        if (flush_rc != 0) {
            fprintf(stderr, "[send_udp] tst_udp_sender_flush failed (rc=%d): %s\n",
                    flush_rc, tst_get_last_error_str());
            exit_code = 1;
        }
    }
    tst_udp_sender_close(sender);
    fprintf(stderr, "[send_udp] sender closed.\n");

    return exit_code;
}
