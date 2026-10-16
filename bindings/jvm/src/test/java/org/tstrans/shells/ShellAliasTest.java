package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.DatagramSocket;
import java.net.InetAddress;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.SrtException;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.shells.ShellPeers.Cleanup;
import org.tstrans.shells.ShellPeers.Pending;

/**
 * Pins every 0.8 spelling that still exists as a deprecated subclass: each
 * {@code fromUrl} overload is called once against a live peer and must return an
 * object that is both the deprecated class and its base. The suite names no
 * deprecated class anywhere else. This file is removed together with the
 * aliases in 0.10.0.
 */
@SuppressWarnings("removal")
@Timeout(60)
class ShellAliasTest {
    private static String srtListen(int port) { return "srt://:" + port + "?mode=listener"; }

    private static String srtCall(int port) { return "srt://127.0.0.1:" + port + "?mode=caller"; }

    /** The listener side of an srt caller test: a shells.Receiver on a daemon thread. */
    private static Pending<Receiver> listener(int port) {
        return ShellPeers.openInBackground(() -> Receiver.open(srtListen(port)));
    }

    @Test
    void srtSenderFromUrl() throws Exception {
        int port = TestSupport.freeUdpPort();
        try (Cleanup c = new Cleanup()) {
            Pending<Receiver> peer = listener(port);
            org.tstrans.srt.Sender s = c.add(ShellPeers.retryCaller(() -> org.tstrans.srt.Sender.fromUrl(srtCall(port))));
            c.add(peer.await());
            assertTrue((Object) s instanceof Sender);
            assertTrue((Object) s instanceof org.tstrans.srt.Sender);
        }
    }

    @Test
    void srtReceiverFromUrl() throws Exception {
        int port = TestSupport.freeUdpPort();
        try (Cleanup c = new Cleanup()) {
            Pending<org.tstrans.srt.Receiver> rx = ShellPeers.openInBackground(
                () -> org.tstrans.srt.Receiver.fromUrl(srtListen(port)));
            c.add(ShellPeers.retryCaller(() -> Sender.open(srtCall(port))));
            org.tstrans.srt.Receiver r = c.add(rx.await());
            assertTrue((Object) r instanceof Receiver);
            assertTrue((Object) r instanceof org.tstrans.srt.Receiver);
        }
    }

    @Test
    void srtMuxSenderFromUrl() throws Exception {
        int port = TestSupport.freeUdpPort();
        try (Cleanup c = new Cleanup()) {
            Pending<Receiver> peer = listener(port);
            org.tstrans.srt.MuxSender m = c.add(ShellPeers.retryCaller(
                () -> org.tstrans.srt.MuxSender.fromUrl(srtCall(port), TestSupport.roundtripConfig())));
            c.add(peer.await());
            assertTrue((Object) m instanceof MuxSender);
            assertTrue((Object) m instanceof org.tstrans.srt.MuxSender);
        }
    }

    private static void srtDemuxReceiver(boolean withConfig) throws Exception {
        int port = TestSupport.freeUdpPort();
        try (Cleanup c = new Cleanup()) {
            Pending<org.tstrans.srt.DemuxReceiver> rx = ShellPeers.openInBackground(() -> withConfig
                ? org.tstrans.srt.DemuxReceiver.fromUrl(srtListen(port), DemuxerConfig.builder().build())
                : org.tstrans.srt.DemuxReceiver.fromUrl(srtListen(port)));
            c.add(ShellPeers.retryCaller(() -> Sender.open(srtCall(port))));
            org.tstrans.srt.DemuxReceiver d = c.add(rx.await());
            assertTrue((Object) d instanceof DemuxReceiver);
            assertTrue((Object) d instanceof org.tstrans.srt.DemuxReceiver);
        }
    }

    @Test
    void srtDemuxReceiverFromUrl() throws Exception { srtDemuxReceiver(false); }

    @Test
    void srtDemuxReceiverFromUrlWithConfig() throws Exception { srtDemuxReceiver(true); }

    // ---- rtp ---------------------------------------------------------------------------------

    private static String rtpUrl(DatagramSocket sink) { return "rtp://127.0.0.1:" + sink.getLocalPort(); }

    private static DatagramSocket sink(Cleanup c) throws Exception {
        return c.add(new DatagramSocket(0, InetAddress.getLoopbackAddress()));
    }

    @Test
    void rtpSenderFromUrl() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.Sender s = c.add(org.tstrans.rtp.Sender.fromUrl(rtpUrl(sink(c))));
            assertTrue((Object) s instanceof org.tstrans.rtp.Transport);
            assertTrue((Object) s instanceof org.tstrans.rtp.Sender);
        }
    }

    @Test
    void rtpSenderFromUrlWithPktSizeAndSsrc() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.Sender s = c.add(org.tstrans.rtp.Sender.fromUrl(rtpUrl(sink(c)), 1316, 42L));
            assertTrue((Object) s instanceof org.tstrans.rtp.Transport);
            assertTrue((Object) s instanceof org.tstrans.rtp.Sender);
        }
    }

    @Test
    void rtpReceiverFromUrl() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.Receiver r = c.add(
                org.tstrans.rtp.Receiver.fromUrl("rtp://127.0.0.1:" + TestSupport.freeUdpPort()));
            assertTrue((Object) r instanceof org.tstrans.rtp.RecvTransport);
            assertTrue((Object) r instanceof org.tstrans.rtp.Receiver);
        }
    }

    @Test
    void rtpMuxSenderFromUrl() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.MuxSender m = c.add(
                org.tstrans.rtp.MuxSender.fromUrl(rtpUrl(sink(c)), TestSupport.roundtripConfig()));
            assertTrue((Object) m instanceof MuxSender);
            assertTrue((Object) m instanceof org.tstrans.rtp.MuxSender);
        }
    }

    @Test
    void rtpMuxSenderFromUrlWithPktSize() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.MuxSender m = c.add(
                org.tstrans.rtp.MuxSender.fromUrl(rtpUrl(sink(c)), TestSupport.roundtripConfig(), 1316));
            assertTrue((Object) m instanceof MuxSender);
            assertTrue((Object) m instanceof org.tstrans.rtp.MuxSender);
        }
    }

    @Test
    void rtpDemuxReceiverFromUrl() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.DemuxReceiver d = c.add(
                org.tstrans.rtp.DemuxReceiver.fromUrl("rtp://127.0.0.1:" + TestSupport.freeUdpPort()));
            assertTrue((Object) d instanceof DemuxReceiver);
            assertTrue((Object) d instanceof org.tstrans.rtp.DemuxReceiver);
        }
    }

    @Test
    void rtpDemuxReceiverFromUrlWithConfig() throws Exception {
        try (Cleanup c = new Cleanup()) {
            org.tstrans.rtp.DemuxReceiver d = c.add(org.tstrans.rtp.DemuxReceiver.fromUrl(
                "rtp://127.0.0.1:" + TestSupport.freeUdpPort(), DemuxerConfig.builder().build()));
            assertTrue((Object) d instanceof DemuxReceiver);
            assertTrue((Object) d instanceof org.tstrans.rtp.DemuxReceiver);
        }
    }

    // ---- the 0.8 scheme check ---------------------------------------------------------------

    @Test
    void srtFactoryRejectsAnotherScheme() {
        SrtException e = assertThrows(SrtException.class,
            () -> org.tstrans.srt.Sender.fromUrl("tcp://127.0.0.1:1"));
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
        assertTrue(e.getMessage().contains("requires an srt:// URL"), e.getMessage());
    }

    @Test
    void rtpFactoryRejectsAnotherScheme() {
        RtpException e = assertThrows(RtpException.class,
            () -> org.tstrans.rtp.MuxSender.fromUrl(
                "udp://127.0.0.1:" + TestSupport.freeUdpPort(), TestSupport.roundtripConfig()));
        assertEquals(RtpException.Kind.CONFIG_INVALID, e.kind());
        assertTrue(e.getMessage().contains("requires an rtp:// URL"), e.getMessage());
    }

    // ---- the markers -------------------------------------------------------------------------

    @Test
    void everyAliasIsDeprecatedForRemoval() {
        for (Class<?> c : new Class<?>[] {
                org.tstrans.srt.Sender.class, org.tstrans.srt.Receiver.class,
                org.tstrans.srt.MuxSender.class, org.tstrans.srt.DemuxReceiver.class,
                org.tstrans.rtp.MuxSender.class, org.tstrans.rtp.DemuxReceiver.class,
                org.tstrans.rtp.Sender.class, org.tstrans.rtp.Receiver.class}) {
            Deprecated d = c.getAnnotation(Deprecated.class);
            assertNotNull(d, c.getName());
            assertTrue(d.forRemoval(), c.getName());
            assertEquals("0.9.0", d.since(), c.getName());
        }
    }
}
