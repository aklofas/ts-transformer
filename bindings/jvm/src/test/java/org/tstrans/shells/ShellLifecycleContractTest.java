package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.stream.Stream;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;
import org.tstrans.shells.ShellPeers.Cleanup;
import org.tstrans.shells.ShellPeers.Parked;
import org.tstrans.shells.ShellPeers.Rx;
import org.tstrans.shells.ShellPeers.SenderPeer;
import org.tstrans.shells.ShellPeers.Tx;

/**
 * One lifecycle contract for every shell over every transport.
 *
 * <p>Each shell ({@code Sender}, {@code Receiver}, {@code MuxSender}, {@code DemuxReceiver}) is
 * opened from an {@code srt://}, {@code rtp://}, {@code udp://}, {@code tcp://} or
 * {@code rist://} URL against a live peer and honours the same rules whichever transport it
 * landed on:
 *
 * <ol>
 *   <li>{@code close()} twice throws nothing.
 *   <li>After {@code close()} a call throws {@link IllegalStateException}, the closed-handle
 *       contract of every handle in this binding.
 *   <li>A receive parked on a thread ends within 5 s of {@code cancelHandle().cancel()} from
 *       another thread, with the transport's own exception class (exactly, not a base class)
 *       and kind {@code CLOSED}. An rtp receiver then reports {@link EndReason#CANCELLED}; the other
 *       transports record no end reason and report {@code null}.
 *   <li>While a receive is parked, {@code isAlive()} and {@code cancelHandle()} each answer within
 *       1 s. {@code socketStats()} reads the counters held inside the shell, so it waits for the
 *       parked call by design; it answers within 1 s once the cancel has ended that call.
 *   <li>{@code srt} and {@code tcp} senders: once the peer is gone, further sends end in
 *       {@code BROKEN} or {@code CLOSED}. libsrt notices a closed peer only through its 5 s
 *       peer-idle timeout, so the srt window is 15 s and the tcp window 5 s.
 *   <li>{@code MuxSender} and {@code DemuxReceiver} repeat 1 to 3 over the same peers as
 *       {@code Sender} and {@code Receiver}.
 *   <li>A {@code Sender} fed bytes that hold no TS sync for more than the framing limit throws the
 *       transport's own exception class (exactly) with kind {@code INPUT_MALFORMED}, on every
 *       transport.
 *   <li>{@code srt} and {@code tcp} receivers: once the peer closes cleanly, the first
 *       {@code recvBytes()} throws the transport's class with kind {@code BROKEN} and every later
 *       one throws it with kind {@code END_OF_STREAM}; {@code endReason()} stays {@code null}. A
 *       datagram transport ({@code rtp}, {@code udp}, {@code rist}) cannot see a peer close, so it
 *       has no rule 8 case.
 * </ol>
 *
 * Case names read {@code {shell}-{scheme}-r{rule}}. A {@code rist} case skips only when librist
 * cannot bind an even loopback port.
 */
@Timeout(120)
class ShellLifecycleContractTest {
    private static final List<String> SCHEMES = List.of("srt", "rtp", "udp", "tcp", "rist");
    private static final List<String> SENDERS = List.of("Sender", "MuxSender");
    private static final List<String> RECEIVERS = List.of("Receiver", "DemuxReceiver");

    private static Stream<Arguments> cases(List<String> shells, List<String> schemes) {
        return shells.stream().flatMap(shell -> schemes.stream().map(s -> Arguments.of(shell, s)));
    }

    static Stream<Arguments> everyShell() {
        return Stream.concat(cases(SENDERS, SCHEMES), cases(RECEIVERS, SCHEMES));
    }

    static Stream<Arguments> receivers() { return cases(RECEIVERS, SCHEMES); }

    static Stream<Arguments> senders() { return cases(SENDERS, SCHEMES); }

    static Stream<Arguments> senderPeersThatNoticeADroppedPeer() { return cases(SENDERS, List.of("srt", "tcp")); }

    static Stream<Arguments> plainSenders() { return cases(List.of("Sender"), SCHEMES); }

    static Stream<Arguments> plainReceiversThatSeeAPeerClose() {
        return cases(List.of("Receiver"), List.of("srt", "tcp"));
    }

    private static boolean isSender(String shell) { return SENDERS.contains(shell); }

    @ParameterizedTest(name = "{0}-{1}-r1")
    @MethodSource("everyShell")
    void closeTwiceThrowsNothing(String shell, String scheme) throws Exception {
        try (Cleanup c = new Cleanup()) {
            if (isSender(shell)) {
                Tx tx = ShellPeers.senderWithPeer(shell, scheme, c).tx();
                tx.close();
                assertDoesNotThrow(tx::close);
                assertFalse(tx.isAlive());
            } else {
                Rx rx = ShellPeers.receiverWithPeer(shell, scheme, c);
                rx.close();
                assertDoesNotThrow(rx::close);
                assertFalse(rx.isAlive());
            }
        }
    }

    @ParameterizedTest(name = "{0}-{1}-r2")
    @MethodSource("everyShell")
    void useAfterCloseIsIllegalState(String shell, String scheme) throws Exception {
        try (Cleanup c = new Cleanup()) {
            if (isSender(shell)) {
                Tx tx = ShellPeers.senderWithPeer(shell, scheme, c).tx();
                tx.close();
                assertThrows(IllegalStateException.class, tx::sendOnce);
            } else {
                Rx rx = ShellPeers.receiverWithPeer(shell, scheme, c);
                rx.close();
                assertThrows(IllegalStateException.class, rx::recvOnce);
            }
        }
    }

    @ParameterizedTest(name = "{0}-{1}-r3")
    @MethodSource("receivers")
    void cancelEndsAParkedRecvWithClosed(String shell, String scheme) throws Exception {
        try (Cleanup c = new Cleanup()) {
            Rx rx = ShellPeers.receiverWithPeer(shell, scheme, c);
            CancelHandle cancel = rx.cancelHandle();
            try (Parked parked = new Parked(rx)) {
                cancel.cancel();
                Throwable ended = parked.awaitEnd(5_000);
                assertEquals(ShellPeers.errorClass(scheme), ended.getClass(), String.valueOf(ended));
                assertEquals("CLOSED", ShellPeers.kindName(ended));
            }
            // Only the rtp transport records an end reason; the others document
            // "the transport records no end reason" and answer null.
            if (scheme.equals("rtp")) {
                assertEquals(EndReason.CANCELLED, rx.endReason());
            } else {
                assertNull(rx.endReason(), scheme + " records no end reason");
            }
        }
    }

    @ParameterizedTest(name = "{0}-{1}-r4")
    @MethodSource("receivers")
    void accessorsDoNotWaitBehindAParkedRecv(String shell, String scheme) throws Exception {
        try (Cleanup c = new Cleanup()) {
            Rx rx = ShellPeers.receiverWithPeer(shell, scheme, c);
            CancelHandle cancel = rx.cancelHandle();
            try (Parked parked = new Parked(rx)) {
                try {
                    assertTrue(ShellPeers.within(1_000, "isAlive()", rx::isAlive));
                    assertNotNull(ShellPeers.within(1_000, "cancelHandle()", rx::cancelHandle));
                } finally {
                    cancel.cancel();
                }
                parked.awaitEnd(5_000);
            }
            assertNotNull(ShellPeers.within(1_000, "socketStats() after the cancel", rx::socketStats));
        }
    }

    @ParameterizedTest(name = "{0}-{1}-r5")
    @MethodSource("senderPeersThatNoticeADroppedPeer")
    void sendsEndBrokenOrClosedOnceThePeerIsGone(String shell, String scheme) throws Exception {
        long windowMs = scheme.equals("srt") ? 15_000 : 5_000;
        try (Cleanup c = new Cleanup()) {
            SenderPeer sp = ShellPeers.senderWithPeer(shell, scheme, c);
            sp.tx().sendOnce();
            sp.dropPeer().run();
            CancelHandle watchdog = sp.tx().cancelHandle();
            CompletableFuture<Throwable> failure = new CompletableFuture<>();
            Thread sender = new Thread(() -> {
                long end = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(windowMs);
                try {
                    while (System.nanoTime() < end) {
                        sp.tx().sendOnce();
                        Thread.sleep(20);
                    }
                    failure.complete(null);
                } catch (Throwable e) {
                    failure.complete(e);
                }
            }, "shell-send-loop");
            sender.setDaemon(true);
            sender.start();
            Throwable err;
            try {
                err = failure.get(windowMs + 10_000, TimeUnit.MILLISECONDS);
            } catch (java.util.concurrent.TimeoutException e) {
                watchdog.cancel();
                throw new AssertionError("a send hung after the peer went away");
            }
            assertNotNull(err, "sends kept succeeding for " + windowMs + " ms after the peer went away");
            assertEquals(ShellPeers.errorClass(scheme), err.getClass(), String.valueOf(err));
            String kind = ShellPeers.kindName(err);
            assertTrue(kind.equals("BROKEN") || kind.equals("CLOSED"), kind);
        }
    }

    @ParameterizedTest(name = "{0}-{1}-r7")
    @MethodSource("plainSenders")
    void bytesWithoutTsSyncThrowInputMalformed(String shell, String scheme) throws Exception {
        try (Cleanup c = new Cleanup()) {
            Tx tx = ShellPeers.senderWithPeer(shell, scheme, c).tx();
            Throwable err = assertThrows(ShellPeers.errorClass(scheme), () -> tx.sendBytes(ShellPeers.NOT_TS));
            assertEquals(ShellPeers.errorClass(scheme), err.getClass(), String.valueOf(err));
            assertEquals("INPUT_MALFORMED", ShellPeers.kindName(err));
        }
    }

    @ParameterizedTest(name = "{0}-{1}-r8")
    @MethodSource("plainReceiversThatSeeAPeerClose")
    void aCleanPeerCloseEndsInBrokenThenEndOfStream(String shell, String scheme) throws Exception {
        try (Cleanup c = new Cleanup()) {
            ShellPeers.ReceiverPeer rp = ShellPeers.receiverAndPeer(shell, scheme, c);
            Rx rx = rp.rx();
            CancelHandle watchdog = rx.cancelHandle();
            rp.closePeer().run();
            CompletableFuture<Throwable> first = new CompletableFuture<>();
            Thread receiver = new Thread(() -> {
                long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(15);
                try {
                    while (System.nanoTime() < end) {
                        rx.recvOnce();
                    }
                    first.complete(null);
                } catch (Throwable e) {
                    first.complete(e);
                }
            }, "shell-recv-loop");
            receiver.setDaemon(true);
            receiver.start();
            Throwable err;
            try {
                err = first.get(25, TimeUnit.SECONDS);
            } catch (java.util.concurrent.TimeoutException e) {
                watchdog.cancel();
                throw new AssertionError("a recv hung after the peer closed");
            }
            assertNotNull(err, "the receiver kept delivering for 15 s after the peer closed");
            assertEquals(ShellPeers.errorClass(scheme), err.getClass(), String.valueOf(err));
            assertEquals("BROKEN", ShellPeers.kindName(err));
            for (int i = 0; i < 2; i++) {
                Throwable later = assertThrows(ShellPeers.errorClass(scheme), rx::recvOnce);
                assertEquals(ShellPeers.errorClass(scheme), later.getClass(), String.valueOf(later));
                assertEquals("END_OF_STREAM", ShellPeers.kindName(later));
            }
            assertNull(rx.endReason(), scheme + " records no end reason");
        }
    }
}
