// lwip-loopback — the committed 564-byte video-roundtrip golden round-
// trips through lwIP UDP on the loopback netif. Sender + receiver run as
// FreeRTOS-Plus-POSIX pthreads (the threading API libsrt's sync_posix.cpp binds
// to) concurrently with lwIP's own tcpip thread — the R2-hardening concurrency
// mix. Receiver blocks on select() (libsrt's CEPoll primitive off-Linux).
#include <cstdio>
#include <cstdint>
#include <cstring>
extern "C" {
#include "FreeRTOS.h"
#include "task.h"
#include "FreeRTOS_POSIX.h"
#include "FreeRTOS_POSIX/pthread.h"
#include "lwip/tcpip.h"
#include "lwip/sockets.h"
#include "lwip/inet.h"
}
#include "golden.h"   // build/generated/golden.h (gen-golden-h.sh) — GOLDEN[], GOLDEN_LEN = 564
extern "C" void _exit(int);  // matches startup.c + the libc builtin (void, noreturn)

#define PORT 9000

static volatile int g_lwip_up = 0;
static volatile int g_fail = 0;
static volatile int g_rx_ready = 0;
static int g_rx_sock = -1;
static uint8_t g_rxbuf[1024];
static int g_rxlen = 0;

static void tcpip_ready(void*) { g_lwip_up = 1; }

// Receiver pthread: bind, block on select(), recvfrom into g_rxbuf.
static void* rx_thread(void*) {
    struct sockaddr_in addr; memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = lwip_htons(PORT);
    addr.sin_addr.s_addr = lwip_htonl(INADDR_LOOPBACK);
    g_rx_sock = lwip_socket(AF_INET, SOCK_DGRAM, 0);
    if (g_rx_sock < 0 || lwip_bind(g_rx_sock, (struct sockaddr*)&addr, sizeof addr) != 0) {
        g_fail = 1; return nullptr;
    }
    // Signal the tx thread that the socket is bound. A bound UDP socket will
    // queue the datagram even if select() hasn't started yet, so this is the
    // earliest safe moment for tx to send.
    g_rx_ready = 1;
    fd_set rfds; FD_ZERO(&rfds); FD_SET(g_rx_sock, &rfds);
    struct timeval tv; tv.tv_sec = 5; tv.tv_usec = 0;
    if (lwip_select(g_rx_sock + 1, &rfds, nullptr, nullptr, &tv) <= 0) { g_fail = 1; return nullptr; }
    g_rxlen = lwip_recvfrom(g_rx_sock, g_rxbuf, sizeof g_rxbuf, 0, nullptr, nullptr);
    return nullptr;
}

// Sender pthread: send the full golden buffer to the loopback receiver.
static void* tx_thread(void*) {
    struct sockaddr_in addr; memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = lwip_htons(PORT);
    addr.sin_addr.s_addr = lwip_htonl(INADDR_LOOPBACK);
    int tx = lwip_socket(AF_INET, SOCK_DGRAM, 0);
    // Wait until the receiver is BOUND (not a fixed delay — on a starved TCG
    // runner 50 ms was not always enough, and an unbound receiver silently
    // drops the one datagram). Bounded so an rx failure can't spin forever.
    for (int i = 0; !g_rx_ready; i++) {
        if (g_fail) return nullptr;
        if (i >= 5000) { g_fail = 1; return nullptr; }
        vTaskDelay(pdMS_TO_TICKS(1));
    }
    if (lwip_sendto(tx, GOLDEN, GOLDEN_LEN, 0, (struct sockaddr*)&addr, sizeof addr) < 0) g_fail = 1;
    return nullptr;
}

static void run_task(void*) {
    tcpip_init(tcpip_ready, nullptr);
    while (!g_lwip_up) vTaskDelay(pdMS_TO_TICKS(1));

    pthread_t rx, tx;
    pthread_attr_t attr; pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 4096);   // ≥4 KiB (exceptions finding)
    if (pthread_create(&rx, &attr, rx_thread, nullptr) != 0) {
        printf("FAIL[s1_lwip]: pthread_create(rx)\n"); fflush(stdout); _exit(1);
    }
    if (pthread_create(&tx, &attr, tx_thread, nullptr) != 0) {
        printf("FAIL[s1_lwip]: pthread_create(tx)\n"); fflush(stdout); _exit(1);
    }
    pthread_join(tx, nullptr);
    pthread_join(rx, nullptr);

    int ok = !g_fail
          && (g_rxlen == (int)GOLDEN_LEN)
          && (memcmp(g_rxbuf, GOLDEN, GOLDEN_LEN) == 0);
    if (ok) printf("PASS: s1_lwip (golden %uB through lwIP UDP loopback, select+pthreads+hires-clock)\n",
                   (unsigned)GOLDEN_LEN);
    else    printf("FAIL[s1_lwip]: g_fail=%d rxlen=%d (expected %u)\n",
                   g_fail, g_rxlen, (unsigned)GOLDEN_LEN);
    fflush(stdout);
    _exit(ok ? 0 : 1);
}

int main() {
    xTaskCreate(run_task, "run", 2048, nullptr, 2, nullptr);
    vTaskStartScheduler();
    for (;;) {}
}

extern "C" void vApplicationMallocFailedHook(void) { printf("FAIL[malloc]\n"); _exit(1); }
extern "C" void vApplicationStackOverflowHook(TaskHandle_t, char*) { printf("FAIL[stack]\n"); _exit(1); }
