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
//!   re-muxed from its first keyframe on, which also gets any parameter
//!   sets it lacks from the units skipped before it; AV1 is skipped, and
//!   fatal only when it is the only video; KLV, audio and any other stream
//!   are left out (configure a KLV stream and call `MuxPublisher::push_klv`
//!   to carry KLV).
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

use std::collections::BTreeMap;
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

/// What tells two parameter sets apart: the NAL type, then the set's id (or,
/// when its id does not parse, its bytes). A stream may carry several sets of
/// one type, such as PPS 0 and PPS 1, and a slice names the one it uses.
/// The NAL types rise VPS < SPS < PPS < APS in all three codecs, which is the
/// order a decoder needs them in: each set is read against the one before.
type SetKey = (u8, Result<u8, Vec<u8>>);

/// The key of `nal` when it is a parameter set a keyframe may depend on:
/// H.264 SPS 7 and PPS 8; H.265 VPS 32, SPS 33 and PPS 34; H.266 VPS 14,
/// SPS 15, PPS 16 and prefix APS 17. `None` for any other NAL. The ids come
/// from tst-core's own parameter-set parsers.
fn set_key(nal: &NalUnit) -> Option<SetKey> {
    use tst_core::codec::{h264, h265, h266};
    let (id, payload) = match nal {
        NalUnit::H264 {
            nal_type, payload, ..
        } => match nal_type {
            7 => (
                h264::parse_sps(payload)
                    .ok()
                    .map(|s| s.seq_parameter_set_id),
                payload,
            ),
            8 => (
                h264::parse_pps(payload)
                    .ok()
                    .map(|p| p.pic_parameter_set_id),
                payload,
            ),
            _ => return None,
        },
        NalUnit::H265 {
            nal_type, payload, ..
        } => match nal_type {
            32 => (
                h265::parse_vps(payload)
                    .ok()
                    .map(|v| v.vps_video_parameter_set_id),
                payload,
            ),
            33 => (
                h265::parse_sps(payload)
                    .ok()
                    .map(|s| s.sps_seq_parameter_set_id),
                payload,
            ),
            34 => (
                h265::parse_pps(payload)
                    .ok()
                    .map(|p| p.pps_pic_parameter_set_id),
                payload,
            ),
            _ => return None,
        },
        NalUnit::H266 {
            nal_type, payload, ..
        } => match nal_type {
            14 => (h266::parse_vps(payload).ok().map(|v| v.vps_id), payload),
            15 => (h266::parse_sps(payload).ok().map(|s| s.sps_id), payload),
            16 => (h266::parse_pps(payload).ok().map(|p| p.pps_id), payload),
            // An APS id is scoped by its kind (ALF, LMCS, scaling list), and
            // the first byte holds both: aps_params_type u(3), then the id u(5).
            17 => (payload.first().copied(), payload),
            _ => return None,
        },
    };
    Some((nal_type(nal), id.ok_or_else(|| payload.to_vec())))
}

/// Whether `nal` is an access unit delimiter (H.264 type 9, H.265 35, H.266
/// 20). When present it must stay the first NAL of its unit.
fn is_delimiter(nal: &NalUnit) -> bool {
    matches!(
        nal,
        NalUnit::H264 { nal_type: 9, .. }
            | NalUnit::H265 { nal_type: 35, .. }
            | NalUnit::H266 { nal_type: 20, .. }
    )
}

fn nal_type(nal: &NalUnit) -> u8 {
    match nal {
        NalUnit::H264 { nal_type, .. }
        | NalUnit::H265 { nal_type, .. }
        | NalUnit::H266 { nal_type, .. } => *nal_type,
    }
}

/// One NAL unit as Annex-B bytes: a 4-byte start code, the header rebuilt
/// from its parsed fields, then the body. The splitter leaves the header out
/// of `payload`; the bits it does not keep (forbidden and reserved) are zero
/// in any unit a decoder accepts.
fn annex_b_nal(nal: &NalUnit) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1];
    let payload = match nal {
        NalUnit::H264 {
            nal_type,
            ref_idc,
            payload,
        } => {
            out.push((ref_idc << 5) | nal_type);
            payload
        }
        NalUnit::H265 {
            nal_type,
            layer_id,
            temporal_id_plus1,
            payload,
        } => {
            out.push((nal_type << 1) | (layer_id >> 5));
            out.push(((layer_id & 0x1f) << 3) | temporal_id_plus1);
            payload
        }
        NalUnit::H266 {
            nal_type,
            layer_id,
            temporal_id_plus1,
            payload,
        } => {
            out.push(*layer_id);
            out.push((nal_type << 3) | temporal_id_plus1);
            payload
        }
    };
    out.extend_from_slice(payload);
    out
}

/// The NAL units of an Annex-B access unit, split the way
/// [`starts_decoding`] splits them.
fn nals(raw: &tst_core::shared::SharedBytes, codec: VideoCodec) -> Vec<NalUnit> {
    match split_video(raw, codec, Default::default()).0 {
        VideoPayload::Nals(nals) => nals,
        _ => Vec::new(),
    }
}

/// Which access units of the input are re-muxed. The first non-AV1 video PID
/// seen is the one re-muxed; its units are admitted from the first keyframe
/// on, so segment 0 opens on a unit a decoder can start from.
#[derive(Default)]
struct VideoPick {
    pid: Option<u16>,
    started: bool,
    saw_av1: bool,
    /// The latest copy of each parameter set seen in a skipped unit of the
    /// selected PID, as Annex-B bytes.
    params: BTreeMap<SetKey, Vec<u8>>,
}

impl VideoPick {
    /// Keep the parameter sets of a unit that is skipped. An encoder may send
    /// its parameter sets once, ahead of a delta frame, and not repeat them
    /// at the next keyframe. Dropping that unit whole would leave segment 0
    /// undecodable until the sets come round again: a decoder cannot start
    /// an IDR without the SPS and PPS it refers to.
    fn remember_params(&mut self, codec: VideoCodec, raw: &tst_core::shared::SharedBytes) {
        for nal in nals(raw, codec) {
            if let Some(key) = set_key(&nal) {
                self.params.insert(key, annex_b_nal(&nal));
            }
        }
    }

    /// The bytes to push for the first admitted keyframe when a skipped unit
    /// carried a parameter set the keyframe does not: the unit's delimiter if
    /// it has one, then every parameter set, the keyframe's own and the kept
    /// ones it lacks, in dependency order, then the rest of the unit. `None`
    /// means push the unit as it came. The kept sets are used up here, so
    /// every later call returns `None`.
    fn prefix_for_keyframe(
        &mut self,
        codec: VideoCodec,
        raw: &tst_core::shared::SharedBytes,
    ) -> Option<Vec<u8>> {
        if self.params.is_empty() {
            return None;
        }
        let mut sets = std::mem::take(&mut self.params);
        let nals = nals(raw, codec);
        let keys: Vec<Option<SetKey>> = nals.iter().map(set_key).collect();
        // A set the keyframe carries itself replaces the kept copy: it is the
        // newer one.
        sets.retain(|k, _| !keys.iter().flatten().any(|own| own == k));
        if sets.is_empty() {
            return None;
        }
        for (nal, key) in nals.iter().zip(&keys) {
            if let Some(key) = key {
                sets.insert(key.clone(), annex_b_nal(nal));
            }
        }
        // A PPS read before the SPS it refers to is discarded, so the sets go
        // out in key order (VPS, SPS, PPS, APS) after the delimiter, which
        // must stay the unit's first NAL.
        let lead = nals.iter().take_while(|n| is_delimiter(n)).count();
        let mut out = Vec::new();
        for nal in &nals[..lead] {
            out.extend(annex_b_nal(nal));
        }
        for bytes in sets.values() {
            out.extend_from_slice(bytes);
        }
        for (nal, key) in nals.iter().zip(&keys).skip(lead) {
            if key.is_none() {
                out.extend(annex_b_nal(nal));
            }
        }
        Some(out)
    }
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
                // A unit skipped before the first keyframe may carry the
                // parameter sets that keyframe needs.
                if pick.pid == Some(stream.pid) {
                    pick.remember_params(codec, &raw);
                }
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
            // The demuxed unit is already Annex-B: push it as it came, except
            // for a first keyframe that needs parameter sets from the units
            // skipped before it.
            let prefixed = pick.prefix_for_keyframe(codec, &raw);
            let au: &[u8] = prefixed.as_deref().unwrap_or(&raw);
            match dts {
                Some(dts) if dts != pts => shell.push_video_with_dts(au, pts, dts, key)?,
                _ => shell.push_video(au, pts, key)?,
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

    const SC: [u8; 4] = [0, 0, 0, 1];

    /// An Annex-B access unit: each NAL behind a 4-byte start code.
    fn annex_b(nals: &[&[u8]]) -> tst_core::shared::SharedBytes {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&SC);
            out.extend_from_slice(nal);
        }
        tst_core::shared::SharedBytes::from_vec(out)
    }

    /// A NAL unit: its header bytes, then a body.
    fn nal(header: &[u8], body: &[u8]) -> Vec<u8> {
        [header, body].concat()
    }

    // Parameter-set bodies captured from real encoders (tst-core's codec
    // fixtures); the slice bodies are short stand-ins.
    const H264_SPS: &[u8] =
        include_bytes!("../../crates/tst-core/tests/fixtures/codec/h264/h264_720p_main31_sps.bin");
    const H264_PPS: &[u8] =
        include_bytes!("../../crates/tst-core/tests/fixtures/codec/h264/h264_720p_main31_pps.bin");
    const H265_VPS: &[u8] =
        include_bytes!("../../crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main40_vps.bin");
    const H265_SPS: &[u8] =
        include_bytes!("../../crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main40_sps.bin");
    const H265_PPS: &[u8] =
        include_bytes!("../../crates/tst-core/tests/fixtures/codec/h265/h265_1080p_main40_pps.bin");

    #[test]
    fn skipped_param_sets_are_prepended_to_the_first_keyframe() {
        let sps = nal(&[0x67], H264_SPS);
        let pps = nal(&[0x68], H264_PPS);
        let p_slice = nal(&[0x41], &[0x9a, 0x02, 0x0c]);
        let idr = nal(&[0x65], &[0x88, 0x84, 0x00, 0x33]);
        let mut p = VideoPick::default();
        let lead = annex_b(&[&sps, &pps, &p_slice]);
        assert!(!p.admit(
            0x100,
            VideoCodec::H264,
            starts_decoding(&lead, VideoCodec::H264, false)
        ));
        p.remember_params(VideoCodec::H264, &lead);
        let key = annex_b(&[&idr]);
        assert!(p.admit(
            0x100,
            VideoCodec::H264,
            starts_decoding(&key, VideoCodec::H264, false)
        ));
        let pushed = p.prefix_for_keyframe(VideoCodec::H264, &key);
        assert_eq!(pushed.as_deref(), Some(&annex_b(&[&sps, &pps, &idr])[..]));
        // Only the first keyframe is prefixed.
        assert_eq!(p.prefix_for_keyframe(VideoCodec::H264, &key), None);
    }

    #[test]
    fn keyframe_with_its_own_param_sets_is_unchanged() {
        let sps = nal(&[0x67], H264_SPS);
        let pps = nal(&[0x68], H264_PPS);
        let p_slice = nal(&[0x41], &[0x9a, 0x02, 0x0c]);
        let idr = nal(&[0x65], &[0x88, 0x84, 0x00, 0x33]);
        let mut p = VideoPick::default();
        p.remember_params(VideoCodec::H264, &annex_b(&[&sps, &pps, &p_slice]));
        // The keyframe repeats both sets: it is pushed as it came, so `None`.
        let key = annex_b(&[&sps, &pps, &idr]);
        assert_eq!(p.prefix_for_keyframe(VideoCodec::H264, &key), None);
    }

    #[test]
    fn h265_sets_go_after_the_access_unit_delimiter() {
        let aud = nal(&[0x46, 0x01], &[0x50]);
        let vps = nal(&[0x40, 0x01], H265_VPS);
        let sps = nal(&[0x42, 0x01], H265_SPS);
        let pps = nal(&[0x44, 0x01], H265_PPS);
        let trail = nal(&[0x02, 0x01], &[0xd0, 0x2c]);
        let idr = nal(&[0x26, 0x01], &[0xaf, 0x0c, 0x41]);
        let mut p = VideoPick::default();
        let lead = annex_b(&[&aud, &vps, &sps, &pps, &trail]);
        assert!(!starts_decoding(&lead, VideoCodec::H265, false));
        p.remember_params(VideoCodec::H265, &lead);
        // The keyframe carries its own SPS only: the kept VPS and PPS are
        // added around it in VPS, SPS, PPS order, and the delimiter stays the
        // unit's first NAL.
        let key = annex_b(&[&aud, &sps, &idr]);
        assert!(starts_decoding(&key, VideoCodec::H265, false));
        let pushed = p.prefix_for_keyframe(VideoCodec::H265, &key);
        assert_eq!(
            pushed.as_deref(),
            Some(&annex_b(&[&aud, &vps, &sps, &pps, &idr])[..])
        );
    }

    #[test]
    fn a_kept_pps_goes_after_the_keyframes_own_sps() {
        let sps = nal(&[0x67], H264_SPS);
        let pps = nal(&[0x68], H264_PPS);
        let p_slice = nal(&[0x41], &[0x9a, 0x02, 0x0c]);
        let idr = nal(&[0x65], &[0x88, 0x84, 0x00, 0x33]);
        let mut p = VideoPick::default();
        p.remember_params(VideoCodec::H264, &annex_b(&[&sps, &pps, &p_slice]));
        let key = annex_b(&[&sps, &idr]);
        let pushed = p.prefix_for_keyframe(VideoCodec::H264, &key);
        assert_eq!(pushed.as_deref(), Some(&annex_b(&[&sps, &pps, &idr])[..]));
    }

    #[test]
    fn every_kept_pps_id_is_carried() {
        use tst_core::codec::h264::parse_pps;
        // The fixture PPS with its id rewritten from 0 to 1.
        let pps1_body = [0x5b, 0x83, 0xcb, 0x20];
        assert_eq!(parse_pps(H264_PPS).unwrap().pic_parameter_set_id, 0);
        assert_eq!(parse_pps(&pps1_body).unwrap().pic_parameter_set_id, 1);
        let sps = nal(&[0x67], H264_SPS);
        let pps0 = nal(&[0x68], H264_PPS);
        let pps1 = nal(&[0x68], &pps1_body);
        let p_slice = nal(&[0x41], &[0x9a, 0x02, 0x0c]);
        let idr = nal(&[0x65], &[0x88, 0x84, 0x00, 0x33]);
        let mut p = VideoPick::default();
        p.remember_params(VideoCodec::H264, &annex_b(&[&sps, &pps0, &p_slice]));
        p.remember_params(VideoCodec::H264, &annex_b(&[&pps1, &p_slice]));
        let key = annex_b(&[&idr]);
        let pushed = p.prefix_for_keyframe(VideoCodec::H264, &key);
        assert_eq!(
            pushed.as_deref(),
            Some(&annex_b(&[&sps, &pps0, &pps1, &idr])[..])
        );
        // A keyframe carrying PPS 1 still lacks PPS 0.
        let mut p = VideoPick::default();
        p.remember_params(VideoCodec::H264, &annex_b(&[&sps, &pps0, &p_slice]));
        let key = annex_b(&[&sps, &pps1, &idr]);
        let pushed = p.prefix_for_keyframe(VideoCodec::H264, &key);
        assert_eq!(
            pushed.as_deref(),
            Some(&annex_b(&[&sps, &pps0, &pps1, &idr])[..])
        );
    }

    #[test]
    fn an_av1_only_input_selects_no_pid() {
        let mut p = VideoPick::default();
        assert!(!p.admit(0x200, VideoCodec::Av1, true));
        assert_eq!(p.pid, None);
        assert!(p.saw_av1);
    }
}
