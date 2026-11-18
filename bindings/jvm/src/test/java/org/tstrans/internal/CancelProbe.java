package org.tstrans.internal;

/**
 * jni-test-hooks probe: a cancel view whose {@code cancel()} sets its latch and then
 * blocks until {@link #nRelease(long)}, the way an SRT {@code cancel()} can sit in
 * {@code srt_close} for the socket's linger.
 */
public final class CancelProbe {
    private CancelProbe() {}
    static { org.tstrans.NativeLoader.load(); }

    /** A new blocking gate; returns its probe key. */
    public static native long nOpen();
    /** Register a cancel view over the gate; returns the key a {@code CancelHandle} wraps. */
    public static native long nCancelView(long probe);
    /** Wait up to {@code timeoutMs} for a thread to be inside the gate's {@code cancel()}. */
    public static native boolean nAwaitEntered(long probe, long timeoutMs);
    /** Let every parked {@code cancel()} return. */
    public static native void nRelease(long probe);
    /** Drop the probe key; idempotent. */
    public static native void nClose(long probe);
}
