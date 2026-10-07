package org.tstrans.hls;

/** Shell-level stats. Mirrors {@code tst_pipeline::MuxPublisherStats}. */
public record MuxPublisherStats(long bytesPushed, long drainCalls, long cutCalls) {}
