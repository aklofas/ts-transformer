// malloc-stress — regression proof that malloc/free stays safe under
// preemption and that per-task errno stays isolated. 4 equal-priority
// tasks hammer malloc/free with per-block canaries under 1 kHz
// time-slicing, periodically throw/catch (the EH-alloc malloc path), and
// cross-check per-task errno isolation. Any heap corruption or errno
// bleed prints a labeled FAIL and exits 1. main() also throws BEFORE the
// scheduler starts (pre-scheduler EH bootstrap regression).
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cerrno>
extern "C" void _exit(int);  // matches startup.c + the libc builtin (void, noreturn)
extern "C" {
#include "FreeRTOS.h"
#include "task.h"
#include "semphr.h"
}

static SemaphoreHandle_t s_done;
static volatile int s_fail = 0;

static void fail(const char *what, unsigned id, int iter) {
    s_fail = 1;
    printf("FAIL[s5_malloc_stress]: %s (task %u iter %d)\n", what, id, iter);
    fflush(stdout);
    _exit(1);
}

static void churn_task(void *arg) {
    const unsigned id = (unsigned)(uintptr_t)arg;
    unsigned lcg = 0x9E3779B9u ^ (id * 0x85EBCA6Bu);
    for (int i = 0; i < 20000 && !s_fail; i++) {
        lcg = lcg * 1664525u + 1013904223u;
        const size_t n = 16 + (lcg % 497);
        unsigned char *p = (unsigned char *)malloc(n);
        if (!p) fail("alloc returned null", id, i);
        const unsigned char pat = (unsigned char)(id * 31 + (unsigned)i);
        memset(p, pat, n);
        taskYIELD(); // widen the cross-task window while the block is live
        for (size_t k = 0; k < n; k++)
            if (p[k] != pat) fail("canary corrupted", id, i);
        free(p);
        if ((i & 63) == 0) {
            try { throw i; } catch (int) {} // __cxa EH-alloc under churn
        }
        if ((i & 255) == 0) {
            errno = (int)(1000 + id);
            taskYIELD();
            if (errno != (int)(1000 + id)) fail("errno bled across tasks", id, i);
        }
    }
    xSemaphoreGive(s_done);
    vTaskDelete(nullptr);
}

static void collector_task(void *) {
    for (int i = 0; i < 4; i++) xSemaphoreTake(s_done, portMAX_DELAY);
    printf("PASS: s5_malloc_stress (4 tasks x 20000 alloc/free + EH + errno)\n");
    fflush(stdout);
    _exit(0);
}

int main() {
    try { throw 1; } catch (int) {} // pre-scheduler EH bootstrap
    s_done = xSemaphoreCreateCounting(4, 0);
    if (!s_done) { printf("FAIL[s5_malloc_stress]: semaphore create\n"); fflush(stdout); _exit(1); }
    for (unsigned t = 0; t < 4; t++)
        if (xTaskCreate(churn_task, "churn", 4096, (void *)(uintptr_t)t, 2, nullptr) != pdPASS) {
            printf("FAIL[s5_malloc_stress]: xTaskCreate churn\n"); fflush(stdout); _exit(1);
        }
    if (xTaskCreate(collector_task, "collect", 2048, nullptr, 3, nullptr) != pdPASS) {
        printf("FAIL[s5_malloc_stress]: xTaskCreate collector\n"); fflush(stdout); _exit(1);
    }
    vTaskStartScheduler();
    for (;;) {}
}

extern "C" void vApplicationMallocFailedHook(void) { printf("FAIL[malloc]\n"); _exit(1); }
extern "C" void vApplicationStackOverflowHook(TaskHandle_t, char *) { printf("FAIL[stack]\n"); _exit(1); }
