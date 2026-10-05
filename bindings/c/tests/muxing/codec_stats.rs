//! Integration coverage for the `tst_*_get_stream_codec_stats` family
//! (5 C entry points). Exercises null-pointer error paths, the
//! pid-never-observed (`TST_E_NOT_FOUND`) path, happy-path Video on
//! the local `tst_muxer_t`, and end-to-end Video-PID happy-path +
//! NotFound-for-PSI through a loopback `tst_mux_sender_t` ↔
//! `tst_demux_receiver_t` pair.
//!
//! Note on the PSI-PID assertion: the underlying
//! `Demuxer::stream_codec_stats` only returns `Some(Unknown)` for PIDs
//! that appear in `stats_per_stream`, which is populated exclusively
//! from elementary-stream events (PES via `lookup_stream`). PSI PIDs
//! (PAT 0x0000, PMT) never enter `stats_per_stream`, so the entry
//! point surfaces `TST_E_NOT_FOUND` (-14), NOT `TST_E_OK` + kind=UNKNOWN.
//! The loopback test asserts that actual behavior.
//!
//! Entry points covered:
//! * `tst_muxer_get_stream_codec_stats`                      (muxer.rs)
//! * `tst_mux_sender_get_stream_codec_stats`                 (mux_sender.rs)
//! * `tst_managed_mux_sender_get_stream_codec_stats`         (mux_sender.rs)
//! * `tst_demux_receiver_get_stream_codec_stats`             (demux_receiver.rs)
//! * `tst_managed_demux_receiver_get_stream_codec_stats`     (demux_receiver.rs)

// The null-pointer error-path tests at the top need only the unconditional
// `tstrans::muxer::*`, but the loopback/happy-path tests below pull in
// `tstrans::sender::*` / `tstrans::receiver::*`, both gated behind
// `feature = "srt"`. The whole file is gated at the binary level so
// `cargo test --workspace --no-default-features` compiles; the muxing test
// binary links SRT-dependent code and defaults to `srt` anyway. (It is no
// longer true that *every* test here requires sender/receiver — splitting the
// offline cases out from the gate was out of scope for this relocation.)
#![cfg(feature = "srt")]

use std::ptr;

use tstrans::error::TstError;
use tstrans::stats::{TST_CODEC_KIND_UNKNOWN, TST_CODEC_KIND_VIDEO, TstStreamCodecStats};

// --- Null-pointer error paths (no live handle needed) -----------------------

#[test]
fn muxer_get_stream_codec_stats_null_handle_returns_invalid_config() {
    let mut out = TstStreamCodecStats {
        kind: 0,
        _pad: 0,
        u: unsafe { std::mem::zeroed() },
    };
    let rc = unsafe {
        tstrans::muxer::tst_muxer_get_stream_codec_stats(ptr::null_mut(), 0x100, &mut out)
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn mux_sender_get_stream_codec_stats_null_handle_returns_invalid_config() {
    let mut out = TstStreamCodecStats {
        kind: 0,
        _pad: 0,
        u: unsafe { std::mem::zeroed() },
    };
    let rc = unsafe {
        tstrans::sender::mux_sender::tst_mux_sender_get_stream_codec_stats(
            ptr::null_mut(),
            0x100,
            &mut out,
        )
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn managed_mux_sender_get_stream_codec_stats_null_handle_returns_invalid_config() {
    let mut out = TstStreamCodecStats {
        kind: 0,
        _pad: 0,
        u: unsafe { std::mem::zeroed() },
    };
    let rc = unsafe {
        tstrans::sender::mux_sender::tst_managed_mux_sender_get_stream_codec_stats(
            ptr::null_mut(),
            0x100,
            &mut out,
        )
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn demux_receiver_get_stream_codec_stats_null_handle_returns_invalid_config() {
    let mut out = TstStreamCodecStats {
        kind: 0,
        _pad: 0,
        u: unsafe { std::mem::zeroed() },
    };
    let rc = unsafe {
        tstrans::receiver::demux_receiver::tst_demux_receiver_get_stream_codec_stats(
            ptr::null_mut(),
            0x100,
            &mut out,
        )
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

#[test]
fn managed_demux_receiver_get_stream_codec_stats_null_handle_returns_invalid_config() {
    let mut out = TstStreamCodecStats {
        kind: 0,
        _pad: 0,
        u: unsafe { std::mem::zeroed() },
    };
    let rc = unsafe {
        tstrans::receiver::demux_receiver::tst_managed_demux_receiver_get_stream_codec_stats(
            ptr::null_mut(),
            0x100,
            &mut out,
        )
    };
    assert_eq!(rc, TstError::InvalidConfig as i32);
}

// --- Live muxer happy-path + null-out + not-found ---------------------------

#[test]
fn muxer_get_stream_codec_stats_null_out_returns_invalid_config() {
    use tstrans::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::muxer::{tst_muxer_close, tst_muxer_open};
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x0100, TstVideoCodec::H264);
        let m = tst_muxer_open(cfg);
        assert!(!m.is_null());

        let rc = tstrans::muxer::tst_muxer_get_stream_codec_stats(m, 0x0100, ptr::null_mut());
        assert_eq!(rc, TstError::InvalidConfig as i32);

        tst_muxer_close(m);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn muxer_get_stream_codec_stats_unconfigured_pid_returns_not_found() {
    use tstrans::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::muxer::{tst_muxer_close, tst_muxer_open};
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x0100, TstVideoCodec::H264);
        let m = tst_muxer_open(cfg);
        assert!(!m.is_null());

        // PID 0x9999 was never configured on this muxer; the underlying
        // `Muxer::stream_codec_stats` returns None and the entry point
        // surfaces TST_E_NOT_FOUND (-14).
        let mut out = TstStreamCodecStats {
            kind: 0,
            _pad: 0,
            u: std::mem::zeroed(),
        };
        let rc = tstrans::muxer::tst_muxer_get_stream_codec_stats(m, 0x9999, &mut out);
        assert_eq!(rc, TstError::NotFound as i32);

        tst_muxer_close(m);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn muxer_get_stream_codec_stats_after_push_video_returns_video_variant() {
    use tstrans::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::muxer::{tst_muxer_close, tst_muxer_open, tst_muxer_push_video};
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x0100, TstVideoCodec::H264);
        let m = tst_muxer_open(cfg);
        assert!(!m.is_null());

        // Pre-push: configured PID returns Some(Unknown) — counters not yet
        // materialized (Muxer's accessor falls back to per_stream.contains_key).
        let mut out = TstStreamCodecStats {
            kind: 0xFFFF,
            _pad: 0,
            u: std::mem::zeroed(),
        };
        let rc = tstrans::muxer::tst_muxer_get_stream_codec_stats(m, 0x0100, &mut out);
        assert_eq!(rc, 0);
        assert_eq!(out.kind, TST_CODEC_KIND_UNKNOWN);

        // Push one minimal H.264 AU: AUD (nal_type=9) + IDR (nal_type=5).
        // Same byte shape as tst-core's build_minimal_h264_au helper.
        let nal: [u8; 14] = [
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC,
        ];
        let rc = tst_muxer_push_video(m, nal.as_ptr(), nal.len(), 0, /*key_frame=*/ true);
        assert_eq!(rc, 0);

        // Post-push: Video variant materialized; nals_or_obus + random_access_aus
        // both > 0 (push_video with key_frame=true bumps the RA counter).
        let mut out = TstStreamCodecStats {
            kind: 0,
            _pad: 0,
            u: std::mem::zeroed(),
        };
        let rc = tstrans::muxer::tst_muxer_get_stream_codec_stats(m, 0x0100, &mut out);
        assert_eq!(rc, 0);
        assert_eq!(out.kind, TST_CODEC_KIND_VIDEO);
        assert!(
            out.u.video.nals_or_obus > 0,
            "nals_or_obus={}",
            out.u.video.nals_or_obus
        );
        assert!(
            out.u.video.random_access_aus > 0,
            "random_access_aus={}",
            out.u.video.random_access_aus
        );

        tst_muxer_close(m);
        tst_mux_config_free(cfg);
    }
}

// --- Loopback: tst_mux_sender_t → tst_demux_receiver_t ---------------------
//
// Mirrors bindings/c/tests/receiving/demux_receiver_loopback.rs threading
// shape. The sender thread builds a single-program H.264 mux config,
// sends a few NAL bursts, then closes. The receiver thread drains events
// until EOS and then queries codec stats:
//   * video PID 0x1011 → kind=VIDEO with nals_or_obus > 0
//   * PAT PID 0x0000  → TST_E_NOT_FOUND (PSI PIDs never enter
//     `stats_per_stream`, see module-level note)
//
// This single loopback test covers BOTH the `tst_mux_sender_*` happy-path
// AND the `tst_demux_receiver_*` not-found-for-PSI scenario in one fixture.

#[test]
#[cfg(target_os = "linux")]
fn loopback_mux_sender_to_demux_receiver_codec_stats_video_and_psi_not_found() {
    use std::ffi::{CStr, CString};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tstrans::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::error::tst_get_last_error_str;
    use tstrans::event::TstEvent;
    use tstrans::receiver::demux_receiver::{
        tst_demux_receiver_close, tst_demux_receiver_open_listener, tst_demux_receiver_recv_event,
    };
    use tstrans::sender::mux_sender::{
        tst_mux_sender_close, tst_mux_sender_open, tst_mux_sender_send_video,
    };

    fn last_error_msg() -> String {
        unsafe {
            let p = tst_get_last_error_str();
            if p.is_null() {
                return "<null>".into();
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    // Same port-selection idiom as the existing loopback tests: ephemeral
    // range, process-id-keyed offset to limit collisions when tests run
    // concurrently.
    let port: u16 = 29_200 + (std::process::id() as u16 % 500);

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    // Push the receiver-side codec-stats results back to the main thread.
    let (stats_tx, stats_rx) =
        mpsc::channel::<(TstStreamCodecStats, TstStreamCodecStats, i32, i32)>();

    let receiver_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        let rx = unsafe { tst_demux_receiver_open_listener(url.as_ptr()) };
        if rx.is_null() {
            let msg = last_error_msg();
            panic!("tst_demux_receiver_open_listener failed: {msg}");
        }
        ready_tx.send(()).expect("ready channel dropped");

        // Drain events until EOS so the demuxer's per-PID stats fully populate.
        let mut ev = TstEvent::default();
        loop {
            let rc = unsafe { tst_demux_receiver_recv_event(rx, &mut ev) };
            if rc == 0 {
                continue;
            }
            if rc == TstError::EndOfStream as i32 {
                break;
            }
            panic!("recv_event failed (rc={rc}): {}", last_error_msg());
        }

        // Video PID 0x1011 — should have a Video variant with nals_or_obus > 0.
        let mut video_out = TstStreamCodecStats {
            kind: 0xFFFF,
            _pad: 0,
            u: unsafe { std::mem::zeroed() },
        };
        let rc_video = unsafe {
            tstrans::receiver::demux_receiver::tst_demux_receiver_get_stream_codec_stats(
                rx,
                0x1011,
                &mut video_out,
            )
        };

        // PAT PID 0x0000 — PSI PIDs are NOT recorded in `stats_per_stream`
        // (only elementary-stream events from PES surface there), so the
        // accessor returns None → TST_E_NOT_FOUND. See module docstring.
        let mut pat_out = TstStreamCodecStats {
            kind: 0xFFFF,
            _pad: 0,
            u: unsafe { std::mem::zeroed() },
        };
        let rc_pat = unsafe {
            tstrans::receiver::demux_receiver::tst_demux_receiver_get_stream_codec_stats(
                rx,
                0x0000,
                &mut pat_out,
            )
        };

        unsafe { tst_demux_receiver_close(rx) };

        stats_tx
            .send((video_out, pat_out, rc_video, rc_pat))
            .expect("stats channel dropped");
    });

    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

        let cfg = unsafe { tst_mux_config_new() };
        let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x0100) };
        let _video =
            unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

        // Retry-connect loop matching demux_receiver_loopback.rs's pattern —
        // the listener may not be bound yet when this thread starts.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let tx = loop {
            let h = unsafe { tst_mux_sender_open(url.as_ptr(), cfg) };
            if !h.is_null() {
                break h;
            }
            if std::time::Instant::now() > deadline {
                unsafe { tst_mux_config_free(cfg) };
                panic!(
                    "tst_mux_sender_open timed out after 5s: {}",
                    last_error_msg()
                );
            }
            thread::sleep(Duration::from_millis(50));
        };

        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver did not signal ready within 5s");

        // Synthetic H.264 AU bytes — see demux_receiver_loopback.rs for the
        // rationale on shape (start codes + nal_type=7 SPS + nal_type=5 IDR).
        let nal: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, // Annex-B start code
            0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, // SPS-shaped
            0x00, 0x00, 0x00, 0x01, // Annex-B start code
            0x65, 0x88, 0x80, 0x40, // IDR-shaped
        ];

        for i in 0..5 {
            let pts = (i as i64) * 3_600;
            let rc = unsafe {
                tst_mux_sender_send_video(tx, nal.as_ptr(), nal.len(), pts, /*key=*/ true)
            };
            assert_eq!(rc, 0, "send_video[{i}] rc={rc}: {}", last_error_msg());
        }

        // Drain pause — same 1 s window as demux_receiver_loopback.rs (covers
        // SRT's 120 ms default latency + Apple-silicon scheduling jitter).
        thread::sleep(Duration::from_secs(1));

        unsafe { tst_mux_sender_close(tx) };
        unsafe { tst_mux_config_free(cfg) };
    });

    sender_thread.join().expect("sender thread panicked");
    let (video_out, _pat_out, rc_video, rc_pat) = stats_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stats channel timeout");
    receiver_thread.join().expect("receiver thread panicked");

    // Video PID assertions.
    assert_eq!(rc_video, 0, "get_stream_codec_stats(0x1011) rc={rc_video}");
    assert_eq!(
        video_out.kind, TST_CODEC_KIND_VIDEO,
        "expected VIDEO kind for video PID, got {}",
        video_out.kind
    );
    // Under the raw-first model the demuxer emits raw AU bytes without splitting
    // into NALs/OBUs, so nals_or_obus stays 0 at the receiver. The muxer side
    // still counts NALs (it receives already-split units). Verify that video
    // was received by checking random_access_aus instead — all 5 AUs were sent
    // with key=true, which sets the TS adaptation-field random_access_indicator.
    unsafe {
        assert!(
            video_out.u.video.random_access_aus > 0,
            "random_access_aus=0, expected > 0 (all 5 AUs sent with key=true)",
        );
    }

    // PAT PID assertion: PSI PIDs are not surfaced in stats_per_stream so
    // the accessor returns NotFound rather than a synthetic Unknown.
    assert_eq!(
        rc_pat,
        TstError::NotFound as i32,
        "expected TST_E_NOT_FOUND (-14) for PSI PID 0x0000, got rc={rc_pat}"
    );
}
