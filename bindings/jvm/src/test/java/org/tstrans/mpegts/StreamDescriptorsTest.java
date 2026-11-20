package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;

/**
 * Program and per-stream PMT descriptors for every stream kind. The JVM
 * {@code ProgramMap} event does not surface descriptors, so each test reads
 * the muxed PMT section back from the TS bytes itself.
 */
class StreamDescriptorsTest {

    private static final int PMT_PID = 0x1000;

    /** {@code metadata_descriptor} (tag 0x26) for KLVA with {@code serviceId}. */
    private static byte[] metadataDescriptor(int serviceId) {
        return new byte[] {0x26, 0x09, 0x01, 0x00, (byte) 0xFF, 'K', 'L', 'V', 'A',
            (byte) serviceId, 0x0F};
    }

    /** A user-private descriptor (tag 0xE0..0xFE) the muxer passes through. */
    private static byte[] privateDescriptor(int tag, int marker) {
        return new byte[] {(byte) tag, 2, (byte) marker, (byte) ~marker};
    }

    private static byte[][] one(byte[] d) {
        return new byte[][] {d};
    }

    /** The first PMT section in {@code ts}: program info loop and per-PID ES loops. */
    private record Pmt(List<byte[]> programInfo, Map<Integer, List<byte[]>> esInfo) {}

    private static Pmt readPmt(byte[] ts) {
        for (int off = 0; off + 188 <= ts.length; off += 188) {
            int pid = ((ts[off + 1] & 0x1F) << 8) | (ts[off + 2] & 0xFF);
            boolean pusi = (ts[off + 1] & 0x40) != 0;
            if (pid != PMT_PID || !pusi) continue;
            int afc = (ts[off + 3] >> 4) & 0x3;
            int p = off + 4;
            if (afc == 3) p += 1 + (ts[p] & 0xFF);
            p += 1 + (ts[p] & 0xFF); // pointer_field
            assertEquals(0x02, ts[p] & 0xFF, "table_id of a PMT section");
            int sectionLength = ((ts[p + 1] & 0x0F) << 8) | (ts[p + 2] & 0xFF);
            int end = p + 3 + sectionLength - 4; // CRC excluded
            assertTrue(end <= off + 188, "the PMT fits one packet in these configs");
            int programInfoLength = ((ts[p + 10] & 0x0F) << 8) | (ts[p + 11] & 0xFF);
            int q = p + 12;
            List<byte[]> programInfo = descriptors(ts, q, q + programInfoLength);
            q += programInfoLength;
            Map<Integer, List<byte[]>> es = new LinkedHashMap<>();
            while (q < end) {
                int esPid = ((ts[q + 1] & 0x1F) << 8) | (ts[q + 2] & 0xFF);
                int esInfoLength = ((ts[q + 3] & 0x0F) << 8) | (ts[q + 4] & 0xFF);
                es.put(esPid, descriptors(ts, q + 5, q + 5 + esInfoLength));
                q += 5 + esInfoLength;
            }
            return new Pmt(programInfo, es);
        }
        throw new AssertionError("no PMT section in the muxed bytes");
    }

    private static List<byte[]> descriptors(byte[] b, int from, int to) {
        List<byte[]> out = new ArrayList<>();
        while (from < to) {
            int len = b[from + 1] & 0xFF;
            out.add(Arrays.copyOfRange(b, from, from + 2 + len));
            from += 2 + len;
        }
        return out;
    }

    private static List<Integer> serviceIds(List<byte[]> descs) {
        List<Integer> ids = new ArrayList<>();
        for (byte[] d : descs) {
            if ((d[0] & 0xFF) == 0x26) ids.add(d[9] & 0xFF); // the layout of metadataDescriptor
        }
        return ids;
    }

    private static boolean contains(List<byte[]> descs, byte[] d) {
        return descs.stream().anyMatch(x -> Arrays.equals(x, d));
    }

    private static byte[] muxOneVideoFrame(MuxerConfig cfg) throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(syntheticH264Idr(), 0L, true);
            int n;
            while ((n = m.pull(out)) > 0) acc.write(out, 0, n);
        }
        return acc.toByteArray();
    }

    @Test
    void twoSynchronousKlvStreamsWithoutDescriptorsAreRefused() {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .addKlv(0x1013, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .build();
        MuxException e = assertThrows(MuxException.class, () -> new Muxer(cfg).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
    }

    @Test
    void twoSynchronousKlvStreamsWithServiceIdsBuild() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .addKlv(0x1013, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .streamDescriptorsForKlv(1, one(metadataDescriptor(1)))
            .build();
        Pmt pmt = readPmt(muxOneVideoFrame(cfg));
        assertEquals(List.of(0), serviceIds(pmt.esInfo().get(0x1012)));
        assertEquals(List.of(1), serviceIds(pmt.esInfo().get(0x1013)));
    }

    @Test
    void aProgramDescriptorAppearsInProgramInfo() throws Exception {
        byte[] d = privateDescriptor(0xE7, 0x5A);
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .programDescriptors(one(d))
            .build();
        Pmt pmt = readPmt(muxOneVideoFrame(cfg));
        assertEquals(1, pmt.programInfo().size());
        assertArrayEquals(d, pmt.programInfo().get(0));
        assertFalse(contains(pmt.esInfo().get(0x1011), d), "a program descriptor is not a stream's");
    }

    /** A program descriptor rides along, so the two halves of the flattened layout are pinned too. */
    @Test
    void eachTypedSetterLandsOnItsStream() throws Exception {
        byte[] video = privateDescriptor(0xE0, 1);
        byte[] klv = privateDescriptor(0xE1, 2);
        byte[] audio = privateDescriptor(0xE2, 3);
        byte[] subtitle = privateDescriptor(0xE3, 4);
        byte[] data = privateDescriptor(0xE4, 5);
        byte[] byAbsoluteIndex = privateDescriptor(0xE5, 6);
        byte[] program = privateDescriptor(0xE6, 7);
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1012, KlvStreamType.PRIVATE_DATA, false)
            .addAudio(0x1013, AudioCodec.AAC)
            .addSubtitle(0x1014, SubtitleCodec.WEBVTT_IN_TS)
            .addData(0x1015, 0xF0, true)
            .addData(0x1016, 0xF1, true)
            .streamDescriptorsForVideo(0, one(video))
            .streamDescriptorsForKlv(0, one(klv))
            .streamDescriptorsForAudio(0, one(audio))
            .streamDescriptorsForSubtitle(0, one(subtitle))
            .streamDescriptorsForData(0, one(data))
            .streamDescriptorsForStream(5, one(byAbsoluteIndex))
            .programDescriptors(one(program))
            .build();
        Pmt pmt = readPmt(muxOneVideoFrame(cfg));
        Map<Integer, byte[]> expected = new LinkedHashMap<>();
        expected.put(0x1011, video);
        expected.put(0x1012, klv);
        expected.put(0x1013, audio);
        expected.put(0x1014, subtitle);
        expected.put(0x1015, data);
        expected.put(0x1016, byAbsoluteIndex);
        for (Map.Entry<Integer, byte[]> e : expected.entrySet()) {
            assertFalse(contains(pmt.esInfo().get(e.getKey()), program),
                "the program descriptor on 0x" + Integer.toHexString(e.getKey()));
            assertFalse(contains(pmt.programInfo(), e.getValue()),
                "the descriptor of 0x" + Integer.toHexString(e.getKey()) + " in the program loop");
            for (Map.Entry<Integer, byte[]> other : expected.entrySet()) {
                boolean here = contains(pmt.esInfo().get(e.getKey()), other.getValue());
                assertEquals(e.getKey().equals(other.getKey()), here,
                    "descriptor of 0x" + Integer.toHexString(other.getKey())
                        + " on 0x" + Integer.toHexString(e.getKey()));
            }
        }
        assertEquals(1, pmt.programInfo().size());
        assertArrayEquals(program, pmt.programInfo().get(0));
    }

    @Test
    void theLastSetterNamingAStreamWins() throws Exception {
        byte[] first = privateDescriptor(0xE0, 1);
        byte[] second = privateDescriptor(0xE0, 2);
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .streamDescriptorsForVideo(0, one(first))
            .streamDescriptorsForStream(0, one(second))
            .build();
        List<byte[]> es = readPmt(muxOneVideoFrame(cfg)).esInfo().get(0x1011);
        assertFalse(contains(es, first));
        assertTrue(contains(es, second));
    }

    @Test
    void aMalformedDescriptorIsConfigInvalid() {
        // The length byte claims 10 payload bytes; one follows.
        byte[][] truncated = {{(byte) 0xE0, 10, 1}};
        MuxerConfig onStream = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .streamDescriptorsForVideo(0, truncated)
            .build();
        MuxException e = assertThrows(MuxException.class, () -> new Muxer(onStream).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());

        MuxerConfig onProgram = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .programDescriptors(truncated)
            .build();
        MuxException p = assertThrows(MuxException.class, () -> new Muxer(onProgram).close());
        assertEquals(MuxException.Kind.CONFIG_INVALID, p.kind());
    }

    @Test
    void anIndexPastTheKindsCountIsIllegalArgument() {
        byte[][] d = one(privateDescriptor(0xE0, 1));
        MuxerConfig.Builder one = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, true);
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .streamDescriptorsForVideo(1, d).build());
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, true)
                .streamDescriptorsForKlv(1, d).build());
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .streamDescriptorsForAudio(0, d).build());
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .streamDescriptorsForSubtitle(0, d).build());
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .streamDescriptorsForStream(2, d).build());
        assertThrows(IllegalArgumentException.class,
            () -> one.streamDescriptorsForVideo(-1, d));
        assertThrows(NullPointerException.class, () -> one.programDescriptors(null));
        assertNotNull(one.build(), "the refused calls left the builder usable");
    }
}
