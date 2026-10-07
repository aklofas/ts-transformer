package org.tstrans.hls;

/** HLS-specific stats. Mirrors {@code tst_hls::HlsStats}. Counters widen to {@code long}. */
public record HlsStats(long segmentsWritten, long bytesPushedTotal, long openSegmentBytes, long forcedCuts) {}
