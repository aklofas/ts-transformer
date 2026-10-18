/* ABI fixture v2: a field appended to the struct the function takes. */
typedef enum { TST_FIX_A = 0, TST_FIX_B = 1 } tst_fix_kind;
typedef struct { unsigned long long count; int flags; unsigned long long extra; } tst_fix_stats;

int tst_fix_get(tst_fix_stats *out, tst_fix_kind kind) {
    out->count = (unsigned long long)kind;
    out->flags = 0;
    out->extra = 0;
    return 0;
}
