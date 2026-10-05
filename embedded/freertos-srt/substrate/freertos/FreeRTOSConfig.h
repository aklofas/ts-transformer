#ifndef FREERTOS_CONFIG_H
#define FREERTOS_CONFIG_H

/* Minimal FreeRTOS V11.1.0 config for the Cortex-M4F (ARM_CM4F) port on
   QEMU mps2-an386. C++ exceptions want generous task stacks, so stacks are
   sized at task-create time; the heap is large to leave room for libstdc++. */

#define configUSE_PREEMPTION                    1
#define configUSE_TIME_SLICING                  1
#define configUSE_PORT_OPTIMISED_TASK_SELECTION 0
#define configCPU_CLOCK_HZ                      (25000000)   /* QEMU mps2 ~25 MHz */
#define configTICK_RATE_HZ                      (1000)
#define configMAX_PRIORITIES                    (5)
#define configMINIMAL_STACK_SIZE                (256)        /* words */
#define configSTACK_DEPTH_TYPE                  uint32_t     /* lwIP sys_thread_new byte→word conversion */
/* loopback-arq: the SRT data plane spins up several worker pthreads (2 per multiplexer +
 * GC) plus our 2 app pthreads, each with a multi-KiB stack carved from this
 * heap. NOTE the two-heap split: ucHeap (this define) backs task stacks +
 * kernel objects ONLY; libsrt's buffers and all C++/malloc allocations come
 * from the separate ~2.8 MiB newlib _sbrk heap set up by the linker script.
 * 192 KiB (libsrt-smoke's boot-smoke budget) is far too small for the stacks
 * alone. RAM is 4 MiB (mps2_an386.ld), so be generous. */
#define configTOTAL_HEAP_SIZE                   (1024 * 1024)
#define configMAX_TASK_NAME_LEN                 (16)
#define configUSE_16_BIT_TICKS                  0
#define configIDLE_SHOULD_YIELD                 1
#define configUSE_MUTEXES                       1
#define configUSE_RECURSIVE_MUTEXES             1
#define configQUEUE_REGISTRY_SIZE               0
#define configUSE_TASK_NOTIFICATIONS            1
#define configTASK_NOTIFICATION_ARRAY_ENTRIES   1

/* Hooks */
#define configUSE_IDLE_HOOK                     0
#define configUSE_TICK_HOOK                     0
#define configUSE_MALLOC_FAILED_HOOK            1
#define configCHECK_FOR_STACK_OVERFLOW          2
#define configUSE_DAEMON_TASK_STARTUP_HOOK      0

/* Software timers */
#define configUSE_TIMERS                        1
#define configTIMER_TASK_PRIORITY               (3)
#define configTIMER_QUEUE_LENGTH                10
#define configTIMER_TASK_STACK_DEPTH            (256)

/* Thread-local storage — two slots: slot 0 backs libsrt's pthread_key TSD
   (pthread_key_shim.c), slot 1 holds per-task __cxa_eh_globals (cxa_override.cpp)
   so concurrent exceptions don't clobber each other. loopback-arq needs BOTH at once
   (libsrt's setOpt etc. throw from within pthreads that also use pthread_key),
   so they must live on distinct slots. */
#define configNUM_THREAD_LOCAL_STORAGE_POINTERS 2

/* Allocation. FreeRTOS-Plus-POSIX creates pthread join mutex/barrier via the
   xSemaphoreCreate*Static APIs, so static allocation must be on. Let the kernel
   supply the idle/timer task static memory (configKERNEL_PROVIDED_STATIC_MEMORY)
   so we don't hand-roll vApplicationGet{Idle,Timer}TaskMemory. */
#define configSUPPORT_STATIC_ALLOCATION         1
#define configSUPPORT_DYNAMIC_ALLOCATION        1
#define configKERNEL_PROVIDED_STATIC_MEMORY     1

/* FreeRTOS-Plus-POSIX needs these: it stashes the pthread object in the task's
   application tag (vTaskSetApplicationTaskTag), and uses the POSIX errno field
   in the TCB for its return-error reporting. */
#define configUSE_APPLICATION_TASK_TAG          1
#define configUSE_POSIX_ERRNO                   1

/* Per-task newlib _reent (errno, stdio state) + the retargeted newlib locks in
 * substrate/newlib_lock.c together make newlib safe under preemption. */
#define configUSE_NEWLIB_REENTRANT              1
#define configUSE_COUNTING_SEMAPHORES           1

/* Cortex-M interrupt priority configuration */
#define configPRIO_BITS                         3
#define configLIBRARY_LOWEST_INTERRUPT_PRIORITY      (7)
#define configLIBRARY_MAX_SYSCALL_INTERRUPT_PRIORITY (5)
#define configKERNEL_INTERRUPT_PRIORITY \
    (configLIBRARY_LOWEST_INTERRUPT_PRIORITY << (8 - configPRIO_BITS))
#define configMAX_SYSCALL_INTERRUPT_PRIORITY \
    (configLIBRARY_MAX_SYSCALL_INTERRUPT_PRIORITY << (8 - configPRIO_BITS))

/* Optional API */
#define INCLUDE_vTaskPrioritySet                1
#define INCLUDE_uxTaskPriorityGet               1
#define INCLUDE_vTaskDelete                     1
#define INCLUDE_vTaskSuspend                    1
#define INCLUDE_vTaskDelayUntil                 1
#define INCLUDE_vTaskDelay                      1
#define INCLUDE_xTaskGetSchedulerState          1
/* FreeRTOS-Plus-POSIX uses these in pthread_detach/join/cond/cancel paths. */
#define INCLUDE_eTaskGetState                   1
#define INCLUDE_xTaskAbortDelay                 1
#define INCLUDE_xTaskGetCurrentTaskHandle       1
#define INCLUDE_xSemaphoreGetMutexHolder        1

extern void vAssertCalled(const char *file, unsigned long line);
#define configASSERT(x) do { if ((x) == 0) { vAssertCalled(__FILE__, __LINE__); } } while (0)

/* Map the FreeRTOS port handlers onto the names our vector table uses. The
   ARM_CM4F port already exports vPortSVCHandler/xPortPendSVHandler/
   xPortSysTickHandler, and startup.c references those directly, so no remap
   is needed — but if a CMSIS-style build expected SVC_Handler etc., this is
   where the #define would go. */

#endif /* FREERTOS_CONFIG_H */
