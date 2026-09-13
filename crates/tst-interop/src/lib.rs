// tst-interop library root.
// Modules land in later tasks.

/// How often `send`/`recv`'s long-running loops emit a one-line
/// progress heartbeat to stderr (→ `soak.sh`'s per-process log files).
/// Added after soak run 1 (2026-08-04) died 14.5h in with nothing in
/// the send logs but the final panic — a periodic
/// counters-and-bytes line bounds "when did it stop, and how far had it
/// gotten" to one interval without needing RUST_LOG. Short interop
/// matrix cells (seconds long) never reach the first beat, so their
/// logs are unchanged.
pub(crate) const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub mod cli;
pub mod fixtures;
// `gen` is a reserved keyword since the 2024 edition (future generator
// syntax) — the module still lives at `src/gen.rs` / is invoked as the
// `gen` CLI subcommand, just spelled `r#gen` at every Rust use site.
pub mod r#gen;
pub mod impair;
pub mod mux_setup;
pub mod profiles;
pub mod proxy;
pub mod rawts;
pub mod recv;
pub mod report;
pub mod report_types;
pub mod schedule;
pub mod send;
pub mod serve;
pub mod transport;
pub mod verify;
