package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Offline (no live socket) tests for the srt convenience wrappers.
 *
 * <p>Note on {@code Socket.intoMuxSender}: its double-free-avoidance (zero the
 * handle BEFORE the consume-then-fallible native call) cannot be regression-
 * tested offline, because obtaining a {@code Socket} requires
 * {@code Builder.connect()} against a live peer. The fix is structural —
 * consume-first ordering means a thrown muxer-config rejection leaves the
 * socket handle already zeroed, so a subsequent {@code close()} is a no-op.
 * The live success path is exercised by the Task 4 loopback scenario test.
 */
class SrtConvenienceTest {

    private static MuxerConfig sampleConfig() {
        return MuxerConfig.builder().addVideo(0x1011, VideoCodec.H264).build();
    }

    /**
     * A listener-mode URL supplied to {@code MuxSender.fromUrl} must throw
     * {@code CONFIG_INVALID} — the wrapper checks that the parsed URL uses
     * {@code mode=caller} before attempting a connection (proven without a
     * live socket).
     */
    @Test
    void muxSenderRejectsListenerModeUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> MuxSender.fromUrl("srt://127.0.0.1:9000?mode=listener", sampleConfig())
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A clearly-malformed URL (no scheme, no port) should fail
     * {@code SrtUrl::parse} and surface as {@code CONFIG_INVALID}.
     */
    @Test
    void muxSenderRejectsMalformedUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> MuxSender.fromUrl("not-a-url", sampleConfig())
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A caller-mode URL supplied to {@code DemuxReceiver.fromUrl} must throw
     * {@code CONFIG_INVALID} — the wrapper checks that the parsed URL uses
     * {@code mode=listener} before attempting to bind (proven without a live
     * socket).
     */
    @Test
    void demuxReceiverRejectsCallerModeUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> DemuxReceiver.fromUrl("srt://127.0.0.1:9000?mode=caller")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A clearly-malformed URL (no scheme, no port) should fail
     * {@code SrtUrl::parse} and surface as {@code CONFIG_INVALID}.
     */
    @Test
    void demuxReceiverRejectsMalformedUrl() {
        var e = assertThrows(
            SrtException.class,
            () -> DemuxReceiver.fromUrl("not-a-url")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * {@code DemuxReceiver.fromUrl(url, DemuxerConfig)} routes through
     * {@code nFromUrlWithConfig} — the flattened-args native NOT otherwise
     * exercised by this file's malformed-URL checks. A malformed URL fails at
     * parse time before any bind, so this stays socket-free while still driving
     * every arg across the JNI boundary: the native crashes/misbehaves rather
     * than cleanly throwing if the Java declaration and Rust fn arg lists drift.
     * Mirrors {@code SrtManagedTest.managedDemuxReceiverWithConfigRejectsMalformedUrl}.
     */
    @Test
    void demuxReceiverWithConfigRejectsMalformedUrl() {
        var cfg = org.tstrans.mpegts.DemuxerConfig.builder().unwrapTimestamps(true).build();
        var e = assertThrows(
            SrtException.class,
            () -> DemuxReceiver.fromUrl("not-a-url", cfg)
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }
}
