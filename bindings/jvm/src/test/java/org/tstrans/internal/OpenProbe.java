package org.tstrans.internal;

/**
 * Test-only JNI hook (never shipped), compiled only under the {@code jni-test-hooks}
 * cargo feature: a cancel of an open's {@code OpenCancel} that lands after the open
 * has checked the object and before it builds its configuration.
 */
public final class OpenProbe {
    static { org.tstrans.NativeLoader.load(); }
    private OpenProbe() {}

    /**
     * With {@code armed}, the next open that has begun with an {@code OpenCancel} cancels
     * that object before it builds its configuration (once); {@code false} disarms.
     */
    public static native void nArmCancelAfterBegin(boolean armed);
}
