# Sanitizer suppressions

Files in this directory tune the nightly `sanitizers` GHA workflow
(see `.github/workflows/sanitizers.yml`).

## Files

- `asan.txt` — AddressSanitizer + LeakSanitizer suppression rules.
  Loaded via `LSAN_OPTIONS=suppressions=...` env var.
- `tsan.txt` — ThreadSanitizer suppression rules. Loaded via
  `TSAN_OPTIONS=suppressions=...` env var.

## Symbolizer

Suppressions match on symbolised function names, so every job that loads
a suppressions file must have `llvm-symbolizer` on PATH (the jobs install
the `llvm` package; all four sanitizer jobs install it). Without it the runtime falls back to `addr2line`,
which symbolises each suppressed report through a fresh process per test
process and stalls the test for seconds (the "TSan `rtsp_server` test margins"
class in `docs/project/ci-flakes.md`).

## When to add a suppression

When a nightly run surfaces a finding that's:
- A false positive from a third-party dependency we don't control AND
- Doesn't affect production code AND
- Has no realistic upstream fix path.

**Always include in the suppression block:**
- The date it was added.
- A 1-2 line description of the symptom.
- The CI run URL that surfaced it.
- A pointer to a follow-up issue, plan, or "investigate within N weeks" note.

## When to remove a suppression

- The underlying issue was fixed (in our code or upstream).
- The follow-up date passed AND nobody re-triaged: remove it and let it
  surface again on the next nightly run — fresh data is more useful
  than stale assumptions.

## Format references

- ASan / LSan suppressions:
  https://github.com/google/sanitizers/wiki/AddressSanitizerLeakSanitizer#suppressions
- TSan suppressions:
  https://github.com/google/sanitizers/wiki/ThreadSanitizerSuppressions
