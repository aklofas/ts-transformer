package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.closeUnderneath;
import static org.tstrans.TestSupport.connectSender;
import static org.tstrans.TestSupport.forgetKey;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;

/**
 * A {@code close()} that lands between a managed shell's {@code cancelHandle()} open check
 * and its native finds no shell: {@code IllegalStateException}, never a handle born closed.
 * Each test closes a connected shell natively and puts its key back, so the window is hit
 * every time.
 */
class ManagedCancelHandleClosedUnderneathTest {

    @Test
    @Timeout(60)
    void cancelHandleOnAClosedUnderneathManagedSenderThrows() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (as the other managed pairs)");
        try (Listener peer = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen()) {
            ManagedSender tx = ManagedSender.fromUrl(
                "srt://127.0.0.1:" + peer.localAddr().port() + "?mode=caller");
            closeUnderneath(tx);
            try {
                IllegalStateException e = assertThrows(IllegalStateException.class, tx::cancelHandle);
                assertEquals("ManagedSender is closed", e.getMessage());
            } finally {
                forgetKey(tx);
            }
        }
    }

    @Test
    @Timeout(60)
    void cancelHandleOnAClosedUnderneathManagedReceiverThrows() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (as the other managed pairs)");
        int port = freeUdpPort();
        CompletableFuture<ManagedReceiver> rxFuture = CompletableFuture.supplyAsync(() -> {
            try {
                return ManagedReceiver.fromUrl("srt://:" + port + "?mode=listener");
            } catch (SrtException e) {
                throw new RuntimeException(e);
            }
        });
        try (ManagedMuxSender peer = connectSender("srt://127.0.0.1:" + port, 10_000)) {
            ManagedReceiver rx = rxFuture.get(10, TimeUnit.SECONDS);
            closeUnderneath(rx);
            try {
                IllegalStateException e = assertThrows(IllegalStateException.class, rx::cancelHandle);
                assertEquals("ManagedReceiver is closed", e.getMessage());
            } finally {
                forgetKey(rx);
            }
        }
    }
}
