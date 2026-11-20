package org.tstrans.webrtc;

/** Where KLV records travel to the viewer. Mirrors {@code tst_webrtc::KlvCarriage}. */
public enum KlvCarriage {
    /** In an SEI message inside the video access unit and on the {@code klv} data channel. */
    SEI_AND_DATA_CHANNEL(0),
    /** In an SEI message inside the video access unit only. */
    SEI_ONLY(1),
    /** On the {@code klv} data channel only. */
    DATA_CHANNEL_ONLY(2);

    private final int code;

    KlvCarriage(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
