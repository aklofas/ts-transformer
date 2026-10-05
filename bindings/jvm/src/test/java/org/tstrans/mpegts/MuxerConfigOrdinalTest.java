package org.tstrans.mpegts;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import org.junit.jupiter.api.Test;
import org.tstrans.MuxException;

/**
 * Verifies that the Rust-side config-enum ordinal decode helpers in
 * {@code build_muxer_config_from_arrays} reject out-of-range ordinals with
 * {@code CONFIG_INVALID} instead of silently falling back.
 *
 * <p>Drives {@code Muxer.nOpen} via reflection with ordinal 99 injected into
 * the relevant array slot — a value that the typed Java enum API cannot
 * produce in normal usage but that could appear under enum drift (caller
 * compiled against a future version of the binding). No
 * {@code --add-opens} is required: the compiled library loads as an unnamed
 * module on the test classpath, so reflective access to the package-private
 * {@code nOpen} native is unrestricted.
 *
 * <p>Valid-ordinal boundary coverage lives in {@link MuxerTest} and
 * {@link MuxerConfigTest} via the normal typed-enum builder path.
 */
class MuxerConfigOrdinalTest {

    // Cached reflective handle to Muxer.nOpen — obtained once to avoid
    // repeated getDeclaredMethod overhead across test methods.
    private static final Method N_OPEN;

    static {
        try {
            N_OPEN = Muxer.class.getDeclaredMethod("nOpen",
                    int.class, int.class, int.class,
                    int.class, int.class, int.class, int.class,
                    int[].class, int[].class, int[].class, int[].class,
                    boolean[].class, byte[].class, int[].class);
            N_OPEN.setAccessible(true);
        } catch (NoSuchMethodException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /**
     * Invoke {@code Muxer.nOpen} with one stream and the supplied per-stream
     * codec/type-code values. The invalid ordinal is injected here; all other
     * args use defaults that would form a structurally valid config for a single
     * video stream (so any exception comes from the ordinal decode, not from an
     * unrelated structural check).
     */
    private static void invokeNOpen(
            int av1Carriage, int streamKind, int streamCodec, int streamTypeCode)
            throws Throwable {
        try {
            N_OPEN.invoke(null,
                    /* programNumber  */ 1,
                    /* pmtPid         */ 0x1000,
                    /* pcrPid         */ -1,
                    /* pcrIntervalMs  */ 40,
                    /* psiIntervalMs  */ 100,
                    /* bufferPackets  */ 10_000,
                    /* av1Carriage    */ av1Carriage,
                    /* streamPids     */ new int[]{0x1011},
                    /* streamKinds    */ new int[]{streamKind},
                    /* streamCodecs   */ new int[]{streamCodec},
                    /* streamTypeCodes*/ new int[]{streamTypeCode},
                    /* streamCarriesPts */ new boolean[]{false},
                    /* dataDescBytes  */ new byte[0],
                    /* dataDescLens   */ new int[]{0});
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    // ── invalid-ordinal paths (must throw CONFIG_INVALID) ────────────────────

    @Test
    void unknownVideoCodecOrdinalThrowsConfigInvalid() throws Throwable {
        MuxException ex = assertThrows(MuxException.class,
                () -> invokeNOpen(0, MuxerConfig.KIND_VIDEO, 99, -1));
        assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind(),
                "out-of-range VideoCodec ordinal must yield CONFIG_INVALID");
    }

    @Test
    void unknownAudioCodecOrdinalThrowsConfigInvalid() throws Throwable {
        MuxException ex = assertThrows(MuxException.class,
                () -> invokeNOpen(0, MuxerConfig.KIND_AUDIO, 99, -1));
        assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind(),
                "out-of-range AudioCodec ordinal must yield CONFIG_INVALID");
    }

    @Test
    void unknownKlvTypeOrdinalThrowsConfigInvalid() throws Throwable {
        // KIND_KLV uses streamTypeCodes, not streamCodecs.
        MuxException ex = assertThrows(MuxException.class,
                () -> invokeNOpen(0, MuxerConfig.KIND_KLV, -1, 99));
        assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind(),
                "out-of-range KlvStreamType ordinal must yield CONFIG_INVALID");
    }

    @Test
    void unknownAv1CarriageOrdinalThrowsConfigInvalid() throws Throwable {
        // av1Carriage is a scalar arg, not in the per-stream arrays. Use a
        // valid video codec ordinal (H264=0) so the exception comes from the
        // av1_mode decode, not from the codec decode.
        MuxException ex = assertThrows(MuxException.class,
                () -> invokeNOpen(99, MuxerConfig.KIND_VIDEO, 0, -1));
        assertEquals(MuxException.Kind.CONFIG_INVALID, ex.kind(),
                "out-of-range Av1CarriageMode ordinal must yield CONFIG_INVALID");
    }

    // ── valid boundary ordinals (must NOT throw) ─────────────────────────────
    // These use the public typed-enum API so no reflection or handle leaks.

    @Test
    void highestValidVideoCodecOrdinalAccepted() throws MuxException {
        // VideoCodec.AV1 is ordinal 3 — the highest; must not throw after the
        // strict-ordinal change.
        MuxerConfig cfg = MuxerConfig.builder()
                .addVideo(0x1011, VideoCodec.AV1)
                .build();
        try (Muxer m = new Muxer(cfg)) {
            assertNotNull(m, "AV1 codec (ordinal 3) must produce a valid Muxer");
        }
    }

    @Test
    void klvTypeOrdinal1AcceptedAsPrivateData() throws MuxException {
        // KlvStreamType.PRIVATE_DATA is ordinal 1. The Rust arm used to be
        // '_ => PrivateData' which covered ordinal 1 AND any out-of-range
        // value; now both ordinals 0 and 1 are explicit. Verify ordinal 1 still
        // works. A video stream is included to provide a PCR-eligible stream
        // (a KLV-only program has no eligible PCR source and would throw
        // CONFIG_INVALID for an unrelated reason — obscuring the test intent).
        MuxerConfig cfg = MuxerConfig.builder()
                .addVideo(0x1011, VideoCodec.H264)
                .addKlv(0x1031, KlvStreamType.PRIVATE_DATA, true)
                .build();
        try (Muxer m = new Muxer(cfg)) {
            assertNotNull(m, "KlvStreamType.PRIVATE_DATA (ordinal 1) must produce a valid Muxer");
        }
    }
}
