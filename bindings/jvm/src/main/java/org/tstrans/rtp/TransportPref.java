package org.tstrans.rtp;

/**
 * Transport preference at SETUP. {@code AUTO} = UDP-first with TCP fallback on
 * 461; {@code UDP}/{@code TCP} force a single transport. Mirrors tst-py
 * {@code tstrans.rtp.TransportPref}.
 *
 * <p>{@code UDP} and {@code TCP} override the URL's {@code ?transport=} key;
 * {@code AUTO} leaves the URL in charge.
 */
public enum TransportPref { AUTO, UDP, TCP }
