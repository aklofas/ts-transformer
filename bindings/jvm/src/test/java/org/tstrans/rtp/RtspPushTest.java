package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.scaledMs;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Base64;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.BooleanSupplier;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.tstrans.MuxException;
import org.tstrans.RtpException;
import org.tstrans.RtspException;
import org.tstrans.internal.PushProbe;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.Sender;

/**
 * The publish direction of the RTSP client: {@link RtspClient#push}, {@link
 * PushDescription}, {@link RtspPushSession} and {@link RtspPushStats}.
 *
 * <p>Two kinds of server. The library's own {@link RtspServer} with a publish mount takes
 * the pushes whose media must arrive (Linux-gated like the other tests that start one:
 * real sockets and a tokio runtime). A scripted one-connection server written here
 * ({@link ScriptedRecordServer}) answers ANNOUNCE / SETUP / RECORD itself, so a test
 * chooses its TEARDOWN status or makes it stop reading. Every wait for a cross-thread
 * effect has a deadline.
 */
class RtspPushTest {

    /** An H.264 SPS and PPS a decoder takes (a 320x240 Baseline stream). */
    private static final byte[] SPS = Base64.getDecoder().decode("Z/QADJGWgUH7ARAAAAMAEAAAAwHg8UKq");
    private static final byte[] PPS = Base64.getDecoder().decode("aM4PGSA=");

    /** Seven MPEG-TS null packets: the largest payload {@code sendTs} takes. */
    private static final byte[] NULL_BUNDLE = nullBundle();

    /** A KLV unit: the start of a universal key and one byte. */
    private static final byte[] KLV = {0x06, 0x0E, 0x2B, 0x34, 0x01};

    // ── Media reaches the library's server ──────────────────────────────────

    @Test
    @Timeout(30)
    void mp2tPushThroughMuxSenderReachesTheMount() throws Exception {
        assumeLinux();
        try (RtspServer server = startServer();
             PublishMount mount = server.addPublishMount("/pub");
             DemuxReceiver rx = mount.intoDemuxReceiver()) {
            RtspPushSession push = RtspClient.push(serverConfig(server), PushDescription.mp2t());
            assertTrue(mount.publisher().isPresent(), "RECORD returned but the mount has no publisher");
            try (MuxSender tx = push.intoMuxSender(roundtripConfig())) {
                // The session moved into the sender.
                assertThrows(IllegalStateException.class, () -> push.sendTs(NULL_BUNDLE));
                for (int i = 0; i < 24; i++) tx.sendVideo(syntheticH264Idr(), i * 3003L, true);
                DemuxEvent.Video video = null;
                long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(scaledMs(5_000));
                while (video == null && System.nanoTime() < deadline) {
                    DemuxEvent ev = rx.recvEvent();
                    if (ev instanceof DemuxEvent.Video v) video = v;
                }
                assertNotNull(video, "no video reached the mount");
                tx.finish();
            }
            assertTrue(waitFor(() -> mount.publisher().isEmpty()), "the finished sender kept the mount");
        }
    }

    @Test
    @Timeout(30)
    void h264KlvPushReachesTheMountWithSenderReportAlignment() throws Exception {
        assumeLinux();
        final int units = 30;
        try (RtspServer server = startServer();
             PublishMount mount = server.addPublishMount("/pub");
             RtspPushSession push = RtspClient.push(serverConfig(server),
                 PushDescription.h264(List.of(SPS, PPS), true))) {
            for (int i = 0; i < units; i++) {
                long pts = i * 3003L;
                push.sendVideo(accessUnit(i), pts);
                push.sendKlv(new byte[] {0x06, 0x0E, 0x2B, 0x34, (byte) i}, pts);
            }
            // The mount holds the last access unit until the next one starts.
            assertTrue(waitFor(() -> mount.stats().ausEmitted() >= units - 1
                    && mount.stats().klvUnitsEmitted() >= units - 1
                    && mount.stats().alignment() == ClockAlignment.SENDER_REPORT),
                () -> "the mount stopped at " + mount.stats());
            assertEquals(0L, mount.stats().malformedPackets());
            assertEquals(PublishShape.ELEMENTARY, mount.publisher().orElseThrow().shape());
            assertTrue(mount.publisher().orElseThrow().klv());
        }
    }

    @Test
    @Timeout(30)
    void h264FromKeyFrameDescribesTheStream() throws Exception {
        assumeLinux();
        PushDescription d = PushDescription.h264FromKeyFrame(accessUnit(0), false);
        try (RtspServer server = startServer();
             PublishMount mount = server.addPublishMount("/pub");
             RtspPushSession push = RtspClient.push(serverConfig(server), d)) {
            for (int i = 0; i < 5; i++) push.sendVideo(accessUnit(i), i * 3003L);
            assertTrue(waitFor(() -> mount.stats().ausEmitted() >= 4),
                () -> "the mount stopped at " + mount.stats());
            assertFalse(mount.publisher().orElseThrow().klv());
        }
    }

    @Test
    void keyFramesWithoutParameterSetsAreIllegalArgument() {
        assertThrows(IllegalArgumentException.class,
            () -> PushDescription.h264FromKeyFrame(syntheticH264Idr(), false));
        assertThrows(IllegalArgumentException.class,
            () -> PushDescription.h265FromKeyFrame(syntheticH264Idr(), true));
        assertThrows(IllegalArgumentException.class,
            () -> PushDescription.h264FromKeyFrame(new byte[0], false));
    }

    @Test
    @Timeout(30)
    void tryWithResourcesDrainsAndCloses() throws Exception {
        assumeLinux();
        final int bundles = 400;
        try (RtspServer server = startServer();
             PublishMount mount = server.addPublishMount("/pub")) {
            try (RtspPushSession push = RtspClient.push(serverConfig(server), PushDescription.mp2t())) {
                for (int i = 0; i < bundles; i++) push.sendTs(NULL_BUNDLE);
            }
            // close() wrote every queued packet before its TEARDOWN.
            assertTrue(waitFor(() -> mount.stats().rtpPacketsReceived() == bundles),
                () -> "the mount counted " + mount.stats().rtpPacketsReceived() + " of " + bundles);
            assertTrue(waitFor(() -> mount.publisher().isEmpty()), "the closed push kept the mount");
        }
    }

    @Test
    @Timeout(60)
    void aTeardownRacingAStatsReaderEndsCleanly() throws Exception {
        assumeLinux();
        final int bundles = 50;
        try (RtspServer server = startServer();
             PublishMount mount = server.addPublishMount("/pub")) {
            // Several rounds, so the TEARDOWN meets the reader inside stats() at least once.
            for (int round = 0; round < 10; round++) {
                long before = mount.stats().rtpPacketsReceived();
                RtspPushSession push = RtspClient.push(serverConfig(server), PushDescription.mp2t());
                AtomicReference<Throwable> readerError = new AtomicReference<>();
                AtomicLong reads = new AtomicLong();
                Thread reader = new Thread(() -> {
                    try {
                        while (true) {
                            push.stats();
                            reads.incrementAndGet();
                        }
                    } catch (IllegalStateException closed) {
                        // The session closed: the reader is done.
                    } catch (Throwable t) {
                        readerError.set(t);
                    }
                }, "stats-reader");
                reader.setDaemon(true);
                reader.start();
                for (int i = 0; i < bundles; i++) push.sendTs(NULL_BUNDLE);
                assertTrue(waitFor(() -> reads.get() > 0), "the reader never read");
                // A reader holding the session for a moment is not a call to cancel: the
                // TEARDOWN is accepted, so nothing is thrown.
                push.teardown();
                reader.join(scaledMs(5_000));
                assertFalse(reader.isAlive(), "the stats reader did not end");
                assertEquals(null, readerError.get());
                final int r = round;
                assertTrue(waitFor(() -> mount.stats().rtpPacketsReceived() - before == bundles),
                    () -> "round " + r + ": the mount counted "
                        + (mount.stats().rtpPacketsReceived() - before) + " of " + bundles);
                assertTrue(waitFor(() -> mount.publisher().isEmpty()), "the torn-down push kept the mount");
            }
        }
    }

    @Test
    @Timeout(30)
    void h265PushIsRefused415ByTheLibraryServer() throws Exception {
        assumeLinux();
        try (RtspServer server = startServer();
             PublishMount mount = server.addPublishMount("/pub")) {
            PushDescription d = PushDescription.h265(
                new byte[] {0x40, 0x01, 0x0C}, new byte[] {0x42, 0x01, 0x01}, new byte[] {0x44, 0x01, (byte) 0xC1},
                false);
            RtspException e = assertThrows(RtspException.class,
                () -> RtspClient.push(serverConfig(server), d));
            assertEquals(RtspException.Kind.PROTOCOL, e.kind());
            assertTrue(e.getMessage().contains("415"), e.getMessage());
            assertTrue(mount.publisher().isEmpty());
        }
    }

    // ── Errors and the session's lifecycle, against the scripted server ─────

    @Test
    @Timeout(30)
    void teardownReportsARefusedTeardown() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(501, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            push.sendTs(NULL_BUNDLE);
            RtspException e = assertThrows(RtspException.class, push::teardown);
            assertEquals(RtspException.Kind.PROTOCOL, e.kind());
            assertTrue(e.getMessage().contains("501"), e.getMessage());
            assertTrue(peer.awaitMethod("TEARDOWN"));
            // The media went out before the TEARDOWN.
            assertEquals(1L, peer.rtpFramesRead.get(), "the RTP frame before the TEARDOWN");
            // teardown() closed the session.
            assertThrows(IllegalStateException.class, () -> push.sendTs(NULL_BUNDLE));
            push.close();
        }
    }

    @Test
    @Timeout(30)
    void teardownAcceptedReturnsAndClosesTheSession() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            assertEquals(ScriptedRecordServer.SESSION, push.sessionId());
            assertTrue(push.isSessionAlive());
            push.teardown();
            assertThrows(IllegalStateException.class, push::teardown);
            push.close();
            assertEquals(List.of("ANNOUNCE", "SETUP", "RECORD", "TEARDOWN"), peer.methods);
        }
    }

    @Test
    @Timeout(30)
    void cancelFromASecondThreadReleasesAParkedSendAsClosed() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, true);
             RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t())) {
            Sending sending = Sending.start(push);
            sending.awaitParked();
            // The handle answers while the send holds the session.
            long asked = System.nanoTime();
            try (RtspCancelHandle cancel = push.cancelHandle()) {
                long handleMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - asked);
                assertTrue(handleMs < scaledMs(500), "cancelHandle() waited " + handleMs + " ms");
                assertTrue(sending.thread.isAlive(), "the send ended before the cancel");
                long t0 = System.nanoTime();
                cancel.cancel();
                assertTrue(sending.join(scaledMs(1_000)), "the parked send outlived the cancel by 1 s");
                long tookMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
                assertTrue(tookMs <= scaledMs(1_000), "the cancel took " + tookMs + " ms");
                RtpException e = assertInstanceOf(RtpException.class, sending.error.get());
                assertEquals(RtpException.Kind.CLOSED, e.kind());
                assertTrue(cancel.isCancelled());
            }
            RtpException again = assertThrows(RtpException.class, () -> push.sendTs(NULL_BUNDLE));
            assertEquals(RtpException.Kind.CLOSED, again.kind());
        }
    }

    @Test
    @Timeout(30)
    void closeDuringAParkedSendCancelsFirst() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, true)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            Sending sending = Sending.start(push);
            sending.awaitParked();
            long t0 = System.nanoTime();
            push.close();
            long tookMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            // The cancel releases the send at once; the TEARDOWN is bounded at
            // 500 ms and the client's thread at 2 s.
            assertTrue(tookMs < scaledMs(4_000), "close() took " + tookMs + " ms");
            assertTrue(sending.join(scaledMs(1_000)), "the parked send did not end");
            RtpException e = assertInstanceOf(RtpException.class, sending.error.get());
            assertEquals(RtpException.Kind.CLOSED, e.kind());
        }
    }

    @Test
    @Timeout(30)
    void closeDuringAPauseAwaitingItsReplyCancelsFirst() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            push.sendTs(NULL_BUNDLE);
            AtomicReference<Throwable> pauseError = new AtomicReference<>();
            Thread pausing = new Thread(() -> {
                try {
                    push.pause();
                } catch (Throwable t) {
                    pauseError.set(t);
                }
            }, "pausing");
            pausing.setDaemon(true);
            pausing.start();
            // The PAUSE is on the wire and its reply never comes: the call waits up to
            // its 10 s request timeout unless the close cancels it.
            assertTrue(peer.awaitMethod("PAUSE"), "the PAUSE was never written");
            long t0 = System.nanoTime();
            push.close();
            long tookMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertTrue(tookMs < scaledMs(4_000), "close() took " + tookMs + " ms");
            pausing.join(scaledMs(1_000));
            assertFalse(pausing.isAlive(), "the pause did not end");
            RtspException e = assertInstanceOf(RtspException.class, pauseError.get());
            assertEquals(RtspException.Kind.CLOSED, e.kind());
        }
    }

    @Test
    @Timeout(30)
    @EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
    void aTeardownWaitsForAQueryCallWithoutCancelling() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            for (int i = 0; i < 5; i++) push.sendTs(NULL_BUNDLE);
            Side holder = Side.start("query-holder", () -> PushProbe.nHoldAsReader(push.nativeHandleForTest()));
            try {
                assertTrue(waitFor(PushProbe::nReaderHolds), "the probe never held the session");
                Side tearing = Side.start("tearing-down", push::teardown);
                // The teardown has begun while the query holds the session; only then
                // does the query let go.
                awaitInNative(tearing.thread, "nTeardown");
                PushProbe.nRelease();
                // A query holding the session is not a call to cancel: the TEARDOWN is
                // accepted (a cancelled session's teardown() throws CLOSED).
                tearing.assertEndedNormally();
            } finally {
                PushProbe.nRelease();
            }
            holder.assertEndedNormally();
            assertTrue(peer.awaitMethod("TEARDOWN"));
            assertEquals(5L, peer.rtpFramesRead.get(), "the queued media was not all written");
        }
    }

    @Test
    @Timeout(30)
    @EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
    void aTeardownBehindAQueryDoesNotCancelForASendWaitingForTheSession() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            for (int i = 0; i < 3; i++) push.sendTs(NULL_BUNDLE);
            Side holder = Side.start("query-holder", () -> PushProbe.nHoldAsReader(push.nativeHandleForTest()));
            try {
                assertTrue(waitFor(PushProbe::nReaderHolds), "the probe never held the session");
                // A send that waits for the session behind the query: it does not hold it.
                Side sender = Side.start("waiting-sender", () -> push.sendTs(NULL_BUNDLE));
                awaitInNative(sender.thread, "nSendTs");
                Side tearing = Side.start("tearing-down", push::teardown);
                awaitInNative(tearing.thread, "nTeardown");
                PushProbe.nRelease();
                // Neither holder can park on the network, so nothing is cancelled: the
                // TEARDOWN is accepted, and the waiting send runs nothing.
                tearing.assertEndedNormally();
                RtpException e = assertInstanceOf(RtpException.class, sender.endedWith());
                assertEquals(RtpException.Kind.CLOSED, e.kind());
            } finally {
                PushProbe.nRelease();
            }
            holder.assertEndedNormally();
            assertTrue(peer.awaitMethod("TEARDOWN"));
            assertEquals(3L, peer.rtpFramesRead.get(), "the waiting send wrote a frame");
        }
    }

    @Test
    @Timeout(30)
    @EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
    void aBlockingCallThatGetsTheSessionAfterACloseBeganRunsNothing() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            push.sendTs(NULL_BUNDLE);
            assertTrue(waitFor(() -> peer.rtpFramesRead.get() == 1), "RTP frames read: " + peer.rtpFramesRead.get());
            // The flag a close sets before it takes the session; the session is free.
            PushProbe.nMarkClosing(push.nativeHandleForTest());
            RtpException send = assertThrows(RtpException.class, () -> push.sendTs(NULL_BUNDLE));
            assertEquals(RtpException.Kind.CLOSED, send.kind());
            RtspException pause = assertThrows(RtspException.class, push::pause);
            assertEquals(RtspException.Kind.CLOSED, pause.kind());
            RtspException resume = assertThrows(RtspException.class, push::resume);
            assertEquals(RtspException.Kind.CLOSED, resume.kind());
            // A query still answers.
            assertEquals(1L, push.stats().packetsSent());
            push.close();
            assertTrue(peer.awaitMethod("TEARDOWN"));
            assertEquals(1L, peer.rtpFramesRead.get(), "a call after the close began sent a frame");
            assertFalse(peer.methods.contains("PAUSE"), "a PAUSE was written: " + peer.methods);
            assertEquals(1L, peer.methods.stream().filter("RECORD"::equals).count(), "a RECORD was written again");
        }
    }

    @Test
    @Timeout(30)
    void callsAfterCloseThrowIllegalState() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t());
            push.close();
            push.close();
            assertThrows(IllegalStateException.class, () -> push.sendTs(NULL_BUNDLE));
            assertThrows(IllegalStateException.class, () -> push.sendVideo(syntheticH264Idr(), 0L));
            assertThrows(IllegalStateException.class, () -> push.sendKlv(KLV, 0L));
            assertThrows(IllegalStateException.class, push::pause);
            assertThrows(IllegalStateException.class, push::resume);
            assertThrows(IllegalStateException.class, push::teardown);
            assertThrows(IllegalStateException.class, push::cancelHandle);
            assertThrows(IllegalStateException.class, push::stats);
            assertThrows(IllegalStateException.class, push::sessionId);
            assertThrows(IllegalStateException.class, push::isSessionAlive);
            assertThrows(IllegalStateException.class, push::intoSender);
            assertThrows(IllegalStateException.class, () -> push.intoMuxSender(roundtripConfig()));
            assertTrue(peer.awaitMethod("TEARDOWN"));
        }
    }

    @Test
    @Timeout(30)
    void pauseAndResume() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t())) {
            push.sendTs(NULL_BUNDLE);
            push.pause();
            RtspException twice = assertThrows(RtspException.class, push::pause);
            assertEquals(RtspException.Kind.PROTOCOL, twice.kind());
            push.resume();
            push.sendTs(NULL_BUNDLE);
            assertTrue(peer.awaitMethod("RECORD", 2));
            assertTrue(peer.methods.contains("PAUSE"));
        }
    }

    @Test
    @Timeout(30)
    void intoMuxSenderOnAnElementaryPushIsIllegalArgumentAndKeepsTheSession() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession push = RtspClient.push(peer.config(),
                 PushDescription.h264(List.of(SPS, PPS), false))) {
            assertThrows(IllegalArgumentException.class, () -> push.intoMuxSender(roundtripConfig()));
            assertThrows(IllegalArgumentException.class, push::intoSender);
            push.sendVideo(accessUnit(0), 0L);
            assertTrue(push.isSessionAlive());
            assertEquals(ScriptedRecordServer.SESSION, push.sessionId());
        }
    }

    @Test
    @Timeout(30)
    void aRefusedProgramConfigKeepsTheSession() throws Exception {
        MuxerConfig refused = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1000, VideoCodec.H264)
            .build();
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t())) {
            MuxException e = assertThrows(MuxException.class, () -> push.intoMuxSender(refused));
            assertEquals(MuxException.Kind.CONFIG_INVALID, e.kind());
            push.sendTs(NULL_BUNDLE);
            assertTrue(waitFor(() -> peer.rtpFramesRead.get() == 1), "RTP frames read: " + peer.rtpFramesRead.get());
            try (Sender tx = push.intoSender()) {
                assertThrows(IllegalStateException.class, () -> push.sendTs(NULL_BUNDLE));
                tx.sendBytes(NULL_BUNDLE);
                // Read before the close: a sender's close() cancels the session first
                // and drops media still queued.
                assertTrue(waitFor(() -> peer.rtpFramesRead.get() == 2), "RTP frames read: " + peer.rtpFramesRead.get());
            }
            assertTrue(peer.awaitMethod("TEARDOWN"));
        }
    }

    @Test
    @Timeout(30)
    void wrongTrackAndEmptyUnitsAreIllegalArgument() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession h264 = RtspClient.push(peer.config(),
                 PushDescription.h264(List.of(SPS, PPS), false))) {
            assertThrows(IllegalArgumentException.class, () -> h264.sendKlv(KLV, 0L));
            assertThrows(IllegalArgumentException.class, () -> h264.sendTs(NULL_BUNDLE));
            assertThrows(IllegalArgumentException.class, () -> h264.sendVideo(new byte[0], 0L));
            assertThrows(IllegalArgumentException.class, () -> h264.sendVideo(new byte[] {0, 0, 0, 1}, 0L));
            h264.sendVideo(accessUnit(0), 0L);
            assertEquals(0L, h264.stats().framesDroppedInbound());
        }
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession klv = RtspClient.push(peer.config(),
                 PushDescription.h264(List.of(SPS, PPS), true))) {
            assertThrows(IllegalArgumentException.class, () -> klv.sendKlv(new byte[0], 0L));
            klv.sendKlv(KLV, 0L);
        }
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession mp2t = RtspClient.push(peer.config(), PushDescription.mp2t())) {
            assertThrows(IllegalArgumentException.class, () -> mp2t.sendVideo(accessUnit(0), 0L));
            assertThrows(IllegalArgumentException.class, () -> mp2t.sendKlv(KLV, 0L));
            RtpException tooLarge = assertThrows(RtpException.class,
                () -> mp2t.sendTs(new byte[NULL_BUNDLE.length + 188]));
            assertEquals(RtpException.Kind.TOO_LARGE, tooLarge.kind());
            RtpException notTs = assertThrows(RtpException.class, () -> mp2t.sendTs(new byte[188]));
            assertEquals(RtpException.Kind.BROKEN, notTs.kind());
            // Neither refusal ended the session.
            mp2t.sendTs(NULL_BUNDLE);
        }
    }

    @Test
    @Timeout(30)
    void transportUdpIsUnsupportedTransport() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspClientConfig udp = RtspClientConfig.builder(peer.url())
                .transportPref(TransportPref.UDP).keepalive(false).build();
            RtspException e = assertThrows(RtspException.class,
                () -> RtspClient.push(udp, PushDescription.mp2t()));
            assertEquals(RtspException.Kind.UNSUPPORTED_TRANSPORT, e.kind());
            assertFalse(peer.methods.contains("SETUP"), "a SETUP was written: " + peer.methods);
        }
    }

    @Test
    @Timeout(30)
    void rtspVersion2IsRefusedLocally() throws Exception {
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false)) {
            RtspClientConfig v2 = RtspClientConfig.builder(peer.url())
                .rtspVersion(RtspVersion.V2_0).keepalive(false).build();
            RtspException e = assertThrows(RtspException.class,
                () -> RtspClient.push(v2, PushDescription.mp2t()));
            assertEquals(RtspException.Kind.PROTOCOL, e.kind());
            assertFalse(peer.methods.contains("ANNOUNCE"), "an ANNOUNCE was written: " + peer.methods);
        }
    }

    @Test
    @Timeout(30)
    void statsCountsPacketsAndReports() throws Exception {
        final int bundles = 10;
        try (ScriptedRecordServer peer = new ScriptedRecordServer(200, false);
             RtspPushSession push = RtspClient.push(peer.config(), PushDescription.mp2t())) {
            RtspPushStats before = push.stats();
            assertEquals(0L, before.packetsSent());
            assertEquals(0L, before.senderReportsSent());
            for (int i = 0; i < bundles; i++) push.sendTs(NULL_BUNDLE);
            RtspPushStats s = push.stats();
            assertEquals(bundles, s.packetsSent());
            assertEquals(bundles * (12L + NULL_BUNDLE.length), s.bytesSent());
            assertTrue(s.senderReportsSent() >= 1, s.toString());
            // The scripted server sends no receiver report.
            assertEquals(0L, s.receiverReports());
            assertEquals(0, s.fractionLostQ8());
            assertEquals(0L, s.cumulativeLost());
            assertEquals(0L, s.framesDroppedInbound());
            assertEquals(0L, s.framesDroppedAtClose());
        }
    }

    @Test
    void descriptionsCopyTheirArguments() {
        byte[] sps = SPS.clone();
        PushDescription d = PushDescription.h264(List.of(sps, PPS), true);
        sps[0] = 0;
        assertArrayEquals(SPS, d.parameterSets()[0]);
        assertEquals(PushDescription.KIND_H264, d.kind());
        assertTrue(d.klv());
        assertThrows(NullPointerException.class, () -> PushDescription.h264(null, false));
        assertThrows(NullPointerException.class, () -> PushDescription.h264(List.of(SPS, null), false));
        assertThrows(NullPointerException.class, () -> PushDescription.h265(null, SPS, PPS, false));
        assertThrows(NullPointerException.class, () -> PushDescription.h264FromKeyFrame(null, false));
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    private static void assumeLinux() {
        assumeTrue(isLinux(), "RTSP publish mounts gated to Linux (real sockets + tokio runtime)");
    }

    private static RtspServer startServer() throws RtspException {
        return RtspServer.start(RtspServerConfig.builder()
            .bindAddr("127.0.0.1:0")
            .gracefulShutdownDrainMs(50)
            .build());
    }

    private static RtspClientConfig serverConfig(RtspServer server) {
        String addr = server.localAddr();
        String port = addr.substring(addr.lastIndexOf(':') + 1);
        return RtspClientConfig.builder("rtsp://127.0.0.1:" + port + "/pub").keepalive(false).build();
    }

    @FunctionalInterface
    private interface Call { void run() throws Exception; }

    /** A call on a daemon thread; the assertions join it with a deadline. */
    private static final class Side {
        final Thread thread;
        final AtomicReference<Throwable> error = new AtomicReference<>();

        private Side(String name, Call call) {
            thread = new Thread(() -> {
                try {
                    call.run();
                } catch (Throwable t) {
                    error.set(t);
                }
            }, name);
            thread.setDaemon(true);
        }

        static Side start(String name, Call call) {
            Side s = new Side(name, call);
            s.thread.start();
            return s;
        }

        /** Join (up to 5 s, scaled) and return what the call threw, or null. */
        Throwable endedWith() throws InterruptedException {
            thread.join(scaledMs(5_000));
            assertFalse(thread.isAlive(), thread.getName() + " did not end");
            return error.get();
        }

        void assertEndedNormally() throws InterruptedException {
            Throwable t = endedWith();
            if (t != null) throw new AssertionError(thread.getName() + " threw", t);
        }
    }

    /** Wait (up to 5 s, scaled) until {@code t}'s top frame is the native {@code method}. */
    private static void awaitInNative(Thread t, String method) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(scaledMs(5_000));
        while (System.nanoTime() < deadline) {
            StackTraceElement[] st = t.getStackTrace();
            if (st.length > 0 && st[0].isNativeMethod() && st[0].getMethodName().equals(method)) return;
            if (!t.isAlive()) fail(t.getName() + " ended before it reached " + method);
            Thread.sleep(2);
        }
        fail(t.getName() + " never reached " + method);
    }

    /** Poll {@code cond} every 20 ms for up to 5 s (scaled). */
    private static boolean waitFor(BooleanSupplier cond) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(scaledMs(5_000));
        while (System.nanoTime() < deadline) {
            if (cond.getAsBoolean()) return true;
            Thread.sleep(20);
        }
        return cond.getAsBoolean();
    }

    /**
     * Access unit {@code i}: the first a key frame behind its SPS and PPS, the rest P
     * slices; each slice is large enough to be fragmented.
     */
    private static byte[] accessUnit(int i) {
        ByteArrayOutputStream au = new ByteArrayOutputStream();
        byte[] slice = new byte[3_001];
        slice[0] = (byte) (i == 0 ? 0x65 : 0x41);
        for (int k = 1; k < slice.length; k++) slice[k] = (byte) ((k % 250) + 1);
        byte[][] nals = i == 0 ? new byte[][] {SPS, PPS, slice} : new byte[][] {slice};
        for (byte[] nal : nals) {
            au.writeBytes(new byte[] {0, 0, 0, 1});
            au.writeBytes(nal);
        }
        return au.toByteArray();
    }

    private static byte[] nullBundle() {
        byte[] b = new byte[7 * 188];
        for (int p = 0; p < 7; p++) {
            int o = p * 188;
            b[o] = 0x47; b[o + 1] = 0x1F; b[o + 2] = (byte) 0xFF; b[o + 3] = 0x10;
            for (int i = 4; i < 188; i++) b[o + i] = (byte) 0xFF;
        }
        return b;
    }

    /** A thread that sends {@link #NULL_BUNDLE} until a send throws. */
    private static final class Sending {
        final AtomicLong sent = new AtomicLong();
        final AtomicReference<Throwable> error = new AtomicReference<>();
        Thread thread;

        static Sending start(RtspPushSession push) {
            Sending s = new Sending();
            s.thread = new Thread(() -> {
                try {
                    while (true) {
                        push.sendTs(NULL_BUNDLE);
                        s.sent.incrementAndGet();
                    }
                } catch (Throwable t) {
                    s.error.set(t);
                }
            }, "push-sender");
            s.thread.setDaemon(true);
            s.thread.start();
            return s;
        }

        /**
         * Wait until a send is parked: no send has returned for 300 ms and the thread is
         * inside the native send. Fails after 4 s, before the client's 5 s write stall.
         */
        void awaitParked() throws InterruptedException {
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(4);
            long last = -1;
            long stillSince = System.nanoTime();
            while (System.nanoTime() < deadline) {
                if (!thread.isAlive()) fail("the sender ended before it parked: " + error.get());
                long now = sent.get();
                if (now != last) {
                    last = now;
                    stillSince = System.nanoTime();
                } else if (System.nanoTime() - stillSince >= TimeUnit.MILLISECONDS.toNanos(300)) {
                    StackTraceElement[] st = thread.getStackTrace();
                    if (st.length > 0 && st[0].isNativeMethod() && st[0].getMethodName().equals("nSendTs")) {
                        return;
                    }
                }
                Thread.sleep(10);
            }
            fail("the send never parked; " + sent.get() + " sends returned");
        }

        boolean join(long ms) throws InterruptedException {
            thread.join(ms);
            return !thread.isAlive();
        }
    }

    /**
     * A one-connection RTSP server that takes a publisher: ANNOUNCE, SETUP (echoing the
     * client's interleaved transport) and RECORD get {@code 200}, TEARDOWN the status the
     * test chose. It reads and counts the client's interleaved frames, or, when asked,
     * stops reading once it has answered RECORD so the client's sends back up.
     */
    static final class ScriptedRecordServer implements AutoCloseable {
        static final String SESSION = "5EC0DE01";

        private final ServerSocket listener;
        private final Thread thread;
        private final int teardownStatus;
        private final boolean stopReadingAfterRecord;
        private final boolean answerPause;
        private final CountDownLatch release = new CountDownLatch(1);
        private volatile Socket conn;
        /** The methods of the requests read, in order. */
        final List<String> methods = new CopyOnWriteArrayList<>();
        /** Interleaved frames read. */
        final AtomicLong framesRead = new AtomicLong();
        /** Interleaved frames read on an even (RTP) channel. */
        final AtomicLong rtpFramesRead = new AtomicLong();

        ScriptedRecordServer(int teardownStatus, boolean stopReadingAfterRecord) throws IOException {
            this(teardownStatus, stopReadingAfterRecord, true);
        }

        /** As above; with {@code answerPause} false a PAUSE is read and never answered. */
        ScriptedRecordServer(int teardownStatus, boolean stopReadingAfterRecord, boolean answerPause)
                throws IOException {
            this.teardownStatus = teardownStatus;
            this.stopReadingAfterRecord = stopReadingAfterRecord;
            this.answerPause = answerPause;
            this.listener = new ServerSocket();
            // A small receive buffer: a server that stops reading backs the client up sooner.
            listener.setReceiveBufferSize(4096);
            listener.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 1);
            this.thread = new Thread(this::serve, "scripted-record-server");
            thread.setDaemon(true);
            thread.start();
        }

        String url() {
            return "rtsp://127.0.0.1:" + listener.getLocalPort() + "/pub";
        }

        RtspClientConfig config() {
            return RtspClientConfig.builder(url()).keepalive(false).build();
        }

        /** Wait up to 5 s (scaled) until {@code method} has been read. */
        boolean awaitMethod(String method) throws InterruptedException {
            return awaitMethod(method, 1);
        }

        /** Wait up to 5 s (scaled) until {@code method} has been read {@code count} times. */
        boolean awaitMethod(String method, int count) throws InterruptedException {
            return waitFor(() -> methods.stream().filter(method::equals).count() >= count);
        }

        @Override public void close() throws Exception {
            release.countDown();
            listener.close();
            Socket s = conn;
            if (s != null) s.close();
            thread.join(5_000);
            if (thread.isAlive()) throw new AssertionError("the scripted server's thread did not end");
        }

        private void serve() {
            try (Socket sock = listener.accept()) {
                conn = sock;
                sock.setSoTimeout(30_000);
                DataInputStream in = new DataInputStream(sock.getInputStream());
                OutputStream out = sock.getOutputStream();
                while (true) {
                    int first = in.read();
                    if (first == -1) return;
                    if (first == '$') {
                        int channel = in.readUnsignedByte();
                        in.skipNBytes(in.readUnsignedShort());
                        framesRead.incrementAndGet();
                        if (channel % 2 == 0) rtpFramesRead.incrementAndGet();
                        continue;
                    }
                    String req = (char) first + readHead(in);
                    int length = contentLength(req);
                    if (length > 0) in.skipNBytes(length);
                    String method = req.split(" ", 2)[0];
                    methods.add(method);
                    int status = 200;
                    String extra = "";
                    switch (method) {
                        case "SETUP" -> extra = "Session: " + SESSION + ";timeout=60\r\nTransport: "
                            + RawRtspPeer.header(req, "Transport") + "\r\n";
                        case "RECORD", "PAUSE" -> extra = "Session: " + SESSION + "\r\n";
                        case "TEARDOWN" -> status = teardownStatus;
                        default -> { }
                    }
                    if (method.equals("PAUSE") && !answerPause) continue;
                    String cseq = RawRtspPeer.header(req, "CSeq");
                    out.write(("RTSP/1.0 " + status + " " + (status == 200 ? "OK" : "Refused")
                        + "\r\nCSeq: " + cseq + "\r\n" + extra + "\r\n").getBytes(StandardCharsets.US_ASCII));
                    out.flush();
                    if (method.equals("RECORD") && stopReadingAfterRecord) {
                        release.await();
                        return;
                    }
                }
            } catch (Exception ignored) {
                // The client closed the connection, or close() closed the sockets.
            }
        }

        /** The rest of a request's head, through CRLFCRLF. */
        private static String readHead(InputStream in) throws IOException {
            StringBuilder sb = new StringBuilder();
            int b;
            while ((b = in.read()) != -1) {
                sb.append((char) b);
                if (sb.length() >= 4 && sb.substring(sb.length() - 4).equals("\r\n\r\n")) break;
            }
            return sb.toString();
        }

        private static int contentLength(String head) {
            String v = RawRtspPeer.header(head, "Content-Length");
            return v.isEmpty() ? 0 : Integer.parseInt(v);
        }
    }
}
