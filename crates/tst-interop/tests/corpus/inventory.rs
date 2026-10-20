use tst_interop::corpus::{RunDir, inventory};

use crate::{gen_profile_bytes, temp_dir_named};

#[test]
fn inventory_dedupes_identical_content_under_two_names() {
    let dir = temp_dir_named("inv-dedupe");
    let bytes = gen_profile_bytes("baseline", 2.0);
    std::fs::write(dir.join("one.ts"), &bytes).unwrap();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("two.ts"), &bytes).unwrap();
    let inv = inventory::build(&dir).unwrap();
    assert_eq!(inv.files.len(), 1);
    assert_eq!(inv.files[0].paths.len(), 2);
    assert_eq!(inv.files[0].shape, "a");
    assert_eq!(inv.files[0].klv_sets, ["st0601"]);
    assert_eq!(inv.files[0].codecs, ["h264"]);
    assert!(inv.files[0].duration_s > 0.0);
    assert!(inv.files[0].largest_au_bytes > 0);
    assert_eq!(inv.files[0].bytes, bytes.len() as u64);
}

#[test]
fn a_file_without_a_pmt_gets_an_entry_with_a_probe_error() {
    let dir = temp_dir_named("inv-nopmt");
    std::fs::write(dir.join("junk.ts"), [0x47, 0x1F, 0xFF, 0x10].repeat(47)).unwrap();
    let inv = inventory::build(&dir).unwrap();
    assert_eq!(inv.files.len(), 1);
    assert!(
        inv.files[0]
            .probe_error
            .as_deref()
            .unwrap_or("")
            .contains("PMT")
    );
}

#[test]
fn inventory_round_trips_through_the_run_dir() {
    let dir = temp_dir_named("inv-roundtrip");
    let bytes = gen_profile_bytes("baseline", 1.0);
    std::fs::write(dir.join("a.ts"), &bytes).unwrap();
    let inv = inventory::build(&dir).unwrap();
    let run = RunDir {
        root: temp_dir_named("inv-roundtrip-run"),
    };
    inventory::write(&inv, &run).unwrap();
    let back = inventory::read(&run).unwrap();
    let shas =
        |i: &inventory::Inventory| i.files.iter().map(|f| f.sha256.clone()).collect::<Vec<_>>();
    assert_eq!(shas(&inv), shas(&back));
    assert_eq!(back.schema_version, 1);
}
