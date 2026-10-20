package org.tstrans.webrtc;

/** What a mount is fed with. Mirrors {@code tst_webrtc::MountKind}. */
public enum MountKind {
    /**
     * Encoded access units, KLV records and audio frames pushed one by one with
     * {@link MountHandle#pushVideo}, {@link MountHandle#pushKlv} and
     * {@link MountHandle#pushAudio}.
     */
    ELEMENTARY,
    /** A complete MPEG-TS byte stream fed with {@link MountHandle#pushTs}. */
    MP2T
}
