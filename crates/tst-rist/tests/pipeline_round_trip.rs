//! Pipeline-shell integration: `MuxSender<RistTransport>` → RIST loopback →
//! `DemuxReceiver<RistRecvTransport>`.
//!
//! Mirrors `crates/tst-tcp/tests/pipeline_round_trip.rs` but over RIST. The
//! sync-lock requirement is identical: ≥ 941 bytes of aligned 0x47-framed
//! data before the demuxer transitions VERIFY → LOCKED and starts emitting
//! events. Strategy: `psi_interval_ms(10)` so PAT + PMT re-emit on every
//! push, then send several frames at 9001-tick spacing.
//!
//! librist's handshake (~500ms on Linux loopback) is slower than UDP/TCP,
//! so the receive thread tolerates the librist-internal poll Backpressure
//! timeouts that fire while the handshake settles.
//!
//! Runs on Windows too: the librist ≤ 0.2.16 Windows teardown hang and
//! zero-delivery bug are fixed upstream in librist 0.2.18 (see `loopback.rs`).

use std::net::UdpSocket;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::{DemuxReceiver, MuxSender, ShellErrorKind};
use tst_rist::{RistRecvTransportBuilder, RistTransportBuilder};

/// Serialize RIST pipeline tests within the process — same rationale as
/// `tests/loopback.rs`. Ephemeral-port + librist-rebind race is brittle
/// when multiple RIST tests run in parallel.
static SERIAL: Mutex<()> = Mutex::new(());

fn find_free_udp_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = s.local_addr().expect("local_addr").port();
    drop(s);
    port
}

/// Minimal Annex-B H.264 AUD + IDR — just enough for the muxer to accept and
/// frame into a PES.
fn synthetic_h264_au() -> Vec<u8> {
    // AUD NAL (nal_type=9, primary_pic_type=0x10) followed by IDR NAL header.
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
    v.extend(std::iter::repeat(0xab).take(200));
    v
}

#[test]
fn mux_via_rist_demux_round_trip_recovers_program_map() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());

    let port = find_free_udp_port();
    let bind_url = format!("rist://@127.0.0.1:{port}");
    let connect_url = format!("rist://127.0.0.1:{port}");

    // -----------------------------------------------------------------------
    // 1. Build the muxer config — PSI every 10ms so the lock fires quickly.
    // -----------------------------------------------------------------------
    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().expect("MuxerConfig::build")
    };

    // -----------------------------------------------------------------------
    // 2. Open the receiver first so the sender's first packet doesn't get
    //    dropped on the floor.
    // -----------------------------------------------------------------------
    let recv_transport = RistRecvTransportBuilder::new(&bind_url)
        .expect("RistRecvTransportBuilder::new")
        .listen()
        .expect("listen");

    // -----------------------------------------------------------------------
    // 3. Spawn the receiver loop. Iterates DemuxReceiver events until it
    //    sees a ProgramMap.
    // -----------------------------------------------------------------------
    let (found_tx, found_rx) = std::sync::mpsc::channel::<bool>();
    let _recv_thread = thread::spawn(move || {
        let mut receiver = DemuxReceiver::new(recv_transport);
        // DemuxReceiver implements Iterator<Item = Result<DemuxEvent, _>>.
        for item in &mut receiver {
            match item {
                Ok(DemuxEvent::ProgramMap(_)) => {
                    let _ = found_tx.send(true);
                    return;
                }
                Ok(_) => { /* keep reading */ }
                Err(e) if e.kind == ShellErrorKind::Backpressure => {
                    // librist poll timeout during handshake settling — retry.
                }
                Err(e) => {
                    eprintln!("demux err: {e:?}");
                    let _ = found_tx.send(false);
                    return;
                }
            }
        }
        // Transport closed before we saw a ProgramMap.
        let _ = found_tx.send(false);
    });

    // -----------------------------------------------------------------------
    // 4. Connect the sender. Sleep for the librist handshake to settle.
    //
    //    200ms gives the receiver thread time to fully bind before we
    //    initiate; the additional 600ms lets the librist Simple Profile
    //    handshake (~500ms on Linux loopback) complete before we send.
    // -----------------------------------------------------------------------
    thread::sleep(Duration::from_millis(200));
    let send_transport = RistTransportBuilder::new(&connect_url)
        .expect("RistTransportBuilder::new")
        .connect()
        .expect("connect");
    let sender = MuxSender::new(send_transport, mux_cfg).expect("MuxSender::new");

    thread::sleep(Duration::from_millis(600));

    // -----------------------------------------------------------------------
    // 5. Push several frames at 9001-tick spacing — enough for the sync
    //    state machine to lock.
    //
    //    Spacing each frame 9001 ticks apart (≈ 10 ms at 90 kHz) crosses the
    //    10-ms PSI re-emission threshold on every push, so PAT + PMT appear
    //    in every outbound batch.  After a few sends the sync state machine has
    //    seen ≥ 941 bytes of aligned 0x47-framed data and will lock.
    // -----------------------------------------------------------------------
    let au = synthetic_h264_au();
    for i in 0i64..10 {
        let pts = Pts90khz::new(i * 9001);
        sender.send_video(&au, pts, true).expect("send_video");
    }

    // -----------------------------------------------------------------------
    // 6. Verify: ProgramMap arrives within 10s (generous slack for librist).
    // -----------------------------------------------------------------------
    let ok = found_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("receiver thread did not report within 10 s");

    assert!(ok, "DemuxReceiver did not emit a ProgramMap event");
}
