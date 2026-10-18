/* ABI fixture: a function takes a pointer to a handle struct. */
struct tst_fix_h { int a; };
int tst_fix_use(struct tst_fix_h *h) { return h->a; }
