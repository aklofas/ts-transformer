/*
 * recv_rtp.c — join a multicast RTP group, receive a typed demux
 * event stream from an MPEG-TS-over-RTP source, and shut down gracefully
 * on Ctrl-C.
 *
 * Why RTP instead of SRT for gimbaled-platform video?
 *   SRT is the preferred transport for unicast links from a single sensor
 *   to a single ground station: it adds retransmission, encryption, and
 *   congestion control.  RTP shines when the same stream must reach several
 *   consumers simultaneously — e.g., a UAS downlink distributed to a
 *   mission commander console, a recording server, and a map overlay
 *   service.  IP multicast delivers one UDP stream to all joiners with no
 *   sender-side fan-out, which is why STANAG 4609 / MISB ST 1402 pipelines
 *   routinely use RTP+MPEG-TS over multicast inside the ground segment.
 *
 * What this example shows:
 *   - Parsing a CLI URL argument (rtp://239.1.2.3:5000?iface=eth0)
 *   - Opening a `TstRtpDemuxReceiver` — the RTP twin of
 *     `tst_demux_receiver_t`, sharing the same `tst_event_t` event model
 *   - A blocking event loop driven by `tst_rtp_demux_receiver_next_event`
 *   - Full event-kind dispatch: ProgramMap / Sample / Metadata /
 *     Discontinuity / NonConformant / ReconnectDiscontinuity
 *   - SIGINT graceful shutdown via `tst_rtp_demux_receiver_cancel`:
 *     the signal handler flags the receiver and _next_event returns
 *     TST_E_CLOSED at its next poll tick, with no busy-spin
 *
 * What it doesn't show (see the listed examples for those):
 *   - KLV hex-dump or typed ST 0601 decode → recv_klv_to_stdout.c
 *   - Stats polling                         → operations/poll_socket_stats.c
 *
 * How to run:
 *   1. Sender side (push multicast RTP to group 239.1.2.3:5000):
 *        cargo run -p tst-examples --example send_rtp -- \
 *          rtp://239.1.2.3:5000?iface=eth0
 *      (or any MPEG-TS-over-RTP multicast source in the same LAN segment)
 *
 *   2. Receiver (this example) — run AFTER configuring your NIC to join:
 *        ./recv_rtp rtp://239.1.2.3:5000?iface=eth0
 *
 *   3. Press Ctrl-C to stop cleanly.  Without a sender, the receiver
 *      blocks in _next_event waiting for packets; Ctrl-C still exits
 *      gracefully via the cancel path.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror \
 *      -o /tmp/recv_rtp \
 *      bindings/c/examples/receiving/recv_rtp.c \
 *      -ltstrans -lpthread -ldl
 *
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_rtp \
 *       rtp://239.1.2.3:5000?iface=eth0
 *
 * Requires: TST_HAS_RTP == 1 (default when the `rtp` cargo feature is
 * enabled).  The guard below produces a clear build-time error if the
 * library was compiled without RTP support.
 *
 * Closest Rust analog:
 *   examples/receiving/rtp_demux_to_events.rs — same event walk, Rust API.
 */

#include "tstrans.h"

#if !defined(TST_HAS_RTP) || TST_HAS_RTP == 0
#error "This example requires TST_HAS_RTP. Rebuild tst-c with the rtp cargo feature enabled."
#endif

#include <inttypes.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ── SIGINT state ─────────────────────────────────────────────────────────── */

/*
 * Global receiver pointer used by the signal handler.  Declared volatile so
 * the compiler cannot cache a NULL in a register after the assignment
 * in main() — the signal arrives on the same thread in this single-threaded
 * example, but the volatile also ensures correctness in multi-threaded
 * consumers that copy this pattern.
 *
 * sig_atomic_t flag: set to 1 by the handler; checked in the loop error
 * path to distinguish cancel-by-signal from other TST_E_CLOSED sources.
 */
static volatile TstRtpDemuxReceiver *g_receiver = NULL;
static volatile sig_atomic_t g_shutdown_requested = 0;

static void on_sigint(int sig) {
    (void)sig;
    g_shutdown_requested = 1;

    /*
     * Why call cancel from the signal handler?
     *   tst_rtp_demux_receiver_next_event polls the UDP socket in short
     *   ticks. tst_rtp_demux_receiver_cancel only flags the transport (an
     *   atomic store, so it is signal-safe); there is no wakeup pipe. The
     *   parked _next_event notices the flag at its next poll tick and
     *   returns TST_E_CLOSED. The handle still needs _close after the loop.
     *
     *   The cast drops volatile — cancel takes a plain pointer. main()
     *   restores the default disposition and clears g_receiver before the
     *   close, so the handler never sees a freed handle.
     */
    TstRtpDemuxReceiver *rx = (TstRtpDemuxReceiver *)g_receiver;
    if (rx) {
        tst_rtp_demux_receiver_cancel(rx);
    }
}

/* ── helpers ──────────────────────────────────────────────────────────────── */

static const char *event_tag(tst_event_kind kind) {
    /* Four-character tag for uniform log line prefix. */
    switch (kind) {
        case TST_EVENT_KIND_PROGRAM_MAP:            return "PMT ";
        case TST_EVENT_KIND_SAMPLE:                 return "SMPL";
        case TST_EVENT_KIND_METADATA:               return "META";
        case TST_EVENT_KIND_DISCONTINUITY:          return "DISC";
        case TST_EVENT_KIND_NON_CONFORMANT:         return "NONC";
        case TST_EVENT_KIND_RECONNECT_DISCONTINUITY: return "RCON";
        default:                                    return "????";
    }
}

static void print_usage(const char *prog) {
    fprintf(stderr,
            "Usage: %s <url>\n"
            "\n"
            "  url  RTP source URL, e.g.:\n"
            "         rtp://239.1.2.3:5000?iface=eth0   (multicast)\n"
            "         rtp://0.0.0.0:5000                (unicast, any NIC)\n"
            "\n"
            "Press Ctrl-C to stop cleanly.\n",
            prog);
}

/* ── event dispatch ───────────────────────────────────────────────────────── */

static void print_program_map(const tst_event_t *ev) {
    /*
     * ProgramMap fires once per PMT update.  A well-behaved sender emits
     * the PMT at stream start and again when the program description
     * changes (new PID, codec change, KLV link added/removed).
     * In stable streams you'll see exactly one PMT event.
     */
    fprintf(stdout,
            "[%s] program=%u pcr_pid=0x%04x streams=%zu klv_links=%zu\n",
            event_tag(ev->kind),
            ev->u.program_map.program_number,
            ev->u.program_map.pcr_pid,
            ev->u.program_map.stream_count,
            ev->u.program_map.klv_link_count);
}

static void print_sample(const tst_event_t *ev) {
    /*
     * Sample is the dominant event kind in a video stream: one event per
     * access unit (AU) — a frame for H.264/H.265, one or more OBUs for
     * AV1, one or more ADTS frames for AAC.
     *
     * stream_kind discriminates video / audio / subtitle; codec identifies
     * the specific codec within that kind.  For video,
     * random_access_indicator=1 means the TS adaptation field carried the
     * random_access_indicator bit (ISO/IEC 13818-1 §2.4.3.4 bit 0x40) on
     * the PES_start packet — equivalent to an IDR / key frame, useful for
     * splicing and random access.
     *
     * payload_len is the raw byte count; nal_count / obu_count are
     * non-zero when the demuxer parsed the payload into typed units.
     */
    fprintf(stdout,
            "[%s] pid=0x%04x pts=%" PRId64
            " kind=%d codec=%d rai=%u payload=%zu nals=%zu obus=%zu\n",
            event_tag(ev->kind),
            ev->u.sample.pid,
            ev->u.sample.pts,
            ev->u.sample.stream_kind,
            ev->u.sample.codec,
            ev->u.sample.random_access_indicator,
            ev->u.sample.payload_len,
            ev->u.sample.nal_count,
            ev->u.sample.obu_count);
}

static void print_metadata(const tst_event_t *ev) {
    /*
     * Metadata events carry KLV records.  For KlvSynchronousMetadata
     * (the STANAG 4609 / MISB ST 1402 shape), the demuxer has already
     * stripped the 5-byte Metadata_AU_cell wrapper — payload is the
     * inner KLV LS bytes.  sequence_number tracks record order per PID.
     *
     * The payload pointer borrows from the receiver's internal arena;
     * copy it if you need it beyond the next _next_event call.
     */
    fprintf(stdout,
            "[%s] pid=0x%04x pts=%" PRId64
            " kind=%d seq=%u len=%zu\n",
            event_tag(ev->kind),
            ev->u.metadata.pid,
            ev->u.metadata.pts,
            ev->u.metadata.metadata_kind,
            ev->u.metadata.sequence_number,
            ev->u.metadata.payload_len);
}

static void print_discontinuity(const tst_event_t *ev) {
    /*
     * Discontinuity events indicate a gap in the continuity counter (CC)
     * sequence for a PID — a reliable indicator of dropped packets.
     * In multicast over a lossy LAN, these are common.  Logging them
     * is the first diagnostic step when video glitches appear.
     *
     * cc_expected / cc_observed are the predicted and actual CC values
     * (0-15 modular); the gap tells you how many 188-byte packets were
     * lost.  Multiply by 7 for approximate milliseconds at 25 Mbps.
     */
    fprintf(stdout,
            "[%s] pid=0x%04x kind=%d cc_expected=%u cc_observed=%u\n",
            event_tag(ev->kind),
            ev->u.discontinuity.pid,
            ev->u.discontinuity.discontinuity_kind,
            ev->u.discontinuity.cc_expected,
            ev->u.discontinuity.cc_observed);
}

static void print_nonconformant(const tst_event_t *ev) {
    /*
     * NonConformant events surface spec-deviation diagnostics: malformed
     * AU cell CFI bytes, PES header anomalies, etc.  They are advisory —
     * the demuxer continues past them.  The detail string is null-safe
     * (may be NULL for issue codes with no per-instance context).
     *
     * By default, DemuxerConfig runs in CFI-tolerant mode (most production
     * encoders do not set the CFI field correctly — see cfi_tolerance
     * in the config API).  You'll still see these events for other
     * non-conformances; they don't abort the stream.
     */
    fprintf(stdout,
            "[%s] pid=0x%04x issue=%d detail=%s\n",
            event_tag(ev->kind),
            ev->u.nonconformant.pid,
            ev->u.nonconformant.issue_code,
            ev->u.nonconformant.detail ? ev->u.nonconformant.detail : "(none)");
}

static void print_reconnect_discontinuity(const tst_event_t *ev) {
    /*
     * ReconnectDiscontinuity is emitted by a ManagedDemuxReceiver when it
     * reconnects after a transport error.  For a plain RTP demux receiver
     * (no managed reconnect), you may still see this if the upstream
     * joined a stream mid-flight.
     *
     * For most applications this is an informational event; a recorder
     * might use it to mark a chapter boundary in the output file.
     *
     * Note: TST_EVENT_KIND_RECONNECT_DISCONTINUITY has no per-PID body in
     * the TstEventBody union — the union carries only ProgramMap / Sample /
     * Metadata / Discontinuity / NonConformant fields.  The kind value
     * alone is sufficient to act on this event.
     */
    fprintf(stdout, "[%s] stream reconnect discontinuity\n",
            event_tag(ev->kind));
}

/* ── main ─────────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    if (argc != 2) {
        print_usage(argv[0]);
        return 1;
    }
    const char *url = argv[1];

    /*
     * Install SIGINT handler before opening the receiver so there is no
     * window where Ctrl-C cannot cancel it.  We use signal() rather than
     * sigaction() to stay C99-portable; the one-shot semantics (signal
     * resets to SIG_DFL after delivery on some platforms) are fine here
     * because we check g_shutdown_requested in the loop and exit promptly.
     */
    signal(SIGINT, on_sigint);

    /*
     * Open the RTP-backed demux receiver.
     *
     * The second argument is a tst_demux_config_t pointer.  Passing NULL
     * selects all defaults: CFI-tolerant mode, 1 MiB AU cell reassembly
     * cap per PID, standard PSI reassembly.  That is appropriate for
     * most real-world multicast MPEG-TS sources whose encoders do not
     * set the AU cell CFI field correctly.
     *
     * For multicast URLs the library joins the group on the specified
     * interface at open time.  For unicast (rtp://0.0.0.0:port) it
     * simply binds the socket.
     */
    TstRtpDemuxReceiver *rx = tst_rtp_demux_receiver_open(url, NULL);
    if (!rx) {
        fprintf(stderr,
                "tst_rtp_demux_receiver_open(\"%s\") failed: %s\n",
                url,
                tst_get_last_error_str());
        return 1;
    }

    /*
     * Publish the receiver handle for the SIGINT handler.  The assignment
     * is not guarded by a mutex because:
     *   (a) SIGINT is not delivered until the process receives the signal,
     *       which happens after this assignment in practice.
     *   (b) The volatile qualifier ensures the compiler does not hoist
     *       the read past the store.
     * Multi-threaded consumers that register the handler before opening
     * should use an atomic or a mutex instead.
     */
    g_receiver = rx;

    fprintf(stderr, "Opened: %s\n", url);
    fprintf(stderr, "Waiting for MPEG-TS events. Press Ctrl-C to stop.\n");

    /*
     * Stack-allocate the event struct.  Zero-initialize so any union
     * field the demuxer did not write has predictable bytes — avoids
     * reading garbage if a future event kind extends the union.
     */
    tst_event_t ev = {0};
    uint64_t total_events = 0;

    for (;;) {
        /*
         * _next_event blocks until one event is ready, the socket is
         * cancelled, or a transport error occurs.  It is NOT a spin loop —
         * it parks on the UDP receive path and returns only when there is
         * something to deliver.  CPU usage while idle is effectively zero.
         */
        int rc = tst_rtp_demux_receiver_next_event(rx, &ev);

        if (rc == 0) {
            total_events += 1;

            switch (ev.kind) {
                case TST_EVENT_KIND_PROGRAM_MAP:
                    print_program_map(&ev);
                    break;
                case TST_EVENT_KIND_SAMPLE:
                    print_sample(&ev);
                    break;
                case TST_EVENT_KIND_METADATA:
                    print_metadata(&ev);
                    break;
                case TST_EVENT_KIND_DISCONTINUITY:
                    print_discontinuity(&ev);
                    break;
                case TST_EVENT_KIND_NON_CONFORMANT:
                    print_nonconformant(&ev);
                    break;
                case TST_EVENT_KIND_RECONNECT_DISCONTINUITY:
                    print_reconnect_discontinuity(&ev);
                    break;
                default:
                    /*
                     * Future event kinds: log and continue rather than
                     * aborting.  Callers should be resilient to new kinds
                     * added in minor ABI bumps.
                     */
                    fprintf(stderr,
                            "[????] unknown event kind %d — ignoring\n",
                            ev.kind);
                    break;
            }

            /* fflush keeps stdout current when piped to a file or grep. */
            fflush(stdout);
            continue;
        }

        /*
         * Error paths — evaluate in priority order.
         */

        if (rc == TST_E_CLOSED) {
            /*
             * TST_E_CLOSED means our side requested shutdown, either via
             * the SIGINT handler above or an explicit cancel from another
             * thread.  This is the clean exit path for interactive use.
             */
            if (g_shutdown_requested) {
                fprintf(stderr,
                        "\nCaught SIGINT — shutting down. "
                        "%" PRIu64 " events received.\n",
                        total_events);
            } else {
                fprintf(stderr,
                        "Receiver was cancelled externally. "
                        "%" PRIu64 " events received.\n",
                        total_events);
            }
            break;
        }

        if (rc == TST_E_END_OF_STREAM) {
            /*
             * TST_E_END_OF_STREAM: the sender closed the stream or the
             * multicast group was left without any active senders.  Unlike
             * TST_E_CLOSED, this is a remote-side event.  A production
             * application might log a "stream ended" alert and reconnect.
             */
            fprintf(stderr,
                    "Stream ended (no more packets). "
                    "%" PRIu64 " events received.\n",
                    total_events);
            break;
        }

        /*
         * All other negative rc values indicate an unrecoverable transport
         * or demux error.  Log the numeric code and the human-readable
         * thread-local error string, then exit non-zero.
         *
         * Common codes:
         *   TST_E_RTP_TRANSPORT (-15) — UDP I/O error (socket closed, etc.)
         *   TST_E_INVALID_TS    (-3)  — catastrophic demux failure
         *   TST_E_INVALID_CONFIG (-1) — NULL pointer passed (programmer error)
         */
        fprintf(stderr,
                "tst_rtp_demux_receiver_next_event failed (rc=%d): %s\n",
                rc,
                tst_get_last_error_str());
        /* Why first? A Ctrl-C during or after the close must not run the
         * handler against a freed receiver: restore the default
         * disposition and clear the global before the handle is released. */
        signal(SIGINT, SIG_DFL);
        g_receiver = NULL;
        tst_rtp_demux_receiver_close(rx);
        return 2;
    }

    /*
     * _close frees all internal state including the joined multicast
     * group membership and the event arena.  Safe to call with NULL
     * (no-op), but we know rx is non-NULL here.
     *
     * Do NOT access any ev.u.* pointer-fields after this call — they
     * borrow from the arena that _close just freed.
     *
     * Restore the default SIGINT disposition and clear g_receiver first, so
     * the handler cannot reach the receiver while or after it is freed.
     */
    signal(SIGINT, SIG_DFL);
    g_receiver = NULL;
    tst_rtp_demux_receiver_close(rx);
    return 0;
}
