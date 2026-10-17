//! Smoke test for the HLS publisher C ABI surface.
//!
//! Builds an HLS publisher on an ephemeral loopback port writing to a
//! `tempfile::tempdir()`, pushes ~100 NULL MPEG-TS packets through the
//! universal `tst_publisher_push_ts`, cuts a segment, finishes, and
//! asserts that a `.ts` segment + the `.m3u8` playlist landed on disk.
//!
//! Gated on `feature = "hls"` so it compiles only in builds that include
//! the HLS publisher. The lib name for `tst-c` is `tstrans` (see
//! `[lib] name` in Cargo.toml), so this references it as `tstrans`.
#![cfg(feature = "hls")]

use std::ffi::CString;

use tstrans::error::{TstError, tst_get_last_error};
use tstrans::hls::{
    TstPublisherKind, tst_hls_publisher_builder_bind, tst_hls_publisher_builder_build,
    tst_hls_publisher_builder_new, tst_hls_publisher_builder_output_dir,
    tst_hls_publisher_builder_segment_duration_ms, tst_publisher_cut_segment, tst_publisher_finish,
    tst_publisher_free, tst_publisher_get_kind, tst_publisher_push_ts,
};

const TS_PACKET_SIZE: usize = 188;

/// One MPEG-TS null packet (PID 0x1FFF, payload-only). Aligned to 188.
fn null_ts_packet() -> [u8; TS_PACKET_SIZE] {
    let mut p = [0xFFu8; TS_PACKET_SIZE];
    p[0] = 0x47; // sync byte
    p[1] = 0x1F; // null PID high 5 bits
    p[2] = 0xFF; // null PID low 8 bits
    p[3] = 0x10; // adaptation_field_control=01 (payload only)
    p
}

#[test]
fn hls_publish_round_trip() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let out_dir = dir.path().to_str().expect("tempdir path is utf8");

    // ── Build the publisher ────────────────────────────────────────────
    let builder = unsafe { tst_hls_publisher_builder_new() };
    assert!(!builder.is_null(), "builder_new returned null");

    let bind = CString::new("127.0.0.1:0").unwrap();
    assert_eq!(
        unsafe { tst_hls_publisher_builder_bind(builder, bind.as_ptr()) },
        0,
        "bind failed: {}",
        unsafe { tst_get_last_error() }
    );

    let out_c = CString::new(out_dir).unwrap();
    assert_eq!(
        unsafe { tst_hls_publisher_builder_output_dir(builder, out_c.as_ptr()) },
        0
    );

    assert_eq!(
        unsafe { tst_hls_publisher_builder_segment_duration_ms(builder, 1000) },
        0
    );

    let publisher = unsafe { tst_hls_publisher_builder_build(builder) };
    assert!(
        !publisher.is_null(),
        "build returned null: code {}",
        unsafe { tst_get_last_error() }
    );

    // Kind discriminator.
    assert_eq!(
        unsafe { tst_publisher_get_kind(publisher) },
        TstPublisherKind::Hls as u32
    );

    // ── Push ~100 TS packets ───────────────────────────────────────────
    let pkt = null_ts_packet();
    for i in 0..100 {
        let rc = unsafe { tst_publisher_push_ts(publisher, pkt.as_ptr(), pkt.len()) };
        assert_eq!(rc, 0, "push_ts[{i}] failed: code {}", unsafe {
            tst_get_last_error()
        });
    }

    // ── Cut a segment, then finish ─────────────────────────────────────
    assert_eq!(unsafe { tst_publisher_cut_segment(publisher) }, 0);
    assert_eq!(
        unsafe { tst_publisher_finish(publisher) },
        0,
        "finish failed: code {}",
        unsafe { tst_get_last_error() }
    );

    // After finish, push must report HLS_FINISHED.
    let rc = unsafe { tst_publisher_push_ts(publisher, pkt.as_ptr(), pkt.len()) };
    assert_eq!(rc, TstError::HlsFinished as i32);

    // ── Assert on-disk artifacts ───────────────────────────────────────
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    assert!(
        entries.iter().any(|n| n.ends_with(".ts")),
        "expected a .ts segment in {entries:?}"
    );
    assert!(
        entries.iter().any(|n| n.ends_with(".m3u8")),
        "expected an .m3u8 playlist in {entries:?}"
    );

    // ── Free ───────────────────────────────────────────────────────────
    unsafe { tst_publisher_free(publisher) };
}

/// A `tst_publisher_push_ts` racing `tst_publisher_finish` from another
/// thread must be serialized by the handle lock: every push either lands
/// or reports the handle finished, and the loop always ends on the
/// finished code. Before the handle took its lock both entry points
/// aliased a `&mut` to the same slot, which ThreadSanitizer reports as a
/// data race (`tsan-native` job).
#[test]
fn push_ts_racing_finish_is_serialized() {
    use std::sync::{Arc, Barrier};

    let dir = tempfile::tempdir().expect("create tempdir");
    let out_c = CString::new(dir.path().to_str().expect("utf8 path")).unwrap();
    let bind = CString::new("127.0.0.1:0").unwrap();
    let builder = unsafe { tst_hls_publisher_builder_new() };
    assert_eq!(
        unsafe { tst_hls_publisher_builder_bind(builder, bind.as_ptr()) },
        0
    );
    assert_eq!(
        unsafe { tst_hls_publisher_builder_output_dir(builder, out_c.as_ptr()) },
        0
    );
    let publisher = unsafe { tst_hls_publisher_builder_build(builder) };
    assert!(!publisher.is_null());

    // Raw pointers are not `Send`; the address crosses the thread boundary.
    let addr = publisher as usize;
    let barrier = Arc::new(Barrier::new(2));

    let pusher = {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let p = addr as *mut _;
            let pkt = null_ts_packet();
            barrier.wait();
            // Push until the handle reports the finish; bounded so a
            // regression that never reports it fails instead of hanging.
            for _ in 0..1_000_000 {
                let rc = unsafe { tst_publisher_push_ts(p, pkt.as_ptr(), pkt.len()) };
                if rc != 0 {
                    return rc;
                }
            }
            0
        })
    };

    barrier.wait();
    // Let some pushes land before the finish arrives.
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(unsafe { tst_publisher_finish(publisher) }, 0);

    let last = pusher.join().expect("pusher thread");
    assert_eq!(
        last,
        TstError::HlsFinished as i32,
        "the push after the finish reports the handle finished"
    );
    // A second finish takes nothing out of the empty slot.
    assert_eq!(
        unsafe { tst_publisher_finish(publisher) },
        TstError::HlsFinished as i32
    );
    unsafe { tst_publisher_free(publisher) };
}
