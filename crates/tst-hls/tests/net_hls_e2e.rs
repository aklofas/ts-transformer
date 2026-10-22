//! End-to-end: MuxPublisher<HlsPublisher> → ffmpeg pulls /playlist.m3u8 → TS byte-identity.
//!
//! Skips gracefully if `ffmpeg` is not on $PATH.

#![cfg(feature = "serve")]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;

/// Every test here owns an HTTP server for its whole body; holding this lock
/// keeps the thread probe's before/after snapshots free of sibling servers
/// under plain `cargo test`, where tests share one process.
static SERVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hls-e2e-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn synthetic_h264_au() -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
    v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
    v.extend(std::iter::repeat_n(0xab, 200));
    v
}

#[test]
fn hls_pipeline_event_mode_writes_three_segments_locally() {
    let _serial = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tmpdir("event-3");
    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .playlist_window(6)
        .mode(HlsMode::Event)
        .build()
        .unwrap();

    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    };

    let pub_shell = MuxPublisher::with_config(publisher, mux_cfg).unwrap();
    let au = synthetic_h264_au();
    for i in 0i64..3 {
        let pts = Pts90khz::new(i * 9001);
        pub_shell.push_video(&au, pts, true).unwrap();
    }

    let publisher = pub_shell.finish().unwrap();
    publisher.finish().unwrap();

    let pl = std::fs::read_to_string(dir.join("playlist.m3u8")).unwrap();
    assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(pl.contains("#EXT-X-ENDLIST"));
    assert!(dir.join("segment_00000.ts").exists());
}

#[test]
fn hls_extinf_is_media_derived_and_target_is_immutable() {
    let _serial = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tmpdir("media-extinf");
    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(4)) // configured target → ceil(4) = 4
        .playlist_window(6)
        .mode(HlsMode::Event)
        .build()
        .unwrap();

    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    };

    let pub_shell = MuxPublisher::with_config(publisher, mux_cfg).unwrap();
    let au = synthetic_h264_au();
    // 2 s GOPs (IDR every 180000 ticks, P frames 60000 ticks apart) under a
    // 4 s target. A segment closes on the first keyframe at or after 4 s of
    // media time, so each holds two GOPs:
    //   IDR@0:       stream head — opens seg0 at start 0, NO cut.
    //   IDR@180000:  span 2 s < 4 s — no cut, seg0 continues.
    //   IDR@360000:  span 4 s — cut seg0 = 360000 ticks = 4.000 s; opens seg1.
    //   IDR@540000:  span 2 s — no cut.
    //   IDR@720000:  span 4 s — cut seg1 = 4.000 s; opens seg2.
    //   seg2 (IDR@720000 + two P) is still open when the playlist renders.
    for idr in (0..=4).map(|g| g * 180_000i64) {
        pub_shell.push_video(&au, Pts90khz::new(idr), true).unwrap();
        pub_shell
            .push_video(&au, Pts90khz::new(idr + 60_000), false)
            .unwrap();
        pub_shell
            .push_video(&au, Pts90khz::new(idr + 120_000), false)
            .unwrap();
    }

    let _ = pub_shell.publisher_stats(); // touch stats path

    let publisher = pub_shell.finish().unwrap();
    let rendered = publisher.render_playlist(true);
    publisher.finish().unwrap();

    // Target duration is the immutable ceiling (4 s), not the actual segment
    // duration; it must not change across reloads.
    assert!(
        rendered.contains("#EXT-X-TARGETDURATION:4"),
        "playlist:\n{rendered}"
    );
    // Every #EXTINF, rounded to the nearest integer, is at most
    // #EXT-X-TARGETDURATION (RFC 8216 §4.3.3.1).
    let target: f64 = rendered
        .lines()
        .find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:"))
        .expect("TARGETDURATION")
        .parse()
        .unwrap();
    let extinfs: Vec<f64> = rendered
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .map(|v| v.trim_end_matches(',').parse().unwrap())
        .collect();
    // Two cut segments plus the one `finish` closes (IDR@720000 + two P
    // frames = 120000 ticks).
    assert_eq!(extinfs.len(), 3, "playlist:\n{rendered}");
    for d in &extinfs {
        assert!(
            d.round() <= target,
            "EXTINF {d} > target {target}:\n{rendered}"
        );
    }
    // The two closed segments carry a media-derived EXTINF of 4.000 s (two
    // 2 s GOPs each, keyframe to keyframe). A wall-clock EXTINF would read
    // ~0.000 for fast ingestion, and a cut on every keyframe 2.000.
    assert_eq!(
        rendered.matches("#EXTINF:4.000,").count(),
        2,
        "expected two media-derived 4.000 s EXTINFs, got:\n{rendered}"
    );
}

#[test]
fn the_last_segment_duration_is_media_time() {
    let _serial = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tmpdir("last-extinf");
    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(4))
        .playlist_window(6)
        .mode(HlsMode::Event)
        .build()
        .unwrap();
    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    };
    let pub_shell = MuxPublisher::with_config(publisher, mux_cfg).unwrap();
    let au = synthetic_h264_au();
    // IDR@0, IDR@4 s (cuts a 4 s segment), then frames up to 7 s pushed
    // instantly: the open segment spans 3 s of media time.
    pub_shell.push_video(&au, Pts90khz::new(0), true).unwrap();
    pub_shell
        .push_video(&au, Pts90khz::new(360_000), true)
        .unwrap();
    for t in [450_000i64, 540_000, 630_000] {
        pub_shell.push_video(&au, Pts90khz::new(t), false).unwrap();
    }
    let publisher = pub_shell.finish().unwrap();
    publisher.finish().unwrap();

    let playlist = std::fs::read_to_string(dir.join("playlist.m3u8")).unwrap();
    let last = playlist
        .lines()
        .rev()
        .find(|l| l.starts_with("#EXTINF:"))
        .expect("EXTINF");
    assert_eq!(last, "#EXTINF:3.000,", "playlist:\n{playlist}");
}

#[test]
fn hls_pipeline_via_ffmpeg_validates_playlist() {
    let _serial = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH — skipping HLS e2e validation");
        return;
    }

    let dir = tmpdir("ffmpeg");
    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .playlist_window(6)
        .mode(HlsMode::Event)
        .build()
        .unwrap();
    let addr = publisher.local_addr().unwrap();

    let mux_cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    };

    let pub_shell = MuxPublisher::with_config(publisher, mux_cfg).unwrap();
    let au = synthetic_h264_au();
    for i in 0i64..3 {
        let pts = Pts90khz::new(i * 9001);
        pub_shell.push_video(&au, pts, true).unwrap();
    }

    // Finish the stream BEFORE pointing ffmpeg at it, keeping the server up
    // via finish_serving() so ffmpeg reads a complete (ENDLIST-terminated)
    // playlist. Reading the LIVE playlist here deadlocks: the un-ended EVENT
    // playlist lists only ~0.2 s of media while `-t 1` asks ffmpeg for a
    // full second, so ffmpeg polls the playlist for more segments while this
    // test blocks in `output()` waiting for ffmpeg — the finish() that would
    // have written ENDLIST only ran after ffmpeg exited. Masked for as long
    // as no machine running the suite had ffmpeg installed (the guard above
    // skips silently).
    let publisher = pub_shell.finish().unwrap();
    let server = publisher.finish_serving().unwrap();

    let url = format!("http://{}/playlist.m3u8", addr);
    let out = Command::new("ffmpeg")
        .args(["-y", "-i", &url, "-c", "copy", "-f", "mpegts", "-t", "1"])
        .arg(dir.join("ffmpeg_out.ts"))
        .output()
        .expect("ffmpeg failed to spawn");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Stream #0") || stderr.contains("Input #0"),
        "ffmpeg didn't see any stream — stderr:\n{stderr}"
    );

    server.shutdown();
}

/// The HTTP server adds exactly one OS thread, named `tst-hls`, and removes
/// it on `shutdown()`. The probe reads `/proc`, so it only runs where
/// `/proc/self/task` exists.
#[test]
fn http_server_runs_on_one_thread_named_tst_hls() {
    let _serial = SERVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !std::path::Path::new("/proc/self/task").is_dir() {
        eprintln!("skipped: /proc/self/task is not available on this platform");
        return;
    }
    fn thread_names() -> Vec<String> {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|t| std::fs::read_to_string(t.unwrap().path().join("comm")).ok())
            .map(|c| c.trim_end().to_owned())
            .collect()
    }
    fn count(names: &[String], name: &str) -> usize {
        names.iter().filter(|n| *n == name).count()
    }
    let before = thread_names();

    let dir = tmpdir("thread-name");
    let mut publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .mode(HlsMode::Vod)
        .build()
        .unwrap();
    let addr = publisher.local_addr().unwrap();
    // A connection exercises the accept loop before the threads are counted.
    drop(std::net::TcpStream::connect(addr).unwrap());
    let during = thread_names();
    assert_eq!(
        count(&during, "tst-hls"),
        count(&before, "tst-hls") + 1,
        "before: {before:?} during: {during:?}"
    );
    assert_eq!(count(&during, "tst-hls-rt"), count(&before, "tst-hls-rt"));

    publisher.push_ts(&[0x47u8; 376]).unwrap();
    publisher.cut_segment().unwrap();
    publisher.finish_serving().unwrap().shutdown();
    // `shutdown()` joins the thread, but the kernel wakes the joiner a moment
    // before it removes the task's `/proc` entry (seen on linux-aarch64), so
    // the count is allowed a bounded settle, never a fixed sleep.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut after = thread_names();
    while count(&after, "tst-hls") != count(&before, "tst-hls")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
        after = thread_names();
    }
    assert_eq!(
        count(&after, "tst-hls"),
        count(&before, "tst-hls"),
        "after: {after:?}"
    );
}
