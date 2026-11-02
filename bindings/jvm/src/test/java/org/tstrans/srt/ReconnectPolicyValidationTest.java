package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;

/**
 * A reconnect policy with zero backoff and unlimited attempts is refused at
 * open, before any connect, with the config-invalid kind.
 */
final class ReconnectPolicyValidationTest {
    @Test
    @Timeout(10)
    void managedSenderRefusesAnUnpacedUnlimitedPolicyBeforeConnecting() {
        ReconnectPolicy policy = ReconnectPolicy.builder()
            .maxAttempts(null)
            .backoff(BackoffStrategy.constant(0))
            .build();
        // Nothing listens on port 1: a connect attempt would take the connect
        // timeout, so a fast refusal proves no socket was touched.
        long started = System.nanoTime();
        SrtException e = assertThrows(
            SrtException.class, () -> ManagedSender.fromUrl("srt://127.0.0.1:1", policy));
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
        assertTrue((System.nanoTime() - started) < 2_000_000_000L);
    }
}
