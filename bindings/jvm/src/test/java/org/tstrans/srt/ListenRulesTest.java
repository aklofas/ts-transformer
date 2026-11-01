package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;

/**
 * {@link Listener#rules()}: per-stream-ID accept / passphrase / reject rules
 * the listener applies during each SRT handshake.
 */
class ListenRulesTest {

    private static Listener listen() throws SrtException {
        return new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();
    }

    private static Builder caller(int port, String streamId) {
        return new Builder("srt://127.0.0.1:" + port + "?streamid=" + streamId).caller();
    }

    /** Runs {@code accept(timeoutMs)} on a daemon thread and records how it ended. */
    private static Thread acceptOnDaemon(Listener listener, Integer timeoutMs,
                                         AtomicReference<Object> outcome) {
        Thread t = new Thread(() -> {
            try {
                Socket s = listener.accept(timeoutMs);
                outcome.set(s);
                s.close();
            } catch (Throwable e) {
                outcome.set(e);
            }
        }, "listen-rules-accept");
        t.setDaemon(true);
        t.start();
        return t;
    }

    @Test
    @Timeout(30)
    void rejectRuleRefusesCallerWithReason() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Listener listener = listen()) {
            int port = listener.localAddr().port();
            listener.rules().set("publish:amber", ListenRule.reject(RejectReason.FORBIDDEN));

            AtomicReference<Object> outcome = new AtomicReference<>();
            Thread acceptor = acceptOnDaemon(listener, 2000, outcome);

            SrtException e = assertThrows(SrtException.class,
                () -> caller(port, "publish:amber").connect());
            assertEquals(SrtException.Kind.CONNECT_FAILED, e.kind());
            assertTrue(e.getMessage().contains("Forbidden"),
                "the caller reads the rule's reason, got: " + e.getMessage());

            acceptor.join(10_000);
            Object got = outcome.get();
            assertTrue(got instanceof SrtException, "accept never returned a socket, got: " + got);
            assertEquals(SrtException.Kind.TIMEOUT, ((SrtException) got).kind());
        }
    }

    @Test
    @Timeout(30)
    void passphraseRuleRoundTrip() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Listener listener = listen()) {
            int port = listener.localAddr().port();
            listener.rules().set("a", ListenRule.acceptWith("secretsecretsecret"));

            try (Socket ok = caller(port, "a").passphrase("secretsecretsecret").connect()) {
                assertTrue(ok.isAlive());
            }

            SrtException e = assertThrows(SrtException.class,
                () -> caller(port, "a").passphrase("othersecretother").connect());
            assertEquals(SrtException.Kind.CONNECT_FAILED, e.kind());
            assertTrue(e.getMessage().contains("BadSecret"),
                "a wrong passphrase is refused as BadSecret, got: " + e.getMessage());
        }
    }

    @Test
    @Timeout(30)
    void rulesEditedWhileAcceptParked() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Listener listener = listen()) {
            ListenRules rules = listener.rules();
            AtomicReference<Object> outcome = new AtomicReference<>();
            CountDownLatch entered = new CountDownLatch(1);
            Thread acceptor = new Thread(() -> {
                try {
                    entered.countDown();
                    listener.accept(null).close();
                    outcome.set("socket");
                } catch (Throwable e) {
                    outcome.set(e);
                }
            }, "listen-rules-accept");
            acceptor.setDaemon(true);
            acceptor.start();
            assertTrue(entered.await(5, TimeUnit.SECONDS), "accept never started");
            Thread.sleep(300); // inside the native accept, holding the listener's slot
            assertNull(outcome.get(), "accept returned on its own; no caller connects");

            // The edits never take the listener's slot, so the parked accept
            // cannot hold them up.
            Integer size = CompletableFuture.supplyAsync(() -> {
                try {
                    rules.setDefault(ListenRule.reject(RejectReason.OVERLOAD));
                    rules.set("x", ListenRule.accept());
                    rules.set("y", ListenRule.accept());
                    assertTrue(rules.remove("x"));
                    return rules.size();
                } catch (SrtException e) {
                    throw new RuntimeException(e);
                }
            }).get(1, TimeUnit.SECONDS);
            assertEquals(1, size);
            assertNull(outcome.get(), "accept ended before the edits answered");

            listener.cancelHandle().cancel();
            acceptor.join(10_000);
            Object got = outcome.get();
            assertTrue(got instanceof SrtException, "accept was woken, got: " + got);
            assertEquals(SrtException.Kind.CLOSED, ((SrtException) got).kind());
        }
    }

    @Test
    @Timeout(30)
    void closedListenerThrows() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        Listener listener = listen();
        ListenRules rules = listener.rules();
        listener.close();

        SrtException e = assertThrows(SrtException.class,
            () -> rules.set("a", ListenRule.accept()));
        assertEquals(SrtException.Kind.CLOSED, e.kind());
        assertEquals(SrtException.Kind.CLOSED,
            assertThrows(SrtException.class, rules::size).kind());
        assertThrows(IllegalStateException.class, listener::rules);
    }

    @Test
    @Timeout(30)
    void cancelledListenerThrows() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Listener listener = listen()) {
            ListenRules rules = listener.rules();
            rules.set("a", ListenRule.accept());
            listener.cancelHandle().cancel();

            assertEquals(SrtException.Kind.CLOSED,
                assertThrows(SrtException.class, rules::size).kind());
            assertEquals(SrtException.Kind.CLOSED,
                assertThrows(SrtException.class, () -> rules.remove("a")).kind());
            assertEquals(SrtException.Kind.CLOSED,
                assertThrows(SrtException.class, rules::clear).kind());
            assertEquals(SrtException.Kind.CLOSED,
                assertThrows(SrtException.class, () -> rules.set("b", ListenRule.accept())).kind());
            assertEquals(SrtException.Kind.CLOSED,
                assertThrows(SrtException.class,
                    () -> rules.setDefault(ListenRule.accept())).kind());
        }
    }

    @Test
    @Timeout(30)
    void validation() throws Exception {
        assertThrows(IllegalArgumentException.class,
            () -> ListenRule.reject(RejectReason.ofCode(10)));
        assertThrows(IllegalArgumentException.class, () -> RejectReason.userDefined(1999));
        assertEquals(2042, RejectReason.userDefined(2042).code());
        String shown = ListenRule.acceptWith("secretsecretsecret").toString();
        assertFalse(shown.contains("secretsecretsecret"), "passphrase leaked: " + shown);

        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Listener listener = listen()) {
            SrtException e = assertThrows(SrtException.class,
                () -> listener.rules().set("a", ListenRule.acceptWith("short")));
            assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
            SrtException nul = assertThrows(SrtException.class,
                () -> listener.rules().set("a\0b", ListenRule.accept()));
            assertEquals(SrtException.Kind.CONFIG_INVALID, nul.kind());
            assertFalse(listener.rules().remove("a\0b"));
        }
    }
}
