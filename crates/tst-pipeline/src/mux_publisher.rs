//! [`MuxPublisher`] — pipeline shell that owns a [`Muxer`] and pushes its
//! output to a [`Publisher`].
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use std::sync::Mutex;
use std::time::Duration;

use tracing::info_span;
use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig, StreamKind};
use tst_core::publisher::{Publisher, PublisherStats};

use crate::shell_error::{ShellErrorKind, kind_from_mux};

/// Cumulative stats for a single `MuxPublisher`.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct MuxPublisherStats {
    /// Total TS bytes drained from the muxer and handed to the publisher.
    pub bytes_pushed: u64,
    /// Total muxer drain calls that produced ≥1 chunk.
    pub drain_calls: u64,
    /// Total explicit `cut_segment()` calls.
    pub cut_calls: u64,
}

struct Inner<P: Publisher> {
    muxer: Muxer,
    publisher: P,
    stats: MuxPublisherStats,
    closed: bool,
    /// PTS of the first AU pushed into the segment currently being built;
    /// `None` immediately after a cut (the next push re-baselines it).
    segment_start_pts: Option<Pts90khz>,
    /// PTS of the most recent video AU (drives the explicit `cut_segment`).
    last_video_pts: Option<Pts90khz>,
    /// Reusable scratch buffer for `drain_locked`. Allocated once on first
    /// push and reused for every subsequent drain call, avoiding a fresh
    /// 16 KB zero-init alloc per push.
    scratch: Vec<u8>,
}

/// Shell that owns a [`Muxer`] and pushes its output to a [`Publisher`].
/// Mirrors [`crate::MuxSender`].
///
/// # Poison policy
///
/// No method panics on a poisoned inner mutex. If a prior call panicked
/// mid-mutation and poisoned the lock, each method falls back gracefully:
/// fallible methods (`send_*`, `cut_segment`) return
/// [`MuxPublisherError::LockPoisoned`]; infallible methods (`stats`,
/// `publisher_stats`) recover the poisoned guard and return the last
/// observed value. `finish` recovers the poisoned guard and returns the
/// owned publisher — the publisher's state may be partial if the panic
/// occurred during a push, but the caller receives it for its own
/// disposal.
pub struct MuxPublisher<P: Publisher> {
    inner: Mutex<Inner<P>>,
    /// Lifetime span, entered only during construction and `Drop` — see
    /// [`crate::shell_error::ShellSpan`] for the unwind-safety rationale.
    _span: crate::shell_error::ShellSpan,
}

impl<P: Publisher> std::fmt::Debug for MuxPublisher<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.try_lock() {
            Ok(inner) => f
                .debug_struct("MuxPublisher")
                .field("bytes_pushed", &inner.stats.bytes_pushed)
                .field("drain_calls", &inner.stats.drain_calls)
                .field("cut_calls", &inner.stats.cut_calls)
                .field("closed", &inner.closed)
                .finish(),
            Err(_) => f
                .debug_struct("MuxPublisher")
                .field("locked", &true)
                .finish(),
        }
    }
}

impl<P: Publisher> MuxPublisher<P> {
    /// Build a publisher shell from a publisher + muxer config.
    pub fn with_config(
        publisher: P,
        config: MuxerConfig,
    ) -> Result<Self, MuxPublisherError<P::Error>> {
        let span = info_span!(
            target: "tst_pipeline::mux_publisher",
            "mux_publisher",
            program_count = config.programs.len(),
            publisher_kind = std::any::type_name::<P>(),
        );
        let _enter = span.enter();
        let muxer = Muxer::new(config).map_err(MuxPublisherError::Mux)?;
        tracing::info!("MuxPublisher opened");
        drop(_enter);
        Ok(Self {
            inner: Mutex::new(Inner {
                muxer,
                publisher,
                stats: MuxPublisherStats::default(),
                closed: false,
                segment_start_pts: None,
                last_video_pts: None,
                scratch: Vec::new(),
            }),
            _span: std::panic::AssertUnwindSafe(span),
        })
    }

    /// Send one video access unit (Annex-B framing required).
    ///
    /// A keyframe BEGINS the next segment: before this AU is pushed, the
    /// closing segment is flushed and cut (EXTINF = the exact PTS span
    /// `segment_start..this keyframe`), then PSI (PAT/PMT) is re-emitted so
    /// the new segment opens PAT → PMT → IDR and is independently decodable
    /// (RFC 8216 §3). The keyframe's own bytes then land at the HEAD of the
    /// fresh segment. The stream-head keyframe (no open segment yet) opens
    /// the first segment without a spurious zero-duration cut.
    pub fn send_video(
        &self,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        self.send_video_inner(nal, pts, key_frame, None)
    }

    /// [`Self::send_video`] plus an ST 0604 MISP timestamp SEI spliced
    /// before the first VCL NAL (see `Muxer::push_video_misp_to`).
    /// Requires exactly one configured video stream (like `send_video`).
    pub fn send_video_misp(
        &self,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
        misp: &tst_core::codec::misp_time::MispTimestamp,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        self.send_video_inner(nal, pts, key_frame, Some(misp))
    }

    fn send_video_inner(
        &self,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
        misp: Option<&tst_core::codec::misp_time::MispTimestamp>,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MuxPublisherError::LockPoisoned)?;
        if inner.closed {
            return Err(MuxPublisherError::Closed);
        }
        // A keyframe BEGINS the next segment. Flush everything already muxed
        // into the closing segment, cut (EXTINF = exact PTS span
        // start..this keyframe), then re-emit PSI so the new segment opens
        // PAT → PMT → IDR (RFC 8216 independent decodability). Pushing first
        // and cutting after would put the IDR at the TAIL of the closing
        // segment.
        if key_frame && inner.segment_start_pts.is_some() {
            Self::drain_locked(&mut inner)?;
            let start = inner.segment_start_pts.unwrap_or(pts);
            let media_dur = media_span(start, pts);
            inner
                .publisher
                .cut_segment_with_duration(media_dur)
                .map_err(MuxPublisherError::Publisher)?;
            inner.stats.cut_calls = inner.stats.cut_calls.saturating_add(1);
            inner.muxer.request_psi();
            inner.segment_start_pts = None;
        }
        if inner.segment_start_pts.is_none() {
            inner.segment_start_pts = Some(pts);
        }
        match misp {
            None => inner
                .muxer
                .push_video(nal, pts, key_frame)
                .map_err(MuxPublisherError::Mux)?,
            Some(m) => {
                let handles = inner.muxer.video_handles();
                let [handle] = handles.as_slice() else {
                    return Err(MuxPublisherError::Mux(MuxError::AmbiguousTarget {
                        kind: StreamKind::Video,
                        count: handles.len(),
                    }));
                };
                inner
                    .muxer
                    .push_video_misp_to(*handle, nal, pts, key_frame, m)
                    .map_err(MuxPublisherError::Mux)?;
            }
        }
        Self::drain_locked(&mut inner)?;
        inner.last_video_pts = Some(pts);
        Ok(())
    }

    /// Send one KLV unit.
    ///
    /// `metadata_service_id` selects the KLV stream when multiple KLV PIDs
    /// are configured; pass `0` for single-stream muxer configs.
    pub fn send_klv(
        &self,
        klv: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MuxPublisherError::LockPoisoned)?;
        if inner.closed {
            return Err(MuxPublisherError::Closed);
        }
        inner
            .muxer
            .push_klv(klv, pts, metadata_service_id)
            .map_err(MuxPublisherError::Mux)?;
        Self::drain_locked(&mut inner)
    }

    /// Send one audio frame.
    pub fn send_audio(
        &self,
        frames: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MuxPublisherError::LockPoisoned)?;
        if inner.closed {
            return Err(MuxPublisherError::Closed);
        }
        inner
            .muxer
            .push_audio(frames, pts)
            .map_err(MuxPublisherError::Mux)?;
        Self::drain_locked(&mut inner)
    }

    /// Send one subtitle payload.
    pub fn send_subtitle(
        &self,
        payload: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MuxPublisherError::LockPoisoned)?;
        if inner.closed {
            return Err(MuxPublisherError::Closed);
        }
        // Note: Muxer::push_subtitle takes (pts, payload) — pts first.
        inner
            .muxer
            .push_subtitle(pts, payload)
            .map_err(MuxPublisherError::Mux)?;
        Self::drain_locked(&mut inner)
    }

    /// Send one data payload on the muxer's single data stream.
    ///
    /// Data streams are a PES pass-through — no AU-cell wrap, no framing;
    /// [`Muxer::push_data_to`] holds the contract (`pts` is written into
    /// the PES header only for `carries_pts: true` streams).
    pub fn send_data(&self, data: &[u8], pts: Pts90khz) -> Result<(), MuxPublisherError<P::Error>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MuxPublisherError::LockPoisoned)?;
        if inner.closed {
            return Err(MuxPublisherError::Closed);
        }
        inner
            .muxer
            .push_data(data, pts)
            .map_err(MuxPublisherError::Mux)?;
        Self::drain_locked(&mut inner)
    }

    /// Explicit segment-cut hint.
    pub fn cut_segment(&self) -> Result<(), MuxPublisherError<P::Error>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| MuxPublisherError::LockPoisoned)?;
        if inner.closed {
            return Err(MuxPublisherError::Closed);
        }
        let media_dur = match (inner.segment_start_pts, inner.last_video_pts) {
            (Some(start), Some(end)) => media_span(start, end),
            _ => Duration::ZERO,
        };
        inner
            .publisher
            .cut_segment_with_duration(media_dur)
            .map_err(MuxPublisherError::Publisher)?;
        inner.stats.cut_calls = inner.stats.cut_calls.saturating_add(1);
        inner.muxer.request_psi();
        inner.segment_start_pts = None;
        Ok(())
    }

    /// Snapshot stats.
    pub fn stats(&self) -> MuxPublisherStats {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).stats
    }

    /// Publisher-side stats (universal subset across publisher impls).
    pub fn publisher_stats(&self) -> PublisherStats {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publisher
            .stats()
    }

    /// Consume the shell and return the owned inner publisher.
    ///
    /// No flush or drain is performed here: pending muxed bytes are not
    /// pushed to the publisher, and no end-of-stream marker is written.
    /// If the caller needs to finalize the publisher (flush the pending
    /// segment, write an HLS `#EXT-X-ENDLIST` tag, tear down sinks, etc.),
    /// they should call [`Publisher::finish`] on the returned publisher.
    ///
    /// If the inner lock was poisoned, the publisher is still returned:
    /// `into_inner` recovers the poisoned guard, and the publisher
    /// (possibly in partial state if the panic interrupted a push) is
    /// handed back for caller disposal.
    pub fn finish(self) -> Result<P, MuxPublisherError<P::Error>> {
        let Inner {
            muxer: _,
            publisher,
            stats: _,
            closed: _,
            segment_start_pts: _,
            last_video_pts: _,
            scratch: _,
        } = self.inner.into_inner().unwrap_or_else(|e| e.into_inner());
        Ok(publisher)
    }

    fn drain_locked(inner: &mut Inner<P>) -> Result<(), MuxPublisherError<P::Error>> {
        // Grow the reusable scratch to 16 KB on first use; subsequent calls
        // reuse the already-allocated buffer (no zero-init per push).
        const DRAIN_BUF_SIZE: usize = 16 * 1024;
        if inner.scratch.len() < DRAIN_BUF_SIZE {
            inner.scratch.resize(DRAIN_BUF_SIZE, 0);
        }
        loop {
            let n = inner.muxer.pull(&mut inner.scratch);
            if n == 0 {
                return Ok(());
            }
            inner
                .publisher
                .push_ts(&inner.scratch[..n])
                .map_err(MuxPublisherError::Publisher)?;
            inner.stats.bytes_pushed = inner.stats.bytes_pushed.saturating_add(n as u64);
            inner.stats.drain_calls = inner.stats.drain_calls.saturating_add(1);
        }
    }
}

/// Error returned by [`MuxPublisher`] methods.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MuxPublisherError<E: std::error::Error + Send + Sync + 'static> {
    /// The inner muxer rejected the input.
    #[error("muxer error: {0}")]
    Mux(#[source] MuxError),

    /// The publisher sink returned an error.
    #[error("publisher error: {0}")]
    Publisher(#[source] E),

    /// The shell has been consumed via [`MuxPublisher::finish`].
    #[error("MuxPublisher closed")]
    Closed,

    /// The inner mutex was poisoned because a previous call panicked
    /// mid-mutation. All subsequent fallible calls on this shell return
    /// this error. Infallible methods (`stats`, `publisher_stats`) recover
    /// the poisoned guard and return the last observed value.
    #[error("MuxPublisher lock poisoned")]
    LockPoisoned,
}

impl<E: std::error::Error + Send + Sync + 'static> MuxPublisherError<E> {
    /// Cross-impl categorization for bindings.
    pub fn kind(&self) -> ShellErrorKind {
        match self {
            Self::Mux(e) => kind_from_mux(e),
            Self::Publisher(_) => ShellErrorKind::TransportBroken,
            Self::Closed => ShellErrorKind::Closed,
            Self::LockPoisoned => ShellErrorKind::TransportBroken,
        }
    }
}

/// Media-presentation duration spanned from `start` to `end` PTS (90 kHz).
///
/// Saturates to zero on a non-increasing delta. PTS wraparound is out of
/// scope — a single segment does not span the ~26.5 h 33-bit PTS period.
fn media_span(start: Pts90khz, end: Pts90khz) -> Duration {
    let ticks = end.as_ticks().saturating_sub(start.as_ticks()).max(0) as u64;
    // Split into whole seconds + sub-second nanos rather than computing
    // `ticks * 1e9` (which overflows u64 past ~56.9 h of delta). `ticks / 90_000`
    // always fits u64 and `(ticks % 90_000) * 1e9 < 9e13` cannot overflow, so
    // this is exact for every representable tick count — no clamp, no truncation.
    let secs = ticks / 90_000;
    let subsec_nanos = ((ticks % 90_000) * 1_000_000_000 / 90_000) as u32;
    Duration::new(secs, subsec_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct MemoryPublisher {
        buffers: Vec<Vec<u8>>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("memory")]
    struct MemErr;

    impl Publisher for MemoryPublisher {
        type Error = MemErr;
        fn push_ts(&mut self, ts: &[u8]) -> Result<(), MemErr> {
            if self.buffers.is_empty() {
                self.buffers.push(Vec::new());
            }
            self.buffers.last_mut().unwrap().extend_from_slice(ts);
            Ok(())
        }
        fn cut_segment(&mut self) -> Result<(), MemErr> {
            self.buffers.push(Vec::new());
            Ok(())
        }
        fn finish(self) -> Result<(), MemErr> {
            Ok(())
        }
        fn stats(&self) -> PublisherStats {
            let mut s = PublisherStats::default();
            s.segments_written = self.buffers.len() as u64;
            s.bytes_written = self.buffers.iter().map(|v| v.len() as u64).sum();
            s
        }
    }

    fn test_muxer_config() -> MuxerConfig {
        use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    }

    #[test]
    fn media_span_does_not_overflow_on_large_pts_delta() {
        // A PTS delta beyond ~56.9 h (u64::MAX / 1e9 ticks) overflowed the
        // intermediate `ticks * 1e9` multiply — debug panic / release wrap.
        // 20e9 ticks is above that threshold; the span must be computed
        // exactly (20e9 / 90_000 = 222_222.222222222 s) without panicking.
        let start = Pts90khz::new(0);
        let end = Pts90khz::new(20_000_000_000);
        let span = media_span(start, end);
        assert_eq!(span, Duration::new(222_222, 222_222_222));
    }

    #[test]
    fn cut_creates_new_segment() {
        let p = MemoryPublisher { buffers: vec![] };
        let pub_ = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        pub_.cut_segment().unwrap();
        pub_.cut_segment().unwrap();
        let publisher = pub_.finish().unwrap();
        assert_eq!(publisher.buffers.len(), 2);
    }

    #[test]
    fn stats_track_cut_calls() {
        let p = MemoryPublisher { buffers: vec![] };
        let pub_ = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        pub_.cut_segment().unwrap();
        pub_.cut_segment().unwrap();
        assert_eq!(pub_.stats().cut_calls, 2);
    }

    #[test]
    fn send_data_pushes_ts_bytes() {
        use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_data(0x101, 0xF0, /*carries_pts=*/ true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let cfg = b.build().unwrap();
        let p = MemoryPublisher { buffers: vec![] };
        let pub_ = MuxPublisher::with_config(p, cfg).unwrap();
        pub_.send_data(&[0x42; 64], Pts90khz::new(0)).unwrap();
        assert!(pub_.stats().bytes_pushed > 0);
    }

    #[test]
    fn debug_impl_does_not_panic() {
        let p = MemoryPublisher { buffers: vec![] };
        let pub_ = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        let _ = format!("{:?}", pub_);
    }

    /// A publisher that panics inside `push_ts` — used to poison the inner
    /// mutex while it is held, simulating a mid-mutation panic.
    struct PanicPublisher;

    impl Publisher for PanicPublisher {
        type Error = MemErr;
        fn push_ts(&mut self, _ts: &[u8]) -> Result<(), MemErr> {
            panic!("intentional panic to poison MuxPublisher mutex");
        }
        fn cut_segment(&mut self) -> Result<(), MemErr> {
            Ok(())
        }
        fn finish(self) -> Result<(), MemErr> {
            Ok(())
        }
        fn stats(&self) -> PublisherStats {
            PublisherStats::default()
        }
    }

    /// After the inner mutex is poisoned by a mid-mutation panic,
    /// every public method must NOT panic:
    ///   - fallible send/cut methods → `Err(MuxPublisherError::LockPoisoned)`
    ///   - infallible stats methods → return a value without panicking
    ///   - `finish` → returns `Ok(publisher)` by recovering the poisoned guard
    #[test]
    fn lock_poison_no_panic_and_correct_error() {
        // Build a publisher whose push_ts panics.  The muxer will produce TS
        // bytes on the first valid video push, calling push_ts and triggering
        // the panic while the inner Mutex guard is held — poisoning the mutex.
        let pub_ = MuxPublisher::with_config(PanicPublisher, test_muxer_config()).unwrap();

        // Catch the panic so the test thread survives.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let nal = h264_au();
            let _ = pub_.send_video(&nal, Pts90khz::new(0), true);
        }));
        assert!(result.is_err(), "expected a panic from push_ts");

        // The inner mutex is now poisoned.  No subsequent call must panic.

        // All fallible push methods → LockPoisoned.
        assert!(
            matches!(
                pub_.send_video(&[], Pts90khz::new(0), false),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "send_video: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.send_klv(&[], Pts90khz::new(0), 0),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "send_klv: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.send_audio(&[], Pts90khz::new(0)),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "send_audio: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.send_subtitle(&[], Pts90khz::new(0)),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "send_subtitle: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.send_data(&[], Pts90khz::new(0)),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "send_data: expected LockPoisoned"
        );
        assert!(
            matches!(pub_.cut_segment(), Err(MuxPublisherError::LockPoisoned)),
            "cut_segment: expected LockPoisoned"
        );

        // LockPoisoned kind → TransportBroken.
        assert_eq!(
            pub_.send_video(&[], Pts90khz::new(0), false)
                .unwrap_err()
                .kind(),
            crate::shell_error::ShellErrorKind::TransportBroken,
        );

        // Infallible methods recover the poisoned guard — must not panic.
        let _ = pub_.stats();
        let _ = pub_.publisher_stats();

        // finish recovers the poisoned guard and returns the publisher.
        pub_.finish()
            .expect("finish must succeed even on a poisoned lock");
    }

    struct RecordingPublisher {
        cuts: Vec<Duration>,
    }

    impl Publisher for RecordingPublisher {
        type Error = MemErr;
        fn push_ts(&mut self, _ts: &[u8]) -> Result<(), MemErr> {
            Ok(())
        }
        fn cut_segment(&mut self) -> Result<(), MemErr> {
            self.cuts.push(Duration::ZERO);
            Ok(())
        }
        fn cut_segment_with_duration(&mut self, d: Duration) -> Result<(), MemErr> {
            self.cuts.push(d);
            Ok(())
        }
        fn finish(self) -> Result<(), MemErr> {
            Ok(())
        }
        fn stats(&self) -> PublisherStats {
            PublisherStats::default()
        }
    }

    fn h264_au() -> Vec<u8> {
        // AUD + one slice NAL + filler — the muxer packetizes the bytes; the
        // key_frame *bool* (not the NAL type) drives the segment cut.
        let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10];
        v.extend([0x00, 0x00, 0x00, 0x01, 0x65]);
        v.extend(std::iter::repeat(0xab).take(64));
        v
    }

    #[test]
    fn send_video_derives_media_duration_from_pts() {
        // Keyframes BEGIN segments (cut-before-push): IDR@0 is the stream
        // head — it opens the first segment and emits NO cut. P@9000, P@18000
        // extend it; IDR@270000 closes it with the PTS span 0..270000 =
        // 270000 ticks = 3.0 s, then begins the next segment.
        let p = RecordingPublisher { cuts: vec![] };
        let mp = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        let au = h264_au();
        mp.send_video(&au, Pts90khz::new(0), true).unwrap();
        mp.send_video(&au, Pts90khz::new(9000), false).unwrap();
        mp.send_video(&au, Pts90khz::new(18000), false).unwrap();
        mp.send_video(&au, Pts90khz::new(270000), true).unwrap();
        let p = mp.finish().unwrap();
        assert_eq!(p.cuts.len(), 1, "stream-head IDR must not emit a cut");
        assert_eq!(
            p.cuts[0],
            Duration::from_nanos(270_000 * 1_000_000_000 / 90_000)
        );
    }

    /// One recorded publisher operation, in call order.
    #[derive(Debug, PartialEq)]
    enum Op {
        /// A `push_ts` of this many bytes.
        Push(usize),
        /// A `cut_segment_with_duration` of this media duration.
        Cut(Duration),
    }

    /// Test double that records the exact interleaving of pushes and cuts so
    /// tests can prove a GOP's bytes land in the segment that BEGINS with its
    /// keyframe (i.e. all bytes of one GOP come before the cut, and the next
    /// keyframe's bytes come after it).
    struct OpLogPublisher {
        ops: Vec<Op>,
    }

    impl Publisher for OpLogPublisher {
        type Error = MemErr;
        fn push_ts(&mut self, ts: &[u8]) -> Result<(), MemErr> {
            self.ops.push(Op::Push(ts.len()));
            Ok(())
        }
        fn cut_segment(&mut self) -> Result<(), MemErr> {
            self.ops.push(Op::Cut(Duration::ZERO));
            Ok(())
        }
        fn cut_segment_with_duration(&mut self, d: Duration) -> Result<(), MemErr> {
            self.ops.push(Op::Cut(d));
            Ok(())
        }
        fn finish(self) -> Result<(), MemErr> {
            Ok(())
        }
        fn stats(&self) -> PublisherStats {
            PublisherStats::default()
        }
    }

    #[test]
    fn keyframe_begins_segment_push_before_cut() {
        // AU sequence K1 P P K2 P K3 at 9000-tick spacing. Segments begin with
        // their keyframe: the first GOP (K1 P P) is pushed BEFORE the first cut,
        // and K2's bytes land AFTER it. Exactly two cuts (for K2 and K3 — the
        // stream-head K1 emits none). First cut span = pts(K2) - pts(K1).
        let p = OpLogPublisher { ops: vec![] };
        let mp = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        let au = h264_au();
        let k1 = 0;
        let k2 = 3 * 9000; // K1 + 3 AUs (K1 P P) → K2 at index 3
        let k3 = 5 * 9000; // K2 + 2 AUs (K2 P) → K3 at index 5
        mp.send_video(&au, Pts90khz::new(k1), true).unwrap(); // K1
        mp.send_video(&au, Pts90khz::new(9000), false).unwrap(); // P
        mp.send_video(&au, Pts90khz::new(18000), false).unwrap(); // P
        mp.send_video(&au, Pts90khz::new(k2), true).unwrap(); // K2
        mp.send_video(&au, Pts90khz::new(4 * 9000), false).unwrap(); // P
        mp.send_video(&au, Pts90khz::new(k3), true).unwrap(); // K3
        let p = mp.finish().unwrap();

        // Exactly two cuts (K2, K3 — none for the stream-head K1).
        let cut_positions: Vec<usize> = p
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, Op::Cut(_)))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(cut_positions.len(), 2, "ops: {:?}", p.ops);

        // Some GOP bytes were pushed before the first cut (K1 P P), and some
        // after it (K2 …) — proving segments begin with the keyframe.
        let first_cut = cut_positions[0];
        assert!(
            p.ops[..first_cut]
                .iter()
                .any(|op| matches!(op, Op::Push(_))),
            "K1 GOP bytes must precede the first cut; ops: {:?}",
            p.ops
        );
        assert!(
            p.ops[first_cut + 1..]
                .iter()
                .any(|op| matches!(op, Op::Push(_))),
            "K2 bytes must follow the first cut; ops: {:?}",
            p.ops
        );

        // First cut duration == pts(K2) - pts(K1).
        let first_cut_dur = match &p.ops[first_cut] {
            Op::Cut(d) => *d,
            _ => unreachable!(),
        };
        assert_eq!(
            first_cut_dur,
            Duration::from_nanos((k2 - k1) as u64 * 1_000_000_000 / 90_000)
        );
    }

    /// `send_video_misp` splices the ST 0604 SEI; bytes demux to a Video
    /// sample from which `misp_time::extract` recovers the timestamp.
    #[test]
    fn send_video_misp_recovers_timestamp() {
        use tst_core::codec::misp_time::MispTimestamp;
        use tst_core::mpegts::demux::event::{DemuxEvent, SamplePayload};
        use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
        use tst_core::mpegts::mux::VideoCodec;

        let p = MemoryPublisher { buffers: vec![] };
        let mp = MuxPublisher::with_config(p, test_muxer_config()).unwrap();

        // AUD + SPS + PPS + IDR — canonical H.264 keyframe AU.
        let au: Vec<u8> = {
            fn nal(nal_type: u8, nri: u8, body: &[u8]) -> Vec<u8> {
                let mut v = vec![0x00, 0x00, 0x00, 0x01, (nri << 5) | nal_type];
                v.extend_from_slice(body);
                v
            }
            let mut au = Vec::new();
            au.extend(nal(9, 0b00, &[0xF0])); // AUD
            au.extend(nal(7, 0b11, &[0x42, 0xC0, 0x28])); // SPS
            au.extend(nal(8, 0b11, &[0xCE, 0x38])); // PPS
            au.extend(nal(5, 0b11, &[0x88, 0x84, 0x0A])); // IDR
            au
        };
        let misp = MispTimestamp::micros(0xDEAD_BEEF_0000_0001, 0x3F);
        mp.send_video_misp(&au, Pts90khz::new(0), true, &misp)
            .unwrap();

        let publisher = mp.finish().unwrap();
        // The MemoryPublisher accumulates TS bytes in its first buffer.
        let ts_bytes: Vec<u8> = publisher.buffers.into_iter().flatten().collect();
        assert!(
            !ts_bytes.is_empty(),
            "publisher must have received TS bytes"
        );

        let mut demuxer = Demuxer::with_config(DemuxerConfig::builder().build());
        demuxer.feed(&ts_bytes).unwrap();
        demuxer.flush();
        let mut found_misp: Option<MispTimestamp> = None;
        loop {
            match demuxer.next_event() {
                Some(DemuxEvent::Sample {
                    payload: SamplePayload::Video { raw, .. },
                    ..
                }) => {
                    let extracted =
                        tst_core::codec::misp_time::extract(&raw, VideoCodec::H264).unwrap();
                    if extracted.is_some() {
                        found_misp = extracted;
                        break;
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        let recovered = found_misp.expect("MISP timestamp must be present in demuxed AU");
        assert_eq!(recovered.value, misp.value, "timestamp value mismatch");
        assert_eq!(
            recovered.time_status, misp.time_status,
            "status byte mismatch"
        );
    }
}
