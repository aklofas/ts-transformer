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
 * has a {@link org.tstrans.shells.CancelHandle}, and the receivers report why a
 * session ended through {@link org.tstrans.shells.EndReason}.
 */
package org.tstrans.shells;
