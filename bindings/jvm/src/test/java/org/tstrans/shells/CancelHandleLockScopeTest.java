package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.tstrans.internal.CancelProbe;

/**
 * A thread inside {@link CancelHandle#cancel()} (on SRT, {@code srt_close} lingering)
 * must not hold up the handle's other methods on the same object.
 */
@EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
class CancelHandleLockScopeTest {

    private static final long DEADLINE_MS = 5_000;

    @Test
    void isCancelledAnswersWhileAnotherThreadIsInsideCancel() throws Exception {
        withParkedCancel(handle -> assertTrue(
                within(handle::isCancelled, "isCancelled()"),
                "the latch is set before the transport's cancel blocks"));
    }

    @Test
    void closeReturnsWhileAnotherThreadIsInsideCancel() throws Exception {
        withParkedCancel(handle -> within(() -> {
            handle.close();
            return null;
        }, "close()"));
    }

    private interface Body {
        void run(CancelHandle handle) throws Exception;
    }

    /** Park a thread in {@code cancel()} on a fresh handle, run {@code body}, then release it. */
    private static void withParkedCancel(Body body) throws Exception {
        long probe = CancelProbe.nOpen();
        try (CancelHandle handle = new CancelHandle(CancelProbe.nCancelView(probe))) {
            Thread canceller = new Thread(handle::cancel, "parked-cancel");
            canceller.setDaemon(true);
            canceller.start();
            try {
                assertTrue(CancelProbe.nAwaitEntered(probe, DEADLINE_MS),
                        "cancel() never reached the transport's cancel");
                body.run(handle);
            } finally {
                CancelProbe.nRelease(probe);
                canceller.join(DEADLINE_MS);
            }
            assertFalse(canceller.isAlive(), "cancel() did not return after its transport's cancel was released");
        } finally {
            CancelProbe.nClose(probe);
        }
    }

    /** Run {@code call} on another thread and fail if it has not returned by the deadline. */
    private static <T> T within(Callable<T> call, String what) throws Exception {
        ExecutorService pool = Executors.newSingleThreadExecutor(r -> {
            Thread t = new Thread(r, "lock-scope-probe");
            t.setDaemon(true);
            return t;
        });
        try {
            Future<T> result = pool.submit(call);
            try {
                return result.get(DEADLINE_MS, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                return fail(what + " waited for the cancel() parked on another thread");
            }
        } finally {
            pool.shutdownNow();
        }
    }
}
