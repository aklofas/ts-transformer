# ST 0601 fixture files

Committed `.klv` fixtures used by `tests/klv/st0601.rs`.

## Synthetic

Generated via `cargo run -p tst-core --bin gen-synthetic-fixtures`. Re-run any time
the encoder is intentionally changed; commit the diff alongside the change.

| File | Description |
|---|---|
| `synthetic_minimal.klv` | Minimum-viable record (timestamp only) |
| `synthetic_full.klv` | All ~41 typed tags populated |
| `synthetic_funky_ul.klv` | Non-canonical UL (version byte = 0x09 = ST 0601.9) |
| `synthetic_field_errors.klv` | Carries a deliberately-malformed Tag 13 in the unknown bag |

## MISB public test vectors

When MISB ships official ST 0601 test vectors via gwg.nga.mil/misb, drop them
here as `misb_<name>.klv` and add a row to this README documenting their
provenance. Total budget: a few hundred KB across all fixtures.

## Local-only fixtures (sensitive)

Real-world `.ts` and `.klv` extractions from operational captures live at
`crates/tst-core/tests/fixtures/local/` (gitignored). The
`crates/tst-core/tests/klv/local_fixtures.rs` test discovers and exercises
them when present.
