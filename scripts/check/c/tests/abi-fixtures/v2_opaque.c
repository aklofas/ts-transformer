/* ABI fixture: the same handle struct with a field appended. */
struct tst_fix_h { int a; long b; };
int tst_fix_use(struct tst_fix_h *h) { return h->a; }
