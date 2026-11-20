package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.condition.DisabledOnOs;
import org.junit.jupiter.api.condition.OS;
import org.tstrans.SrtException;

/**
 * Memory-safety stress test for the {@link Listener#close()} / {@link Listener#accept(Integer)}
 * race.
 *
 * <p>Before the lifetime fix, {@code close()} freed the {@code Box<Listener>} while another
 * thread was parked inside {@code srt_accept}. The {@code srt_close} fired by close woke the
 * parked accept, which then read its (now-freed) {@code accepted_*_timeout} fields → a
 * use-after-free that intermittently crashed the JVM worker with SIGSEGV.
 *
 * <p>The loop builds a listener on an ephemeral port, parks a thread in {@code accept()}, lets
 * it reach {@code srt_accept}, then calls {@code close()} from the main thread. After the fix,
 * close cancels the parked accept (it returns {@code CLOSED}) and only frees the allocation once
 * the parked accept has released the internal lock — no thread ever touches freed memory.
 *
 * <p><b>Sanctioned outcomes.</b> This is a RACE stress test: the 20ms pre-close pause makes the
 * parked interleaving overwhelmingly likely, but on a starved CI runner the accept thread can
 * lose the CPU past the pause + {@code close()}, so each iteration legitimately lands in exactly
 * one of the three clean interleavings the {@link Listener#close()} contract documents:
 * <ol>
 *   <li><b>Parked wake</b> — accept was parked in {@code srt_accept}; close's cancel hook wakes
 *       it with libsrt {@code MJ_SETUP/MN_CLOSED} → {@code SrtException(CLOSED)}. The common
 *       case, and the interleaving this test exists to drive (asserted ≥ 1 of 50 below).</li>
 *   <li><b>Close-first entry</b> — accept() entered after {@code close()} claimed the registry
 *       id → clean {@link IllegalStateException} (from {@code ensureOpen()} or the registry's
 *       closed-handle arm). This was the historical CI flake: a sanctioned, memory-safe
 *       interleaving the test used to report as "unexpected".</li>
 *   <li><b>Cancelled entry</b> — accept won the registry lease but the cancel hook's
 *       {@code srt_close} landed before the {@code srt_accept} syscall, which then sees a
 *       closed/non-listening SRTSOCKET (libsrt {@code MN_SIDINVAL} / {@code MN_NOLISTEN},
 *       neither of which maps to ListenerClosed) → {@code SrtException(ACCEPT_FAILED)}.</li>
 * </ol>
 * Anything else — a successful accept (no peer exists), any other exception, or a JVM crash —
 * is a genuine failure. The memory-safety substance is unchanged: 50 close-vs-accept races with
 * no UAF/SIGSEGV and every accept thread unblocking promptly.
 */
final class ListenerCloseRaceTest {

    /**
     * A {@code close()} that lands between {@code cancelHandle()}'s open check and its native
     * finds no listener: {@code IllegalStateException}, never a handle born closed.
     */
    @Test
    void cancelHandleOnAClosedUnderneathSrtListenerThrows() throws Exception {
        Listener l = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();
        org.tstrans.TestSupport.closeUnderneath(l);
        try {
            IllegalStateException e = assertThrows(IllegalStateException.class, l::cancelHandle);
            assertEquals("Listener is closed", e.getMessage());
        } finally {
            org.tstrans.TestSupport.forgetKey(l);
        }
    }

    @Test
    @Timeout(120) // 50 races with widened per-race waits; only a genuine hang ever pays this
    @DisabledOnOs(
            value = OS.WINDOWS,
            disabledReason =
                    "libsrt's srt_close does not reliably wake a thread parked in srt_accept within"
                        + " the 2s bound on Windows (the memory-safety fix itself holds — no UAF"
                        + " crash — but the parked accept may not unblock promptly). Full"
                        + " close-while-parked coverage runs on Linux + macOS.")
    void closeWhileAcceptParkedIsMemorySafe() throws Exception {
        // Interleaving tally across the 50 races. Written by the accept thread, read by main
        // after join() (the join is the happens-before edge; AtomicInteger for the lambda
        // capture). See the method javadoc for the three sanctioned interleavings.
        AtomicInteger parkedWakes = new AtomicInteger();
        AtomicInteger closeFirstEntries = new AtomicInteger();
        AtomicInteger cancelledEntries = new AtomicInteger();

        for (int i = 0; i < 50; i++) {
            Listener listener =
                new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();

            // Signalled by the accept thread immediately before it calls accept(), so the
            // main thread can bound-wait until accept is about to (or has) parked in srt_accept.
            CountDownLatch aboutToAccept = new CountDownLatch(1);
            AtomicReference<Throwable> unexpected = new AtomicReference<>();

            Thread acc =
                new Thread(
                    () -> {
                        aboutToAccept.countDown();
                        try {
                            // Blocks in srt_accept until close() wakes it. srt_accept reads
                            // self.handle out of the listener struct, and the success path would
                            // read self.accepted_*_timeout — the exact memory the pre-fix close()
                            // frees out from under this parked call.
                            Socket s = listener.accept(null);
                            // No peer connects, so a successful accept is itself a bug.
                            s.close();
                            unexpected.set(new AssertionError("accept returned a Socket"));
                        } catch (SrtException e) {
                            if (e.kind() == SrtException.Kind.CLOSED) {
                                // Sanctioned interleaving 1: parked in srt_accept, woken by
                                // close's cancel hook (libsrt MJ_SETUP/MN_CLOSED).
                                parkedWakes.incrementAndGet();
                            } else if (e.kind() == SrtException.Kind.ACCEPT_FAILED) {
                                // Sanctioned interleaving 3: lease won, syscall lost — the cancel
                                // hook's srt_close landed before srt_accept entered, so libsrt
                                // reports MN_SIDINVAL ("invalid socket ID") or MN_NOLISTEN ("not
                                // in listening state"); neither maps to ListenerClosed. Clean and
                                // memory-safe (the lease kept the allocation alive throughout).
                                cancelledEntries.incrementAndGet();
                            } else {
                                unexpected.set(e);
                            }
                        } catch (IllegalStateException e) {
                            // Sanctioned interleaving 2: close() claimed the handle before this
                            // thread entered accept() — the documented fresh-entry-after-close
                            // outcome (same sanction as closeVsFreshEntryIsMemorySafe below).
                            // This is the interleaving that used to flake CI runs as
                            // "unexpected" when a starved runner descheduled this thread past
                            // the main thread's 20ms pause + close().
                            closeFirstEntries.incrementAndGet();
                        } catch (Throwable t) {
                            unexpected.set(t);
                        }
                    },
                    "accept-" + i);
            acc.start();

            // Wait until the accept thread is at the brink of srt_accept, then give it a brief
            // window to actually enter the blocking call. (Thread.sleep here is in the JVM, not
            // the shell — the sandbox sleep restriction does not apply.) The pause makes the
            // parked interleaving overwhelmingly likely but deliberately does NOT guarantee it —
            // there is no JVM-visible way to observe "parked inside srt_accept", which is why
            // all three clean interleavings are sanctioned above.
            assertTrue(aboutToAccept.await(5, TimeUnit.SECONDS), "accept thread never started");
            Thread.sleep(20);

            // The race under test: free the listener while accept is (very likely) parked. Under
            // the pre-fix close() this freed the Box<Listener> the parked accept still
            // dereferences (UAF); after the fix close() cancels first and frees only once the
            // parked accept has released the internal lock.
            listener.close();

            acc.join(5000);
            assertFalse(acc.isAlive(), "accept thread did not unblock after close (run " + i + ")");
            assertNull(unexpected.get(), "accept thread saw an unexpected failure (run " + i + ")");
        }

        // Coverage guarantee: the parked-wake interleaving — the one this test exists to drive —
        // must actually have been exercised. With a 20ms park window per race, missing all 50
        // would take a scheduler that starves the accept thread for 20ms+ fifty times in a row
        // while running the main thread normally; even at a pessimistic 50% per-race miss rate
        // that is a ~1e-15 event, far below any realistic flake threshold.
        assertTrue(parkedWakes.get() >= 1,
            "no race iteration reached the parked-accept interleaving (parked=" + parkedWakes
                + ", closeFirst=" + closeFirstEntries + ", cancelled=" + cancelledEntries + ")");
    }

    /**
     * Close-vs-fresh-entry stress. With the leased {@code HandleRegistry}, a thread that
     * keeps calling a normal native getter ({@code localAddr()}) while another thread calls
     * {@code close()} must never crash the JVM; the only sanctioned exception the busy thread
     * may observe is {@link IllegalStateException} (handle claimed/closed). This is the case
     * round-1 declared unsafe (close racing a fresh entry); the registry now makes it
     * deterministically memory-safe.
     */
    @Test
    @Timeout(30)
    void closeVsFreshEntryIsMemorySafe() throws Exception {
        for (int i = 0; i < 200; i++) {
            Listener listener =
                new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();

            CountDownLatch ready = new CountDownLatch(1);
            AtomicReference<Throwable> unexpected = new AtomicReference<>();

            Thread caller =
                new Thread(
                    () -> {
                        ready.countDown();
                        // Hammer a cheap, non-blocking native getter. Each call reads the
                        // current registry id and leases it; a concurrent close() either lets
                        // the call run or trips a clean IllegalStateException — never UB.
                        for (int k = 0; k < 1000; k++) {
                            try {
                                listener.localAddr();
                            } catch (IllegalStateException expected) {
                                // Handle was claimed by close() — sanctioned, keep going.
                            } catch (SrtException e) {
                                // localAddr can surface IO if the socket is mid-teardown; not a
                                // memory-safety failure, tolerate it.
                            } catch (Throwable t) {
                                unexpected.set(t);
                                return;
                            }
                        }
                    },
                    "caller-" + i);
            caller.start();

            // Let the caller get going, then race close() against its in-flight getter calls.
            assertTrue(ready.await(2, TimeUnit.SECONDS), "caller thread never started");
            listener.close();

            caller.join(5000);
            assertFalse(caller.isAlive(), "caller thread did not finish (run " + i + ")");
            assertNull(unexpected.get(), "caller thread saw an unexpected failure (run " + i + ")");
        }
    }
}
