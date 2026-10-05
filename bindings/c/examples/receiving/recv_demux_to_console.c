/*
 * recv_demux_to_console.c — bind a listener on srt://:7000, recv
 * typed demux events in a loop, print one line per event to stdout,
 * exit cleanly on peer disconnect.
 *
 * Why this example:
 *   This is the flagship receiver-side example. It exercises
 *   the full typed-event API surface: ProgramMap topology events,
 *   per-sample video/audio/subtitle frames, KLV metadata records,
 *   discontinuity and non-conformance diagnostics. One switch statement
 *   on ev.kind drives the printing — that's the shape any consumer
 *   integrating the demux receiver will use.
 *
 *   Use this shape for: building a probe that classifies a stream,
 *   a relay that re-encodes selected PIDs, an HLS/DASH adapter,
 *   a diagnostic tool that tails events from a live stream.
 *
 *   Don't use this shape for: pulling raw bytes off the wire (use
 *   recv_raw_to_file.c), or aligned 188-byte packets without typed
 *   decoding (use recv_ts_to_file.c).
 *
 * How to run:
 *   1. Receiver first (port is ready before peer connects):
 *        ./recv_demux_to_console
 *   2. Sender (any tst sender pushing TS bytes to srt://127.0.0.1:7000):
 *        cargo run -p tst-examples --example mux_dual_camera
 *      (or any mux_to_file output redirected over SRT — see the
 *       examples/sending/ catalogue).
 *   3. Receiver exits automatically on graceful sender close.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c
 *   cc -I bindings/c/include \
 *      -L target/debug \
 *      -Wl,-rpath,target/debug \
 *      -Wall -Wextra -Werror \
 *      -o /tmp/recv_demux_to_console \
 *      bindings/c/examples/receiving/recv_demux_to_console.c -ltstrans
 *
 * Closest Rust analog: examples/receiving/demux_to_events.rs (event-stream
 *   pretty-printer over a Demuxer; the C side here drives the equivalent
 *   walk through the C ABI).
 */

#include "tstrans.h"
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *kind_name(int kind) {
    /* Map TST_EVENT_KIND_* discriminator to a short tag string for the log. */
    switch (kind) {
        case TST_EVENT_KIND_PROGRAM_MAP:   return "PMT ";
        case TST_EVENT_KIND_SAMPLE:        return "SMPL";
        case TST_EVENT_KIND_METADATA:      return "META";
        case TST_EVENT_KIND_DISCONTINUITY: return "DISC";
        case TST_EVENT_KIND_NON_CONFORMANT: return "NONC";
        default:                           return "????";
    }
}

int main(int argc, char **argv) {
    (void) argc;
    (void) argv;

    tst_demux_receiver_t *rx = tst_demux_receiver_open_listener("srt://:7000");
    if (!rx) {
        fprintf(stderr, "open_listener failed: %s\n", tst_get_last_error_str());
        return 1;
    }
    fprintf(stderr, "listening on srt://:7000; waiting for peer...\n");

    /*
     * Stack-allocated tst_event_t. zero-initialize so any union
     * field the demuxer doesn't touch has predictable bytes.
     */
    tst_event_t ev = {0};
    uint64_t total_events = 0;

    for (;;) {
        int rc = tst_demux_receiver_recv_event(rx, &ev);

        if (rc == 0) {
            total_events += 1;
            /* Tag-then-switch — discriminator-first printing keeps
             * the per-kind detail uniform across event types. */
            switch (ev.kind) {
                case TST_EVENT_KIND_PROGRAM_MAP:
                    fprintf(stdout,
                            "[%s] program=%u pcr_pid=0x%04x streams=%zu klv_links=%zu\n",
                            kind_name(ev.kind),
                            ev.u.program_map.program_number,
                            ev.u.program_map.pcr_pid,
                            ev.u.program_map.stream_count,
                            ev.u.program_map.klv_link_count);
                    break;
                case TST_EVENT_KIND_SAMPLE:
                    fprintf(stdout,
                            "[%s] pid=0x%04x pts=%" PRId64
                            " kind=%d codec=%d payload_len=%zu nals=%zu obus=%zu\n",
                            kind_name(ev.kind),
                            ev.u.sample.pid,
                            ev.u.sample.pts,
                            ev.u.sample.stream_kind,
                            ev.u.sample.codec,
                            ev.u.sample.payload_len,
                            ev.u.sample.nal_count,
                            ev.u.sample.obu_count);
                    break;
                case TST_EVENT_KIND_METADATA:
                    fprintf(stdout,
                            "[%s] pid=0x%04x pts=%" PRId64
                            " kind=%d payload_len=%zu seq=%u\n",
                            kind_name(ev.kind),
                            ev.u.metadata.pid,
                            ev.u.metadata.pts,
                            ev.u.metadata.metadata_kind,
                            ev.u.metadata.payload_len,
                            ev.u.metadata.sequence_number);
                    break;
                case TST_EVENT_KIND_DISCONTINUITY:
                    fprintf(stdout,
                            "[%s] pid=0x%04x kind=%d cc=%u→%u\n",
                            kind_name(ev.kind),
                            ev.u.discontinuity.pid,
                            ev.u.discontinuity.discontinuity_kind,
                            ev.u.discontinuity.cc_expected,
                            ev.u.discontinuity.cc_observed);
                    break;
                case TST_EVENT_KIND_NON_CONFORMANT:
                    fprintf(stdout,
                            "[%s] pid=0x%04x issue=%d detail=%s\n",
                            kind_name(ev.kind),
                            ev.u.nonconformant.pid,
                            ev.u.nonconformant.issue_code,
                            ev.u.nonconformant.detail ? ev.u.nonconformant.detail : "");
                    break;
                default:
                    fprintf(stderr, "unknown event kind: %d\n", ev.kind);
                    break;
            }
            continue;
        }

        if (rc == TST_E_END_OF_STREAM) {
            fprintf(stderr,
                    "peer disconnected; %" PRIu64 " events received\n",
                    total_events);
            break;
        }
        if (rc == TST_E_CLOSED) {
            fprintf(stderr, "receiver was cancelled\n");
            break;
        }
        fprintf(stderr,
                "recv_event failed (rc=%d): %s\n",
                rc,
                tst_get_last_error_str());
        tst_demux_receiver_close(rx);
        return 2;
    }

    tst_demux_receiver_close(rx);
    return 0;
}
