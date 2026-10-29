//! What `gen-scenarios --check` compares: every scenario regenerated into a
//! scratch directory against the committed fixtures, by golden, by manifest
//! and by input bytes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::golden::Golden;
use super::{Scenario, all_scenarios};

/// `scenarios.toml`: one row per scenario.
#[derive(Serialize, Deserialize)]
pub struct ScenariosManifest {
    pub scenario: Vec<ScenarioEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct ScenarioEntry {
    pub id: String,
    pub kind: String,
    pub input: String,
    pub golden: String,
    pub features: Vec<String>,
    pub tier: String,
    pub schema_version: u32,
}

impl ScenarioEntry {
    /// The manifest row of a scenario whose generator returned `input_rel`
    /// and `golden`.
    pub fn new(scenario: &dyn Scenario, input_rel: &Path, golden: &Golden) -> Self {
        ScenarioEntry {
            id: scenario.id().to_string(),
            kind: scenario.kind().to_string(),
            input: input_rel.to_string_lossy().replace('\\', "/"),
            golden: golden_rel(scenario).to_string_lossy().replace('\\', "/"),
            features: scenario.features().iter().map(|s| s.to_string()).collect(),
            tier: scenario.tier().to_string(),
            schema_version: golden.schema_version,
        }
    }
}

/// Where a scenario's golden lives, relative to the fixtures directory.
pub fn golden_rel(scenario: &dyn Scenario) -> PathBuf {
    PathBuf::from(scenario.id()).join("golden.json")
}

/// Regenerates every scenario into `scratch` and returns one message per
/// difference from `fixtures_dir`: a stale or missing golden, a stale or
/// missing manifest, and an input whose bytes differ from the committed file.
/// Empty means the committed fixtures are what the generators produce.
pub fn drift(fixtures_dir: &Path, scratch: &Path) -> Vec<String> {
    std::fs::create_dir_all(scratch).expect("create scratch dir");
    let mut out = Vec::new();
    let mut fresh_entries = Vec::new();

    for scenario in &all_scenarios() {
        let (input_rel, golden) = scenario.generate(scratch);
        let id = scenario.id();

        // Goldens compare as parsed values, so whitespace is not drift. The
        // URL corpus's golden is the JSON twin the JVM leg reads.
        let golden_rel = golden_rel(scenario.as_ref());
        let committed_golden = fixtures_dir.join(&golden_rel);
        match std::fs::read_to_string(&committed_golden) {
            Ok(text) => match serde_json::from_str::<Golden>(&text) {
                Ok(committed) if committed == golden => {}
                Ok(_) => out.push(format!(
                    "golden differs for scenario '{id}': committed {}, generated {}",
                    committed_golden.display(),
                    scratch.join(&golden_rel).display()
                )),
                Err(e) => out.push(format!(
                    "golden unreadable for scenario '{id}': {}: {e}",
                    committed_golden.display()
                )),
            },
            Err(_) => out.push(format!(
                "golden missing for scenario '{id}': {}",
                committed_golden.display()
            )),
        }
        // The goldens describe what the inputs decode to; two inputs that
        // decode alike can still differ in a stuffing or PSI byte, so the
        // bytes are compared too.
        let committed_input = fixtures_dir.join(&input_rel);
        match std::fs::read(&committed_input) {
            Err(_) => out.push(format!(
                "input missing for scenario '{id}': {}",
                committed_input.display()
            )),
            // A scenario whose input is hand-written (the URL corpus) writes
            // none: its committed file is the source, and the golden above
            // is what is generated from it.
            Ok(committed) => {
                if let Ok(generated) = std::fs::read(scratch.join(&input_rel)) {
                    let (c, g) = (sha256_hex(&committed), sha256_hex(&generated));
                    if c != g {
                        out.push(format!(
                            "input differs for scenario '{id}': committed {c}, generated {g}"
                        ));
                    }
                }
            }
        }
        fresh_entries.push(ScenarioEntry::new(scenario.as_ref(), &input_rel, &golden));
    }

    // The manifest compares as re-serialised TOML, so formatting is not drift.
    let manifest = fixtures_dir.join("scenarios.toml");
    let fresh = toml::to_string(&ScenariosManifest {
        scenario: fresh_entries,
    })
    .expect("serialise manifest");
    match std::fs::read_to_string(&manifest) {
        Ok(text) => match toml::from_str::<ScenariosManifest>(&text) {
            Ok(committed) => {
                let committed = toml::to_string(&committed).expect("re-serialise manifest");
                if committed != fresh {
                    out.push(format!("manifest differs: {}", manifest.display()));
                }
            }
            Err(e) => out.push(format!("manifest unreadable: {}: {e}", manifest.display())),
        },
        Err(_) => out.push(format!("manifest missing: {}", manifest.display())),
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
        s
    })
}
