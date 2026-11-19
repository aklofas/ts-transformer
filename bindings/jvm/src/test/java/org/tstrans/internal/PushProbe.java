package org.tstrans.internal;

/**
 * Test-only JNI hooks (never shipped) on a real {@code RtspPushSession}, compiled only
 * under the {@code jni-test-hooks} cargo feature: {@link #nHoldAsReader} holds the
 * session's lock the way a query call ({@code stats()}, {@code isSessionAlive()}) does,
 * for long enough that a test can close the session while it is held.
 */
public final class PushProbe {
    static { org.tstrans.NativeLoader.load(); }

    private PushProbe() {}

    /**
     * Hold the push session's lock as a query call does, for {@code ms} milliseconds.
     * Throws {@link IllegalStateException} for a closed session.
     */
    public static native void nHoldAsReader(long sessionHandle, long ms);

    /** Whether a {@link #nHoldAsReader} call holds the lock now. */
    public static native boolean nReaderHolds();
}
