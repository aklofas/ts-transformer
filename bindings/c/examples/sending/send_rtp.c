/*
 * send_rtp.c — Minimal RTP sender: pre-built TS packets over UDP/RTP.
 *
 * Demonstrates the lowest-level RTP send API: `tst_rtp_sender_t`. This is
 * the "raw TS bytes" path — the caller provides 188-byte-aligned MPEG-TS
 * packets and the library wraps them in RTP (RFC 2250) datagrams before
 * handing off to UDP. No mux config, no encoder state, no stream handles.
 *
 * This is distinct from `tst_rtp_mux_sender_t` (which owns a full MPEG-TS
 * muxer and accepts encoded NAL / KLV / audio frames). Use `tst_rtp_sender_t`
 * when your upstream already produces TS packets — for example a hardware
 * encoder, a relay from a file, or a downstream repackager.
 *
 * What this example shows:
 *   1. CLI arg parsing: --dest host:port (unicast) or multicast address.
 *   2. Building an rtp:// URL with optional pkt_size and ttl query params.
 *   3. Opening a tst_rtp_sender_t and checking for NULL + last-error.
 *   4. Synthesising structurally valid 188-byte TS null packets as "payload".
 *   5. Pushing 100 of those packets via tst_rtp_sender_send_ts.
 *   6. Closing cleanly via tst_rtp_sender_close.
 *
 * Why MPEG-TS over RTP for gimbaled platforms?
 *   RFC 2250 defines the RTP payload format for MPEG-TS: each UDP datagram
 *   carries one RTP header (12 bytes) followed by 1..N 188-byte TS packets.
 *   Multicast delivery lets a single airborne sender reach ground stations,
 *   recording servers, and real-time display simultaneously without unicast
 *   fan-out at the sender — critical when the platform link budget is tight.
 *   The receiver side mirrors this with `tst_rtp_recv_open`.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wall -Werror \
 *      -o /tmp/send_rtp \
 *      bindings/c/examples/sending/send_rtp.c -ltstrans -lpthread -ldl
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rtp
 *
 * Run (unicast, default):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rtp
 *
 * Run with explicit destination (unicast):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rtp --dest 192.168.1.100:5000
 *
 * Run (multicast):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_rtp --dest 239.1.2.3:5000
 *
 * Receive with ffplay (in a separate terminal):
 *   ffplay rtp://239.1.2.3:5000
 *   # or for unicast: ffplay rtp://127.0.0.1:5000
 *
 * Mirrors: examples/sending/rtp_basic_sender.rs (Rust).
 */

#include "tstrans.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * MPEG-TS packet size: exactly 188 bytes, standardised in ISO/IEC 13818-1
 * (H.222.0) §2.4.3.2. Every cell of the bitstream must be a multiple of
 * this length; receivers use it to re-synchronise on byte-stream boundaries.
 * RTP senders SHOULD align datagram payloads to multiples of 188.
 */
#define TS_PACKET_SIZE   188

/*
 * MPEG-TS sync byte: 0x47. The first byte of every TS packet. A receiver
 * that loses sync scans forward byte by byte looking for 0x47 at offset 0,
 * then 188, then 376, etc. to reacquire framing. If the sync byte is wrong
 * the packet is discarded at the transport layer.
 */
#define TS_SYNC_BYTE     0x47

/*
 * The null PID (0x1FFF = 8191 in decimal) is used for "null packets" —
 * TS cells carrying no useful payload. Hardware encoders pad CBR streams
 * with null packets to maintain a constant bit rate. Here we use them as
 * a convenient stand-in for real video/KLV data: every receiver is required
 * to silently discard null packets per §2.4.4.4.
 *
 * PID is a 13-bit field occupying bits [12:0] of the second+third bytes of
 * the TS header. 0x1FFF encodes as 0x1F in byte[1] (high bits) + 0xFF in
 * byte[2] (low bits). The transport_error_indicator and
 * payload_unit_start_indicator bits in byte[1] are 0.
 */
#define TS_NULL_PID_HI   0x1F
#define TS_NULL_PID_LO   0xFF

/*
 * Number of TS packets to send in the main loop. 100 packets × 188 bytes =
 * 18,800 bytes — small enough to finish instantly, large enough to exercise
 * the sender's RTP sequence-number and timestamp update logic across
 * multiple datagrams (pkt_size defaults to 7×188 = 1316 bytes per UDP
 * datagram, so 100 / 7 ≈ 15 UDP sends).
 */
#define SEND_COUNT       100

/* Default destination for unicast loopback testing. */
#define DEFAULT_HOST_PORT "127.0.0.1:5000"

/* ── Helper: synthesize one null TS packet ─────────────────────────────── */

/*
 * make_null_ts_packet — fill `buf` (exactly TS_PACKET_SIZE bytes) with a
 * valid MPEG-TS null packet (PID 0x1FFF, no adaptation field, no payload).
 *
 * WHY a null packet rather than random bytes?
 *   Null packets have a defined PID that every conformant receiver discards
 *   without parsing the payload. Using 0xFF-filled payload after the 4-byte
 *   header is the standard stuffing byte per H.222.0 §2.4.3.8. This means
 *   any receiver that actually reads these datagrams (e.g. ffplay) won't
 *   emit spurious parse errors — it'll simply ignore all 100 packets.
 *
 * WHY is `continuity_counter` set to 0 unconditionally?
 *   Null packets have no continuity counter requirement; receivers ignore
 *   the CC field for PID 0x1FFF per §2.4.3.3. A real sender would
 *   increment CC per-PID; for synthetic null traffic it doesn't matter.
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
    /*
     * Payload (bytes 4..187): 0xFF stuffing, as required by H.222.0 §2.4.3.8
     * for null packets. Any value is technically legal, but 0xFF makes it
     * obvious in a hex dump that this is filler, not a coding error.
     */
    memset(buf + 4, 0xFF, TS_PACKET_SIZE - 4);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    /*
     * ── Step 1: Parse CLI args ────────────────────────────────────────────
     *
     * Accept --dest host:port (or host:port without the flag for brevity).
     * The host may be a unicast address (e.g. 192.168.1.100) or an IPv4
     * multicast group address (224.0.0.0/4 — e.g. 239.1.2.3). IPv6
     * multicast is also supported by the underlying library (ff02::1 etc.),
     * but this example restricts to IPv4 for simplicity.
     *
     * WHY "239.x.x.x" for multicast in video surveillance?
     *   RFC 2365 reserves 239.0.0.0/8 as "Organization-Local" multicast —
     *   routers in a site's LAN forward it, but backbone routers filter it
     *   at administrative scope boundaries. This is the correct range for
     *   operational aviation/ISR networks where receivers and senders are
     *   co-located on the same tactical LAN.
     */
    const char *host_port = DEFAULT_HOST_PORT;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--dest") == 0 && i + 1 < argc) {
            host_port = argv[++i];
        } else if (argv[i][0] != '-') {
            /* Bare positional arg: treat as host:port. */
            host_port = argv[i];
        } else {
            fprintf(stderr, "usage: send_rtp [--dest host:port]\n"
                    "  Default dest: %s\n"
                    "  Multicast:    --dest 239.1.2.3:5000\n",
                    DEFAULT_HOST_PORT);
            return 1;
        }
    }

    /*
     * ── Step 2: Build the rtp:// URL ─────────────────────────────────────
     *
     * The URL scheme tells libtstrans which transport to instantiate.
     * "rtp://" routes to the RTP-over-UDP sender path. The host:port
     * becomes the socket destination address (unicast) or group address
     * + port (multicast — the library auto-detects from the address range).
     *
     * WHY pkt_size=1316?
     *   1316 = 7 × 188. Each UDP datagram carries exactly 7 TS packets,
     *   preceded by a 12-byte RTP header — total 1328 bytes. This is well
     *   below the 1500-byte Ethernet MTU (accounting for IP + UDP headers:
     *   1500 - 20 - 8 - 12 = 1460 usable bytes) and matches the de-facto
     *   industry standard for MPEG-TS-over-RTP used by VLC, FFmpeg, and
     *   most professional IPTV middleware. Larger values risk IP
     *   fragmentation; smaller values waste header overhead per TS packet.
     *
     * WHY is pkt_size optional here (just documenting the default)?
     *   We include the query param explicitly so the reader can see and
     *   modify it. The library default is also 1316, so omitting the param
     *   produces identical behavior — this is teaching code, not a magic
     *   number to hide.
     */
    char url[256];
    snprintf(url, sizeof(url), "rtp://%s?pkt_size=1316", host_port);
    fprintf(stderr, "[send_rtp] destination: %s\n", url);

    /*
     * ── Step 3: Open the RTP sender ──────────────────────────────────────
     *
     * `tst_rtp_sender_open` creates a UDP socket bound to an ephemeral
     * source port and resolves the destination address. For multicast
     * destinations, the socket is configured with IP_MULTICAST_TTL and
     * (if ?iface= was given) IP_MULTICAST_IF before the first send.
     *
     * Returns NULL on failure. The last-error thread-local is set; call
     * tst_get_last_error_str() immediately (before any other TST call on
     * this thread) to retrieve the human-readable message.
     *
     * NOTE: tst_rtp_sender_t is the "raw TS bytes in" shape. If you want
     * the library to also mux NAL units, KLV, and audio into TS packets
     * for you, use tst_rtp_mux_sender_open instead — same URL format,
     * richer push API.
     */
    TstRtpSender *sender = tst_rtp_sender_open(url);
    if (!sender) {
        fprintf(stderr, "[send_rtp] tst_rtp_sender_open failed: %s\n",
                tst_get_last_error_str());
        return 2;
    }
    fprintf(stderr, "[send_rtp] sender opened; pushing %d TS packets\n",
            SEND_COUNT);

    /*
     * ── Step 4: Build a synthetic TS packet ──────────────────────────────
     *
     * We synthesise one null TS packet and reuse it for every iteration.
     * In a real application this buffer would be filled by:
     *   - A hardware encoder's DMA output ring.
     *   - A libtstrans Muxer flush (tst_muxer_drain) writing into a caller
     *     buffer (the pipeline shape).
     *   - A file relay: read 188-byte chunks from a .ts file and push them.
     *
     * WHY reuse the same buffer?
     *   Demonstrating the API, not benchmarking memory; sender_send_ts
     *   copies the bytes before returning, so mutating the buffer after
     *   the call is safe.
     */
    uint8_t ts_pkt[TS_PACKET_SIZE];
    make_null_ts_packet(ts_pkt);

    /*
     * ── Step 5: Push 100 TS packets ──────────────────────────────────────
     *
     * tst_rtp_sender_send_ts accepts any non-zero buffer length; the
     * library encourages (but does not require) multiples of 188. Passing
     * exactly 188 bytes here means each call is one TS packet. For
     * throughput-oriented code, batch multiple packets per call up to the
     * configured pkt_size (1316 bytes = 7 × 188 for the default).
     *
     * The library packetises the bytes into UDP datagrams, adding the
     * 12-byte RTP header (sequence number, timestamp, SSRC) per
     * RFC 2250 §2. The RTP timestamp uses a 90 kHz clock; the library
     * advances it proportionally to the number of TS bytes consumed.
     *
     * Returns 0 on success, a negative TST_E_* code on failure (e.g.
     * TST_E_CLOSED if tst_rtp_sender_cancel was called concurrently).
     *
     * WHY no sleep between iterations?
     *   Null packets carry no timing information; a receiver that discards
     *   them has no basis for rate complaints. For real video the sender
     *   should pace output to match the encoder clock so the receiver's
     *   jitter buffer is not overrun. This example omits the sleep to keep
     *   the iteration loop readable — add usleep(11111) for a rough 90 fps
     *   rate (close to real 1316-byte datagrams at 10 Mbps).
     */
    int exit_code = 0;
    for (int i = 0; i < SEND_COUNT; i++) {
        int rc = tst_rtp_sender_send_ts(sender, ts_pkt, TS_PACKET_SIZE);
        if (rc != 0) {
            fprintf(stderr,
                    "[send_rtp] tst_rtp_sender_send_ts[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
    }

    if (exit_code == 0) {
        fprintf(stderr,
                "[send_rtp] sent %d TS packets (%d bytes) successfully.\n",
                SEND_COUNT, SEND_COUNT * TS_PACKET_SIZE);
    }

    /*
     * ── Step 6: Close the sender ─────────────────────────────────────────
     *
     * Flush first. The sender sends one datagram per seven-packet bundle,
     * so 100 packets leave as 14 bundles and the last two stay buffered.
     * tst_rtp_sender_close is cancel-first and abortive: it discards that
     * buffered tail, so tst_rtp_sender_flush must run before it for a
     * lossless end of stream. close then closes the UDP socket and frees
     * all associated memory; it is safe on a valid non-NULL pointer at any
     * time, including after a send failure.
     *
     * WHY not tst_rtp_sender_cancel first?
     *   cancel() signals a blocked send to unblock without flushing —
     *   useful when the sender is blocked in a multi-threaded context.
     *   Our single-threaded loop has already exited, so flush + close is
     *   all that is needed.
     */
    if (exit_code == 0) {
        int flush_rc = tst_rtp_sender_flush(sender);
        if (flush_rc != 0) {
            fprintf(stderr, "[send_rtp] tst_rtp_sender_flush failed (rc=%d): %s\n",
                    flush_rc, tst_get_last_error_str());
            exit_code = 1;
        }
    }
    tst_rtp_sender_close(sender);
    fprintf(stderr, "[send_rtp] sender closed.\n");

    return exit_code;
}
