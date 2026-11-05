/*
 * send_rtsp_server.c — RTSP server: synthesize H.264 + KLV → publish
 *   to multiple RTSP clients simultaneously via a unicast mount.
 *
 * This example demonstrates the RTSP server publish path:
 *
 *   1. Builder chain: tst_rtsp_server_builder_new(bind_url)
 *      → _max_sessions → _session_timeout → _fanout_capacity
 *      → _graceful_shutdown_drain_ms
 *   2. tst_rtsp_server_builder_start(builder) — spawns an internal tokio
 *      Runtime, binds the listener, and begins accepting connections.
 *   3. tst_rtsp_server_add_unicast_mount(server, "/live", mux_cfg) — registers
 *      a named "stream path" on the server so that RTSP clients can connect
 *      with  rtsp://<host>:8554/live  and receive the same TS feed.
 *   4. A push loop that synthesizes H.264 NAL frames + MISB ST 0601 KLV
 *      and feeds them via tst_rtsp_mount_push_video / _push_klv with
 *      monotonically-increasing 90 kHz PTS values.
 *   5. SIGINT handling: a cancel handle obtained before the push loop is
 *      triggered on Ctrl-C; the main thread detects the cancellation,
 *      drains gracefully, and exits cleanly.
 *
 * HOW THE RTSP SERVER DIFFERS FROM tst_mux_sender_t (SRT):
 *   The SRT sender is a single-client, point-to-point push.  The RTSP server
 *   is a fan-out publisher: N clients can independently DESCRIBE/SETUP/PLAY
 *   the same stream path.  Internally the server maintains a broadcast
 *   channel (tokio::sync::broadcast) per mount; each push_* call deposits
 *   one chunk into the channel, and per-client tasks drain their own views.
 *   Lagging clients drop frames rather than back-pressuring the producer.
 *
 * HOW THE MOUNT PATH WORKS:
 *   tst_rtsp_server_add_unicast_mount copies the mux_cfg at registration time
 *   and creates one internal Muxer per mount.  The returned handle is the
 *   write side of the broadcast channel feeding that Muxer's output.
 *   Clients that PLAY the mount receive real-time RTP packets from the live
 *   muxer output — there is no server-side recording or ring buffer.
 *
 * GRACEFUL DRAIN (Notice 5402):
 *   When tst_rtsp_server_stop is called, the server sends an RFC 7826
 *   §13.5.1 "Server-Initiated TEARDOWN" ANNOUNCE over each active session's
 *   TCP control channel, giving clients a heads-up before dropping them.
 *   The drain_ms window (set here to 2000 ms) then waits for in-flight RTP
 *   packets to leave the network stack before closing the runtime.
 *   Passing drain_ms=0 to tst_rtsp_server_stop uses the drain window that
 *   was set at build time via tst_rtsp_server_builder_graceful_shutdown_drain_ms.
 *
 * Build (from the ts-transformer workspace root). The committed header's
 * config declares no transport, so including bindings/c/include directly
 * leaves the RTSP declarations out; build against a packaged copy instead:
 *   bash scripts/dev/c-package.sh --features rtp --out /tmp/tstrans-pkg
 *   cc -I /tmp/tstrans-pkg/include -L /tmp/tstrans-pkg/lib \
 *      -Wl,-rpath,/tmp/tstrans-pkg/lib -Wall -Werror \
 *      -o /tmp/send_rtsp_server \
 *      bindings/c/examples/sending/send_rtsp_server.c \
 *      -ltstrans -lpthread
 *
 * Run (server):
 *   /tmp/send_rtsp_server \
 *       --bind rtsp://0.0.0.0:8554 --mount /live
 *
 * Connect a client (second terminal):
 *   ffplay rtsp://127.0.0.1:8554/live
 *   # or
 *   vlc rtsp://127.0.0.1:8554/live
 *   # or with GStreamer:
 *   gst-launch-1.0 rtspsrc location=rtsp://127.0.0.1:8554/live ! decodebin ! autovideosink
 *
 * Stop the server:
 *   Ctrl-C (SIGINT) — triggers graceful drain + Notice 5402 to active clients.
 *
 * Mirrors examples/sending/send_pipeline_to_socket.rs (Rust) but uses the
 * RTSP server transport instead of the SRT point-to-point transport.
 */

#include "tstrans.h"

#include <signal.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* ── Signal handling ────────────────────────────────────────────────────────
 *
 * WHY a cancel handle instead of calling tst_rtsp_server_stop from the
 * signal handler?
 *   Signal handlers have tight constraints on which functions are async-
 *   signal-safe; POSIX only guarantees a small set (write, _exit, etc.).
 *   tst_rtsp_server_stop blocks waiting for sessions to drain — calling a
 *   blocking library function from a signal handler causes undefined behavior.
 *
 *   Instead we:
 *     1. Obtain a tst_rtsp_cancel_handle_t before the push loop.
 *     2. Call tst_rtsp_cancel_handle_cancel from the handler — this sets an
 *        AtomicBool flag (async-signal-safe) and is effectively instant.
 *     3. Back on the main thread, detect cancellation when push_video returns
 *        TST_E_CLOSED, then call tst_rtsp_server_stop with a drain window.
 *
 * WHY volatile sig_atomic_t not the cancel handle directly?
 *   The signal handler also needs to break the main push loop so that
 *   tst_rtsp_server_stop isn't called from inside the handler.  A plain flag
 *   is the simplest cross-platform mechanism for this.
 */
static volatile sig_atomic_t g_stop_requested = 0;
/* Set by main() before the handler can run; cleared by main() before it frees
 * the handle.  The handler runs only on the main thread (see main()). */
static struct tst_rtsp_cancel_handle_t *volatile g_cancel_handle = NULL;

static void sigint_handler(int sig) {
    (void)sig;
    g_stop_requested = 1;
    /* Read the global once: test and use must see the same value. */
    struct tst_rtsp_cancel_handle_t *h = g_cancel_handle;
    /* tst_rtsp_cancel_handle_cancel is atomic-flag-set — safe in a handler. */
    if (h) {
        tst_rtsp_cancel_handle_cancel(h);
    }
}

/* ── Synthetic H.264 NAL ────────────────────────────────────────────────────
 *
 * A minimal Annex-B NAL unit.  The byte layout is:
 *   [00 00 00 01] — 4-byte Annex-B start code
 *   [65]          — nal_unit_type 0x65 = IDR slice (H.264 §7.3.1)
 *   [AA AA ... ]  — 0xAA filler payload
 *
 * WHY 0x65 for every frame?
 *   For a standalone server example without a real encoder, using IDR type
 *   for every frame keeps the stream self-contained: a connecting client that
 *   joins mid-stream can immediately decode because every frame is a random-
 *   access point.  In production you would interleave P-frames (0x61) and set
 *   key_frame=false for them; the muxer's random_access_indicator in the TS
 *   adaptation field is set from the key_frame parameter, not the NAL type.
 *
 * WHY 512 bytes?
 *   A 512-byte NAL crosses three TS packets (188 - ~14 PES header = 174
 *   payload bytes per packet, 512 / 174 ≈ 3 packets).  This exercises the
 *   muxer's PES fragmentation path without generating so much data that the
 *   loopback saturates before a client can PLAY.
 */
#define NAL_SIZE 512

static void make_nal(uint8_t *buf, size_t len) {
    if (len < 5) return;
    buf[0] = 0x00; buf[1] = 0x00; buf[2] = 0x00; buf[3] = 0x01;
    buf[4] = 0x65; /* IDR slice */
    memset(buf + 5, 0xAA, len - 5);
}

/* ── Synthetic MISB ST 0601 KLV ─────────────────────────────────────────────
 *
 * Layout:
 *   [16 bytes] UAS Datalink LS Universal Label (SMPTE ST 0601 UL)
 *   [1  byte]  BER short-form length = 18 (the payload below)
 *   [1  byte]  tag 0x02 (Precision Time Stamp)
 *   [1  byte]  length 0x08 (8-byte microsecond epoch)
 *   [8  bytes] microsecond timestamp from `pts_90khz`
 *   [1  byte]  tag 0x01 (Checksum)
 *   [1  byte]  length 0x02
 *   [2  bytes] CRC-16/CCITT-false checksum placeholder (0x00 0x00)
 *
 * Total: 16 + 1 + 1 + 1 + 8 + 1 + 1 + 2 = 31 bytes.
 *
 * WHY tag 0x02 (Precision Time Stamp) and tag 0x01 (Checksum)?
 *   ST 0601.19 mandates that every KLV Local Set include both a Checksum
 *   (tag 1) and a Precision Time Stamp (tag 2) as the first two items.
 *   A conformant parser will reject packets missing these tags.  The checksum
 *   bytes here are zeroed (placeholder) — in production you compute the
 *   CRC-16/CCITT-false over the full LS bytes.
 *
 * WHY derive microseconds from pts_90khz?
 *   The MPEG-TS PTS uses a 90 kHz clock; ST 0601 timestamps are in
 *   microseconds since the Unix epoch.  Converting pts × (1000000/90000)
 *   ≈ pts × 11.11 keeps the embedded timestamp consistent with the TS PTS
 *   without requiring wall-clock knowledge.  For a real sensor, use the
 *   platform time source.
 */
#define KLV_SIZE 31

static size_t make_klv(uint8_t *buf, int64_t pts_90khz) {
    /* MISB ST 0601 Universal Label (16 bytes) */
    static const uint8_t ul[16] = {
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
        0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    };
    memcpy(buf, ul, 16);

    /* BER short-form length: 1+1+8 + 1+1+2 = 14 bytes of inner TLVs */
    buf[16] = 14;

    /* Tag 2: Precision Time Stamp (8 bytes, microseconds since epoch) */
    buf[17] = 0x02;
    buf[18] = 0x08;
    /* Convert 90 kHz ticks → microseconds (multiply by 100/9) */
    uint64_t us = (uint64_t)pts_90khz * 100 / 9;
    buf[19] = (uint8_t)(us >> 56);
    buf[20] = (uint8_t)(us >> 48);
    buf[21] = (uint8_t)(us >> 40);
    buf[22] = (uint8_t)(us >> 32);
    buf[23] = (uint8_t)(us >> 24);
    buf[24] = (uint8_t)(us >> 16);
    buf[25] = (uint8_t)(us >>  8);
    buf[26] = (uint8_t)(us >>  0);

    /* Tag 1: Checksum (2 bytes, placeholder — production should compute CRC) */
    buf[27] = 0x01;
    buf[28] = 0x02;
    buf[29] = 0x00; /* checksum high byte (placeholder) */
    buf[30] = 0x00; /* checksum low  byte (placeholder) */

    return KLV_SIZE;
}

/* ── CLI argument parsing ───────────────────────────────────────────────────
 *
 * Accepts:
 *   --bind  <rtsp://host:port>   (default: rtsp://0.0.0.0:8554)
 *   --mount <path>               (default: /live)
 *   --frames <n>                 (default: 0 = run until SIGINT)
 *
 * WHY parse manually instead of getopt?
 *   getopt is POSIX but the GNU long-option extension (getopt_long) is not
 *   available on all targets this project supports.  A simple --key value
 *   scan is portable to every C99 platform.
 */
static const char *parse_arg(int argc, char **argv, const char *key,
                              const char *dflt) {
    for (int i = 1; i + 1 < argc; i++) {
        if (strcmp(argv[i], key) == 0) return argv[i + 1];
    }
    return dflt;
}

static long parse_long(int argc, char **argv, const char *key, long dflt) {
    const char *v = parse_arg(argc, argv, key, NULL);
    return v ? atol(v) : dflt;
}

/* ── main ───────────────────────────────────────────────────────────────────*/

int main(int argc, char **argv) {
    /*
     * ── Pin signal delivery to this thread (do this before ANY tst_* call) ──
     *
     * Why block SIGINT/SIGTERM first?
     *   A new thread inherits the signal mask of the thread that creates it,
     *   and a process-directed signal is delivered to any thread that does
     *   not block it.  The library spawns threads inside its calls.  Blocking
     *   the two signals here, before anything is spawned, makes every thread
     *   created before the unblock start with them blocked, so the handler
     *   can only ever run on this thread.  A handler on this thread cannot
     *   overlap the teardown code below: it interrupts it between
     *   instructions.  The guarantee rests on the mask being inherited,
     *   which POSIX specifies for pthread_create and which the library's
     *   threads (created while the mask is blocked) therefore share.  The
     *   signals are unblocked again once the handler is installed and the
     *   library objects exist.
     */
    sigset_t sigs;
    sigemptyset(&sigs);
    sigaddset(&sigs, SIGINT);
    sigaddset(&sigs, SIGTERM);
    pthread_sigmask(SIG_BLOCK, &sigs, NULL);

    /* ── Step 0: parse CLI ──────────────────────────────────────────────── */
    const char *bind_url   = parse_arg(argc, argv, "--bind",  "rtsp://0.0.0.0:8554");
    const char *mount_path = parse_arg(argc, argv, "--mount", "/live");
    long max_frames        = parse_long(argc, argv, "--frames", 0);
    /* max_frames == 0 means "run until SIGINT" */

    fprintf(stderr,
            "send_rtsp_server: binding to %s, mount %s\n"
            "  (--bind <url>  --mount <path>  --frames <n>  to override)\n",
            bind_url, mount_path);

    /* ── Step 1: Build the mux config ───────────────────────────────────────
     *
     * The mux config here describes the MPEG-TS program structure that the
     * RTSP server will mux into RTP payloads.  It is the same builder used
     * for SRT (tst_mux_sender_open) and file output (tst_muxer_open_file).
     *
     * WHY call tst_mux_config_free after add_unicast_mount?
     *   add_unicast_mount *borrows* the config for this call — it clones what
     *   it needs internally.  Ownership stays with the caller; we free it as
     *   soon as the mount is registered.
     */
    tst_mux_config_t *cfg = tst_mux_config_new();
    if (!cfg) {
        fprintf(stderr, "tst_mux_config_new: out of memory\n");
        return 1;
    }

    tst_program_handle_t prog = tst_mux_config_add_program(cfg, 1, 0x1000);
    if (prog == TST_INVALID_PROGRAM_HANDLE) {
        fprintf(stderr, "add_program failed: %s\n", tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 2;
    }

    /*
     * Add video (H.264) + KLV (synchronous metadata with PTS).
     *
     * WHY TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA with carries_pts=true?
     *   Synchronous KLV (stream_type 0x15 / PES with PTS) lets downstream
     *   RTSP clients time-align KLV records with video frames using the
     *   shared 90 kHz clock.  This is the ST 1402 / ST 1910.1 recommended
     *   carriage mode for mission-critical KLV.  Async (0x06) works too but
     *   gives receivers no timestamp anchor for KLV correlation.
     *
     * WHY PID 0x1011 (video) and 0x1031 (KLV)?
     *   Same convention as mux_synthetic_srt.c — spreading by 0x20 keeps PIDs
     *   visually distinct in a Wireshark / TSDuck capture.
     */
    tst_mux_config_add_video_stream(cfg, prog, 0x1011, TST_VIDEO_CODEC_H264);
    tst_mux_config_add_klv_stream(cfg, prog, 0x1031,
                                   TST_KLV_STREAM_TYPE_SYNCHRONOUS_METADATA,
                                   /*carries_pts=*/true);

    /* ── Step 2: Build and start the RTSP server ────────────────────────────
     *
     * tst_rtsp_server_builder_new allocates the builder and sets the bind URL.
     * The host must be a literal IP address (not a hostname) — the kernel
     * performs the bind, not a DNS resolver.
     *
     * Each setter returns void; errors are recorded in the thread-local last-
     * error and surfaced when _start is called.  This makes the builder chain
     * compact: configure freely, check once at _start.
     *
     * Builder knobs set here:
     *   max_sessions(8)             — cap at 8 concurrent RTSP clients; beyond
     *                                 that, new TCP connections are accepted and
     *                                 immediately closed (avoids OS backlog).
     *   session_timeout(30)         — 30 s keepalive; clients must ping at
     *                                 ≤15 s intervals or risk being dropped.
     *   fanout_capacity(512)        — broadcast channel depth per mount.
     *                                 A lagging client drops frames at 512+1
     *                                 rather than back-pressuring the producer.
     *   graceful_shutdown_drain_ms  — pre-shutdown drain window; we set it here
     *                                 and pass drain_ms=0 to _stop so it uses
     *                                 this value.  Could also pass 2000 directly
     *                                 to _stop and skip this setter.
     */
    struct TstRtspServerBuilder *builder =
        tst_rtsp_server_builder_new(bind_url);
    if (!builder) {
        fprintf(stderr, "tst_rtsp_server_builder_new failed: %s\n",
                tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 3;
    }

    tst_rtsp_server_builder_max_sessions(builder, 8);
    tst_rtsp_server_builder_session_timeout(builder, 30);
    tst_rtsp_server_builder_fanout_capacity(builder, 512);
    tst_rtsp_server_builder_graceful_shutdown_drain_ms(builder, 2000);

    /*
     * _start consumes the builder on both success and failure paths.
     * After this call, `builder` is invalid — do not free it separately.
     *
     * On success: the internal tokio Runtime is live and the TCP listener
     * is already bound.  Clients can start connecting immediately.
     *
     * On failure: the builder was freed by _start; check last-error for the
     * TST_E_* code.  Common causes: port already in use (EADDRINUSE), bad URL
     * scheme (only rtsp:// and rtsps:// are valid), or host is a DNS name
     * rather than a literal IP.
     */
    struct TstRtspServer *server =
        tst_rtsp_server_builder_start(builder);
    builder = NULL; /* consumed — prevent accidental reuse */
    if (!server) {
        fprintf(stderr, "tst_rtsp_server_builder_start failed: %s\n",
                tst_get_last_error_str());
        tst_mux_config_free(cfg);
        return 4;
    }

    fprintf(stderr, "server listening on %s\n", bind_url);

    /* ── Step 3: Register a unicast mount ───────────────────────────────────
     *
     * A "mount" is a named stream path that clients connect to.  Multiple
     * mounts can coexist on one server (e.g. "/live/eo", "/live/ir", "/ops").
     * Here we register one: "/live".
     *
     * Clients that issue  RTSP DESCRIBE rtsp://<host>:8554/live  will receive
     * an SDP describing the program from the mux_cfg we supply here.
     *
     * WHY is cfg still valid here after we finished configuring it?
     *   add_unicast_mount borrows the config — it does NOT consume it.
     *   We free cfg ourselves after this call.
     *
     * The returned TstRtspMountHandle is the write side of the broadcast
     * fanout.  Push frames into it with tst_rtsp_mount_push_video / _push_klv.
     */
    struct TstRtspMountHandle *mount =
        tst_rtsp_server_add_unicast_mount(server, mount_path, cfg);
    tst_mux_config_free(cfg);
    cfg = NULL; /* prevent accidental use after free */
    if (!mount) {
        fprintf(stderr, "add_unicast_mount failed: %s\n",
                tst_get_last_error_str());
        tst_rtsp_server_free(server);
        return 5;
    }

    fprintf(stderr, "mount registered: %s%s\n", bind_url, mount_path);
    fprintf(stderr, "waiting for clients… (Ctrl-C to stop)\n");

    /* ── Step 4: Obtain a cancel handle for signal-safe shutdown ────────────
     *
     * The cancel handle wraps an Arc<AtomicBool> shared with the server.
     * tst_rtsp_cancel_handle_cancel sets that flag — safe to call from a
     * signal handler.  The server's internal tasks observe the flag and stop
     * accepting new frames; active sessions see TST_E_CLOSED on the next push.
     *
     * This is NOT the same as tst_rtsp_server_stop: cancel is instant and
     * does not send Notice 5402; stop sends Notice 5402 and waits for drain.
     * We use cancel from the signal handler to wake up the push loop quickly,
     * then call stop from the main thread for a clean shutdown.
     */
    g_cancel_handle = tst_rtsp_server_cancel_handle(server);
    if (!g_cancel_handle) {
        fprintf(stderr, "tst_rtsp_server_cancel_handle failed: %s\n",
                tst_get_last_error_str());
        /* Non-fatal: we can still run without signal-safe cancel; SIGINT will
         * terminate the process hard.  Proceed anyway for demonstration. */
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = sigint_handler;
    sigaction(SIGINT,  &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);
    /* Deliver the signals to this thread now that the handler is installed
     * and the library's threads exist (they keep the blocked mask). */
    pthread_sigmask(SIG_UNBLOCK, &sigs, NULL);

    /* ── Step 5: Push loop ──────────────────────────────────────────────────
     *
     * PTS clock: MPEG-TS uses 90 kHz.  At 30 fps: 90000 / 30 = 3000 ticks.
     * Wall-clock sleep: usleep(33 ms) approximates 30 fps cadence.
     *
     * WHY not use a precise timer?
     *   This example demonstrates the push API, not a production-quality
     *   clock-sync loop.  A real encoder drives the push from its frame
     *   callback; for file-based re-streaming you would pace to the source TS
     *   PCR values.  The 33 ms sleep is intentionally approximate.
     *
     * WHY key_frame=true every 30 frames (every ~1 s at 30 fps)?
     *   In production, only the actual IDR/I-frame from the encoder is a
     *   keyframe.  Here, with synthetic NALs, we periodically mark a frame as
     *   a keyframe so that late-joining RTSP clients have a chance to find a
     *   random-access point quickly (most decoders wait for the next keyframe
     *   before producing output).
     *
     * Loop termination:
     *   - If max_frames > 0: stop after that many frames.
     *   - Otherwise: stop when g_stop_requested is set (SIGINT) or when
     *     tst_rtsp_mount_push_video returns TST_E_CLOSED (cancel handle fired).
     */
    uint8_t nal[NAL_SIZE];
    uint8_t klv[KLV_SIZE];
    int64_t pts = 0;
    long frame_num = 0;
    int exit_code = 0;

    while (!g_stop_requested) {
        if (max_frames > 0 && frame_num >= max_frames) break;

        make_nal(nal, sizeof(nal));
        size_t klv_len = make_klv(klv, pts);
        bool keyframe = (frame_num % 30 == 0);

        /*
         * Push video first, then KLV.  The muxer timestamps them to the same
         * PTS so downstream demuxers correlate them by timestamp.
         *
         * Returns:
         *   0             — success
         *   TST_E_CLOSED  — mount was cancelled (signal handler or error)
         *   TST_E_RTSP_MOUNT — muxer or fanout error
         */
        int rc = tst_rtsp_mount_push_video(mount, nal, sizeof(nal), pts, keyframe);
        if (rc != 0) {
            if (rc == TST_E_CLOSED) {
                fprintf(stderr, "mount closed (shutdown requested)\n");
            } else {
                fprintf(stderr, "push_video[%ld] failed (code %d): %s\n",
                        frame_num, rc, tst_get_last_error_str());
                exit_code = 6;
            }
            break;
        }

        rc = tst_rtsp_mount_push_klv(mount, klv, klv_len, pts);
        if (rc != 0 && rc != TST_E_CLOSED) {
            fprintf(stderr, "push_klv[%ld] failed (code %d): %s\n",
                    frame_num, rc, tst_get_last_error_str());
            exit_code = 7;
            break;
        }

        /* Print stats every 150 frames (~5 s at 30 fps) */
        if (frame_num % 150 == 0) {
            tst_mount_stats_t mstats = {0};
            if (tst_rtsp_mount_get_stats(mount, &mstats) == 0) {
                tst_server_stats_t sstats = {0};
                tst_rtsp_server_get_stats(server, &sstats);
                fprintf(stderr,
                        "  frame %-6ld | clients=%llu | "
                        "pushed=%llu B / %llu pkts | dropped=%llu\n",
                        frame_num,
                        (unsigned long long)sstats.active_sessions,
                        (unsigned long long)mstats.bytes_pushed,
                        (unsigned long long)mstats.packets_pushed,
                        (unsigned long long)mstats.frames_dropped_total);
            }
        }

        pts += 3000; /* 90 kHz / 30 fps = 3000 ticks per frame */
        frame_num++;
        usleep(33 * 1000); /* ≈ 30 fps wall-clock pacing */
    }

    fprintf(stderr, "pushed %ld frames, stopping server…\n", frame_num);

    /* ── Step 6: Graceful shutdown ──────────────────────────────────────────
     *
     * Two-phase:
     *   tst_rtsp_server_stop  — sends Notice 5402 to active sessions, fires
     *                           the global cancel token, waits for the
     *                           sessions to end (the drain_ms argument is
     *                           ignored; the 2000 ms drain set at build time
     *                           applies, plus a 500 ms margin), then shuts
     *                           each session's write half.
     *   tst_rtsp_server_free  — frees the handle. The server's tokio
     *                           runtime shuts down when the last reference
     *                           to the server drops: after _stop that is
     *                           inside _stop itself (or a call still
     *                           parked), otherwise inside _free. Either
     *                           way the shutdown is bounded at 5 s; it does
     *                           not wait for every task without limit.
     *
     * WHY call both _stop and _free?
     *   _stop sends the graceful TEARDOWN notices and drains; _free then
     *   deallocates.  Calling _free directly (without _stop) is also safe and
     *   does a hard-cancel drop — use it when fast process exit is acceptable.
     *
     * Cleanup order:
     *   1. Free mount handle (cancels future pushes, but server still running)
     *   2. Free cancel handle
     *   3. Stop server (drain + Notice 5402)
     *   4. Free server
     */
    tst_rtsp_mount_handle_free(mount);
    mount = NULL;

    /*
     * Why this order?
     *   The handler dereferences g_cancel_handle.  It runs only on this
     *   thread, so it cannot overlap this code; what remains is a signal
     *   arriving AFTER the free.  (1) SIG_DFL first: a late Ctrl-C just ends
     *   the process instead of running the handler.  (2) Clear the global,
     *   then (3) free the handle.
     */
    struct sigaction dfl;
    memset(&dfl, 0, sizeof dfl);
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    sigaction(SIGINT, &dfl, NULL);
    sigaction(SIGTERM, &dfl, NULL);
    struct tst_rtsp_cancel_handle_t *cancel = g_cancel_handle;
    g_cancel_handle = NULL;
    if (cancel) {
        tst_rtsp_cancel_handle_free(cancel);
    }

    int stop_rc = tst_rtsp_server_stop(server, /*drain_ms=*/0);
    if (stop_rc != 0) {
        fprintf(stderr, "tst_rtsp_server_stop warning (code %d): %s\n",
                stop_rc, tst_get_last_error_str());
    }

    tst_rtsp_server_free(server);
    server = NULL;

    fprintf(stderr, "done.\n");
    return exit_code;
}
