package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.CountDownLatch;
import org.junit.jupiter.api.Test;
import org.tstrans.TestSupport;

/** {@link ShellPeers.Parked} returns only for a call that is really parked. */
class ParkedTest {
    @Test
    void aCallThatReturnsAtOnceIsNotTakenForParked() throws Exception {
        try (Receiver rx = Receiver.open("udp://127.0.0.1:" + TestSupport.freeUdpPort())) {
            AssertionError e = assertThrows(AssertionError.class,
                () -> new ShellPeers.Parked(rx, rx.cancelHandle(), "returned", () -> null).close());
            assertTrue(e.getMessage().contains("ended before it parked"), e.getMessage());
        }
    }

    @Test
    void aThreadAliveOutsideTheCallIsNotTakenForParked() throws Exception {
        CountDownLatch release = new CountDownLatch(1);
        try (Receiver rx = Receiver.open("udp://127.0.0.1:" + TestSupport.freeUdpPort())) {
            AssertionError e = assertThrows(AssertionError.class,
                () -> new ShellPeers.Parked(rx, rx.cancelHandle(), "returned", () -> {
                    release.await();
                    return null;
                }, 500).close());
            assertTrue(e.getMessage().contains("did not park within"), e.getMessage());
        } finally {
            release.countDown();
        }
    }
}
