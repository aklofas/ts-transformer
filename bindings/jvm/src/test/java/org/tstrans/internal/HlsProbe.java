package org.tstrans.internal;

import org.tstrans.HlsException;

/** jni-test-hooks probe: raise an HlsException of the named kind from Rust. */
public final class HlsProbe {
    private HlsProbe() {}
    static { org.tstrans.NativeLoader.load(); }
    public static native void nRaise(String kind, String message) throws HlsException;
}
