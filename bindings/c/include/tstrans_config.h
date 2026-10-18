/* tstrans_config.h - the bare flavour: no transport feature is declared.
 * Every tst-c build writes its own tstrans_config.h beside the generated
 * tstrans.h, defining TST_HAS_<FEATURE> to 1 for each feature that build
 * enabled. This committed copy lets tstrans.h be used straight from the
 * source tree, where every feature macro then falls back to 0. */
#ifndef TSTRANS_CONFIG_H
#define TSTRANS_CONFIG_H
#endif /* TSTRANS_CONFIG_H */
