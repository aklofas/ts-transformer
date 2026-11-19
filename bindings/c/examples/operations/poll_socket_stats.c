/*
 * poll_socket_stats.c — periodic libsrt wire-stats poll during a send.
 *
 * Why this example: app-level stats (in tst_mux_sender_stats_t) tell
 * you what you ASKED libsrt to do; socket-level stats (this example)
 * tell you what the network actually DID — RTT, packet loss,
 * retransmits, send-buffer occupancy. Operators tuning latency,
 * picking encryption modes, or budgeting reconnect timeouts need both
 * layers of telemetry. This example pushes 5 seconds of synthetic
 * media to a peer listener and prints socket stats every 500 ms.
 *
 * Build:
 *   cd bindings/c
 *   cargo build  # produces target/debug/libtstrans.{so,a} + include/tstrans.h
 *   cc -I ../../target/debug/include -L ../../target/debug \
 *      -Wall -Werror -o /tmp/poll_socket_stats \
 *      examples/operations/poll_socket_stats.c -ltstrans
 *
 * Run:
 *   # Terminal 1 (receiver — anything that accepts a libsrt connection):
 *   srt-live-transmit 'srt://:9000?mode=listener' file://con > /tmp/out.ts
 *
 *   # Terminal 2 (this example):
 *   LD_LIBRARY_PATH=../../target/debug /tmp/poll_socket_stats srt://127.0.0.1:9000
 *
 * What you'll see in stderr:
 *   t=  500ms  rtt=  321 µs  bytes_sent=  168192  pkts_sent=  128  ...
 *   t= 1000ms  rtt=  308 µs  bytes_sent=  333240  pkts_sent=  254  ...
 *   ...
 *
 * Mirrors the operational-telemetry concept of
 * examples/operations/managed_reconnect.rs (Rust side; that one shows
 * application-level reconnect telemetry; this one shows wire-level
 * socket telemetry — complementary, not duplicates).
 */

#include "tstrans.h"
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* Build a tiny H.264 IDR-ish NAL (start code + nal_unit_type=5 + payload). */
static void make_nal(uint8_t *buf, size_t len) {
    buf[0] = 0; buf[1] = 0; buf[2] = 0; buf[3] = 1; buf[4] = 0x65;
    memset(buf + 5, 0xAA, len - 5);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <srt-url>\n", argv[0]);
        fprintf(stderr, "  e.g.  %s srt://127.0.0.1:9000\n", argv[0]);
        return 1;
    }
    const char *url = argv[1];
    /* The query can carry `passphrase=`, so only the part before the `?` is
     * printed: `%.*s` with strcspn stops there. */
    fprintf(stderr, "sending to %.*s\n", (int) strcspn(url, "?"), url);

    /* Single H.264 video stream. Minimal mux topology — the point of
     * this example is the poll loop, not the muxing. */
    tst_mux_config_t *cfg = tst_mux_config_new();
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);

    tst_mux_sender_t *s = tst_mux_sender_open(url, cfg);
    if (!s) {
        fprintf(stderr, "open failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 1;
    }
    tst_mux_config_free(cfg);

    /* 30 fps for 5 seconds = 150 frames. Poll socket stats every 15
     * frames (~500 ms). The payload is a fixed 16 KB blob — large enough
     * that bandwidth fields become meaningful within a few hundred ms. */
    uint8_t nal[16 * 1024];
    int64_t pts_90khz = 0;
    const int64_t pts_step = 90000 / 30;  /* 3000 = 33.333 ms in 90 kHz ticks */

    fprintf(stderr, "starting 5s push (30 fps, 16 KB NAL each)...\n");
    for (int frame = 0; frame < 150; frame++) {
        make_nal(nal, sizeof(nal));
        int rc = tst_mux_sender_send_video(s, nal, sizeof(nal), pts_90khz, frame == 0);
        if (rc != 0) {
            fprintf(stderr, "send_video[%d] failed: %s\n", frame,
                    tst_get_last_error_str());
            tst_mux_sender_close(s);
            return 1;
        }
        pts_90khz += pts_step;

        /* Every 15 frames (~500 ms at 30 fps), print socket stats. */
        if ((frame % 15) == 14) {
            tst_socket_stats_t sock;
            memset(&sock, 0, sizeof(sock));
            rc = tst_mux_sender_get_socket_stats(s, &sock);
            if (rc == 0) {
                /* Pretty-print: convert pts_90khz back to ms for the t= prefix.
                 * pts is in 90 kHz ticks; ÷90 = ms. */
                fprintf(stderr,
                        "t=%5lldms  rtt=%5u us  bytes_sent=%9llu  pkts_sent=%6llu  "
                        "loss_recv=%4llu  retrans=%4llu  sndbuf=%4u\n",
                        (long long)(pts_90khz / 90),
                        sock.rtt_us,
                        (unsigned long long)sock.bytes_sent,
                        (unsigned long long)sock.packets_sent,
                        (unsigned long long)sock.packets_lost_recv,
                        (unsigned long long)sock.packets_retransmitted,
                        sock.send_buffer_packets);
            } else if (rc == TST_E_NOT_AVAILABLE) {
                /* Transient: managed reconnect would surface this. For a
                 * plain sender it shouldn't happen unless the inner
                 * libsrt socket died — log and continue. */
                fprintf(stderr, "t=%5lldms  (socket transient: NOT_AVAILABLE)\n",
                        (long long)(pts_90khz / 90));
            } else {
                fprintf(stderr, "get_socket_stats: rc=%d (%s)\n", rc,
                        tst_get_last_error_str());
                break;
            }
        }

        usleep(33333);  /* ~30 fps */
    }

    fprintf(stderr, "done\n");
    tst_mux_sender_close(s);
    return 0;
}
