//! Maintainer tool: prints the `BindingErrorKind` table for
//! `scripts/check/repo/kind-equivalence.sh` — one
//! `variant_name\tname\tcode\tc_projection` line per `ALL` entry, in table
//! order, which is exactly the TSV's first four columns. The rail compares
//! the two line by line, so the order is part of the contract. Not learner
//! code; see `docs/reference/api-stability.md` for what these names mean.
fn main() {
    for k in tst_pipeline::binding::BindingErrorKind::ALL {
        println!(
            "{}\t{}\t{}\t{}",
            k.variant_name(),
            k.name(),
            k.c_code(),
            k.c_projection()
        );
    }
}
