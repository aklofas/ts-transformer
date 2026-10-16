package org.tstrans.rtp;

/**
 * Wire-time RTSP version. Mirrors tst-py {@code tstrans.rtp.RtspVersion}.
 *
 * <p>{@code V2_0} overrides the URL's {@code ?rtsp_version=} key; {@code V1_0}
 * (the default) leaves the URL in charge.
 */
public enum RtspVersion { V1_0, V2_0 }
