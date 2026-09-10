package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Non-live (socket-free) checks for the managed basic-bytes wrappers: URL /
 * mode validation, which rejects before any connect and so runs on every
 * platform, plus the policy/stats value objects. The live-socket
 * {@code endReason()} group lives in {@link SrtManagedEndReasonTest}.
 */
class SrtManagedTest {

    /**
     * A listener-mode URL supplied to {@code ManagedSender.fromUrl} must throw
     * {@code CONFIG_INVALID} — the sender checks {@code mode=caller} up-front.
     */
    @Test
    void managedSenderRejectsListenerModeUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedSender.fromUrl("srt://127.0.0.1:9000?mode=listener")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A clearly-malformed URL fails {@code SrtUrl::parse} → {@code CONFIG_INVALID}.
     */
    @Test
    void managedSenderRejectsMalformedUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedSender.fromUrl("not-a-url")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A caller-mode URL supplied to {@code ManagedReceiver.fromUrl} must throw
     * {@code CONFIG_INVALID} — the receiver checks {@code mode=listener} up-front.
     */
    @Test
    void managedReceiverRejectsCallerModeUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedReceiver.fromUrl("srt://127.0.0.1:9000?mode=caller")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A clearly-malformed URL fails {@code SrtUrl::parse} → {@code CONFIG_INVALID}.
     */
    @Test
    void managedReceiverRejectsMalformedUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedReceiver.fromUrl("not-a-url")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * {@code PolicyArgs.from(null)} flattens the default {@link ReconnectPolicy}:
     * maxAttempts=10 (present), exponential backoff 100ms..10_000ms,
     * gapBufferCapacity=256, overflowPolicy=DROP_OLDEST (ordinal 0).
     */
    @Test
    void policyArgsDefaultsRoundTrip() {
        PolicyArgs p = PolicyArgs.from(null);
        assertTrue(p.maxAttemptsPresent());
        assertEquals(10, p.maxAttempts());
        assertEquals(1, p.backoffKind()); // exponential
        assertEquals(100, p.backoffBaseMs());
        assertEquals(10000, p.backoffMaxMs());
        assertEquals(256, p.gapBufferCapacity());
        assertEquals(0, p.overflowPolicy()); // DROP_OLDEST
        assertEquals(0, p.mode()); // BLOCKING
    }

    /**
     * {@link ReconnectPolicy#defaults()} carries {@link ReconnectMode#BLOCKING}
     * (the pre-0.6 behavior) unless the builder overrides it.
     */
    @Test
    void reconnectPolicyModeDefaultsToBlocking() {
        assertEquals(ReconnectMode.BLOCKING, ReconnectPolicy.defaults().mode());
    }

    /**
     * {@code Builder.mode(ReconnectMode)} round-trips through {@link ReconnectPolicy#mode()}.
     */
    @Test
    void reconnectPolicyBuilderModeRoundTrip() {
        ReconnectPolicy p = ReconnectPolicy.builder().mode(ReconnectMode.BACKGROUND).build();
        assertEquals(ReconnectMode.BACKGROUND, p.mode());
    }

    /**
     * {@link PolicyArgs#from} flattens {@link ReconnectPolicy#mode()} to its
     * ordinal (0 = BLOCKING, 1 = BACKGROUND) for the JNI boundary.
     */
    @Test
    void policyArgsModeOrdinalRoundTrip() {
        assertEquals(1, PolicyArgs.from(
            ReconnectPolicy.builder().mode(ReconnectMode.BACKGROUND).build()).mode());
        assertEquals(0, PolicyArgs.from(
            ReconnectPolicy.builder().mode(ReconnectMode.BLOCKING).build()).mode());
    }

    // ── Convenience wrappers (sub-wave C, Task 2) ─────────────────────────

    private static MuxerConfig sampleProgram() {
        return MuxerConfig.builder()
            .programNumber(1)
            .pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
    }

    /**
     * A listener-mode URL supplied to {@code ManagedMuxSender.fromUrl} must throw
     * {@code CONFIG_INVALID} — the sender checks {@code mode=caller} up-front,
     * before any connect (socket-free).
     */
    @Test
    void managedMuxSenderRejectsListenerModeUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedMuxSender.fromUrl("srt://127.0.0.1:9000?mode=listener", sampleProgram())
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A clearly-malformed URL fails {@code SrtUrl::parse} → {@code CONFIG_INVALID}
     * (rejected at parse time, before any connect).
     */
    @Test
    void managedMuxSenderRejectsMalformedUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedMuxSender.fromUrl("not-a-url", sampleProgram())
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A clearly-malformed URL fails {@code SrtUrl::parse} → {@code CONFIG_INVALID}.
     * The receiver accepts BOTH listener and caller mode, so we only assert the
     * malformed-URL case (a valid-but-unconnectable URL would block on bind/dial).
     */
    @Test
    void managedDemuxReceiverRejectsMalformedUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> ManagedDemuxReceiver.fromUrl("not-a-url")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * {@code ManagedDemuxReceiver.fromUrl(url, DemuxerConfig, ReconnectPolicy)}
     * routes through {@code nFromUrlWithConfig} — the one flattened-args native
     * NOT otherwise exercised by this file's malformed-URL checks. A malformed
     * URL fails at parse time before any connect, so this stays socket-free
     * while still driving every arg (including a BACKGROUND-mode policy)
     * across the JNI boundary — the native crashes/misbehaves rather than
     * cleanly throwing if the Java declaration and Rust fn arg lists drift.
     */
    @Test
    void managedDemuxReceiverWithConfigRejectsMalformedUrl() {
        DemuxerConfig cfg = DemuxerConfig.builder().build();
        ReconnectPolicy policy = ReconnectPolicy.builder().mode(ReconnectMode.BACKGROUND).build();
        var e = assertThrows(
            SrtException.class,
            () -> ManagedDemuxReceiver.fromUrl("not-a-url", cfg, policy)
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * Pins {@link ManagedTransportStats}'s record field order (declaration order
     * = accessor order = ctor arg order for a Java record) — construct with six
     * distinct values and confirm each accessor returns the one at its position.
     * Guards against a future field reorder silently desyncing from the JNI
     * builder's positional {@code JValue} list (Rust side never sees this test;
     * it only pins the Java-side contract the natives must match).
     */
    @Test
    void managedTransportStatsFieldOrderPinned() {
        ManagedTransportStats s = new ManagedTransportStats(1L, 2L, 3L, 4L, 5L, true);
        assertEquals(1L, s.reconnectAttempts());
        assertEquals(2L, s.reconnectSuccesses());
        assertEquals(3L, s.gapLen());
        assertEquals(4L, s.gapMessagesDropped());
        assertEquals(5L, s.gapBytesDropped());
        assertTrue(s.reconnecting());
    }
}
