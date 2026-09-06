/*
 * managed_reconnect.c — `tst_managed_mux_sender_t` against a deliberately
 * flaky peer: watch the sender survive two forced disconnects.
 *
 * A peer thread accepts a connection, reads a few messages, DROPS it, and
 * re-listens — twice — then drains to a clean close. The sender on the
 * main thread is the MANAGED mux sender: on each break it queues outbound
 * TS chunks in a gap buffer, reconnects with exponential backoff, drains
 * the queue, and carries on. The application code never sees the outage
 * as an error — that is the property this example demonstrates.
 *
 * The C twin of the Rust `examples/operations/managed_reconnect.rs`. One
 * structural difference: the Rust example supplies a FACTORY closure that
 * the managed transport calls to rebuild the link, and builds the policy
 * as a struct literal. The C ABI derives the factory from the URL you
 * opened with (reconnect == "dial that URL again") and takes the policy as
 * an opaque `tst_reconnect_policy_t` built through setters. Same knobs,
 * same semantics, no callback to write.
 *
 * Reconnect mode: this example uses the DEFAULT, TST_RECONNECT_MODE_BLOCKING
 * — a send that hits a broken link blocks the CALLER inside the send call
 * until reconnect succeeds or max_attempts runs out. The sibling
 * `managed_reconnect_background.c` flips exactly one knob
 * (TST_RECONNECT_MODE_BACKGROUND) and shows how differently the producing
 * thread experiences the same outage.
 *
 * Peer-side mechanics worth knowing before reading `flaky_peer` below:
 *   - `tst_raw_receiver_open_listener` = bind + accept in ONE blocking
 *     call; the listening socket is released as soon as one caller is
 *     accepted, so the returned handle owns just that connection.
 *   - `tst_raw_receiver_close` on that handle therefore closes the
 *     connection AND leaves nothing listening on the port. libsrt sends a
 *     teardown to the remote peer; the managed sender sees that as a
 *     broken transport on its next send — and THAT is the entire
 *     disconnect simulation. The next round's `_open_listener` re-binds
 *     the port so the sender's reconnect has something to dial.
 *   - A thread blocked in `_open_listener` cannot be cancelled through the
 *     C ABI today (there is no handle to cancel until accept returns), so
 *     main joins the peer with a DEADLINE and reports rather than hangs if
 *     the choreography ever goes wrong (see Step 6). ROADMAP tracks a
 *     cancellable listener re-accept.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c --features srt
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/managed_reconnect \
 *      bindings/c/examples/operations/managed_reconnect.c -ltstrans -lpthread
 *
 * Run:
 *   LD_LIBRARY_PATH=target/debug /tmp/managed_reconnect
 *   # watch stderr: "peer: round 0 dropping", "peer: round 1 accepted", ...
 *   # final line: "OK: completed run with reconnects (... successes=2 ...)"
 *
 * Requires: TST_HAS_SRT == 1 (set when the `srt` cargo feature is enabled).
 */

/* pthread_timedjoin_np is a GNU extension (Linux-only, as are all C
 * examples here by build convention). Must precede every include. */
#define _GNU_SOURCE

#include "tstrans.h"

#if !defined(TST_HAS_SRT) || TST_HAS_SRT == 0
#error "This example requires TST_HAS_SRT. Rebuild tst-c with the srt cargo feature enabled."
#endif

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * NUM_FRAMES: total synthetic video frames the sender pushes. Sized to span
 * two peer-induced disconnects with comfortable headroom — at 30 fps this
 * is ~1 s of "video". Small on purpose: a smoke test of the reconnect
 * machinery, not a throughput demo.
 *
 * PEER_ROUNDS / PEER_DROP_AFTER: the peer accepts PEER_ROUNDS times; in every
 * round but the last it drops the link after PEER_DROP_AFTER messages. The
 * peer counts SRT MESSAGES (1316-byte TS bundles), not video frames — one
 * frame here is roughly one bundle, but keep the two ideas distinct.
 */
#define NUM_FRAMES       30
#define FRAME_PTS_STEP   3000   /* 90 kHz / 30 fps */
#define PEER_ROUNDS      3
#define PEER_DROP_AFTER  5

/* Both ends set the same TSBPD latency — SRT negotiates the max of the two,
 * so a mismatch silently picks the larger; setting both is the habit. */
#define LATENCY_MS       "120"

/* Bound on how long main waits for the peer thread after the sender closes.
 * Generous: the peer normally exits within one latency window. */
#define PEER_JOIN_SECS   10

/* Mux PIDs: the `MuxerConfig::default()` layout, see mux_to_file.c. */
#define PMT_PID          0x1000
#define VIDEO_PID        0x1011
#define KLV_PID          0x1031

/* ── Helpers ───────────────────────────────────────────────────────────── */

/* Ask the kernel for a free loopback port (TCP bind(0) idiom — see
 * send_srt_encrypted.c for why TCP is fine for an SRT/UDP port). */
static uint16_t pick_free_port(void) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return 0;
    }
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof(addr);
    uint16_t port = 0;
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0 &&
        getsockname(fd, (struct sockaddr *)&addr, &len) == 0) {
        port = ntohs(addr.sin_port);
    }
    close(fd);
    return port;
}

/*
 * Synthetic H.264 access unit: Annex-B start code + IDR NAL header (0x65 =
 * nal_ref_idc 3, nal_unit_type 5) + 0xAA filler. The muxer never parses
 * NAL contents; the header byte is for tools that inspect the wire.
 */
static size_t make_nal(uint8_t *buf, size_t filler) {
    buf[0] = 0x00; buf[1] = 0x00; buf[2] = 0x00; buf[3] = 0x01; buf[4] = 0x65;
    memset(buf + 5, 0xAA, filler);
    return 5 + filler;
}

/*
 * Synthetic KLV: the 16-byte MISB ST 0601 UAS Datalink LS Universal Label,
 * a BER short-form length byte (valid for n < 128), then n bytes of `seq`.
 * The muxer is opaque to KLV contents on the send path.
 */
static size_t make_klv(uint8_t *buf, size_t n, uint8_t seq) {
    static const uint8_t ul[16] = {
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
        0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    };
    memcpy(buf, ul, 16);
    buf[16] = (uint8_t)n;
    memset(buf + 17, seq, n);
    return 17 + n;
}

/* ── Flaky peer thread ─────────────────────────────────────────────────── */

struct peer_ctx {
    char listen_url[256];
    atomic_int ready;      /* set just before the first blocking open */
    int rounds_done;
    int failed;
};

/*
 * This thread SIMULATES a flaky receiver — its job is to drop the link on
 * purpose so the sender can demonstrate reconnect. Real receivers accept
 * once and drain; this one misbehaves so the whole failure-handling stack
 * (managed transport, policy, gap buffer, backoff) is exercised inside one
 * process with no external harness.
 *
 *   round 0: accept → read 5 messages → close (induce disconnect #1)
 *   round 1: accept → read 5 messages → close (induce disconnect #2)
 *   round 2: accept → read until the sender's clean close
 *
 * If the sender closes cleanly during an EARLIER round (END_OF_STREAM),
 * the peer stops there — another `_open_listener` would block forever with
 * no caller left to accept.
 */
static void *flaky_peer(void *arg) {
    struct peer_ctx *ctx = arg;
    uint8_t buf[1500];   /* > 1316 so each recv returns one whole message */

    for (int round = 0; round < PEER_ROUNDS; round++) {
        if (round == 0) {
            atomic_store(&ctx->ready, 1);
        }
        tst_raw_receiver_t *rx = tst_raw_receiver_open_listener(ctx->listen_url);
        if (!rx) {
            fprintf(stderr, "peer: round %d open_listener failed: %s\n",
                    round, tst_get_last_error_str());
            ctx->failed = 1;
            return NULL;
        }
        fprintf(stderr, "peer: round %d accepted\n", round);

        int messages = 0;
        int sender_closed = 0;
        for (;;) {
            size_t n = 0;
            int rc = tst_raw_receiver_recv(rx, buf, sizeof(buf), &n);
            if (rc == 0) {
                messages++;
                if (messages >= PEER_DROP_AFTER && round < PEER_ROUNDS - 1) {
                    fprintf(stderr, "peer: round %d dropping after %d messages\n",
                            round, messages);
                    break;
                }
            } else if (rc == TST_E_END_OF_STREAM) {
                /* The sender called close — canonical clean-close signal. */
                fprintf(stderr, "peer: round %d clean close after %d messages\n",
                        round, messages);
                sender_closed = 1;
                break;
            } else {
                fprintf(stderr, "peer: round %d recv failed (rc=%d): %s\n",
                        round, rc, tst_get_last_error_str());
                ctx->failed = 1;
                break;
            }
        }
        /*
         * The disconnect simulation is this one call: it closes the
         * accepted socket (libsrt tears the link down on the wire) and, the
         * listening socket having been released at accept, leaves the port
         * closed until the next round re-binds it.
         */
        tst_raw_receiver_close(rx);
        ctx->rounds_done++;
        if (sender_closed || ctx->failed) {
            break;
        }
    }
    return NULL;
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(void) {
    /* ── Step 1: Port, URLs, peer thread ─────────────────────────────── */
    uint16_t port = pick_free_port();
    if (port == 0) {
        fprintf(stderr, "could not pick a free loopback port\n");
        return 1;
    }

    struct peer_ctx peer;
    memset(&peer, 0, sizeof(peer));
    atomic_init(&peer.ready, 0);
    snprintf(peer.listen_url, sizeof(peer.listen_url),
             "srt://127.0.0.1:%u?latency=%s", (unsigned)port, LATENCY_MS);

    /* The sender dials the same address. This URL is also, implicitly, the
     * reconnect factory: every reconnect attempt re-dials exactly this. */
    char connect_url[256];
    snprintf(connect_url, sizeof(connect_url),
             "srt://127.0.0.1:%u?latency=%s", (unsigned)port, LATENCY_MS);

    pthread_t peer_thread;
    if (pthread_create(&peer_thread, NULL, flaky_peer, &peer) != 0) {
        perror("pthread_create");
        return 1;
    }
    /* Wait for "about to bind", then a short pause for bind() to land. SRT's
     * handshake retry covers a hair-too-early connect regardless. */
    while (atomic_load(&peer.ready) == 0) {
        usleep(1000);
    }
    usleep(50 * 1000);

    /*
     * ── Step 2: ReconnectPolicy — the knobs that govern reconnect ──────
     *
     * Built through setters on an opaque handle; every setter returns 0 or
     * TST_E_INVALID_CONFIG for an out-of-range value. The values mirror the
     * Rust twin exactly:
     *
     *   mode = BLOCKING (the default; set explicitly here so the knob is
     *     visible — see the header, and managed_reconnect_background.c)
     *
     *   max_attempts = 20
     *     Up to 20 reconnect attempts per outage before the policy gives
     *     up and sends start failing with TST_E_CLOSED. -1 would mean
     *     retry forever; 20 keeps this demo finite even if the peer thread
     *     dies. The budget resets after every successful reconnect.
     *
     *   backoff = exponential, base 50 ms, cap 2 s
     *     wait = 50 ms × 2^(attempt-1), capped. The base is short so the
     *     demo iterates visibly fast; production defaults are 100 ms /
     *     10 s. The cap prevents pathologically long waits if the peer
     *     stays down.
     *
     *   gap_buffer_capacity = 256
     *     Max TS chunks queued while disconnected. Rule of thumb: max
     *     disconnect window × send rate. The peer here is back within
     *     milliseconds, so 256 is two orders of magnitude of headroom —
     *     cheap, and the production default.
     *
     *   overflow_policy = DROP_OLDEST
     *     When the gap buffer is full, evict the oldest queued chunk and
     *     accept the new one (REJECT would surface an error instead).
     *     Keeps the receiver caught up to "now" once the link returns, at
     *     the cost of the tail of the gap — for live video, the right
     *     trade: receivers want fresh frames, not stale ones.
     */
    tst_reconnect_policy_t *policy = tst_reconnect_policy_new();
    if (!policy) {
        fprintf(stderr, "tst_reconnect_policy_new: out of memory\n");
        return 1;
    }
    (void) tst_reconnect_policy_set_mode(policy, TST_RECONNECT_MODE_BLOCKING);
    (void) tst_reconnect_policy_set_max_attempts(policy, 20);
    (void) tst_reconnect_policy_set_backoff_exponential_ms(policy, 50, 2000);
    (void) tst_reconnect_policy_set_gap_buffer_capacity(policy, 256);
    (void) tst_reconnect_policy_set_overflow_policy(policy, TST_OVERFLOW_POLICY_DROP_OLDEST);

    /* ── Step 3: Mux config (default single-program layout) ──────────── */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        tst_reconnect_policy_free(policy);
        return 1;
    }
    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, PMT_PID);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        tst_reconnect_policy_free(policy);
        return 1;
    }
    tst_mux_config_add_video_stream(cfg, prog, VIDEO_PID, TST_VIDEO_CODEC_H264);
    tst_mux_config_add_klv_stream(cfg, prog, KLV_PID,
                                  TST_KLV_STREAM_TYPE_PRIVATE_DATA,
                                  /*carries_pts=*/ false);

    /*
     * ── Step 4: Open the MANAGED mux sender ──────────────────────────────
     *
     * The initial connect runs synchronously inside open — if even the
     * first link fails there is no point starting the pipeline, so open
     * returns NULL (TST_E_TRANSPORT). Later failures are absorbed by the
     * reconnect machinery. Both the config and the policy are copied, so
     * both are freed right after, on both branches.
     *
     * End-to-end the path is: NAL+KLV → muxer → 188-byte TS packets →
     * managed transport (gap buffer + reconnect) → SRT → wire. The muxer
     * cannot tell it is sitting on a managed transport; it just sees a
     * transport that occasionally pauses and never fails for transient
     * breakage.
     */
    tst_managed_mux_sender_t *s = tst_managed_mux_sender_open(connect_url, cfg, policy);
    tst_mux_config_free(cfg);
    tst_reconnect_policy_free(policy);
    cfg = NULL;
    policy = NULL;
    if (!s) {
        fprintf(stderr, "tst_managed_mux_sender_open failed (%d): %s\n",
                tst_get_last_error(), tst_get_last_error_str());
        return 2;
    }

    /*
     * ── Step 5: Push frames straight through two outages ─────────────────
     *
     * Send results here are INFORMATIONAL, not fatal. The managed
     * transport absorbs broken-link errors internally (queues the chunk,
     * reconnects, drains) and returns 0. Only catastrophic failures — the
     * reconnect budget exhausted (TST_E_CLOSED) or an oversized payload —
     * come back non-zero, so the loop logs and keeps going: the very next
     * send may well succeed once the reconnect lands.
     *
     * In BLOCKING mode the outage is visible only as latency: a send that
     * hits the dead link returns after the reconnect completes, and the
     * frames that queued up meanwhile go out in a burst on the new link.
     */
    fprintf(stderr, "sender: sending %d frames; peer drops after %d messages, %d rounds\n",
            NUM_FRAMES, PEER_DROP_AFTER, PEER_ROUNDS);
    uint8_t nal[1024];
    uint8_t klv[128];
    int sent_ok = 0;
    int sent_err = 0;
    for (int i = 0; i < NUM_FRAMES; i++) {
        const int64_t pts = (int64_t)i * FRAME_PTS_STEP;
        const size_t nal_len = make_nal(nal, 800);
        const size_t klv_len = make_klv(klv, 64, (uint8_t)i);

        /* First frame is the IDR; the rest are non-key (a real encoder's
         * P-frames). The flag drives the TS random_access_indicator. */
        int rc = tst_managed_mux_sender_send_video(s, nal, nal_len, pts, i == 0);
        if (rc == 0) {
            sent_ok++;
        } else {
            fprintf(stderr, "sender: send_video[%d] -> rc=%d: %s\n",
                    i, rc, tst_get_last_error_str());
            sent_err++;
        }
        rc = tst_managed_mux_sender_send_klv(s, klv, klv_len, pts);
        if (rc != 0) {
            fprintf(stderr, "sender: send_klv[%d] -> rc=%d: %s\n",
                    i, rc, tst_get_last_error_str());
            sent_err++;
        }
        /* 33 ms ≈ 30 fps cadence, purely so the peer's log lines are
         * readable; a real publisher pushes as the encoder produces. */
        usleep(33 * 1000);
    }
    fprintf(stderr, "sender: %d OK, %d errored across reconnects\n", sent_ok, sent_err);

    /*
     * Reconnect telemetry. In BLOCKING mode this getter shares a lock with
     * the inline reconnect loop, so calling it DURING an outage would block
     * for the outage's duration — read it at the end here. (The background
     * sibling polls it live; that non-blocking property is what
     * BACKGROUND mode buys.)
     */
    tst_managed_transport_stats_t st;
    memset(&st, 0, sizeof(st));
    int have_stats = (tst_managed_mux_sender_get_reconnect_stats(s, &st) == 0);

    /* Close flushes the muxer, then tears down the transport. The peer sees
     * this as END_OF_STREAM and exits its recv loop. */
    tst_managed_mux_sender_close(s);
    s = NULL;

    /*
     * ── Step 6: Bounded join of the peer ─────────────────────────────────
     *
     * pthread_timedjoin_np (GNU) rather than pthread_join: a peer stuck in
     * an accept nobody will ever complete cannot be cancelled through the C
     * ABI, and an unbounded join would hang the process. Return from main
     * ends the process (and the stuck thread) either way; the deadline just
     * turns "hangs forever" into a diagnosed non-zero exit.
     */
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += PEER_JOIN_SECS;
    int jrc = pthread_timedjoin_np(peer_thread, NULL, &deadline);
    if (jrc == ETIMEDOUT) {
        fprintf(stderr, "FAIL: peer thread did not exit within %d s (rounds_done=%d)\n",
                PEER_JOIN_SECS, peer.rounds_done);
        fflush(stderr);
        _exit(5);   /* skip atexit: libsrt cleanup must not wait on a blocked accept */
    }

    if (have_stats) {
        fprintf(stderr, "reconnect stats: attempts=%llu successes=%llu gap_len=%llu "
                        "dropped_msgs=%llu dropped_bytes=%llu\n",
                (unsigned long long)st.reconnect_attempts,
                (unsigned long long)st.reconnect_successes,
                (unsigned long long)st.gap_len,
                (unsigned long long)st.gap_messages_dropped,
                (unsigned long long)st.gap_bytes_dropped);
    }
    if (peer.failed || peer.rounds_done != PEER_ROUNDS) {
        fprintf(stderr, "FAIL: peer completed %d of %d rounds (failed=%d)\n",
                peer.rounds_done, PEER_ROUNDS, peer.failed);
        return 4;
    }
    printf("OK: completed run with reconnects (sent_ok=%d, sent_err=%d, successes=%llu)\n",
           sent_ok, sent_err, (unsigned long long)st.reconnect_successes);
    return 0;
}
