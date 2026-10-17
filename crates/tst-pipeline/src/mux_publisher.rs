//! [`MuxPublisher`] — pipeline shell that owns a muxer and pushes its
//! output to a [`Publisher`].
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use std::time::Duration;

use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{MuxStreamKind, MuxerConfig};
use tst_core::publisher::{Publisher, PublisherStats};
use tst_core::transport::{BrokenCause, Transport, TransportError};

use crate::binding::kind::{coarsen, kind_of_mux};
use crate::mux_sender::{MuxSender, MuxSenderError, MuxSenderErrorSource, PartsError};
use crate::shell_error::ShellErrorKind;

tst_core::binding_record! {
    /// Cumulative stats of a mux-publisher shell.
    #[derive(Debug, Clone, Copy, Default)]
    #[non_exhaustive]
    pub struct MuxPublisherStats {
        /// Total TS bytes drained from the muxer and handed to the publisher.
        pub bytes_pushed: u64,
        /// Total muxer drain calls that produced at least one chunk.
        pub drain_calls: u64,
        /// Total explicit segment cuts, plus the automatic cuts on keyframes.
        pub cut_calls: u64,
    }
}

/// A [`Publisher`] seen as a [`Transport`]: the transport half of
/// [`MuxPublisher`], usable on its own as `MuxSender<Segmented<P>>`.
///
/// Muxed bytes go to [`Publisher::push_ts`]. A keyframe BEGINS the next
/// segment: [`Transport::on_video_au`] cuts the closing segment (EXTINF =
/// the exact PTS span `segment_start..this keyframe`) and asks the caller
/// to re-emit PAT/PMT, so the new segment opens PAT → PMT → IDR and is
/// independently decodable (RFC 8216 §3). The stream-head keyframe, with no
/// open segment yet, opens the first segment without a zero-duration cut.
///
/// `last_video_pts` and the open segment's start are recorded when the hook
/// runs, before the muxer push. A push the muxer then rejects (a full
/// buffer, a malformed NAL) therefore still advances them, and a following
/// explicit cut writes an `#EXTINF` that runs to the rejected access unit's
/// PTS.
pub struct Segmented<P: Publisher> {
    publisher: P,
    /// PTS of the first AU pushed into the segment currently being built;
    /// `None` immediately after a cut (the next AU re-baselines it).
    segment_start_pts: Option<Pts90khz>,
    /// PTS of the most recent video AU (drives an explicit cut).
    last_video_pts: Option<Pts90khz>,
    cut_calls: u64,
    bytes_pushed: u64,
    drain_calls: u64,
    /// The publisher error behind the last `TransportError::Broken` this
    /// transport returned, kept typed for [`MuxPublisher`] to hand back. It
    /// is stored under the failing thread's id inside the sender's critical
    /// section, so concurrent failing callers each get their own error back.
    last_error: Vec<(std::thread::ThreadId, P::Error)>,
}

impl<P: Publisher> Segmented<P> {
    /// Wrap a publisher.
    pub fn new(publisher: P) -> Self {
        Self {
            publisher,
            segment_start_pts: None,
            last_video_pts: None,
            cut_calls: 0,
            bytes_pushed: 0,
            drain_calls: 0,
            last_error: Vec::new(),
        }
    }

    /// Return the wrapped publisher.
    pub fn into_publisher(self) -> P {
        self.publisher
    }

    /// Keep the typed error and report it to the sender as a broken transport.
    fn fail(&mut self, e: P::Error) -> TransportError {
        let msg = format!("publisher error: {e}");
        let me = std::thread::current().id();
        self.last_error.retain(|(t, _)| *t != me);
        self.last_error.push((me, e));
        TransportError::Broken {
            msg,
            errno_code: None,
            cause: BrokenCause::Unspecified,
        }
    }

    /// Take the error the current thread's last failed call stored.
    fn take_error(&mut self) -> Option<P::Error> {
        let me = std::thread::current().id();
        let i = self.last_error.iter().position(|(t, _)| *t == me)?;
        Some(self.last_error.swap_remove(i).1)
    }

    /// Cut the open segment with its media duration and start a new one.
    fn cut(&mut self, media_dur: Duration) -> Result<(), P::Error> {
        self.publisher.cut_segment_with_duration(media_dur)?;
        self.cut_calls = self.cut_calls.saturating_add(1);
        self.segment_start_pts = None;
        Ok(())
    }
}

impl<P: Publisher + Send> Transport for Segmented<P> {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        match self.publisher.push_ts(msg) {
            Ok(()) => {
                self.bytes_pushed = self.bytes_pushed.saturating_add(msg.len() as u64);
                self.drain_calls = self.drain_calls.saturating_add(1);
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    fn max_payload(&self) -> usize {
        16 * 1024
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn close(&mut self) {}

    fn on_video_au(&mut self, pts: Pts90khz, key_frame: bool) -> Result<bool, TransportError> {
        let mut cut = false;
        if let (true, Some(start)) = (key_frame, self.segment_start_pts) {
            if let Err(e) = self.cut(media_span(start, pts)) {
                return Err(self.fail(e));
            }
            cut = true;
        }
        if self.segment_start_pts.is_none() {
            self.segment_start_pts = Some(pts);
        }
        self.last_video_pts = Some(pts);
        Ok(cut)
    }
}

/// Shell that owns a muxer and pushes its output to a [`Publisher`].
/// Mirrors [`crate::MuxSender`]: it is a [`MuxSender`] over a [`Segmented`]
/// transport, so it drains, applies back-pressure and reports input
/// consumption the same way.
///
/// # Poison policy
///
/// No method panics on a poisoned inner mutex. If a prior call panicked
/// mid-mutation and poisoned the lock, each method falls back gracefully:
/// fallible methods (`push_*`, `cut_segment`) return
/// [`MuxPublisherError::LockPoisoned`]; infallible methods (`stats`,
/// `publisher_stats`) recover the poisoned guard and return the last
/// observed value. `finish` recovers the poisoned guard and returns the
/// owned publisher — the publisher's state may be partial if the panic
/// occurred during a push, but the caller receives it for its own
/// disposal.
pub struct MuxPublisher<P: Publisher + Send>(MuxSender<Segmented<P>>);

impl<P: Publisher + Send> std::fmt::Debug for MuxPublisher<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MuxPublisher").field(&self.0).finish()
    }
}

impl<P: Publisher + Send> MuxPublisher<P> {
    /// Build a publisher shell from a publisher + muxer config.
    pub fn with_config(
        publisher: P,
        config: MuxerConfig,
    ) -> Result<Self, MuxPublisherError<P::Error>> {
        MuxSender::new(Segmented::new(publisher), config)
            .map(Self)
            .map_err(MuxPublisherError::Mux)
    }

    /// Turn a sender error back into this shell's error type: a muxer
    /// rejection stays one, a poisoned lock is reported as such, and a
    /// transport failure hands back the publisher's own error.
    fn map_err(&self, e: MuxSenderError) -> MuxPublisherError<P::Error> {
        if self.0.is_poisoned() {
            return MuxPublisherError::LockPoisoned;
        }
        match e.source {
            MuxSenderErrorSource::Mux(m) => MuxPublisherError::Mux(m),
            _ => match self.0.with_transport(Segmented::take_error) {
                Some(pe) => MuxPublisherError::Publisher(pe),
                None => MuxPublisherError::Closed,
            },
        }
    }

    fn done(&self, r: Result<(), MuxSenderError>) -> Result<(), MuxPublisherError<P::Error>> {
        r.map_err(|e| self.map_err(e))
    }

    /// Push one video access unit (Annex-B framing required).
    ///
    /// A keyframe BEGINS the next segment: before this AU is pushed, the
    /// closing segment is flushed and cut (EXTINF = the exact PTS span
    /// `segment_start..this keyframe`), then PSI (PAT/PMT) is re-emitted so
    /// the new segment opens PAT → PMT → IDR and is independently decodable
    /// (RFC 8216 §3). The keyframe's own bytes then land at the HEAD of the
    /// fresh segment. The stream-head keyframe (no open segment yet) opens
    /// the first segment without a spurious zero-duration cut.
    pub fn push_video(
        &self,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        self.done(self.0.send_video(nal, pts, key_frame))
    }

    /// [`Self::push_video`] plus an ST 0604 MISP timestamp SEI spliced
    /// before the first VCL NAL (see `Muxer::push_video_misp_to`).
    /// Requires exactly one configured video stream (like `push_video`).
    pub fn push_video_misp(
        &self,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
        misp: &tst_core::codec::misp_time::MispTimestamp,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        let handles = self.0.video_handles();
        let [handle] = handles.as_slice() else {
            return Err(MuxPublisherError::Mux(MuxError::AmbiguousTarget {
                kind: MuxStreamKind::Video,
                count: handles.len(),
            }));
        };
        self.done(
            self.0
                .send_video_misp_to(*handle, nal, pts, key_frame, misp),
        )
    }

    /// Push one KLV unit.
    ///
    /// `metadata_service_id` selects the KLV stream when multiple KLV PIDs
    /// are configured; pass `0` for single-stream muxer configs.
    pub fn push_klv(
        &self,
        klv: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        self.done(self.0.send_klv(klv, pts, metadata_service_id))
    }

    /// Push one audio frame.
    pub fn push_audio(
        &self,
        frames: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        self.done(self.0.send_audio(frames, pts))
    }

    /// Push one subtitle payload.
    pub fn push_subtitle(
        &self,
        payload: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxPublisherError<P::Error>> {
        self.done(self.0.send_subtitle(payload, pts))
    }

    /// Push one data payload on the muxer's single data stream.
    ///
    /// Data streams are a PES pass-through — no AU-cell wrap, no framing;
    /// [`Muxer::push_data_to`] holds the contract (`pts` is written into
    /// the PES header only for `carries_pts: true` streams).
    ///
    /// [`Muxer::push_data_to`]: tst_core::mpegts::mux::Muxer::push_data_to
    pub fn push_data(&self, data: &[u8], pts: Pts90khz) -> Result<(), MuxPublisherError<P::Error>> {
        self.done(self.0.send_data(data, pts))
    }

    /// Explicit segment-cut hint.
    pub fn cut_segment(&self) -> Result<(), MuxPublisherError<P::Error>> {
        let cut = self.0.with_parts(|muxer, seg| {
            let media_dur = match (seg.segment_start_pts, seg.last_video_pts) {
                (Some(start), Some(end)) => media_span(start, end),
                _ => Duration::ZERO,
            };
            seg.cut(media_dur)?;
            muxer.request_psi();
            Ok(())
        });
        match cut {
            Ok(r) => r.map_err(MuxPublisherError::Publisher),
            Err(PartsError::Poisoned) => Err(MuxPublisherError::LockPoisoned),
            Err(PartsError::Closed) => Err(MuxPublisherError::Closed),
        }
    }

    /// Snapshot stats.
    pub fn stats(&self) -> MuxPublisherStats {
        self.0.with_transport(|t| MuxPublisherStats {
            bytes_pushed: t.bytes_pushed,
            drain_calls: t.drain_calls,
            cut_calls: t.cut_calls,
        })
    }

    /// Publisher-side stats (universal subset across publisher impls).
    pub fn publisher_stats(&self) -> PublisherStats {
        self.0.with_transport(|t| t.publisher.stats())
    }

    /// Consume the shell and return the owned inner publisher.
    ///
    /// No end-of-stream marker is written: if the caller needs to finalize
    /// the publisher (flush the pending segment, write an HLS
    /// `#EXT-X-ENDLIST` tag, tear down sinks, etc.), they should call
    /// [`Publisher::finish`] on the returned publisher. Bytes a failed
    /// earlier push left queued are offered to the publisher once more on
    /// the way out ([`MuxSender::into_inner`]).
    ///
    /// If the inner lock was poisoned, the publisher is still returned:
    /// `into_inner` recovers the poisoned guard, and the publisher
    /// (possibly in partial state if the panic interrupted a push) is
    /// handed back for caller disposal.
    pub fn finish(self) -> Result<P, MuxPublisherError<P::Error>> {
        Ok(self.0.into_inner().into_publisher())
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
            Self::Mux(e) => coarsen(kind_of_mux(e)),
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

    /// A publisher whose every push fails with the id of the failing thread.
    struct ThreadErrPublisher;

    #[derive(Debug, thiserror::Error)]
    #[error("failed on {0}")]
    struct ThreadErr(String);

    impl Publisher for ThreadErrPublisher {
        type Error = ThreadErr;
        fn push_ts(&mut self, _: &[u8]) -> Result<(), ThreadErr> {
            Err(ThreadErr(format!("{:?}", std::thread::current().id())))
        }
        fn cut_segment(&mut self) -> Result<(), ThreadErr> {
            Ok(())
        }
        fn finish(self) -> Result<(), ThreadErr> {
            Ok(())
        }
        fn stats(&self) -> PublisherStats {
            PublisherStats::default()
        }
    }

    #[test]
    fn concurrent_failing_pushes_each_get_their_own_publisher_error() {
        use std::sync::Barrier;
        let shell = MuxPublisher::with_config(ThreadErrPublisher, test_muxer_config()).unwrap();
        let rounds = 5_000;
        let barrier = Barrier::new(4);
        let wrong = std::sync::atomic::AtomicUsize::new(0);
        let au = [0u8, 0, 0, 1, 0x65, 1, 2, 3];
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let me = format!("{:?}", std::thread::current().id());
                    for i in 0..rounds {
                        barrier.wait();
                        // Count instead of panicking: a panic here would leave
                        // the other threads parked at the barrier.
                        match shell.push_video(&au, Pts90khz::new(i), true) {
                            Err(MuxPublisherError::Publisher(e)) if e.0 == me => {}
                            _ => {
                                wrong.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        };
                    }
                });
            }
        });
        assert_eq!(
            wrong.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a caller got another thread's error or Closed"
        );
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
    fn push_data_pushes_ts_bytes() {
        use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_data(0x101, 0xF0, /*carries_pts=*/ true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let cfg = b.build().unwrap();
        let p = MemoryPublisher { buffers: vec![] };
        let pub_ = MuxPublisher::with_config(p, cfg).unwrap();
        pub_.push_data(&[0x42; 64], Pts90khz::new(0)).unwrap();
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
            let _ = pub_.push_video(&nal, Pts90khz::new(0), true);
        }));
        assert!(result.is_err(), "expected a panic from push_ts");

        // The inner mutex is now poisoned.  No subsequent call must panic.

        // All fallible push methods → LockPoisoned.
        assert!(
            matches!(
                pub_.push_video(&[], Pts90khz::new(0), false),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "push_video: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.push_klv(&[], Pts90khz::new(0), 0),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "push_klv: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.push_audio(&[], Pts90khz::new(0)),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "push_audio: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.push_subtitle(&[], Pts90khz::new(0)),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "push_subtitle: expected LockPoisoned"
        );
        assert!(
            matches!(
                pub_.push_data(&[], Pts90khz::new(0)),
                Err(MuxPublisherError::LockPoisoned)
            ),
            "push_data: expected LockPoisoned"
        );
        assert!(
            matches!(pub_.cut_segment(), Err(MuxPublisherError::LockPoisoned)),
            "cut_segment: expected LockPoisoned"
        );

        // LockPoisoned kind → TransportBroken.
        assert_eq!(
            pub_.push_video(&[], Pts90khz::new(0), false)
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
    fn push_video_derives_media_duration_from_pts() {
        // Keyframes BEGIN segments (cut-before-push): IDR@0 is the stream
        // head — it opens the first segment and emits NO cut. P@9000, P@18000
        // extend it; IDR@270000 closes it with the PTS span 0..270000 =
        // 270000 ticks = 3.0 s, then begins the next segment.
        let p = RecordingPublisher { cuts: vec![] };
        let mp = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        let au = h264_au();
        mp.push_video(&au, Pts90khz::new(0), true).unwrap();
        mp.push_video(&au, Pts90khz::new(9000), false).unwrap();
        mp.push_video(&au, Pts90khz::new(18000), false).unwrap();
        mp.push_video(&au, Pts90khz::new(270000), true).unwrap();
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
        mp.push_video(&au, Pts90khz::new(k1), true).unwrap(); // K1
        mp.push_video(&au, Pts90khz::new(9000), false).unwrap(); // P
        mp.push_video(&au, Pts90khz::new(18000), false).unwrap(); // P
        mp.push_video(&au, Pts90khz::new(k2), true).unwrap(); // K2
        mp.push_video(&au, Pts90khz::new(4 * 9000), false).unwrap(); // P
        mp.push_video(&au, Pts90khz::new(k3), true).unwrap(); // K3
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

    /// `MuxSender<Segmented<P>>` records the same pushes and cuts as the
    /// `MuxPublisher` shell over it, for the same access units.
    #[test]
    fn segmented_sender_matches_mux_publisher_ops() {
        use crate::mux_sender::MuxSender;
        let au = h264_au();
        let feed = [(0i64, true), (9000, false), (270_000, true)];

        let mp =
            MuxPublisher::with_config(OpLogPublisher { ops: vec![] }, test_muxer_config()).unwrap();
        for (t, key) in feed {
            mp.push_video(&au, Pts90khz::new(t), key).unwrap();
        }
        let expected = mp.finish().unwrap().ops;

        let sender = MuxSender::new(
            Segmented::new(OpLogPublisher { ops: vec![] }),
            test_muxer_config(),
        )
        .unwrap();
        for (t, key) in feed {
            sender.send_video(&au, Pts90khz::new(t), key).unwrap();
        }
        let got = sender.into_inner().into_publisher().ops;

        assert_eq!(got, expected);
        // The vector the shell recorded before it drained through a
        // `MuxSender`: one cut, 3 s wide, between the first two AUs' bytes
        // and the keyframe's.
        assert_eq!(
            got,
            [
                Op::Push(564),
                Op::Push(564),
                Op::Cut(Duration::from_secs(3)),
                Op::Push(564)
            ]
        );
    }

    /// A publisher failure surfaces as `MuxPublisherError::Publisher` with
    /// the publisher's own error, and the sender keeps the unsent bytes.
    #[test]
    fn publisher_error_is_handed_back_typed() {
        struct FailPush;
        impl Publisher for FailPush {
            type Error = MemErr;
            fn push_ts(&mut self, _: &[u8]) -> Result<(), MemErr> {
                Err(MemErr)
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
        let mp = MuxPublisher::with_config(FailPush, test_muxer_config()).unwrap();
        let err = mp
            .push_video(&h264_au(), Pts90khz::new(0), true)
            .unwrap_err();
        assert!(matches!(err, MuxPublisherError::Publisher(MemErr)), "{err}");
        assert_eq!(
            err.kind(),
            crate::shell_error::ShellErrorKind::TransportBroken
        );
        assert_eq!(err.to_string(), "publisher error: memory");
    }

    /// `push_video_misp` splices the ST 0604 SEI; bytes demux to a Video
    /// sample from which `misp_time::extract` recovers the timestamp.
    #[test]
    fn push_video_misp_recovers_timestamp() {
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
        mp.push_video_misp(&au, Pts90khz::new(0), true, &misp)
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

    /// A publisher whose first `push_ts` fails; every offered buffer is
    /// recorded, failed or not.
    struct FailOncePublisher {
        offered: Vec<Vec<u8>>,
        has_failed: bool,
    }

    impl Publisher for FailOncePublisher {
        type Error = MemErr;
        fn push_ts(&mut self, ts: &[u8]) -> Result<(), MemErr> {
            self.offered.push(ts.to_vec());
            if self.has_failed {
                Ok(())
            } else {
                self.has_failed = true;
                Err(MemErr)
            }
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

    /// A failed `push_ts` keeps its bytes queued, and the next call offers
    /// them to the publisher again before anything newer.
    #[test]
    fn failed_push_ts_keeps_its_bytes_and_the_next_call_reoffers_them() {
        let p = FailOncePublisher {
            offered: vec![],
            has_failed: false,
        };
        let mp = MuxPublisher::with_config(p, test_muxer_config()).unwrap();
        let au = h264_au();
        let err = mp.push_video(&au, Pts90khz::new(0), true).unwrap_err();
        assert!(matches!(err, MuxPublisherError::Publisher(_)), "{err:?}");

        mp.push_video(&au, Pts90khz::new(3000), false).unwrap();
        let p = mp.finish().unwrap();
        assert!(p.offered.len() >= 2, "{} offers", p.offered.len());
        assert_eq!(
            p.offered[0], p.offered[1],
            "the failed buffer is the first thing offered again"
        );
    }

    /// `push_video_misp` with other than one video stream errors before the
    /// keyframe cuts a segment.
    #[test]
    fn push_video_misp_with_two_video_streams_errors_before_any_cut() {
        use tst_core::codec::misp_time::MispTimestamp;
        use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_video(0x101, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let cfg = b.build().unwrap();

        let mp = MuxPublisher::with_config(RecordingPublisher { cuts: vec![] }, cfg).unwrap();
        let au = h264_au();
        let err = mp
            .push_video_misp(
                &au,
                Pts90khz::new(270_000),
                true,
                &MispTimestamp::micros(1, 0x1F),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                MuxPublisherError::Mux(MuxError::AmbiguousTarget { count: 2, .. })
            ),
            "{err:?}"
        );
        assert_eq!(mp.stats().cut_calls, 0);
        assert!(mp.finish().unwrap().cuts.is_empty());
    }
}
