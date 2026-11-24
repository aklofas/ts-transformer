package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.OptionalInt;
import org.junit.jupiter.api.Test;

/**
 * End-to-end proof that the JNI {@link Demuxer} now surfaces a
 * {@link DemuxEvent.Metadata} (KLV) event for a synchronous-metadata stream —
 * the C.1 wiring that stopped {@code convert_event} from skipping
 * {@code DemuxEvent::Metadata}.
 *
 * <p>Feeds the SHARED committed scenario fixture
 * {@code crates/tst-integration/tests/fixtures/scenarios/h264-sync-klv-aucell/input.ts}
 * (the same artifact the Rust/Python/C adapters consume). Its golden declares a
 * {@code klv} core event on pid 4145, {@code stream_type 0x15} (synchronous
 * metadata → AU-cell path → {@link MetadataKind#KLV_SYNC_AU_CELL}), carrying an
 * ST 0601 UAS Datalink Local Set whose 16-byte UL key begins with the universal
 * MISB prefix {@code 06 0e 2b 34}.
 */
class MetadataEventTest {

    /** Workspace-relative shared scenario input; Gradle's user.dir is bindings/jvm, so ../../ reaches the root. */
    private static final Path FIXTURE =
        Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios/h264-sync-klv-aucell/input.ts")
            .normalize();

    @Test
    void surfacesSyncKlvMetadataEvent() throws Exception {
        // The shared fixture IS committed; its absence is a hard failure, not a skip.
        assertTrue(Files.isRegularFile(FIXTURE),
            "shared scenario input missing (expected committed fixture): " + FIXTURE);

        byte[] ts = Files.readAllBytes(FIXTURE);

        List<DemuxEvent.Metadata> metadata = new ArrayList<>();
        try (Demuxer d = new Demuxer()) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Metadata m) {
                    metadata.add(m);
                }
            }
        }

        // At least one synchronous-KLV Metadata event must surface.
        DemuxEvent.Metadata m = metadata.stream()
            .filter(ev -> ev.kind() == MetadataKind.KLV_SYNC_AU_CELL)
            .findFirst()
            .orElse(null);
        assertNotNull(m,
            "expected >=1 DemuxEvent.Metadata with kind KLV_SYNC_AU_CELL; observed=" + metadata);

        assertEquals(4145, m.stream().pid(), "golden's KLV pid");
        assertTrue(m.cellCount() >= 1, "expected >=1 AU cell, got " + m.cellCount());
        assertTrue(m.payload().remaining() > 0, "expected non-empty KLV payload");

        // The payload is the raw KLV LS — it must begin with the ST 0601 UAS
        // Datalink universal-label prefix. Read via duplicate() so the assertion
        // doesn't disturb the buffer position.
        byte[] ul = new byte[4];
        m.payload().duplicate().get(ul);
        assertArrayEquals(new byte[] {0x06, 0x0e, 0x2b, 0x34}, ul, "KLV UAS Datalink UL prefix");
    }

    // A whole KLV local set (16-byte UL key, length, value): the demuxer reads an
    // asynchronous PES as metadata only when it starts with such a key.
    private static final byte[] KLV = {
        0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01,
        0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
        0x03, 0x02, 0x01, 0x02,
    };

    /** Mux one video frame and one KLV unit pushed with {@code serviceId}, demux, return the KLV events. */
    private static List<DemuxEvent.Metadata> muxAndDemux(MuxerConfig cfg, int serviceId)
            throws Exception {
        byte[] ts;
        try (Muxer m = new Muxer(cfg)) {
            m.pushVideo(org.tstrans.TestSupport.syntheticH264Idr(), 90_000L, true);
            m.pushKlv(KLV, 90_000L, serviceId);
            m.pushVideo(org.tstrans.TestSupport.syntheticH264Idr(), 93_000L, false);
            // A second unit: a PES whose length the demuxer cannot know ends at
            // the next start on its PID.
            m.pushKlv(KLV, 93_000L, serviceId);
            ts = org.tstrans.TestSupport.drain(m);
        }
        List<DemuxEvent.Metadata> out = new ArrayList<>();
        for (DemuxEvent e : org.tstrans.TestSupport.demuxAll(ts)) {
            if (e instanceof DemuxEvent.Metadata md) out.add(md);
        }
        return out;
    }

    @Test
    void aSyncKlvEventCarriesItsMetadataServiceId() throws Exception {
        byte[] service3 = {0x26, 0x09, 0x01, 0x00, (byte) 0xFF, 'K', 'L', 'V', 'A', 3, 0x0F};
        MuxerConfig cfg = MuxerConfig.builder().pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true)
            .streamDescriptorsForKlv(0, new byte[][] {service3}).build();
        List<DemuxEvent.Metadata> events = muxAndDemux(cfg, 3);
        assertFalse(events.isEmpty());
        for (DemuxEvent.Metadata e : events) {
            assertEquals(MetadataKind.KLV_SYNC_AU_CELL, e.kind());
            assertEquals(OptionalInt.of(3), e.metadataServiceId());
        }
    }

    @Test
    void anAsyncKlvEventHasNoMetadataServiceId() throws Exception {
        MuxerConfig cfg = MuxerConfig.builder().pmtPid(0x100)
            .addVideo(0x101, VideoCodec.H264)
            .addKlv(0x102, KlvStreamType.PRIVATE_DATA, true).build();
        List<DemuxEvent.Metadata> events = muxAndDemux(cfg, 0);
        assertFalse(events.isEmpty());
        for (DemuxEvent.Metadata e : events) {
            assertEquals(MetadataKind.KLV_ASYNC, e.kind());
            assertEquals(OptionalInt.empty(), e.metadataServiceId());
        }
    }

    @Test
    void theSixArgumentConstructorGivesAnEmptyServiceId() {
        DemuxEvent.Metadata m = new DemuxEvent.Metadata(new StreamId(0x102, new StreamKind.KlvSync(null), 1), 0L,
            MetadataKind.KLV_SYNC_AU_CELL, java.nio.ByteBuffer.wrap(KLV), false, 1);
        assertEquals(OptionalInt.empty(), m.metadataServiceId());
    }
}
