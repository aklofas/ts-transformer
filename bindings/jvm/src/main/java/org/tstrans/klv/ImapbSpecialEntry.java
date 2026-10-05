package org.tstrans.klv;

/**
 * One entry of {@link UasDatalinkLs#imapbSpecials()} — an ST 1201.5
 * IMAPB item whose wire value decoded to a spec-defined special (§7.2.3)
 * rather than a normal-range float.
 *
 * <p>{@code code} names the special family: {@code "below_min"},
 * {@code "above_max"}, {@code "pos_infinity"}, {@code "neg_infinity"},
 * {@code "pos_quiet_nan"}, {@code "neg_quiet_nan"}, {@code "pos_signaling_nan"},
 * {@code "neg_signaling_nan"}, or {@code "user_defined"} — the same
 * strings tst-py's {@code imapb_specials} crossing uses. {@code payload}
 * carries the NaN-Id / signal value for the payload-carrying codes (0 for
 * the payload-less below_min/above_max/infinity codes).
 *
 * @param tag     the ST 0601 tag number
 * @param code    the special-value family name
 * @param payload the NaN-Id / signal payload (0 when not applicable)
 */
public record ImapbSpecialEntry(int tag, String code, long payload) {}
