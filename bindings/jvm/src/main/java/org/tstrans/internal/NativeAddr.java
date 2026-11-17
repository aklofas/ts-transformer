package org.tstrans.internal;

import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.UnknownHostException;
import java.nio.ByteBuffer;

/**
 * Turns the address bytes a native getter returns into an {@link InetSocketAddress}.
 *
 * <p><b>Implementation detail.</b> Public only so the sibling packages can call it; the module
 * does not export this package. The layout is the address bytes (4 for IPv4, 16 for IPv6), the
 * port as two big-endian bytes, and for IPv6 the scope id as four big-endian bytes. The address
 * is built from those bytes alone, so no name lookup can happen.
 */
public final class NativeAddr {
    private NativeAddr() {}

    /**
     * The address in {@code b}, or {@code null} for a {@code null} array.
     *
     * @param b the bytes a native getter returned
     * @return the socket address, or {@code null}
     * @throws IllegalStateException if {@code b} is not one of the two layouts
     */
    public static InetSocketAddress decode(byte[] b) {
        if (b == null) return null;
        ByteBuffer buf = ByteBuffer.wrap(b);
        try {
            switch (b.length) {
                case 6: {
                    byte[] ip = new byte[4];
                    buf.get(ip);
                    return new InetSocketAddress(InetAddress.getByAddress(ip), buf.getShort() & 0xFFFF);
                }
                case 22: {
                    byte[] ip = new byte[16];
                    buf.get(ip);
                    int port = buf.getShort() & 0xFFFF;
                    int scope = buf.getInt();
                    InetAddress addr = scope == 0
                        ? InetAddress.getByAddress(ip)
                        : Inet6Address.getByAddress(null, ip, scope);
                    return new InetSocketAddress(addr, port);
                }
                default:
                    throw new IllegalStateException("native address of " + b.length + " bytes");
            }
        } catch (UnknownHostException e) {
            // getByAddress throws only for a wrong length, which the switch rules out.
            throw new IllegalStateException(e);
        }
    }
}
