package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.params.provider.Arguments.arguments;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Map;
import java.util.function.ToIntFunction;
import java.util.stream.Collectors;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;
import org.tstrans.codec.MispTimeKind;
import org.tstrans.hls.HlsMode;
import org.tstrans.mpegts.Av1CarriageMode;
import org.tstrans.mpegts.AudioCodec;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.StrictMode;
import org.tstrans.mpegts.SubtitleCodec;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.rtp.H264Receiver;
import org.tstrans.rtp.ParameterSetInjection;
import org.tstrans.shells.OpenCancel;
import org.tstrans.srt.ManagedSender;
import org.tstrans.srt.OverflowPolicy;
import org.tstrans.srt.ReconnectMode;
import org.tstrans.webrtc.AudioPolicy;
import org.tstrans.webrtc.KlvCarriage;
import org.tstrans.webrtc.MountKind;

/**
 * Each enum below crosses JNI as its {@code code()}, which the native decode table in
 * bindings/jvm/src (the file is named per row) maps back to a variant. Changing a code here
 * without the Rust match arm silently selects a different variant; a constant added without a
 * row here fails.
 */
class OrdinalContractTest {

    @ParameterizedTest(name = "{0}")
    @MethodSource("contracts")
    void codesMatchTheNativeDecodeTable(String name, Map<String, Integer> codes, Map<String, Integer> nativeTable) {
        assertEquals(nativeTable, codes);
    }

    static <E extends Enum<E>> Arguments row(Class<E> e, ToIntFunction<E> code, Map<String, Integer> nativeTable) {
        Map<String, Integer> codes = Arrays.stream(e.getEnumConstants())
            .collect(Collectors.toMap(Enum::name, code::applyAsInt));
        return arguments(e.getSimpleName(), codes, nativeTable);
    }

    static Stream<Arguments> contracts() {
        return Stream.of(
            row(MountKind.class, MountKind::code, Map.of("ELEMENTARY", 0, "MP2T", 1)),            // webrtc/server.rs
            row(AudioPolicy.class, AudioPolicy::code, Map.of("DROP", 0, "PASSTHROUGH_OPUS", 1)),  // webrtc/server.rs
            row(KlvCarriage.class, KlvCarriage::code,
                Map.of("SEI_AND_DATA_CHANNEL", 0, "SEI_ONLY", 1, "DATA_CHANNEL_ONLY", 2)),        // webrtc/server.rs
            row(VideoCodec.class, VideoCodec::code,
                Map.of("H264", 0, "H265", 1, "H266", 2, "AV1", 3)),  // mpegts/muxer.rs, mpegts/mod.rs, codec/misp_time.rs, webrtc/server.rs
            row(AudioCodec.class, AudioCodec::code,
                Map.of("MP2", 0, "AAC", 1, "AAC_LATM", 2, "AC3", 3)),                             // mpegts/muxer.rs, mpegts/mod.rs
            row(SubtitleCodec.class, SubtitleCodec::code,
                Map.of("DVB_SUBTITLING", 0, "DVB_TELETEXT", 1, "CEA708_STANDALONE", 2, "WEBVTT_IN_TS", 3)), // mpegts/muxer.rs
            row(KlvStreamType.class, KlvStreamType::code,
                Map.of("SYNCHRONOUS_METADATA", 0, "PRIVATE_DATA", 1)),                            // mpegts/muxer.rs
            row(Av1CarriageMode.class, Av1CarriageMode::code,
                Map.of("MPEG2_TS_BINDING", 0, "INTEROP_RAW_OBU", 1)),                             // mpegts/muxer.rs, mpegts/mod.rs
            row(StrictMode.class, StrictMode::code,
                Map.of("OFF", 0, "TIMING_ONLY", 1, "PSI_ONLY", 2, "FULL", 3)),                    // mpegts/mod.rs
            row(MispTimeKind.class, MispTimeKind::code, Map.of("MICRO", 0, "NANO", 1)),           // mpegts/muxer.rs
            row(OverflowPolicy.class, OverflowPolicy::code, Map.of("DROP_OLDEST", 0, "REJECT", 1)), // srt/mod.rs
            row(ReconnectMode.class, ReconnectMode::code, Map.of("BLOCKING", 0, "BACKGROUND", 1)),  // srt/mod.rs
            row(ParameterSetInjection.class, ParameterSetInjection::code,
                Map.of("NONE", 0, "BEFORE_IDR", 1)),                                              // rtp/h264_receiver.rs
            row(HlsMode.class, HlsMode::code, Map.of("LIVE", 0, "EVENT", 1, "VOD", 2)));          // hls/publisher.rs
    }

    /**
     * A strictness code the native table does not know is enum drift: it is refused as an
     * internal failure (a {@code RuntimeException}, as for an unknown AV1 carriage code), not
     * mapped to some level.
     */
    @Test
    void anUnknownStrictModeCodeIsInternal() throws Exception {
        Method open = Demuxer.class.getDeclaredMethod("nOpenWithConfig", int.class, long.class, long.class,
            boolean.class, int.class, long.class, boolean.class, long.class, boolean.class);
        open.setAccessible(true);
        InvocationTargetException e = assertThrows(InvocationTargetException.class,
            () -> open.invoke(null, 99, 0L, 0L, false, Av1CarriageMode.MPEG2_TS_BINDING.code(), 0L, false, 0L, false));
        assertSame(RuntimeException.class, e.getCause().getClass(), String.valueOf(e.getCause()));
        assertTrue(e.getCause().getMessage().contains("unknown StrictMode code 99"), e.getCause().getMessage());
    }

    /**
     * An unknown reconnect mode is refused as an invalid configuration, as an unknown overflow
     * policy is, before any connection attempt; it is not run as {@code BLOCKING}.
     */
    @Test
    void anUnknownReconnectModeCodeIsConfigInvalid() throws Exception {
        Method open = ManagedSender.class.getDeclaredMethod("nFromUrl", String.class, boolean.class, int.class,
            int.class, long.class, long.class, int.class, int.class, int.class, OpenCancel.class);
        open.setAccessible(true);
        InvocationTargetException e = assertThrows(InvocationTargetException.class,
            () -> open.invoke(null, "srt://127.0.0.1:49199", false, 0, 0, 100L, 10_000L, 256,
                OverflowPolicy.DROP_OLDEST.code(), 99, null));
        SrtException s = assertInstanceOf(SrtException.class, e.getCause());
        assertEquals(SrtException.Kind.CONFIG_INVALID, s.kind(), s.getMessage());
        assertTrue(s.getMessage().contains("unknown ReconnectMode code 99"), s.getMessage());
    }

    /**
     * An unknown parameter-set injection code is enum drift: refused as an internal failure
     * before the receiver binds, not read as {@code NONE}.
     */
    @Test
    void anUnknownParameterSetInjectionCodeIsInternal() throws Exception {
        Method listen = H264Receiver.class.getDeclaredMethod("nListenWithConfig", String.class, int.class,
            int.class, byte[][].class, long.class);
        listen.setAccessible(true);
        InvocationTargetException e = assertThrows(InvocationTargetException.class,
            () -> listen.invoke(null, "rtp://127.0.0.1:0?pt=96", 96, 99, null, 0L));
        assertSame(RuntimeException.class, e.getCause().getClass(), String.valueOf(e.getCause()));
        assertTrue(e.getCause().getMessage().contains("unknown ParameterSetInjection code 99"),
            e.getCause().getMessage());
    }
}
