/* Port-specific FreeRTOS+POSIX configuration for the exceptions C++ exceptions gate
 * (arm-none-eabi-gcc / newlib, Cortex-M4F, QEMU mps2-an386).
 *
 * FreeRTOS_POSIX.h includes this header first. Its job is to suppress the
 * FreeRTOS+POSIX typedefs that the arm-none-eabi newlib headers ALSO define,
 * so the two don't collide. Modeled on the vendored STM32+GNU port header
 * (portable/st/stm32h745zi_nucleo/FreeRTOS_POSIX_portable.h) — same newlib
 * lineage, same collisions. */

#ifndef _FREERTOS_POSIX_PORTABLE_H_
#define _FREERTOS_POSIX_PORTABLE_H_

/* newlib already defines these via <sys/_timespec.h> / <sys/timespec.h>. */
#define posixconfigENABLE_TIMESPEC      0
#include <sys/_timespec.h>

#define posixconfigENABLE_ITIMERSPEC    0
#include <sys/timespec.h>

/* newlib already provides mode_t and clockid_t. */
#define posixconfigENABLE_MODE_T        0
#define posixconfigENABLE_CLOCKID_T     0

/* Suppress newlib timer_t so it doesn't fight the FreeRTOS+POSIX one. */
#define __timer_t_defined               1

/* Block newlib's <sys/_pthreadtypes.h> definitions (which differ from the
 * FreeRTOS+POSIX pthread_* typedefs) before pulling <sys/types.h>. */
#define _SYS__PTHREADTYPES_H_
#include <sys/types.h>

/* Use newlib's sched_param/sched_yield/etc. rather than the POSIX-layer copy. */
/* Latent scheduling cliffs — documented, not
 * fixed, because no gate exercises them:
 * - SCHED_PARAM is disabled, so every pthread runs at the FreeRTOS-Plus-POSIX
 *   default priority (tskIDLE_PRIORITY, 0). All SRT worker threads time-slice
 *   at one priority; a competing priority>=1 compute task would starve the
 *   whole SRT data plane. Fine for the QEMU gates (nothing else runs);
 *   revisit before reusing this substrate in a real product.
 * - posixconfigPTHREAD_COND_MAX_WAITERS defaults to 4; a 5th concurrent
 *   waiter on one condvar gets an immediate ENOMEM, which libsrt does not
 *   check -> busy-spin. The gates never exceed 4; raise it if the workload
 *   grows. */
#define posixconfigENABLE_SCHED_PARAM   0
#include <sys/sched.h>

/* loopback-arq: libsrt spawns its snd/rcv queue worker pthreads with the DEFAULT attr, so
 * the default pthread stack (PTHREAD_STACK_MIN = configMINIMAL_STACK_SIZE*4 =
 * 1 KiB) applies — far too small for libsrt's select()+packet+congctl call
 * chains (FreeRTOS stack-overflow hook fired). Raise the floor. Defined here
 * (before FreeRTOS_POSIX_portable_default.h's #ifndef) so it wins. usStackSize
 * is stored in a uint16_t, so keep this < 64 KiB. */
#define PTHREAD_STACK_MIN  16384

#endif /* _FREERTOS_POSIX_PORTABLE_H_ */
