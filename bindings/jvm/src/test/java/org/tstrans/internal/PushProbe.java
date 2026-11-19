package org.tstrans.internal;

/**
 * Test-only JNI hooks (never shipped) on a real {@code RtspPushSession}, compiled only
 * under the {@code jni-test-hooks} cargo feature. {@link #nHoldAsReader} holds the
 * session's lock the way a query call ({@code stats()}) does until {@link #nRelease}
 * ends it, so a test can start a close while a query holds the session and only then
 * let it go. {@link #nMarkClosing} sets the flag a close sets
 * first, without taking the session, so the next blocking call meets a close that has
 * begun.
 */
public final class PushProbe {
    static { org.tstrans.NativeLoader.load(); }

    private PushProbe() {}

    /**
     * Hold the push session's lock as a query call does until {@link #nRelease()} (at
     * most 30 s). Throws {@link IllegalStateException} for a closed session.
     */
    public static native void nHoldAsReader(long sessionHandle);

    /** End the hold of {@link #nHoldAsReader}. */
    public static native void nRelease();

    /** Whether a {@link #nHoldAsReader} call holds the lock now. */
    public static native boolean nReaderHolds();

    /** Mark the session as closing, as a close does before it takes the session. */
    public static native void nMarkClosing(long sessionHandle);
}
