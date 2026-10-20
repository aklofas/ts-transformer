package org.tstrans.internal;

/** jni-test-hooks probe: play a WHEP mount with the Rust test viewer. */
public final class WebrtcProbe {
    private WebrtcProbe() {}
    static { org.tstrans.NativeLoader.load(); }

    /**
     * Connects one viewer to {@code mount} on the server behind {@code serverHandle},
     * waits until it has received a frame (at most 5 s to connect and 5 s for media)
     * and returns the frames received.
     */
    public static native int nLoopbackViewerProbe(long serverHandle, String mount);
}
