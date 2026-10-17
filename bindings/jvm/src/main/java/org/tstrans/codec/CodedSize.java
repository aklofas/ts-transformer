package org.tstrans.codec;

/**
 * The coded size of an SPS: the post-crop size plus the two crop offsets. The formula of the Rust
 * {@code coded_width()} / {@code coded_height()}, shared by the H.264, H.265 and H.266 records.
 */
final class CodedSize {
    private CodedSize() {}

    static long of(long size, long cropA, long cropB) {
        return size + cropA + cropB;
    }
}
