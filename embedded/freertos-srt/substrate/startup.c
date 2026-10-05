/* Cortex-M4F startup for QEMU mps2-an386 + newlib/rdimon semihosting + FreeRTOS.
 *
 * Adapted from embedded/baremetal-qemu-c/firmware/startup.c. The only structural
 * change for FreeRTOS is the vector table: the kernel's ARM_CM4F port installs
 * the context-switch + tick machinery via the SVC, PendSV and SysTick exception
 * vectors. Their handler symbols are vPortSVCHandler / xPortPendSVHandler /
 * xPortSysTickHandler (see embedded/vendor/freertos-kernel/portable/GCC/ARM_CM4F/port.c).
 * We wire them into vector slots 11 (SVCall), 14 (PendSV) and 15 (SysTick).
 *
 * Root-cause notes carried over from c-1:
 *   - __libc_init_array() calls _init() first; _init is assembled from
 *     crti.o (prologue) + crtn.o (epilogue). The linker script must sequence
 *     KEEP(*crti.o(.init)) … KEEP(*crtn.o(.init)) or _init never returns.
 *   - _exit() is overridden with a raw SYS_EXIT semihosting BKPT for a clean,
 *     newlib-version-independent QEMU process exit.
 */
#include <stdint.h>
#include <errno.h>
#include "diag.h"

extern uint32_t _sidata, _sdata, _edata, _sbss, _ebss, _estack;
extern char end, _heap_end;
extern int  main(void);
extern void __libc_init_array(void);
extern void initialise_monitor_handles(void); /* rdimon */

/* FreeRTOS ARM_CM4F port handlers (defined in vendor port.c). */
extern void vPortSVCHandler(void);
extern void xPortPendSVHandler(void);
extern void xPortSysTickHandler(void);

/* Forward declaration — defined at the bottom of this file. */
__attribute__((noreturn)) void _exit(int status);

void Reset_Handler(void);
static void Default_Handler(void) { tst_diag_fail("unexpected_irq"); }

/* Speaking fault handlers: recover the exception frame from MSP or PSP
 * (EXC_RETURN bit 2 selects which) then call a C reporter that prints the
 * label, stacked PC, and stacked LR before calling _exit(1).  The naked
 * wrapper is required because the hardware has already pushed the exception
 * frame onto the stack we must read; a normal prologue would corrupt r0. */
__attribute__((used)) static void fault_report(uint32_t *frame, const char *label) {
    /* Stacked frame layout (ARMv7-M): r0 r1 r2 r3 r12 lr pc xpsr */
    char hex[9];
    tst_diag_write0("FAIL[");
    tst_diag_write0(label);
    tst_diag_write0("] pc=0x");
    tst_diag_hex32(frame[6], hex); tst_diag_write0(hex);
    tst_diag_write0(" lr=0x");
    tst_diag_hex32(frame[5], hex); tst_diag_write0(hex);
    tst_diag_write0("\n");
    _exit(1);
}

#define TST_FAULT_HANDLER(asm_name, c_name, label_str)                        \
    __attribute__((used)) static void c_name(uint32_t *frame) {               \
        fault_report(frame, label_str);                                       \
    }                                                                         \
    __attribute__((naked)) static void asm_name(void) {                       \
        __asm volatile("tst lr, #4        \n"                                 \
                       "ite eq            \n"                                 \
                       "mrseq r0, msp     \n"                                 \
                       "mrsne r0, psp     \n"                                 \
                       "b %0              \n" : : "i"(c_name));               \
    }

TST_FAULT_HANDLER(HardFault_Handler, hardfault_c, "hardfault")
TST_FAULT_HANDLER(MemManage_Handler, memmanage_c, "memmanage")
TST_FAULT_HANDLER(BusFault_Handler,  busfault_c,  "busfault")
TST_FAULT_HANDLER(UsageFault_Handler, usagefault_c, "usagefault")

/* Cortex-M vector table. Slots 0..15 are the system exceptions; we only need
   the system ones FreeRTOS uses plus reset/NMI/HardFault. Intervening slots
   are padded with Default_Handler so the indices line up. */
__attribute__((section(".isr_vector"), used))
void (* const g_vectors[])(void) = {
    (void (*)(void))(&_estack), /* [0]  initial MSP        */
    Reset_Handler,              /* [1]  Reset              */
    Default_Handler,            /* [2]  NMI                */
    HardFault_Handler,          /* [3]  HardFault          */
    MemManage_Handler,          /* [4]  MemManage          */
    BusFault_Handler,           /* [5]  BusFault           */
    UsageFault_Handler,         /* [6]  UsageFault         */
    0,                          /* [7]  reserved           */
    0,                          /* [8]  reserved           */
    0,                          /* [9]  reserved           */
    0,                          /* [10] reserved           */
    vPortSVCHandler,            /* [11] SVCall             */
    Default_Handler,            /* [12] DebugMon           */
    0,                          /* [13] reserved           */
    xPortPendSVHandler,         /* [14] PendSV             */
    xPortSysTickHandler,        /* [15] SysTick            */
};

void Reset_Handler(void) {
    /* Enable the FPU (CP10/CP11 full access) — hard-float target. */
    volatile uint32_t *cpacr = (volatile uint32_t *)0xE000ED88u;
    *cpacr |= (0xFu << 20);
    __asm volatile("dsb"); __asm volatile("isb");

    /* Copy .data from FLASH to RAM. */
    for (uint32_t *s = &_sidata, *d = &_sdata; d < &_edata; ) *d++ = *s++;
    /* Zero .bss. */
    for (uint32_t *d = &_sbss; d < &_ebss; ) *d++ = 0;

    initialise_monitor_handles();
    __libc_init_array();
    _exit(main());
    for (;;) {} /* unreachable; satisfies noreturn analysis */
}

extern void tst_heap_lock(void);
extern void tst_heap_unlock(void);

/* Self-contained heap for newlib malloc / the C++ runtime.
 * tst_heap_lock/unlock are no-ops before the scheduler starts; under a
 * running scheduler they suspend-all so concurrent malloc → _sbrk paths
 * can't race on the brk pointer. */
void *_sbrk(int incr) {
    static char *brk = 0;
    tst_heap_lock();
    if (!brk) brk = &end;
    char *prev;
    if (brk + incr > &_heap_end || brk + incr < &end) {
        errno = ENOMEM;
        prev = (void *)-1;
    } else {
        prev = brk;
        brk += incr;
    }
    tst_heap_unlock();
    return prev;
}

/* Override newlib's _exit() with a direct ARM semihosting SYS_EXIT BKPT.
 *   R0 = 0x18 (SYS_EXIT)
 *   R1 = ADP_Stopped_ApplicationExit (0x20026) for exit(0),
 *        ADP_Stopped_RunTimeError    (0x20023) for any non-zero status.
 * QEMU maps ADP_Stopped_ApplicationExit → process exit 0. */
__attribute__((noreturn)) void _exit(int status) {
    register uint32_t r0 __asm("r0") = 0x18u; /* SYS_EXIT */
    register uint32_t r1 __asm("r1") =
        (status == 0) ? 0x20026u   /* ADP_Stopped_ApplicationExit */
                      : 0x20023u;  /* ADP_Stopped_RunTimeError    */
    __asm volatile("bkpt 0xab" : : "r"(r0), "r"(r1));
    for (;;) {} /* unreachable */
}
