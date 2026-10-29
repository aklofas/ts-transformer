//! Generate or check cross-binding scenario fixtures.
//!
//! Default mode: iterate all scenarios, write input artifacts and
//! `golden.json` files under the crate-local
//! `crates/tst-integration/tests/fixtures/scenarios/<id>/` (resolved from
//! `CARGO_MANIFEST_DIR`), and regenerate
//! `crates/tst-integration/tests/fixtures/scenarios/scenarios.toml`.
//! This crate-local path — not a workspace-root `tests/fixtures/scenarios` — is
//! the canonical location all binding adapters (Rust/C/Python) resolve against.
//!
//! `--check` mode: regenerate into a temp dir and compare it with the
//! committed fixtures (`tst_integration::scenarios::check::drift`): every
//! golden, the manifest, and every generated input byte for byte. Exits
//! non-zero with one message per difference, keeping the temp dir to diff.
//!
//! # Synthetic data only
//!
//! This binary NEVER reads from `testfiles/`, any `local/` directory, or any
//! real corpus.  All scenario generators produce purely synthetic inputs.

use std::path::{Path, PathBuf};

use tst_integration::scenarios::all_scenarios;
use tst_integration::scenarios::check::{ScenarioEntry, ScenariosManifest, drift, golden_rel};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let check_mode = args.iter().any(|a| a == "--check");

    // Locate the workspace root relative to this binary's location.
    // `CARGO_MANIFEST_DIR` is set during `cargo run`; fall back to CWD.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("cwd"));
    let fixtures_dir = manifest_dir
        .join("tests")
        .join("fixtures")
        .join("scenarios");

    if check_mode {
        run_check(&fixtures_dir);
    } else {
        run_generate(&fixtures_dir);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Generate mode
// ─────────────────────────────────────────────────────────────────────────────

fn run_generate(fixtures_dir: &Path) {
    std::fs::create_dir_all(fixtures_dir).expect("create fixtures dir");

    let scenarios = all_scenarios();
    let mut entries: Vec<ScenarioEntry> = Vec::new();

    for scenario in &scenarios {
        let (input_rel, golden) = scenario.generate(fixtures_dir);

        // Write golden.json.
        let golden_abs = fixtures_dir.join(golden_rel(scenario.as_ref()));
        let golden_json = serde_json::to_string_pretty(&golden).expect("serialise golden");
        std::fs::write(&golden_abs, &golden_json).expect("write golden.json");

        entries.push(ScenarioEntry::new(scenario.as_ref(), &input_rel, &golden));

        println!("  generated: {}", scenario.id());
    }

    // Write scenarios.toml manifest.
    let manifest = ScenariosManifest { scenario: entries };
    let toml_str = toml::to_string(&manifest).expect("serialise manifest");
    let manifest_path = fixtures_dir.join("scenarios.toml");
    std::fs::write(&manifest_path, &toml_str).expect("write scenarios.toml");

    println!("scenarios.toml written to {}", manifest_path.display());
}

// ─────────────────────────────────────────────────────────────────────────────
// Check mode
// ─────────────────────────────────────────────────────────────────────────────

fn run_check(fixtures_dir: &Path) {
    let tmp = std::env::temp_dir().join(format!("tst-integration-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let messages = drift(fixtures_dir, &tmp);
    if !messages.is_empty() {
        for m in &messages {
            eprintln!("STALE: {m}");
        }
        eprintln!(
            "\nScenario fixtures are stale (the fresh copy is in {}).  Run:\n  cargo run -p tst-integration --bin gen-scenarios\nto regenerate them.",
            tmp.display()
        );
        std::process::exit(1);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    println!(
        "check-scenarios: all {} scenarios up to date.",
        all_scenarios().len()
    );
}
