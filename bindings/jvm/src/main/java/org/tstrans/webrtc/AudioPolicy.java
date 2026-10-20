package org.tstrans.webrtc;

/** What happens to audio pushed into a mount. Mirrors {@code tst_webrtc::AudioPolicy}. */
public enum AudioPolicy {
    /** Audio is discarded; viewers get video and KLV only. */
    DROP,
    /**
     * Each pushed buffer is one Opus packet, forwarded unchanged to the viewers
     * whose offer asks to receive Opus. Needs an {@link MountKind#ELEMENTARY} mount.
     */
    PASSTHROUGH_OPUS
}
