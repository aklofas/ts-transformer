//! `gen-scenarios --check` compares what it regenerates with the committed
//! fixtures: the goldens, the manifest and the input bytes themselves.

use std::path::{Path, PathBuf};

use tst_integration::scenarios::Scenario;
use tst_integration::scenarios::check::{drift, drift_of};
use tst_integration::scenarios::golden::Golden;

fn scratch_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tst-integration-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// The `input` field of scenario `id`'s row in the copied `scenarios.toml`,
/// relative to the scenario's own directory.
fn input_name_of(fixtures: &Path, id: &str) -> String {
    let text = std::fs::read_to_string(fixtures.join("scenarios.toml")).unwrap();
    let doc: toml::Table = toml::from_str(&text).unwrap();
    let row = doc["scenario"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("no scenario {id}"));
    let input = row["input"].as_str().unwrap();
    input
        .strip_prefix(&format!("{id}/"))
        .unwrap_or_else(|| panic!("input {input} is not under {id}/"))
        .to_string()
}

#[test]
fn a_changed_input_byte_is_drift() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenarios");
    let copy = scratch_dir("scn-check");
    copy_dir(&fixtures, &copy);
    let clean = drift(&copy, &scratch_dir("scn-check-gen"));
    assert!(clean.is_empty(), "{clean:?}");
    let input = copy
        .join("video-roundtrip")
        .join(input_name_of(&copy, "video-roundtrip"));
    let mut b = std::fs::read(&input).unwrap();
    b[200] ^= 0x01; // a byte inside a PSI or stuffing area
    std::fs::write(&input, b).unwrap();
    let d = drift(&copy, &scratch_dir("scn-check-gen2"));
    assert!(
        d.iter()
            .any(|m| m.starts_with("input differs for scenario 'video-roundtrip'")),
        "{d:?}"
    );
    std::fs::remove_dir_all(&copy).unwrap();
}

#[test]
fn a_missing_input_is_drift() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenarios");
    let copy = scratch_dir("scn-check-missing");
    copy_dir(&fixtures, &copy);
    let input = copy
        .join("video-roundtrip")
        .join(input_name_of(&copy, "video-roundtrip"));
    std::fs::remove_file(&input).unwrap();
    let d = drift(&copy, &scratch_dir("scn-check-missing-gen"));
    assert!(
        d.iter()
            .any(|m| m.starts_with("input missing for scenario 'video-roundtrip'")),
        "{d:?}"
    );
    std::fs::remove_dir_all(&copy).unwrap();
}

#[test]
fn an_edited_url_corpus_twin_is_drift() {
    // The JVM leg reads the corpus through golden.json's JSON twin, so a twin
    // that no longer matches the corpus is drift.
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenarios");
    let copy = scratch_dir("scn-check-twin");
    copy_dir(&fixtures, &copy);
    let twin = copy.join("url-corpus/golden.json");
    let text = std::fs::read_to_string(&twin).unwrap();
    let edited = text.replacen("\"srt\"", "\"srtx\"", 1);
    assert_ne!(text, edited);
    std::fs::write(&twin, edited).unwrap();
    let d = drift(&copy, &scratch_dir("scn-check-twin-gen"));
    assert!(
        d.iter()
            .any(|m| m.starts_with("golden differs for scenario 'url-corpus'")),
        "{d:?}"
    );
    std::fs::remove_dir_all(&copy).unwrap();
}

/// A scenario whose generator writes no input file, though its input is not
/// hand-written.
struct Ghost;

impl Scenario for Ghost {
    fn id(&self) -> &'static str {
        "video-roundtrip"
    }
    fn kind(&self) -> &'static str {
        "roundtrip"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }
    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        std::fs::create_dir_all(out_dir.join(self.id())).unwrap();
        let golden = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/scenarios/video-roundtrip/golden.json"),
        )
        .unwrap();
        (
            PathBuf::from(self.id()).join("output.ts"),
            serde_json::from_str(&golden).unwrap(),
        )
    }
}

#[test]
fn a_generator_that_writes_no_input_is_drift() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenarios");
    let copy = scratch_dir("scn-check-ghost");
    copy_dir(&fixtures, &copy);
    let scenarios: Vec<Box<dyn Scenario>> = vec![Box::new(Ghost)];
    let d = drift_of(&scenarios, &copy, &scratch_dir("scn-check-ghost-gen"));
    assert!(
        d.iter()
            .any(|m| m.starts_with("input not generated for scenario 'video-roundtrip'")),
        "{d:?}"
    );
    std::fs::remove_dir_all(&copy).unwrap();
}
