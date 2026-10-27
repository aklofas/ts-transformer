package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.DemuxReceiver;

/**
 * Offline (no live socket) tests for the srt convenience wrappers.
 *
 * <p>Note on {@code Socket.intoMuxSender}: its double-free-avoidance (zero the
 * handle BEFORE the consume-then-fallible native call) cannot be regression-
 * tested offline, because obtaining a {@code Socket} requires
 * {@code Builder.connect()} against a live peer. The fix is structural —
 * consume-first ordering means a thrown muxer-config rejection leaves the
 * socket handle already zeroed, so a subsequent {@code close()} is a no-op.
 * The live success path is exercised by {@link SrtLoopbackScenarioTest}.
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
            () -> MuxSender.open("srt://127.0.0.1:9000?mode=listener", sampleConfig())
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A URL with no supported scheme is rejected before any transport opens, as an
     * {@code IllegalArgumentException}.
     */
    @Test
    void muxSenderRejectsMalformedUrl() {
        assertThrows(
            IllegalArgumentException.class,
            () -> MuxSender.open("not-a-url", sampleConfig())
        );
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
            () -> DemuxReceiver.open("srt://127.0.0.1:9000?mode=caller")
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * libsrt takes a passphrase of 10 to 80 bytes: 80 is the top of the range, so a
     * listener carrying one opens; 9 is below it, so the same listener is refused
     * with {@code CONFIG_INVALID} before any socket exists. A listener on port 0 is
     * the offline way to reach the passphrase check — {@code Builder} validates only
     * in its finalizers.
     */
    @Test
    void passphraseOf80BytesIsAccepted() throws SrtException {
        String url = "srt://127.0.0.1:0?mode=listener";
        try (Listener l = new Builder(url).listener().passphrase("a".repeat(80)).listen()) {
            assertNotNull(l);
        }
        var e = assertThrows(
            SrtException.class,
            () -> new Builder(url).listener().passphrase("a".repeat(9)).listen()
        );
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * A URL with no supported scheme is rejected before any transport opens, as an
     * {@code IllegalArgumentException}.
     */
    @Test
    void demuxReceiverRejectsMalformedUrl() {
        assertThrows(
            IllegalArgumentException.class,
            () -> DemuxReceiver.open("not-a-url")
        );
    }

    /**
     * {@code DemuxReceiver.open(url, DemuxerConfig)} routes through
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
        assertThrows(
            IllegalArgumentException.class,
            () -> DemuxReceiver.open("not-a-url", cfg)
        );
    }
}
