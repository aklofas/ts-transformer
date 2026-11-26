package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.drain;
import static org.tstrans.TestSupport.syntheticH264IdrPrefix;
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
            m.pushVideoMispTo(h, syntheticH264IdrPrefix(), 9000L, true, input);
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
        MispTimestamp input = MispTimestamp.micros(42L, 0x1F);

        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        byte[] ts;
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            m.pushVideoMispTo(h, syntheticH264IdrPrefix(), 9000L, 6000L, true, input);
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
                () -> m.pushVideoMispTo(h, syntheticH264IdrPrefix(), 9000L, true, nano));
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
        MispTimestamp result = MispTimestamp.extract(syntheticH264IdrPrefix(), VideoCodec.H264);
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

        MispTimestamp nano = MispTimestamp.nanos(999L, 0x9F);
        assertEquals(MispTimeKind.NANO, nano.kind());
        assertEquals(0x9F, nano.timeStatus());
        assertEquals(999L, nano.value());
    }

    @Test
    void pushVideoMispToRefusesReservedTimeStatusBits() throws Exception {
        // ST 0603.5 Table 3: the Time Status's bits 4-0 are reserved `11111`.
        MuxerConfig cfg = MuxerConfig.builder().addVideo(0x100, VideoCodec.H264).build();
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            for (int bad : new int[] {0x00, 0x80, 0xE0, 0x0F}) {
                MispTimestamp ts = MispTimestamp.micros(1L, bad);
                MuxException e = assertThrows(MuxException.class,
                    () -> m.pushVideoMispTo(h, syntheticH264IdrPrefix(), 9000L, true, ts));
                assertEquals(MuxException.Kind.MISP_TIME, e.kind());
                assertTrue(e.getMessage().contains("reserved bits 4-0"), e.getMessage());
            }
            assertEquals(0, drain(m).length, "a refused push queues nothing");
        }
    }

    @Test
    void pushVideoMispToRejectsATimeStatusOutsideAByte() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().addVideo(0x100, VideoCodec.H264).build();
        try (Muxer m = new Muxer(cfg)) {
            VideoStreamHandle h = m.videoStreamHandle(0).orElseThrow();
            for (int bad : new int[] {-1, 256}) {
                MispTimestamp ts = MispTimestamp.micros(1L, bad);
                var e = assertThrows(IllegalArgumentException.class,
                    () -> m.pushVideoMispTo(h, syntheticH264IdrPrefix(), 9000L, true, ts));
                assertTrue(e.getMessage().contains("timeStatus"), e.getMessage());
            }
            assertEquals(0, drain(m).length, "a rejected push queues nothing");
        }
    }

    /**
     * An access unit whose SEI NAL unit holds a message of payload type 4,
     * then one of payload type 128 (H.265 {@code structure_of_pictures_info};
     * its first byte {@code 0x80} also reads as a stop bit), then the MISP
     * message: the timestamp behind the type-128 message is found.
     */
    private static byte[] seiAfterType128Message(VideoCodec codec, byte[] uuid) {
        java.io.ByteArrayOutputStream au = new java.io.ByteArrayOutputStream();
        au.writeBytes(new byte[] {0, 0, 0, 1});
        if (codec == VideoCodec.H264) {
            au.write(0x06);
        } else {
            au.writeBytes(new byte[] {0x4E, 0x01});
        }
        au.writeBytes(new byte[] {0x04, 0x01, 0x55, (byte) 0x80, 0x02, 0x11, 0x22, 0x05, 28});
        au.writeBytes(uuid);
        // time_status 0x1F, value 0x0102030405060708 in four 2-byte groups
        // with a 0xFF guard byte after each of the first three.
        au.writeBytes(new byte[] {
            0x1F, 0x01, 0x02, (byte) 0xFF, 0x03, 0x04, (byte) 0xFF,
            0x05, 0x06, (byte) 0xFF, 0x07, 0x08, (byte) 0x80});
        if (codec == VideoCodec.H264) {
            au.writeBytes(new byte[] {0, 0, 0, 1, 0x65, (byte) 0xAA});
        } else {
            au.writeBytes(new byte[] {0, 0, 0, 1, 0x26, 0x01, (byte) 0xAA});
        }
        return au.toByteArray();
    }

    @Test
    void extractFindsMispAfterType128Message() throws Exception {
        byte[] h264Uuid = "MISPmicrosectime".getBytes(java.nio.charset.StandardCharsets.US_ASCII);
        byte[] h265Uuid = {
            (byte) 0xa8, 0x68, 0x7d, (byte) 0xd4, (byte) 0xd7, 0x59, 0x37, 0x58,
            (byte) 0xa5, (byte) 0xce, (byte) 0xf0, 0x33, (byte) 0x8b, 0x65, 0x45, (byte) 0xf1};
        MispTimestamp want = MispTimestamp.micros(0x0102_0304_0506_0708L, 0x1F);
        assertEquals(want, MispTimestamp.extract(
                seiAfterType128Message(VideoCodec.H264, h264Uuid), VideoCodec.H264));
        assertEquals(want, MispTimestamp.extract(
                seiAfterType128Message(VideoCodec.H265, h265Uuid), VideoCodec.H265));
    }
}
