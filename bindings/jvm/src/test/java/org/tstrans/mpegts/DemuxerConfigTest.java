package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.syntheticH264Idr;
import java.io.ByteArrayOutputStream;
import java.nio.file.*;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.DemuxException;

class DemuxerConfigTest {
    // A valid PAT + a PMT with a deliberately-corrupted CRC-32 (564 bytes). Under
    // StrictMode.FULL the demuxer escalates the PsiChecksumMismatch to a hard
    // StrictRejection (→ JNI code STRICT_REJECTION); under the default StrictMode.OFF
    // it tolerates the mismatch (no throw, surfaces a NonConformant event). The
    // sibling `strict-rejection` fixture is NOT used here: it is garbage bytes that
    // hit DemuxError::Unrecoverable (→ INTERNAL) regardless of strictness, so it
    // does not exercise the strict-MODE knob.
    private static final Path FIXTURE =
        Path.of(System.getProperty("user.dir"), "..", "..",
                "crates/tst-integration/tests/fixtures/scenarios/malformed-psi-strict/input.bin")
            .normalize();

    @Test
    void strictFullRejectsNonConformantInput() throws Exception {
        byte[] bytes = Files.readAllBytes(FIXTURE);
        DemuxerConfig cfg = DemuxerConfig.builder().strictMode(StrictMode.FULL).build();
        try (Demuxer d = new Demuxer(cfg)) {
            // The PMT CRC mismatch is detected inside feed()'s packet loop, so the
            // StrictRejection surfaces on feed(), not flush().
            DemuxException ex = assertThrows(DemuxException.class, () -> d.feed(bytes));
            assertEquals(DemuxException.Kind.STRICT_REJECTION, ex.kind(),
                "StrictMode.FULL must reject the corrupted-PMT-CRC fixture with STRICT_REJECTION");
        }
    }

    @Test
    void defaultConfigDoesNotReject() throws Exception {
        // Control: the SAME bytes under the default config (StrictMode.OFF) must
        // NOT throw — proving the strict knob is what changed the outcome. The
        // mismatch is tolerated and surfaced as a NonConformant event instead.
        byte[] bytes = Files.readAllBytes(FIXTURE);
        try (Demuxer d = new Demuxer()) {
            assertDoesNotThrow(() -> {
                d.feed(bytes);
                d.flush();
            });
            boolean sawNonConformant = false;
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.NonConformant) {
                    sawNonConformant = true;
                }
            }
            assertTrue(sawNonConformant,
                "default config tolerates the mismatch and surfaces a NonConformant event");
        }
    }

    @Test
    void negativeCapsAreRejected() {
        // 0 = "use the Rust default" sentinel; a negative would be silently
        // coerced to the default by the JNI bridge, so the builder rejects it.
        assertThrows(IllegalArgumentException.class,
            () -> DemuxerConfig.builder().pesCapPerPid(-1));
        assertThrows(IllegalArgumentException.class,
            () -> DemuxerConfig.builder().pesCapTotal(-1));
        assertThrows(IllegalArgumentException.class,
            () -> DemuxerConfig.builder().auCellCapPerPid(-1));
        assertThrows(IllegalArgumentException.class,
            () -> DemuxerConfig.builder().syncBufCap(-1));
    }

    @Test
    void syncBufCapPermitsWholeFileFeed() throws Exception {
        // 5 MiB of valid TS in one feed: default config raises DemuxException
        // with a message naming sync_buf_cap; raised cap accepts it.
        byte[] pkt = new byte[188];
        pkt[0] = 0x47;
        pkt[1] = 0x1f;
        pkt[2] = (byte) 0xff;
        pkt[3] = 0x10;
        java.util.Arrays.fill(pkt, 4, 188, (byte) 0xff);
        int count = (5 * 1024 * 1024) / 188 + 1;
        byte[] data = new byte[count * 188];
        for (int i = 0; i < count; i++) {
            System.arraycopy(pkt, 0, data, i * 188, 188);
        }

        // Default config: feed of 5 MiB throws DemuxException with SYNC_LOSS kind.
        try (Demuxer d = new Demuxer()) {
            DemuxException ex = assertThrows(DemuxException.class, () -> d.feed(data));
            assertTrue(ex.getMessage().contains("sync_buf_cap"),
                "error message must mention sync_buf_cap; got: " + ex.getMessage());
        }

        // Raised cap: same feed must not throw.
        DemuxerConfig cfg = DemuxerConfig.builder()
            .syncBufCap(16L * 1024 * 1024)
            .build();
        try (Demuxer d = new Demuxer(cfg)) {
            assertDoesNotThrow(() -> d.feed(data));
        }
    }

    /** The 33-bit PTS rollover boundary (ITU-T H.222.0 §2.4.3.6). */
    private static final long WRAP = 1L << 33;

    @Test
    void unwrapTimestampsDefaultsOffAndIsSettable() {
        // Default must mirror tst_core's `DemuxerConfig::default()` (false).
        assertFalse(DemuxerConfig.builder().build().unwrapTimestamps(),
            "unwrapTimestamps must default to false, matching the Rust default");
        assertTrue(DemuxerConfig.builder().unwrapTimestamps(true).build().unwrapTimestamps(),
            "builder must carry the set value onto the built config");
    }

    /**
     * Wire parity with the core
     * {@code mpegts::pts_unwrap::reordered_pts_across_wrap_does_not_double_the_epoch}
     * test: a composition order straddling the rollover ({@code WRAP-100}, {@code 100},
     * {@code WRAP-50}, {@code 200} — the pre-wrap {@code WRAP-50} arrives LATE, after
     * the wrap has been observed) must, with the knob on, place each sample in the
     * epoch its signed 33-bit delta implies. A second epoch ({@code 2*WRAP}) would mean
     * the reorder was mistaken for another wrap. With the knob off the raw wire values
     * come through unchanged.
     */
    @Test
    void unwrapTimestampsCarriesPtsAcrossTheWrap() throws Exception {
        MuxerConfig muxCfg = MuxerConfig.builder()
            .addVideo(0x100, VideoCodec.H264)
            .build();
        byte[] ts;
        try (Muxer m = new Muxer(muxCfg)) {
            for (long pts : new long[] {WRAP - 100, 100, WRAP - 50, 200}) {
                m.pushVideo(syntheticH264Idr(), pts, true);
            }
            ts = drain(m);
        }

        // Default off: byte-for-byte today's behavior — raw wire values, reorder and all.
        // Feed a *built* default config (not the no-config constructor) so this leg runs
        // through the same 9-argument native as the on leg, marshalling `false`.
        assertEquals(List.of(WRAP - 100, 100L, WRAP - 50, 200L),
            videoPts(ts, DemuxerConfig.builder().build()),
            "with the knob off the demuxer must emit the raw 33-bit wire PTS unchanged");

        // On: each sample lands in the epoch its signed 33-bit delta implies.
        DemuxerConfig unwrap = DemuxerConfig.builder().unwrapTimestamps(true).build();
        assertEquals(List.of(WRAP - 100, WRAP + 100, WRAP - 50, WRAP + 200), videoPts(ts, unwrap),
            "each sample must land in the epoch its signed 33-bit delta implies; "
                + "a second epoch (2*WRAP) means the reorder was mistaken for a wrap");
    }

    /** Drain every TS byte the muxer has queued. */
    private static byte[] drain(Muxer m) {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        int n;
        while ((n = m.pull(buf)) > 0) acc.write(buf, 0, n);
        return acc.toByteArray();
    }

    /** Feed {@code ts} through a fresh demuxer configured by {@code cfg} and collect video PTS in order. */
    private static List<Long> videoPts(byte[] ts, DemuxerConfig cfg) throws Exception {
        List<Long> out = new ArrayList<>();
        try (Demuxer d = new Demuxer(cfg)) {
            d.feed(ts);
            d.flush();
            for (DemuxEvent ev : d) {
                if (ev instanceof DemuxEvent.Video v) out.add(v.pts());
            }
        }
        return out;
    }
}
