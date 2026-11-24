package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;

class MuxerConfigTest {
    @Test
    void builderAccumulatesStreamsAndScalars() {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000).pcrPid(0x1011)
            .addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1031, KlvStreamType.PRIVATE_DATA, /*carriesPts=*/ true)
            .pcrIntervalMs(40).psiIntervalMs(100).bufferPackets(10_000)
            .av1Carriage(Av1CarriageMode.MPEG2_TS_BINDING)
            .build();
        assertEquals(1, cfg.programNumber());
        assertEquals(2, cfg.streamCount());
    }

    @Test
    void accessorsReturnCopies() {
        MuxerConfig cfg = MuxerConfig.builder().programNumber(1).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264).build();
        cfg.streamPids()[0] = 0x1FFF;
        cfg.streamKinds()[0] = 99;
        cfg.streamCodecs()[0] = 99;
        cfg.streamTypeCodes()[0] = 99;
        assertEquals(0x101, cfg.streamPids()[0]);
        assertNotEquals(99, cfg.streamKinds()[0]);
        assertNotEquals(99, cfg.streamCodecs()[0]);
        assertNotEquals(99, cfg.streamTypeCodes()[0]);
        boolean first = cfg.streamCarriesPts()[0];
        cfg.streamCarriesPts()[0] = !first;
        assertEquals(first, cfg.streamCarriesPts()[0]);
    }

    @Test
    void emptyConfigRejected() {
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().build());
    }

    @Test
    void dvbSubtitleDeferred() {
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addSubtitle(0x1041, SubtitleCodec.DVB_SUBTITLING));
    }

    @Test
    void outOfRangePidRejected() {
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().addVideo(0x0000, VideoCodec.H264));
    }

    @Test
    void addDataRejectsStreamTypeOutOfU8Range() {
        assertThrows(IllegalArgumentException.class, () ->
            MuxerConfig.builder().addData(0x0100, 256, false));
        assertThrows(IllegalArgumentException.class, () ->
            MuxerConfig.builder().addData(0x0100, -1, false));
    }

    @Test
    void streamDescriptorsForDataRejectsOutOfRangeIdxAtBuild() {
        MuxerConfig.Builder b = MuxerConfig.builder()
            .addData(0x0100, 0xF0, true)
            .streamDescriptorsForData(1, new byte[][] {{(byte) 0x05, 4, 'A', 'R', 'S', 'X'}});
        assertThrows(IllegalArgumentException.class, b::build);
    }

    @Test
    void addDataBuildsAlongsideOtherKinds() {
        // bare builder acceptance; deep validation happens at new Muxer(...)
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .addData(0x0100, 0xF0, true)
            .streamDescriptorsForData(0, new byte[][] {{(byte) 0x05, 4, 'A', 'R', 'S', 'X'}})
            .build();
        assertEquals(3, cfg.streamCount());
    }

    @Test
    void dataStreamTypeOutsideAByteIsRefused() throws Exception {
        MuxerConfig.Builder b = MuxerConfig.builder().addVideo(0x100, VideoCodec.H264);
        for (int bad : new int[] {-1, 256}) {
            var e = assertThrows(IllegalArgumentException.class, () -> b.addData(0x101, bad, false));
            assertTrue(e.getMessage().contains(String.valueOf(bad)), e.getMessage());
        }
        b.addData(0x101, 0x06, false); // the builder is usable afterwards
        new Muxer(b.build()).close();
    }

    @Test
    void programNumberOutsideU16Rejected() {
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().programNumber(0x10000));
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().programNumber(-1));
    }

    @Test
    void pmtPidOutsideRangeRejected() {
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().pmtPid(0x1FFF));
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().pmtPid(0x11000));
    }

    @Test
    void pcrPidBelowMinusOneRejected() {
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().pcrPid(-2));
    }

    @Test
    void boundaryPidsAccepted() {
        assertDoesNotThrow(() -> MuxerConfig.builder()
            .pmtPid(0x0010).pmtPid(0x1FFE).pcrPid(-1).pcrPid(0x0010).pcrPid(0x1FFE)
            .programNumber(0xFFFF).programNumber(0));
    }

    /**
     * The first packet on {@code pid} that starts a section, as
     * {@code {table id extension, version}} (PAT: the transport_stream_id; PMT:
     * the program number).
     */
    private static int[] firstSectionHeader(byte[] ts, int pid) {
        for (int off = 0; off + 188 <= ts.length; off += 188) {
            int p = ((ts[off + 1] & 0x1F) << 8) | (ts[off + 2] & 0xFF);
            boolean start = (ts[off + 1] & 0x40) != 0;
            if (p != pid || !start) continue;
            int i = off + 4;
            if ((ts[off + 3] & 0x20) != 0) i += 1 + (ts[i] & 0xFF); // adaptation field
            i += 1 + (ts[i] & 0xFF); // pointer field
            int extension = ((ts[i + 3] & 0xFF) << 8) | (ts[i + 4] & 0xFF);
            int version = (ts[i + 5] >> 1) & 0x1F;
            return new int[] {extension, version};
        }
        throw new AssertionError("no section start on PID 0x" + Integer.toHexString(pid));
    }

    private static byte[] firstPush(MuxerConfig cfg) throws Exception {
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(org.tstrans.TestSupport.longH264Idr(), 90_000, true);
            return org.tstrans.TestSupport.drain(m);
        }
    }

    @Test
    void transportStreamIdAndPsiVersionBaseReachTheWire() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .transportStreamId(0x1234).psiVersionBase(7)
            .build();
        assertEquals(0x1234, cfg.transportStreamId());
        assertEquals(7, cfg.psiVersionBase());
        byte[] ts = firstPush(cfg);
        assertArrayEquals(new int[] {0x1234, 7}, firstSectionHeader(ts, 0x0000), "PAT");
        assertArrayEquals(new int[] {1, 7}, firstSectionHeader(ts, 0x1000), "PMT");
    }

    @Test
    void transportStreamIdAndPsiVersionBaseDefaultsAreOneAndZero() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264).build();
        assertEquals(1, cfg.transportStreamId());
        assertEquals(0, cfg.psiVersionBase());
        byte[] ts = firstPush(cfg);
        assertArrayEquals(new int[] {1, 0}, firstSectionHeader(ts, 0x0000), "PAT");
        assertArrayEquals(new int[] {1, 0}, firstSectionHeader(ts, 0x1000), "PMT");
    }

    @Test
    void aVersionBaseOutsideZeroToThirtyOneIsRefused() {
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().psiVersionBase(32));
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().psiVersionBase(-1));
        assertDoesNotThrow(() -> MuxerConfig.builder().psiVersionBase(0).psiVersionBase(31));
    }

    @Test
    void aTransportStreamIdOutsideSixteenBitsIsRefused() {
        assertThrows(IllegalArgumentException.class,
            () -> MuxerConfig.builder().transportStreamId(0x10000));
        assertThrows(IllegalArgumentException.class, () -> MuxerConfig.builder().transportStreamId(-1));
        assertDoesNotThrow(() -> MuxerConfig.builder().transportStreamId(0).transportStreamId(0xFFFF));
    }

    @Test
    void aReconfigureThatChangesTheIdentityValuesIsRefused() throws Exception {
        MuxerConfig.Builder base = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264);
        try (Muxer m = new Muxer(base.transportStreamId(5).psiVersionBase(3).build())) {
            for (MuxerConfig changed : new MuxerConfig[] {
                MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                    .transportStreamId(6).psiVersionBase(3).build(),
                MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                    .transportStreamId(5).psiVersionBase(4).build(),
            }) {
                org.tstrans.MuxException e = assertThrows(org.tstrans.MuxException.class,
                    () -> m.reconfigure(changed));
                assertEquals(org.tstrans.MuxException.Kind.CONFIG_INVALID, e.kind());
            }
            // The same values are not a change.
            m.reconfigure(MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .transportStreamId(5).psiVersionBase(3).build());
        }
    }

    @Test
    void aDataStreamOfASectionCarriedTypeIsConfigInvalid() {
        for (int type : new int[] {0x05, 0x13, 0x16, 0x2C, 0x2F, 0x30}) {
            MuxerConfig cfg = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
                .addData(0x1020, type, false).build();
            org.tstrans.MuxException e = assertThrows(org.tstrans.MuxException.class,
                () -> new Muxer(cfg).close(), "stream type 0x" + Integer.toHexString(type));
            assertEquals(org.tstrans.MuxException.Kind.CONFIG_INVALID, e.kind());
        }
    }

    @Test
    void aPsiIntervalAbove200WithKlvIsConfigInvalid() throws Exception {
        MuxerConfig.Builder b = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
            .addKlv(0x1012, KlvStreamType.PRIVATE_DATA, true);
        MuxerConfig over = b.psiIntervalMs(201).build();
        org.tstrans.MuxException e = assertThrows(org.tstrans.MuxException.class,
            () -> new Muxer(over).close());
        assertEquals(org.tstrans.MuxException.Kind.CONFIG_INVALID, e.kind());
        new Muxer(b.psiIntervalMs(200).build()).close();
        // Without KLV the bound does not apply.
        new Muxer(MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
            .psiIntervalMs(201).build()).close();
    }
}
