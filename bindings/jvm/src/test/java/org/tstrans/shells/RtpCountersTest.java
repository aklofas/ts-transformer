package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.Optional;
import org.junit.jupiter.api.Test;
import org.tstrans.rtp.RtpStats;

/** The fold that keeps a receiving shell's RTP counters from going back. */
class RtpCountersTest {
    /** Two reads on two threads may land in either order: a lower later one changes nothing. */
    @Test
    void aLowerLaterReadDoesNotGoBack() {
        RtpCounters c = new RtpCounters(new RtpStats(0, 0));
        assertEquals(Optional.of(new RtpStats(5, 2)), c.read(new RtpStats(5, 2)));
        assertEquals(Optional.of(new RtpStats(5, 2)), c.read(new RtpStats(3, 1)));
        // Each counter is kept on its own.
        assertEquals(Optional.of(new RtpStats(5, 4)), c.read(new RtpStats(4, 4)));
        // So is the close's read.
        c.atClose(new long[] {1, 1});
        assertEquals(Optional.of(new RtpStats(5, 4)), c.read(null));
    }
}
