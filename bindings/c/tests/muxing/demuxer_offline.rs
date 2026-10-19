//! Offline byte-feeding demuxer (`tst_demuxer_t`) via the C ABI.
//!
//! Exercises the pure tst-core path: no SRT, no transport URL, no
//! network. Builds TS bytes in-process with the C muxer, then feeds
//! them through the new `tst_demuxer_*` surface.

// Only used to build TS bytes via the C muxer, which is `srt`-gated below.
#[cfg(feature = "srt")]
use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::demuxer::{
    tst_demuxer_close, tst_demuxer_feed, tst_demuxer_flush, tst_demuxer_next_event,
    tst_demuxer_open, tst_demuxer_open_with_config,
};
use tstrans::error::TstError;
use tstrans::event::TstEvent;
// TstEventKind variants are compared only in the `srt`-gated event-check block.
#[cfg(feature = "srt")]
use tstrans::event::TstEventKind;

// Pull in the muxer open/pull/close for building TS bytes offline.
#[cfg(feature = "srt")]
use tstrans::muxer::{tst_muxer_close, tst_muxer_open, tst_muxer_pull, tst_muxer_push_video};

// Use the C-ABI demux config for the open_with_config test.
use tstrans::demux_config::{tst_demux_config_free, tst_demux_config_new};

// A minimal Annex-B IDR NAL unit (SPS + IDR slice stub) accepted by the muxer.
// Only fed to the `srt`-gated C muxer below.
#[cfg(feature = "srt")]
const NAL_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xAA, 0xAA, 0xAA];

/// Build TS bytes offline: one H.264 IDR AU through a single-stream muxer.
/// Returns the TS bytes (multiple of 188).
/// Falls back to a pre-baked synthetic TS if the `srt` feature is not active
/// (in that case the muxer C ABI is not compiled, but the demuxer C ABI is
/// unconditional). `srt` is opt-in (default-off); the CI matrix exercises
/// this real-muxer path via its `--all-features` run.
fn build_ts_bytes() -> Vec<u8> {
    // Build real TS from the C muxer when `srt` feature is on.
    // When `srt` is off we fall back to a known-good 188-byte PAT/PMT-less
    // synthetic packet so the demuxer open/feed/flush/close contract can
    // still be exercised (it won't produce events, but the lifecycle
    // functions must all succeed).
    #[cfg(feature = "srt")]
    {
        unsafe {
            let cfg = tst_mux_config_new();
            let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
            tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
            let mux = tst_muxer_open(cfg);
            tst_mux_config_free(cfg);
            assert!(!mux.is_null(), "tst_muxer_open returned null");

            let rc = tst_muxer_push_video(mux, NAL_IDR.as_ptr(), NAL_IDR.len(), 0, true);
            assert_eq!(rc, 0, "tst_muxer_push_video failed: {rc}");

            let mut buf = vec![0u8; 64 * 188];
            let n = tst_muxer_pull(mux, buf.as_mut_ptr(), buf.len());
            assert!(n > 0, "muxer produced no output");
            assert_eq!(n % 188, 0, "TS output not multiple of 188");

            tst_muxer_close(mux);
            buf.truncate(n);
            buf
        }
    }
    #[cfg(not(feature = "srt"))]
    {
        // Minimal valid PAT packet: 1 × 188 bytes, PID 0x0000.
        // sync byte + PID=0 + continuity | PUSI, then PAT section.
        // This won't produce events but lets the lifecycle be exercised.
        let mut pkt = vec![0u8; 188];
        pkt[0] = 0x47; // sync
        pkt[1] = 0x40; // PUSI + PID 0 high
        pkt[2] = 0x00; // PID 0 low
        pkt[3] = 0x10; // AF=01, CC=0 → payload only
        pkt[4] = 0x00; // pointer_field = 0
        // PAT section header (minimal, section_length too short to parse — that's fine)
        pkt[5] = 0x00; // table_id = 0x00 (PAT)
        pkt[6] = 0x80; // section_syntax | section_length high = 0
        pkt[7] = 0x08; // section_length = 8 (ts_id + version + sn + last_sn + CRC)
        pkt[8] = 0x00;
        pkt[9] = 0x01; // transport_stream_id = 1
        pkt[10] = 0xC1; // version 0, current
        pkt[11] = 0x00; // section_number
        pkt[12] = 0x00; // last_section_number
        // CRC32 (synthetic, may fail check — lenient mode only)
        pkt[13] = 0x00;
        pkt[14] = 0x00;
        pkt[15] = 0x00;
        pkt[16] = 0x00;
        pkt
    }
}

// ── Test: basic open / feed / flush / drain / close lifecycle ───────────────

#[test]
fn demuxer_open_with_config_returns_non_null() {
    unsafe {
        let cfg = tst_demux_config_new();
        assert!(!cfg.is_null());
        let d = tst_demuxer_open_with_config(cfg);
        assert!(!d.is_null(), "tst_demuxer_open_with_config returned null");
        tst_demuxer_close(d);
        tst_demux_config_free(cfg);
    }
}

#[test]
fn demuxer_feed_flush_produces_events() {
    // Build real TS from the C muxer, then feed into the C demuxer.
    // Assert that at minimum a ProgramMap event arrives.
    let ts_bytes = build_ts_bytes();

    unsafe {
        let d = tst_demuxer_open();
        assert!(!d.is_null());

        // feed
        let rc = tst_demuxer_feed(d, ts_bytes.as_ptr(), ts_bytes.len());
        assert_eq!(rc, 0, "tst_demuxer_feed returned {rc}");

        // flush (surfaces any partial PES still buffered)
        let rc = tst_demuxer_flush(d);
        assert_eq!(rc, 0, "tst_demuxer_flush returned {rc}");

        // drain all events
        let mut event_kinds: Vec<i32> = Vec::new();
        loop {
            let mut out = TstEvent::default();
            let rc = tst_demuxer_next_event(d, &mut out);
            if rc == TstError::NotAvailable as i32 {
                // The "no event ready" sentinel — feed more or we're done.
                break;
            }
            assert_eq!(rc, 0, "tst_demuxer_next_event returned unexpected {rc}");
            event_kinds.push(out.kind);
        }

        // We fed real MPEG-TS bytes produced by the C muxer, so we expect
        // at least one ProgramMap event.
        #[cfg(feature = "srt")]
        {
            let has_pmt = event_kinds.contains(&(TstEventKind::ProgramMap as i32));
            assert!(has_pmt, "expected a ProgramMap event; got {event_kinds:?}");

            // Also expect at least one Sample (the IDR AU).
            let has_sample = event_kinds.contains(&(TstEventKind::Sample as i32));
            assert!(has_sample, "expected a Sample event; got {event_kinds:?}");
        }

        tst_demuxer_close(d);
    }
}

#[test]
fn demuxer_feed_null_data_pointer_returns_error() {
    // Feeding a null data pointer (with non-zero len) must return
    // TST_E_INVALID_CONFIG, not panic or segfault.
    unsafe {
        let d = tst_demuxer_open();
        assert!(!d.is_null());
        let rc = tst_demuxer_feed(d, std::ptr::null(), 42);
        assert!(rc < 0, "expected negative error code, got {rc}");
        tst_demuxer_close(d);
    }
}
