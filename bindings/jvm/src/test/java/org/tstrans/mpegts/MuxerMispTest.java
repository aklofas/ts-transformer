package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;
import org.tstrans.codec.MispTimeKind;
import org.tstrans.codec.MispTimestamp;

/**
 * Exercises the MISP-timestamp mux/extract surface:
 * {@link Muxer#pushVideoMispTo} (with and without DTS),
 * {@link MispTimestamp#extract}, and error paths.
 *
 * <p>Mirrors the Rust {@code mux_misp_push} integration test shape:
 * push → mux → demux → extract raw bytes → compare.
 */
class MuxerMispTest {

    /** Annex-B IDR AU (same pattern as MuxerDtsTest). */
    private static byte[] syntheticH264Idr() {
        return new byte[]{0x00, 0x00, 0x00, 0x01, 0x65, (byte) 0x88, (byte) 0x84, 0x00};
    }

    /** Drain all TS bytes from a muxer into a byte array. */
    private static byte[] drain(Muxer m) throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        int n;
        while ((n = m.pull(buf)) > 0) acc.write(buf, 0, n);
        return acc.toByteArray();
    }

    /**
     * {@link Muxer#pushVideoMispTo} splices the MISP SEI correctly: after a
     * mux→demux round-trip the raw AU bytes contain the SEI, and
     * {@link MispTimestamp#extract} recovers the original kind / timeStatus / value.
     */
    @Test
    void pushVideoMispToRoundTrips() throws Exception {
        MispTimestamp input = MispTimestamp.micros(0x0005_F5E1_0000_0001L, 0x1F);

        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        byte[] ts;
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            m.pushVideoMispTo(h, syntheticH264Idr(), 9000L, true, input);
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

        // Extract the MISP timestamp from the raw AU bytes.
        byte[] rawBytes = new byte[v.raw().remaining()];
        v.raw().duplicate().get(rawBytes);

        MispTimestamp extracted = MispTimestamp.extract(rawBytes, VideoCodec.H264);
        assertNotNull(extracted, "MISP SEI must be present in the demuxed AU");
        assertEquals(input.kind(), extracted.kind(), "kind must round-trip");
        assertEquals(input.timeStatus(), extracted.timeStatus(), "timeStatus must round-trip");
        assertEquals(input.value(), extracted.value(), "value must round-trip");
    }

    /**
     * The DTS variant {@link Muxer#pushVideoMispTo(VideoStreamHandle, byte[], long, long, boolean, MispTimestamp)}
     * also splices the SEI and preserves a distinct PTS/DTS. Extract must succeed.
     */
    @Test
    void pushVideoMispToWithDtsRoundTrips() throws Exception {
        MispTimestamp input = MispTimestamp.micros(42L, 0x00);

        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        byte[] ts;
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            m.pushVideoMispTo(h, syntheticH264Idr(), 9000L, 6000L, true, input);
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
        assertNotNull(v.dts(), "dts must be non-null for PTS+DTS misp push");
        assertEquals(6000L, v.dts(), "dts must survive round-trip");

        byte[] rawBytes = new byte[v.raw().remaining()];
        v.raw().duplicate().get(rawBytes);

        MispTimestamp extracted = MispTimestamp.extract(rawBytes, VideoCodec.H264);
        assertNotNull(extracted, "MISP SEI must be present in the demuxed AU");
        assertEquals(input.kind(), extracted.kind());
        assertEquals(input.timeStatus(), extracted.timeStatus());
        assertEquals(input.value(), extracted.value());
    }

    /**
     * Pushing a {@link MispTimeKind#NANO} timestamp to an H.264 stream must
     * throw {@link MuxException} with kind {@code INPUT_MALFORMED} — nano is
     * H.265-only per ST 0604.6 §12.2, and the Rust {@code MispTimeError}
     * maps to {@code MuxErrorKind::InputMalformed}.
     */
    @Test
    void nanoOnH264ThrowsMuxException() throws Exception {
        MispTimestamp nano = MispTimestamp.nanos(1L, 0x1F);

        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            MuxException ex = assertThrows(MuxException.class,
                () -> m.pushVideoMispTo(h, syntheticH264Idr(), 9000L, true, nano));
            assertEquals(MuxException.Kind.MISP_TIME, ex.kind(),
                "nano-on-H264 must surface as MISP_TIME (MuxError::MispTime)");
        }
    }

    /**
     * {@link MispTimestamp#extract} on a plain H.264 AU with no SEI returns
     * {@code null}.
     */
    @Test
    void extractOnPlainAuReturnsNull() throws Exception {
        MispTimestamp result = MispTimestamp.extract(syntheticH264Idr(), VideoCodec.H264);
        assertNull(result, "no MISP SEI present → must return null");
    }

    /**
     * {@link MispTimestamp} factory methods produce the expected kind/value/status.
     */
    @Test
    void factoryMethodsProduceCorrectFields() {
        MispTimestamp micro = MispTimestamp.micros(1000L, 0x3F);
        assertEquals(MispTimeKind.MICRO, micro.kind());
        assertEquals(0x3F, micro.timeStatus());
        assertEquals(1000L, micro.value());

        MispTimestamp nano = MispTimestamp.nanos(999L, 0x01);
        assertEquals(MispTimeKind.NANO, nano.kind());
        assertEquals(0x01, nano.timeStatus());
        assertEquals(999L, nano.value());
    }
}
