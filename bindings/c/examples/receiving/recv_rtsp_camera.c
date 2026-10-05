/*
 * recv_rtsp_camera.c — Full RTSP client lifecycle against a camera
 * endpoint: builder chain, optional Digest MD5 auth, transport preference
 * (UDP / TCP-interleaved / auto), PLAY, demux event loop, SIGINT cancel.
 *
 * Why this example:
 *   Gimbaled-platform cameras (EO/IR turrets on UAVs, helicopters, sensor
 *   pods) typically stream MPEG-TS over RTSP.  The camera acts as an RTSP
 *   server; ground stations, cloud ingest nodes, or relay daemons are the
 *   clients.  RTSP (RFC 7826) uses a symmetric request/response exchange over
 *   TCP for control (OPTIONS / DESCRIBE / SETUP / PLAY), then delivers the
 *   RTP data payload on a separate channel — either UDP (lower latency, no
 *   HOL blocking) or TCP-interleaved (firewall-friendly; RTP frames are
 *   interleaved on the control TCP connection as RFC 7826 §14 binary records).
 *
 *   Why Digest MD5 dominates for camera authentication:
 *     Cameras manufactured for ISR/maritime/public-safety use carry legacy
 *     firmware that implements RFC 2617 Digest (MD5, qop=auth) as the
 *     highest available auth tier.  Basic auth sends credentials in
 *     base64 on the wire — acceptable only over TLS (rtsps://).  When
 *     the transport is plain RTSP (rtsp://), Digest is the right choice.
 *     `tst_rtsp_client_builder_auth_digest_md5` models that usage.
 *
 *   Why TCP-interleaved matters:
 *     UDP works well on open networks.  In operational environments —
 *     military networks, cloud VPCs, or satellite uplinks — stateful
 *     firewalls frequently drop UDP coming back from a camera (the camera
 *     opens a new UDP socket for RTP; the firewall has no record of that
 *     ephemeral port).  TCP-interleaved rides the already-established RTSP
 *     control TCP connection, so nothing new passes the firewall.
 *
 *   The "auto" transport mode (transport pref = 0 = PreferUdp) tries UDP
 *   first; if the server responds 461 Unsupported Transport it retries with
 *   TCP-interleaved automatically.  This is the recommended default for
 *   programs that need to work in both network environments.
 *
 *   STANAG 4609 / MISB ST 1402 context:
 *     ISR video streams carry KLV metadata (MISB ST 0601 GPS, sensor
 *     gimbal angles, platform attitude …) multiplexed in the same MPEG-TS
 *     as the video elementary stream.  `tst_rtsp_session_into_demux_receiver`
 *     bridges the RTSP session transport into the same `DemuxReceiver`
 *     machinery used by the SRT receiver surface, so the same event-loop
 *     code that handles `recv_demux_to_console.c` works here unchanged.
 *
 * How to run:
 *   1. Default (demo/no camera — will fail gracefully at connect):
 *        ./recv_rtsp_camera
 *   2. With a real or simulated camera:
 *        ./recv_rtsp_camera \
 *          --url rtsp://camera.example.com:554/live/main \
 *          --user admin --pass secret --transport auto
 *   3. Force TCP-interleaved (useful behind firewalls):
 *        ./recv_rtsp_camera \
 *          --url rtsp://10.0.0.5:554/video \
 *          --user admin --pass s3cr3t --transport tcp
 *   4. Ctrl-C sends SIGINT → graceful TEARDOWN + cleanup.
 *
 * CLI args (all optional, parsed positionally):
 *   --url       <rtsp://...>    default: rtsp://127.0.0.1:554/live
 *   --user      <username>      if set, enables Digest MD5 auth
 *   --pass      <password>      required when --user is present
 *   --transport udp|tcp|auto    default: auto  (PreferUdp with 461 fallback)
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror \
 *      -lpthread \
 *      -o /tmp/recv_rtsp_camera \
 *      bindings/c/examples/receiving/recv_rtsp_camera.c -ltstrans
 *
 * Closest Rust analog: examples/receiving/rtsp_receive.rs (if it exists) or
 *   tst-rtp's integration tests under crates/tst-rtp/tests/.
 */

#include "tstrans.h"
#include <inttypes.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* -------------------------------------------------------------------------
 * Global cancel token — written by the SIGINT handler, read by the event
 * loop.  `volatile sig_atomic_t` is the only type POSIX guarantees is safe
 * to write from a signal handler without a mutex.
 * ---------------------------------------------------------------------- */
static volatile sig_atomic_t g_cancel_requested = 0;

/*
 * SIGINT / SIGTERM handler.  Sets the flag and returns immediately.
 * The event loop checks the flag before each blocking call and calls
 * `tst_rtp_demux_receiver_cancel` once to wake the blocking thread.
 * We do NOT call tst_* functions directly from the signal handler —
 * those functions are not async-signal-safe.
 */
static void handle_signal(int sig) {
    (void) sig;
    g_cancel_requested = 1;
}

/* -------------------------------------------------------------------------
 * Transport preference parsing.
 *
 * The C ABI accepts a uint32_t discriminator:
 *   0 = PreferUdp   — try UDP first, fall back to TCP-interleaved on 461
 *   1 = ForceUdp    — UDP only; surface error on 461
 *   2 = ForceTcp    — TCP-interleaved only; skip UDP attempt entirely
 *
 * "auto" is an alias for PreferUdp (0) because it transparently handles
 * both network topologies without user intervention.
 * ---------------------------------------------------------------------- */
static int parse_transport(const char *name, uint32_t *out) {
    if (strcmp(name, "auto") == 0 || strcmp(name, "prefer-udp") == 0) {
        *out = 0; /* PreferUdp */
        return 0;
    }
    if (strcmp(name, "udp") == 0 || strcmp(name, "force-udp") == 0) {
        *out = 1; /* ForceUdp */
        return 0;
    }
    if (strcmp(name, "tcp") == 0 || strcmp(name, "force-tcp") == 0) {
        *out = 2; /* ForceTcp / TCP-interleaved */
        return 0;
    }
    fprintf(stderr, "unknown transport '%s'; expected udp, tcp, or auto\n", name);
    return -1;
}

/* -------------------------------------------------------------------------
 * Print one line to stdout per demux event.  Same format as
 * recv_demux_to_console.c so the two examples can be diffed.
 * ---------------------------------------------------------------------- */
static void print_event(const tst_event_t *ev) {
    switch (ev->kind) {
        case TST_EVENT_KIND_PROGRAM_MAP:
            fprintf(stdout,
                    "[PMT ] program=%u  pcr_pid=0x%04x  streams=%zu  klv_links=%zu\n",
                    ev->u.program_map.program_number,
                    ev->u.program_map.pcr_pid,
                    ev->u.program_map.stream_count,
                    ev->u.program_map.klv_link_count);
            break;

        case TST_EVENT_KIND_SAMPLE:
            /*
             * For video samples, pts==INT64_MIN means "no PTS in the PES
             * header" (some cameras omit PTS on non-IDR slices).  The
             * codec field distinguishes H.264 / H.265 / H.266 / AV1.
             */
            fprintf(stdout,
                    "[SMPL] pid=0x%04x  pts=%" PRId64
                    "  kind=%d  codec=%d  payload=%zu B\n",
                    ev->u.sample.pid,
                    ev->u.sample.pts,
                    ev->u.sample.stream_kind,
                    ev->u.sample.codec,
                    ev->u.sample.payload_len);
            break;

        case TST_EVENT_KIND_METADATA:
            /*
             * KLV metadata record.  payload points into the receiver's
             * EventArena — copy before the next _next_event call if you
             * need the bytes to outlive this iteration.
             * `sequence_number` counts KLV records across this stream,
             * useful for monitoring producer-side drops.
             */
            fprintf(stdout,
                    "[META] pid=0x%04x  pts=%" PRId64
                    "  kind=%d  len=%zu  seq=%u\n",
                    ev->u.metadata.pid,
                    ev->u.metadata.pts,
                    ev->u.metadata.metadata_kind,
                    ev->u.metadata.payload_len,
                    ev->u.metadata.sequence_number);
            break;

        case TST_EVENT_KIND_DISCONTINUITY:
            /*
             * Continuity counter (CC) gap in the MPEG-TS stream.  Usually
             * indicates dropped UDP packets (if transport is UDP) or a camera
             * reboot / stream restart.  Expected cc vs observed cc helps
             * diagnose whether the loss is on the wire or at the camera.
             */
            fprintf(stdout,
                    "[DISC] pid=0x%04x  kind=%d  cc=%u→%u\n",
                    ev->u.discontinuity.pid,
                    ev->u.discontinuity.discontinuity_kind,
                    ev->u.discontinuity.cc_expected,
                    ev->u.discontinuity.cc_observed);
            break;

        case TST_EVENT_KIND_NON_CONFORMANT:
            /*
             * Non-fatal spec-compliance issue (CFI mismatch, AU cell header
             * anomaly, etc.).  In CFI-tolerance mode (the default) these
             * appear as diagnostics rather than hard errors.
             * `detail` is a static string from the library — no need to free.
             */
            fprintf(stdout,
                    "[NONC] pid=0x%04x  issue=%d  %s\n",
                    ev->u.nonconformant.pid,
                    ev->u.nonconformant.issue_code,
                    ev->u.nonconformant.detail ? ev->u.nonconformant.detail : "");
            break;

        default:
            fprintf(stderr, "unknown event kind: %d\n", ev->kind);
            break;
    }
}

/* -------------------------------------------------------------------------
 * main
 * ---------------------------------------------------------------------- */
int main(int argc, char **argv) {
    /* ------------------------------------------------------------------
     * Defaults — override via CLI flags.
     * ---------------------------------------------------------------- */
    const char *url       = "rtsp://127.0.0.1:554/live";
    const char *user      = NULL;
    const char *pass      = NULL;
    uint32_t    transport = 0;   /* 0 = PreferUdp (auto fallback to TCP) */

    /* ------------------------------------------------------------------
     * Minimal flag parser.  Order: --url, --user, --pass, --transport.
     * ---------------------------------------------------------------- */
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--url") == 0 && i + 1 < argc) {
            url = argv[++i];
        } else if (strcmp(argv[i], "--user") == 0 && i + 1 < argc) {
            user = argv[++i];
        } else if (strcmp(argv[i], "--pass") == 0 && i + 1 < argc) {
            pass = argv[++i];
        } else if (strcmp(argv[i], "--transport") == 0 && i + 1 < argc) {
            if (parse_transport(argv[++i], &transport) != 0) {
                return 1;
            }
        } else if (strcmp(argv[i], "--help") == 0 || strcmp(argv[i], "-h") == 0) {
            fprintf(stderr,
                    "usage: %s [--url <rtsp://...>] [--user U] [--pass P]"
                    " [--transport udp|tcp|auto]\n",
                    argv[0]);
            return 0;
        } else {
            fprintf(stderr, "unknown arg '%s'; pass --help for usage\n", argv[i]);
            return 1;
        }
    }

    /* Validate: if user is set, pass must also be set. */
    if (user != NULL && pass == NULL) {
        fprintf(stderr, "error: --pass is required when --user is given\n");
        return 1;
    }

    fprintf(stderr,
            "RTSP client camera example\n"
            "  url:       %s\n"
            "  user:      %s\n"
            "  transport: %s (pref=%u)\n",
            url,
            user ? user : "(none — no auth)",
            transport == 0 ? "auto (PreferUdp→TCP fallback)"
          : transport == 1 ? "force-udp"
          :                  "force-tcp-interleaved",
            transport);

    /* ------------------------------------------------------------------
     * Install SIGINT + SIGTERM handler.
     *
     * We use struct sigaction (POSIX) rather than signal() because
     * signal()'s behaviour on re-raise is implementation-defined on
     * Linux.  SA_RESTART restarts slow syscalls after the signal — we
     * don't want that here because we want the blocking _next_event
     * call to return promptly.
     * ---------------------------------------------------------------- */
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = handle_signal;
    /* Deliberately leave SA_RESTART unset — EINTR wakes the event loop
     * promptly so the flag check takes effect. */
    sigaction(SIGINT,  &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);

    /* ------------------------------------------------------------------
     * Step 1 — builder allocation.
     *
     * `tst_rtsp_client_builder_new` allocates and returns an opaque
     * configuration accumulator.  It does not open any socket yet —
     * the TCP connection to the camera happens inside `_connect`.
     *
     * The builder is consumed (freed internally) by `_connect`.
     * If you decide not to connect, call `tst_rtsp_client_builder_free`
     * to release it instead.
     * ---------------------------------------------------------------- */
    tst_rtsp_client_builder_t *builder = tst_rtsp_client_builder_new(url);
    if (!builder) {
        fprintf(stderr, "builder_new failed: %s\n", tst_get_last_error_str());
        return 1;
    }

    /* ------------------------------------------------------------------
     * Step 2 — optional authentication.
     *
     * `_auth_digest_md5` stores (user, pass) and tells the client to
     * respond to 401 WWW-Authenticate: Digest challenges using MD5 +
     * qop=auth (RFC 2617 / RFC 7616).  The client will retry the
     * initial request after receiving the challenge — this is the
     * standard one-round-trip Digest handshake.
     *
     * Use `_auth_digest_sha256` for cameras that advertise SHA-256
     * Digest (RFC 7616).  Use `_auth_basic` only over TLS (rtsps://).
     * ---------------------------------------------------------------- */
    if (user != NULL) {
        tst_rtsp_client_builder_auth_digest_md5(builder, user, pass);
        fprintf(stderr, "  auth:      Digest MD5 (user=%s)\n", user);
    }

    /* ------------------------------------------------------------------
     * Step 3 — transport preference.
     *
     * pref=0 (PreferUdp / "auto"):
     *   The client sends SETUP with Transport: RTP/AVP;unicast first.
     *   If the server replies 461 Unsupported Transport, it retries with
     *   Transport: RTP/AVP/TCP;interleaved automatically.
     *   This handles firewalled environments transparently.
     *
     * pref=1 (ForceUdp):
     *   UDP only.  If the server replies 461, the connect call returns
     *   TST_E_RTSP_UNSUPPORTED rather than falling back.
     *
     * pref=2 (ForceTcp):
     *   TCP-interleaved only.  Skips the UDP SETUP attempt.  Use when
     *   you know the network topology requires it and want to avoid
     *   the unnecessary round-trip.
     * ---------------------------------------------------------------- */
    tst_rtsp_client_builder_transport_pref(builder, transport);

    /* ------------------------------------------------------------------
     * Step 4 — keepalive thread.
     *
     * The keepalive thread sends periodic OPTIONS requests to keep the
     * server from timing out the session (RFC 7826 §18.43 TIMEOUT).
     * It is enabled by default.  Disable only for very short-lived
     * sessions (sub-second test probes) where the overhead matters.
     * ---------------------------------------------------------------- */
    tst_rtsp_client_builder_keepalive(builder, true);

    /* ------------------------------------------------------------------
     * Step 5 — connect (OPTIONS / DESCRIBE / SETUP / handshake).
     *
     * `_connect` consumes the builder — do not call any `_builder_*`
     * functions after this line.  The builder pointer becomes dangling
     * immediately; set it to NULL as a defensive habit.
     *
     * On success the function returns a `TstRtspSession*` that holds
     * the RTSP control channel but has not yet sent PLAY.
     * On failure it returns NULL; check `tst_get_last_error_str()`.
     * ---------------------------------------------------------------- */
    TstRtspSession *session = tst_rtsp_client_builder_connect(builder);
    builder = NULL; /* consumed — pointer is now dangling */

    if (!session) {
        fprintf(stderr,
                "connect failed: %s\n"
                "  (Is the camera reachable?  Is the URL correct?)\n",
                tst_get_last_error_str());
        return 1;
    }
    fprintf(stderr, "session established — sending PLAY\n");

    /* ------------------------------------------------------------------
     * Step 6 — PLAY.
     *
     * PLAY is NOT sent automatically by `_connect`.  This design lets
     * callers configure a pre-play pause window (e.g. to synchronise
     * multiple camera sessions before data starts flowing).
     *
     * Returns 0 on success; a negative TST_E_RTSP_* code on failure.
     * ---------------------------------------------------------------- */
    int rc = tst_rtsp_session_play(session);
    if (rc != 0) {
        fprintf(stderr, "PLAY failed (rc=%d): %s\n", rc, tst_get_last_error_str());
        tst_rtsp_session_teardown_and_free(session);
        return 1;
    }
    fprintf(stderr, "PLAY sent — RTP data flowing\n");

    /* ------------------------------------------------------------------
     * Step 7 — bridge session to a DemuxReceiver.
     *
     * `tst_rtsp_session_into_demux_receiver` consumes the session handle:
     *   1. Extracts the negotiated transport (bound UDP socket OR the
     *      mpsc channel fed by the TCP-interleaved pump thread).
     *   2. Wraps it in a `DemuxReceiver` using default demux config
     *      (NULL = CFI-tolerant mode, lenient PSI reassembly).
     *   3. Returns the same opaque `TstRtpDemuxReceiver*` type used by
     *      `tst_rtp_demux_receiver_open`, so the event loop below is
     *      identical regardless of whether the data came from a raw RTP
     *      URL or an RTSP session.
     *
     * IMPORTANT — session is consumed after this call.  The RTSP control
     * channel (RtspClient) is dropped along with the session object.  If
     * you need to send an explicit TEARDOWN before closing, call
     * `tst_rtsp_session_teardown_and_free(session)` INSTEAD of this
     * function.  Using both on the same session is undefined.
     * ---------------------------------------------------------------- */
    TstRtpDemuxReceiver *rx = tst_rtsp_session_into_demux_receiver(session, NULL);
    session = NULL; /* consumed — pointer is now dangling */

    if (!rx) {
        fprintf(stderr,
                "into_demux_receiver failed: %s\n",
                tst_get_last_error_str());
        return 1;
    }
    fprintf(stderr, "demux receiver ready — press Ctrl-C to stop\n");

    /* ------------------------------------------------------------------
     * Step 8 — event loop.
     *
     * `tst_rtp_demux_receiver_next_event` blocks until one typed demux
     * event is ready.  Pointer fields on `ev` borrow from the receiver's
     * internal EventArena and are valid only until the next `_next_event`
     * or `_close` call on the same handle.  memcpy payload bytes out
     * before advancing if you need them to outlive the iteration.
     *
     * Return codes:
     *   0                  — success; ev populated
     *   TST_E_END_OF_STREAM (-12) — graceful peer close / stream ended
     *   TST_E_CLOSED (-7)  — cancelled (SIGINT / _cancel called)
     *   TST_E_TRANSPORT (-8) — network I/O failure
     *   TST_E_INVALID_TS (-3) — irrecoverable demuxer error
     * ---------------------------------------------------------------- */
    tst_event_t ev   = {0};
    uint64_t n_events = 0;
    int exit_code     = 0;

    for (;;) {
        /* Check the SIGINT flag before blocking.  If cancel was requested
         * while we were in print_event() or elsewhere outside the blocking
         * call, this catches it without waiting for the next packet. */
        if (g_cancel_requested) {
            fprintf(stderr, "\ncaught signal — cancelling receiver\n");
            tst_rtp_demux_receiver_cancel(rx);
            /* Fall through; the next _next_event will return TST_E_CLOSED. */
        }

        rc = tst_rtp_demux_receiver_next_event(rx, &ev);

        if (rc == 0) {
            n_events++;
            print_event(&ev);
            continue;
        }

        if (rc == TST_E_END_OF_STREAM) {
            /*
             * Camera closed the stream gracefully (TEARDOWN from the server,
             * EOF on the TCP connection, or RTP BYE).  Normal exit.
             */
            fprintf(stderr,
                    "\nstream ended; %" PRIu64 " events received\n",
                    n_events);
            break;
        }

        if (rc == TST_E_CLOSED) {
            /* Cancelled via SIGINT or an explicit _cancel call. */
            fprintf(stderr,
                    "\nreceiver cancelled; %" PRIu64 " events received\n",
                    n_events);
            break;
        }

        /* Any other return code is an unexpected error. */
        fprintf(stderr,
                "\nnext_event error (rc=%d): %s\n",
                rc,
                tst_get_last_error_str());
        exit_code = 2;
        break;
    }

    /* ------------------------------------------------------------------
     * Step 9 — cleanup.
     *
     * `tst_rtp_demux_receiver_close` closes the underlying socket (or
     * drains the mpsc channel), joins the background threads, and frees
     * all memory associated with this handle.  Safe to call after cancel.
     * The handle must not be used after this point.
     *
     * Note: the RTSP control channel was already dropped when we called
     * `tst_rtsp_session_into_demux_receiver`.  There is no TEARDOWN sent
     * here — the server will timeout the session naturally after the TCP
     * connection closes.  If you need a clean TEARDOWN, use
     * `tst_rtsp_session_teardown_and_free` before the bridge step instead.
     * ---------------------------------------------------------------- */
    tst_rtp_demux_receiver_close(rx);
    fprintf(stderr, "done\n");
    return exit_code;
}
