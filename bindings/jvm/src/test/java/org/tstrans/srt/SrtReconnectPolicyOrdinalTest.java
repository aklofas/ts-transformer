package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;

/**
 * Verifies that the Rust-side reconnect-policy ordinal decode in
 * {@code build_reconnect_policy} rejects out-of-range ordinals with
 * {@code CONFIG_INVALID} instead of silently falling back.
 *
 * <p>Drives {@code ManagedSender.nFromUrl} via reflection with ordinal 99
 * injected into {@code backoffKind} or {@code overflowPolicy} — values that
 * the typed Java API cannot produce in normal usage but that could appear under
 * enum drift. The ordinal check is performed before the initial connection
 * attempt, so no real SRT endpoint is needed.
 */
class SrtReconnectPolicyOrdinalTest {

    // Cached reflective handle to ManagedSender.nFromUrl.
    private static final Method N_FROM_URL;

    static {
        try {
            N_FROM_URL = ManagedSender.class.getDeclaredMethod("nFromUrl",
                    String.class,
                    boolean.class, int.class,
                    int.class, long.class, long.class,
                    int.class, int.class, int.class);
            N_FROM_URL.setAccessible(true);
        } catch (NoSuchMethodException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /**
     * Invoke {@code ManagedSender.nFromUrl} with the given ordinal values
     * ({@code mode} pinned to the always-valid 0 = BLOCKING; it is not under
     * test here). All other args use structurally-valid defaults; the ordinal
     * check fires before the initial connection attempt so no listening server
     * is required.
     */
    private static void invokeNFromUrl(int backoffKind, int overflowPolicy) throws Throwable {
        try {
            N_FROM_URL.invoke(null,
                    /* url                 */ "srt://127.0.0.1:49199",
                    /* maxAttemptsPresent  */ false,
                    /* maxAttempts        */ 0,
                    /* backoffKind        */ backoffKind,
                    /* backoffBaseMs      */ 100L,
                    /* backoffMaxMs       */ 10_000L,
                    /* gapBufferCapacity  */ 256,
                    /* overflowPolicy     */ overflowPolicy,
                    /* mode               */ 0);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    // ── invalid-ordinal paths (must throw CONFIG_INVALID) ────────────────────

    @Test
    void unknownBackoffKindOrdinalThrowsConfigInvalid() throws Throwable {
        SrtException ex = assertThrows(SrtException.class,
                () -> invokeNFromUrl(99, 0));
        assertEquals(SrtException.Kind.CONFIG_INVALID, ex.kind(),
                "out-of-range BackoffStrategy ordinal must yield CONFIG_INVALID");
    }

    @Test
    void unknownOverflowPolicyOrdinalThrowsConfigInvalid() throws Throwable {
        SrtException ex = assertThrows(SrtException.class,
                () -> invokeNFromUrl(0, 99));
        assertEquals(SrtException.Kind.CONFIG_INVALID, ex.kind(),
                "out-of-range OverflowPolicy ordinal must yield CONFIG_INVALID");
    }

    // ── valid boundary ordinals (must NOT throw CONFIG_INVALID) ──────────────
    // The initial connect to a non-listening port will still fail, but the
    // exception kind must not be CONFIG_INVALID — proving the ordinal was accepted.

    @Test
    void highestValidBackoffKindOrdinalAccepted() throws Throwable {
        // BackoffStrategy ordinal 1 = Exponential (the highest valid value).
        SrtException ex = assertThrows(SrtException.class,
                () -> invokeNFromUrl(1, 0));
        assertNotEquals(SrtException.Kind.CONFIG_INVALID, ex.kind(),
                "ordinal 1 (Exponential) must not produce CONFIG_INVALID");
    }

    @Test
    void highestValidOverflowPolicyOrdinalAccepted() throws Throwable {
        // OverflowPolicy ordinal 1 = Reject (the highest valid value).
        SrtException ex = assertThrows(SrtException.class,
                () -> invokeNFromUrl(0, 1));
        assertNotEquals(SrtException.Kind.CONFIG_INVALID, ex.kind(),
                "ordinal 1 (Reject) must not produce CONFIG_INVALID");
    }
}
