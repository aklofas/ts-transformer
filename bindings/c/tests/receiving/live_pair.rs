//! End-to-end: tst_mux_sender_t → real Listener → accept → recv.
//!
//! Binds a Listener on 127.0.0.1:0, opens tst_mux_sender_t against the
//! kernel-assigned port, sends a synthetic NAL, and asserts the Listener
//! receives TS-shaped bytes (first byte 0x47).
//!
//! Uses the rlib output (crate-type includes "rlib") so that this
//! integration test can call the crate's Rust API directly — the same
//! `unsafe extern "C"` functions exported to C consumers.

// `tst-c`'s sender/receiver modules + the `tst-srt` dev-dep are gated
// behind `feature = "srt"`. Skip the entire file under
// `--no-default-features` (and `rtp-only`) so cargo test --workspace
// compiles cleanly without the SRT layer.
#![cfg(feature = "srt")]

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::SrtListenerBuilder;
use tstrans::config::{TstKlvStreamType, TstMuxConfig, TstVideoCodec};
use tstrans::config::{
    tst_mux_config_add_data_stream, tst_mux_config_add_klv_stream, tst_mux_config_add_program,
    tst_mux_config_add_video_stream, tst_mux_config_free, tst_mux_config_new,
    tst_reconnect_policy_free, tst_reconnect_policy_new,
};
use tstrans::sender::mux_sender::{
    tst_managed_mux_sender_close, tst_managed_mux_sender_open, tst_managed_mux_sender_send_data_to,
    tst_managed_mux_sender_send_klv_to, tst_managed_mux_sender_send_video_to, tst_mux_sender_close,
    tst_mux_sender_open, tst_mux_sender_send_data, tst_mux_sender_send_data_to,
    tst_mux_sender_send_video,
};

use crate::common::last_error_msg;

#[test]
fn mux_sender_to_listener_roundtrip() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (recv_tx, recv_rx) = mpsc::channel::<Vec<u8>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = SrtListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (mut accepted, _peer) = listener.accept().expect("accept");
        let mut buf = vec![0u8; 4096];
        let n = accepted.recv(&mut buf).expect("recv");
        recv_tx.send(buf[..n].to_vec()).expect("send recv'd bytes");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener didn't bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg: *mut TstMuxConfig = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32);
        tst_mux_config_add_klv_stream(
            cfg,
            prog,
            0x1031,
            TstKlvStreamType::PrivateData as i32,
            false,
        );

        let s = tst_mux_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());
        tst_mux_config_free(cfg);

        let nal: [u8; 9] = [0, 0, 0, 1, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];
        let rc = tst_mux_sender_send_video(s, nal.as_ptr(), nal.len(), 0, true);
        assert_eq!(rc, 0, "send_video failed: {}", last_error_msg());

        let received = recv_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no bytes received within 5s");
        assert!(!received.is_empty(), "received empty buffer");
        assert_eq!(
            received[0], 0x47,
            "expected TS sync byte 0x47; got 0x{:02x}",
            received[0]
        );

        tst_mux_sender_close(s);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ---------------------------------------------------------------------------
// Multi-stream: managed_mux_sender with 2 video + 1 KLV stream
// ---------------------------------------------------------------------------

#[test]
fn managed_mux_sender_multi_stream_loopback() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = SrtListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (mut accepted, _peer) = listener.accept().expect("accept");

        // Read until we've seen all three stream PIDs or the deadline expires.
        // Each srt_recv call delivers one SRT message (7 TS packets = 1316
        // bytes for the default bundle size). We need enough messages to see PAT,
        // PMT, and at least one packet from each of the three elementary
        // streams (EO video at 0x1011, IR video at 0x1012, KLV at 0x1031).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut accumulated = Vec::with_capacity(64 * 1024);
        let mut buf = vec![0u8; 4096];
        while std::time::Instant::now() < deadline {
            match accepted.recv(&mut buf) {
                Ok(n) if n > 0 => accumulated.extend_from_slice(&buf[..n]),
                Ok(_) => break, // 0-byte recv means connection closed
                Err(_) => break,
            }
            // Stop as soon as all three elementary-stream PIDs have appeared.
            // PAT (0x0000) and PMT will be present too, but what matters for
            // the assertion below is the three stream-level PIDs.
            let seen = pids_seen(&accumulated);
            if seen.contains(&0x1011) && seen.contains(&0x1012) && seen.contains(&0x1031) {
                break;
            }
        }
        bytes_tx.send(accumulated).expect("send bytes");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener didn't bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg: *mut TstMuxConfig = tst_mux_config_new();

        // Add two video streams (EO + IR) and one KLV stream, capturing the
        // handles returned by the _stream variants so we can fan out later.
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_eo = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32);
        let h_ir = tst_mux_config_add_video_stream(cfg, prog, 0x1012, TstVideoCodec::H264 as i32);
        let h_klv = tst_mux_config_add_klv_stream(
            cfg,
            prog,
            0x1031,
            TstKlvStreamType::PrivateData as i32,
            false,
        );

        // Use a default reconnect policy (no forced backoff — connect immediately).
        let policy = tst_reconnect_policy_new();
        let s = tst_managed_mux_sender_open(url.as_ptr(), cfg, policy);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());
        tst_mux_config_free(cfg);
        tst_reconnect_policy_free(policy);

        // Minimal Annex-B IDR NAL: start code + NAL header byte 0x65 (IDR,
        // nal_unit_type=5 for H.264). Payload bytes are synthetic filler.
        let nal: [u8; 9] = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];

        // Minimal KLV packet: 16-byte UL key from ST 0601 followed by
        // zero-length BER length. The muxer wraps this in a PES but does
        // not validate KLV content.
        let klv: [u8; 16] = [
            0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01, 0x0e, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ];

        // PTS values must be strictly increasing across all streams so the
        // muxer's PCR/PTS ordering logic stays happy. Send two rounds to
        // give the muxer enough elementary-stream data to flush output
        // bundles containing all three PIDs; PTS increments by ~3003 each
        // round (≈1/30 s at 90 kHz).
        for round in 0u64..3 {
            let base_pts = (round * 3003) as i64;
            let rc_eo = tst_managed_mux_sender_send_video_to(
                s,
                h_eo,
                nal.as_ptr(),
                nal.len(),
                base_pts,
                true,
            );
            assert_eq!(
                rc_eo,
                0,
                "send_video_to EO round {round} failed: {}",
                last_error_msg()
            );

            let rc_ir = tst_managed_mux_sender_send_video_to(
                s,
                h_ir,
                nal.as_ptr(),
                nal.len(),
                base_pts + 1,
                true,
            );
            assert_eq!(
                rc_ir,
                0,
                "send_video_to IR round {round} failed: {}",
                last_error_msg()
            );

            let rc_klv =
                tst_managed_mux_sender_send_klv_to(s, h_klv, klv.as_ptr(), klv.len(), base_pts + 2);
            assert_eq!(
                rc_klv,
                0,
                "send_klv_to round {round} failed: {}",
                last_error_msg()
            );
        }

        let received = bytes_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("listener thread didn't deliver bytes within 15s");

        assert!(!received.is_empty(), "received empty buffer");
        assert_eq!(
            received[0], 0x47,
            "expected TS sync byte 0x47 at offset 0; got 0x{:02x}",
            received[0]
        );

        let pids = pids_seen(&received);
        eprintln!("pids_seen = {:?}", pids);
        assert!(
            pids.contains(&0x1011),
            "missing EO video PID 0x1011; pids_seen = {:?}",
            pids
        );
        assert!(
            pids.contains(&0x1012),
            "missing IR video PID 0x1012; pids_seen = {:?}",
            pids
        );
        assert!(
            pids.contains(&0x1031),
            "missing KLV PID 0x1031; pids_seen = {:?}",
            pids
        );

        tst_managed_mux_sender_close(s);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ---------------------------------------------------------------------------
// Data stream: plain mux_sender with 1 video + 1 data stream
// ---------------------------------------------------------------------------

#[test]
fn mux_sender_data_stream_loopback() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = SrtListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (mut accepted, _peer) = listener.accept().expect("accept");

        // Read until both elementary-stream PIDs (video at 0x1011, data at
        // 0x1041) have appeared or the deadline expires — same accumulation
        // shape as managed_mux_sender_multi_stream_loopback above.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut accumulated = Vec::with_capacity(64 * 1024);
        let mut buf = vec![0u8; 4096];
        while std::time::Instant::now() < deadline {
            match accepted.recv(&mut buf) {
                Ok(n) if n > 0 => accumulated.extend_from_slice(&buf[..n]),
                Ok(_) => break, // 0-byte recv means connection closed
                Err(_) => break,
            }
            let seen = pids_seen(&accumulated);
            if seen.contains(&0x1011) && seen.contains(&0x1041) {
                break;
            }
        }
        bytes_tx.send(accumulated).expect("send bytes");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener didn't bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg: *mut TstMuxConfig = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        // One video stream (PCR anchor) + one data stream (stream_type 0xF0,
        // carries_pts = true so the PTS we pass lands in the PES header).
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);

        let s = tst_mux_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());
        tst_mux_config_free(cfg);

        // Minimal Annex-B IDR NAL: start code + NAL header byte 0x65 (IDR,
        // nal_unit_type=5 for H.264). Payload bytes are synthetic filler.
        let nal: [u8; 9] = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];

        // Arbitrary opaque payload — data streams are a PES pass-through, so
        // the muxer applies no framing and no payload inspection.
        let payload: [u8; 12] = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        ];

        // PTS values strictly increasing across all streams (see the managed
        // multi-stream test above). Exercise both entry points: the
        // single-stream shorthand (resolves because exactly one data stream
        // is configured) and the explicit-handle _to variant.
        for round in 0u64..3 {
            let base_pts = (round * 3003) as i64;
            let rc_video = tst_mux_sender_send_video(s, nal.as_ptr(), nal.len(), base_pts, true);
            assert_eq!(
                rc_video,
                0,
                "send_video round {round} failed: {}",
                last_error_msg()
            );

            let rc_data = if round % 2 == 0 {
                tst_mux_sender_send_data(s, payload.as_ptr(), payload.len(), base_pts + 1)
            } else {
                tst_mux_sender_send_data_to(
                    s,
                    h_data,
                    payload.as_ptr(),
                    payload.len(),
                    base_pts + 1,
                )
            };
            assert_eq!(
                rc_data,
                0,
                "send_data round {round} failed: {}",
                last_error_msg()
            );
        }

        let received = bytes_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("listener thread didn't deliver bytes within 15s");

        assert!(!received.is_empty(), "received empty buffer");
        assert_eq!(
            received[0], 0x47,
            "expected TS sync byte 0x47 at offset 0; got 0x{:02x}",
            received[0]
        );

        let pids = pids_seen(&received);
        eprintln!("pids_seen = {:?}", pids);
        assert!(
            pids.contains(&0x1011),
            "missing video PID 0x1011; pids_seen = {:?}",
            pids
        );
        assert!(
            pids.contains(&0x1041),
            "missing data PID 0x1041; pids_seen = {:?}",
            pids
        );

        tst_mux_sender_close(s);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ---------------------------------------------------------------------------
// Managed data stream: managed_mux_sender with 1 video + 1 data stream
// ---------------------------------------------------------------------------

#[test]
fn managed_mux_sender_data_stream_loopback() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = SrtListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (mut accepted, _peer) = listener.accept().expect("accept");

        // Accumulate until both PIDs (video 0x1011, data 0x1041) have been
        // seen or the deadline expires.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut accumulated = Vec::with_capacity(64 * 1024);
        let mut buf = vec![0u8; 4096];
        while std::time::Instant::now() < deadline {
            match accepted.recv(&mut buf) {
                Ok(n) if n > 0 => accumulated.extend_from_slice(&buf[..n]),
                Ok(_) => break,
                Err(_) => break,
            }
            let seen = pids_seen(&accumulated);
            if seen.contains(&0x1011) && seen.contains(&0x1041) {
                break;
            }
        }
        bytes_tx.send(accumulated).expect("send bytes");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener didn't bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg: *mut TstMuxConfig = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        // Capture both handles before the config is freed — they are stable
        // integer indices (not pointers) and survive config teardown.
        let h_video =
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32);
        // One data stream (stream_type 0xF0, carries_pts = true).
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);

        let policy = tst_reconnect_policy_new();
        let s = tst_managed_mux_sender_open(url.as_ptr(), cfg, policy);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());
        tst_mux_config_free(cfg);
        tst_reconnect_policy_free(policy);

        let nal: [u8; 9] = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];
        // Arbitrary opaque payload — data streams are a PES pass-through, so
        // the muxer applies no framing and no payload inspection.
        let payload: [u8; 12] = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        ];

        // Three rounds; PTS strictly increasing. Uses the explicit-handle
        // tst_managed_mux_sender_send_data_to entry point.
        for round in 0u64..3 {
            let base_pts = (round * 3003) as i64;
            let rc_video = tst_managed_mux_sender_send_video_to(
                s,
                h_video,
                nal.as_ptr(),
                nal.len(),
                base_pts,
                true,
            );
            assert_eq!(
                rc_video,
                0,
                "send_video_to round {round} failed: {}",
                last_error_msg()
            );

            let rc_data = tst_managed_mux_sender_send_data_to(
                s,
                h_data,
                payload.as_ptr(),
                payload.len(),
                base_pts + 1,
            );
            assert_eq!(
                rc_data,
                0,
                "send_data_to round {round} failed: {}",
                last_error_msg()
            );
        }

        let received = bytes_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("listener thread didn't deliver bytes within 15s");

        assert!(!received.is_empty(), "received empty buffer");
        assert_eq!(
            received[0], 0x47,
            "expected TS sync byte 0x47 at offset 0; got 0x{:02x}",
            received[0]
        );

        let pids = pids_seen(&received);
        eprintln!("pids_seen = {:?}", pids);
        assert!(
            pids.contains(&0x1011),
            "missing video PID 0x1011; pids_seen = {:?}",
            pids
        );
        assert!(
            pids.contains(&0x1041),
            "missing data PID 0x1041; pids_seen = {:?}",
            pids
        );

        tst_managed_mux_sender_close(s);
    }

    listener_thread.join().expect("listener thread panicked");
}

/// Walk a byte buffer treating every 188-byte aligned run as a TS packet and
/// collect all 13-bit PIDs encountered.  Byte 0 of each packet must be the
/// TS sync byte 0x47; if it isn't the walker advances one byte at a time to
/// re-align (handles minor framing skew).
fn pids_seen(bytes: &[u8]) -> std::collections::HashSet<u16> {
    let mut pids = std::collections::HashSet::new();
    let mut offset = 0;
    while offset + 188 <= bytes.len() {
        if bytes[offset] != 0x47 {
            // Not aligned — step forward one byte and try again.
            offset += 1;
            continue;
        }
        let pid = (((bytes[offset + 1] & 0x1F) as u16) << 8) | (bytes[offset + 2] as u16);
        pids.insert(pid);
        offset += 188;
    }
    pids
}
