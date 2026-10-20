use std::cell::{Cell, RefCell};
use std::time::Duration;

use tst_interop::corpus::pace::{PcrPacer, send_paced};

use crate::gen_profile_bytes;

#[test]
fn chunks_cover_the_whole_file_exactly_once_in_order() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let mut joined = Vec::new();
    let mut last = -1.0;
    for c in pacer.chunks() {
        assert!(c.offset_s >= last, "offsets never go backwards");
        last = c.offset_s;
        joined.extend_from_slice(c.bytes);
    }
    assert_eq!(joined, bytes);
    assert!(pacer.total_seconds() > 1.0);
}

#[test]
fn a_prefix_cut_stops_at_the_first_pcr_past_the_prefix() {
    let bytes = gen_profile_bytes("baseline", 4.0);
    let pacer = PcrPacer::new(&bytes, Some(1)).unwrap();
    let end = tst_interop::corpus::pcr::prefix_end(&bytes, 1);
    let mut joined = Vec::new();
    for c in pacer.chunks() {
        joined.extend_from_slice(c.bytes);
    }
    assert_eq!(joined, bytes[..end]);
    assert!(pacer.total_seconds() <= 1.0);
    assert!(pacer.total_seconds() > 0.5);
}

#[test]
fn send_paced_sleeps_until_each_chunks_offset_under_a_fake_clock() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let now = Cell::new(Duration::from_secs(1000));
    let sleeps = RefCell::new(Vec::new());
    let writes = RefCell::new(Vec::new());
    send_paced(
        &pacer,
        &mut || now.get(),
        &mut |d| {
            sleeps.borrow_mut().push(d);
            now.set(now.get() + d);
        },
        |b| {
            writes.borrow_mut().push((now.get(), b.len()));
            Ok(())
        },
    )
    .unwrap();
    let total: Duration = sleeps.borrow().iter().sum();
    let last_offset = pacer.chunks().last().unwrap().offset_s;
    assert!((total.as_secs_f64() - last_offset).abs() < 1e-6);
    assert!(sleeps.borrow().iter().all(|d| !d.is_zero()));
    // one write per chunk, none before its offset
    let chunks: Vec<_> = pacer.chunks().collect();
    assert_eq!(writes.borrow().len(), chunks.len());
    for ((at, len), c) in writes.borrow().iter().zip(&chunks) {
        assert_eq!(*len, c.bytes.len());
        assert!((*at - Duration::from_secs(1000)).as_secs_f64() >= c.offset_s - 1e-6);
    }
}

#[test]
fn a_write_error_stops_the_send() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let n = Cell::new(0);
    let r = send_paced(&pacer, &mut || Duration::ZERO, &mut |_| {}, |_| {
        n.set(n.get() + 1);
        Err("boom".into())
    });
    assert_eq!(r, Err("boom".to_string()));
    assert_eq!(n.get(), 1);
}

#[test]
fn a_stream_without_pcr_is_unpaceable() {
    let mut pkt = vec![0u8; 188 * 10];
    for p in pkt.chunks_mut(188) {
        p[0] = 0x47;
        p[1] = 0x1f;
        p[2] = 0xff;
        p[3] = 0x10;
    }
    let e = PcrPacer::new(&pkt, None).err().unwrap();
    assert!(e.starts_with("unpaceable"), "{e}");
}

/// A PAT and PMT (taken from a generated capture) declaring `pcr_pid`, then
/// one adaptation-field-only packet per `(pid, pcr_base)`.
fn hand_built(pcr_marks: &[(u16, u64)]) -> (Vec<u8>, u16) {
    let src = gen_profile_bytes("baseline", 1.0);
    let probe = tst_test_helpers::rawts::parsed(&src);
    let pcr_pid = probe.pcr_pid.unwrap();
    let mut out = Vec::new();
    // The generator opens with the PAT then the PMT.
    let pid_of = |k: usize| (u16::from(src[k * 188 + 1] & 0x1F) << 8) | u16::from(src[k * 188 + 2]);
    assert_eq!(pid_of(0), 0, "first packet is the PAT");
    assert_eq!(probe.pmt_pid, Some(pid_of(1)), "second packet is the PMT");
    out.extend_from_slice(&src[..188 * 2]);
    for (i, (pid, base)) in pcr_marks.iter().enumerate() {
        let mut p = [0xFFu8; 188];
        p[0] = 0x47;
        p[1] = (pid >> 8) as u8 & 0x1F;
        p[2] = *pid as u8;
        p[3] = 0x20 | (i as u8 & 0x0F);
        p[4] = 183;
        p[5] = 0x10;
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) as u8) << 7 | 0x7E;
        p[11] = 0;
        out.extend_from_slice(&p);
    }
    (out, pcr_pid)
}

fn joined(pacer: &PcrPacer) -> Vec<u8> {
    pacer.chunks().flat_map(|c| c.bytes.to_vec()).collect()
}

#[test]
fn a_pcr_reset_and_a_33_bit_wrap_still_cover_the_stream_once() {
    let wrap = (1u64 << 33) - 90_000;
    let (probe, pid) = hand_built(&[]);
    let marks: Vec<(u16, u64)> = [0, 90_000, 180_000, 100_000, 190_000, wrap, wrap + 90_000]
        .iter()
        .map(|b| (pid, b & ((1u64 << 33) - 1)))
        .collect();
    let (bytes, _) = hand_built(&marks);
    assert!(bytes.len() > probe.len());
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    assert_eq!(joined(&pacer), bytes);
    // Steps: 3 x 90 000, a reset (0), the wrap-bound step, then 90 000 across the wrap.
    let ticks = (1u64 << 33) + 80_000;
    assert!((pacer.total_seconds() - ticks as f64 / 90_000.0).abs() < 1e-3);
}

#[test]
fn a_misaligned_start_paces_and_starts_at_the_first_full_packet() {
    let bytes = gen_profile_bytes("baseline", 2.0);
    let pacer = PcrPacer::new(&bytes[100..], None).unwrap();
    // The 88 bytes before the first full packet are never sent.
    assert_eq!(joined(&pacer), bytes[188..]);
    assert!(pacer.total_seconds() > 1.0);
}

#[test]
fn pcrs_on_two_pids_pace_on_the_declared_one() {
    let (_, pid) = hand_built(&[]);
    let other = 0x1FF0;
    let marks = [
        (other, 1_000_000),
        (pid, 0),
        (other, 5_000_000),
        (pid, 90_000),
        (other, 9_000_000),
        (pid, 180_000),
    ];
    let (bytes, _) = hand_built(&marks);
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    assert!(
        (pacer.total_seconds() - 2.0).abs() < 1e-6,
        "{}",
        pacer.total_seconds()
    );
}

#[test]
fn a_single_pcr_stream_is_unpaceable() {
    let (_, pid) = hand_built(&[]);
    let (bytes, _) = hand_built(&[(pid, 0)]);
    let e = PcrPacer::new(&bytes, None).err().unwrap();
    assert!(e.starts_with("unpaceable"), "{e}");
}
