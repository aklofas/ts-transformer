//! Verifies the `info_span!`/`info!` lifetime events on each of the
//! six pipeline shells.
//!
//! Each shell opens an `info_span!` in its constructor and emits an
//! `info!` event there ("<Shell> opened"); on `Drop` it re-enters the
//! span and emits a closing event ("<Shell> closed"). These tests
//! construct each shell and let it drop at end-of-scope, then assert
//! both events appeared in the captured `tracing` output.

mod common;

use crate::common::{ScriptedRecv, ScriptedSend};

use tracing_test::traced_test;

use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::TransportError;

use tst_pipeline::{
    DemuxReceiver, MuxSender, RawReceiver, RawReceiverConfig, RawSender, RawSenderConfig, Receiver,
    ReceiverConfig, Sender, SenderConfig,
};

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
        let _sender = MuxSender::new(ScriptedSend::ok(), cfg).unwrap();
        // Drops at end of block.
    }
    assert!(logs_contain("MuxSender opened"));
    assert!(logs_contain("MuxSender closed"));
}

#[traced_test]
#[test]
fn sender_emits_open_and_close_events() {
    {
        let _sender = Sender::new(ScriptedSend::ok(), SenderConfig::default());
    }
    assert!(logs_contain("Sender opened"));
    assert!(logs_contain("Sender closed"));
}

#[traced_test]
#[test]
fn raw_sender_emits_open_and_close_events() {
    {
        let _sender = RawSender::new(ScriptedSend::ok(), RawSenderConfig::default());
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
        let _rx = DemuxReceiver::new(ScriptedRecv::failing(TransportError::Closed));
    }
    assert!(logs_contain("DemuxReceiver opened"));
    assert!(logs_contain("DemuxReceiver closed"));
}

#[traced_test]
#[test]
fn receiver_emits_open_and_close_events() {
    {
        let _rx = Receiver::new(
            ScriptedRecv::failing(TransportError::Closed),
            ReceiverConfig::default(),
        );
    }
    assert!(logs_contain("Receiver opened"));
    assert!(logs_contain("Receiver closed"));
}

#[traced_test]
#[test]
fn raw_receiver_emits_open_and_close_events() {
    {
        let _rx = RawReceiver::new(
            ScriptedRecv::failing(TransportError::Closed),
            RawReceiverConfig::default(),
        );
    }
    assert!(logs_contain("RawReceiver opened"));
    assert!(logs_contain("RawReceiver closed"));
}
