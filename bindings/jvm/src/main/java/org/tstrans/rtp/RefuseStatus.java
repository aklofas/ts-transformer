package org.tstrans.rtp;

/** The status line a {@link MountPolicy#refuse(RefuseStatus)} answers with. */
public enum RefuseStatus {
    /** {@code 403 Forbidden}. */
    FORBIDDEN(403),
    /** {@code 404 Not Found}. */
    NOT_FOUND(404),
    /** {@code 453 Not Enough Bandwidth}. */
    NOT_ENOUGH_BANDWIDTH(453),
    /** {@code 503 Service Unavailable}. */
    SERVICE_UNAVAILABLE(503);

    private final int code;

    RefuseStatus(int code) { this.code = code; }

    /** The numeric RTSP status code. */
    public int code() { return code; }
}
