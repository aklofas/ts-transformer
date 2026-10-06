//! Internal rolling segmenter — IDR-aligned cuts + duration-driven fallback.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::{HlsConfig, HlsMode};
use crate::error::HlsError;

/// The media playlist's on-disk name under `output_dir`.
pub(crate) const PLAYLIST_FILENAME: &str = "playlist.m3u8";
/// Staging name for the atomic playlist rewrite (see
/// [`Segmenter::write_playlist`]); never served, purged on construction.
const PLAYLIST_TMP_FILENAME: &str = "playlist.m3u8.tmp";

/// One completed segment on disk.
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    /// Monotonically-increasing sequence number (starts at 0).
    pub seq: u64,
    /// Filename relative to [`HlsConfig::output_dir`].
    pub filename: String,
    /// Wall-clock duration of this segment.
    pub duration: Duration,
}

/// A segment evicted from the live playlist but retained on disk until its
/// RFC 8216 §6.2.2 availability window elapses (so clients mid-download of
/// an older playlist don't get a 404).
struct GraceEntry {
    filename: String,
    removed_at: Instant,
    /// Removed-segment duration + the longest playlist that referenced it.
    availability: Duration,
}

pub(crate) struct Segmenter {
    config: HlsConfig,
    /// Immutable `#EXT-X-TARGETDURATION` (seconds), chosen once at
    /// construction as the ceiling of the configured segment duration.
    /// RFC 8216 §6.2.1 forbids the value from changing once the playlist is
    /// published, and §4.3.3.1 requires every EXTINF (rounded to the nearest
    /// integer) to be ≤ this value — so we ceil the configured cap, never
    /// floor it and never recompute it from observed history.
    target_duration_secs: u64,
    next_seq: u64,
    history: VecDeque<Segment>,
    grace: VecDeque<GraceEntry>,
    current: Option<OpenSegment>,
    /// True once at least one explicit (keyframe-driven) cut has been noted.
    /// Switches `tick` from wall-clock segmenting to keyframe-owned cutting
    /// where only the hard cap force-cuts (see [`Segmenter::tick_at`]).
    has_explicit_cuts: bool,
    /// Count of segments cut by the hard-cap fallback (a keyframe was overdue).
    /// Surfaced via `HlsStats::forced_cuts`.
    forced_cuts: u64,
}

struct OpenSegment {
    seq: u64,
    filename: String,
    file: File,
    opened_at: Instant,
    bytes_written: u64,
}

impl Segmenter {
    pub(crate) fn new(config: HlsConfig) -> Result<Self, HlsError> {
        // Ensure output_dir exists.
        std::fs::create_dir_all(&config.output_dir).map_err(HlsError::Io)?;

        // Clean up stale segments + playlist from prior runs (best-effort).
        if let Ok(entries) = std::fs::read_dir(&config.output_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if (name_str.starts_with("segment_") && name_str.ends_with(".ts"))
                    || name_str == PLAYLIST_FILENAME
                    || name_str == PLAYLIST_TMP_FILENAME
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        // Integer ceil-of-seconds: the target is a conformance-critical value
        // (RFC 8216 §4.3.3.1/§6.2.1), so compute it exactly without f64 rounding
        // — `subsec_nanos() > 0` rounds a fractional duration up.
        let target_duration_secs = (config.segment_duration.as_secs()
            + u64::from(config.segment_duration.subsec_nanos() > 0))
        .max(1);

        Ok(Self {
            config,
            target_duration_secs,
            next_seq: 0,
            history: VecDeque::new(),
            grace: VecDeque::new(),
            current: None,
            has_explicit_cuts: false,
            forced_cuts: 0,
        })
    }

    /// Append TS bytes to the current segment (opens one if none).
    pub(crate) fn push_ts(&mut self, ts_bytes: &[u8]) -> Result<(), HlsError> {
        if ts_bytes.len() % 188 != 0 {
            return Err(HlsError::UnalignedPushTs {
                len: ts_bytes.len(),
            });
        }
        if self.current.is_none() {
            self.open_new()?;
        }
        let open = self.current.as_mut().expect("just opened");
        open.file.write_all(ts_bytes).map_err(HlsError::Io)?;
        open.bytes_written = open.bytes_written.saturating_add(ts_bytes.len() as u64);
        Ok(())
    }

    /// Explicitly cut the current segment (called on keyframe by
    /// MuxPublisher). No-op if no segment is currently open.
    pub(crate) fn cut(&mut self) -> Result<(), HlsError> {
        self.cut_with_duration(None)
    }

    /// Cut the current segment, recording `media_duration` (from PTS) as the
    /// segment's duration. `None`, or a zero media duration (a degenerate
    /// single-AU segment), falls back to wall-clock elapsed so `#EXTINF` is
    /// never exactly zero. No-op if no segment is open.
    pub(crate) fn cut_with_duration(
        &mut self,
        media_duration: Option<Duration>,
    ) -> Result<(), HlsError> {
        if let Some(open) = self.current.take() {
            self.close_segment(open, media_duration)?;
        }
        Ok(())
    }

    /// Note that an explicit (keyframe-driven) cut is driving segmentation.
    /// Flips `tick` from wall-clock cutting to keyframe-owned cutting, where
    /// only the hard cap force-cuts. Called by `HlsPublisher::cut_segment{,
    /// _with_duration}` before delegating to the segmenter.
    pub(crate) fn note_explicit_cut(&mut self) {
        self.has_explicit_cuts = true;
    }

    /// Check duration cap and cut if exceeded.
    pub(crate) fn tick(&mut self) -> Result<(), HlsError> {
        self.tick_at(Instant::now())
    }

    /// [`tick`](Self::tick) with an injectable `now` so tests advance time
    /// deterministically (mirrors [`purge_grace`](Self::purge_grace)).
    pub(crate) fn tick_at(&mut self, now: Instant) -> Result<(), HlsError> {
        let Some(open) = self.current.as_ref() else {
            return Ok(());
        };
        let elapsed = now.duration_since(open.opened_at);
        if self.has_explicit_cuts {
            // Keyframe-driven flow: the next explicit cut is coming; only the
            // hard cap force-cuts (a mid-GOP cut yields a segment that does not
            // start on an IDR — worth it only to bound unbounded growth).
            let cap = self.config.max_segment_duration.unwrap_or_else(|| {
                self.config
                    .segment_duration
                    .checked_mul(2)
                    .unwrap_or(Duration::MAX)
            });
            if elapsed >= cap {
                self.forced_cuts += 1;
                self.cut()?;
            }
        } else if elapsed >= self.config.segment_duration {
            // Raw push_ts flow (pre-muxed TS relay): no keyframe signal exists,
            // wall-clock segmenting is all we have — unchanged v0.2.0 behavior.
            //
            // NOTE: `has_explicit_cuts` only flips on the FIRST explicit cut
            // (which, for a keyframe-driven `MuxPublisher`, is the second
            // keyframe). So the very first segment lands here until then, and
            // if the initial GOP is longer than `segment_duration` it can be
            // wall-clock-cut mid-GOP — breaking the "segments begin with IDR"
            // guarantee for segment 0 only. This is unreachable in normal
            // config (a keyframe interval shorter than `segment_duration` cuts
            // the first segment before this fires). Under the misconfiguration
            // GOP > segment_duration the cut fires here at `segment_duration`
            // and is deliberately NOT counted in `forced_cuts` (pinned by
            // raw_mode_tick_cuts_at_segment_duration_and_forced_cuts_stays_zero),
            // so segment-0 mid-GOP cuts are invisible in telemetry today. A
            // complete fix needs a keyframe-driven-intent signal through the
            // `Publisher` trait — deferred, see
            // docs/project/deferred-features.md ("HLS: keyframe-driven-intent
            // signal").
            self.cut()?;
        }
        Ok(())
    }

    /// Visible segments for the playlist. Live history is already pruned by
    /// eviction (older segments live in the grace queue until their
    /// availability window elapses); Event/Vod retain everything — so all
    /// modes render the full history.
    pub(crate) fn visible_segments(&self) -> Vec<Segment> {
        self.history.iter().cloned().collect()
    }

    /// First sequence number in `visible_segments`.
    pub(crate) fn media_sequence(&self) -> u64 {
        self.history.front().map(|s| s.seq).unwrap_or(0)
    }

    /// Bytes in the segment currently being written (zero between cuts).
    pub(crate) fn open_segment_bytes(&self) -> u64 {
        self.current.as_ref().map(|o| o.bytes_written).unwrap_or(0)
    }

    /// Number of segments completed in this run.
    pub(crate) fn segments_written(&self) -> u64 {
        self.next_seq
    }

    /// Segments cut by the hard-cap fallback (a keyframe was overdue).
    pub(crate) fn forced_cuts(&self) -> u64 {
        self.forced_cuts
    }

    pub(crate) fn current_segment_age(&self) -> Option<Duration> {
        self.current.as_ref().map(|o| o.opened_at.elapsed())
    }

    pub(crate) fn last_segment_duration(&self) -> Option<Duration> {
        self.history.back().map(|s| s.duration)
    }

    /// Cut any open segment (called on finish).
    pub(crate) fn finalize(&mut self) -> Result<(), HlsError> {
        self.cut()
    }

    /// Resolve a request name against the authoritative set of segments this
    /// segmenter created and still serves (live window ∪ grace queue). The
    /// request string is a LOOKUP KEY, never a path component — names not in
    /// the set resolve to `None`, which the server turns into a 404. This is
    /// what makes path traversal (CWE-22) structurally impossible: no attacker-
    /// controlled string is ever joined onto `output_dir` unless it was already
    /// emitted by `open_new()`.
    #[cfg(feature = "serve")]
    pub(crate) fn serve_lookup(&self, name: &str) -> Option<PathBuf> {
        let known = self.history.iter().any(|s| s.filename == name)
            || self.grace.iter().any(|g| g.filename == name);
        known.then(|| self.config.output_dir.join(name))
    }

    pub(crate) fn mode(&self) -> HlsMode {
        self.config.mode
    }

    /// Immutable target duration for `#EXT-X-TARGETDURATION` (seconds).
    ///
    /// Chosen once at construction (the ceiling of the configured segment
    /// duration) and never recomputed — RFC 8216 §6.2.1 forbids the value
    /// from changing once the playlist is published, and §4.3.3.1 requires
    /// every EXTINF, rounded to the nearest integer, to be ≤ this value.
    pub(crate) fn target_duration_secs(&self) -> u64 {
        self.target_duration_secs
    }

    fn open_new(&mut self) -> Result<(), HlsError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let filename = format!("segment_{seq:05}.ts");
        let path: PathBuf = self.config.output_dir.join(&filename);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(HlsError::Io)?;
        self.current = Some(OpenSegment {
            seq,
            filename,
            file,
            opened_at: Instant::now(),
            bytes_written: 0,
        });
        Ok(())
    }

    fn close_segment(
        &mut self,
        mut open: OpenSegment,
        media_duration: Option<Duration>,
    ) -> Result<(), HlsError> {
        open.file.flush().map_err(HlsError::Io)?;
        drop(open.file);
        let duration = media_duration
            .filter(|d| !d.is_zero())
            .unwrap_or_else(|| open.opened_at.elapsed());
        let segment = Segment {
            seq: open.seq,
            filename: open.filename.clone(),
            duration,
        };
        self.history.push_back(segment);
        self.evict_if_needed()?;
        // Every cut republishes the playlist on disk so an external static
        // server (nginx, a CDN origin) can serve the stream while it runs,
        // not only after `finish()`; the built-in server keeps rendering
        // from memory. Written after eviction so the file never references
        // a segment that is already in the grace queue.
        self.write_playlist(false)
    }

    /// Render the playlist and replace `output_dir/playlist.m3u8` with it
    /// atomically: the text goes to a staging file first and is renamed
    /// into place, so a reader that opens the playlist mid-write (a static
    /// web server racing a cut) sees either the previous or the new
    /// complete playlist, never a truncated one (`rename(2)` replaces the
    /// target in one step on POSIX; `MoveFileEx` with replace on Windows).
    /// `is_final` appends `#EXT-X-ENDLIST` for EVENT/VOD — the finish path.
    pub(crate) fn write_playlist(&self, is_final: bool) -> Result<(), HlsError> {
        let text = crate::playlist::render(self, is_final);
        let tmp = self.config.output_dir.join(PLAYLIST_TMP_FILENAME);
        std::fs::write(&tmp, text).map_err(HlsError::Io)?;
        std::fs::rename(&tmp, self.config.output_dir.join(PLAYLIST_FILENAME)).map_err(HlsError::Io)
    }

    fn evict_if_needed(&mut self) -> Result<(), HlsError> {
        if !matches!(self.config.mode, HlsMode::Live) {
            return Ok(());
        }
        let now = Instant::now();
        let target = Duration::from_secs(self.target_duration_secs);
        // RFC 8216 §6.2.2: never let the live playlist fall below 3× target.
        // `playlist_window`/`segment_duration` are caller-controlled (settable
        // via URL), so use clamped/checked arithmetic: an absurd value must
        // neither truncate the retention window (early grace deletion → 404)
        // nor overflow `Duration` (debug panic).
        let min_duration = target.checked_mul(3).unwrap_or(Duration::MAX);
        // Conservative bound on "the longest Playlist file" that referenced a
        // segment: the full window at target duration.
        let window_u32 = u32::try_from(self.config.playlist_window).unwrap_or(u32::MAX);
        let longest_playlist = target.checked_mul(window_u32).unwrap_or(Duration::MAX);

        let mut total: Duration = self.history.iter().map(|s| s.duration).sum();
        while self.history.len() > self.config.playlist_window {
            let front_dur = self.history.front().map(|s| s.duration).unwrap_or_default();
            // Stop if evicting the oldest would drop the playlist below 3×
            // target (this is what makes eviction duration-aware, not count-
            // only).
            if total.saturating_sub(front_dur) < min_duration {
                break;
            }
            if let Some(evict) = self.history.pop_front() {
                total = total.saturating_sub(evict.duration);
                self.grace.push_back(GraceEntry {
                    filename: evict.filename,
                    removed_at: now,
                    availability: evict.duration + longest_playlist,
                });
            }
        }
        self.purge_grace(now);
        Ok(())
    }

    /// Delete grace-queued files whose RFC 8216 §6.2.2 availability window
    /// (removed-segment duration + longest referencing playlist) has elapsed.
    /// `now` is a parameter so tests can advance time deterministically.
    pub(crate) fn purge_grace(&mut self, now: Instant) {
        while let Some(front) = self.grace.front() {
            if now.duration_since(front.removed_at) >= front.availability {
                let entry = self.grace.pop_front().expect("front exists");
                let path = self.config.output_dir.join(&entry.filename);
                let _ = std::fs::remove_file(&path);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hls-seg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unaligned_push_rejected() {
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        assert!(matches!(
            s.push_ts(&[0u8; 187]),
            Err(HlsError::UnalignedPushTs { len: 187 })
        ));
    }

    #[test]
    fn cut_creates_segment_file() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.push_ts(&[0x47u8; 376]).unwrap();
        s.cut().unwrap();
        assert!(dir.join("segment_00000.ts").exists());
        assert_eq!(s.segments_written(), 1);
    }

    #[test]
    fn every_cut_rewrites_the_playlist_on_disk() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Event,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        assert!(
            !dir.join("playlist.m3u8").exists(),
            "no playlist before the first cut"
        );
        for seq in 0..2u64 {
            s.push_ts(&[0x47u8; 376]).unwrap();
            s.cut().unwrap();
            let on_disk = std::fs::read_to_string(dir.join("playlist.m3u8"))
                .unwrap_or_else(|e| panic!("playlist missing after cut {seq}: {e}"));
            assert_eq!(
                on_disk,
                crate::playlist::render(&s, false),
                "on-disk playlist must equal the in-memory render after cut {seq}"
            );
            assert!(on_disk.contains(&format!("segment_{seq:05}.ts")));
            assert!(!on_disk.contains("#EXT-X-ENDLIST"));
            assert!(
                !dir.join("playlist.m3u8.tmp").exists(),
                "the atomic-rename temp file must not linger after cut {seq}"
            );
        }
    }

    #[test]
    fn live_eviction_is_reflected_in_the_on_disk_playlist() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Live,
            segment_duration: Duration::from_secs(2),
            playlist_window: 3,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        for _ in 0..6 {
            s.push_ts(&[0x47u8; 188]).unwrap();
            s.cut_with_duration(Some(Duration::from_secs(2))).unwrap();
        }
        let on_disk = std::fs::read_to_string(dir.join("playlist.m3u8")).unwrap();
        assert_eq!(on_disk, crate::playlist::render(&s, false));
        assert!(on_disk.contains("#EXT-X-MEDIA-SEQUENCE:3"));
        assert!(!on_disk.contains("segment_00000.ts"));
        assert!(on_disk.contains("segment_00005.ts"));
    }

    #[test]
    fn stale_playlist_temp_file_is_purged_on_construction() {
        let dir = tmpdir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("playlist.m3u8.tmp"), b"stale").unwrap();
        std::fs::write(dir.join("playlist.m3u8"), b"stale").unwrap();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            ..HlsConfig::default()
        };
        let _s = Segmenter::new(cfg).unwrap();
        assert!(!dir.join("playlist.m3u8.tmp").exists());
        assert!(!dir.join("playlist.m3u8").exists());
    }

    #[test]
    fn live_evicts_beyond_window_into_grace_respecting_3x_target() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Live,
            segment_duration: Duration::from_secs(2), // target 2 → 3× = 6 s
            playlist_window: 3,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        for _ in 0..6 {
            s.push_ts(&[0x47u8; 188]).unwrap();
            s.cut_with_duration(Some(Duration::from_secs(2))).unwrap();
        }
        // 3 newest visible; 3 oldest moved to grace (files still on disk).
        assert_eq!(s.visible_segments().len(), 3);
        assert_eq!(s.media_sequence(), 3);
        for seq in 0..3u64 {
            assert!(
                dir.join(format!("segment_{seq:05}.ts")).exists(),
                "grace-retained file {seq} must still exist"
            );
        }
        // Advance past the availability window and purge — files deleted.
        let far_future = Instant::now() + Duration::from_secs(3600);
        s.purge_grace(far_future);
        for seq in 0..3u64 {
            assert!(
                !dir.join(format!("segment_{seq:05}.ts")).exists(),
                "file {seq} must be deleted after its availability window"
            );
        }
        assert!(dir.join("segment_00005.ts").exists());
    }

    #[test]
    fn live_keeps_more_than_window_to_satisfy_3x_target() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir,
            mode: HlsMode::Live,
            segment_duration: Duration::from_secs(4), // target 4 → 3× = 12 s
            playlist_window: 3,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        // Actual segments are 1 s each; 5 s total never reaches 12 s, so the
        // duration floor blocks all eviction even though count > window.
        for _ in 0..5 {
            s.push_ts(&[0x47u8; 188]).unwrap();
            s.cut_with_duration(Some(Duration::from_secs(1))).unwrap();
        }
        assert_eq!(s.visible_segments().len(), 5, "duration floor keeps all 5");
    }

    #[test]
    fn event_mode_keeps_all() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Event,
            playlist_window: 2,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        for _ in 0..4 {
            s.push_ts(&[0x47u8; 188]).unwrap();
            s.cut().unwrap();
        }
        assert_eq!(s.visible_segments().len(), 4);
        assert!(dir.join("segment_00000.ts").exists());
    }

    #[test]
    fn target_duration_is_ceiling_not_floor() {
        // A 4.6 s cap must advertise target 5 (a 4.6 s EXTINF rounds to 5),
        // never 4 — RFC 8216 §4.3.3.1.
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            segment_duration: Duration::from_millis(4600),
            ..HlsConfig::default()
        };
        let s = Segmenter::new(cfg).unwrap();
        assert_eq!(s.target_duration_secs(), 5);
    }

    #[test]
    fn target_duration_ignores_long_history_segments() {
        // RFC 8216 §6.2.1: the target MUST NOT change once published. The old
        // code recomputed max(config, longest-observed); inject a 9 s segment
        // and prove the target stays at the ceiling of the config (4).
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            segment_duration: Duration::from_secs(4),
            mode: HlsMode::Event,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.history.push_back(Segment {
            seq: 0,
            filename: "segment_00000.ts".into(),
            duration: Duration::from_secs(9),
        });
        assert_eq!(
            s.target_duration_secs(),
            4,
            "target must be immutable, not max-of-history"
        );
    }

    #[test]
    fn explicit_mode_tick_does_not_cut_at_segment_duration() {
        // After one note_explicit_cut(), the keyframe-driven flow owns cutting;
        // tick() must NOT wall-clock-cut at segment_duration.
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            mode: HlsMode::Event,
            segment_duration: Duration::from_secs(2),
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.note_explicit_cut();
        s.push_ts(&[0x47u8; 188]).unwrap();
        let open_at = s.current.as_ref().unwrap().opened_at;
        // 3 s ≥ segment_duration (2 s) but < hard cap (2× = 4 s): no cut.
        s.tick_at(open_at + Duration::from_secs(3)).unwrap();
        assert!(
            s.current.is_some(),
            "explicit-mode tick must not cut at segment_duration"
        );
        assert_eq!(s.forced_cuts, 0);
    }

    #[test]
    fn explicit_mode_tick_force_cuts_at_hard_cap() {
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            mode: HlsMode::Event,
            segment_duration: Duration::from_secs(2), // hard cap defaults to 4 s
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.note_explicit_cut();
        s.push_ts(&[0x47u8; 188]).unwrap();
        let open_at = s.current.as_ref().unwrap().opened_at;
        // 5 s ≥ hard cap (2× 2 s = 4 s): force-cut.
        s.tick_at(open_at + Duration::from_secs(5)).unwrap();
        assert!(s.current.is_none(), "hard cap must force a cut");
        assert_eq!(s.forced_cuts, 1);
        assert_eq!(s.segments_written(), 1);
    }

    #[test]
    fn raw_mode_tick_cuts_at_segment_duration_and_forced_cuts_stays_zero() {
        // No explicit cut ever → pre-muxed-TS relay flow: tick wall-clock-cuts
        // at segment_duration exactly as before, and forced_cuts stays 0.
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            mode: HlsMode::Event,
            segment_duration: Duration::from_secs(2),
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.push_ts(&[0x47u8; 188]).unwrap();
        let open_at = s.current.as_ref().unwrap().opened_at;
        // Just under segment_duration: no cut.
        s.tick_at(open_at + Duration::from_millis(1999)).unwrap();
        assert!(s.current.is_some(), "must not cut before segment_duration");
        // At segment_duration: cut.
        s.tick_at(open_at + Duration::from_secs(2)).unwrap();
        assert!(
            s.current.is_none(),
            "raw-mode tick must cut at segment_duration"
        );
        assert_eq!(s.forced_cuts, 0, "raw-mode cuts are not forced_cuts");
        assert_eq!(s.segments_written(), 1);
    }

    #[test]
    fn media_duration_recorded_verbatim() {
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            mode: HlsMode::Event,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.push_ts(&[0x47u8; 188]).unwrap();
        s.cut_with_duration(Some(Duration::from_millis(2500)))
            .unwrap();
        assert_eq!(
            s.history.back().unwrap().duration,
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn zero_media_duration_falls_back_to_wall_clock() {
        let cfg = HlsConfig {
            output_dir: tmpdir(),
            mode: HlsMode::Event,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.push_ts(&[0x47u8; 188]).unwrap();
        // A degenerate single-AU segment reports zero media duration; the
        // segmenter must fall back to wall-clock so EXTINF is never 0.000.
        s.cut_with_duration(Some(Duration::ZERO)).unwrap();
        assert_eq!(s.history.len(), 1);
    }

    // -- serve_lookup unit tests -------------------------------------------

    #[cfg(feature = "serve")]
    #[test]
    fn serve_lookup_returns_some_for_history_filename() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Event,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.push_ts(&[0x47u8; 188]).unwrap();
        s.cut().unwrap();
        // segment_00000.ts is in history; lookup must resolve to the absolute path.
        let result = s.serve_lookup("segment_00000.ts");
        assert_eq!(result, Some(dir.join("segment_00000.ts")));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn serve_lookup_returns_some_for_grace_filename() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Live,
            segment_duration: Duration::from_secs(2), // target 2 → 3× = 6 s
            playlist_window: 3,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        // Push 6 segments with 2 s each — segments 0–2 evict into grace (file
        // still on disk, still in the grace queue, still lookup-available).
        for _ in 0..6 {
            s.push_ts(&[0x47u8; 188]).unwrap();
            s.cut_with_duration(Some(Duration::from_secs(2))).unwrap();
        }
        assert_eq!(
            s.visible_segments().len(),
            3,
            "only newest 3 visible in live window"
        );
        // segment_00000.ts is in the grace queue, not history.
        assert!(
            s.history
                .iter()
                .all(|seg| seg.filename != "segment_00000.ts"),
            "segment_00000.ts must have been evicted from history"
        );
        let result = s.serve_lookup("segment_00000.ts");
        assert_eq!(
            result,
            Some(dir.join("segment_00000.ts")),
            "grace-queued segment must still be lookup-available"
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn serve_lookup_returns_none_after_grace_purge() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            mode: HlsMode::Live,
            segment_duration: Duration::from_secs(2),
            playlist_window: 3,
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        for _ in 0..6 {
            s.push_ts(&[0x47u8; 188]).unwrap();
            s.cut_with_duration(Some(Duration::from_secs(2))).unwrap();
        }
        // Advance past all availability windows.
        let far_future = Instant::now() + Duration::from_secs(3600);
        s.purge_grace(far_future);
        // segment_00000.ts is no longer in history or grace; lookup must return None.
        assert_eq!(s.serve_lookup("segment_00000.ts"), None);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn serve_lookup_returns_none_for_arbitrary_names() {
        let dir = tmpdir();
        let cfg = HlsConfig {
            output_dir: dir.clone(),
            ..HlsConfig::default()
        };
        let mut s = Segmenter::new(cfg).unwrap();
        s.push_ts(&[0x47u8; 188]).unwrap();
        s.cut().unwrap();

        // Names never created by this segmenter → None regardless of form.
        for name in [
            "segment_99999.ts",
            "segment_../secret.ts",
            "../../etc/passwd",
            "segment_00000.ts\0.ts",
            "",
        ] {
            assert_eq!(
                s.serve_lookup(name),
                None,
                "unexpected Some for never-created name {name:?}"
            );
        }
    }
}
