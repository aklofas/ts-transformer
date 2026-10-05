//! Pins multi-program demux event identity at the C ABI:
//! - `program_number` survives end-to-end
//! - `random_access_indicator` surfaces on Video samples
//! - `stream_type` surfaces on Unknown samples
//! - `PesOversize.variant_pid` is preserved
//! - Per-stream stats carry `program_number` with distinct PIDs across programs
//!
//! Scope note: these tests stay scoped to valid multi-program streams with
//! DISTINCT elementary PIDs. Duplicate PIDs across programs trigger
//! `NonConformantIssue::PidReusedAcrossPrograms` (first-program-wins) under
//! the current demux design, where stream identity / stats keys are not
//! `(program_number, pid)`.
//!
//! Test infrastructure:
//! - All 5 tests drive a real `tst_raw_sender_t` → `tst_demux_receiver_t`
//!   SRT loopback so the C event-conversion path is exercised end-to-end.
//! - Multi-program / RAI / multi-stats streams are built via the in-process
//!   `tst_core::mpegts::mux::Muxer` (it produces spec-conformant PAT/PMT/PES
//!   for the common stream types and exposes per-program push handles).
//! - Unknown-stream-type and PesOversize streams need PMT/PES shapes the
//!   Muxer doesn't emit, so those are hand-built per the layout helpers used
//!   by `tst-core/tests/mpegts_demux_strict.rs`.

// Drives a real SRT loopback via `tst_raw_sender_t` → `tst_demux_receiver_t`,
// both `cfg(feature = "srt")`.
#![cfg(feature = "srt")]

use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::common::crc32::crc32_mpeg2;
use tst_core::mpegts::mux::{
    Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tstrans::demux_config::{
    tst_demux_config_free, tst_demux_config_new, tst_demux_config_set_pes_cap,
};
use tstrans::error::{TstError, tst_get_last_error_str};
use tstrans::event::{TstDiscontinuityKindTag, TstEvent, TstEventKind, TstStreamKindTag};
use tstrans::receiver::demux_receiver::{
    tst_demux_receiver_close, tst_demux_receiver_get_stream_stats,
    tst_demux_receiver_open_listener, tst_demux_receiver_open_listener_with_config,
    tst_demux_receiver_recv_event,
};
use tstrans::sender::raw_sender::{tst_raw_sender_close, tst_raw_sender_open, tst_raw_sender_send};
use tstrans::stats::TstStreamStats;

// ---------------------------------------------------------------------------
// last-error helper
// ---------------------------------------------------------------------------

fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Synthetic NAL — opaque to the muxer/demuxer; just needs an Annex-B envelope.
// ---------------------------------------------------------------------------

const NAL_IDR: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, // Annex-B start code
    0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, // SPS-shaped (nal_type=7)
    0x00, 0x00, 0x00, 0x01, //
    0x65, 0x88, 0x80, 0x40, // IDR-shaped (nal_type=5)
];

// ---------------------------------------------------------------------------
// Stream builders (Muxer-based, valid streams)
// ---------------------------------------------------------------------------

/// Build a 2-program TS stream. Each program has its own H.264 video stream
/// on a distinct PID. Pushes enough frames at PSI-cadence-spanning PTSes so
/// PAT/PMT for both programs emit at least once before the muxer drains.
///
/// Layout:
///   Program 1: PMT 0x0100, video 0x1011 (H.264)
///   Program 2: PMT 0x0200, video 0x1211 (H.264)
///
/// PIDs constrained by the muxer to `0x0010..=0x1FFE`.
fn build_multi_program_stream_with_video_in_each_program() -> Vec<u8> {
    let cfg = {
        let mut p1 = MuxerProgramConfigBuilder::new(1, 0x0100);
        p1.add_video(0x1011, MuxVideoCodec::H264);
        let mut p2 = MuxerProgramConfigBuilder::new(2, 0x0200);
        p2.add_video(0x1211, MuxVideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(p1.build());
        b.add_program(p2.build());
        b.build().expect("mux config build")
    };
    let mut mux = Muxer::new(cfg).expect("Muxer::new");
    let v_handles = mux.video_handles();
    assert_eq!(v_handles.len(), 2, "expected 2 video stream handles");
    let v1 = v_handles[0];
    let v2 = v_handles[1];

    // 30 frames at 3003 ticks (~30fps@90kHz) spans the default 100 ms PSI
    // interval, guaranteeing PAT + both PMTs emit before drain.
    for tick in 0u64..30 {
        let pts = Pts90khz::new(90_000i64 + (tick * 3_003) as i64);
        mux.push_video_to(v1, NAL_IDR, pts, true).expect("push v1");
        mux.push_video_to(v2, NAL_IDR, pts, true).expect("push v2");
    }
    drain_mux(&mut mux)
}

/// Build a single-program TS stream with one H.264 video stream; pushes
/// frames with `key_frame=true` so the TS adaptation field carries the
/// `random_access_indicator` bit on the PES_start packet.
///
/// Muxer::push_video_to sets `adaptation.random_access = true` when
/// `key_frame=true` (see `crates/tst-core/src/mpegts/mux/mod.rs`).
fn build_stream_with_video_rai_set() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x0100);
        prog.add_video(0x1011, MuxVideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("mux config build")
    };
    let mut mux = Muxer::new(cfg).expect("Muxer::new");
    let vh = mux.video_handles()[0];
    // 15 key-frames at 3003 ticks — ensures PSI emits and at least one
    // sample event makes it through with RAI set.
    for tick in 0u64..15 {
        let pts = Pts90khz::new(90_000i64 + (tick * 3_003) as i64);
        mux.push_video_to(vh, NAL_IDR, pts, /* key_frame = */ true)
            .expect("push v");
    }
    drain_mux(&mut mux)
}

fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 * 1024);
    let mut buf = [0u8; 188 * 256];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

// ---------------------------------------------------------------------------
// Hand-built TS stream builders (cases the Muxer doesn't emit)
// ---------------------------------------------------------------------------

/// Build a single-program TS stream where the PMT declares one elementary
/// stream with the caller-supplied `stream_type` byte. The Muxer maps
/// recognized codecs to fixed stream_types; for "unknown" coverage
/// (e.g. 0x77 — undefined in our typed mapping) we hand-build the PSI.
///
/// Layout:
///   PAT (PID 0x0000) → program 1 / PMT 0x0100
///   PMT (PID 0x0100) → ES PID 0x1099, stream_type=`stream_type_byte`
///   PES (PID 0x1099) → tiny PTS-only payload (the demuxer surfaces this
///                      as `SamplePayload::Unknown { stream_type, raw }`)
fn build_stream_with_unknown_stream_type(stream_type_byte: u8) -> Vec<u8> {
    let mut ts = Vec::new();
    let es_pid: u16 = 0x1099;

    // PAT: cycle out a few copies so the demuxer's PSI cadence has multiple
    // PUSI-aligned starts to latch onto — cheaper than computing the exact
    // first-time emission window.
    for cc in 0u8..4 {
        ts.extend_from_slice(&build_pat_packet(0x0100, cc));
    }
    // PMT: one ES entry, stream_type = caller-supplied byte.
    for cc in 0u8..4 {
        ts.extend_from_slice(&build_pmt_packet_single_es(
            /* pmt_pid = */ 0x0100,
            /* program_number = */ 1,
            /* pcr_pid = */ es_pid,
            /* es_pid = */ es_pid,
            stream_type_byte,
            cc,
        ));
    }
    // PES: small payload so it fits in one TS packet body.
    let pes_body = build_pes(
        /* stream_id = */ 0xBD,
        Some(90_000),
        &[0xAA, 0xBB, 0xCC, 0xDD],
    );
    ts.extend(pack_pes_into_ts_packets(
        es_pid, &pes_body, /* cc_start = */ 0,
    ));
    ts
}

/// Build a single-program TS stream where the PMT declares one H.264
/// elementary stream, then sends a PES whose declared `PES_packet_length`
/// exceeds the demuxer's per-PID reassembly cap. The demuxer surfaces this
/// as `DiscontinuityKind::PesOversize { pid }`.
///
/// `pid` is the elementary stream PID; the test configures the demux receiver
/// with `pes_cap_per_pid = 4096`, so the PES is constructed with a body large
/// enough to blow the cap.
fn build_stream_with_pes_oversize_on_pid(pid: u16) -> Vec<u8> {
    let mut ts = Vec::new();

    for cc in 0u8..4 {
        ts.extend_from_slice(&build_pat_packet(0x0100, cc));
    }
    for cc in 0u8..4 {
        ts.extend_from_slice(&build_pmt_packet_single_es(
            /* pmt_pid = */ 0x0100, /* program_number = */ 1, /* pcr_pid = */ pid,
            /* es_pid = */ pid, /* stream_type = */ 0x1B, // H.264
            cc,
        ));
    }

    // Body sized to exceed the 4096-byte per-PID cap configured below.
    // 8 KiB is comfortably over; once the reassembler's growing buffer
    // crosses cap_per_pid, it emits `Overflow { pid }`.
    let big_payload = vec![0xA5u8; 8 * 1024];
    let pes_body = build_pes(/* stream_id = */ 0xE0, Some(90_000), &big_payload);
    ts.extend(pack_pes_into_ts_packets(
        pid, &pes_body, /* cc_start = */ 0,
    ));
    ts
}

/// Same shape as `build_multi_program_stream_with_video_in_each_program`,
/// but exposes the per-program video PIDs so the stats test can assert which
/// PID belongs to which program. Returns just the bytes — the test knows the
/// PIDs it passed in.
fn build_multi_program_stream_with_distinct_pids(
    program_1_video_pid: u16,
    program_2_video_pid: u16,
) -> Vec<u8> {
    let cfg = {
        let mut p1 = MuxerProgramConfigBuilder::new(1, 0x0100);
        p1.add_video(program_1_video_pid, MuxVideoCodec::H264);
        let mut p2 = MuxerProgramConfigBuilder::new(2, 0x0200);
        p2.add_video(program_2_video_pid, MuxVideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(p1.build());
        b.add_program(p2.build());
        b.build().expect("mux config build")
    };
    let mut mux = Muxer::new(cfg).expect("Muxer::new");
    let v_handles = mux.video_handles();
    let v1 = v_handles[0];
    let v2 = v_handles[1];
    for tick in 0u64..30 {
        let pts = Pts90khz::new(90_000i64 + (tick * 3_003) as i64);
        mux.push_video_to(v1, NAL_IDR, pts, true).expect("push v1");
        mux.push_video_to(v2, NAL_IDR, pts, true).expect("push v2");
    }
    drain_mux(&mut mux)
}

// ---------------------------------------------------------------------------
// Hand-built PSI/PES helpers (mirrors patterns in
// `tst-core/tests/mpegts_demux_strict.rs` so the test reads like the rest of
// the suite). Kept local to this file because the helpers there are
// `#[cfg(test)]`-private to that crate.
// ---------------------------------------------------------------------------

/// Build a single 188-byte TS packet carrying a PAT that references one
/// program. PUSI=1, PID=0.
fn build_pat_packet(pmt_pid: u16, cc: u8) -> [u8; 188] {
    let section_length: u16 = 13;
    let mut sec: Vec<u8> = Vec::with_capacity(16);
    sec.push(0x00); // table_id = PAT
    sec.push(0xB0 | (((section_length >> 8) & 0x0F) as u8));
    sec.push((section_length & 0xFF) as u8);
    sec.push(0x00); // transport_stream_id high
    sec.push(0x01); // transport_stream_id low
    sec.push(0xC1); // reserved + version=0 + current_next_indicator=1
    sec.push(0x00); // section_number
    sec.push(0x00); // last_section_number
    sec.push(0x00); // program_number high
    sec.push(0x01); // program_number low
    sec.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
    sec.push((pmt_pid & 0xFF) as u8);
    let crc = crc32_mpeg2(&sec);
    sec.push((crc >> 24) as u8);
    sec.push((crc >> 16) as u8);
    sec.push((crc >> 8) as u8);
    sec.push(crc as u8);

    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40; // PUSI=1, PID high=0
    pkt[2] = 0x00; // PID low
    pkt[3] = 0x10 | (cc & 0x0F); // payload-only
    pkt[4] = 0x00; // pointer_field
    let chunk = sec.len().min(183);
    pkt[5..5 + chunk].copy_from_slice(&sec[..chunk]);
    pkt
}

/// Build a single 188-byte TS packet carrying a PMT with one ES entry.
///
/// The section is short (no descriptors), so it fits in a single packet —
/// no continuation logic required.
fn build_pmt_packet_single_es(
    pmt_pid: u16,
    program_number: u16,
    pcr_pid: u16,
    es_pid: u16,
    stream_type: u8,
    cc: u8,
) -> [u8; 188] {
    // PMT section body (table_id through CRC):
    //   header (12 bytes) + ES entry (5 bytes) + CRC (4 bytes) = 21 bytes.
    // section_length counts everything after the first 3 bytes →
    //   = 9 (header tail) + 5 (ES entry) + 4 (CRC) = 18.
    let section_length: u16 = 9 + 5 + 4;
    let mut sec: Vec<u8> = Vec::with_capacity(3 + section_length as usize);
    sec.push(0x02); // table_id = PMT
    sec.push(0xB0 | (((section_length >> 8) & 0x0F) as u8));
    sec.push((section_length & 0xFF) as u8);
    sec.push((program_number >> 8) as u8);
    sec.push(program_number as u8);
    sec.push(0xC1); // reserved + version=0 + current_next_indicator=1
    sec.push(0x00); // section_number
    sec.push(0x00); // last_section_number
    sec.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
    sec.push(pcr_pid as u8);
    sec.push(0xF0); // reserved + program_info_length high (=0)
    sec.push(0x00); // program_info_length low

    // ES entry: stream_type + PID + ES_info_length=0
    sec.push(stream_type);
    sec.push(0xE0 | ((es_pid >> 8) as u8 & 0x1F));
    sec.push(es_pid as u8);
    sec.push(0xF0); // reserved + ES_info_length high (=0)
    sec.push(0x00); // ES_info_length low

    let crc = crc32_mpeg2(&sec);
    sec.push((crc >> 24) as u8);
    sec.push((crc >> 16) as u8);
    sec.push((crc >> 8) as u8);
    sec.push(crc as u8);

    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | ((pmt_pid >> 8) as u8 & 0x1F);
    pkt[2] = pmt_pid as u8;
    pkt[3] = 0x10 | (cc & 0x0F);
    pkt[4] = 0x00; // pointer_field
    let chunk = sec.len().min(183);
    pkt[5..5 + chunk].copy_from_slice(&sec[..chunk]);
    pkt
}

/// Build a PES packet body (header + payload). `stream_id` selects the PES
/// header shape — use 0xE0-0xEF for video, 0xBD/0xFC for private/metadata.
fn build_pes(stream_id: u8, pts: Option<i64>, payload: &[u8]) -> Vec<u8> {
    let mut s = Vec::with_capacity(14 + payload.len());
    s.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    // Reserve length field; backfill at end.
    s.push(0x00);
    s.push(0x00);
    // Optional header: marker(2)=10, scrambling/priority/alignment/copyright/original.
    s.push(0x80);
    let pts_dts_flags: u8 = if pts.is_some() { 0b10 } else { 0b00 };
    s.push(pts_dts_flags << 6);
    let pts_bytes = if let Some(p) = pts {
        let p = p as u64;
        let b0 = (0x21 | (((p >> 30) as u8) << 1)) & 0xEF;
        let b1 = ((p >> 22) & 0xFF) as u8;
        let b2 = (((p >> 14) & 0xFE) as u8) | 0x01;
        let b3 = ((p >> 7) & 0xFF) as u8;
        let b4 = (((p << 1) & 0xFE) as u8) | 0x01;
        vec![b0, b1, b2, b3, b4]
    } else {
        Vec::new()
    };
    s.push(pts_bytes.len() as u8); // header_data_length (after this byte)
    s.extend_from_slice(&pts_bytes);
    s.extend_from_slice(payload);
    // Backfill PES_packet_length (bytes after byte 5).
    let pes_packet_length = (s.len() - 6) as u16;
    s[4] = (pes_packet_length >> 8) as u8;
    s[5] = pes_packet_length as u8;
    s
}

/// Pack a PES body into a sequence of 188-byte TS packets on `pid`.
/// First packet has PUSI=1; continuation packets have PUSI=0.
fn pack_pes_into_ts_packets(pid: u16, pes_body: &[u8], cc_start: u8) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut cc = cc_start & 0x0F;
    let mut pos: usize = 0;
    let pid_hi = (pid >> 8) as u8 & 0x1F;
    let pid_lo = pid as u8;

    // First packet: PUSI=1, payload up to 184 bytes (no adaptation field).
    {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40 | pid_hi;
        pkt[2] = pid_lo;
        pkt[3] = 0x10 | cc;
        let avail = 184;
        let chunk = (pes_body.len() - pos).min(avail);
        pkt[4..4 + chunk].copy_from_slice(&pes_body[pos..pos + chunk]);
        pos += chunk;
        out.extend_from_slice(&pkt);
        cc = (cc + 1) & 0x0F;
    }
    // Continuation packets.
    while pos < pes_body.len() {
        let mut pkt = [0xFFu8; 188];
        pkt[0] = 0x47;
        pkt[1] = pid_hi; // PUSI=0
        pkt[2] = pid_lo;
        pkt[3] = 0x10 | cc;
        let avail = 184;
        let chunk = (pes_body.len() - pos).min(avail);
        pkt[4..4 + chunk].copy_from_slice(&pes_body[pos..pos + chunk]);
        pos += chunk;
        out.extend_from_slice(&pkt);
        cc = (cc + 1) & 0x0F;
    }
    out
}

// ---------------------------------------------------------------------------
// Loopback driver: send `stream` over SRT, collect drained events from
// `tst_demux_receiver_t`, return them to the caller.
// ---------------------------------------------------------------------------

/// Default SRT payload size in live mode. 7 × 188 — each chunk fits exactly
/// 7 TS packets, matching what real ingest tools (ffmpeg, gstreamer) send.
const SRT_PAYLOAD_SIZE: usize = 7 * 188;

/// Drive one round-trip: spin a listener-mode `tst_demux_receiver_t` on
/// `port`, send `stream` over SRT from a caller-mode `tst_raw_sender_t`,
/// drain typed events until EOS, return them.
///
/// Optionally configures `pes_cap_per_pid` on the receiver (used by the
/// PesOversize test to drive overflow with a modest payload).
fn run_loopback(stream: Vec<u8>, port: u16, pes_cap_per_pid: Option<usize>) -> Vec<RecordedEvent> {
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (events_tx, events_rx) = mpsc::channel::<Vec<RecordedEvent>>();

    let receiver_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        let rx = if let Some(cap) = pes_cap_per_pid {
            unsafe {
                let cfg = tst_demux_config_new();
                let rc = tst_demux_config_set_pes_cap(cfg, cap, 0);
                assert_eq!(rc, 0, "set_pes_cap failed: {}", last_error_msg());
                let h = tst_demux_receiver_open_listener_with_config(url.as_ptr(), cfg);
                tst_demux_config_free(cfg);
                h
            }
        } else {
            unsafe { tst_demux_receiver_open_listener(url.as_ptr()) }
        };
        if rx.is_null() {
            panic!(
                "tst_demux_receiver_open_listener failed: {}",
                last_error_msg()
            );
        }
        ready_tx.send(()).expect("ready channel dropped");

        let mut events: Vec<RecordedEvent> = Vec::new();
        let mut ev = TstEvent::default();
        loop {
            let rc = unsafe { tst_demux_receiver_recv_event(rx, &mut ev) };
            if rc == 0 {
                if let Some(rec) = RecordedEvent::from_event(&ev) {
                    events.push(rec);
                }
                continue;
            }
            if rc == TstError::EndOfStream as i32 {
                break;
            }
            panic!("recv_event failed (rc={rc}): {}", last_error_msg());
        }
        unsafe { tst_demux_receiver_close(rx) };
        events_tx.send(events).expect("events channel dropped");
    });

    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();
        // Retry until the listener has bound + is at accept.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let tx = loop {
            let h = unsafe { tst_raw_sender_open(url.as_ptr(), std::ptr::null()) };
            if !h.is_null() {
                break h;
            }
            if std::time::Instant::now() > deadline {
                panic!("tst_raw_sender_open timed out: {}", last_error_msg());
            }
            thread::sleep(Duration::from_millis(50));
        };
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver ready timeout");

        for chunk in stream.chunks(SRT_PAYLOAD_SIZE) {
            let rc = unsafe { tst_raw_sender_send(tx, chunk.as_ptr(), chunk.len()) };
            assert_eq!(rc, 0, "raw_sender_send failed: {}", last_error_msg());
        }
        // Drain pause — give libsrt time to flush before close → FIN.
        // Matches the 1 s budget used by demux_receiver_loopback.rs.
        thread::sleep(Duration::from_secs(1));
        unsafe { tst_raw_sender_close(tx) };
    });

    sender_thread.join().expect("sender thread panicked");
    let events = events_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("events not received from receiver thread");
    receiver_thread.join().expect("receiver thread panicked");
    events
}

// ---------------------------------------------------------------------------
// Snapshot type — events are arena-borrowed, so we capture relevant fields
// out before the next recv_event clears the arena.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum RecordedEvent {
    Sample {
        pid: u16,
        program_number: u16,
        stream_kind: i32,
        random_access_indicator: u8,
        stream_type: u8,
    },
    Discontinuity {
        pid: u16,
        variant_pid: u16,
        discontinuity_kind: i32,
    },
}

impl RecordedEvent {
    fn from_event(ev: &TstEvent) -> Option<Self> {
        match ev.kind {
            k if k == TstEventKind::Sample as i32 => {
                let s = unsafe { ev.u.sample };
                Some(RecordedEvent::Sample {
                    pid: s.pid,
                    program_number: s.program_number,
                    stream_kind: s.stream_kind,
                    random_access_indicator: s.random_access_indicator,
                    stream_type: s.stream_type,
                })
            }
            k if k == TstEventKind::Discontinuity as i32 => {
                let d = unsafe { ev.u.discontinuity };
                Some(RecordedEvent::Discontinuity {
                    pid: d.pid,
                    variant_pid: d.variant_pid,
                    discontinuity_kind: d.discontinuity_kind,
                })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-test ports — within-file tests run in parallel; one port per test
// avoids accept races. Offset by pid (mod 100) so concurrent test
// invocations on the same host don't collide either.
// ---------------------------------------------------------------------------

fn port_for(slot: u16) -> u16 {
    29_700 + slot + (std::process::id() as u16 % 100)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn sample_events_carry_correct_program_number() {
    let stream = build_multi_program_stream_with_video_in_each_program();
    let events = run_loopback(stream, port_for(0), None);

    let mut program_1_count = 0;
    let mut program_2_count = 0;
    for ev in &events {
        if let RecordedEvent::Sample {
            program_number,
            pid,
            ..
        } = ev
        {
            match program_number {
                1 => program_1_count += 1,
                2 => program_2_count += 1,
                other => panic!("unexpected program_number {other} on PID 0x{pid:04X}"),
            }
        }
    }
    assert!(
        program_1_count > 0,
        "must observe samples in program 1 (events={events:?})"
    );
    assert!(
        program_2_count > 0,
        "must observe samples in program 2 (events={events:?})"
    );
}

#[test]
fn video_sample_carries_random_access_indicator() {
    let stream = build_stream_with_video_rai_set();
    let events = run_loopback(stream, port_for(1), None);

    let video_sample_with_rai = events.iter().find(|ev| {
        matches!(
            ev,
            RecordedEvent::Sample {
                stream_kind,
                random_access_indicator: 1,
                ..
            } if *stream_kind == TstStreamKindTag::Video as i32,
        )
    });
    assert!(
        video_sample_with_rai.is_some(),
        "expected at least one Video sample with RAI=1 (events={events:?})"
    );
}

#[test]
fn unknown_sample_carries_stream_type_byte() {
    let stream = build_stream_with_unknown_stream_type(0x77);
    let events = run_loopback(stream, port_for(2), None);

    let unknown_sample = events.iter().find(|ev| {
        matches!(
            ev,
            RecordedEvent::Sample {
                stream_kind,
                ..
            } if *stream_kind == TstStreamKindTag::Unknown as i32,
        )
    });
    let sample = unknown_sample
        .unwrap_or_else(|| panic!("expected at least one Unknown sample (events={events:?})"));
    if let RecordedEvent::Sample { stream_type, .. } = sample {
        assert_eq!(
            *stream_type, 0x77,
            "unknown stream_type byte must surface in C event"
        );
    }
}

#[test]
fn pes_oversize_discontinuity_carries_variant_pid() {
    let pid: u16 = 0x1099;
    let stream = build_stream_with_pes_oversize_on_pid(pid);
    let events = run_loopback(stream, port_for(3), Some(4096));

    let pes_oversize = events.iter().find(|ev| {
        matches!(
            ev,
            RecordedEvent::Discontinuity { discontinuity_kind, .. }
                if *discontinuity_kind == TstDiscontinuityKindTag::PesOversize as i32,
        )
    });
    let disc = pes_oversize.unwrap_or_else(|| {
        panic!("expected at least one PesOversize discontinuity (events={events:?})")
    });
    if let RecordedEvent::Discontinuity {
        pid: parent_pid,
        variant_pid,
        ..
    } = disc
    {
        // `pid` is parent stream PID; `variant_pid`
        // carries the discontinuity-variant-specific PID.
        assert_eq!(
            *variant_pid, pid,
            "PesOversize.variant_pid must be preserved"
        );
        assert_eq!(
            *parent_pid, pid,
            "parent stream PID also matches in this construction"
        );
    }
}

#[test]
fn per_stream_stats_carry_program_number_with_distinct_pids() {
    // Scoped to distinct PIDs — see file docstring.
    let p1_pid: u16 = 0x1011;
    let p2_pid: u16 = 0x1211;
    let stream = build_multi_program_stream_with_distinct_pids(p1_pid, p2_pid);

    // We re-implement run_loopback inline here because we need to query
    // stats on the receiver before close() — the shared driver closes the
    // receiver as soon as EOS arrives, after which the handle is gone.
    let port = port_for(4);
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (stats_tx, stats_rx) = mpsc::channel::<Vec<TstStreamStats>>();

    let receiver_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        let rx = unsafe { tst_demux_receiver_open_listener(url.as_ptr()) };
        if rx.is_null() {
            panic!("open_listener failed: {}", last_error_msg());
        }
        ready_tx.send(()).expect("ready channel dropped");

        let mut ev = TstEvent::default();
        loop {
            let rc = unsafe { tst_demux_receiver_recv_event(rx, &mut ev) };
            if rc == 0 {
                continue;
            }
            if rc == TstError::EndOfStream as i32 {
                break;
            }
            panic!("recv_event failed (rc={rc}): {}", last_error_msg());
        }

        // Snapshot per-stream stats. The borrow is valid until the next
        // _get_stream_stats / _reset_stats / _close call; copy out before
        // we close.
        let mut arr_ptr: *const TstStreamStats = std::ptr::null();
        let mut count: libc::size_t = 0;
        let rc = unsafe { tst_demux_receiver_get_stream_stats(rx, &mut arr_ptr, &mut count) };
        assert_eq!(rc, 0, "get_stream_stats failed: {}", last_error_msg());
        let snapshot: Vec<TstStreamStats> = if count == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(arr_ptr, count) }.to_vec()
        };
        unsafe { tst_demux_receiver_close(rx) };
        stats_tx.send(snapshot).expect("stats channel dropped");
    });

    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let tx = loop {
            let h = unsafe { tst_raw_sender_open(url.as_ptr(), std::ptr::null()) };
            if !h.is_null() {
                break h;
            }
            if std::time::Instant::now() > deadline {
                panic!("raw_sender_open timed out: {}", last_error_msg());
            }
            thread::sleep(Duration::from_millis(50));
        };
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver ready timeout");
        for chunk in stream.chunks(SRT_PAYLOAD_SIZE) {
            let rc = unsafe { tst_raw_sender_send(tx, chunk.as_ptr(), chunk.len()) };
            assert_eq!(rc, 0, "raw_sender_send failed: {}", last_error_msg());
        }
        thread::sleep(Duration::from_secs(1));
        unsafe { tst_raw_sender_close(tx) };
    });

    sender_thread.join().expect("sender thread panicked");
    let snapshot = stats_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stats not received");
    receiver_thread.join().expect("receiver thread panicked");

    let p1 = snapshot
        .iter()
        .find(|s| s.pid == p1_pid)
        .unwrap_or_else(|| {
            panic!(
                "program 1 video PID 0x{p1_pid:04X} must have a stats entry; snapshot pids: {:?}",
                snapshot.iter().map(|s| s.pid).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        p1.program_number, 1,
        "program 1 video stats must carry program_number=1"
    );

    let p2 = snapshot
        .iter()
        .find(|s| s.pid == p2_pid)
        .unwrap_or_else(|| {
            panic!(
                "program 2 video PID 0x{p2_pid:04X} must have a stats entry; snapshot pids: {:?}",
                snapshot.iter().map(|s| s.pid).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        p2.program_number, 2,
        "program 2 video stats must carry program_number=2"
    );
}
