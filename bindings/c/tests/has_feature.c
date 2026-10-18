/*
 * Compiled and run from tests/abi/has_feature.rs against the header the
 * build generated for the active cargo features. For every feature the
 * header's TST_HAS_* define and the runtime tst_has_feature() answer must
 * agree: that agreement is what lets a dlopen user trust the runtime query
 * in place of the compile-time define.
 */

#include "tstrans.h"
#include <stdio.h>

#define CHECK(name, define_value)                                          \
    do {                                                                   \
        int rt = tst_has_feature(name);                                    \
        if (rt != (define_value)) {                                        \
            fprintf(stderr, "feature %s: header says %d, runtime says %d\n", \
                    name, (define_value), rt);                             \
            failed = 1;                                                    \
        }                                                                  \
    } while (0)

int main(void) {
    int failed = 0;
    CHECK("srt", TST_HAS_SRT);
    CHECK("rtp", TST_HAS_RTP);
    CHECK("udp", TST_HAS_UDP);
    CHECK("tcp", TST_HAS_TCP);
    CHECK("hls", TST_HAS_HLS);
    CHECK("rist", TST_HAS_RIST);
    CHECK("tls", TST_HAS_TLS);
    /* Anything else, including NULL, is "not built with it". */
    CHECK("SRT", 0);
    CHECK("quic", 0);
    CHECK("", 0);
    if (tst_has_feature(NULL) != 0) {
        fprintf(stderr, "NULL must answer 0\n");
        failed = 1;
    }
    if (failed) return 1;
    printf("has_feature OK\n");
    return 0;
}
