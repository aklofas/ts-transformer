package org.tstrans.mpegts;

/** Collapsed discriminator for {@code DemuxEvent.NonConformant}. Mirrors tst-py's NonConformantKind, which collapses Rust {@code NonConformantIssue}'s 30+ variants; the {@code issue} field on the event carries the human-readable detail. */
public enum NonConformantKind {
    PCR_ANOMALY, PSI_CHECKSUM_MISMATCH, MALFORMED_PES, PUSI_MID_PES, TRANSPORT_ERROR_PACKET,
    STREAM_TYPE_MISMATCH, PID_REUSED_ACROSS_PROGRAMS, NAL_HEADER, AV1_OBU_HEADER,
    AV1_REGISTRATION_MALFORMED, PES_HEADER_MALFORMED, PTS_ANOMALY, MISSING_REQUIRED_PTS,
    PCR_MALFORMED, SUBTITLE_MISSING_DESCRIPTOR, SUBTITLE_ALIGNMENT_MISSING, MULTI_CELL_AU,
    CFI_TOLERATED, PSI_MULTI_SECTION_UNSUPPORTED, PSI_CC_DISCONTINUITY, PSI_OVERLONG_SECTION,
    DVB_SUB_DATA_IDENTIFIER, AC3_SYNC_MISSING, LATM_FRAMING, AV1_WRONG_STREAM_ID,
    AV1_MISSING_TS_OBU_FRAMING, AV1_OBU_MISSING_SIZE_FIELD, AV1_TILE_LIST_NOT_ALLOWED,
    MISSING_METADATA_DESCRIPTOR, SUBTITLE_DESCRIPTOR_AMBIGUOUS, SUBTITLE_DESCRIPTOR_MALFORMED,
    /** PMT body {@code program_number} does not match PAT assignment. */
    PMT_PROGRAM_NUMBER_MISMATCH,
    /** transport_scrambling_control != 0; payload not routed. */
    UNSUPPORTED_SCRAMBLING,
    /** Adaptation-field control/length violation. */
    ADAPTATION_FIELD_MALFORMED,
    /** Zero PES_packet_length on a non-video stream; partial dropped. */
    ZERO_LENGTH_PES_NON_VIDEO,
    /** PAT/PMT fixed/reserved syntax field violation. */
    PSI_SYNTAX,
    OTHER
}
