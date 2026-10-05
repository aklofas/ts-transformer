#include <cstdio>
#include <cstdint>
#include <exception>
extern "C" {
#include "FreeRTOS.h"
#include "task.h"
#include "FreeRTOS_POSIX.h"
#include "FreeRTOS_POSIX/pthread.h"
}
extern "C" void _exit(int);  // matches startup.c + the libc builtin (void, noreturn)

// Re-runs the concurrent C++ exception gate with the worker threads
// created as FreeRTOS-Plus-POSIX *pthreads* — the threading API libsrt's
// sync_posix.cpp binds to — to validate that the per-task __cxa_eh_globals
// override (cxa_override.cpp) still isolates exception state when threads come
// in through the POSIX wrapper (which maps each pthread onto a FreeRTOS task
// via xTaskCreate + a per-task application tag).

struct TaggedError { int id; };

static const int N = 3;
static const int M = 20000;
static volatile int g_fail = 0;

// A local whose destructor yields *during* stack unwinding — i.e. while THIS
// thread's exception is still in flight on __cxa_eh_globals. That hands control
// to another thread that also throws, so without per-task globals the shared
// in-flight state (uncaught-exception count + the propagating-exception ptr)
// gets clobbered. std::uncaught_exceptions() must read exactly 1 here.
struct YieldOnUnwind {
    int my_id;
    ~YieldOnUnwind() {
        if (std::uncaught_exceptions() != 1) g_fail = 1;
        taskYIELD();
    }
};

// Each pthread throws ITS OWN id and must catch exactly that — any cross-thread
// bleed of exception state shows up as e.id != my_id. The mid-unwind yield (via
// YieldOnUnwind's destructor) and the in-catch yield force the throws of the 3
// threads to interleave WHILE an exception is in flight on the shared global.
static void* exc_thread(void* arg) {
    int my_id = (int)(intptr_t)arg;
    for (int i = 0; i < M; i++) {
        try {
            YieldOnUnwind guard{my_id};
            throw TaggedError{my_id};
        } catch (const TaggedError& e) {
            if (e.id != my_id) g_fail = 1;
            taskYIELD();   // yield while still inside the catch (caughtExceptions live)
        }
        taskYIELD();
    }
    return nullptr;
}

// pthread_create must run from a FreeRTOS task context (the POSIX layer calls
// FreeRTOS task APIs under the hood), so the scheduler is started with this
// single bootstrap task that spawns + joins the workers, then reports.
static void bootstrap_task(void*) {
    pthread_t th[N];
    // The default pthread stack (PTHREAD_STACK_MIN = configMINIMAL_STACK_SIZE *
    // sizeof(StackType_t) = 1 KiB) is far too small for the C++ exception
    // unwinder. Match the 4 KiB (1024-word) stack this gate's raw-xTaskCreate
    // workers originally used so the two-phase unwind has room.
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 4096);
    for (int i = 0; i < N; i++) {
        if (pthread_create(&th[i], &attr, exc_thread, (void*)(intptr_t)i) != 0) {
            printf("FAIL[s0_cpp_gate]: pthread_create failed\n");
            fflush(stdout);
            _exit(1);
        }
    }
    for (int i = 0; i < N; i++) pthread_join(th[i], nullptr);

    if (g_fail) printf("FAIL[s0_cpp_gate]: cross-thread exception corruption\n");
    else        printf("PASS: s0_cpp_gate (%d threads, %d iters, no corruption)\n", N, M);
    fflush(stdout);
    _exit(g_fail ? 1 : 0);
}

int main() {
    // Verify that C++ EH machinery is usable before vTaskStartScheduler
    // (static ctors or early error paths may legitimately throw pre-scheduler).
    try { throw 1; } catch (int) {}

    xTaskCreate(bootstrap_task, "boot", 1024, nullptr, 2, nullptr);
    vTaskStartScheduler();
    for (;;) {}
}
extern "C" void vApplicationMallocFailedHook(void) { printf("FAIL[malloc]\n"); _exit(1); }
extern "C" void vApplicationStackOverflowHook(TaskHandle_t, char*) { printf("FAIL[stack]\n"); _exit(1); }
