/* the same handle with its body in the header: part of the ABI */
typedef struct tst_fix_h { int a; } tst_fix_h;
int tst_fix_use(struct tst_fix_h *h);
