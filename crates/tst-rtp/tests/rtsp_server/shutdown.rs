//! RtspServer graceful + hard shutdown integration tests. Exercises the
//! registry/cancel path; the RFC 7826 §13.5.1 Notice 5402 wire
//! transmission is covered by `notice_5402.rs`.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspServer, RtspServerError};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

#[test]
fn start_then_stop_clean_exit() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().expect("clean stop after start");
}

#[test]
fn stop_is_idempotent() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    // A second stop() on a shut-down server is a no-op and must return Ok.
    server.stop().expect("second stop is idempotent");
}

#[test]
fn add_mount_after_stop_errors_shutdown() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    let e = server.add_mount("/live", make_muxer_cfg()).unwrap_err();
    assert!(
        matches!(e, RtspServerError::Shutdown),
        "post-stop add_mount must return Shutdown; got {e:?}",
    );
}

#[test]
fn add_multicast_mount_after_stop_errors_shutdown() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    server.stop().unwrap();
    let e = server
        .add_multicast_mount("/mc", make_muxer_cfg(), "rtp://239.0.0.1:5004")
        .unwrap_err();
    assert!(
        matches!(e, RtspServerError::Shutdown),
        "post-stop add_multicast_mount must return Shutdown; got {e:?}",
    );
}

#[test]
fn cancel_handle_flip_observable() {
    // Hard-cancel path: cancel_handle().cancel() flips the public flag.
    // The internal runtime tear-down happens on Drop; this test verifies
    // only the visible cancel-handle observation contract.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let h = server.cancel_handle();
    assert!(!h.is_cancelled());
    h.cancel();
    assert!(h.is_cancelled());
    // Drop after a hard-cancel completes without hanging.
    drop(server);
}

#[test]
fn drop_started_server_with_mount_does_not_leak() {
    // Drop fires the hard-cancel path; shutdown_timeout(5s) bounds the
    // runtime teardown. Validates the mount-held + started path doesn't
    // hang or panic at drop.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let nal = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0xBB];
    mount.push_video(&nal, Pts90khz::new(0), true).unwrap();
    // Mount handle outlives the server momentarily — Drop on RtspServer
    // must not panic even with an outstanding MountHandle reference.
    drop(server);
    drop(mount);
}

#[test]
fn stop_does_not_panic_with_mounts_registered() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _m1 = server.add_mount("/a", make_muxer_cfg()).unwrap();
    let _m2 = server.add_mount("/b", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    server.stop().expect("stop with registered mounts");
}
