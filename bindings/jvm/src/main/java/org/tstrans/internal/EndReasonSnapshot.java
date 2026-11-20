package org.tstrans.internal;

import org.tstrans.shells.EndReason;

/**
 * The close-time carrier a receiver's {@code nClose} returns: the end reason and detail as they
 * stood after teardown, cached by the Java object because its native handle no longer exists.
 *
 * <p><b>Implementation detail.</b> Public only so the sibling packages can read it; the module
 * does not export this package.
 */
public final class EndReasonSnapshot {
    private final EndReason reason;
    private final String detail;

    // Built by the natives only.
    private EndReasonSnapshot(int wireValue, String detail) {
        this.reason = EndReason.fromWireValue(wireValue);
        this.detail = detail;
    }

    /** @return the end reason, or {@code null} when none was recorded */
    public EndReason reason() { return reason; }

    /** @return the free-text detail, or {@code null} */
    public String detail() { return detail; }
}
