package org.tstrans.hls;

import org.tstrans.NativeHandle;

/** Filled in by the serving task. */
public final class HlsServerHandle extends NativeHandle {
    HlsServerHandle(long h) { setHandle(h); }
    @Override protected void nativeClose(long h) {}
}
