/* newlib locking backend for FreeRTOS.
 *
 * Dual-mode: the build-time macro _RETARGETABLE_LOCKING (set in newlib.h)
 * selects which interface newlib's libc.a calls at runtime.
 *
 * Mode 1 — _RETARGETABLE_LOCKING defined (xpack 14.2.1 and other modern
 *   toolchains): libc.a references __retarget_lock_* and links dummy bx-lr
 *   no-ops. Under a preemptive scheduler those no-ops leave malloc/free/
 *   stdio/env completely unsynchronized. These strong definitions override
 *   the archive dummies, giving every newlib subsystem a real lock.
 *
 * Mode 2 — _RETARGETABLE_LOCKING NOT defined (distro gcc-arm-none-eabi and
 *   older toolchains): sys/lock.h macros are no-ops, stdio locking is
 *   compiled out of libc — we cannot reach it. malloc and env are still
 *   function-call-based (__malloc_lock/__env_lock in mlock.o/envlock.o);
 *   providing strong definitions here overrides those archive defaults.
 *   Residual: stdio FILE locking is unavailable in this libc configuration.
 *
 * Shared pre-scheduler behaviour: scheduler_running() returns false until
 * vTaskStartScheduler has been called; all acquire/release paths are no-ops
 * in that window (mirrors pthread_key_shim.c's pattern). The static locks
 * are created lazily inside a suspend-all window so a post-scheduler
 * first-use cannot race. */

#include <sys/lock.h>   /* must come first — sets _RETARGETABLE_LOCKING via newlib.h */
#include <reent.h>      /* struct _reent — needed by Mode 2 __malloc_lock signatures */
#include <stdlib.h>
#include "FreeRTOS.h"
#include "semphr.h"
#include "task.h"
#include "diag.h"

/* ── shared helpers (both modes) ──────────────────────────────────────── */

static int scheduler_running(void) {
    return xTaskGetSchedulerState() != taskSCHEDULER_NOT_STARTED;
}

/* Create a recursive mutex into *h using *buf if it hasn't been created yet.
 * Called inside a suspend-all window or before the scheduler starts. */
static void lazy_create(SemaphoreHandle_t *h, StaticSemaphore_t *buf) {
    if (*h) return;
    *h = xSemaphoreCreateRecursiveMutexStatic(buf);
}

/* Heap-morecore guard for startup.c's _sbrk (belt-and-suspenders: malloc's
 * own lock already serializes the malloc→_sbrk path). */
void tst_heap_lock(void)   { if (scheduler_running()) vTaskSuspendAll(); }
void tst_heap_unlock(void) { if (scheduler_running()) (void)xTaskResumeAll(); }

/* ── Mode 1: retargetable locking ────────────────────────────────────── */

#ifdef _RETARGETABLE_LOCKING

struct __lock {
    SemaphoreHandle_t h;
    StaticSemaphore_t buf;
};

struct __lock __lock___sinit_recursive_mutex;
struct __lock __lock___sfp_recursive_mutex;
struct __lock __lock___atexit_recursive_mutex;
struct __lock __lock___at_quick_exit_mutex;
struct __lock __lock___malloc_recursive_mutex;
struct __lock __lock___env_recursive_mutex;
struct __lock __lock___tz_mutex;
struct __lock __lock___dd_hash_mutex;
struct __lock __lock___arc4random_mutex;

/* All newlib-internal locks are taken recursively-safe; backing every lock
 * with a recursive mutex is a superset of the required semantics. */
static void ensure(struct __lock *l) {
    if (l->h) return;
    vTaskSuspendAll();
    lazy_create(&l->h, &l->buf);
    (void)xTaskResumeAll();
}

void __retarget_lock_init(_LOCK_T *lock) { __retarget_lock_init_recursive(lock); }

void __retarget_lock_init_recursive(_LOCK_T *lock) {
    struct __lock *l = calloc(1, sizeof *l);
    if (!l) tst_diag_fail("lock_alloc");
    *lock = l;
}

void __retarget_lock_close(_LOCK_T lock) { __retarget_lock_close_recursive(lock); }

void __retarget_lock_close_recursive(_LOCK_T lock) {
    if (lock && lock->h) vSemaphoreDelete(lock->h);
    free(lock);
}

void __retarget_lock_acquire(_LOCK_T lock) { __retarget_lock_acquire_recursive(lock); }

void __retarget_lock_acquire_recursive(_LOCK_T lock) {
    if (!scheduler_running()) return;
    ensure(lock);
    (void)xSemaphoreTakeRecursive(lock->h, portMAX_DELAY);
}

int __retarget_lock_try_acquire(_LOCK_T lock) { return __retarget_lock_try_acquire_recursive(lock); }

int __retarget_lock_try_acquire_recursive(_LOCK_T lock) {
    if (!scheduler_running()) return 1;
    ensure(lock);
    return xSemaphoreTakeRecursive(lock->h, 0) == pdTRUE;
}

void __retarget_lock_release(_LOCK_T lock) { __retarget_lock_release_recursive(lock); }

void __retarget_lock_release_recursive(_LOCK_T lock) {
    if (!scheduler_running()) return;
    (void)xSemaphoreGiveRecursive(lock->h);
}

/* ── Mode 2: non-retargetable newlib (distro gcc-arm-none-eabi) ──────── */

#else /* !_RETARGETABLE_LOCKING */

/* Non-retargetable newlib (e.g. distro gcc-arm-none-eabi): only the malloc
 * and env locks are function-call-based (mlock.o / envlock.o) and can be
 * overridden with strong definitions. stdio FILE locking is compiled out of
 * this libc build — documented residual. */

static SemaphoreHandle_t s_malloc_mtx;
static StaticSemaphore_t s_malloc_buf;
static SemaphoreHandle_t s_env_mtx;
static StaticSemaphore_t s_env_buf;

static void ensure2(SemaphoreHandle_t *h, StaticSemaphore_t *buf) {
    if (*h) return;
    vTaskSuspendAll();
    lazy_create(h, buf);
    (void)xTaskResumeAll();
}

void __malloc_lock(struct _reent *r) {
    (void)r;
    if (!scheduler_running()) return;
    ensure2(&s_malloc_mtx, &s_malloc_buf);
    (void)xSemaphoreTakeRecursive(s_malloc_mtx, portMAX_DELAY);
}

void __malloc_unlock(struct _reent *r) {
    (void)r;
    if (!scheduler_running()) return;
    (void)xSemaphoreGiveRecursive(s_malloc_mtx);
}

void __env_lock(struct _reent *r) {
    (void)r;
    if (!scheduler_running()) return;
    ensure2(&s_env_mtx, &s_env_buf);
    (void)xSemaphoreTakeRecursive(s_env_mtx, portMAX_DELAY);
}

void __env_unlock(struct _reent *r) {
    (void)r;
    if (!scheduler_running()) return;
    (void)xSemaphoreGiveRecursive(s_env_mtx);
}

#endif /* _RETARGETABLE_LOCKING */
