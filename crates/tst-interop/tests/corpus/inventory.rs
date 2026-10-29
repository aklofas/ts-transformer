use tst_interop::corpus::{RunDir, inventory};

use crate::{gen_profile_bytes, pcr_only_packets, temp_dir_named};

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

#[test]
fn a_capture_that_does_not_start_on_a_packet_is_inventoried() {
    let dir = temp_dir_named("inv-shifted");
    let bytes = gen_profile_bytes("baseline", 2.0);
    std::fs::write(dir.join("shifted.ts"), &bytes[100..]).unwrap();
    let inv = inventory::build(&dir).unwrap();
    let f = &inv.files[0];
    assert_eq!(f.probe_error, None);
    assert_eq!(f.shape, "a");
    assert!(
        (1.5..=2.5).contains(&f.duration_s),
        "duration {}",
        f.duration_s
    );
}

fn pid_of(p: &[u8]) -> u16 {
    (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2])
}

/// The PID of the generated capture's PMT and the offset of its first packet.
fn first_pmt(bytes: &[u8]) -> (u16, usize) {
    let pmt = tst_test_helpers::rawts::parsed(bytes).pmt_pid.unwrap();
    let at = (0..bytes.len())
        .step_by(188)
        .find(|&o| pid_of(&bytes[o..]) == pmt)
        .unwrap();
    (pmt, at)
}

/// Every PMT packet of `bytes` rewritten as a section padded past one
/// packet (a 200-byte program descriptor), carried in two packets.
fn with_long_pmt(bytes: &[u8]) -> Vec<u8> {
    let (pmt_pid, _) = first_pmt(bytes);
    let mut cc = 0u8;
    let mut out = Vec::new();
    for pkt in bytes.chunks_exact(188) {
        if pid_of(pkt) != pmt_pid {
            out.extend_from_slice(pkt);
            continue;
        }
        assert_eq!(
            (pkt[3] >> 4) & 3,
            1,
            "a PMT packet without an adaptation field"
        );
        assert_eq!(pkt[4], 0, "a PMT section at the start of the payload");
        let sec = &pkt[5..];
        let len = (usize::from(sec[1] & 0x0F) << 8) | usize::from(sec[2]);
        let sec = &sec[..3 + len];
        let info = (usize::from(sec[10] & 0x0F) << 8) | usize::from(sec[11]);
        let new_info = info + 202;
        let mut long = sec[..10].to_vec();
        long.extend_from_slice(&[0xF0 | (new_info >> 8) as u8, new_info as u8]);
        long.extend_from_slice(&sec[12..12 + info]);
        long.extend_from_slice(&[0xC0, 200]);
        long.extend(std::iter::repeat_n(0u8, 200));
        long.extend_from_slice(&sec[12 + info..sec.len() - 4]);
        let new_len = long.len() - 3 + 4;
        long[1] = (long[1] & 0xF0) | (new_len >> 8) as u8;
        long[2] = new_len as u8;
        let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(&long);
        long.extend_from_slice(&crc.to_be_bytes());
        let mut data = vec![0u8];
        data.extend_from_slice(&long);
        assert!(data.len() > 184, "the section spans two packets");
        for (i, chunk) in data.chunks(184).enumerate() {
            let mut p = vec![0xFF; 188];
            p[..3].copy_from_slice(&pkt[..3]);
            p[1] = (p[1] & !0x40) | if i == 0 { 0x40 } else { 0 };
            p[3] = 0x10 | cc;
            cc = (cc + 1) & 0x0F;
            p[4..4 + chunk.len()].copy_from_slice(chunk);
            out.extend_from_slice(&p);
        }
    }
    out
}

/// The inventory entry of one capture written alone into a fresh directory.
fn probe_one(name: &str, bytes: &[u8]) -> inventory::FileEntry {
    let dir = temp_dir_named(name);
    std::fs::write(dir.join("one.ts"), bytes).unwrap();
    let mut inv = inventory::build(&dir).unwrap();
    inv.files.remove(0)
}

#[test]
fn a_pmt_that_spans_two_packets_is_read() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let plain = probe_one("inv-pmt-plain", &bytes);
    let long = probe_one("inv-pmt-long", &with_long_pmt(&bytes));
    assert_eq!(long.probe_error, None);
    assert_eq!(long.shape, "a");
    assert_eq!(long.streams, plain.streams);
    assert_eq!(long.pcr_pid, plain.pcr_pid);
}

#[test]
fn junk_before_the_pmt_is_resynced_past() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let plain = probe_one("inv-junk-plain", &bytes);
    let (_, at) = first_pmt(&bytes);
    let mut junk = bytes[..at].to_vec();
    junk.extend(std::iter::repeat_n(0u8, 50));
    junk.extend_from_slice(&bytes[at..]);
    let e = probe_one("inv-junk", &junk);
    assert_eq!(e.probe_error, None);
    assert_eq!(e.shape, "a");
    assert_eq!(e.streams, plain.streams);
}

#[test]
fn an_adaptation_field_that_overruns_its_packet_before_the_pmt_is_read_past() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let (_, at) = first_pmt(&bytes);
    let mut bad = [0xFFu8; 188];
    bad[..4].copy_from_slice(&[0x47, 0x1F, 0xF0, 0x30]);
    bad[4] = 200;
    let mut out = bytes[..at].to_vec();
    out.extend_from_slice(&bad);
    out.extend_from_slice(&bytes[at..]);
    let e = probe_one("inv-af", &out);
    assert_eq!(e.probe_error, None);
    assert_eq!(e.shape, "a");
}

#[test]
fn an_empty_corpus_directory_is_an_error() {
    let dir = temp_dir_named("inv-empty");
    std::fs::write(dir.join("notes.txt"), b"not a capture").unwrap();
    let e = inventory::build(&dir).unwrap_err();
    assert!(e.contains("no captures found"), "{e}");
    assert!(!e.contains(&dir.display().to_string()), "{e}");
}

/// A `quarantine` directory, at the top or deeper, is neither walked nor
/// inventoried; each one skipped is counted.
#[test]
fn a_quarantine_directory_is_not_inventoried() {
    let dir = temp_dir_named("inventory-quarantine");
    std::fs::write(dir.join("a.ts"), gen_profile_bytes("baseline", 2.0)).unwrap();
    std::fs::create_dir_all(dir.join("quarantine")).unwrap();
    std::fs::write(
        dir.join("quarantine").join("bad.ts"),
        gen_profile_bytes("klv-sync", 2.0),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("deeper").join("quarantine")).unwrap();
    std::fs::write(
        dir.join("deeper").join("quarantine").join("bad2.ts"),
        gen_profile_bytes("baseline", 3.0),
    )
    .unwrap();
    // The match is on the whole name.
    std::fs::create_dir_all(dir.join("quarantined")).unwrap();
    std::fs::write(
        dir.join("quarantined").join("kept.ts"),
        gen_profile_bytes("baseline", 4.0),
    )
    .unwrap();
    let inv = inventory::build(&dir).unwrap();
    assert_eq!(inv.files.len(), 2, "{inv:?}");
    assert!(
        inv.files
            .iter()
            .flat_map(|f| &f.paths)
            .all(|p| !p.components().any(|c| c.as_os_str() == "quarantine")),
        "{inv:?}"
    );
    assert_eq!(inv.skipped_dirs, 2);
}
