/* Borrows the suppressed name tst_sender_config_set_framing_mode on purpose, to exercise the
   enum-retype pin in abi-compat.sh; goes with the block in bindings/c/abi.suppr when it expires. */
#include <stdint.h>

struct tst_sender_config_t;
int64_t tst_sender_config_set_framing_mode(struct tst_sender_config_t *p, int32_t mode) {
    (void)p;
    return (int64_t)mode;
}
