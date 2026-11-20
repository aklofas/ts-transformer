package org.tstrans.webrtc;

/** What happens to audio pushed into a mount. Mirrors {@code tst_webrtc::AudioPolicy}. */
public enum AudioPolicy {
    /** Audio is discarded; viewers get video and KLV only. */
    DROP(0),
    /**
     * Each pushed buffer is one Opus packet, forwarded unchanged to the viewers
     * whose offer asks to receive Opus. Needs an {@link MountKind#ELEMENTARY} mount.
     */
    PASSTHROUGH_OPUS(1);

    private final int code;

    AudioPolicy(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
