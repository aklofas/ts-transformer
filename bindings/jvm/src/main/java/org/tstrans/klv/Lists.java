package org.tstrans.klv;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/** List helpers for the record compact constructors. */
final class Lists {
    private Lists() {}

    /**
     * Unmodifiable snapshot of {@code in}, or an empty list for {@code null}. The copy is
     * shallow: the elements stay caller-owned, so a later change to the caller's list is
     * not visible through the record.
     */
    static <T> List<T> snapshot(List<T> in) {
        return in == null ? List.of() : Collections.unmodifiableList(new ArrayList<>(in));
    }

    /** Like {@link #snapshot} but keeps {@code null}, for fields where null means "absent". */
    static <T> List<T> snapshotOrNull(List<T> in) {
        return in == null ? null : snapshot(in);
    }
}
