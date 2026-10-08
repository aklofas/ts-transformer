/*
 * recv_rtsp_publish.c — RTSP publisher ingest: let encoders PUSH into the
 * library's RTSP server, then demux what they push.
 *
 * Why this example:
 *   The other RTSP examples pull: the application is the RTSP client and
 *   fetches a stream with DESCRIBE / SETUP / PLAY (recv_rtsp_camera.c), or
 *   the application is the server and pushes its own frames to PLAY
 *   readers (send_rtsp_server.c).  This one inverts the ingest direction.
 *   The application runs the server, and a publisher (ffmpeg, GStreamer, a
 *   hardware encoder) connects and pushes with ANNOUNCE / SETUP
 *   `mode=record` / RECORD (RFC 2326 §10.3, §10.11).  Each pushed stream
 *   reaches the application as MPEG-TS through an ordinary
 *   `TstRtpDemuxReceiver`, so the usual `tst_rtp_demux_receiver_next_event`
 *   loop reads it, and the same mount keeps serving PLAY readers
 *   (`ffplay rtsp://host:8554/<name>`) from the published bytes.
 *
 *   Whatever shape the publisher pushes, the application reads MPEG-TS:
 *   an MPEG-TS-over-RTP publisher's bytes pass straight through, and
 *   elementary H.264 (+ KLV) tracks are re-muxed by the server into one
 *   program (video PID 0x100, KLV PID 0x101).
 *
 * What it does:
 *   1. Binds rtsp://0.0.0.0:8554 (or the URL in argv[1]) with
 *      `tst_rtsp_server_builder_accept_unregistered_publishers(true)`: any
 *      name a publisher announces is created on demand.
 *   2. Prints the bound address with `tst_rtsp_server_local_addr` (useful
 *      with port 0, where the kernel picks the port).
 *   3. Loops `tst_rtsp_server_next_publisher` with a 1 s timeout; each new
 *      mount gets a demux thread that prints video access units (the first
 *      and every 30th), the first KLV record, and discontinuities.
 *   4. Every 5 s prints the server's publisher counters and each mount's
 *      publisher + `tst_rtsp_publish_mount_stats_t`.
 *   5. Ctrl-C: stops the server, joins the demux threads, frees everything.
 *
 * Push commands (start this first; `demo` is any name, and each distinct
 * name becomes its own mount):
 *
 *   # ffmpeg, TCP-interleaved (elementary H.264, re-muxed by the server):
 *   ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 \
 *       -c:v libx264 -preset ultrafast -tune zerolatency \
 *       -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:8554/demo
 *
 *   # ffmpeg, UDP (same stream, RTP over separate UDP sockets):
 *   ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=15 \
 *       -c:v libx264 -preset ultrafast -tune zerolatency \
 *       -f rtsp -rtsp_transport udp rtsp://127.0.0.1:8554/demo
 *
 *   # GStreamer, MPEG-TS over RTP (video + KLV muxed by the publisher).
 *   # rtspclientsink payloads its input itself (rtpmp2tpay, MP2T/90000):
 *   gst-launch-1.0 filesrc location=in.ts ! tsparse set-timestamps=true \
 *       ! rtspclientsink location=rtsp://127.0.0.1:8554/demo
 *
 *   # GStreamer, elementary H.264 + KLV (two tracks, re-muxed by the server;
 *   # rtspclientsink picks rtph264pay and rtpklvpay).  The KLV PES packets
 *   # must carry a PTS (synchronous KLV does): untimed KLV leaves every KLV
 *   # RTP packet on one timestamp, which the server cannot place.
 *   gst-launch-1.0 filesrc location=in.ts ! tsdemux name=d \
 *       d. ! queue ! h264parse ! s.sink_0 \
 *       d. ! queue ! meta/x-klv ! s.sink_1 \
 *       rtspclientsink name=s location=rtsp://127.0.0.1:8554/demo
 *
 *   ffmpeg cannot push KLV over RTSP in any shape (it announces elementary
 *   tracks only and stops with `Unsupported codec klv`).  To get KLV into a
 *   publish mount, push from GStreamer as above, or send the TS over SRT or
 *   UDP instead.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c --features rtp
 *   cc -I target/debug/include -L target/debug -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror -o /tmp/recv_rtsp_publish \
 *      bindings/c/examples/receiving/recv_rtsp_publish.c -ltstrans -lpthread
 *   /tmp/recv_rtsp_publish                      # rtsp://0.0.0.0:8554
 *   /tmp/recv_rtsp_publish rtsp://127.0.0.1:0   # kernel-picked port
 *
 * Requires: TST_HAS_RTP == 1 (the `rtp` cargo feature).  Linux-only by
 * convention (POSIX signals + pthreads).
 *
 * Mirrors: examples/receiving/recv_rtsp_publish.rs (Rust).
 */

#include "tstrans.h"

#if !defined(TST_HAS_RTP) || TST_HAS_RTP == 0
#error "This example requires TST_HAS_RTP. Rebuild tst-c with the rtp cargo feature enabled."
#endif

#include <inttypes.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* -------------------------------------------------------------------------
 * Shutdown signalling.
 *
 * The handler may only touch async-signal-safe state, so it sets a
 * `volatile sig_atomic_t` flag (the one type POSIX guarantees is safe to
 * write from a handler) and fires the server's hard cancel, which only
 * stores an atomic flag (the same pattern as send_rtsp_server.c).
 *
 * WHY the hard cancel here, and WHY it is not the shutdown itself:
 *   The hard cancel makes the listener stop accepting within ~100 ms, so
 *   no new publisher slips in while the process is shutting down, and it
 *   aborts every RTSP session at its next poll.  It does NOT end a publish
 *   mount's application transport and does NOT wake a call parked in
 *   `tst_rtsp_server_next_publisher` (that call keeps timing out with
 *   TST_E_BUFFER_FULL).  Only `tst_rtsp_server_stop` does both, so the
 *   main loop notices the flag within one 1 s timeout and calls stop.
 *
 *   Trade-off: because the sessions are already aborted, publishers see
 *   their connection drop instead of the RTSP Notice 5402 that stop would
 *   send.  For the polite TEARDOWN, set only the flag in the handler and
 *   let `tst_rtsp_server_stop` do everything.
 * ---------------------------------------------------------------------- */
static volatile sig_atomic_t g_stop = 0;
static tst_rtsp_cancel_handle_t *g_cancel = NULL;

static void handle_signal(int sig) {
    (void) sig;
    g_stop = 1;
    if (g_cancel != NULL) {
        tst_rtsp_cancel_handle_cancel(g_cancel);
    }
}

/* -------------------------------------------------------------------------
 * One slot per publish mount the application is reading.
 *
 * The bound of 64 is this example's choice, not the library's.  The
 * library's own bounds: at most 64 on-demand mounts wait in the
 * `next_publisher` queue (an ANNOUNCE past that answers 503), and the
 * server holds at most 256 on-demand mounts (an ANNOUNCE past that also
 * answers 503).
 * ---------------------------------------------------------------------- */
#define MAX_MOUNTS 64

typedef struct mount_slot {
    tst_rtsp_publish_mount_t *mount; /* getters + free; never closes the mount */
    TstRtpDemuxReceiver *rx;         /* the mount's take-once transport */
    pthread_t thread;                /* runs demux_thread over `rx` */
    char path[256];                  /* copy: printed by the thread */
} mount_slot;

static const char *alignment_name(enum tst_rtsp_clock_alignment a) {
    switch (a) {
        case TST_RTSP_CLOCK_ALIGNMENT_NOT_APPLICABLE: return "not-applicable";
        case TST_RTSP_CLOCK_ALIGNMENT_PENDING:        return "pending";
        case TST_RTSP_CLOCK_ALIGNMENT_PROVISIONAL:    return "provisional";
        case TST_RTSP_CLOCK_ALIGNMENT_SENDER_REPORT:  return "sender-report";
    }
    return "unknown";
}

/* -------------------------------------------------------------------------
 * The per-mount demux loop.
 *
 * `tst_rtp_demux_receiver_next_event` blocks until one typed event is
 * ready.  The receiver outlives publisher churn: when a publisher leaves,
 * the receiver goes quiet; when the next one arrives on the same name,
 * bytes resume and the demuxer reports ordinary continuity
 * discontinuities.  It ends in exactly one of two ways:
 *
 *   TST_E_END_OF_STREAM (-12) — the mount was CLOSED: removed with
 *       `tst_rtsp_server_remove_mount`, or the server stopped.  What was
 *       already queued is delivered first.  This is how this example's
 *       threads end, because main calls `tst_rtsp_server_stop`.
 *   TST_E_CLOSED (-7)         — an explicit CANCEL:
 *       `tst_rtsp_publish_mount_cancel` or `tst_rtp_demux_receiver_cancel`.
 * ---------------------------------------------------------------------- */
static void *demux_thread(void *arg) {
    mount_slot *slot = arg;
    tst_event_t ev;
    uint64_t video_aus = 0, klv_records = 0;
    int rc;

    memset(&ev, 0, sizeof(ev));
    for (;;) {
        rc = tst_rtp_demux_receiver_next_event(slot->rx, &ev);
        if (rc != 0) {
            break;
        }
        switch (ev.kind) {
            case TST_EVENT_KIND_SAMPLE:
                if (ev.u.sample.stream_kind != TST_STREAM_KIND_VIDEO) {
                    break;
                }
                video_aus++;
                if (video_aus == 1 || video_aus % 30 == 0) {
                    /* `random_access_indicator` marks a keyframe; the
                     * first AU of a stream is usually one. */
                    printf("[%s] video AU #%" PRIu64 " PID=0x%04X codec=%d pts=%" PRId64
                           " bytes=%zu rai=%u\n",
                           slot->path, video_aus, ev.u.sample.pid, ev.u.sample.codec,
                           ev.u.sample.pts, ev.u.sample.payload_len,
                           (unsigned) ev.u.sample.random_access_indicator);
                }
                break;

            case TST_EVENT_KIND_METADATA:
                klv_records++;
                if (klv_records == 1) {
                    /* The payload is the bare KLV local set (any sync
                     * AU-cell wrapper already removed) and borrows from the
                     * receiver's arena until the next `_next_event`: copy
                     * it first to keep it.  recv_srt_events.c shows the
                     * typed MISB ST 0601 decode via `tst_st0601_decode`. */
                    printf("[%s] first KLV PID=0x%04X pts=%" PRId64 " bytes=%zu\n",
                           slot->path, ev.u.metadata.pid, ev.u.metadata.pts,
                           ev.u.metadata.payload_len);
                }
                break;

            case TST_EVENT_KIND_DISCONTINUITY:
                /* A publisher change shows up here as continuity breaks on
                 * each PID.  Which publisher a frame came from is on the
                 * mount: `tst_rtsp_publish_mount_publisher_info` and
                 * `_generation`. */
                printf("[%s] discontinuity PID=0x%04X kind=%d\n", slot->path,
                       ev.u.discontinuity.pid, ev.u.discontinuity.discontinuity_kind);
                break;

            default:
                break;
        }
    }

    if (rc == TST_E_END_OF_STREAM) {
        printf("[%s] stream ended (mount closed): %" PRIu64 " video AUs, %" PRIu64
               " KLV records\n", slot->path, video_aus, klv_records);
    } else if (rc == TST_E_CLOSED) {
        printf("[%s] receiver cancelled: %" PRIu64 " video AUs, %" PRIu64 " KLV records\n",
               slot->path, video_aus, klv_records);
    } else {
        fprintf(stderr, "[%s] next_event rc=%d: %s\n", slot->path, rc,
                tst_get_last_error_str());
    }
    return NULL;
}

/* -------------------------------------------------------------------------
 * Stats.  Every getter here is an out-parameter call returning 0 or a
 * negative TST_E_* code: the server counters read TST_E_CLOSED once the
 * server is stopped, the mount getters keep working on a closed mount.
 * ---------------------------------------------------------------------- */
static void print_stats(TstRtspServer *server, mount_slot *slots, size_t n) {
    uint64_t active = 0, packets = 0;
    if (tst_rtsp_server_active_publishers(server, &active) == 0 &&
        tst_rtsp_server_total_rtp_packets_received(server, &packets) == 0) {
        printf("server: active_publishers=%" PRIu64 " rtp_packets_received=%" PRIu64
               " mounts_read=%zu\n", active, packets, n);
    }
    for (size_t i = 0; i < n; i++) {
        tst_rtsp_publisher_info_t info;
        tst_rtsp_publish_mount_stats_t st;
        if (tst_rtsp_publish_mount_publisher_info(slots[i].mount, &info) != 0 ||
            tst_rtsp_publish_mount_get_stats(slots[i].mount, &st) != 0) {
            continue;
        }
        /* `present` is false while the mount is idle (between publishers);
         * `generation` counts publishers that have ended on the mount. */
        char who[128];
        if (info.present) {
            snprintf(who, sizeof(who), "publisher %s %s%s generation %" PRIu64, info.peer,
                     info.shape == TST_RTSP_PUBLISH_SHAPE_MP2T ? "mp2t" : "elementary",
                     info.klv ? "+klv" : "", info.generation);
        } else {
            snprintf(who, sizeof(who), "idle");
        }
        /* `alignment` says how KLV is placed on the video clock:
         * not-applicable for MPEG-TS and video-only publishers, pending while
         * KLV is held, sender-report once RTCP sender reports for both tracks
         * arrived, provisional after 2 s without them.  A nonzero
         * `aus_reordered` means the publisher sends B-frames, which the
         * re-muxed TS carries with a PTS and no DTS.  A growing
         * `frames_dropped_app` means this application stopped reading. */
        printf("[%s] %s; alignment=%s aus_emitted=%" PRIu64 " aus_dropped=%" PRIu64
               " aus_reordered=%" PRIu64 " klv_units_emitted=%" PRIu64
               " frames_emitted=%" PRIu64 " frames_dropped_app=%" PRIu64
               " readers=%" PRIu64 "\n",
               slots[i].path, who, alignment_name(st.alignment), st.aus_emitted,
               st.aus_dropped, st.aus_reordered, st.klv_units_emitted, st.frames_emitted,
               st.frames_dropped_app, st.peer_count);
    }
    fflush(stdout);
}

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double) ts.tv_sec + (double) ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    /* The server binds an IP literal (no DNS on the server side).
     * 0.0.0.0 accepts publishers on every interface; 8554 is the
     * conventional RTSP alternate port (554 needs privileges). */
    const char *url = argc > 1 ? argv[1] : "rtsp://0.0.0.0:8554";

    /* stdout is line-buffered on a terminal but fully buffered into a pipe
     * or file; line-buffer it so a redirected run shows progress live. */
    setvbuf(stdout, NULL, _IOLBF, 0);

    /* ── Step 1: build and start the server ─────────────────────────────
     *
     * `accept_unregistered_publishers(true)`: an ANNOUNCE on a name no
     * mount is registered under creates a publish mount there and queues
     * its handle for `next_publisher`.  With it off (the default) such an
     * ANNOUNCE answers 404, and the application registers names up front
     * with `tst_rtsp_server_add_publish_mount`.
     *
     * Security: with the flag on, anyone who can reach the port can create
     * mounts.  Add `tst_rtsp_server_builder_auth_basic` / `_auth_digest_*`
     * to require the server's credential (readers and publishers share it).
     *
     * `tst_rtsp_server_builder_start` consumes the builder whether it
     * succeeds or not. */
    TstRtspServerBuilder *builder = tst_rtsp_server_builder_new(url);
    if (builder == NULL) {
        fprintf(stderr, "builder_new(%s): %s\n", url, tst_get_last_error_str());
        return 1;
    }
    tst_rtsp_server_builder_accept_unregistered_publishers(builder, true);
    TstRtspServer *server = tst_rtsp_server_builder_start(builder);
    builder = NULL; /* consumed */
    if (server == NULL) {
        fprintf(stderr, "start(%s): %s\n", url, tst_get_last_error_str());
        return 1;
    }

    /* The bound address: with port 0 in the URL this is the only way to
     * learn the port.  Success returns the bytes written (excluding the
     * NUL), so test for a NEGATIVE result, not for nonzero.  64 bytes holds
     * any IPv4 or IPv6 address; a buffer too small for the address plus its
     * NUL is TST_E_INVALID_CONFIG and nothing is written (the same
     * convention as tst_hls_publisher_local_addr). */
    char addr[64];
    if (tst_rtsp_server_local_addr(server, addr, sizeof(addr)) < 0) {
        fprintf(stderr, "local_addr: %s\n", tst_get_last_error_str());
        tst_rtsp_server_free(server);
        return 1;
    }
    printf("listening on %s (push to rtsp://<host>:<port>/<name>; Ctrl-C to stop)\n", addr);

    /* ── Step 2: signals ────────────────────────────────────────────────
     *
     * The cancel handle is obtained BEFORE the handler is installed, so
     * the handler never sees a half-initialised pointer.  SA_RESTART is
     * left unset; nothing here depends on it either way, because the main
     * loop wakes every second regardless. */
    g_cancel = tst_rtsp_server_cancel_handle(server);
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = handle_signal;
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);

    /* ── Step 3: accept loop ────────────────────────────────────────────
     *
     * `tst_rtsp_server_next_publisher` waits up to the timeout for the next
     * on-demand mount:
     *   0                     — `mount` is a new handle (yours to free);
     *                           the announcing publisher already holds it.
     *   TST_E_BUFFER_FULL (-4) — the retryable "nothing arrived in time";
     *                           `mount` is NULL.
     *   TST_E_CLOSED (-7)     — the server was stopped.
     *
     * The 1 s timeout is what lets this loop notice the stop flag and print
     * stats on time; it is not a liveness bound on publishers.  Concurrent
     * callers are served one at a time, so a call made while another waits
     * can overrun its own timeout; this example has a single caller.
     *
     * A later ANNOUNCE on the same name (that publisher reconnecting, or a
     * new one after it left) reuses the mount and queues nothing: the
     * existing receiver simply resumes.  A second publisher while the first
     * is live is refused with 403.
     *
     * A handle can name a mount that was removed while it waited in the
     * queue; its receiver then reads TST_E_END_OF_STREAM at once, and the
     * thread below exits on its own. */
    static mount_slot slots[MAX_MOUNTS];
    size_t n_slots = 0;
    double last_stats = now_s();

    while (!g_stop) {
        tst_rtsp_publish_mount_t *mount = NULL;
        int rc = tst_rtsp_server_next_publisher(server, 1000, &mount);
        if (rc == 0) {
            const char *path = tst_rtsp_publish_mount_path(mount);
            printf("[%s] new publish mount\n", path);

            if (n_slots == MAX_MOUNTS) {
                /* Nobody would read this mount.  Removing it sends its
                 * publisher Notice 5402 and frees the name; merely freeing
                 * the handle would leave the mount registered and silently
                 * dropping frames (`frames_dropped_app`). */
                fprintf(stderr, "[%s] too many mounts; removing it\n", path);
                tst_rtsp_server_remove_mount(server, path);
                tst_rtsp_publish_mount_free(mount);
                continue;
            }

            /* Take the mount's transport as a demux receiver.  This is
             * take-once across every handle to the mount: a second call
             * returns NULL with TST_E_CLOSED.  NULL config = default demux
             * options.  The mount handle stays valid for the getters. */
            mount_slot *slot = &slots[n_slots];
            memset(slot, 0, sizeof(*slot));
            slot->mount = mount;
            snprintf(slot->path, sizeof(slot->path), "%s", path);
            slot->rx = tst_rtsp_publish_mount_into_demux_receiver(mount, NULL);
            if (slot->rx == NULL) {
                fprintf(stderr, "[%s] into_demux_receiver: %s\n", slot->path,
                        tst_get_last_error_str());
                /* Nobody can read this mount now, so remove it for the same
                 * reason as the MAX_MOUNTS branch above: a registered mount
                 * nobody drains keeps its publisher pushing into nothing. */
                tst_rtsp_server_remove_mount(server, path);
                tst_rtsp_publish_mount_free(mount);
                continue;
            }
            if (pthread_create(&slot->thread, NULL, demux_thread, slot) != 0) {
                fprintf(stderr, "[%s] pthread_create failed\n", slot->path);
                tst_rtp_demux_receiver_close(slot->rx);
                /* No thread will drain the mount: remove it (see above). */
                tst_rtsp_server_remove_mount(server, path);
                tst_rtsp_publish_mount_free(mount);
                continue;
            }
            n_slots++;
        } else if (rc == TST_E_BUFFER_FULL) {
            /* Timeout: nothing new.  Fall through to the stats check. */
        } else if (rc == TST_E_CLOSED) {
            fprintf(stderr, "server stopped\n");
            break;
        } else {
            fprintf(stderr, "next_publisher rc=%d: %s\n", rc, tst_get_last_error_str());
            break;
        }

        if (now_s() - last_stats >= 5.0) {
            last_stats = now_s();
            print_stats(server, slots, n_slots);
        }

        /* A real service would also expire idle names here: an on-demand
         * mount stays registered after its publisher leaves until
         * `tst_rtsp_server_remove_mount` removes it. */
    }

    /* ── Step 4: shutdown ───────────────────────────────────────────────
     *
     * Order matters:
     *   1. Final stats while the server still answers its counters.
     *   2. `tst_rtsp_server_stop`: sends each remaining session Notice 5402,
     *      closes every publish mount, and waits out the drain window.  A
     *      closed mount ends its receiver: each demux thread's
     *      `_next_event` returns TST_E_END_OF_STREAM once the queue drains.
     *      The hard cancel fired by the signal handler does NOT do this,
     *      which is why stop, not the cancel, ends the threads.
     *   3. Join the threads: no call is in flight on any receiver after
     *      this, so closing them is safe.
     *   4. Close each receiver and free each mount handle (independent
     *      objects: freeing the handle never closes the mount, and the
     *      receiver does not borrow from the handle).
     *   5. Free the cancel handle and the server last.  Never free the
     *      server while another thread is inside one of its calls. */
    print_stats(server, slots, n_slots);
    int rc = tst_rtsp_server_stop(server, 0);
    if (rc != 0) {
        fprintf(stderr, "stop rc=%d: %s\n", rc, tst_get_last_error_str());
    }
    for (size_t i = 0; i < n_slots; i++) {
        pthread_join(slots[i].thread, NULL);
        tst_rtp_demux_receiver_close(slots[i].rx);
        tst_rtsp_publish_mount_free(slots[i].mount);
    }

    /* Restore the default disposition before freeing the cancel handle,
     * so a late Ctrl-C cannot reach a freed pointer. */
    signal(SIGINT, SIG_DFL);
    signal(SIGTERM, SIG_DFL);
    tst_rtsp_cancel_handle_t *cancel = g_cancel;
    g_cancel = NULL;
    tst_rtsp_cancel_handle_free(cancel);
    tst_rtsp_server_free(server);
    printf("done\n");
    return rc == 0 ? 0 : 1;
}
