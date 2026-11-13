package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertThrows;

import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;

/**
 * The 0.8 shell classes of {@code org.tstrans.srt} and {@code org.tstrans.rtp}
 * are gone; the shells are the {@code org.tstrans.shells} classes and the raw
 * RTP transports are {@code org.tstrans.rtp.Transport} / {@code RecvTransport}.
 */
class ShellOldNamesRemovedTest {
    @ParameterizedTest
    @ValueSource(strings = {
        "org.tstrans.srt.Sender", "org.tstrans.srt.Receiver",
        "org.tstrans.srt.MuxSender", "org.tstrans.srt.DemuxReceiver",
        "org.tstrans.rtp.Sender", "org.tstrans.rtp.Receiver",
        "org.tstrans.rtp.MuxSender", "org.tstrans.rtp.DemuxReceiver"})
    void theClassNoLongerExists(String name) {
        assertThrows(ClassNotFoundException.class, () -> Class.forName(name));
    }
}
