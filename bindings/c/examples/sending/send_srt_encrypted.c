/*
 * send_srt_encrypted.c — passphrase-encrypted SRT send + receive in ONE
 * process: a listener thread and a caller on the main thread, both
 * configured entirely through URL query keys.
 *
 * Sends 16 short messages over an AES-256 SRT link, receives them all on
 * the listener side, verifies the count, exits 0. The same shape applies
 * across a network — only the address changes.
 *
 * The C twin of the Rust `examples/sending/encrypted_send_recv.rs`. The
 * Rust example builds both ends with `SocketBuilder` / `ListenerBuilder`
 * setters (`.passphrase()`, `.key_length()`, `.stream_id()`); the C ABI has
 * no socket builder — the URL query string IS the per-connection option
 * surface — so every knob that example sets programmatically is spelled
 * here as `?passphrase=...&pbkeylen=32&streamid=...&latency=120`.
 *
 * What this example shows:
 *   1. The RAW (message-oriented, no TS framing) handle pair:
 *      `tst_raw_sender_t` and `tst_raw_receiver_t`. Each `send` is one SRT
 *      message; each `recv` returns exactly one whole message. This is the
 *      lowest-level SRT shape the C ABI offers — no muxer, no 188-byte
 *      alignment — and therefore the cleanest way to see the encryption
 *      handshake in isolation. `mux_synthetic_srt.c` is the same link
 *      carrying MPEG-TS.
 *   2. The listener-side open (`_open_listener`) BLOCKS inside the call
 *      until a caller completes the handshake — bind + accept are one step
 *      in the C ABI. That handshake includes the SRT key-material exchange
 *      (KMREQ / KMRSP), so once `_open_listener` returns, encryption is
 *      already negotiated. A passphrase mismatch never yields a connected
 *      handle on either side: the caller's `_open` fails and the listener
 *      keeps waiting for a caller it can agree with.
 *   3. Thread structure: the listener runs on a pthread because its open
 *      and recv both block; the sender runs on main. A one-shot ready flag
 *      plus SRT's own handshake retry covers the bind-before-connect
 *      ordering (see Step 3).
 *   4. Picking a free loopback port at runtime, so two copies of this
 *      example (or this and another test) never collide.
 *
 * The passphrase below is deliberately named so anyone scanning for
 * hard-coded secrets sees at once that it is not a real credential. Real
 * deployments read the passphrase from the environment or a key file and
 * place it in the URL at runtime — never in source, and never in URL
 * userinfo (`user:pass@`), which the parser rejects precisely because
 * userinfo ends up in logs and shell history.
 *
 * Build (from the ts-transformer workspace root):
 *   SRT_FORCE_VENDORED=1 cargo build -p tst-c --features srt
 *   cc -I target/debug/include -L target/debug -Wall -Werror \
 *      -o /tmp/send_srt_encrypted \
 *      bindings/c/examples/sending/send_srt_encrypted.c -ltstrans -lpthread
 *
 * Run:
 *   LD_LIBRARY_PATH=target/debug /tmp/send_srt_encrypted
 *   # ... 16 "listener: recv" lines, then "OK: 16 encrypted messages round-tripped"
 *
 * Requires: TST_HAS_SRT == 1 (set when the `srt` cargo feature is enabled),
 * and a libtstrans built with the default `mbedtls` feature (encryption
 * support) — a `--no-default-features` build rejects `?passphrase=`.
 */

#include "tstrans.h"

#if !defined(TST_HAS_SRT) || TST_HAS_SRT == 0
#error "This example requires TST_HAS_SRT. Rebuild tst-c with the srt cargo feature enabled."
#endif

#include <arpa/inet.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* ── Constants ─────────────────────────────────────────────────────────── */

/*
 * The shared secret both peers must agree on. libsrt requires 10..79
 * printable ASCII bytes; the URL parser enforces the same range up front
 * (a too-short value fails the open with TST_E_INVALID_CONFIG before any
 * socket exists). 32 characters here.
 */
#define PASSPHRASE   "shared-secret-not-for-production"

/*
 * pbkeylen selects the AES key length: 16 / 24 / 32 bytes = AES-128 / -192
 * / -256. AES-256 is more than these 16 messages need, but it is what a
 * production link would pick, and changing it is a one-token edit.
 */
#define PBKEYLEN     "32"

/*
 * SRTO_STREAMID — an opaque string the caller presents and the listener
 * can read after accept to route or authorize the connection. The C ABI
 * exposes it as a URL key on the caller side only (a listener reads it via
 * the Rust API today).
 */
#define STREAM_ID    "encrypted-demo"

/* TSBPD latency in ms; both ends negotiate the max of their two values, so
 * setting it on both sides to the same number is the common config smell
 * to get right (a mismatch silently picks the larger). */
#define LATENCY_MS   "120"

/* Small on purpose — this is a smoke test of the encrypted link, not a
 * throughput demo. */
#define NUM_MESSAGES 16

/* ── Helper: pick a free loopback port ─────────────────────────────────── */

/*
 * pick_free_port — bind a TCP socket to 127.0.0.1:0, read back the port the
 * kernel chose, close the socket, return the port.
 *
 * WHY TCP when SRT runs over UDP?
 *   Asking the kernel for "any free port" via bind(0) + getsockname is the
 *   standard idiom, and a TCP bind does not conflict with the UDP bind SRT
 *   does moments later. There is a small window between close() and the SRT
 *   bind in which another process could grab the port; on a single dev
 *   machine that is effectively zero, and it is the same trade the Rust
 *   twin makes. Returns 0 on any failure (the caller treats that as fatal).
 */
static uint16_t pick_free_port(void) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        return 0;
    }
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = 0;
    socklen_t len = sizeof(addr);
    uint16_t port = 0;
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0 &&
        getsockname(fd, (struct sockaddr *)&addr, &len) == 0) {
        port = ntohs(addr.sin_port);
    }
    close(fd);
    return port;
}

/* ── Listener thread ───────────────────────────────────────────────────── */

/*
 * Everything the listener thread needs, passed by pointer. `ready` is set
 * by the thread immediately before it enters the blocking open, so main
 * can wait for "about to bind" (the closest observable point — the C ABI
 * offers no separate bind step). `received` carries the result back.
 */
struct listener_ctx {
    char url[256];
    atomic_int ready;
    int received;
    int failed;
};

static void *listener_main(void *arg) {
    struct listener_ctx *ctx = arg;

    /*
     * Signal "about to bind", then block in bind + accept. Once this
     * returns the encrypted link is fully negotiated — KMREQ/KMRSP happen
     * inside the SRT handshake, before accept completes.
     */
    atomic_store(&ctx->ready, 1);
    tst_raw_receiver_t *rx = tst_raw_receiver_open_listener(ctx->url);
    if (!rx) {
        fprintf(stderr, "listener: open_listener failed: %s\n", tst_get_last_error_str());
        ctx->failed = 1;
        return NULL;
    }
    fprintf(stderr, "listener: accepted a caller (encryption negotiated in the handshake)\n");

    /*
     * 1500 bytes is above SRT's default 1316-byte payload, so every recv
     * returns one whole message; a message larger than the buffer would be
     * reported as TST_E_TOO_LARGE with the buffer untouched.
     *
     * Three-outcome loop: 0 = one message; TST_E_END_OF_STREAM = the caller
     * closed cleanly (our normal exit if it closes before we hit the
     * count); anything else is a real error.
     */
    uint8_t buf[1500];
    while (ctx->received < NUM_MESSAGES) {
        size_t n = 0;
        int rc = tst_raw_receiver_recv(rx, buf, sizeof(buf), &n);
        if (rc == 0) {
            fprintf(stderr, "listener: recv %zu bytes (msg %d): %.*s\n",
                    n, ctx->received, (int)n, (const char *)buf);
            ctx->received++;
        } else if (rc == TST_E_END_OF_STREAM) {
            fprintf(stderr, "listener: peer closed after %d messages\n", ctx->received);
            break;
        } else {
            fprintf(stderr, "listener: recv failed (rc=%d): %s\n", rc, tst_get_last_error_str());
            ctx->failed = 1;
            break;
        }
    }
    tst_raw_receiver_close(rx);
    return NULL;
}

/* ── main ──────────────────────────────────────────────────────────────── */

int main(void) {
    /*
     * ── Step 1: Port + URLs ──────────────────────────────────────────────
     *
     * Both URLs carry the SAME passphrase and pbkeylen — that agreement is
     * the whole of "configuring encryption". The listener URL names the
     * bind address; the caller URL additionally carries streamid (a
     * caller-side option). `latency` is set on both for the reason in the
     * constant's comment.
     */
    uint16_t port = pick_free_port();
    if (port == 0) {
        fprintf(stderr, "could not pick a free loopback port\n");
        return 1;
    }

    struct listener_ctx ctx;
    memset(&ctx, 0, sizeof(ctx));
    atomic_init(&ctx.ready, 0);
    snprintf(ctx.url, sizeof(ctx.url),
             "srt://127.0.0.1:%u?passphrase=%s&pbkeylen=%s&latency=%s",
             (unsigned)port, PASSPHRASE, PBKEYLEN, LATENCY_MS);

    char caller_url[256];
    snprintf(caller_url, sizeof(caller_url),
             "srt://127.0.0.1:%u?passphrase=%s&pbkeylen=%s&streamid=%s&latency=%s",
             (unsigned)port, PASSPHRASE, PBKEYLEN, STREAM_ID, LATENCY_MS);

    /*
     * ── Step 2: Start the listener thread ────────────────────────────────
     *
     * A thread because `_open_listener` and `_recv` both block, and we
     * want the caller's connect to run concurrently on main.
     */
    pthread_t listener;
    if (pthread_create(&listener, NULL, listener_main, &ctx) != 0) {
        perror("pthread_create");
        return 1;
    }

    /*
     * ── Step 3: Wait for the listener to be (about to be) bound ─────────
     *
     * The flag flips just before the listener thread calls open, so this
     * loop plus a short pause gives bind() time to land before our first
     * handshake datagram. Belt-and-braces: even if we connect a hair too
     * early, SRT's caller handshake retransmits for the connect timeout
     * (default 3 s), so the connection still completes — the pause only
     * avoids burning a retransmit round-trip.
     */
    while (atomic_load(&ctx.ready) == 0) {
        usleep(1000);
    }
    usleep(50 * 1000);

    /*
     * ── Step 4: Open the encrypted caller ────────────────────────────────
     *
     * Passing NULL for the config = defaults (the raw sender has no
     * framing to configure). A NULL return here with the URL rejected is
     * TST_E_INVALID_CONFIG (e.g. a passphrase shorter than 10 bytes); a
     * handshake that fails — including a passphrase MISMATCH — is
     * TST_E_TRANSPORT.
     */
    tst_raw_sender_t *tx = tst_raw_sender_open(caller_url, NULL);
    if (!tx) {
        fprintf(stderr, "sender: open failed (%d): %s\n",
                tst_get_last_error(), tst_get_last_error_str());
        /* The listener is blocked in accept with nobody coming; exiting the
         * process is the only clean way out (see managed_reconnect.c for
         * the same limitation and its bounded-join pattern). */
        return 2;
    }

    /*
     * ── Step 5: Send 16 messages ─────────────────────────────────────────
     *
     * The 20 ms cadence is only so the listener's per-message log line is
     * readable as the example runs. A real publisher pushes back-to-back
     * and lets SRT's pacing layer shape the wire rate; pacing in the
     * application is the wrong layer.
     */
    int exit_code = 0;
    for (int i = 0; i < NUM_MESSAGES; i++) {
        char msg[64];
        int len = snprintf(msg, sizeof(msg), "encrypted message %02d", i);
        int rc = tst_raw_sender_send(tx, (const uint8_t *)msg, (size_t)len);
        if (rc != 0) {
            fprintf(stderr, "sender: send[%d] failed (rc=%d): %s\n",
                    i, rc, tst_get_last_error_str());
            exit_code = 3;
            break;
        }
        usleep(20 * 1000);
    }
    if (exit_code == 0) {
        fprintf(stderr, "sender: sent %d messages\n", NUM_MESSAGES);
    }

    /*
     * ── Step 6: Drain, close, join ───────────────────────────────────────
     *
     * Sleep longer than the latency window before close so the tail of
     * the stream is ACKed and delivered; close tears the link down
     * promptly and would otherwise race the last datagrams, leaving the
     * listener short. Then join the listener — it exits on its own once it
     * has the count or sees our close as END_OF_STREAM.
     */
    usleep(200 * 1000);
    tst_raw_sender_close(tx);
    pthread_join(listener, NULL);

    if (ctx.failed || ctx.received != NUM_MESSAGES) {
        fprintf(stderr, "FAIL: received %d of %d\n", ctx.received, NUM_MESSAGES);
        return exit_code ? exit_code : 4;
    }
    printf("OK: %d encrypted messages round-tripped\n", NUM_MESSAGES);
    return exit_code;
}
