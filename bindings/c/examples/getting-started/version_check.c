/*
 * version_check.c — verify the loaded libtstrans matches the compiled-against header.
 *
 * Build (Linux x86_64, with libtstrans built locally):
 *   cd bindings/c
 *   cargo build
 *   cc -I include -L ../../target/debug \
 *      -Wall -Werror -o /tmp/version_check \
 *      examples/getting-started/version_check.c -ltstrans
 *
 * Run:
 *   LD_LIBRARY_PATH=../../target/debug /tmp/version_check
 *
 * What it does: queries every tst_get_*_version_* runtime accessor,
 * compares each result against the corresponding TST_*_VERSION_* compile-
 * time macro from the header, prints all seven values, checks each
 * feature query against its TST_HAS_* define, and exits 0 on agreement /
 * 1 on mismatch.
 *
 * Why this matters:
 *
 * The 3-tier version model (package + ABI + header) lets binding authors
 * (tst-jni, tst-uniffi, pure-C consumers) detect SO/header mismatches:
 *
 *   - Compile-time header macros (TST_*_VERSION_*): what the header you
 *     compiled against says.
 *   - Runtime accessors (tst_get_*_version_*): what the libtstrans.so
 *     actually loaded at this runtime reports.
 *
 * Within a single build artifact, the two ALWAYS match (both sourced
 * from Cargo.toml at compile time). They diverge ONLY when an old
 * header is paired with a new SO, or vice versa — exactly the failure
 * mode this example demonstrates how to catch.
 *
 * Binding authors should run a check like this at library load:
 *
 *     if (tst_get_abi_version_major() != TST_ABI_VERSION_MAJOR) {
 *         fprintf(stderr, "tstrans ABI major mismatch\n");
 *         exit(1);
 *     }
 *     if (tst_get_abi_version_minor() < TST_ABI_VERSION_MINOR) {
 *         fprintf(stderr, "tstrans ABI minor too old\n");
 *         exit(1);
 *     }
 *
 * From here:
 *   - For the smallest mux example:  hello_world.c
 *   - For SRT sending:                muxing/mux_synthetic_srt.c
 */

#include "tstrans.h"
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>

/*
 * Helper: assert that runtime == header. The check is symmetric — we
 * print both values regardless of agreement so a passing run still
 * shows what version was built. On mismatch, we set the `mismatched`
 * out-flag so main() exits non-zero AFTER reporting all four package +
 * two ABI checks (not on the first failure — that way developers see
 * the full picture in one run).
 */
static void check_pair(const char *label,
                       uint32_t runtime,
                       uint32_t compile_time,
                       int *mismatched) {
    const char *verdict = (runtime == compile_time) ? "OK" : "MISMATCH";
    printf("  %-26s runtime=%u  header=%u  [%s]\n",
           label, runtime, compile_time, verdict);
    if (runtime != compile_time) {
        *mismatched = 1;
    }
}

int main(void) {
    int mismatched = 0;

    /*
     * ── Tier 1: Package / runtime version ──────────────────────────────
     *
     * Tracks Cargo.toml. Bumps every release per SemVer. Binding authors
     * surface this to their consumers as the "library version" string
     * (e.g., `Tstrans.VERSION` in a Java wrapper). The packed encoding
     * `(M<<16)|(m<<8)|p` lets consumers compare versions as integers.
     */
    printf("Package version (matches Cargo.toml):\n");
    check_pair("TST_VERSION_MAJOR",
               tst_get_version_major(), TST_VERSION_MAJOR, &mismatched);
    check_pair("TST_VERSION_MINOR",
               tst_get_version_minor(), TST_VERSION_MINOR, &mismatched);
    check_pair("TST_VERSION_PATCH",
               tst_get_version_patch(), TST_VERSION_PATCH, &mismatched);

    /*
     * The packed value is its own derivation; no corresponding compile-
     * time macro. Print it for completeness — useful when consumers want
     * to compare versions in a single integer comparison.
     */
    uint32_t packed = tst_get_version_packed();
    uint32_t packed_expected =
        ((uint32_t)TST_VERSION_MAJOR << 16) |
        ((uint32_t)TST_VERSION_MINOR <<  8) |
        ((uint32_t)TST_VERSION_PATCH      );
    check_pair("packed (M<<16|m<<8|p)",
               packed, packed_expected, &mismatched);

    /*
     * The version string accessor returns a process-lifetime static C
     * string; caller must NOT free. We just print it as-is for the
     * smoke check.
     */
    const char *version_str = tst_get_version_string();
    printf("  %-26s runtime=\"%s\"\n", "version string", version_str);

    /*
     * ── Tier 2: ABI contract version ──────────────────────────────────
     *
     * Bumped ONLY on breaking C-ABI change. Bindings cross-check this at
     * library load — major MUST match exactly; minor SHOULD be >= the
     * value the binding was compiled against.
     */
    printf("\nABI contract version (breaking-change cadence):\n");
    check_pair("TST_ABI_VERSION_MAJOR",
               tst_get_abi_version_major(), TST_ABI_VERSION_MAJOR, &mismatched);
    check_pair("TST_ABI_VERSION_MINOR",
               tst_get_abi_version_minor(), TST_ABI_VERSION_MINOR, &mismatched);

    /*
     * ── Feature query ─────────────────────────────────────────────────
     *
     * The TST_HAS_* defines describe the build the header came from. A
     * program that loads libtstrans at run time (dlopen / LoadLibrary)
     * never sees the build the loaded library came from, so it asks:
     * tst_has_feature(name) is 1 when that library was built with the
     * cargo feature, 0 otherwise. Ask BEFORE resolving the feature's
     * symbols, since a library built without it does not export them.
     * Here the library and the header come from one build, so the two
     * answers must agree.
     */
    printf("\nBuilt-in features (runtime query vs header define):\n");
    check_pair("srt",  (uint32_t)tst_has_feature("srt"),  TST_HAS_SRT,  &mismatched);
    check_pair("rtp",  (uint32_t)tst_has_feature("rtp"),  TST_HAS_RTP,  &mismatched);
    check_pair("udp",  (uint32_t)tst_has_feature("udp"),  TST_HAS_UDP,  &mismatched);
    check_pair("tcp",  (uint32_t)tst_has_feature("tcp"),  TST_HAS_TCP,  &mismatched);
    check_pair("hls",  (uint32_t)tst_has_feature("hls"),  TST_HAS_HLS,  &mismatched);
    check_pair("rist", (uint32_t)tst_has_feature("rist"), TST_HAS_RIST, &mismatched);
    check_pair("tls",  (uint32_t)tst_has_feature("tls"),  TST_HAS_TLS,  &mismatched);

    /*
     * ── Tier 3: Header compile-time macros ────────────────────────────
     *
     * The TST_*_VERSION_* macros from <tstrans.h> were already printed
     * in the comparisons above as the "header=" column. No separate
     * runtime equivalent — they're literally compile-time integer
     * constants emitted by cbindgen from the `pub const` declarations
     * in the library source.
     */

    /*
     * ── Bonus: tst_clear_last_error round-trip ────────────────────────
     *
     * Quick sanity check that the new last-error-clear entry doesn't
     * crash. We don't have a way to set an error from this example
     * (every fallible tst_* needs allocated handles + a non-trivial
     * setup), so we just demonstrate the call shape. The function
     * never fails — no return code to check.
     */
    tst_clear_last_error();
    int code_after_clear = tst_get_last_error();
    const char *msg_after_clear = tst_get_last_error_str();
    printf("\nAfter tst_clear_last_error():\n");
    printf("  tst_get_last_error()     = %d  (expect 0 = TST_E_SUCCESS)\n",
           code_after_clear);
    printf("  tst_get_last_error_str() = \"%s\"  (expect empty)\n",
           msg_after_clear);
    if (code_after_clear != 0 || msg_after_clear[0] != '\0') {
        mismatched = 1;
    }

    /*
     * ── Verdict ────────────────────────────────────────────────────────
     */
    if (mismatched) {
        fprintf(stderr,
                "\nFAIL: at least one runtime/header pair mismatched. "
                "This indicates the loaded libtstrans.so does NOT match "
                "the tstrans.h compiled into this binary. Rebuild against "
                "the same tstrans installation.\n");
        return 1;
    }

    printf("\nOK: all runtime/header pairs match. Loaded libtstrans is "
           "consistent with the tstrans.h compiled into this binary.\n");
    return 0;
}
