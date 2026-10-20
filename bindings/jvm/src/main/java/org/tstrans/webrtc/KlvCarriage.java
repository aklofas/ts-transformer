package org.tstrans.webrtc;

/** Where KLV records travel to the viewer. Mirrors {@code tst_webrtc::KlvCarriage}. */
public enum KlvCarriage {
    /** In an SEI message inside the video access unit and on the {@code klv} data channel. */
    SEI_AND_DATA_CHANNEL,
    /** In an SEI message inside the video access unit only. */
    SEI_ONLY,
    /** On the {@code klv} data channel only. */
    DATA_CHANNEL_ONLY
}
