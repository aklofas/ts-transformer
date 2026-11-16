/**
 * The listening side of raw TCP. {@link org.tstrans.tcp.Listener} binds a
 * {@code tcp://ip:port?listen=1} URL and accepts one {@link org.tstrans.tcp.Transport}
 * per connecting peer; its failures throw {@link org.tstrans.TcpException}. The
 * calling side is a shell opened from a {@code tcp://} URL in
 * {@link org.tstrans.shells}.
 */
package org.tstrans.tcp;
