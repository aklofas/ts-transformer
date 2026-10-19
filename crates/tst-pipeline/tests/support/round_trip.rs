//! Shared body of the `pipeline_round_trip.rs` tests in tst-udp, tst-tcp
//! and tst-rist: `MuxSender` over a live send transport, the wire, then
//! `DemuxReceiver` over the matching receive transport must surface a
//! `ProgramMap`. Each crate includes this file with `#[path]`; it is not a
//! tst-pipeline test target (Cargo discovers only `tests/*.rs` and
//! `tests/*/main.rs`).
//!
//! # Sync lock
//!
//! The receive shell's sync state machine needs five consecutive
//! 188-byte-aligned sync bytes (≈ 941 bytes) before it locks and the
//! demuxer emits events; one small video frame is two or three TS
//! packets. The muxer therefore re-emits PAT + PMT every 10 ms
//! (`psi_interval_ms(10)`, the minimum) and frames go out 9001 ticks
//! (≈ 10 ms at 90 kHz) apart, so every push carries PAT + PMT + video and
//! a few pushes cross the lock threshold.
//!
//! # Hang-up tolerance
//!
//! The receive thread returns the moment it has the `ProgramMap`, dropping
//! its socket. A TCP sender then sees a reset on its next write; a UDP
//! sender (unconnected `send_to`) sees nothing. With `stop_on_hangup` a
//! `TransportBroken`/`Closed` send error ends the push loop; the verdict
//! is the receiver's report, which says `false` if the link broke before a
//! `ProgramMap` arrived. Any other error kind panics, so a mux or config
//! regression fails loudly.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::{DemuxReceiver, MuxSender, ShellErrorKind};

/// Per-transport timing of the one round trip.
pub struct Pacing {
    /// Pause before the sender opens, so the receive side is parked first.
    pub before_connect: Duration,
    /// Pause after the sender opens, before the first frame.
    pub after_connect: Duration,
    /// Frames pushed, 9001 ticks apart.
    pub frames: i64,
    /// End the push loop on `TransportBroken`/`Closed` instead of failing.
    pub stop_on_hangup: bool,
    /// Treat a receive-side `Backpressure` as a poll tick and keep reading.
    pub retry_backpressure: bool,
    /// How long the receive thread has to report.
    pub report_within: Duration,
}

/// UDP and TCP loopback. (Unused by the RIST test that includes this file.)
#[allow(dead_code)]
pub const LOOPBACK: Pacing = Pacing {
    before_connect: Duration::from_millis(50),
    after_connect: Duration::ZERO,
    frames: 5,
    stop_on_hangup: true,
    retry_backpressure: false,
    report_within: Duration::from_secs(3),
};

/// RIST loopback (unused by the UDP and TCP tests that include this file).
/// librist's Simple Profile handshake takes ≈ 500 ms on Linux loopback, and
/// its receive poll times out as `Backpressure` while the handshake settles.
#[allow(dead_code)]
pub const RIST: Pacing = Pacing {
    before_connect: Duration::from_millis(200),
    after_connect: Duration::from_millis(600),
    frames: 10,
    stop_on_hangup: false,
    retry_backpressure: true,
    report_within: Duration::from_secs(10),
};

/// Minimal Annex-B H.264 AUD + IDR header + 200 filler bytes — enough for
/// the muxer, which checks only the start code.
fn synthetic_h264_au() -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
    v.extend(std::iter::repeat_n(0xab, 200));
    v
}

/// Program 1, PMT on 0x1000, H.264 on 0x100, PSI every 10 ms.
fn mux_config() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.psi_interval_ms(10);
    b.build().expect("MuxerConfig::build")
}

/// `open_recv` runs on the receive thread (a TCP listener accepts there);
/// `open_send` runs on the caller's thread after `pacing.before_connect`.
pub fn assert_program_map_round_trip<R, T>(
    open_recv: impl FnOnce() -> R + Send + 'static,
    open_send: impl FnOnce() -> T,
    pacing: Pacing,
) where
    R: RecvTransport,
    T: Transport,
{
    let (found_tx, found_rx) = mpsc::channel::<bool>();
    let retry_backpressure = pacing.retry_backpressure;
    let _recv_thread = thread::spawn(move || {
        let mut receiver = DemuxReceiver::new(open_recv());
        for item in &mut receiver {
            match item {
                Ok(DemuxEvent::ProgramMap(_)) => {
                    let _ = found_tx.send(true);
                    return;
                }
                Ok(_) => {}
                Err(e) if retry_backpressure && e.kind == ShellErrorKind::Backpressure => {}
                Err(e) => {
                    eprintln!("demux err: {e:?}");
                    let _ = found_tx.send(false);
                    return;
                }
            }
        }
        // Transport closed before a ProgramMap arrived.
        let _ = found_tx.send(false);
    });

    thread::sleep(pacing.before_connect);
    let sender = MuxSender::new(open_send(), mux_config()).expect("MuxSender::new");
    thread::sleep(pacing.after_connect);

    let au = synthetic_h264_au();
    for i in 0..pacing.frames {
        match sender.send_video(&au, Pts90khz::new(i * 9001), true) {
            Ok(()) => {}
            Err(e)
                if pacing.stop_on_hangup
                    && matches!(
                        e.kind,
                        ShellErrorKind::TransportBroken | ShellErrorKind::Closed
                    ) =>
            {
                break;
            }
            Err(e) => panic!("send_video failed: {e:?}"),
        }
    }

    let ok = found_rx
        .recv_timeout(pacing.report_within)
        .unwrap_or_else(|_| {
            panic!(
                "receiver thread did not report within {:?}",
                pacing.report_within
            )
        });
    assert!(ok, "DemuxReceiver did not emit a ProgramMap event");
}
