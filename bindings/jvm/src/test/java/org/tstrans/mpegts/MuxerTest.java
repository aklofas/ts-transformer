package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.firstPcrBase;
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

    @Test
    void pcrLeadDefaultsTo100AndIsSettable() throws Exception {
        MuxerConfig byDefault = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264).build();
        assertEquals(100, byDefault.pcrLeadMs());
        try (Muxer m = new Muxer(byDefault)) {
            m.pushVideo(syntheticH264Idr(), /*pts=*/ 90_000L, /*keyFrame=*/ true);
            assertEquals(81_000L, firstPcrBase(org.tstrans.TestSupport.drain(m)),
                "the PCR runs 100 ms (9000 ticks) behind the decode time by default");
        }

        MuxerConfig noLead = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
            .pcrLeadMs(0).build();
        assertEquals(0, noLead.pcrLeadMs());
        try (Muxer m = new Muxer(noLead)) {
            m.pushVideo(syntheticH264Idr(), /*pts=*/ 90_000L, /*keyFrame=*/ true);
            assertEquals(90_000L, firstPcrBase(org.tstrans.TestSupport.drain(m)),
                "pcrLeadMs(0) writes PCR = decode time");
        }
    }

    @Test
    void pcrLeadOver1000IsRejected() {
        // Range validation runs Rust-side in Muxer::new, like the PID collision above.
        MuxerConfig cfg = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
            .pcrLeadMs(1001).build();
        MuxException ex = assertThrows(MuxException.class, () -> new Muxer(cfg));
        assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind());
        assertTrue(ex.getMessage().contains("pcr_lead_ms"), ex.getMessage());

        MuxerConfig negative = MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264)
            .pcrLeadMs(-1).build();
        MuxException neg = assertThrows(MuxException.class, () -> new Muxer(negative));
        assertEquals(MuxException.Kind.CONFIG_INVALID, neg.kind());
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

    private static final byte[] KLV = {1, 2, 3};

    @Test
    void aSynchronousKlvPushUnderAnUndeclaredServiceIsInvalidUsage() throws Exception {
        MuxerConfig automatic = MuxerConfig.builder().pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).build();
        // The automatic metadata_descriptor declares service 0.
        try (Muxer m = new Muxer(automatic)) {
            m.pushVideo(syntheticH264Idr(), 0L, true);
            m.pushKlv(KLV, 0L, 0);
            long before = m.pendingPackets();
            MuxException e = assertThrows(MuxException.class, () -> m.pushKlv(KLV, 0L, 3));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());
            assertEquals(before, m.pendingPackets(), "a refused push queues nothing");
        }
        // A caller metadata_descriptor for service 3 replaces the automatic one.
        byte[] service3 = {0x26, 0x09, 0x01, 0x00, (byte) 0xFF, 'K', 'L', 'V', 'A', 3, 0x0F};
        MuxerConfig declared = MuxerConfig.builder().pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .streamDescriptorsForKlv(0, new byte[][] {service3}).build();
        try (Muxer m = new Muxer(declared)) {
            KlvStreamHandle h = m.klvHandles().get(0);
            m.pushKlvTo(h, KLV, 0L, 3);
            MuxException e = assertThrows(MuxException.class, () -> m.pushKlvTo(h, KLV, 0L, 0));
            assertEquals(MuxException.Kind.INVALID_USAGE, e.kind());
        }
    }

    @Test
    void aTruncatedAv1ObuIsInvalidAv1Obu() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().pmtPid(0x100)
            .addVideo(0x101, VideoCodec.AV1).build();
        try (Muxer m = new Muxer(cfg)) {
            // A sequence header OBU whose size field is missing.
            MuxException e = assertThrows(MuxException.class,
                () -> m.pushVideo(new byte[] {0x0A}, 0L, true));
            assertEquals(MuxException.Kind.INVALID_AV1_OBU, e.kind());
            assertEquals(0, m.pendingPackets(), "a refused push queues nothing");
        }
    }
}
