package org.tstrans.webrtc;

import java.util.Objects;
import org.tstrans.mpegts.VideoCodec;

/**
 * The configuration of one mount. Mirrors {@code tst_webrtc::MountConfig}; the
 * server checks it in {@link WhepServer#addMount} and refuses it with
 * {@code WebrtcException(MOUNT)}.
 *
 * @param kind what the mount is fed with
 * @param video the video codec; H.264 and H.265 are supported
 * @param audio what happens to audio; {@link AudioPolicy#PASSTHROUGH_OPUS} needs an
 *     elementary mount
 * @param klv where KLV records travel
 * @param seiBudgetBytes the most KLV bytes carried in SEI per access unit (1 to 1 MiB)
 * @param viewerQueuePackets the RTP payloads one viewer's queue holds (at least 16)
 */
public record MountConfig(MountKind kind, VideoCodec video, AudioPolicy audio, KlvCarriage klv,
        int seiBudgetBytes, int viewerQueuePackets) {
    public MountConfig {
        Objects.requireNonNull(kind, "kind");
        Objects.requireNonNull(video, "video");
        Objects.requireNonNull(audio, "audio");
        Objects.requireNonNull(klv, "klv");
    }

    /**
     * The Rust defaults: an elementary H.264 mount, audio dropped, KLV in SEI and on
     * the data channel, a 64 KiB SEI budget and a 2048-payload viewer queue.
     */
    public static MountConfig defaults() {
        return new MountConfig(MountKind.ELEMENTARY, VideoCodec.H264, AudioPolicy.DROP,
            KlvCarriage.SEI_AND_DATA_CHANNEL, 64 * 1024, 2048);
    }
}
