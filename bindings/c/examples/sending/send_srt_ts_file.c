/*
 * send_srt_ts_file.c — relay an existing `.ts` file over SRT at real-time
 * pace, driven by the file's own PCRs.
 *
 * Reads a recorded MPEG-TS file 188 bytes at a time and pushes it through
 * a `tst_sender_t` (the TS-bytes shape: no muxer, the file is ALREADY a
 * mux). By default each packet leaves at the wall-clock time its Program
 * Clock Reference says it should — real-time playback, the same thing
 * `ffmpeg -re` and `srt-live-transmit` do — so a receiver's jitter buffer
 * is never flooded.
 *
 * The C counterpart of the Rust `examples/sending/srt_serve_ts_file.rs`,
 * with ONE deliberate difference that changes the deployment shape:
 *
 *   The Rust example is an SRT LISTENER ("VLC dials in"). The C ABI's
 *   sender family is CALLER-ONLY — `tst_sender_open` always dials out, and
 *   there is no `_open_listener` on the sender side (only receivers have
 *   one; see `docs/project/deferred-features.md`, "SRT URL mode
 *   dispatch"). So this example dials a listening receiver instead:
 *   `recv_ts_to_file.c`, `recv_demux_to_console.c`, `srt-live-transmit`,
 *   or a VLC / ffplay configured in listener mode
 *   (`srt://:9000?mode=listener`). The pacing logic is identical.
 *
 * What this example shows:
 *   1. `tst_sender_t` + `tst_sender_config_t`: the framing-mode knob and
 *      why RECOVER is the right default for a file relay.
 *   2. Parsing the PCR out of a TS packet's adaptation field (27 MHz) and
 *      pacing on it: anchor the first PCR to wall-clock, then before
 *      emitting each later PCR-bearing packet sleep until
 *      `now - t0_wall ≈ pcr - t0_pcr`. Packets between PCRs ride out at
 *      line rate inside their (typically 30-100 ms) window, so wire pacing
 *      tracks the encoded bitrate within a frame or two.
 *   3. Re-anchoring on a PCR jump backwards (33-bit rollover every ~26.5 h,
 *      or `--loop` rewinding the file) so playback never pauses for hours.
 *   4. End-of-stream discipline: flush the bundling buffer, wait out one
 *      latency window so the tail is ACKed, then close.
 *   5. `--loop` with a SIGINT flag polled from the pump loop — the only
 *      safe thing to do from a signal handler.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c --features srt
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/send_srt_ts_file bindings/c/examples/sending/send_srt_ts_file.c -ltstrans
 *
 * Run (Terminal A — a listener; `recv_ts_to_file` binds srt://:7000):
 *   LD_LIBRARY_PATH=target/debug /tmp/recv_ts_to_file /tmp/copy.ts
 * Run (Terminal B):
 *   LD_LIBRARY_PATH=target/debug /tmp/mux_to_file /tmp/in.ts 5      # any .ts will do
 *   LD_LIBRARY_PATH=target/debug /tmp/send_srt_ts_file /tmp/in.ts 127.0.0.1:7000
 *   cmp /tmp/in.ts /tmp/copy.ts && echo identical
 *   # takes ~5 s for a 5 s file: that IS the pacing. Add --no-pace to blast.
 *
 * Usage:
 *   send_srt_ts_file <file.ts> <host:port> [--loop] [--no-pace] [--latency-ms <ms>]
 *
 * Requires: TST_HAS_SRT == 1 (set when the `srt` cargo feature is enabled).
 */

#include "tstrans.h"

#if !defined(TST_HAS_SRT) || TST_HAS_SRT == 0
#error "This example requires TST_HAS_SRT. Rebuild tst-c with the srt cargo feature enabled."
#endif

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

#define TS_PACKET_SIZE   188

/*
 * Read granularity. A multiple of 188 so the sender is always handed
 * whole packets and the PCR walker never straddles a packet across two
 * reads. 350 packets ≈ 65 KB — a few hundred ms of typical ISR video.
 */
#define READ_PACKETS     350
#define READ_CHUNK       (TS_PACKET_SIZE * READ_PACKETS)

/* PCR ticks at 27 MHz (ISO/IEC 13818-1 §2.4.3.5: base×300 + ext). */
#define PCR_HZ           27000000ULL

/* A backwards PCR step larger than this is treated as a rollover / rewind
 * and re-anchors the wall clock, rather than as jitter. 1 s. */
#define PCR_REANCHOR_GAP PCR_HZ

/* Default TSBPD latency; 200 ms is a LAN default VLC accepts cleanly. */
#define DEFAULT_LATENCY_MS 200

/* ── SIGINT → flag (async-signal-safe; the pump loop polls it) ─────────── */

static volatile sig_atomic_t g_stop = 0;
static void on_sigint(int sig) { (void)sig; g_stop = 1; }

/* ── PCR parsing ───────────────────────────────────────────────────────── */

/*
 * parse_pcr — return 1 and write the 27 MHz PCR if `pkt` carries one,
 * else 0. The vast majority of packets return 0.
 *
 * Layout per ISO/IEC 13818-1 §2.4.3:
 *   byte 0      : 0x47 sync
 *   byte 3      : bits 5..4 = adaptation_field_control (AFC); 0x2 bit set
 *                 means an adaptation field is present
 *   byte 4      : adaptation_field_length
 *   byte 5      : AF flags — 0x10 = PCR_flag
 *   bytes 6..11 : program_clock_reference_base (33 bits), 6 reserved bits,
 *                 program_clock_reference_extension (9 bits)
 * PCR = base × 300 + extension, in 27 MHz units.
 */
static int parse_pcr(const uint8_t *pkt, uint64_t *pcr_out) {
    if (pkt[0] != 0x47) {
        return 0;
    }
    const unsigned afc = (pkt[3] >> 4) & 0x3;
    if ((afc & 0x2) == 0) {
        return 0;                         /* no adaptation field */
    }
    const unsigned af_len = pkt[4];
    if (af_len < 7) {
        return 0;                         /* too short for flags + 6-byte PCR */
    }
    if ((pkt[5] & 0x10) == 0) {
        return 0;                         /* PCR_flag clear */
    }
    const uint8_t *b = pkt + 6;
    const uint64_t base = ((uint64_t)b[0] << 25) | ((uint64_t)b[1] << 17) |
                          ((uint64_t)b[2] << 9)  | ((uint64_t)b[3] << 1)  |
                          ((uint64_t)b[4] >> 7);
    const uint64_t ext  = (((uint64_t)b[4] & 0x01) << 8) | (uint64_t)b[5];
    *pcr_out = base * 300 + ext;
    return 1;
}

/* ── Wall clock helpers ────────────────────────────────────────────────── */

/* Monotonic wall clock in nanoseconds: immune to NTP steps, which would
 * otherwise show up as a pacing hiccup or a multi-second stall. */
static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static void sleep_until_ns(uint64_t target) {
    uint64_t now = now_ns();
    if (target <= now) {
        return;
    }
    uint64_t delta = target - now;
    struct timespec req = { (time_t)(delta / 1000000000ULL), (long)(delta % 1000000000ULL) };
    /* nanosleep may return early on a signal (our SIGINT); the pump loop
     * checks the flag right after, so just fall through. */
    nanosleep(&req, NULL);
}

/* ── PCR pacer ─────────────────────────────────────────────────────────── */

/*
 * Walks a chunk packet by packet. At each PCR-bearing packet it first
 * sends everything accumulated up to and including that packet, then
 * sleeps until the wall clock has advanced as far past the anchor as the
 * PCR has. Bytes after the last PCR in the chunk are sent at the end at
 * line rate — they belong to the next PCR window, which will pace itself.
 *
 * The pacer keeps everything in 27 MHz until the very last division so
 * dense PCRs keep their sub-millisecond precision.
 */
struct pcr_pacer {
    int      anchored;
    uint64_t t0_pcr;       /* PCR at the anchor */
    uint64_t t0_wall_ns;   /* wall clock at the anchor */
    uint64_t last_pcr;
};

static int pcr_paced_send(struct pcr_pacer *p, tst_sender_t *s,
                          const uint8_t *bytes, size_t len) {
    size_t emit_from = 0;
    for (size_t i = 0; i + TS_PACKET_SIZE <= len; i += TS_PACKET_SIZE) {
        uint64_t pcr;
        if (!parse_pcr(bytes + i, &pcr)) {
            continue;
        }
        /*
         * A PCR that steps backwards by more than a second is a 33-bit
         * rollover (every ~26.5 h) or a `--loop` rewind, not jitter.
         * Re-anchor on it so playback resumes against the new origin
         * instead of sleeping until the old timeline catches up.
         */
        if (p->anchored && pcr < p->last_pcr && (p->last_pcr - pcr) > PCR_REANCHOR_GAP) {
            p->anchored = 0;
        }
        if (!p->anchored) {
            p->t0_pcr = pcr;
            p->t0_wall_ns = now_ns();
            p->anchored = 1;
        }
        p->last_pcr = pcr;

        /* Emit everything up to and including this PCR packet ... */
        const size_t end = i + TS_PACKET_SIZE;
        int rc = tst_sender_send_ts(s, bytes + emit_from, end - emit_from);
        if (rc != 0) {
            return rc;
        }
        emit_from = end;

        /* ... then sleep until wall-clock offset == PCR offset. */
        const uint64_t pcr_offset = pcr - p->t0_pcr;                 /* 27 MHz ticks */
        const uint64_t target = p->t0_wall_ns + (pcr_offset * 1000ULL) / 27ULL;  /* ns */
        sleep_until_ns(target);
        if (g_stop) {
            break;
        }
    }
    if (emit_from < len) {
        return tst_sender_send_ts(s, bytes + emit_from, len - emit_from);
    }
    return 0;
}

/* ── main ──────────────────────────────────────────────────────────────── */

static void usage(const char *why) {
    fprintf(stderr, "%s\nusage: send_srt_ts_file <file.ts> <host:port> "
                    "[--loop] [--no-pace] [--latency-ms <ms>]\n"
                    "  default pacing: PCR-driven (real-time playback)\n",
            why);
}

int main(int argc, char **argv) {
    /* ── Step 1: Args ─────────────────────────────────────────────────── */
    if (argc < 3) {
        usage("missing <file.ts> <host:port>");
        return 2;
    }
    const char *path = argv[1];
    const char *host_port = argv[2];
    int loop_forever = 0;
    int pace = 1;
    unsigned latency_ms = DEFAULT_LATENCY_MS;
    for (int i = 3; i < argc; i++) {
        if (strcmp(argv[i], "--loop") == 0) {
            loop_forever = 1;
        } else if (strcmp(argv[i], "--no-pace") == 0) {
            pace = 0;
        } else if (strcmp(argv[i], "--latency-ms") == 0 && i + 1 < argc) {
            char *end = NULL;
            unsigned long v = strtoul(argv[++i], &end, 10);
            if (end == argv[i] || *end != '\0' || v == 0 || v > 60000) {
                usage("bad --latency-ms");
                return 2;
            }
            latency_ms = (unsigned)v;
        } else {
            usage("unknown argument");
            return 2;
        }
    }

    FILE *in = fopen(path, "rb");
    if (!in) {
        perror(path);
        return 2;
    }

    /*
     * ── Step 2: Sender config + open ─────────────────────────────────────
     *
     * RECOVER framing: the sender checks for 0x47 at every 188-byte
     * boundary and, in this mode, skips a misaligned prefix until it finds
     * sync and re-syncs after loss. A recorded file can start mid-packet
     * (a capture that began between sync bytes) or carry a torn chunk; a
     * relay should shrug that off, not abort. STRICT is for pipelines fed
     * by a muxer you control, where misalignment means a bug you want to
     * hear about — `send_srt.c` discusses the trade in more detail.
     *
     * `latency` goes in the URL: it advertises the TSBPD budget at
     * handshake; both ends negotiate the max of the two values.
     */
    tst_sender_config_t *scfg = tst_sender_config_new();
    if (!scfg) {
        fprintf(stderr, "tst_sender_config_new: out of memory\n");
        fclose(in);
        return 1;
    }
    (void) tst_sender_config_set_framing_mode(scfg, TST_TS_FRAMING_MODE_RECOVER);

    char url[512];
    snprintf(url, sizeof(url), "srt://%s?latency=%u", host_port, latency_ms);
    fprintf(stderr, "dialling %s  (pacing: %s%s)\n", url,
            pace ? "PCR-driven" : "none (blast)", loop_forever ? ", --loop" : "");

    tst_sender_t *sender = tst_sender_open(url, scfg);
    tst_sender_config_free(scfg);
    scfg = NULL;
    if (!sender) {
        fprintf(stderr, "tst_sender_open failed (%d): %s\n",
                tst_get_last_error(), tst_get_last_error_str());
        fclose(in);
        return 3;
    }

    signal(SIGINT, on_sigint);

    /*
     * ── Step 3: Pump the file ────────────────────────────────────────────
     *
     * Read packet-aligned chunks until real EOF. `fread` may return short
     * at EOF, so the byte count is rounded DOWN to a whole number of
     * packets before sending; a trailing partial packet (a file that is
     * not a clean multiple of 188 — rare, cheap to handle) is dropped.
     */
    uint8_t *buf = malloc(READ_CHUNK);
    if (!buf) {
        fprintf(stderr, "out of memory\n");
        tst_sender_close(sender);
        fclose(in);
        return 1;
    }
    struct pcr_pacer pacer;
    memset(&pacer, 0, sizeof(pacer));
    uint64_t total = 0;
    int exit_code = 0;

    while (!g_stop) {
        size_t filled = 0;
        while (filled < READ_CHUNK) {
            size_t n = fread(buf + filled, 1, READ_CHUNK - filled, in);
            if (n == 0) {
                break;                         /* EOF or error */
            }
            filled += n;
        }
        if (ferror(in)) {
            perror("fread");
            exit_code = 4;
            break;
        }
        const size_t aligned = (filled / TS_PACKET_SIZE) * TS_PACKET_SIZE;
        if (aligned == 0) {
            /* Clean EOF. */
            if (!loop_forever) {
                break;
            }
            /* Rewind. The next PCR is far BEHIND the last one seen, so the
             * pacer re-anchors instead of sleeping the file's whole
             * duration — see pcr_paced_send. */
            if (fseek(in, 0, SEEK_SET) != 0) {
                perror("fseek");
                exit_code = 4;
                break;
            }
            fprintf(stderr, "EOF: rewinding (--loop)\n");
            continue;
        }

        int rc = pace ? pcr_paced_send(&pacer, sender, buf, aligned)
                      : tst_sender_send_ts(sender, buf, aligned);
        if (rc != 0) {
            fprintf(stderr, "send_ts failed (rc=%d): %s\n", rc, tst_get_last_error_str());
            exit_code = 5;
            break;
        }
        total += aligned;
    }
    if (g_stop) {
        fprintf(stderr, "SIGINT: stopping\n");
    }

    /*
     * ── Step 4: Flush, drain, close ──────────────────────────────────────
     *
     * flush pushes the last partial 7-packet bundle onto the wire now
     * (close would too, silently — flushing here lets us report a
     * failure on the tail). Then wait one latency window plus margin so
     * libsrt's send buffer empties and the receiver ACKs the tail;
     * closing immediately drops in-flight packets, which the receiver
     * sees as a truncated stream end.
     */
    int frc = tst_sender_flush(sender);
    if (frc != 0 && exit_code == 0) {
        fprintf(stderr, "flush failed (rc=%d): %s\n", frc, tst_get_last_error_str());
        exit_code = 5;
    }
    usleep((latency_ms + 200) * 1000);
    tst_sender_close(sender);
    free(buf);
    fclose(in);

    fprintf(stderr, "session done: streamed %llu bytes (%llu packets)\n",
            (unsigned long long)total, (unsigned long long)(total / TS_PACKET_SIZE));
    return exit_code;
}
