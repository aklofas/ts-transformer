package org.tstrans.internal;

import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.shells.MuxSender;

/**
 * jni-test-hooks probe: a {@link MuxSender} over a test transport whose first send is refused
 * with {@code BACKPRESSURE} (the sender keeps the refused output) and whose every later send
 * waits until the shell's cancel fires, then reports the cancel. A call that has output to
 * send therefore parks inside the transport, holding the shell's slot, for as long as the
 * test wants; the transport's error domain is TCP's.
 */
public final class GatedMuxSender extends MuxSender {
    static { org.tstrans.NativeLoader.load(); }

    private GatedMuxSender(long handle) { super(handle); }

    /** A sender for {@code config} over a fresh gated transport. */
    public static GatedMuxSender open(MuxerConfig config) {
        return new GatedMuxSender(nOpen(
            config.programNumber(), config.pmtPid(), config.pcrPid(),
            config.pcrIntervalMs(), config.pcrLeadMs(), config.psiIntervalMs(),
            config.bufferPackets(), config.av1Carriage().code(),
            config.streamPids(), config.streamKinds(), config.streamCodecs(),
            config.streamTypeCodes(), config.streamCarriesPts(),
            config.dataDescBytes(), config.dataDescLens()));
    }

    private static native long nOpen(int programNumber, int pmtPid, int pcrPid,
        int pcrIntervalMs, int pcrLeadMs, int psiIntervalMs, int bufferPackets, int av1Carriage,
        int[] streamPids, int[] streamKinds, int[] streamCodecs, int[] streamTypeCodes,
        boolean[] streamCarriesPts, byte[] dataDescBytes, int[] dataDescLens);
}
