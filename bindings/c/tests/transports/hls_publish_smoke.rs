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
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::common::scaled;
use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::hls::{
    TstPublisher, TstPublisherKind, tst_hls_publisher_builder_bind,
    tst_hls_publisher_builder_build, tst_hls_publisher_builder_new,
    tst_hls_publisher_builder_output_dir, tst_hls_publisher_builder_segment_duration_ms,
    tst_hls_publisher_local_addr, tst_mux_publisher_cut_segment,
    tst_mux_publisher_finish_into_publisher, tst_mux_publisher_send_video,
    tst_mux_publisher_with_config_hls, tst_publisher_cut_segment, tst_publisher_finish,
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

/// An HLS publisher on an ephemeral loopback port writing into `dir`, and
/// the address its HTTP listener is bound to.
fn hls_publisher_on_loopback(dir: &std::path::Path) -> (*mut TstPublisher, SocketAddr) {
    let builder = unsafe { tst_hls_publisher_builder_new() };
    let bind = CString::new("127.0.0.1:0").unwrap();
    assert_eq!(
        unsafe { tst_hls_publisher_builder_bind(builder, bind.as_ptr()) },
        0
    );
    let out_c = CString::new(dir.to_str().expect("utf8 path")).unwrap();
    assert_eq!(
        unsafe { tst_hls_publisher_builder_output_dir(builder, out_c.as_ptr()) },
        0
    );
    let publisher = unsafe { tst_hls_publisher_builder_build(builder) };
    assert!(!publisher.is_null(), "build failed: {}", unsafe {
        tst_get_last_error()
    });
    let mut buf = [0 as std::ffi::c_char; 64];
    // Returns the number of bytes written on success.
    assert!(
        unsafe { tst_hls_publisher_local_addr(publisher, buf.as_mut_ptr(), buf.len()) } > 0,
        "local_addr: {}",
        unsafe { tst_get_last_error() }
    );
    let addr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .parse()
        .expect("local_addr parses");
    (publisher, addr)
}

/// Polls until a connect to `addr` is refused (the listener is gone);
/// `false` once `bound` has passed first.
fn listener_closes_within(addr: SocketAddr, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return true,
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    false
}

/// The header promises `hls` is consumed by every call: a rejected config
/// must still release the publisher (its HTTP listener closes).
#[test]
fn with_config_hls_rejected_config_still_consumes_the_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let (publisher, addr) = hls_publisher_on_loopback(dir.path());
    let empty = unsafe { tst_mux_config_new() }; // no program: build_config fails
    let mp = unsafe { tst_mux_publisher_with_config_hls(publisher, empty) };
    assert!(mp.is_null());
    assert_ne!(unsafe { tst_get_last_error() }, 0);
    unsafe { tst_mux_config_free(empty) };
    assert!(
        listener_closes_within(addr, scaled(Duration::from_secs(5))),
        "publisher still serving on {addr} after a rejected config: it was not consumed"
    );
}

#[test]
fn with_config_hls_null_config_still_consumes_the_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let (publisher, addr) = hls_publisher_on_loopback(dir.path());
    let mp = unsafe { tst_mux_publisher_with_config_hls(publisher, std::ptr::null()) };
    assert!(mp.is_null());
    assert_eq!(
        unsafe { tst_get_last_error() },
        TstError::InvalidConfig as i32
    );
    assert!(
        listener_closes_within(addr, scaled(Duration::from_secs(5))),
        "publisher still serving on {addr} after a null config: it was not consumed"
    );
}

/// The success path is unchanged.
#[test]
fn with_config_hls_valid_config_still_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let (publisher, _addr) = hls_publisher_on_loopback(dir.path());
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x100, TstVideoCodec::H264 as i32) };
    let mp = unsafe { tst_mux_publisher_with_config_hls(publisher, cfg) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!mp.is_null(), "with_config failed: {}", unsafe {
        tst_get_last_error()
    });

    // SPS, PPS, IDR slice.
    let au: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0xDA, 0x05, 0x07, 0xE8, 0, 0, 0, 1, 0x68, 0xCE, 0x3C,
        0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10,
    ];
    assert_eq!(
        unsafe { tst_mux_publisher_send_video(mp, au.as_ptr(), au.len(), 90_000, true) },
        0,
        "send_video: {}",
        unsafe { tst_get_last_error() }
    );
    assert_eq!(unsafe { tst_mux_publisher_cut_segment(mp) }, 0);
    let back = unsafe { tst_mux_publisher_finish_into_publisher(mp) };
    assert!(!back.is_null());
    unsafe { tst_publisher_free(back) };

    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.iter().any(|n| n.ends_with(".ts")), "{entries:?}");
    assert!(entries.iter().any(|n| n.ends_with(".m3u8")), "{entries:?}");
}
