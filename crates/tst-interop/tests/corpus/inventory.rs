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
    let d = inv.files[0].duration_s;
    assert!((1.5..=2.5).contains(&d), "duration {d}");
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

/// Rewrites every packet of `bytes` so its PCR-bearing packets carry a PCR
/// base offset by `shift` ticks and sit on `new_pid`; non-PCR packets are dropped.
fn pcr_only_packets(bytes: &[u8], from_pid: u16, new_pid: u16, shift: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for pkt in bytes.chunks_exact(188) {
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if pid != from_pid || pkt[3] & 0x20 == 0 || pkt[4] < 7 || pkt[5] & 0x10 == 0 {
            continue;
        }
        let mut p = pkt.to_vec();
        p[1] = (p[1] & 0xE0) | (new_pid >> 8) as u8;
        p[2] = new_pid as u8;
        let base = (u64::from(p[6]) << 25)
            | (u64::from(p[7]) << 17)
            | (u64::from(p[8]) << 9)
            | (u64::from(p[9]) << 1)
            | u64::from(p[10] >> 7);
        let base = (base + shift) & ((1 << 33) - 1);
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = (p[10] & 0x7F) | ((base & 1) as u8) << 7;
        out.extend_from_slice(&p);
    }
    out
}

#[test]
fn duration_follows_the_declared_pcr_pid_only() {
    let dir = temp_dir_named("inv-two-pcr");
    let bytes = gen_profile_bytes("baseline", 2.0);
    let probe = tst_test_helpers::rawts::parsed(&bytes);
    let pcr_pid = probe.pcr_pid.unwrap();
    // Extra PCR packets on an undeclared PID, on a base 1000 s ahead, appended
    // after the real stream: mixing PIDs would report about 1000 s.
    let mut mixed = bytes.clone();
    mixed.extend(pcr_only_packets(&bytes, pcr_pid, 0x1FF0, 1000 * 90_000));
    std::fs::write(dir.join("two.ts"), &mixed).unwrap();
    let inv = inventory::build(&dir).unwrap();
    let d = inv.files[0].duration_s;
    assert!((1.5..=2.5).contains(&d), "duration {d}");
}
