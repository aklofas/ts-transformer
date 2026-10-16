package org.tstrans.srt;

import org.tstrans.BindingException;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * The 0.8 name of {@link org.tstrans.shells.DemuxReceiver} over SRT.
 *
 * @deprecated use {@link org.tstrans.shells.DemuxReceiver#open(String)}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class DemuxReceiver extends org.tstrans.shells.DemuxReceiver {
    private DemuxReceiver(long handle) { super(handle); }

    /**
     * @deprecated use {@link org.tstrans.shells.DemuxReceiver#open(String)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static DemuxReceiver fromUrl(String url) throws SrtException {
        ShellAdapters.requireSrt(url, "DemuxReceiver");
        try {
            return new DemuxReceiver(openHandle(url));
        } catch (BindingException e) {
            throw ShellAdapters.srt(e, url);
        }
    }

    /**
     * @deprecated use {@link org.tstrans.shells.DemuxReceiver#open(String, DemuxerConfig)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static DemuxReceiver fromUrl(String url, DemuxerConfig demuxConfig) throws SrtException {
        ShellAdapters.requireSrt(url, "DemuxReceiver");
        try {
            return new DemuxReceiver(openHandle(url, demuxConfig));
        } catch (BindingException e) {
            throw ShellAdapters.srt(e, url);
        }
    }
}
