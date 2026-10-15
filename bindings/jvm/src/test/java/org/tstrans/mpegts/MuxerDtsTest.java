package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.drain;
import static org.tstrans.TestSupport.syntheticH264IdrPrefix;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * Exercises the DTS-aware targeted push family on the offline {@link Muxer}:
 * {@link Muxer#pushVideoToWithDts} and {@link Muxer#pushVideoWireToWithDts}.
 * Mirrors the core {@code push_video_to_with_dts_emits_pts_dts_flags_11} test:
 * a distinct PTS/DTS round-trips through mux→demux as non-null DTS on
 * {@link DemuxEvent.Video}.
 */
class MuxerDtsTest {

    /**
     * {@link Muxer#pushVideoToWithDts} preserves distinct PTS and DTS through
     * a mux→demux round-trip. The demuxed {@link DemuxEvent.Video} must carry
     * a non-null {@code dts} equal to the pushed value, and {@code pts} must
     * equal the pushed presentation timestamp.
     */
    @Test
    void pushVideoToWithDtsPreservesDistinctPtsDts() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        byte[] ts;
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            m.pushVideoToWithDts(h, syntheticH264IdrPrefix(), 9000L, 6000L, true);
            ts = drain(m);
        }

        List<DemuxEvent.Video> videos = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent ev : d) {
                if (ev instanceof DemuxEvent.Video v) videos.add(v);
            }
        }

        assertEquals(1, videos.size(), "expected exactly one video event");
        DemuxEvent.Video v = videos.get(0);
        assertEquals(9000L, v.pts(), "pts must survive round-trip");
        assertNotNull(v.dts(), "dts must be non-null for PTS+DTS push");
        assertEquals(6000L, v.dts(), "dts must survive round-trip");
    }

    /**
     * {@link Muxer#pushVideoWireToWithDts} preserves distinct PTS and DTS.
     * Wire push is the byte-faithful re-mux path (Annex-B bytes are valid wire
     * for H.264 — no re-wrapping). Same round-trip assertion as the non-wire
     * variant.
     */
    @Test
    void pushVideoWireToWithDtsPreservesDistinctPtsDts() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        byte[] ts;
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            m.pushVideoWireToWithDts(h, syntheticH264IdrPrefix(), 9000L, 6000L, true);
            ts = drain(m);
        }

        List<DemuxEvent.Video> videos = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent ev : d) {
                if (ev instanceof DemuxEvent.Video v) videos.add(v);
            }
        }

        assertEquals(1, videos.size(), "expected exactly one video event");
        DemuxEvent.Video v = videos.get(0);
        assertEquals(9000L, v.pts(), "pts must survive round-trip");
        assertNotNull(v.dts(), "dts must be non-null for PTS+DTS wire push");
        assertEquals(6000L, v.dts(), "dts must survive round-trip");
    }

    /**
     * A forged video handle passed to {@code pushVideoToWithDts} must surface
     * as {@link org.tstrans.MuxException.Kind#INVALID_USAGE}, not a crash.
     */
    @Test
    void forgedHandleOnDtsPushThrowsInvalidUsage() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle forged = VideoStreamHandle.fromRaw(0x1000L);
            org.tstrans.MuxException ex = assertThrows(org.tstrans.MuxException.class,
                () -> m.pushVideoToWithDts(forged, syntheticH264IdrPrefix(), 9000L, 6000L, true));
            assertEquals(org.tstrans.MuxException.Kind.INVALID_USAGE, ex.kind());
        }
    }
}
