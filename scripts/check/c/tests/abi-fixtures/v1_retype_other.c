/* Borrows the suppressed name tst_sender_config_set_framing_mode on purpose, to exercise the
   enum-retype pin in abi-compat.sh; goes with the block in bindings/c/abi.suppr when it expires. */
#include <stdint.h>
enum tst_other_mode { TST_OTHER_A = 0, TST_OTHER_B = 1 };
struct tst_sender_config_t;
int tst_sender_config_set_framing_mode(struct tst_sender_config_t *p, int32_t mode, enum tst_other_mode extra) {
    (void)p; (void)extra;
    return (int)mode;
}
