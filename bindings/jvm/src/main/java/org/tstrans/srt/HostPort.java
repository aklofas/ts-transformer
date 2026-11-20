package org.tstrans.srt;

/**
 * A resolved socket endpoint — the Java mirror of tst-py's {@code (host, port)}
 * tuple return from {@code local_addr()} / {@code peer_addr()}. Java has no tuple
 * type; this 2-field record is the structural equivalent and is IPv6-safe (the
 * host is unbracketed, the port separate).
 *
 * <p>The choice of a record over a flat {@code "host:port"} string avoids IPv6
 * ambiguity: an IPv6 literal like {@code ::1} would need bracketing to distinguish
 * the colon from the host:port separator. Keeping host and port as separate fields
 * mirrors the tuple slot access in tst-py ({@code host, port = local_addr()}).
 *
 * @deprecated nothing returns it any more: {@link Socket#localAddr()},
 *     {@link Socket#peerAddr()} and {@link Listener#localAddr()} return an
 *     {@link java.net.InetSocketAddress}, which keeps an IPv6 scope id. Removed in 0.12.0.
 */
@Deprecated(since = "0.11.0", forRemoval = true)
public record HostPort(String host, int port) {}
