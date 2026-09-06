/*
 * managed_reconnect_background.c — `tst_managed_mux_sender_t` in
 * TST_RECONNECT_MODE_BACKGROUND: the producing thread keeps its cadence
 * straight through a sustained outage.
 *
 * Sibling of `managed_reconnect.c` (which uses the default BLOCKING mode):
 * same flaky-peer shape, same managed mux sender, same policy knobs. The
 * ONE thing this example changes is the policy's mode, and that one knob
 * changes everything about how the producer experiences an outage:
 *
 *   BLOCKING (default): a send that hits a broken link blocks the caller
 *     until reconnect succeeds or max_attempts runs out. Simple, lossless,
 *     and exactly wrong for a thread that is also draining a live encoder.
 *   BACKGROUND: a dedicated per-outage worker thread owns the reconnect
 *     loop (backoff, re-dial, drain). Sends never wait for the link to
 *     come back — they enqueue into the gap buffer under the overflow
 *     policy and return. (A send can still block briefly on lock
 *     contention while the worker is mid-drain, bounded to one in-flight
 *     inner send.)
 *
 * The C twin of the Rust `examples/operations/managed_reconnect_background.rs`.
 * Watch stderr: the send loop keeps producing at a steady 10 ms cadence
 * across the outage (no stall), while a stats line every 10 frames shows
 * the gap buffer filling, messages being EVICTED, and the worker
 * recovering. That eviction is the lesson — see "Ok != delivered" below.
 *
 * Choose BACKGROUND for a single-threaded relay pump — one thread both
 * produces frames and sends them — where blocking that thread through a
 * reconnect window means the upstream source backs up or drops frames on
 * the floor anyway. Prefer BLOCKING for batch / file senders, where losing
 * bytes is worse than waiting and there is no time-sensitive producer to
 * protect: a blocked caller there just means the job takes a bit longer.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c --features srt
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/managed_reconnect_background \
 *      bindings/c/examples/operations/managed_reconnect_background.c -ltstrans -lpthread
 *
 * Run:
 *   LD_LIBRARY_PATH=target/debug /tmp/managed_reconnect_background
 *   # stats lines show reconnecting=1 attempts=1 gap_len=4 and dropped_msgs
 *   # climbing by ~20 per line while the link is down; the final stats
 *   # line shows successes=1 and ~200 evictions. Verified 2026-09-06.
 *
 * Reading the output honestly (both this and the Rust twin behave the same
 * way, and the numbers vary run to run):
 *   - `reconnecting=1 attempts=1` persists well past the 300 ms outage:
 *     the single re-dial started while the port was closed rides libsrt's
 *     handshake retry until the listener answers (see Step 4), so the
 *     link typically comes back late in the run rather than at 300 ms.
 *   - The peer's round 1 often reports a clean close after 0 messages:
 *     the reconnect lands, the 4 queued chunks drain, and then close()
 *     runs — which is prompt (cancel-first) by contract, so whatever was
 *     still in flight on the brand-new link is torn down with it. A real
 *     pump runs long enough that this is a footnote; a batch job that
 *     must not lose the tail is the BLOCKING-mode case anyway.
 *
 * Requires: TST_HAS_SRT == 1 (set when the `srt` cargo feature is enabled).
 */

#define _GNU_SOURCE   /* pthread_timedjoin_np — see managed_reconnect.c */

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
 * NUM_FRAMES × FRAME_INTERVAL_US decide how many messages land while the
 * link is down. At one video AU + one KLV record per iteration and a 10 ms
 * cadence, the ~300 ms outage below comfortably produces enough sends to
 * overflow the (deliberately tiny) gap buffer many times over, so the
 * stats reliably show DROP_OLDEST evicting messages rather than leaving
 * that to timing luck.
 */
#define NUM_FRAMES         120
#define FRAME_INTERVAL_US  10000
#define FRAME_PTS_STEP     900     /* 90 kHz / 100 fps, matching the 10 ms cadence */
#define STATS_EVERY        10      /* frames between stats lines (~100 ms) */

/*
 * Two peer rounds. Round 0 drops after 5 messages and then holds the port
 * CLOSED for OUTAGE_MS before round 1 re-binds — a real, sustained outage
 * where the sink is genuinely unreachable, not a one-frame blip. That is
 * what makes the send loop's steady cadence through it meaningful.
 */
#define PEER_ROUNDS        2
#define PEER_DROP_AFTER    5
#define OUTAGE_MS          300

#define LATENCY_MS         "120"
#define PEER_JOIN_SECS     10

#define PMT_PID            0x1000
#define VIDEO_PID          0x1011
#define KLV_PID            0x1031

/* ── Helpers (identical to managed_reconnect.c) ────────────────────────── */

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

/* Annex-B start code + IDR NAL header + filler; see managed_reconnect.c. */
static size_t make_nal(uint8_t *buf, size_t filler) {
    buf[0] = 0x00; buf[1] = 0x00; buf[2] = 0x00; buf[3] = 0x01; buf[4] = 0x65;
    memset(buf + 5, 0xAA, filler);
    return 5 + filler;
}

/* ST 0601 UL + BER short length + filler; see managed_reconnect.c. */
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
    atomic_int ready;
    int rounds_done;
    int failed;
};

/*
 *   round 0: accept → read 5 messages → close → port stays CLOSED for
 *            OUTAGE_MS (the sender's worker is re-dialling into nothing —
 *            see the "one attempt" note in Step 4 for why that shows up
 *            as attempts=1 rather than a climbing counter)
 *   round 1: re-bind → accept → read until the sender's clean close
 */
static void *flaky_peer(void *arg) {
    struct peer_ctx *ctx = arg;
    uint8_t buf[1500];

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
                    fprintf(stderr, "peer: round %d dropping after %d messages; "
                                    "port closed for %d ms\n",
                            round, messages, OUTAGE_MS);
                    break;
                }
            } else if (rc == TST_E_END_OF_STREAM) {
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
        /* Closes the connection; nothing is listening until the next
         * round's open re-binds — that gap IS the outage. */
        tst_raw_receiver_close(rx);
        ctx->rounds_done++;
        if (sender_closed || ctx->failed) {
            break;
        }
        if (round < PEER_ROUNDS - 1) {
            usleep(OUTAGE_MS * 1000);
        }
    }
    return NULL;
}

/* ── Stats line ────────────────────────────────────────────────────────── */

/*
 * In BACKGROUND mode this getter never waits on the reconnect loop — the
 * counters live on a side channel that stays readable across the gap. (In
 * BLOCKING mode it would contend on the lock the inline reconnect holds
 * for the whole outage; polling it live is a BACKGROUND-mode property.)
 *
 * `reconnecting` is true for the whole time a worker is recovering; it
 * does NOT mean "the link is down right now" so much as "a worker owns
 * the link right now" — read it as "either connected or recovering, but
 * not abandoned".
 */
static void print_stats(tst_managed_mux_sender_t *s, int frame) {
    tst_managed_transport_stats_t st;
    if (tst_managed_mux_sender_get_reconnect_stats(s, &st) != 0) {
        return;
    }
    fprintf(stderr, "sender: frame %3d reconnecting=%d gap_len=%llu attempts=%llu "
                    "successes=%llu dropped_msgs=%llu dropped_bytes=%llu\n",
            frame, st.reconnecting ? 1 : 0,
            (unsigned long long)st.gap_len,
            (unsigned long long)st.reconnect_attempts,
            (unsigned long long)st.reconnect_successes,
            (unsigned long long)st.gap_messages_dropped,
            (unsigned long long)st.gap_bytes_dropped);
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(void) {
    /* ── Step 1: Port, URLs, peer thread (as in managed_reconnect.c) ─── */
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
    char connect_url[256];
    snprintf(connect_url, sizeof(connect_url),
             "srt://127.0.0.1:%u?latency=%s", (unsigned)port, LATENCY_MS);

    pthread_t peer_thread;
    if (pthread_create(&peer_thread, NULL, flaky_peer, &peer) != 0) {
        perror("pthread_create");
        return 1;
    }
    while (atomic_load(&peer.ready) == 0) {
        usleep(1000);
    }
    usleep(50 * 1000);

    /*
     * ── Step 2: ReconnectPolicy — same knobs as managed_reconnect.c, plus
     *            mode, which is the entire point of this example ─────────
     *
     *   mode = BACKGROUND
     *     Reconnect runs on a dedicated worker instead of the caller's
     *     thread. send_video / send_klv never wait for the link: while the
     *     worker is reconnecting they enqueue into the gap buffer under
     *     overflow_policy and return 0.
     *
     *   max_attempts = 20
     *     Bounds ONE continuous outage; the budget resets after every
     *     successful reconnect, as in BLOCKING mode. If the worker exhausts
     *     it (or dies), the NEXT send reports that once as TST_E_TRANSPORT
     *     ("reconnect gave up after N attempts" / "background reconnect
     *     aborted") instead of 0; that call's bytes are not queued and the
     *     caller owns the resend decision. Production code polling the
     *     stats should watch attempts climbing toward this ceiling.
     *
     *   backoff = exponential 50 ms → 500 ms cap
     *     The pause BETWEEN separate re-dial attempts. See the note in
     *     Step 4 on why this demo usually shows attempts=1 anyway.
     *
     *   gap_buffer_capacity = 4
     *     Deliberately tiny (production default 256, see the sibling). A
     *     capacity this small guarantees the messages produced during the
     *     outage overflow it many times over, so the stats reliably show
     *     eviction.
     *
     *   overflow_policy = DROP_OLDEST
     *     Evict the oldest queued chunk when full. This is where
     *     "0 != delivered" comes from: send_video returns 0 for a frame
     *     that is queued and then evicted before the link returns — the
     *     call succeeded at ACCEPTING the bytes, not at delivering them. A
     *     caller that only checks the return code cannot see frames going
     *     missing; that is what the reconnect stats are for.
     */
    tst_reconnect_policy_t *policy = tst_reconnect_policy_new();
    if (!policy) {
        fprintf(stderr, "tst_reconnect_policy_new: out of memory\n");
        return 1;
    }
    (void) tst_reconnect_policy_set_mode(policy, TST_RECONNECT_MODE_BACKGROUND);
    (void) tst_reconnect_policy_set_max_attempts(policy, 20);
    (void) tst_reconnect_policy_set_backoff_exponential_ms(policy, 50, 500);
    (void) tst_reconnect_policy_set_gap_buffer_capacity(policy, 4);
    (void) tst_reconnect_policy_set_overflow_policy(policy, TST_OVERFLOW_POLICY_DROP_OLDEST);

    /* ── Step 3: Mux config ──────────────────────────────────────────── */
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
     * ── Step 4: Open ─────────────────────────────────────────────────────
     *
     * The initial connect is synchronous, as in BLOCKING mode — BACKGROUND
     * only changes how SUBSEQUENT reconnects behave.
     *
     * A note on what "one attempt" means: SRT's own connect handshake
     * retries internally while waiting for a listener to answer, so a
     * single re-dial started while the peer is unreachable can stay in
     * flight for the whole outage — it does not fail fast the way a TCP
     * connect to a closed port would. That is why this demo typically
     * shows attempts=1 for the entire gap: the backoff governs the pause
     * BETWEEN re-dials, not how long any single one may hang, and here the
     * first one simply does not return until the peer is back. A
     * production deployment that wants backoff to visibly drive several
     * short attempts adds `?conntimeo=<ms>` to the URL so each re-dial
     * fails fast instead of riding SRT's handshake retry.
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
     * ── Step 5: Produce at a steady cadence through the outage ───────────
     *
     * In BACKGROUND mode a 0 return means "accepted" — sent live, or
     * queued in the gap buffer. It does NOT mean the bytes reached the
     * peer. The only non-zero this loop can see is the give-up report
     * described in Step 2; ordinary transient outages never surface here,
     * unlike BLOCKING mode where every outage is visible as a stalled call.
     */
    fprintf(stderr, "sender: sending %d frames at %d ms; peer goes unreachable for ~%d ms "
                    "after %d messages\n",
            NUM_FRAMES, FRAME_INTERVAL_US / 1000, OUTAGE_MS, PEER_DROP_AFTER);
    uint8_t nal[1024];
    uint8_t klv[128];
    int sent_ok = 0;
    int sent_err = 0;
    for (int i = 0; i < NUM_FRAMES; i++) {
        const int64_t pts = (int64_t)i * FRAME_PTS_STEP;
        const size_t nal_len = make_nal(nal, 800);
        const size_t klv_len = make_klv(klv, 64, (uint8_t)i);

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
        if (i % STATS_EVERY == 0) {
            print_stats(s, i);
        }
        usleep(FRAME_INTERVAL_US);
    }
    fprintf(stderr, "sender: %d OK, %d errored across the run\n", sent_ok, sent_err);

    tst_managed_transport_stats_t final_st;
    memset(&final_st, 0, sizeof(final_st));
    int have_stats = (tst_managed_mux_sender_get_reconnect_stats(s, &final_st) == 0);
    tst_managed_mux_sender_close(s);
    s = NULL;

    /* ── Step 6: Bounded join (see managed_reconnect.c for the why) ───── */
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += PEER_JOIN_SECS;
    if (pthread_timedjoin_np(peer_thread, NULL, &deadline) == ETIMEDOUT) {
        fprintf(stderr, "FAIL: peer thread did not exit within %d s (rounds_done=%d)\n",
                PEER_JOIN_SECS, peer.rounds_done);
        fflush(stderr);
        _exit(5);
    }

    if (have_stats) {
        fprintf(stderr, "final stats: attempts=%llu successes=%llu gap_len=%llu "
                        "dropped_msgs=%llu dropped_bytes=%llu\n",
                (unsigned long long)final_st.reconnect_attempts,
                (unsigned long long)final_st.reconnect_successes,
                (unsigned long long)final_st.gap_len,
                (unsigned long long)final_st.gap_messages_dropped,
                (unsigned long long)final_st.gap_bytes_dropped);
    }
    if (peer.failed || peer.rounds_done != PEER_ROUNDS) {
        fprintf(stderr, "FAIL: peer completed %d of %d rounds (failed=%d)\n",
                peer.rounds_done, PEER_ROUNDS, peer.failed);
        return 4;
    }
    printf("OK: completed run with a background reconnect "
           "(sent_ok=%d, sent_err=%d, successes=%llu, dropped_msgs=%llu)\n",
           sent_ok, sent_err,
           (unsigned long long)final_st.reconnect_successes,
           (unsigned long long)final_st.gap_messages_dropped);
    return 0;
}
