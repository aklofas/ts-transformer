/*
 * Compiled and run from tests/abi/has_feature.rs. The driver passes the
 * features the test binary was built with as -DEXPECT_<NAME>=0|1, so the
 * check does not depend on which header rendering is on disk.
 */

#include "tstrans.h"
#include <stdio.h>

#if !defined(EXPECT_SRT) || !defined(EXPECT_RTP) || !defined(EXPECT_UDP) || \
    !defined(EXPECT_TCP) || !defined(EXPECT_HLS) || !defined(EXPECT_RIST) || \
    !defined(EXPECT_TLS)
#error "pass -DEXPECT_<NAME>=0|1 for every feature"
#endif

#define CHECK(name, define_value)                                          \
    do {                                                                   \
        int rt = tst_has_feature(name);                                    \
        if (rt != (define_value)) {                                        \
            fprintf(stderr, "feature %s: expected %d, runtime says %d\n", \
                    name, (define_value), rt);                             \
            failed = 1;                                                    \
        }                                                                  \
    } while (0)

int main(void) {
    int failed = 0;
    CHECK("srt", EXPECT_SRT);
    CHECK("rtp", EXPECT_RTP);
    CHECK("udp", EXPECT_UDP);
    CHECK("tcp", EXPECT_TCP);
    CHECK("hls", EXPECT_HLS);
    CHECK("rist", EXPECT_RIST);
    CHECK("tls", EXPECT_TLS);
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
