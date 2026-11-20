package org.tstrans.internal;

/**
 * Test-only JNI hooks (never shipped) on a real {@code RtspPushSession}, compiled only
 * under the {@code jni-test-hooks} cargo feature. {@link #nHoldAsReader} holds the
 * session's lock the way a query call ({@code stats()}) does until {@link #nRelease}
 * ends it, so a test can start a close while a query holds the session and only then
 * let it go; {@link #nWaitRounds} counts the rounds a close or a conversion has waited
 * for it.
 * {@link #nMarkClosing} begins a close without finishing it, so the next blocking call
 * meets a close that has begun; {@link #nPoison} poisons the session.
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

    /**
     * The rounds every close or conversion has waited, without a cancel or a refusal,
     * for a query call holding a push session, since the library loaded: a close or a
     * conversion that met a session held by a query raises it.
     */
    public static native long nWaitRounds();

    /**
     * Begin a close of the session on another thread and return once it has begun;
     * that close takes the session once it is free, and the session's own
     * {@code close()} finishes it.
     */
    public static native void nMarkClosing(long sessionHandle);

    /**
     * Panic inside a query holding the session, as a bug in a call would, so the
     * session is poisoned; the panic is caught inside the native.
     */
    public static native void nPoison(long sessionHandle);
}
