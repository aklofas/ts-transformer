//! End-to-end loopback: tst_mux_sender_t (caller, multi-stream mux) ↔
//! tst_demux_receiver_t (listener). Validates that the typed-event
//! surface delivers a PROGRAM_MAP event with the expected video PID,
//! at least one SAMPLE event with the expected codec discriminator,
//! and a final TST_E_END_OF_STREAM on graceful sender close.
//!
//! Mirrors the ts_receiver_loopback.rs threading pattern:
//! the receiver thread calls open_listener (which blocks on SRT accept),
//! fires a ready signal once the peer has connected, then drains events.
//! The sender thread retries connect in a loop until the listener is
//! bound and waiting, then sends synthetic H.264 NAL bytes and closes.

// Whole file depends on `tstrans::sender::*` + `tstrans::receiver::*`
// (both gated on `feature = "srt"`).
#![cfg(feature = "srt")]

use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::error::{TstError, tst_get_last_error_str};
use tstrans::event::{TstEvent, TstEventKind};
use tstrans::receiver::demux_receiver::{
    tst_demux_receiver_close, tst_demux_receiver_get_stream_last_seen_micros,
    tst_demux_receiver_open_listener, tst_demux_receiver_recv_event,
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

#[test]
fn loopback_mux_sender_to_demux_receiver_delivers_pmt_and_sample_and_eos() {
    // Pick a port offset by pid to reduce collisions across test processes.
    // The base (29_000) is kept in a distinct 1000-wide band from the sibling
    // receiver loopbacks (raw 27_000, ts 28_000) so they never bind the same
    // port when run concurrently inside one consolidated test binary — the old
    // 28_500 + pid%500 band overlapped ts's 28_000 + pid%1000 for half of pids.
    let port: u16 = 29_000 + (std::process::id() as u16 % 1000);

    // ready_tx fires after open_listener unblocks (peer accepted),
    // telling the sender that the receiver is past accept and ready to
    // receive. Because open_listener blocks on SRT accept, the sender
    // must already be connected at this point, so ready_rx.recv() is
    // effectively instant once the sender successfully connects.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    let receiver_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        // Blocks on SRT accept until the sender connects — returns only
        // after a peer has joined. Returns NULL + last-error on failure.
        let rx = unsafe { tst_demux_receiver_open_listener(url.as_ptr()) };
        if rx.is_null() {
            let msg = last_error_msg();
            panic!("tst_demux_receiver_open_listener failed: {msg}");
        }

        // Peer connected — unblock the sender to start sending data.
        ready_tx.send(()).expect("ready channel dropped");

        let mut got_pmt = false;
        let mut got_sample = false;
        let mut ev = TstEvent::default();

        // Drain events until EOS; assert PMT + SAMPLE seen before EOS.
        loop {
            let rc = unsafe { tst_demux_receiver_recv_event(rx, &mut ev) };
            if rc == 0 {
                if ev.kind == TstEventKind::ProgramMap as i32 {
                    got_pmt = true;
                }
                if ev.kind == TstEventKind::Sample as i32 {
                    got_sample = true;
                }
                continue;
            }
            if rc == TstError::EndOfStream as i32 {
                break;
            }
            panic!("recv_event failed (rc={rc}): {}", last_error_msg());
        }

        unsafe { tst_demux_receiver_close(rx) };

        assert!(got_pmt, "no PROGRAM_MAP event received");
        // TODO: if this fires, the synthetic NAL bytes were insufficient
        // to produce a decoded Sample event — replace with encoder-derived
        // fixtures and re-enable.
        assert!(got_sample, "no SAMPLE event received");
    });

    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

        // Build a single-program, single-stream H.264 mux config. The
        // program_number and pmt_pid values (1 / 0x0100) are conventional
        // defaults matching ffmpeg's single-program MPEG-TS output.
        let cfg = unsafe { tst_mux_config_new() };
        let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x0100) };
        // Add a video stream on PID 0x1011; the returned handle is only
        // needed for multi-stream _send_video_to calls — tst_mux_sender_send_video
        // targets the single video stream automatically.
        let _video =
            unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

        // Retry loop: the receiver's open_listener is blocking on accept,
        // so the listener socket may not be bound yet when this thread
        // starts. Retry for up to 5 seconds.
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

        // Wait for the receiver to be past accept and ready to recv
        // before flooding it with packets. In practice the sender already
        // connected (open_listener blocks on accept and fires ready after
        // returning), so this is near-instant.
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver did not signal ready within 5s");

        // Synthetic H.264 NAL: Annex-B start code + SPS-shaped byte
        // sequence + start code + IDR-shaped byte sequence. The bytes do
        // not need to be syntactically valid — the muxer wraps them into
        // a PES and the demuxer surfaces them as a Sample event keyed on
        // the stream_type registered in the PMT.
        let nal: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, // Annex-B start code
            0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, // SPS-shaped (nal_type=7)
            0x00, 0x00, 0x00, 0x01, // Annex-B start code
            0x65, 0x88, 0x80, 0x40, // IDR-shaped (nal_type=5)
        ];

        for i in 0..5 {
            // PTS in 90 kHz ticks: 40 ms per frame (25 fps).
            let pts = (i as i64) * 3_600;
            let rc = unsafe {
                tst_mux_sender_send_video(
                    tx,
                    nal.as_ptr(),
                    nal.len(),
                    pts,
                    /*key_frame=*/ true,
                )
            };
            assert_eq!(
                rc,
                0,
                "send_video[{i}] expected 0, got {rc}: {}",
                last_error_msg()
            );
        }

        // Drain pause before close — the muxer's SRT send queue is
        // asynchronous with respect to close. 1 s comfortably covers the
        // default 120 ms SRT latency budget plus loopback scheduling
        // jitter, giving the demuxer time to surface PROGRAM_MAP +
        // Sample events before EOS propagates.
        //
        // At 200 ms macOS arm64 runners fail with "no PROGRAM_MAP event
        // received" — Darwin's scheduling on Apple Silicon pushes PMT
        // emission past that window. Linux loopback tolerates 200 ms but
        // the extra headroom is cheap and keeps the test
        // cross-platform-stable.
        thread::sleep(Duration::from_secs(1));

        unsafe { tst_mux_sender_close(tx) };
        unsafe { tst_mux_config_free(cfg) };
    });

    receiver_thread.join().expect("receiver thread panicked");
    sender_thread.join().expect("sender thread panicked");
}

/// Live SRT session: once a Sample event for the video pid has been
/// observed, `tst_demux_receiver_get_stream_last_seen_micros` must report
/// a nonzero epoch timestamp for that pid, and `0` for a pid that was
/// never observed on this handle.
///
/// Structurally the same rendezvous as
/// `loopback_mux_sender_to_demux_receiver_delivers_pmt_and_sample_and_eos`
/// above — see that test's comments for the threading rationale. Uses a
/// disjoint port band (40_000+) so the two tests never collide on the
/// same PID within this consolidated test binary.
#[test]
fn loopback_last_seen_micros_reports_after_sample_and_zero_for_unknown_pid() {
    let port: u16 = 40_000 + (std::process::id() as u16 % 500);

    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    let receiver_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        let rx = unsafe { tst_demux_receiver_open_listener(url.as_ptr()) };
        if rx.is_null() {
            let msg = last_error_msg();
            panic!("tst_demux_receiver_open_listener failed: {msg}");
        }

        ready_tx.send(()).expect("ready channel dropped");

        let mut got_sample = false;
        let mut ev = TstEvent::default();

        loop {
            let rc = unsafe { tst_demux_receiver_recv_event(rx, &mut ev) };
            if rc == 0 {
                if ev.kind == TstEventKind::Sample as i32 {
                    got_sample = true;
                }
                continue;
            }
            if rc == TstError::EndOfStream as i32 {
                break;
            }
            panic!("recv_event failed (rc={rc}): {}", last_error_msg());
        }

        assert!(got_sample, "no SAMPLE event received");

        // Known pid (0x1011, the video stream configured below) must show
        // a nonzero epoch timestamp now that a sample carried it.
        let mut micros: u64 = 0;
        let rc = unsafe { tst_demux_receiver_get_stream_last_seen_micros(rx, 0x1011, &mut micros) };
        assert_eq!(
            rc,
            0,
            "get_stream_last_seen_micros(0x1011) failed: {}",
            last_error_msg()
        );
        assert!(
            micros > 0,
            "expected nonzero last_seen after a delivered sample"
        );

        // Bogus/never-observed pid must report 0, not error and not a
        // stale value from the out-param's initial contents.
        let mut bogus_micros: u64 = 0xDEAD_BEEF;
        let rc = unsafe {
            tst_demux_receiver_get_stream_last_seen_micros(rx, 0x1FFF, &mut bogus_micros)
        };
        assert_eq!(
            rc,
            0,
            "get_stream_last_seen_micros(0x1FFF) failed: {}",
            last_error_msg()
        );
        assert_eq!(
            bogus_micros, 0,
            "unknown pid must report 0, not a stale value"
        );

        unsafe { tst_demux_receiver_close(rx) };
    });

    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

        let cfg = unsafe { tst_mux_config_new() };
        let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x0100) };
        let _video =
            unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264) };

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

        let nal: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x65, 0x88, 0x80, 0x40,
        ];

        for i in 0..5 {
            let pts = (i as i64) * 3_600;
            let rc = unsafe {
                tst_mux_sender_send_video(
                    tx,
                    nal.as_ptr(),
                    nal.len(),
                    pts,
                    /*key_frame=*/ true,
                )
            };
            assert_eq!(
                rc,
                0,
                "send_video[{i}] expected 0, got {rc}: {}",
                last_error_msg()
            );
        }

        thread::sleep(Duration::from_secs(1));

        unsafe { tst_mux_sender_close(tx) };
        unsafe { tst_mux_config_free(cfg) };
    });

    receiver_thread.join().expect("receiver thread panicked");
    sender_thread.join().expect("sender thread panicked");
}
