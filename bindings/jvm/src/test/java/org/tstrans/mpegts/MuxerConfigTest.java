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
}
