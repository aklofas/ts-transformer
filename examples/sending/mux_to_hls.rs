//! mux_to_hls — re-mux a .ts file's video into HLS segments + a playlist.
//!
//! WHY this example exists:
//!   Exercises the `Publisher` trait family end to end: MuxPublisher →
//!   HlsPublisher → on-disk .ts segments + .m3u8 + internal HTTP server.
//!   The input is an MPEG-TS file, so it is demuxed first: the publisher
//!   takes encoded access units with their timestamps, never TS packets
//!   (a TS packet starts with the 0x47 sync byte, which is no Annex-B start
//!   code, so pushing file bytes fails on the first push). Each video access
//!   unit goes in with the PTS it carried, its DTS when that differs, and
//!   its keyframe flag, so segments cut on the source's own keyframes and
//!   keep its timing. The first video stream (H.264, H.265 or H.266) is
//!   re-muxed from its first keyframe on; AV1 is skipped, and fatal only
//!   when it is the only video; KLV, audio and any other stream are left
//!   out (configure a KLV stream and call `MuxPublisher::push_klv` to carry
//!   KLV).
//!
//! HOW to run:
//!   cargo run -p tst-examples --example mux_to_hls -- input.ts /tmp/hls 127.0.0.1:8080
//!
//!   The output directory must be yours and not writable by group or
//!   others; a missing one is created 0700.
//!
//! HOW to verify with ffmpeg / VLC / mpv, once "serving" is printed:
//!   ffplay 'http://localhost:8080/playlist.m3u8'
//!   vlc    'http://localhost:8080/playlist.m3u8'
//!   mpv    'http://localhost:8080/playlist.m3u8'
//!
//! The file is re-muxed as fast as it is read. The playlist is then
//! complete (EVENT mode, ending in #EXT-X-ENDLIST) and the server keeps
//! serving it and its segments until you stop the program with Ctrl-C.

use std::fs::File;
use std::io::{BufReader, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, NalUnit, SamplePayload, VideoPayload, split_video,
};
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_hls::{HlsMode, HlsPublisher, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;

/// Bytes read and fed to the demuxer per call: 1024 TS packets. Any size
/// works (the demuxer re-aligns across calls); reading in chunks keeps a
/// large capture out of memory.
const FEED_CHUNK: usize = 188 * 1024;

/// Whether an access unit starts decoding: the PES `random_access_indicator`
/// when the producer set it, otherwise an IRAP NAL in the unit itself (H.264
/// IDR type 5; H.265 BLA/IDR/CRA types 16..=21; H.266 IDR/CRA types 7..=9).
/// Many producers leave the indicator unset, and a segment must open on a
/// unit a decoder can start from (RFC 8216 §3).
fn starts_decoding(raw: &tst_core::shared::SharedBytes, codec: VideoCodec, rai: bool) -> bool {
    if rai {
        return true;
    }
    let (payload, _issues) = split_video(raw, codec, Default::default());
    let VideoPayload::Nals(nals) = payload else {
        return false;
    };
    nals.iter().any(|n| match n {
        NalUnit::H264 { nal_type, .. } => *nal_type == 5,
        NalUnit::H265 { nal_type, .. } => (16..=21).contains(nal_type),
        NalUnit::H266 { nal_type, .. } => (7..=9).contains(nal_type),
    })
}

/// Which access units of the input are re-muxed. The first non-AV1 video PID
/// seen is the one re-muxed; its units are admitted from the first keyframe
/// on, so segment 0 opens on a unit a decoder can start from.
#[derive(Default)]
struct VideoPick {
    pid: Option<u16>,
    started: bool,
    saw_av1: bool,
}

impl VideoPick {
    /// Whether this access unit is pushed.
    fn admit(&mut self, pid: u16, codec: VideoCodec, key: bool) -> bool {
        if codec == VideoCodec::Av1 {
            // AV1 is OBU-framed; `push_video` takes Annex-B NAL units, so it
            // is skipped rather than fatal: another video stream of the same
            // input may still be re-muxable.
            self.saw_av1 = true;
            return false;
        }
        if *self.pid.get_or_insert(pid) != pid {
            return false;
        }
        // A capture cut mid-GOP starts on delta frames; a playlist whose
        // first segment opens on one is undecodable until the next keyframe.
        self.started |= key;
        self.started
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: mux_to_hls <input.ts> <out_dir> <bind>";
    let input = args.next().expect(usage);
    let out_dir = PathBuf::from(args.next().expect(usage));
    let bind: SocketAddr = args.next().expect(usage).parse()?;

    // EVENT, not LIVE: a LIVE playlist evicts all but its last few
    // segments, so a whole-file re-mux would leave only its tail. EVENT
    // keeps every segment and ends the playlist at finish.
    let publisher = HlsPublisherBuilder::new()
        .bind(bind)
        .output_dir(&out_dir)
        .segment_duration(Duration::from_secs(4))
        .mode(HlsMode::Event)
        .build()?;
    eprintln!(
        "HLS publisher on http://{}/playlist.m3u8 (out_dir = {})",
        publisher.local_addr().unwrap(),
        out_dir.display()
    );

    // The muxer needs the codec before the first push, and only the input
    // knows it: the shell is built on the first admitted access unit (the
    // first keyframe of the first re-muxable video stream).
    let mut publisher = Some(publisher);
    let mut shell: Option<MuxPublisher<HlsPublisher>> = None;
    let mut pick = VideoPick::default();
    let mut pushed = 0u64;

    let mut demux = Demuxer::new();
    let mut file = BufReader::new(File::open(&input)?);
    let mut buf = vec![0u8; FEED_CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            // End of input: `flush` completes the last PES, which for video
            // is usually the last frame.
            demux.flush();
        } else {
            demux.feed(&buf[..n])?;
        }
        while let Some(ev) = demux.next_event() {
            let DemuxEvent::Sample {
                stream,
                pts,
                dts,
                payload:
                    SamplePayload::Video {
                        codec,
                        raw,
                        random_access_indicator,
                        ..
                    },
            } = ev
            else {
                continue;
            };
            let key = starts_decoding(&raw, codec, random_access_indicator);
            if !pick.admit(stream.pid, codec, key) {
                continue;
            }
            if shell.is_none() {
                let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
                prog.add_video(0x100, codec);
                let mut cfg = MuxerConfig::builder();
                cfg.add_program(prog.build());
                cfg.psi_interval_ms(10);
                let p = publisher.take().expect("the publisher is taken once");
                shell = Some(MuxPublisher::with_config(p, cfg.build()?)?);
            }
            let Some(shell) = shell.as_ref() else {
                unreachable!("built above");
            };
            // The demuxed unit is already Annex-B: push it as it came.
            match dts {
                Some(dts) if dts != pts => shell.push_video_with_dts(&raw, pts, dts, key)?,
                _ => shell.push_video(&raw, pts, key)?,
            }
            pushed += 1;
        }
        if n == 0 {
            break;
        }
    }

    let Some(shell) = shell else {
        return Err(match (pick.pid, pick.saw_av1) {
            (Some(pid), _) => format!("{input} carries no keyframe on PID {pid:#x}"),
            (None, true) => "mux_to_hls re-muxes H.264, H.265 or H.266 video, not AV1".to_string(),
            (None, false) => format!("{input} carries no video stream"),
        }
        .into());
    };
    let stats = shell.stats();
    eprintln!(
        "re-muxed {pushed} access units ({} bytes through {} drain calls)",
        stats.bytes_pushed, stats.drain_calls
    );

    // `finish_serving` cuts the last segment, writes the ended playlist and
    // keeps the HTTP server up: a player can fetch the whole stream after
    // the input is done. The handle stops serving when it is dropped.
    let server = shell.finish()?.finish_serving()?;
    eprintln!(
        "serving http://{}/playlist.m3u8; stop with Ctrl-C",
        server.local_addr()
    );
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admit_skips_aus_before_the_first_keyframe() {
        let mut p = VideoPick::default();
        let got: Vec<bool> = [false, false, true, false]
            .into_iter()
            .map(|k| p.admit(0x100, VideoCodec::H264, k))
            .collect();
        assert_eq!(got, [false, false, true, true]);
    }

    #[test]
    fn admit_pushes_a_leading_keyframe() {
        let mut p = VideoPick::default();
        assert!(p.admit(0x100, VideoCodec::H264, true));
    }

    #[test]
    fn admit_ignores_av1_on_another_pid() {
        let mut p = VideoPick::default();
        assert!(!p.admit(0x200, VideoCodec::Av1, true));
        assert!(p.admit(0x100, VideoCodec::H264, true));
        assert!(p.saw_av1);
    }

    #[test]
    fn admit_skips_other_pids_after_selection() {
        let mut p = VideoPick::default();
        assert!(p.admit(0x100, VideoCodec::H264, true));
        assert!(!p.admit(0x101, VideoCodec::H265, true));
    }

    #[test]
    fn an_av1_only_input_selects_no_pid() {
        let mut p = VideoPick::default();
        assert!(!p.admit(0x200, VideoCodec::Av1, true));
        assert_eq!(p.pid, None);
        assert!(p.saw_av1);
    }
}
