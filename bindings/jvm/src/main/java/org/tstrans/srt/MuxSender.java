package org.tstrans.srt;

import org.tstrans.BindingException;
import org.tstrans.MuxException;
import org.tstrans.SrtException;
import org.tstrans.mpegts.MuxerConfig;

/**
 * The 0.8 name of {@link org.tstrans.shells.MuxSender} over SRT.
 *
 * @deprecated use {@link org.tstrans.shells.MuxSender#open(String, MuxerConfig)}; removed in 0.10.0.
 */
@Deprecated(since = "0.9.0", forRemoval = true)
public final class MuxSender extends org.tstrans.shells.MuxSender {
    private MuxSender(long handle) { super(handle); }

    /**
     * @deprecated use {@link org.tstrans.shells.MuxSender#open(String, MuxerConfig)}.
     */
    @Deprecated(since = "0.9.0", forRemoval = true)
    public static MuxSender fromUrl(String url, MuxerConfig programConfig) throws SrtException, MuxException {
        ShellAdapters.requireSrt(url, "MuxSender");
        try {
            return new MuxSender(openHandle(url, programConfig));
        } catch (MuxException e) {
            throw e;
        } catch (BindingException e) {
            throw ShellAdapters.srt(e, url);
        }
    }
}
