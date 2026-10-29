package org.tstrans.pipeline;

import static org.junit.jupiter.api.Assertions.*;

import java.io.ByteArrayOutputStream;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.DemuxException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Cross-binding parity capstone for {@link Pairer} — the FIRST live exercise of
 * the 8 JNI natives behind the byte-feeding pairer. Mirrors the Python reference
 * {@code bindings/python/tests/test_pipeline_pairer.py}: build a sync-KLV fixture
 * in-JVM with the offline {@link Muxer} (5 video AUs + 5 KLV records at matching
 * PTS), then feed it through the {@link Pairer} and assert exactly 5 Paired — the
 * same guarantee the Rust core test {@code pairing_demuxer_round_trip.rs} proves.
 *
 * <p>Self-validating: the pass-through event stream is cross-checked against a
 * bare {@link Demuxer} oracle for the same bytes. {@code feed} is pure compute
 * (never parks), so this is a plain synchronous test — no live-socket / watchdog
 * machinery.
 */
class PairerTest {

    private static final int VIDEO_PID = 0x101;
    private static final int KLV_PID = 0x102;
    private static final int N = 5;

    /** Minimal H.264 AU: AUD (nal_type=9) + IDR (nal_type=5), Annex-B — mirrors
     *  the Rust pairing_demuxer_round_trip.rs / Python fixture. */
    private static byte[] minimalH264Au() {
        return new byte[] {
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10,
            0x00, 0x00, 0x00, 0x01, 0x65, (byte) 0xAA, (byte) 0xBB, (byte) 0xCC
        };
    }

    /** 16-byte SMPTE UL (ST 0601 key) + 1-byte BER length (4) + 4-byte value. */
    private static byte[] dummyKlv() {
        return new byte[] {
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01,
            0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
            4, 0x01, 0x02, 0x03, 0x04
        };
    }

    /** Sync-KLV fixture — built ONCE (the byte helpers are deterministic, so the 4
     *  tests that need it share one drain instead of re-muxing on every call). */
    private static final byte[] SYNC_KLV_BYTES;

    static {
        try {
            SYNC_KLV_BYTES = buildSyncKlvBytes(N);
        } catch (Exception e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /** Mux 5 video AUs + 5 KLV records at matching PTS (sync-KLV fixture); drain TS bytes. */
    private static byte[] buildSyncKlvBytes(int count) throws Exception {
        MuxerConfig cfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x100)
            .addVideo(VIDEO_PID, VideoCodec.H264)
            .addKlv(KLV_PID, KlvStreamType.SYNCHRONOUS_METADATA, /*carriesPts=*/ true)
            .build();
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[188 * 64];
        try (Muxer m = new Muxer(cfg)) {
            for (int i = 0; i < count; i++) {
                long pts = 90_000 + i * 3000L;
                m.pushVideo(minimalH264Au(), pts, /*keyFrame=*/ true);
                m.pushKlv(dummyKlv(), pts, /*metadataServiceId=*/ 0);
            }
            int n;
            while ((n = m.pull(out)) > 0) {
                acc.write(out, 0, n);
            }
        }
        return acc.toByteArray();
    }

    /** A batch far past the JNI local-ref table keeps every output, in order. */
    @Test
    void largeBatchKeepsEveryOutputInOrder() throws Exception {
        int n = 2000;
        try (Pairer p = new Pairer(VIDEO_PID, KLV_PID)) {
            List<PairerOutput.Paired> paired = pairedOf(feedAll(p, buildSyncKlvBytes(n)));
            assertEquals(n, paired.size());
            for (int i = 0; i < n; i++) {
                assertEquals(90_000 + i * 3000L, paired.get(i).video().pts());
            }
        }
    }

    private static List<PairerOutput> feedAll(Pairer p, byte[] data) throws DemuxException {
        List<PairerOutput> outs = new ArrayList<>(p.feed(data));
        outs.addAll(p.flush());
        return outs;
    }

    private static List<PairerOutput.Paired> pairedOf(List<PairerOutput> outs) {
        List<PairerOutput.Paired> paired = new ArrayList<>();
        for (PairerOutput o : outs) {
            if (o instanceof PairerOutput.Paired p) paired.add(p);
        }
        return paired;
    }

    @Test
    void realtimePairsSyncKlv() throws Exception {
        byte[] data = SYNC_KLV_BYTES;
        try (Pairer pairer = new Pairer(
                VIDEO_PID, KLV_PID,
                new PairingDemuxerConfig(
                    new PairerConfig(new PairerMode.Realtime(),
                        Duration.ofMillis(100), 32, 32),
                    null))) {
            List<PairerOutput> outs = feedAll(pairer, data);
            List<PairerOutput.Paired> paired = pairedOf(outs);

            // The Rust core proves 5/5. A different count is a wiring/conversion
            // bug — the variant breakdown helps DEBUG it (do NOT change the 5).
            List<String> breakdown = new ArrayList<>();
            for (PairerOutput o : outs) breakdown.add(o.getClass().getSimpleName());
            assertEquals(5, paired.size(),
                "expected 5 Paired, got " + paired.size() + "; variant breakdown: " + breakdown);

            assertEquals(5, pairer.stats().paired(),
                "stats().paired() must equal the returned Paired list size (" + paired.size() + ")");

            PairerOutput.Paired p0 = paired.get(0);
            assertEquals(VideoCodec.H264, p0.video().codec());
            assertFalse(p0.video().payload().isEmpty(), "video payload (NAL units) must be non-empty");
            assertTrue(p0.klv().payload().remaining() > 0, "KLV payload must be non-empty");
        }
    }

    @Test
    void passThroughMatchesBareDemuxerOracle() throws Exception {
        byte[] data = SYNC_KLV_BYTES;
        List<DemuxEvent> passThroughEvents = new ArrayList<>();
        try (Pairer pairer = new Pairer(VIDEO_PID, KLV_PID)) {
            for (PairerOutput o : feedAll(pairer, data)) {
                if (o instanceof PairerOutput.PassThrough pt) passThroughEvents.add(pt.event());
            }
        }

        // The bare Demuxer is the oracle: it must see the same ProgramMap.
        boolean oracleSawProgramMap = false;
        try (Demuxer demux = new Demuxer()) {
            demux.feed(data);
            demux.flush();
            for (DemuxEvent e : demux) {
                if (e instanceof DemuxEvent.ProgramMap) { oracleSawProgramMap = true; }
            }
        }

        boolean passThroughHasProgramMap = passThroughEvents.stream()
            .anyMatch(e -> e instanceof DemuxEvent.ProgramMap);
        List<String> ptTypes = new ArrayList<>();
        for (DemuxEvent e : passThroughEvents) ptTypes.add(e.getClass().getSimpleName());

        assertTrue(passThroughHasProgramMap,
            "no ProgramMap in pass-through events; got types: " + ptTypes);
        assertTrue(oracleSawProgramMap, "bare demuxer also saw no ProgramMap — fixture problem");
    }

    @Test
    void demuxerStatsAndReset() throws Exception {
        byte[] data = SYNC_KLV_BYTES;
        try (Pairer pairer = new Pairer(VIDEO_PID, KLV_PID)) {
            feedAll(pairer, data);

            assertTrue(pairer.demuxerStats().programMapsSeen() > 0, "PMT must have been parsed");
            assertTrue(pairer.stats().paired() > 0, "pairing must have happened");

            pairer.resetStats();
            PairerStats s = pairer.stats();
            assertAll("resetStats clears all pairer counters",
                () -> assertEquals(0, s.paired(),        "paired"),
                () -> assertEquals(0, s.unpairedVideo(), "unpairedVideo"),
                () -> assertEquals(0, s.unpairedKlv(),   "unpairedKlv"),
                () -> assertEquals(0, s.passThrough(),   "passThrough"));
            // resetStats only resets pairing counters — demuxer stats are unaffected.
            assertTrue(pairer.demuxerStats().programMapsSeen() > 0,
                "resetStats must not touch demuxer counters");
        }
    }

    @Test
    void feedMalformedHandled() throws Exception {
        // 200 null bytes contain no TS sync byte (0x47): either an empty list
        // (nothing demuxed) or a DemuxException — both acceptable; never a panic.
        try (Pairer pairer = new Pairer(VIDEO_PID, KLV_PID)) {
            try {
                List<PairerOutput> result = pairer.feed(new byte[200]);
                assertTrue(result.isEmpty(),
                    "null bytes have no TS sync → expected empty list, got " + result.size());
            } catch (DemuxException expected) {
                // explicit error from strict-mode resync is also acceptable
            }
        }
    }

    @Test
    void bufferedConfigConstructsAndPairs() throws Exception {
        try (Pairer pairer = new Pairer(
                VIDEO_PID, KLV_PID,
                new PairingDemuxerConfig(
                    new PairerConfig(new PairerMode.Buffered(Duration.ofMillis(200)),
                        Duration.ofMillis(50), 32, 16),
                    null))) {
            List<PairerOutput> outs = feedAll(pairer, SYNC_KLV_BYTES);
            assertFalse(pairedOf(outs).isEmpty(), "Buffered mode must still produce Paired outputs");
        }
    }

    @Test
    void closedPairerThrows() {
        Pairer pairer = new Pairer(VIDEO_PID, KLV_PID);
        pairer.close();
        assertThrows(IllegalStateException.class, () -> pairer.feed(new byte[] {0x47}));
    }
}
