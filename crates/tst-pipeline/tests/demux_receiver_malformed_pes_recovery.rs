//! `DemuxReceiver` survives a single `MalformedPes` in lenient mode.
//!
//! This is the pipeline-side counterpart to
//! `tst-core/tests/demux_malformed_pes_recovery.rs`: it drives a
//! `DemuxReceiver` (not a bare `Demuxer`) over a mock `RecvTransport` that
//! plays back the same corrupt-PES-then-good-PES TS stream, and asserts
//! that the receive loop emits both the `NonConformant { MalformedPes }`
//! event AND the post-recovery video `Sample` — proving the lenient fix
//! flows through the pipeline shell's `feed_aligned` call site.

mod common;

use crate::common::{build_h264_au, drain_mux, to_ts_packets};

use tst_core::TransportError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, NonConformantIssue, SamplePayload};
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::RecvTransport;
use tst_pipeline::DemuxReceiver;

/// Build a corrupted TS byte stream by muxing two H.264 PESes on PID 0x100
/// and flipping the third PES start-code byte of the first one. Returns the
/// packets as 188-byte aligned chunks ready for the recv mock.
fn build_packets_with_one_malformed_pes() -> Vec<[u8; 188]> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    mux.push_video(&build_h264_au(0xAA), Pts90khz::new(90_000), true)
        .unwrap();
    let bytes1 = drain_mux(&mut mux);
    mux.push_video(&build_h264_au(0xAA), Pts90khz::new(180_000), true)
        .unwrap();
    let bytes2 = drain_mux(&mut mux);

    let mut bytes = bytes1;
    bytes.extend_from_slice(&bytes2);

    let mut corrupted = false;
    for chunk in bytes.chunks_exact_mut(188) {
        if chunk[0] != 0x47 {
            continue;
        }
        let pusi = (chunk[1] & 0x40) != 0;
        let pid = (((chunk[1] as u16) & 0x1F) << 8) | (chunk[2] as u16);
        if !pusi || pid != 0x100 {
            continue;
        }
        let afc = (chunk[3] >> 4) & 0x3;
        let mut payload_off = 4usize;
        if afc & 0x2 != 0 {
            let af_len = chunk[4] as usize;
            payload_off = 5 + af_len;
        }
        if payload_off + 3 >= 188 {
            continue;
        }
        chunk[payload_off + 2] = 0xFF;
        corrupted = true;
        break;
    }
    assert!(
        corrupted,
        "test setup: no PUSI packet found on video PID 0x100 to corrupt"
    );

    to_ts_packets(&bytes)
}

/// Minimal `RecvTransport` that delivers one 188-byte TS packet per
/// `recv_bytes` call, then signals `Closed` (EOF — DemuxReceiver converts
/// this to a flush + `Ok(None)`).
struct PacketSource {
    packets: Vec<[u8; 188]>,
    pos: usize,
}

impl PacketSource {
    fn new(packets: Vec<[u8; 188]>) -> Self {
        Self { packets, pos: 0 }
    }
}

impl RecvTransport for PacketSource {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.pos >= self.packets.len() {
            return Err(TransportError::Closed);
        }
        let pkt = self.packets[self.pos];
        buf[..188].copy_from_slice(&pkt);
        self.pos += 1;
        Ok(188)
    }

    fn max_payload(&self) -> usize {
        188
    }

    fn is_alive(&self) -> bool {
        self.pos < self.packets.len()
    }
}

#[test]
fn demux_receiver_lenient_mode_recovers_from_malformed_pes() {
    let packets = build_packets_with_one_malformed_pes();
    let source = PacketSource::new(packets);
    let mut rx = DemuxReceiver::new(source); // default = lenient

    let mut saw_malformed = false;
    let mut saw_sample = false;
    loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::NonConformant {
                issue: NonConformantIssue::MalformedPes { .. },
                ..
            })) => saw_malformed = true,
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { .. },
                ..
            })) => saw_sample = true,
            Ok(Some(_)) => {}
            Ok(None) => break, // clean EOF after flush
            Err(e) => panic!("lenient DemuxReceiver must not error fatally on MalformedPes: {e:?}"),
        }
    }

    assert!(
        saw_malformed,
        "DemuxReceiver: must surface MalformedPes as NonConformant in lenient mode"
    );
    assert!(
        saw_sample,
        "DemuxReceiver: must continue parsing past the corrupt PES and emit the recovery Sample"
    );
}
