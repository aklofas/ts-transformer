//! Verifies the `info_span!`/`info!` lifetime events on each of the
//! six pipeline shells.
//!
//! Each shell opens an `info_span!` in its constructor and emits an
//! `info!` event there ("<Shell> opened"); on `Drop` it re-enters the
//! span and emits a closing event ("<Shell> closed"). These tests
//! construct each shell and let it drop at end-of-scope, then assert
//! both events appeared in the captured `tracing` output.

use std::collections::VecDeque;
use tracing_test::traced_test;

use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{RecvTransport, Transport, TransportError};

use tst_pipeline::{
    DemuxReceiver, MuxSender, RawReceiver, RawReceiverConfig, RawSender, RawSenderConfig, Receiver,
    ReceiverConfig, Sender, SenderConfig,
};

// ---------------------------------------------------------------------------
// Inline test transports — kept private so each test file has its own copy
// (same canonical pattern used in the sub-phase 2.2 doctests).
// ---------------------------------------------------------------------------

struct Sink(Vec<u8>);
impl Transport for Sink {
    fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
        self.0.extend_from_slice(b);
        Ok(())
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn close(&mut self) {}
    fn is_alive(&self) -> bool {
        true
    }
}

struct Source(VecDeque<Vec<u8>>);
impl RecvTransport for Source {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.0.pop_front() {
            Some(v) => {
                let n = v.len().min(buf.len());
                buf[..n].copy_from_slice(&v[..n]);
                Ok(n)
            }
            None => Err(TransportError::Closed),
        }
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        !self.0.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Sender-side shells
// ---------------------------------------------------------------------------

#[traced_test]
#[test]
fn mux_sender_emits_open_and_close_events() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    {
        let _sender = MuxSender::new(Sink(Vec::new()), cfg).unwrap();
        // Drops at end of block.
    }
    assert!(logs_contain("MuxSender opened"));
    assert!(logs_contain("MuxSender closed"));
}

#[traced_test]
#[test]
fn sender_emits_open_and_close_events() {
    {
        let _sender = Sender::new(Sink(Vec::new()), SenderConfig::default());
    }
    assert!(logs_contain("Sender opened"));
    assert!(logs_contain("Sender closed"));
}

#[traced_test]
#[test]
fn raw_sender_emits_open_and_close_events() {
    {
        let _sender = RawSender::new(Sink(Vec::new()), RawSenderConfig::default());
    }
    assert!(logs_contain("RawSender opened"));
    assert!(logs_contain("RawSender closed"));
}

// ---------------------------------------------------------------------------
// Receiver-side shells
// ---------------------------------------------------------------------------

#[traced_test]
#[test]
fn demux_receiver_emits_open_and_close_events() {
    {
        let _rx = DemuxReceiver::new(Source(VecDeque::new()));
    }
    assert!(logs_contain("DemuxReceiver opened"));
    assert!(logs_contain("DemuxReceiver closed"));
}

#[traced_test]
#[test]
fn receiver_emits_open_and_close_events() {
    {
        let _rx = Receiver::new(Source(VecDeque::new()), ReceiverConfig::default());
    }
    assert!(logs_contain("Receiver opened"));
    assert!(logs_contain("Receiver closed"));
}

#[traced_test]
#[test]
fn raw_receiver_emits_open_and_close_events() {
    {
        let _rx = RawReceiver::new(Source(VecDeque::new()), RawReceiverConfig::default());
    }
    assert!(logs_contain("RawReceiver opened"));
    assert!(logs_contain("RawReceiver closed"));
}
