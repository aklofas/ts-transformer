package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.lang.reflect.Method;
import java.net.Inet4Address;
import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.tstrans.SrtException;
import org.tstrans.internal.NativeAddr;

/**
 * Tests for {@link Builder}, {@link Socket} and {@link Listener}. Most complete without a
 * live SRT peer: they hit config-validation or option-narrowing paths that return before any
 * network call. The address tests and the peer-idle test connect on loopback.
 */
class SrtLowLevelTest {

    /**
     * Builder.rendezvous().connect() must throw CONFIG_INVALID — rendezvous is
     * not yet supported by tst-srt. The rejection happens in nConnect before
     * any URL parse or network call.
     */
    @Test
    void rendezvousConnectRejected() {
        var b = new Builder("srt://127.0.0.1:9000").rendezvous();
        var e = assertThrows(SrtException.class, b::connect);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * Builder.rendezvous().listen() must also throw CONFIG_INVALID for the same
     * reason — rendezvous is not a valid listener mode.
     */
    @Test
    void rendezvousListenRejected() {
        var b = new Builder("srt://127.0.0.1:9000").rendezvous();
        var e = assertThrows(SrtException.class, b::listen);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * All chainable setters return the same {@code Builder} instance and store
     * their fields without making any native call. The native call only happens
     * at {@link Builder#connect()} / {@link Builder#listen()}.
     */
    @Test
    void builderSettersChain() {
        var b = new Builder("srt://127.0.0.1:9000")
            .caller()
            .latencyMs(120)
            .streamId("publish")
            .passphrase("hunter2hunter2")
            .congestion("live")
            .connectTimeoutMs(5000)
            .recvTimeoutMs(3000)
            .sendTimeoutMs(3000)
            .peerLatencyMs(80)
            .recvLatencyMs(80)
            .maxBandwidthBps(10_000_000L)
            .mss(1316)
            .payloadSize(1316);
        // Pure config holder — no native call yet; just verify the object exists.
        assertNotNull(b);
    }

    /**
     * mss(70000) exceeds u16::MAX (65535). The Rust side runs checked_u16
     * BEFORE any socket creation or network operation — the exception is an
     * IllegalArgumentException thrown from nConnect (not an SrtException).
     */
    @Test
    void mssOutOfRangeRejected() {
        var b = new Builder("srt://127.0.0.1:9000").caller().mss(70000);
        assertThrows(IllegalArgumentException.class, b::connect);
    }

    /**
     * payloadSize(70000) also exceeds u16::MAX — same narrowing path as mss.
     */
    @Test
    void payloadSizeOutOfRangeRejected() {
        var b = new Builder("srt://127.0.0.1:9000").caller().payloadSize(70000);
        assertThrows(IllegalArgumentException.class, b::connect);
    }

    /**
     * Negative millisecond/bandwidth knobs are rejected at the Java setter
     * boundary (these map to unsigned Rust quantities). The setter throws
     * IllegalArgumentException eagerly — before any finalizer or network call.
     */
    @Test
    void negativeTimeoutRejected() {
        assertThrows(IllegalArgumentException.class,
            () -> new Builder("srt://127.0.0.1:9000").connectTimeoutMs(-1));
    }

    /**
     * HostPort is a plain Java record; verify field access works correctly.
     */
    @Test
    @SuppressWarnings("removal") // the deprecated record keeps working until its removal
    void hostPortRecord() {
        var hp = new HostPort("192.168.1.1", 9000);
        assertEquals("192.168.1.1", hp.host());
        assertEquals(9000, hp.port());
    }

    /**
     * Builder.caller().listen() must throw CONFIG_INVALID because the URL
     * defaults to mode=caller but nListen requires listener mode.
     */
    @Test
    void callerModeOnListenRejected() {
        var b = new Builder("srt://127.0.0.1:9000").caller();
        var e = assertThrows(SrtException.class, b::listen);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /**
     * Builder.listener().connect() must throw CONFIG_INVALID — listener mode
     * is incompatible with the connect finalizer.
     */
    @Test
    void listenerModeOnConnectRejected() {
        // ?mode=listener in the URL + explicit .listener() on the builder.
        // nConnect checks mode ordinal == 2 (LISTENER) and rejects immediately.
        var b = new Builder("srt://127.0.0.1:9000?mode=listener").listener();
        var e = assertThrows(SrtException.class, b::connect);
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /** listener() supplies mode=listener when the URL has none, including an empty host. */
    @Test
    void listenerModeAddsTheUrlMode() throws Exception {
        try (Listener l = new Builder("srt://127.0.0.1:0").listener().listen()) {
            assertTrue(l.localAddr().getPort() > 0);
        }
        try (Listener l = new Builder("srt://:0").listener().listen()) {
            assertTrue(l.localAddr().getPort() > 0);
        }
    }

    /** listener() against a URL that says mode=caller is a contradiction. */
    @Test
    void listenerModeAgainstACallerUrlIsRefused() {
        var e = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0?mode=caller").listener().listen());
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /** Without listener(), the URL must still carry mode=listener. */
    @Test
    void urlChoiceStillRequiresTheUrlMode() {
        var e = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0").listen());
        assertEquals(SrtException.Kind.CONFIG_INVALID, e.kind());
    }

    /** A zero peer-idle timeout is refused when the socket is configured. */
    @Test
    void zeroPeerIdleTimeoutRefusedOnConnectAndListen() {
        var c = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:9000").caller().peerIdleTimeoutMs(0).connect());
        assertEquals(SrtException.Kind.CONFIG_INVALID, c.kind());
        var l = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0?mode=listener").listener().peerIdleTimeoutMs(0).listen());
        assertEquals(SrtException.Kind.CONFIG_INVALID, l.kind());
    }

    /** libsrt multiplies the millisecond value by 1000 into an int, so 2147484 ms is refused. */
    @Test
    void peerIdleTimeoutAboveCeilingRefusedOnConnectAndListen() {
        var c = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:9000").caller().peerIdleTimeoutMs(2_147_484).connect());
        assertEquals(SrtException.Kind.CONFIG_INVALID, c.kind());
        var l = assertThrows(SrtException.class,
            () -> new Builder("srt://127.0.0.1:0?mode=listener").listener().peerIdleTimeoutMs(2_147_484).listen());
        assertEquals(SrtException.Kind.CONFIG_INVALID, l.kind());
    }

    @Test
    void negativePeerIdleTimeoutRejected() {
        assertThrows(IllegalArgumentException.class,
            () -> new Builder("srt://127.0.0.1:9000").peerIdleTimeoutMs(-1));
    }

    /**
     * A positive peer-idle timeout is applied on both ends of a loopback connection. The value
     * itself is pinned by the Rust readback tests (peer_idle_timeout.rs); this proves the knob
     * crosses the binding without a config error.
     */
    @Test
    void positivePeerIdleTimeoutAcceptedOnLoopback() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");
        try (var lst = new Builder("srt://127.0.0.1:0?mode=listener").listener()
                .peerIdleTimeoutMs(8000).listen()) {
            int port = lst.localAddr().getPort();
            try (var sock = new Builder("srt://127.0.0.1:" + port).caller()
                    .peerIdleTimeoutMs(8000).connect();
                 var accepted = lst.accept(5000)) {
                assertNotNull(accepted);
            }
        }
    }

    // ----------------------------------------------------------------------------------------
    // Addresses: InetSocketAddress built from the native bytes
    // ----------------------------------------------------------------------------------------

    private static final InetAddress LOOPBACK_V4 = loopbackV4();

    private static InetAddress loopbackV4() {
        try {
            return InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        } catch (java.net.UnknownHostException e) {
            throw new AssertionError(e);
        }
    }

    /** The listener's and both sockets' local addresses, before and after close. */
    @Test
    void localAddrIsAnInetSocketAddress() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");
        Listener lst = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen();
        Socket caller;
        Socket accepted;
        try {
            InetSocketAddress at = lst.localAddr();
            assertFalse(at.isUnresolved());
            assertInstanceOf(Inet4Address.class, at.getAddress());
            assertEquals(LOOPBACK_V4, at.getAddress());
            assertTrue(at.getPort() > 0);
            caller = new Builder("srt://127.0.0.1:" + at.getPort()).caller().connect();
            try {
                accepted = lst.accept(5000);
                assertNotNull(accepted);
                try {
                    InetSocketAddress mine = caller.localAddr();
                    assertFalse(mine.isUnresolved());
                    assertTrue(mine.getPort() > 0);
                    assertEquals(at, accepted.localAddr());
                } finally {
                    accepted.close();
                }
            } finally {
                caller.close();
            }
        } finally {
            lst.close();
        }
        assertThrows(IllegalStateException.class, lst::localAddr);
        assertThrows(IllegalStateException.class, caller::localAddr);
        assertThrows(IllegalStateException.class, accepted::peerAddr);
    }

    /** Each end's peer is the other end's local address. */
    @Test
    void peerAddrMatchesTheCallersLocalAddr() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");
        try (Listener lst = new Builder("srt://127.0.0.1:0?mode=listener").listener().listen()) {
            InetSocketAddress at = lst.localAddr();
            try (Socket caller = new Builder("srt://127.0.0.1:" + at.getPort()).caller().connect();
                 Socket accepted = lst.accept(5000)) {
                assertNotNull(accepted);
                InetSocketAddress peer = accepted.peerAddr();
                assertEquals(LOOPBACK_V4, peer.getAddress());
                assertEquals(caller.localAddr().getPort(), peer.getPort());
                assertEquals(at, caller.peerAddr());
            }
        }
    }

    /** An IPv6 listener and its sockets report an Inet6Address. */
    @Test
    void anIpv6LoopbackAddressIsAnInet6Address() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux (same as #![cfg(target_os = \"linux\")] in Rust)");
        try (Listener lst = new Builder("srt://[::1]:0?mode=listener").listener().listen()) {
            InetSocketAddress at = lst.localAddr();
            Inet6Address ip = assertInstanceOf(Inet6Address.class, at.getAddress());
            assertTrue(ip.isLoopbackAddress());
            assertEquals(0, ip.getScopeId());
            try (Socket caller = new Builder("srt://[::1]:" + at.getPort()).caller().connect();
                 Socket accepted = lst.accept(5000)) {
                assertNotNull(accepted);
                assertEquals(at, caller.peerAddr());
                assertEquals(caller.localAddr().getPort(), accepted.peerAddr().getPort());
                assertInstanceOf(Inet6Address.class, accepted.peerAddr().getAddress());
            }
        }
    }

    /**
     * No link-local SRT peer exists on loopback, so the scope id and the IPv4-mapped rule are
     * checked on the byte layout the SRT getters return (jutil's address layout, pinned on
     * the Rust side by {@code socket_addr_array_carries_the_scope_id} and
     * {@code an_ipv4_mapped_address_goes_out_as_ipv4}): a scope id survives, and a mapped peer
     * arrives in its 4-byte form, an {@link Inet4Address}, as the shells' {@code peerAddr()} reports one.
     */
    @Test
    void aScopeIdAndAnIpv4MappedPeerAsTheGettersSendThem() throws Exception {
        byte[] scoped = new byte[22];
        scoped[0] = (byte) 0xfe;
        scoped[1] = (byte) 0x80;
        scoped[15] = 1;
        scoped[16] = (byte) (9000 >>> 8);
        scoped[17] = (byte) 9000;
        scoped[21] = 2;
        InetSocketAddress s = NativeAddr.decode(scoped);
        Inet6Address ip = assertInstanceOf(Inet6Address.class, s.getAddress());
        assertEquals(2, ip.getScopeId());
        assertTrue(ip.isLinkLocalAddress());
        assertEquals(9000, s.getPort());

        InetSocketAddress m = NativeAddr.decode(new byte[] {127, 0, 0, 1, (byte) (9000 >>> 8), (byte) 9000});
        assertInstanceOf(Inet4Address.class, m.getAddress());
        assertEquals(new InetSocketAddress(LOOPBACK_V4, 9000), m);
    }

    /** HostPort is deprecated for removal, and no SRT method returns it any more. */
    @Test
    @SuppressWarnings("removal") // names the deprecated record to check its annotation
    void hostPortIsDeprecatedForRemoval() throws Exception {
        Deprecated d = HostPort.class.getAnnotation(Deprecated.class);
        assertNotNull(d, "HostPort is not @Deprecated");
        assertTrue(d.forRemoval());
        assertEquals("0.11.0", d.since());
        for (Class<?> c : List.of(Socket.class, Listener.class)) {
            for (Method m : c.getDeclaredMethods()) {
                assertNotEquals(HostPort.class, m.getReturnType(), m.toString());
            }
        }
        assertEquals(InetSocketAddress.class, Socket.class.getMethod("localAddr").getReturnType());
        assertEquals(InetSocketAddress.class, Socket.class.getMethod("peerAddr").getReturnType());
        assertEquals(InetSocketAddress.class, Listener.class.getMethod("localAddr").getReturnType());
    }
}
