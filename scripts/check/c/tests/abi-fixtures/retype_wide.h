/* Borrows the suppressed name tst_sender_config_set_framing_mode on purpose, to exercise the
   enum-retype pin in abi-compat.sh; goes with the block in bindings/c/abi.suppr when it expires. */
#include <stdint.h>
struct tst_sender_config_t;
enum tst_ts_framing_mode { TST_TS_FRAMING_MODE_A = 0, TST_TS_FRAMING_MODE_B = 1 };
/* Sets the mode; a comment with a semicolon; must not end the declaration. */
int tst_sender_config_set_framing_mode(struct tst_sender_config_t *p,
                                    int64_t mode);
