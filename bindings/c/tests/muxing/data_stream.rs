//! Data-stream (`StreamSpec::Data` PES pass-through) surface via the
//! C ABI: the config side (`tst_mux_config_add_data_stream` +
//! `tst_mux_config_set_stream_descriptors_for_data` +
//! `tst_mux_config_add_data_descriptor`) and the push side
//! (`tst_muxer_push_data` / `tst_muxer_push_data_to`), including the
//! offline mux→demux round trip through the `tst_demuxer_*` surface.
//! The offline `tst_mux_config_*` / `tst_muxer_*` / `tst_demuxer_*`
//! surface is unconditional, so this module carries no feature gate
//! (matching `demuxer_offline.rs`).

use std::ffi::CStr;

use tstrans::config::{
    TstKlvStreamType, TstMuxConfig, TstProgramHandle, TstVideoCodec,
    tst_mux_config_add_data_descriptor, tst_mux_config_add_data_stream,
    tst_mux_config_add_klv_stream, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new, tst_mux_config_set_stream_descriptors_for_data,
};
use tstrans::demuxer::{
    tst_demuxer_close, tst_demuxer_feed, tst_demuxer_flush, tst_demuxer_next_event,
    tst_demuxer_open,
};
use tstrans::error::{TstError, tst_get_last_error, tst_get_last_error_str};
use tstrans::event::{TstDescriptor, TstEvent, TstEventKind, TstMetadataKindTag, TstStreamKindTag};
use tstrans::handle::TST_INVALID_STREAM_HANDLE;
use tstrans::muxer::{
    TstMuxer, tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_data,
    tst_muxer_push_data_to, tst_muxer_push_klv_to, tst_muxer_push_video_to,
};

const NAL_SPS: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1e, 0xda, 0x02, 0x80, 0xf6, 0xc0,
];

/// Open the config, push one video frame (forces PSI emission alongside the
/// payload), pull the TS output, and close. The config survives — `_open`
/// clones the inner — so callers can mutate + reopen the same `cfg`.
unsafe fn open_push_pull(cfg: *mut TstMuxConfig, h_video: u32) -> Vec<u8> {
    unsafe {
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null(), "tst_muxer_open failed");
        let rc = tst_muxer_push_video_to(mux, h_video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, 0, "push_video_to failed");
        let mut buf = vec![0u8; 64 * 188];
        let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
        assert!(n > 0, "muxer produced no output");
        buf.truncate(n);
        tst_muxer_close(mux);
        buf
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ----------------------------------------------------------------------------
// Constructor — accept paths
// ----------------------------------------------------------------------------

#[test]
fn add_data_stream_returns_distinct_valid_handles() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h0 = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        let h1 = tst_mux_config_add_data_stream(cfg, prog, 0x1042, 0xF1, false);
        assert_ne!(h0, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h1, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h0, h1, "two data streams must get distinct handles");
        tst_mux_config_free(cfg);
    }
}

// ----------------------------------------------------------------------------
// Constructor — reject paths
// ----------------------------------------------------------------------------

#[test]
fn add_data_stream_null_cfg_returns_sentinel() {
    unsafe {
        let h = tst_mux_config_add_data_stream(
            core::ptr::null_mut(),
            TstProgramHandle(0),
            0x1041,
            0xF0,
            true,
        );
        assert_eq!(h, TST_INVALID_STREAM_HANDLE);
        assert_eq!(tst_get_last_error(), TstError::InvalidConfig as i32);
    }
}

#[test]
fn add_data_stream_invalid_program_returns_sentinel() {
    unsafe {
        let cfg = tst_mux_config_new();
        // No programs added — TstProgramHandle(0) is invalid.
        let h = tst_mux_config_add_data_stream(cfg, TstProgramHandle(0), 0x1041, 0xF0, true);
        assert_eq!(h, TST_INVALID_STREAM_HANDLE);
        assert_eq!(tst_get_last_error(), TstError::InvalidUsage as i32);
        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_data_stream_17th_exceeds_per_program_cap() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        for i in 0..16u16 {
            let h = tst_mux_config_add_data_stream(cfg, prog, 0x1100 + i, 0xF0, true);
            assert_ne!(
                h, TST_INVALID_STREAM_HANDLE,
                "stream {i} should be accepted"
            );
        }
        let h = tst_mux_config_add_data_stream(cfg, prog, 0x1110, 0xF0, true);
        assert_eq!(
            h, TST_INVALID_STREAM_HANDLE,
            "17th data stream must be rejected"
        );
        assert_eq!(tst_get_last_error(), TstError::InvalidUsage as i32);
        tst_mux_config_free(cfg);
    }
}

// ----------------------------------------------------------------------------
// Descriptors — set / clear / add round-trip (observed through PMT bytes)
// ----------------------------------------------------------------------------

#[test]
fn set_stream_descriptors_for_data_set_then_clear_roundtrip() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        assert_ne!(h_video, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);

        // One user-private descriptor TLV (tag 0xA0, 4-byte body). A
        // user-private tag never trips the validate-time classify-Unknown
        // rule on data streams.
        let tlv: &[u8] = &[0xA0, 0x04, 0xDE, 0xAD, 0xBE, 0xEF];
        let rc =
            tst_mux_config_set_stream_descriptors_for_data(cfg, h_data, tlv.as_ptr(), tlv.len(), 1);
        assert_eq!(rc, 0);
        let ts = open_push_pull(cfg, h_video);
        assert!(
            contains(&ts, tlv),
            "PMT must carry the data-stream descriptor TLV"
        );

        // Clearing (len 0 / count 0) removes the descriptor on reopen.
        let rc =
            tst_mux_config_set_stream_descriptors_for_data(cfg, h_data, core::ptr::null(), 0, 0);
        assert_eq!(rc, 0);
        let ts = open_push_pull(cfg, h_video);
        assert!(
            !contains(&ts, tlv),
            "cleared descriptor must not appear in PMT"
        );

        tst_mux_config_free(cfg);
    }
}

#[test]
fn add_data_descriptor_accumulates() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);

        let body_a: &[u8] = &[0x01, 0x02, 0x03];
        let desc_a = TstDescriptor {
            tag: 0xA1,
            _reserved: [0; 7],
            data: body_a.as_ptr(),
            data_len: body_a.len(),
        };
        let body_b: &[u8] = &[0x44];
        let desc_b = TstDescriptor {
            tag: 0xA2,
            _reserved: [0; 7],
            data: body_b.as_ptr(),
            data_len: body_b.len(),
        };
        assert_eq!(tst_mux_config_add_data_descriptor(cfg, h_data, &desc_a), 0);
        assert_eq!(tst_mux_config_add_data_descriptor(cfg, h_data, &desc_b), 0);

        let ts = open_push_pull(cfg, h_video);
        assert!(
            contains(&ts, &[0xA1, 0x03, 0x01, 0x02, 0x03]),
            "first added descriptor must appear in PMT"
        );
        assert!(
            contains(&ts, &[0xA2, 0x01, 0x44]),
            "second added descriptor must accumulate, not replace"
        );

        tst_mux_config_free(cfg);
    }
}

// ----------------------------------------------------------------------------
// Descriptors — forged-handle rejection (trust-boundary validation)
// ----------------------------------------------------------------------------

#[test]
fn descriptor_functions_reject_forged_high_bit_handle() {
    // Same threat model as `muxer_push_video_to_forged_high_bit_handle_*`
    // in multi_stream.rs: a raw handle with bits set above the canonical
    // 8-bit packed layout must be rejected — not silently aliased onto the
    // genuine stream.
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);
        let forged = h_data | 0x100;

        let tlv: &[u8] = &[0xA0, 0x01, 0x55];
        let rc =
            tst_mux_config_set_stream_descriptors_for_data(cfg, forged, tlv.as_ptr(), tlv.len(), 1);
        assert_eq!(rc, TstError::InvalidUsage as i32);

        let body: &[u8] = &[0x55];
        let desc = TstDescriptor {
            tag: 0xA0,
            _reserved: [0; 7],
            data: body.as_ptr(),
            data_len: body.len(),
        };
        let rc = tst_mux_config_add_data_descriptor(cfg, forged, &desc);
        assert_eq!(rc, TstError::InvalidUsage as i32);

        tst_mux_config_free(cfg);
    }
}

// ----------------------------------------------------------------------------
// Push surface — tst_muxer_push_data / tst_muxer_push_data_to
// ----------------------------------------------------------------------------

/// Snapshot of the last-error string (arena-independent copy).
unsafe fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// Drain all queued TS bytes from the muxer (loops until `pull` returns 0).
unsafe fn pull_all(mux: *mut TstMuxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 256 * 188];
    loop {
        let n = unsafe { tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len()) };
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out.len() % 188, 0, "TS output not a multiple of 188");
    out
}

/// PMT stream entry snapshot (descriptors copied out of the event arena).
struct StreamEntry {
    pid: u16,
    stream_type: u8,
    stream_kind: i32,
    descriptors: Vec<(u8, Vec<u8>)>,
}

/// Sample snapshot (payload copied out of the event arena — event pointer
/// fields are only valid until the next `tst_demuxer_next_event` call).
struct SampleRec {
    pid: u16,
    stream_kind: i32,
    stream_type: u8,
    pts: i64,
    payload: Vec<u8>,
}

/// Metadata-event snapshot (KLV streams surface as `TstEventKind::Metadata`,
/// not Sample; payload copied out of the event arena like `SampleRec`).
struct MetadataRec {
    pid: u16,
    metadata_kind: i32,
    pts: i64,
    payload: Vec<u8>,
}

/// Feed `ts` through the offline C demuxer and snapshot every ProgramMap
/// stream entry + every Sample event + every Metadata event.
unsafe fn demux_collect(ts: &[u8]) -> (Vec<StreamEntry>, Vec<SampleRec>, Vec<MetadataRec>) {
    unsafe {
        let d = tst_demuxer_open();
        assert!(!d.is_null(), "tst_demuxer_open returned null");
        let rc = tst_demuxer_feed(d, ts.as_ptr(), ts.len());
        assert_eq!(rc, 0, "tst_demuxer_feed failed: {rc}");
        let rc = tst_demuxer_flush(d);
        assert_eq!(rc, 0, "tst_demuxer_flush failed: {rc}");

        let mut streams = Vec::new();
        let mut samples = Vec::new();
        let mut metadata = Vec::new();
        loop {
            let mut ev = TstEvent::default();
            let rc = tst_demuxer_next_event(d, &mut ev);
            if rc == TstError::NotAvailable as i32 {
                break;
            }
            assert_eq!(rc, 0, "tst_demuxer_next_event failed: {rc}");
            if ev.kind == TstEventKind::ProgramMap as i32 {
                let pm = ev.u.program_map;
                for i in 0..pm.stream_count {
                    let s = *pm.streams.add(i);
                    let mut descriptors = Vec::new();
                    for j in 0..s.descriptor_count {
                        let desc = *s.raw_descriptors.add(j);
                        let body = core::slice::from_raw_parts(desc.data, desc.data_len).to_vec();
                        descriptors.push((desc.tag, body));
                    }
                    streams.push(StreamEntry {
                        pid: s.pid,
                        stream_type: s.stream_type,
                        stream_kind: s.stream_kind,
                        descriptors,
                    });
                }
            } else if ev.kind == TstEventKind::Sample as i32 {
                let s = ev.u.sample;
                samples.push(SampleRec {
                    pid: s.pid,
                    stream_kind: s.stream_kind,
                    stream_type: s.stream_type,
                    pts: s.pts,
                    payload: core::slice::from_raw_parts(s.payload, s.payload_len).to_vec(),
                });
            } else if ev.kind == TstEventKind::Metadata as i32 {
                let m = ev.u.metadata;
                metadata.push(MetadataRec {
                    pid: m.pid,
                    metadata_kind: m.metadata_kind,
                    pts: m.pts,
                    payload: core::slice::from_raw_parts(m.payload, m.payload_len).to_vec(),
                });
            }
        }
        tst_demuxer_close(d);
        (streams, samples, metadata)
    }
}

/// The wave's flagship test: video + two data streams (0xF0 with one
/// user-private descriptor; bare 0x06 with none) muxed offline, then fed
/// back through the offline C demuxer. Both streams must surface as
/// Unknown in the ProgramMap with exact stream_type + descriptor TLV
/// bytes, and every pushed payload must arrive on its PID byte-identical
/// with the pushed PTS.
#[test]
fn offline_round_trip_two_data_streams() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_a = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        let h_b = tst_mux_config_add_data_stream(cfg, prog, 0x1042, 0x06, true);
        assert_ne!(h_a, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h_b, TST_INVALID_STREAM_HANDLE);

        // Stream A gets one user-private descriptor (tag 0xFF, 2-byte body).
        let desc_body: &[u8] = &[0x10, 0x20];
        let desc = TstDescriptor {
            tag: 0xFF,
            _reserved: [0; 7],
            data: desc_body.as_ptr(),
            data_len: desc_body.len(),
        };
        assert_eq!(tst_mux_config_add_data_descriptor(cfg, h_a, &desc), 0);

        let mux = tst_muxer_open(cfg);
        assert!(
            !mux.is_null(),
            "tst_muxer_open failed: {}",
            last_error_msg()
        );
        tst_mux_config_free(cfg);

        // Video first — forces PSI emission alongside the data payloads.
        let rc = tst_muxer_push_video_to(mux, h_video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, 0, "push_video_to failed: {}", last_error_msg());

        // Distinct payloads + PTS per stream, interleaved in PTS order.
        let a_pushes: &[(&[u8], i64)] = &[
            (b"alpha-1", 3000),
            (b"alpha-two", 6000),
            (b"alpha-payload-3", 9000),
        ];
        let b_pushes: &[(&[u8], i64)] = &[(b"bravo-1", 4500), (b"bravo-second", 7500)];
        let pushes: &[(u32, &[u8], i64)] = &[
            (h_a, a_pushes[0].0, a_pushes[0].1),
            (h_b, b_pushes[0].0, b_pushes[0].1),
            (h_a, a_pushes[1].0, a_pushes[1].1),
            (h_b, b_pushes[1].0, b_pushes[1].1),
            (h_a, a_pushes[2].0, a_pushes[2].1),
        ];
        for &(h, payload, pts) in pushes {
            let rc = tst_muxer_push_data_to(mux, h, payload.as_ptr(), payload.len(), pts);
            assert_eq!(rc, 0, "push_data_to failed: {}", last_error_msg());
        }

        let ts = pull_all(mux);
        tst_muxer_close(mux);

        let (streams, samples, _metadata) = demux_collect(&ts);

        // ProgramMap: both data streams surface as Unknown with exact
        // stream_type bytes and descriptor TLVs.
        let a = streams
            .iter()
            .find(|s| s.pid == 0x1041)
            .expect("PMT must list data stream A (pid 0x1041)");
        assert_eq!(a.stream_kind, TstStreamKindTag::Unknown as i32);
        assert_eq!(a.stream_type, 0xF0);
        assert_eq!(
            a.descriptors,
            vec![(0xFF, vec![0x10, 0x20])],
            "stream A must carry exactly the one 0xFF descriptor"
        );
        let b = streams
            .iter()
            .find(|s| s.pid == 0x1042)
            .expect("PMT must list data stream B (pid 0x1042)");
        assert_eq!(b.stream_kind, TstStreamKindTag::Unknown as i32);
        assert_eq!(b.stream_type, 0x06);
        assert!(
            b.descriptors.is_empty(),
            "stream B was configured with no descriptors"
        );

        // Samples: every push arrives on the right PID, byte-identical,
        // with the pushed PTS, in push order.
        let a_samples: Vec<&SampleRec> = samples.iter().filter(|s| s.pid == 0x1041).collect();
        let b_samples: Vec<&SampleRec> = samples.iter().filter(|s| s.pid == 0x1042).collect();
        assert_eq!(a_samples.len(), a_pushes.len(), "stream A sample count");
        assert_eq!(b_samples.len(), b_pushes.len(), "stream B sample count");
        for (got, &(payload, pts)) in a_samples.iter().zip(a_pushes) {
            assert_eq!(got.stream_kind, TstStreamKindTag::Unknown as i32);
            assert_eq!(got.stream_type, 0xF0);
            assert_eq!(got.payload, payload, "stream A payload must round-trip");
            assert_eq!(got.pts, pts, "stream A pts must round-trip");
        }
        for (got, &(payload, pts)) in b_samples.iter().zip(b_pushes) {
            assert_eq!(got.stream_kind, TstStreamKindTag::Unknown as i32);
            assert_eq!(got.stream_type, 0x06);
            assert_eq!(got.payload, payload, "stream B payload must round-trip");
            assert_eq!(got.pts, pts, "stream B pts must round-trip");
        }
    }
}

/// Single-stream happy path: exactly one data stream → the no-handle
/// `tst_muxer_push_data` resolves it and the payload round-trips on the
/// configured PID.
#[test]
fn push_data_single_stream_routes_without_handle() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);
        let mux = tst_muxer_open(cfg);
        assert!(
            !mux.is_null(),
            "tst_muxer_open failed: {}",
            last_error_msg()
        );
        tst_mux_config_free(cfg);

        let rc = tst_muxer_push_video_to(mux, h_video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, 0);

        let payload: &[u8] = b"solo-data-payload";
        let rc = tst_muxer_push_data(mux, payload.as_ptr(), payload.len(), 1234);
        assert_eq!(rc, 0, "push_data failed: {}", last_error_msg());

        let ts = pull_all(mux);
        tst_muxer_close(mux);

        let (_streams, samples, _metadata) = demux_collect(&ts);
        let data_samples: Vec<&SampleRec> = samples.iter().filter(|s| s.pid == 0x1041).collect();
        assert_eq!(data_samples.len(), 1, "expected exactly one data sample");
        assert_eq!(
            data_samples[0].stream_kind,
            TstStreamKindTag::Unknown as i32
        );
        assert_eq!(data_samples[0].payload, payload);
        assert_eq!(data_samples[0].pts, 1234);
    }
}

// ----------------------------------------------------------------------------
// Push surface — error matrix
// ----------------------------------------------------------------------------

#[test]
fn push_data_zero_data_streams_returns_invalid_usage() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null());
        tst_mux_config_free(cfg);

        let payload: &[u8] = b"nowhere-to-go";
        let rc = tst_muxer_push_data(mux, payload.as_ptr(), payload.len(), 0);
        // MuxError::NoDataStreamsConfigured → TST_E_INVALID_USAGE.
        assert_eq!(rc, TstError::InvalidUsage as i32);
        assert!(
            last_error_msg().contains("no data streams configured"),
            "expected NoDataStreamsConfigured detail, got: {}",
            last_error_msg()
        );

        tst_muxer_close(mux);
    }
}

#[test]
fn push_data_two_data_streams_returns_ambiguous_target() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        tst_mux_config_add_data_stream(cfg, prog, 0x1042, 0xF1, true);
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null());
        tst_mux_config_free(cfg);

        let payload: &[u8] = b"which-one";
        let rc = tst_muxer_push_data(mux, payload.as_ptr(), payload.len(), 0);
        // MuxError::AmbiguousTarget → TST_E_INVALID_USAGE.
        assert_eq!(rc, TstError::InvalidUsage as i32);
        assert!(
            last_error_msg().contains("ambiguous push"),
            "expected AmbiguousTarget detail, got: {}",
            last_error_msg()
        );

        tst_muxer_close(mux);
    }
}

#[test]
fn push_data_oversized_payload_returns_data_too_large() {
    unsafe {
        // carries_pts = true → PES overhead 8 bytes → ceiling 65527.
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null());
        tst_mux_config_free(cfg);

        let at_ceiling = vec![0xABu8; 65527];
        let rc = tst_muxer_push_data(mux, at_ceiling.as_ptr(), at_ceiling.len(), 3000);
        assert_eq!(rc, 0, "65527 bytes (with PTS) must be accepted");

        let over = vec![0xABu8; 65528];
        let rc = tst_muxer_push_data(mux, over.as_ptr(), over.len(), 6000);
        // WP-A2 K4/K6: MuxError::DataTooLarge is an `InputMalformed`-kind
        // variant, so it now projects to TST_E_INVALID_TS (-3) like the rest
        // of that bucket — the retired raw mapper answered -9 here while the
        // shell path already said -3.
        assert_eq!(rc, TstError::InvalidTs as i32);
        assert!(
            last_error_msg().contains("exceeds PES_packet_length"),
            "expected DataTooLarge detail, got: {}",
            last_error_msg()
        );
        tst_muxer_close(mux);

        // carries_pts = false → PES overhead 3 bytes → ceiling 65532.
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, false);
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null());
        tst_mux_config_free(cfg);

        let at_ceiling = vec![0xCDu8; 65532];
        let rc = tst_muxer_push_data(mux, at_ceiling.as_ptr(), at_ceiling.len(), 3000);
        assert_eq!(rc, 0, "65532 bytes (no PTS) must be accepted");

        let over = vec![0xCDu8; 65533];
        let rc = tst_muxer_push_data(mux, over.as_ptr(), over.len(), 6000);
        assert_eq!(rc, TstError::InvalidTs as i32);
        tst_muxer_close(mux);
    }
}

#[test]
fn push_data_to_forged_high_bit_handle_rejected() {
    // Same trust-boundary threat model as the descriptor-function test
    // above: bits set above the canonical 8-bit packed layout must be
    // rejected — not silently masked onto the genuine stream.
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);
        let mux = tst_muxer_open(cfg);
        assert!(!mux.is_null());
        tst_mux_config_free(cfg);

        let payload: &[u8] = b"forged";
        let forged = h_data | 0x100;
        let rc = tst_muxer_push_data_to(mux, forged, payload.as_ptr(), payload.len(), 0);
        // MuxError::InvalidStreamHandle → TST_E_INVALID_USAGE.
        assert_eq!(rc, TstError::InvalidUsage as i32);

        // The genuine handle still works on the same muxer.
        let rc = tst_muxer_push_data_to(mux, h_data, payload.as_ptr(), payload.len(), 0);
        assert_eq!(
            rc,
            0,
            "genuine handle must still push: {}",
            last_error_msg()
        );

        tst_muxer_close(mux);
    }
}

// ----------------------------------------------------------------------------
// Final-review folds
// ----------------------------------------------------------------------------

// Minimal 17-byte ST 0601 KLV blob: 16-byte UL + 1-byte BER length (0).
// Same fixture as multi_program.rs. Callers pass RAW KLV LS bytes — on a
// SynchronousMetadata stream the muxer auto-wraps them in a 5-byte
// Metadata_AU_cell header (H.222.0 §2.12.4.2), and the demuxer strips it
// again, so the round trip compares against these raw bytes.
const KLV_BLOB: &[u8] = &[
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00,
];

/// KLV + data coexistence: one config carrying video + sync-KLV + a data
/// stream, pushed interleaved, must round-trip with each stream surfacing
/// through its own event channel — KLV as typed `Metadata` events (AU cell
/// stripped, payload == the raw pushed KLV), data as `Unknown` samples
/// byte-identical, and the PMT typing the KLV stream while leaving the
/// data stream Unknown.
#[test]
fn offline_round_trip_klv_and_data_coexist() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_klv = tst_mux_config_add_klv_stream(
            cfg,
            prog,
            0x1031,
            TstKlvStreamType::SynchronousMetadata,
            true,
        );
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, true);
        assert_ne!(h_klv, TST_INVALID_STREAM_HANDLE);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);

        let mux = tst_muxer_open(cfg);
        assert!(
            !mux.is_null(),
            "tst_muxer_open failed: {}",
            last_error_msg()
        );
        tst_mux_config_free(cfg);

        let rc = tst_muxer_push_video_to(mux, h_video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, 0, "push_video_to failed: {}", last_error_msg());

        // Interleave klv + data pushes in PTS order.
        let data_pushes: &[(&[u8], i64)] = &[(b"data-1", 4500), (b"data-second", 7500)];
        let klv_pts: &[i64] = &[3000, 6000];
        let rc = tst_muxer_push_klv_to(mux, h_klv, KLV_BLOB.as_ptr(), KLV_BLOB.len(), klv_pts[0]);
        assert_eq!(rc, 0, "push_klv_to failed: {}", last_error_msg());
        let rc = tst_muxer_push_data_to(
            mux,
            h_data,
            data_pushes[0].0.as_ptr(),
            data_pushes[0].0.len(),
            data_pushes[0].1,
        );
        assert_eq!(rc, 0, "push_data_to failed: {}", last_error_msg());
        let rc = tst_muxer_push_klv_to(mux, h_klv, KLV_BLOB.as_ptr(), KLV_BLOB.len(), klv_pts[1]);
        assert_eq!(rc, 0, "push_klv_to failed: {}", last_error_msg());
        let rc = tst_muxer_push_data_to(
            mux,
            h_data,
            data_pushes[1].0.as_ptr(),
            data_pushes[1].0.len(),
            data_pushes[1].1,
        );
        assert_eq!(rc, 0, "push_data_to failed: {}", last_error_msg());

        let ts = pull_all(mux);
        tst_muxer_close(mux);

        let (streams, samples, metadata) = demux_collect(&ts);

        // PMT: KLV typed, data Unknown.
        let k = streams
            .iter()
            .find(|s| s.pid == 0x1031)
            .expect("PMT must list the KLV stream (pid 0x1031)");
        assert_eq!(k.stream_kind, TstStreamKindTag::KlvSync as i32);
        let d = streams
            .iter()
            .find(|s| s.pid == 0x1041)
            .expect("PMT must list the data stream (pid 0x1041)");
        assert_eq!(d.stream_kind, TstStreamKindTag::Unknown as i32);
        assert_eq!(d.stream_type, 0xF0);

        // Data samples: byte-identical on the right PID with the pushed PTS.
        let data_samples: Vec<&SampleRec> = samples.iter().filter(|s| s.pid == 0x1041).collect();
        assert_eq!(data_samples.len(), data_pushes.len(), "data sample count");
        for (got, &(payload, pts)) in data_samples.iter().zip(data_pushes) {
            assert_eq!(got.stream_kind, TstStreamKindTag::Unknown as i32);
            assert_eq!(got.payload, payload, "data payload must round-trip");
            assert_eq!(got.pts, pts, "data pts must round-trip");
        }

        // KLV: surfaces as Metadata events on its PID with the AU cell
        // stripped — payload == the raw KLV LS bytes we pushed.
        let klv_events: Vec<&MetadataRec> = metadata.iter().filter(|m| m.pid == 0x1031).collect();
        assert_eq!(klv_events.len(), klv_pts.len(), "KLV metadata event count");
        for (got, &pts) in klv_events.iter().zip(klv_pts) {
            assert_eq!(got.metadata_kind, TstMetadataKindTag::KlvSyncAuCell as i32);
            assert_eq!(
                got.payload, KLV_BLOB,
                "inner KLV must round-trip byte-for-byte (AU cell stripped)"
            );
            assert_eq!(got.pts, pts, "KLV pts must surface from the PES header");
        }
    }
}

/// `carries_pts = false` round trip: the pushed pts paces PSI/PCR emission
/// but is NOT written to the PES header, so the demuxed samples surface
/// the documented no-PTS substitute `pts == 0` with payloads intact.
#[test]
fn offline_round_trip_no_pts_data_stream_surfaces_pts_zero() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        let h_video = tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        let h_data = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0xF0, false);
        assert_ne!(h_data, TST_INVALID_STREAM_HANDLE);

        let mux = tst_muxer_open(cfg);
        assert!(
            !mux.is_null(),
            "tst_muxer_open failed: {}",
            last_error_msg()
        );
        tst_mux_config_free(cfg);

        let rc = tst_muxer_push_video_to(mux, h_video, NAL_SPS.as_ptr(), NAL_SPS.len(), 0, true);
        assert_eq!(rc, 0, "push_video_to failed: {}", last_error_msg());

        let payloads: &[&[u8]] = &[b"no-pts-1", b"no-pts-second"];
        for (i, payload) in payloads.iter().enumerate() {
            // Nonzero pts on purpose — must NOT surface on demux.
            let pts = 3000 * (i as i64 + 1);
            let rc = tst_muxer_push_data_to(mux, h_data, payload.as_ptr(), payload.len(), pts);
            assert_eq!(rc, 0, "push_data_to failed: {}", last_error_msg());
        }

        let ts = pull_all(mux);
        tst_muxer_close(mux);

        let (_streams, samples, _metadata) = demux_collect(&ts);
        let data_samples: Vec<&SampleRec> = samples.iter().filter(|s| s.pid == 0x1041).collect();
        assert_eq!(data_samples.len(), payloads.len(), "data sample count");
        for (got, payload) in data_samples.iter().zip(payloads) {
            assert_eq!(got.payload, *payload, "payload must round-trip");
            assert_eq!(
                got.pts, 0,
                "carries_pts=false samples must surface the no-PTS substitute 0"
            );
        }
    }
}

/// Anti-masquerade: a data stream declared with a TYPED stream_type byte
/// (0x1B = H.264) is accepted at add time but rejected at `tst_muxer_open`
/// — validation classifies it and demands the typed StreamSpec variant.
#[test]
fn open_rejects_data_stream_with_typed_stream_type() {
    unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        // 0x1B is the H.264 stream_type byte — add succeeds (validation is
        // deferred to open).
        let h = tst_mux_config_add_data_stream(cfg, prog, 0x1041, 0x1B, true);
        assert_ne!(h, TST_INVALID_STREAM_HANDLE, "add itself must succeed");

        let mux = tst_muxer_open(cfg);
        assert!(
            mux.is_null(),
            "open must reject the masquerading data stream"
        );
        // MuxError::ConfigInvalid → TST_E_INVALID_CONFIG.
        assert_eq!(tst_get_last_error(), TstError::InvalidConfig as i32);
        let msg = last_error_msg();
        assert!(
            msg.contains("classifies as") && msg.contains("Video"),
            "error must name the offending classification, got: {msg}"
        );

        tst_mux_config_free(cfg);
    }
}
