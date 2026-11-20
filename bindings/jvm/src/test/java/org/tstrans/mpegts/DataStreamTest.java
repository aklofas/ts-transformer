package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;

import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;

/**
 * Open-time Rust validation matrix for data streams. Every config carries a
 * video stream alongside the data stream(s): data streams are PCR-ineligible
 * (like subtitles), so a data-only program is rejected up front as a
 * no-PCR-eligible-stream config — which would mask the specific validation
 * each test targets (mirrors the core's own {@code three_data_cfg} fixture).
 */
class DataStreamTest {

    @Test
    void typedStreamTypeRejectedAtOpen() {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0x1B, true) // 0x1B = H.264
            .build();
        MuxException e = assertThrows(MuxException.class, () -> new Muxer(cfg).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
    }

    @Test
    void klvaMasqueradeOn0x06RejectedAtOpen() {
        // 0x06 + KLVA registration descriptor classifies as KLV -> rejected
        byte[] klva = {(byte) 0x05, 4, 'K', 'L', 'V', 'A'};
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0x06, true)
            .streamDescriptorsForData(0, new byte[][] {klva})
            .build();
        MuxException e = assertThrows(MuxException.class, () -> new Muxer(cfg).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
    }

    @Test
    void bare0x06OpensAsData() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0x06, true)
            .build();
        try (Muxer m = new Muxer(cfg)) { /* opens fine */ }
    }

    @Test
    void seventeenDataStreamsRejectedAtOpen() {
        MuxerConfig.Builder b = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264);
        for (int i = 0; i < 17; i++) b.addData(0x0100 + i, 0xF0, false);
        MuxException e = assertThrows(MuxException.class, () -> new Muxer(b.build()).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
    }

    @Test
    void descriptorsAttributeToTheRightStreamAcrossMultipleDataStreams() throws Exception {
        // KLVA registration on the 0xF0 stream is benign (0xF0 never classifies);
        // a mis-attribution that lands it on the 0x06 stream would trip the
        // masquerade rejection — so a clean open proves per-stream attribution.
        byte[] klva = {(byte) 0x05, 4, 'K', 'L', 'V', 'A'};
        byte[] benign = {(byte) 0x05, 4, 'A', 'R', 'S', 'X'};
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0xF0, true)
            .addData(0x0101, 0x06, true)
            .streamDescriptorsForData(0, new byte[][] {klva})
            .streamDescriptorsForData(1, new byte[][] {benign})
            .build();
        try (Muxer m = new Muxer(cfg)) { /* opens fine — attribution correct */ }

        // And the converse: swapped attribution must reject.
        MuxerConfig swapped = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0xF0, true)
            .addData(0x0101, 0x06, true)
            .streamDescriptorsForData(0, new byte[][] {benign})
            .streamDescriptorsForData(1, new byte[][] {klva})
            .build();
        MuxException e = assertThrows(MuxException.class, () -> new Muxer(swapped).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
    }

    @Test
    void pushDataShorthandErrorMatrix() throws Exception {
        // zero data streams -> INVALID_USAGE
        try (Muxer m = new Muxer(MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264).build())) {
            MuxException e = assertThrows(MuxException.class, () -> m.pushData(new byte[] {1}, 0L));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());
        }
        // two data streams -> ambiguous INVALID_USAGE; pushDataTo resolves it
        MuxerConfig two = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0xF0, true).addData(0x0101, 0xF1, true).build();
        try (Muxer m = new Muxer(two)) {
            MuxException e = assertThrows(MuxException.class, () -> m.pushData(new byte[] {1}, 0L));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());
            assertEquals(2, m.dataHandles().size());
            m.pushDataTo(m.dataHandles().get(1), new byte[] {1, 2, 3}, 90_000L); // no throw
        }
    }

    @Test
    void oversizedPayloadIsInputMalformed() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264).addData(0x0100, 0xF0, true).build();
        try (Muxer m = new Muxer(cfg)) {
            MuxException e = assertThrows(MuxException.class,
                () -> m.pushData(new byte[70_000], 0L));
            assertEquals(MuxException.Kind.INPUT_MALFORMED, e.kind());
        }
    }

    @Test
    void forgedHandleIsInvalidUsage() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264).addData(0x0100, 0xF0, true).build();
        try (Muxer m = new Muxer(cfg)) {
            // High bits outside the packed 4-bit program + 4-bit within layout
            // -> rejected by DataStreamHandle::try_from_raw in the JNI shim.
            MuxException e = assertThrows(MuxException.class,
                () -> m.pushDataTo(DataStreamHandle.fromRaw(0x7FFF_FFFFL), new byte[] {1}, 0L));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());

            // Negative jlong -> rejected by the u32::try_from leg in the shim
            // (never truncated into a plausible handle).
            MuxException neg = assertThrows(MuxException.class,
                () -> m.pushDataTo(DataStreamHandle.fromRaw(-1L), new byte[] {1}, 0L));
            assertEquals(MuxException.Kind.INVALID_USAGE, neg.kind());

            // Raw 1 IS in canonical packed layout (program 0, within-index 1 —
            // bits 0..=3 = within, bits 4..=7 = program) but targets a second
            // data stream this 1-stream muxer never configured -> pins the
            // core MuxError::InvalidStreamHandle leg (past try_from_raw).
            MuxException oob = assertThrows(MuxException.class,
                () -> m.pushDataTo(DataStreamHandle.fromRaw(1L), new byte[] {1}, 0L));
            assertEquals(MuxException.Kind.INVALID_USAGE, oob.kind());
        }
    }

    @Test
    void handleAccessorsAndClosedContract() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264).addData(0x0100, 0xF0, true).build();
        Muxer m = new Muxer(cfg);
        assertEquals(1, m.dataHandles().size());
        assertTrue(m.dataStreamHandle(0).isPresent());
        assertTrue(m.dataStreamHandle(1).isEmpty());
        assertTrue(m.dataStreamHandle(-1).isEmpty());
        m.close();
        assertThrows(IllegalStateException.class, m::dataHandles);
        assertThrows(IllegalStateException.class, () -> m.pushData(new byte[] {1}, 0L));
    }

    @Test
    void dataRoundTripPreservesPayloadPtsAndStreamType() throws Exception {
        byte[] desc = {(byte) 0x05, 4, 'A', 'R', 'S', 'X'};
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0xF0, true)
            .addData(0x0101, 0x06, false)   // bare 0x06, no PTS
            .streamDescriptorsForData(0, new byte[][] {desc})
            .build();
        byte[] payloadA = {(byte) 0xDE, (byte) 0xAD, (byte) 0xBE, (byte) 0xEF, 7};
        byte[] payloadB = "JSONCMD{}".getBytes(java.nio.charset.StandardCharsets.US_ASCII);

        java.io.ByteArrayOutputStream acc = new java.io.ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            java.util.List<DataStreamHandle> hs = m.dataHandles();
            m.pushVideo(syntheticH264Idr(), 0L, true);
            m.pushDataTo(hs.get(0), payloadA, 90_000L);
            m.pushDataTo(hs.get(1), payloadB, 90_000L);
            int n;
            while ((n = m.pull(out)) > 0) acc.write(out, 0, n);
        }
        byte[] ts = acc.toByteArray();

        // (a) descriptor TLV embedded verbatim in the muxed PMT (JVM ProgramMap
        // doesn't surface descriptors; the byte-scan is the emission proof —
        // the PMT fits one 188-byte packet at this size, so no split TLV; the
        // pushed payloads are chosen to NOT contain the descriptor TLV bytes,
        // so the whole-TS scan can't false-positive on PES payload data)
        assertTrue(containsBytes(ts, desc), "descriptor TLV must appear in the muxed PMT");

        // (b) demux: per-sample fidelity
        java.util.List<DemuxEvent.UnknownSample> samples = new java.util.ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent ev : d) {
                if (ev instanceof DemuxEvent.UnknownSample u) samples.add(u);
            }
        }
        assertEquals(2, samples.size());
        DemuxEvent.UnknownSample a = samples.stream()
            .filter(s -> s.streamType() == 0xF0).findFirst().orElseThrow();
        DemuxEvent.UnknownSample b = samples.stream()
            .filter(s -> s.streamType() == 0x06).findFirst().orElseThrow();
        assertEquals(90_000L, a.pts());
        assertArrayEquals(payloadA, toBytes(a.payload()));
        assertEquals(0L, b.pts(), "carriesPts=false re-demuxes as pts == 0 (no-PTS pin)");
        assertArrayEquals(payloadB, toBytes(b.payload()));
        // push_data writes a PTS-only PES header (never a DTS) → dts is null.
        assertNull(a.dts());
        assertNull(b.dts());
    }

    /** Naive contiguous-subarray scan — fine at TS-fixture sizes. */
    private static boolean containsBytes(byte[] hay, byte[] needle) {
        outer:
        for (int i = 0; i + needle.length <= hay.length; i++) {
            for (int j = 0; j < needle.length; j++) {
                if (hay[i + j] != needle[j]) continue outer;
            }
            return true;
        }
        return false;
    }

    private static byte[] toBytes(java.nio.ByteBuffer buf) {
        byte[] out = new byte[buf.remaining()];
        buf.duplicate().get(out);
        return out;
    }

    @Test
    void malformedDescriptorTlvRejectedAtBuild() {
        // length byte claims 10, only 2 payload bytes follow
        byte[] bad = {(byte) 0x05, 10, 'A', 'B'};
        MuxerConfig.Builder b = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addData(0x0100, 0xF0, true)
            .streamDescriptorsForData(0, new byte[][] {bad});
        IllegalArgumentException e = assertThrows(IllegalArgumentException.class, b::build);
        assertTrue(e.getMessage().startsWith("streamDescriptorsForData element 0:"), e.getMessage());
    }
}
