// loopback-arq — SRT caller + listener (FILE mode) on one device over the lossy
// loopback netif. The 564-byte golden is streamed REPEAT times and the listener
// reconstructs it byte-exact under ~20% deterministic data-packet loss, proving
// SRT ARQ recovery on the substrate. ENCRYPT=1 adds mbedTLS AES-128 plus a
// negotiated-KM assertion. -DFREERTOS_SRT_CONNECT_PORT points the caller at a
// dead port to exercise the caller-failure path (the arq-connfail gate).
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <srt/srt.h>                 // C++ linkage (pulls C++ stdlib)
extern "C" {
#include "FreeRTOS.h"
#include "task.h"
#include "FreeRTOS_POSIX.h"
#include "FreeRTOS_POSIX/pthread.h"
#include "lwip/tcpip.h"
#include "lwip/inet.h"
#include "lossy_netif.h"
}
#include "golden.h"                  // GOLDEN[], GOLDEN_LEN = 564
#include "srt_opts.h"

#define PORT   9001
// The arq-connfail gate points the caller at a dead port to force a connect
// failure (nothing listens there), proving a caller-side failure aborts the
// listener instead of wedging pthread_join.
#ifndef FREERTOS_SRT_CONNECT_PORT
#define FREERTOS_SRT_CONNECT_PORT PORT
#endif
#define REPEAT 64
static const int STREAM_LEN = (int)GOLDEN_LEN * REPEAT;   // 36096 bytes

// Distinct gate token + label per phase (Phase B sets SRT_PASSPHRASE).
#ifdef SRT_PASSPHRASE
#define PASS_TAG "s3_srt_aes"
#define ENC_SUFFIX " (AES-128 encrypted)"
#else
#define PASS_TAG "s3_srt_plain"
#define ENC_SUFFIX ""
#endif

static volatile int      g_up = 0;
static volatile int      g_listen_ready = 0;
static volatile int      g_fail = 0;
static const char*       g_where = "";
static volatile SRTSOCKET g_ls = SRT_INVALID_SOCK;
static uint8_t           g_rxbuf[STREAM_LEN];
static int               g_rxlen = 0;
static SRT_TRACEBSTATS   g_rx_stats;

static void tcpip_ready(void*) { g_up = 1; }

// First failure wins (a caller-side abort also wakes the listener, whose own
// srt_accept error must not overwrite the root cause), and the label prints
// IMMEDIATELY: if anything later still wedges, the transcript already names
// where things went wrong instead of an anonymous timeout.
static void fail(const char* w) {
    if (g_fail) return;
    g_fail = 1; g_where = w;
    printf("FAIL-DETAIL: where=%s\n", w); fflush(stdout);
}

// On any pre-connect caller failure, close the listen socket too.
// srt_accept has no deadline — without this, the listener stays parked in
// srt_accept forever, run_task wedges in pthread_join, and the gate's outer
// timeout kills QEMU with an empty transcript. Closing the listen socket makes
// srt_accept return; the listener exits and the verdict prints within seconds.
// Closing here can race the listener's own srt_close(ls) — that double-close
// is deliberately relied on as safe: libsrt's srt_close() returns 0 for
// sockets already CLOSING/CLOSED/NONEXIST, CUDT::close() no-ops on m_bBroken,
// and actual teardown is deferred to libsrt's GC, so no double-free is
// possible (verified against crates/srt-sys/vendor/srt srt_c_api.cpp + api.cpp).
static void abort_listener(void) {
    SRTSOCKET ls = g_ls;
    if (ls != SRT_INVALID_SOCK) srt_close(ls);
}

// Listener: bind/listen/accept, then srt_recv until STREAM_LEN bytes.
static void* listener_thread(void*) {
    struct sockaddr_in sa; memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET; sa.sin_port = lwip_htons(PORT);
    sa.sin_addr.s_addr = lwip_htonl(0x0A000001);   /* 10.0.0.1 (our lossy netif) */
    SRTSOCKET ls = srt_create_socket();
    if (ls == SRT_INVALID_SOCK) { fail("listen_create"); return nullptr; }
    // On every pre-accept failure close the partially-created listen socket and
    // set g_fail BEFORE returning. The caller's wait loop watches g_fail, so a
    // setup failure here surfaces as FAIL[...] instead of leaving the caller
    // spinning on g_listen_ready (which it never sets) -> QEMU timeout.
    if (srt_apply_opts(ls) != 0)                              { fail("listen_opts"); srt_close(ls); return nullptr; }
    if (srt_bind(ls, (sockaddr*)&sa, sizeof sa) == SRT_ERROR) { fail("bind");        srt_close(ls); return nullptr; }
    if (srt_listen(ls, 1) == SRT_ERROR)                       { fail("listen");      srt_close(ls); return nullptr; }
    g_ls = ls;
    g_listen_ready = 1;
    SRTSOCKET cs = srt_accept(ls, nullptr, nullptr);
    if (cs == SRT_INVALID_SOCK) { fail("accept"); srt_close(ls); return nullptr; }
    int got = 0;
    while (got < STREAM_LEN) {
        int n = srt_recv(cs, (char*)g_rxbuf + got, STREAM_LEN - got);
        if (n <= 0) { fail("recv"); break; }
        got += n;
    }
    g_rxlen = got;
    // Snapshot receiver stats AFTER the full stream is in: pktRcvRetransTotal is
    // now final and counts every retransmitted packet we had to receive to
    // recover the ~20% the lossy netif dropped. (The sender's pktRetransTotal,
    // sampled right after its send loop, would still be 0 in FILE mode — that
    // loop only buffers; retransmission happens during srt_close's linger.)
    srt_bstats(cs, &g_rx_stats, 0);
    srt_close(cs);
    srt_close(ls);
    return nullptr;
}

// Caller: connect, then srt_send the golden REPEAT times.
static void* caller_thread(void*) {
    // Wait for the listener to reach srt_listen. Abort if it failed during setup
    // (g_fail) — otherwise a pre-accept listener failure would never set
    // g_listen_ready and this loop would spin forever, degrading a real failure
    // into a QEMU timeout. The 10s bound is a backstop for any unforeseen stall.
    for (int i = 0; !g_listen_ready; i++) {
        if (g_fail) return nullptr;
        if (i >= 10000) { fail("listen_ready_timeout"); abort_listener(); return nullptr; }
        vTaskDelay(pdMS_TO_TICKS(1));
    }
    struct sockaddr_in sa; memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET; sa.sin_port = lwip_htons(FREERTOS_SRT_CONNECT_PORT);
    sa.sin_addr.s_addr = lwip_htonl(0x0A000001);   /* 10.0.0.1 (our lossy netif) */
    SRTSOCKET cs = srt_create_socket();
    if (cs == SRT_INVALID_SOCK) { fail("call_create"); abort_listener(); return nullptr; }
    if (srt_apply_opts(cs) != 0) { fail("call_opts"); srt_close(cs); abort_listener(); return nullptr; }
    if (srt_connect(cs, (sockaddr*)&sa, sizeof sa) == SRT_ERROR) { fail("connect"); srt_close(cs); abort_listener(); return nullptr; }
#ifdef SRT_PASSPHRASE
    // The s3_srt_aes PASS token must prove encryption was actually NEGOTIATED,
    // not merely configured: SRT_KM_S_UNSECURED here would mean plaintext flowed
    // and the gate would still have passed on byte-equality alone.
    int km = SRT_KM_S_UNSECURED; int kmlen = (int)sizeof km;
    if (srt_getsockflag(cs, SRTO_SNDKMSTATE, &km, &kmlen) == SRT_ERROR
        || km != SRT_KM_S_SECURED) {
        fail("km_state"); srt_close(cs); abort_listener(); return nullptr;
    }
#endif
    // Handshake (+ KM in Phase B) is complete now; loss may be enabled.
    lossy_set_enabled(FREERTOS_SRT_LOSS_ENABLED);
    for (int r = 0; r < REPEAT && !g_fail; r++) {
        int off = 0;
        while (off < (int)GOLDEN_LEN) {
            int n = srt_send(cs, (const char*)GOLDEN + off, (int)GOLDEN_LEN - off);
            if (n <= 0) { fail("send"); break; }
            off += n;
        }
    }
    srt_close(cs);
    return nullptr;
}

static void run_task(void*) {
    tcpip_init(tcpip_ready, nullptr);
    while (!g_up) vTaskDelay(pdMS_TO_TICKS(1));
    lossy_netif_up();
    lossy_set_enabled(0);              // off until the caller connects

    // srt_startup failure is terminal: every later SRT call would run in an
    // invalid init state, turning a clean startup failure into secondary hangs
    // or misleading where= labels. Fail fast before any worker thread exists.
    if (srt_startup() < 0) { printf("FAIL[" PASS_TAG "]: where=startup\n"); fflush(stdout); _exit(1); }

    pthread_t lt, ct;
    pthread_attr_t attr; pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 32768);         // libsrt API call chains are deep
    if (pthread_create(&lt, &attr, listener_thread, nullptr) != 0) {
        printf("FAIL[" PASS_TAG "]: pthread_create(listener)\n"); fflush(stdout); _exit(1);
    }
    if (pthread_create(&ct, &attr, caller_thread, nullptr) != 0) {
        printf("FAIL[" PASS_TAG "]: pthread_create(caller)\n"); fflush(stdout); _exit(1);
    }
    pthread_join(ct, nullptr);
    pthread_join(lt, nullptr);

    // Verify: full length + every 564-byte chunk equals the golden.
    int bytes_ok = (g_rxlen == STREAM_LEN);
    for (int r = 0; bytes_ok && r < REPEAT; r++)
        if (memcmp(g_rxbuf + r * GOLDEN_LEN, GOLDEN, GOLDEN_LEN) != 0) bytes_ok = 0;

    // dropped = packets the netif deterministically injected as loss (0 when the
    // filter is off). In FILE mode the ONLY way the stream still reconstructs
    // byte-exact (bytes_ok) is SRT retransmitting each dropped packet, so
    // dropped>0 && bytes_ok proves ARQ recovery. rcv_loss is SRT's own
    // receiver-side loss tally, reported for corroboration (it can read 1 even
    // with no injected loss, so it is NOT the gate).
    unsigned dropped = lossy_dropped_count();
    int rcvloss = g_rx_stats.pktRcvLossTotal;

    int ok = !g_fail && bytes_ok && dropped > 0;     // injected loss recovered byte-exact
    if (ok) printf("PASS: " PASS_TAG " (GOLDEN x %d recovered byte-exact under ~20%% loss%s, dropped=%u, rcv_loss=%d)\n",
                   REPEAT, ENC_SUFFIX, dropped, rcvloss);
    else    printf("FAIL[" PASS_TAG "]: where=%s rxlen=%d/%d bytes_ok=%d dropped=%u rcv_loss=%d\n",
                   g_where, g_rxlen, STREAM_LEN, bytes_ok, dropped, rcvloss);
    fflush(stdout);
    srt_cleanup();
    _exit(ok ? 0 : 1);
}

int main() {
    xTaskCreate(run_task, "run", 4096, nullptr, 2, nullptr);
    vTaskStartScheduler();
    for (;;) {}
}

extern "C" void vApplicationMallocFailedHook(void) { printf("FAIL[malloc]\n"); _exit(1); }
extern "C" void vApplicationStackOverflowHook(TaskHandle_t, char*) { printf("FAIL[stack]\n"); _exit(1); }
