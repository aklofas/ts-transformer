/* ABI fixture v2: a new exported function beside the old one (additive). */
typedef enum { TST_FIX_A = 0, TST_FIX_B = 1 } tst_fix_kind;
typedef struct { unsigned long long count; int flags; } tst_fix_stats;

int tst_fix_get(tst_fix_stats *out, tst_fix_kind kind) {
    out->count = (unsigned long long)kind;
    out->flags = 0;
    return 0;
}

int tst_fix_more(tst_fix_stats *out) { out->count = 1; return 0; }
