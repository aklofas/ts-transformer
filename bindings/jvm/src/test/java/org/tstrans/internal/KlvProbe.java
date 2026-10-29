package org.tstrans.internal;

import org.tstrans.KlvDecodeException;
import org.tstrans.KlvEncodeException;

/** jni-test-hooks probe: raise a KLV decode or encode exception of the named kind from Rust. */
public final class KlvProbe {
    private KlvProbe() {}
    static { org.tstrans.NativeLoader.load(); }
    public static native void nRaiseDecode(String kind) throws KlvDecodeException;
    public static native void nRaiseEncode(String kind) throws KlvEncodeException;
}
