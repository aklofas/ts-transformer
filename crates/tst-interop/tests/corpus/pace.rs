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
