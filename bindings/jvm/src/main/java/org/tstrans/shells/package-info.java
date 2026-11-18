/**
 * One class per pipeline shell, over every transport. {@link org.tstrans.shells.Sender}
 * and {@link org.tstrans.shells.Receiver} move pre-muxed TS bytes;
 * {@link org.tstrans.shells.MuxSender} and {@link org.tstrans.shells.DemuxReceiver}
 * own a muxer or demuxer. Each is opened from a URL with {@code Class.open(url, ...)}:
 * the scheme ({@code srt://}, {@code rtp://}, {@code udp://}, {@code tcp://},
 * {@code tcps://}, {@code rist://}) picks the transport, and a transport failure
 * throws that transport's exception ({@link org.tstrans.SrtException},
 * {@link org.tstrans.RtpException}, {@link org.tstrans.UdpException},
 * {@link org.tstrans.TcpException}, {@link org.tstrans.RistException}). Every shell
 * has a {@link org.tstrans.shells.CancelHandle}; an open that has not returned a shell
 * yet is cancelled through an {@link org.tstrans.shells.OpenCancel} given to it. The
 * receivers report why a
 * session ended through {@link org.tstrans.shells.EndReason}. A {@code udp://} or
 * {@code rist://@host:port} receiver reports the port it bound with
 * {@code localPort()}, so it can be opened on port 0. A shell over a TCP connection
 * a server accepted comes from {@link org.tstrans.tcp.Listener}.
 */
package org.tstrans.shells;
