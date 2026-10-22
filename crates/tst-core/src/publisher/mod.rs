//! Outbound-only, segment-aware sinks (HLS, future MPEG-DASH, ...).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Sits alongside [`crate::transport::Transport`] and
//! [`crate::transport::RecvTransport`] as a third trait family.
//!
//! Unlike `Transport`, a `Publisher` is outbound-only — there is no peer to
//! receive from. Unlike a simple byte sink, a `Publisher` is aware of segment
//! boundaries (e.g., HLS .ts segment cuts, .m3u8 playlist rotation).

use core::time::Duration;

/// A sink for MPEG-TS bytes that produces a segmented output stream.
pub trait Publisher {
    /// Concrete error type returned by this publisher.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Push MPEG-TS bytes for the current segment.
    ///
    /// Bytes must be a whole multiple of 188.  The publisher is free to
    /// buffer; segment cuts happen at the next [`cut_segment`] call or
    /// when the configured segment duration cap is reached.
    ///
    /// [`cut_segment`]: Self::cut_segment
    fn push_ts(&mut self, ts_bytes: &[u8]) -> Result<(), Self::Error>;

    /// Hint that the next call to [`push_ts`] should start a new segment.
    ///
    /// Callers should invoke this on keyframe boundaries (IDR / I-frame)
    /// so segments are decodable from byte zero.  May be a no-op for
    /// publishers that segment purely on duration.
    ///
    /// [`push_ts`]: Self::push_ts
    fn cut_segment(&mut self) -> Result<(), Self::Error>;

    /// Hint that the next call to [`push_ts`] should start a new segment,
    /// supplying the media-presentation duration of the segment being
    /// closed (derived from PTS, not wall-clock).
    ///
    /// Publishers that emit per-segment durations (e.g. HLS `#EXTINF`) should
    /// record `media_duration` rather than measuring wall-clock ingestion
    /// time — RFC 8216 §4.3.2.1 defines a segment's duration in terms of
    /// media-presentation time. The `MuxPublisher` pipeline shell derives
    /// `media_duration` from the PTS span of the segment and calls this
    /// method; raw `push_ts` callers without PTS keep the wall-clock fallback.
    ///
    /// The default implementation ignores `media_duration` and delegates to
    /// [`cut_segment`], so existing publishers keep compiling unchanged.
    ///
    /// [`push_ts`]: Self::push_ts
    /// [`cut_segment`]: Self::cut_segment
    fn cut_segment_with_duration(&mut self, media_duration: Duration) -> Result<(), Self::Error> {
        let _ = media_duration;
        self.cut_segment()
    }

    /// The longest a segment may run, in media time, when the publisher
    /// advertises a fixed bound to its clients (the HLS target duration).
    ///
    /// The `MuxPublisher` pipeline shell reads this once at construction.
    /// `Some(d)`: it cuts on the first keyframe at or after `d` of media time
    /// and force-cuts early enough that, at a steady frame cadence, every
    /// segment rounds to at most the advertised bound; the shell's
    /// `Segmented` transport documents the exact rules. `None` (the
    /// default): every keyframe after the first cuts.
    fn segment_duration(&self) -> Option<Duration> {
        None
    }

    /// The shell tells the publisher that keyframes, not the wall clock,
    /// will drive its cuts.
    ///
    /// The `MuxPublisher` pipeline shell calls this once, on its first video
    /// access unit, when [`segment_duration`] is `Some`. It closes nothing:
    /// a publisher with a wall-clock fallback (HLS) keeps that only as a
    /// stalled-producer guard from then on, so the segment already open,
    /// even one that metadata or audio opened before the first video access
    /// unit, is cut by the shell's media-time bound. The default does
    /// nothing.
    ///
    /// [`segment_duration`]: Self::segment_duration
    fn keyframe_driven(&mut self) {}

    /// Cleanly finalize: flush pending segment, write a terminating
    /// playlist tag (e.g., HLS `#EXT-X-ENDLIST`), tear down sinks.
    ///
    /// Consumes `self`.  After this returns the publisher's resources
    /// (HTTP server, file handles, runtime) are released.
    fn finish(self) -> Result<(), Self::Error>
    where
        Self: Sized;

    /// Snapshot of publisher health.  Cheap to call (read-only over
    /// atomic counters).
    fn stats(&self) -> PublisherStats;
}

/// Universal cross-publisher stats.
///
/// Concrete impls expose richer stats via an inherent `stats() -> ConcreteStats`
/// method that this trait method projects from.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PublisherStats {
    /// Total number of completed segments written.
    pub segments_written: u64,
    /// Total bytes pushed (counts toward whatever sink the publisher uses;
    /// for HLS this is bytes written to .ts files).
    pub bytes_written: u64,
    /// Wall-clock age of the segment currently open for writes.  `None`
    /// when no segment is open (between cuts or before first push).
    pub current_segment_age: Option<Duration>,
    /// Wall-clock duration of the most recently completed segment.
    pub last_segment_duration: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal Publisher impl proving the trait is object-safe-via-generics
    /// and that `finish` consumes self correctly.
    struct NoopPublisher {
        segments: u64,
        bytes: u64,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("noop")]
    struct NoopErr;

    impl Publisher for NoopPublisher {
        type Error = NoopErr;
        fn push_ts(&mut self, ts: &[u8]) -> Result<(), NoopErr> {
            self.bytes += ts.len() as u64;
            Ok(())
        }
        fn cut_segment(&mut self) -> Result<(), NoopErr> {
            self.segments += 1;
            Ok(())
        }
        fn finish(self) -> Result<(), NoopErr> {
            Ok(())
        }
        fn stats(&self) -> PublisherStats {
            PublisherStats {
                segments_written: self.segments,
                bytes_written: self.bytes,
                ..Default::default()
            }
        }
    }

    #[test]
    fn default_cut_with_duration_delegates_to_cut_segment() {
        let mut p = NoopPublisher {
            segments: 0,
            bytes: 0,
        };
        p.cut_segment_with_duration(core::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            p.stats().segments_written,
            1,
            "default impl must delegate to cut_segment"
        );
    }

    #[test]
    fn default_segment_duration_is_none() {
        let p = NoopPublisher {
            segments: 0,
            bytes: 0,
        };
        assert_eq!(p.segment_duration(), None);
    }
}
