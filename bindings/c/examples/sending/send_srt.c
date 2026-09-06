/*
 * send_srt.c — Minimal SRT TS-bytes sender opened from a URL, plus a walk
 * through the SRT URL error vocabulary.
 *
 * Two lessons in one file:
 *
 *   1. The `tst_sender_t` shape over SRT: pre-built 188-byte MPEG-TS
 *      packets in, SRT datagrams out. The C ABI's SRT analogue of
 *      `send_udp.c` / `send_tcp.c` / `send_rist.c` — same "raw TS bytes"
 *      contract, different transport. Use `tst_mux_sender_t` instead
 *      (see `../muxing/mux_synthetic_srt.c`) when you want the library to
 *      mux encoded video / KLV / audio into TS for you.
 *
 *   2. How the `srt://host:port?key=value` URL is validated, and how each
 *      malformed URL surfaces through the thread-local last-error. The
 *      error vocabulary is part of the API: an operator reading a log line
 *      needs to tell "the URL in the config file is wrong" apart from "the
 *      network is down", and the two look very different here.
 *
 * The C twin of the Rust `examples/sending/sender_from_url.rs`, with one
 * structural difference worth understanding before copying either:
 *
 *   Rust exposes `SrtUrl::parse` as a standalone step — you can parse,
 *   inspect the overlay, layer it over a `SocketBuilder`'s defaults, and
 *   only then connect. The C ABI has NO standalone parse call: `_open` IS
 *   parse + connect. The consequences are what this example demonstrates:
 *     - URL/config errors are detected BEFORE any socket is created and
 *       come back as TST_E_INVALID_CONFIG (-1). They are deterministic and
 *       network-independent — the same bad URL fails identically on every
 *       host.
 *     - Reachability errors (no listener, refused, timed out) come back
 *       from the same call as TST_E_TRANSPORT (-8).
 *     - The URL is the ONLY per-connection socket-option surface in C.
 *       `tst_sender_config_t` carries just the TS-framing knobs; latency,
 *       passphrase, streamid and friends live in the query string. So the
 *       Rust guide's "URL overrides builder defaults" rule has nothing to
 *       override here — the URL is the whole story.
 *
 * Caller-only: every `tst_*sender_open` in the C ABI dials OUT as an SRT
 * caller. There is no listener-mode sender (the receiver family has
 * `_open_listener`; senders do not) — the last table row below shows what
 * `?mode=listener` does when handed to a sender. See
 * `docs/project/deferred-features.md` ("SRT URL mode dispatch") and
 * `send_srt_ts_file.c` for the practical consequence.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c --features srt
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/send_srt bindings/c/examples/sending/send_srt.c -ltstrans
 *
 * Run (Terminal A — a listener to send into; verified 2026-09-06):
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/recv_ts_to_file bindings/c/examples/receiving/recv_ts_to_file.c -ltstrans
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_ts_to_file /tmp/out.ts     # binds srt://:7000
 * Run (Terminal B):
 *   LD_LIBRARY_PATH=target/debug /tmp/send_srt
 *   LD_LIBRARY_PATH=target/debug /tmp/send_srt 'srt://192.168.1.50:9000?latency=200&streamid=cam1'
 *
 * With no listener up, Step 2 reports TST_E_TRANSPORT and the example
 * carries on to the URL table — the table needs no peer at all.
 *
 * Requires: TST_HAS_SRT == 1 (set when the `srt` cargo feature is enabled).
 */

#include "tstrans.h"

#if !defined(TST_HAS_SRT) || TST_HAS_SRT == 0
#error "This example requires TST_HAS_SRT. Rebuild tst-c with the srt cargo feature enabled."
#endif

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * Default destination. `latency` is the SRT TSBPD budget in ms (both peers
 * negotiate the max of their two values; 120 ms is a sane LAN default).
 * `streamid` is the caller's SRTO_STREAMID — an opaque routing /
 * authorization key a multi-publisher listener reads after accept. Neither
 * is required; they are here so the URL shows the two knobs almost every
 * deployment sets.
 *
 * DEFAULT_LATENCY_MS is spliced into the URL AND sizes the pre-close drain
 * in Step 3, so the two cannot drift apart. If you pass your own URL with a
 * larger `?latency=`, raise the drain to match (`send_srt_ts_file.c` takes
 * `--latency-ms` and derives its drain from it for exactly this reason).
 */
#define DEFAULT_LATENCY_MS 120
#define STR_(x) #x
#define STR(x)  STR_(x)
#define DEFAULT_URL   "srt://127.0.0.1:7000?latency=" STR(DEFAULT_LATENCY_MS) "&streamid=send-srt-demo"

/* MPEG-TS packet size + sync byte + null PID — see send_udp.c for the
 * byte-level rationale; the synthetic payload is the same null packet. */
#define TS_PACKET_SIZE 188
#define TS_SYNC_BYTE   0x47
#define TS_NULL_PID_HI 0x1F
#define TS_NULL_PID_LO 0xFF

/*
 * 100 packets is deliberately NOT a multiple of 7: the sender bundles
 * seven 188-byte packets per 1316-byte SRT datagram, so 100 = 14 full
 * bundles + 2 packets left in the bundling buffer — Step 3 flushes those
 * 2 explicitly, and explains why you want to do that yourself.
 */
#define SEND_COUNT     100

/* ── Helpers ───────────────────────────────────────────────────────────── */

/* One structurally valid TS null packet (PID 0x1FFF, payload-only, 0xFF
 * stuffing). Every conformant receiver discards it silently. */
static void make_null_ts_packet(uint8_t *buf) {
    buf[0] = TS_SYNC_BYTE;
    buf[1] = TS_NULL_PID_HI;
    buf[2] = TS_NULL_PID_LO;
    buf[3] = 0x10;                            /* adaptation_field_control = 01 */
    memset(buf + 4, 0xFF, TS_PACKET_SIZE - 4);
}

/*
 * err_name — the handful of `tst_error_t` codes this example can meet, as
 * strings. The header's enum is the source of truth for the full set;
 * a real application would typically switch on the code, not print it.
 */
static const char *err_name(int code) {
    switch (code) {
    case TST_E_SUCCESS:        return "TST_E_SUCCESS";
    case TST_E_INVALID_CONFIG: return "TST_E_INVALID_CONFIG";
    case TST_E_TRANSPORT:      return "TST_E_TRANSPORT";
    case TST_E_CLOSED:         return "TST_E_CLOSED";
    default:                   return "(other)";
    }
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(int argc, char **argv) {
    const char *url = (argc > 1) ? argv[1] : DEFAULT_URL;
    int exit_code = 0;

    /*
     * ── Step 1: Sender config — the TS-framing knobs ─────────────────────
     *
     * `tst_sender_config_t` is optional (pass NULL for defaults); it is
     * built here to show the one knob most relays care about.
     *
     * WHY framing mode?
     *   `tst_sender_send_ts` verifies the 0x47 sync byte at every 188-byte
     *   boundary before bundling. RECOVER (the default) skips a misaligned
     *   prefix until it finds sync and re-syncs after loss — right for a
     *   relay fed by a file or a socket where a torn read is a fact of
     *   life. STRICT fails the send the moment a boundary byte isn't 0x47 —
     *   right for a pipeline whose upstream is a muxer you control, where
     *   misalignment means a bug you want to hear about immediately.
     *   `tst_sender_config_set_max_unsynced_bytes` bounds how far RECOVER
     *   will scan for sync before failing the send (default 18,800 bytes
     *   = 100 packets' worth of garbage).
     */
    tst_sender_config_t *scfg = tst_sender_config_new();
    if (!scfg) {
        fprintf(stderr, "[send_srt] tst_sender_config_new: out of memory\n");
        return 1;
    }
    (void) tst_sender_config_set_framing_mode(scfg, TST_TS_FRAMING_MODE_RECOVER);

    /*
     * ── Step 2: Open = parse the URL + connect ───────────────────────────
     *
     * On failure the return is NULL and the thread-local last-error holds
     * the code + a detail string. Read it IMMEDIATELY — any other tst_*
     * call on this thread may overwrite it. The config is copied by open,
     * so it is freed right after regardless of outcome.
     *
     * Classifying the failure is the operational lesson:
     *   TST_E_INVALID_CONFIG → fix the URL (or the config); retrying is
     *                          pointless, nothing on the network was touched.
     *   TST_E_TRANSPORT      → the URL is fine; the peer isn't there (yet).
     *                          This is what a reconnect loop retries on —
     *                          or what `tst_managed_sender_t` retries for
     *                          you (see ../operations/managed_reconnect.c).
     */
    fprintf(stderr, "[send_srt] opening %s\n", url);
    tst_sender_t *sender = tst_sender_open(url, scfg);
    tst_sender_config_free(scfg);
    scfg = NULL;

    if (!sender) {
        int code = tst_get_last_error();
        fprintf(stderr, "[send_srt] open failed: %s (%d): %s\n",
                err_name(code), code, tst_get_last_error_str());
        if (code == TST_E_INVALID_CONFIG) {
            fprintf(stderr, "[send_srt]   -> the URL/config is wrong; nothing was sent to the network\n");
            exit_code = 2;
        } else {
            fprintf(stderr, "[send_srt]   -> URL accepted; the peer is not reachable "
                            "(start a listener, see the header)\n");
            /* Not a failure of THIS example — the URL table below still runs. */
        }
    } else {
        /*
         * ── Step 3: Send, flush, close ───────────────────────────────────
         *
         * Passing one 188-byte packet per call is the simplest contract;
         * the sender accumulates seven into a 1316-byte datagram before it
         * hits the wire. Throughput-oriented code hands over larger
         * 188-multiples per call. Returns 0 or a negative TST_E_* code.
         */
        uint8_t pkt[TS_PACKET_SIZE];
        make_null_ts_packet(pkt);
        int sent = 0;
        for (int i = 0; i < SEND_COUNT; i++) {
            int rc = tst_sender_send_ts(sender, pkt, sizeof(pkt));
            if (rc != 0) {
                fprintf(stderr, "[send_srt] send_ts[%d] failed: %s (%d): %s\n",
                        i, err_name(rc), rc, tst_get_last_error_str());
                exit_code = 3;
                break;
            }
            sent++;
        }
        /*
         * WHY flush explicitly?
         *   100 packets = 14 full bundles + 2 packets still sitting in the
         *   bundling buffer. A short (2-packet) datagram only leaves when
         *   something forces it out: a later send completing the bundle,
         *   this call, or close. `tst_sender_close` DOES flush the partial
         *   bundle for you (close == drop by contract) — but it swallows
         *   the flush's result. Calling flush yourself is how you get to
         *   SEE a failure on the last bytes, and in a long-running relay it
         *   is how you push a logical boundary (end of file, end of GOP)
         *   onto the wire now rather than whenever the next bundle fills.
         */
        int rc = tst_sender_flush(sender);
        if (rc != 0) {
            fprintf(stderr, "[send_srt] flush failed: %s (%d): %s\n",
                    err_name(rc), rc, tst_get_last_error_str());
            exit_code = 3;
        }
        /*
         * Give the tail of the stream one latency window (+ margin) to be
         * ACKed before close tears the link down; closing instantly races
         * the last datagrams and the listener may see a truncated tail.
         * Sized from DEFAULT_LATENCY_MS — see the constant's comment if you
         * override the URL with a larger `?latency=`.
         */
        usleep((DEFAULT_LATENCY_MS + 200) * 1000);
        tst_sender_close(sender);
        fprintf(stderr, "[send_srt] sent %d TS packets (%d bytes), flushed, closed.\n",
                sent, sent * TS_PACKET_SIZE);
    }

    /*
     * ── Step 4: The URL error vocabulary ─────────────────────────────────
     *
     * Each row is a URL that is wrong in one specific way. Every one MUST
     * fail with TST_E_INVALID_CONFIG before touching the network — that is
     * the property being demonstrated — and the detail string names the
     * problem the way an operator would want to read it in a log. The
     * rows mirror the Rust twin's table; the last row is C-only.
     *
     *   syntax             no "://" at all
     *   wrong scheme       a valid URL for some other protocol
     *   missing port       SRT has no default port; it is always required
     *   userinfo           user:pass@ is rejected — encryption is ?passphrase=,
     *                      never URL userinfo (which lands in logs and shell history)
     *   unsupported mode   rendezvous is real SRT but not implemented here
     *   unsupported key    a real libsrt option this crate deliberately does
     *                      not expose yet (see docs/guides/srt.md#url-parsing)
     *   unknown key        a typo — not a libsrt option at all
     *   invalid value      right key, wrong type (latency is an integer of ms)
     *   option validation  right key, right type, value outside the option's
     *                      legal range (pbkeylen is 16 / 24 / 32)
     *   listener on sender parses fine (the receiver family honours it) but a
     *                      sender has no listener path — it tries to DIAL the
     *                      URL's empty host, which fails address lookup, i.e.
     *                      TST_E_TRANSPORT, NOT TST_E_INVALID_CONFIG. A
     *                      documented sharp edge, not a validation error.
     */
    static const struct { const char *label; const char *url; } cases[] = {
        { "syntax",             "not-a-url" },
        { "wrong scheme",       "https://1.2.3.4:9000" },
        { "missing port",       "srt://1.2.3.4" },
        { "userinfo",           "srt://op:hunter2@1.2.3.4:9000" },
        { "unsupported mode",   "srt://1.2.3.4:9000?mode=rendezvous" },
        { "unsupported key",    "srt://1.2.3.4:9000?transtype=file" },
        { "unknown key",        "srt://1.2.3.4:9000?lattency=100" },
        { "invalid value",      "srt://1.2.3.4:9000?latency=200ms" },
        { "option validation",  "srt://1.2.3.4:9000?pbkeylen=15" },
        { "listener on sender", "srt://:9000?mode=listener" },
    };
    const size_t n_cases = sizeof(cases) / sizeof(cases[0]);

    fprintf(stderr, "[send_srt] URL error vocabulary (%zu cases):\n", n_cases);
    for (size_t i = 0; i < n_cases; i++) {
        /* NULL config = defaults; we only care about the open's verdict. */
        tst_sender_t *s = tst_sender_open(cases[i].url, NULL);
        if (s) {
            /* Would only happen if the parser started accepting the row —
             * a signal to update this table, not a runtime error. */
            printf("[%s] unexpectedly opened OK: %s\n", cases[i].label, cases[i].url);
            tst_sender_close(s);
            continue;
        }
        int code = tst_get_last_error();
        printf("[%-18s] %s (%d): %s\n",
               cases[i].label, err_name(code), code, tst_get_last_error_str());
    }

    return exit_code;
}
