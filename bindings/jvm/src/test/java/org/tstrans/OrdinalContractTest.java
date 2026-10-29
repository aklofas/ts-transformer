package org.tstrans;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.params.provider.Arguments.arguments;

import java.util.Arrays;
import java.util.List;
import java.util.stream.Stream;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;
import org.tstrans.codec.MispTimeKind;
import org.tstrans.hls.HlsMode;
import org.tstrans.mpegts.Av1CarriageMode;
import org.tstrans.mpegts.AudioCodec;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.StrictMode;
import org.tstrans.mpegts.SubtitleCodec;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.rtp.ParameterSetInjection;
import org.tstrans.srt.OverflowPolicy;
import org.tstrans.srt.ReconnectMode;
import org.tstrans.webrtc.AudioPolicy;
import org.tstrans.webrtc.KlvCarriage;
import org.tstrans.webrtc.MountKind;

/**
 * Each enum below crosses JNI as its ordinal, so its constant order is the native decode table
 * in bindings/jvm/src (the file is named per row). Reordering a constant here without the Rust
 * match arm silently selects a different variant.
 */
class OrdinalContractTest {

    @ParameterizedTest(name = "{0}")
    @MethodSource("contracts")
    void javaOrderMatchesTheNativeDecodeTable(Class<? extends Enum<?>> e, List<String> nativeOrder) {
        assertEquals(nativeOrder, Arrays.stream(e.getEnumConstants()).map(Enum::name).toList());
    }

    static Stream<Arguments> contracts() {
        return Stream.of(
            arguments(MountKind.class, List.of("ELEMENTARY", "MP2T")),                                 // webrtc/server.rs
            arguments(AudioPolicy.class, List.of("DROP", "PASSTHROUGH_OPUS")),                         // webrtc/server.rs
            arguments(KlvCarriage.class, List.of("SEI_AND_DATA_CHANNEL", "SEI_ONLY", "DATA_CHANNEL_ONLY")), // webrtc/server.rs
            arguments(VideoCodec.class, List.of("H264", "H265", "H266", "AV1")),                       // mpegts/muxer.rs, mpegts/mod.rs, codec/misp_time.rs, webrtc/server.rs
            arguments(AudioCodec.class, List.of("MP2", "AAC", "AAC_LATM", "AC3")),                     // mpegts/muxer.rs, mpegts/mod.rs
            arguments(SubtitleCodec.class,
                List.of("DVB_SUBTITLING", "DVB_TELETEXT", "CEA708_STANDALONE", "WEBVTT_IN_TS")),       // mpegts/muxer.rs
            arguments(KlvStreamType.class, List.of("SYNCHRONOUS_METADATA", "PRIVATE_DATA")),           // mpegts/muxer.rs
            arguments(Av1CarriageMode.class, List.of("MPEG2_TS_BINDING", "INTEROP_RAW_OBU")),          // mpegts/muxer.rs, mpegts/mod.rs
            arguments(StrictMode.class, List.of("OFF", "TIMING_ONLY", "PSI_ONLY", "FULL")),            // mpegts/mod.rs
            arguments(MispTimeKind.class, List.of("MICRO", "NANO")),                                   // mpegts/muxer.rs
            arguments(OverflowPolicy.class, List.of("DROP_OLDEST", "REJECT")),                         // srt/mod.rs
            arguments(ReconnectMode.class, List.of("BLOCKING", "BACKGROUND")),                         // srt/mod.rs
            arguments(ParameterSetInjection.class, List.of("NONE", "BEFORE_IDR")),                     // rtp/h264_receiver.rs
            arguments(HlsMode.class, List.of("LIVE", "EVENT", "VOD")));                                // hls/publisher.rs
    }
}
