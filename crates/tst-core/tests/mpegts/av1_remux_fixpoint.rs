//! Acceptance: mux -> demux raw -> remux is a PAYLOAD FIXPOINT in
//! BOTH carriage modes, using the pass-through wire push for the remux.
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::event::{DemuxEvent, SamplePayload};
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_core::shared::SharedBytes;

fn obu(t: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![(t << 3) | 0x02, body.len() as u8];
    v.extend_from_slice(body);
    v
}

fn synth() -> Vec<u8> {
    let mut au = Vec::new();
    au.extend(obu(2, &[]));
    au.extend(obu(1, &[0x00, 0x00, 0x01, 0xAA])); // forces emulation prevention in binding
    au.extend(obu(3, &[0x00, 0xFF]));
    au
}

fn drain(m: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = m.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn mux_cfg(mode: Av1CarriageMode) -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
    prog.add_video(0x101, VideoCodec::Av1);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.av1_carriage(mode);
    b.build().unwrap()
}

fn first_raw(ts: &[u8], mode: Av1CarriageMode) -> (SharedBytes, Option<Av1CarriageMode>) {
    let mut d = Demuxer::with_config(DemuxerConfig::builder().av1_carriage(mode).build());
    d.feed(ts).unwrap();
    d.flush();
    while let Some(e) = d.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video {
                raw, av1_carriage, ..
            },
            ..
        } = e
        {
            return (raw, av1_carriage);
        }
    }
    panic!("no video sample");
}

fn fixpoint(mode: Av1CarriageMode) {
    // In `InteropRawObu` mode neither generation wraps (av1_binding=false ->
    // do_wrap=false for both the elementary push and the wire push), so the
    // interop fixpoint holds by identity and guards against the wire push
    // *accidentally* wrapping in interop mode. The binding case is the
    // substantive proof: wrap-then-passthrough must equal the framed bytes.
    //
    // First generation: elementary OBUs -> wrapping push.
    let mut m1 = Muxer::new(mux_cfg(mode)).unwrap();
    let h1 = m1.video_handles()[0];
    m1.push_video_to(h1, &synth(), Pts90khz::new(90_000), true)
        .unwrap();
    let ts1 = drain(&mut m1);
    let (raw1, prov) = first_raw(&ts1, mode);
    assert_eq!(prov, Some(mode));
    // Guard against a vacuous pass: an empty-payload regression would make
    // the fixpoint assertion below succeed as `[] == []`.
    assert!(
        !raw1.is_empty(),
        "synth must produce a non-empty wire payload"
    );

    // Re-mux: feed the demuxed wire bytes through the PASS-THROUGH push.
    let mut m2 = Muxer::new(mux_cfg(mode)).unwrap();
    let h2 = m2.video_handles()[0];
    m2.push_video_wire_to(h2, raw1.as_slice(), Pts90khz::new(90_000), true)
        .unwrap();
    let ts2 = drain(&mut m2);
    let (raw2, _) = first_raw(&ts2, mode);

    assert_eq!(
        raw1.as_slice(),
        raw2.as_slice(),
        "payload fixpoint must hold for {mode:?}"
    );
}

#[test]
fn binding_mode_remux_is_payload_fixpoint() {
    fixpoint(Av1CarriageMode::Mpeg2TsBinding);
}

#[test]
fn interop_mode_remux_is_payload_fixpoint() {
    fixpoint(Av1CarriageMode::InteropRawObu);
}
