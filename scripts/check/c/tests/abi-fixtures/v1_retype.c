/* Borrows the suppressed name tst_sender_config_set_framing_mode on purpose, to exercise the
   enum-retype pin in abi-compat.sh; goes with the block in bindings/c/abi.suppr when it expires. */
#include <stdint.h>
enum tst_ts_framing_mode { TST_TS_FRAMING_MODE_A = 0, TST_TS_FRAMING_MODE_B = 1 };
struct tst_sender_config_t;
int tst_sender_config_set_framing_mode(struct tst_sender_config_t *p, enum tst_ts_framing_mode mode) {
    (void)p;
    return (int)mode;
}
