package org.tstrans.internal;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.net.Inet4Address;
import java.net.Inet6Address;
import java.net.InetSocketAddress;
import org.junit.jupiter.api.Test;

/** The decoder alone, on the byte layouts the native getters write. */
class NativeAddrTest {
    private static final byte[] V6_LOOPBACK = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1};

    private static byte[] v6(byte[] ip, int port, int scope) {
        byte[] b = new byte[22];
        System.arraycopy(ip, 0, b, 0, 16);
        b[16] = (byte) (port >>> 8);
        b[17] = (byte) port;
        b[18] = (byte) (scope >>> 24);
        b[19] = (byte) (scope >>> 16);
        b[20] = (byte) (scope >>> 8);
        b[21] = (byte) scope;
        return b;
    }

    @Test
    void ipv4WithAPortAbove32767() {
        InetSocketAddress a = NativeAddr.decode(new byte[] {(byte) 203, 0, 113, 5, (byte) 0xFF, (byte) 0xFF});
        Inet4Address ip = assertInstanceOf(Inet4Address.class, a.getAddress());
        assertEquals("203.0.113.5", ip.getHostAddress());
        assertEquals(65535, a.getPort());
        assertEquals("203.0.113.5", a.getHostString());
    }

    @Test
    void ipv6WithoutAScope() {
        InetSocketAddress a = NativeAddr.decode(v6(V6_LOOPBACK, 0x8001, 0));
        Inet6Address ip = assertInstanceOf(Inet6Address.class, a.getAddress());
        assertEquals(0, ip.getScopeId());
        assertEquals("0:0:0:0:0:0:0:1", ip.getHostAddress());
        assertEquals(0x8001, a.getPort());
    }

    @Test
    void ipv6WithAScopeId() {
        byte[] linkLocal = {(byte) 0xFE, (byte) 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7};
        InetSocketAddress a = NativeAddr.decode(v6(linkLocal, 5000, 3));
        Inet6Address ip = assertInstanceOf(Inet6Address.class, a.getAddress());
        assertEquals(3, ip.getScopeId());
        assertEquals(5000, a.getPort());
    }

    @Test
    void aNullArrayIsNoAddress() {
        assertNull(NativeAddr.decode(null));
    }

    @Test
    void aMalformedLengthFailsLoudly() {
        for (int len : new int[] {0, 5, 7, 18, 21, 23}) {
            byte[] b = new byte[len];
            assertThrows(IllegalStateException.class, () -> NativeAddr.decode(b), "length " + len);
        }
    }
}
