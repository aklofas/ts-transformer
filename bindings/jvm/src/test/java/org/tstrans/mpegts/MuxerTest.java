package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;

import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;

class MuxerTest {

    @Test
    void pushVideoThenDrainProducesTsPackets() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();

        int total = 0;
        boolean firstPacketSeen = false;
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(syntheticH264Idr(), /*pts=*/ 0L, /*keyFrame=*/ true);
            int n;
            while ((n = m.pull(out)) > 0) {
                assertEquals(0, n % 188, "pull must return a multiple of 188");
                if (!firstPacketSeen) {
                    assertEquals((byte) 0x47, out[0], "first TS packet must start with sync byte 0x47");
                    firstPacketSeen = true;
                }
                total += n;
            }
        }
        assertTrue(total > 0, "expected muxed TS bytes");
        assertEquals(0, total % 188, "total muxed bytes must be a multiple of 188");
    }

    @Test
    void pushAfterCloseThrows() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        Muxer m = new Muxer(cfg);
        m.close();
        assertThrows(IllegalStateException.class,
            () -> m.pushVideo(syntheticH264Idr(), 0L, true));
    }

    @Test
    void invalidConfigThrowsMuxException() {
        // pmt_pid colliding with the video PID is rejected by Muxer::new (Rust-side
        // validation) -> MuxException(CONFIG_INVALID). Proves the error path crosses JNI.
        MuxerConfig cfg = MuxerConfig.builder()
            .pmtPid(0x1011)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        MuxException ex = assertThrows(MuxException.class, () -> new Muxer(cfg));
        assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind());
    }

    private static MuxerConfig klvConfig() throws Exception {
        return MuxerConfig.builder().programNumber(1).pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.PRIVATE_DATA, /*carriesPts=*/ true).build();
    }

    @Test
    void pushKlvRejectsAServiceIdOutsideAByte() throws Exception {
        try (Muxer m = new Muxer(klvConfig())) {
            for (int bad : new int[] {-1, 256, 257}) {
                var e = assertThrows(IllegalArgumentException.class, () -> m.pushKlv(new byte[] {1}, 0, bad));
                assertTrue(e.getMessage().contains("metadataServiceId"), e.getMessage());
            }
            assertEquals(0, org.tstrans.TestSupport.drain(m).length, "a rejected push queues nothing");
            m.pushKlv(new byte[] {1}, 0, 255);
        }
    }

    @Test
    void pushKlvToRejectsAServiceIdOutsideAByte() throws Exception {
        try (Muxer m = new Muxer(klvConfig())) {
            KlvStreamHandle h = m.klvHandles().get(0);
            for (int bad : new int[] {-1, 256}) {
                var e = assertThrows(IllegalArgumentException.class, () -> m.pushKlvTo(h, new byte[] {1}, 0, bad));
                assertTrue(e.getMessage().contains("metadataServiceId"), e.getMessage());
            }
            assertEquals(0, org.tstrans.TestSupport.drain(m).length, "a rejected push queues nothing");
            m.pushKlvTo(h, new byte[] {1}, 0, 255);
        }
    }
}
