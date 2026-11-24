package org.tstrans.rtp;

/**
 * How many requests one rule refused for the credential they carried, per direction, from
 * {@link MountRules#authFailures(String)} and {@link MountRules#defaultAuthFailures()}.
 * Mirrors {@code tst_rtp::MountRuleAuthFailures}.
 *
 * <p>A request counts when it carried an {@code Authorization} header and was answered 401
 * for any reason but a stale nonce: a wrong password, an unknown user, a malformed header,
 * the wrong scheme. The first challenge every client gets (no header yet), a stale nonce
 * and a {@link MountPolicy#refuse refuse} rule's status never count. An honest client's
 * first answer after {@link MountRules#setChallenge} changed the scheme or the realm does.
 *
 * <p>A rate signal: an unauthenticated peer can raise it at will (three refused answers per
 * connection under Basic or Digest: a Digest peer can take its nonce from a stale challenge,
 * which is no strike; any number of connections), and a peer that holds one valid credential
 * resets its strikes with it and
 * can try other credentials without bound on one connection. A lock-out built on one name's
 * count can therefore be triggered against that name's owner.
 *
 * <p>Unsigned 64-bit counts are carried as the bit pattern of a signed {@code long}.
 *
 * <p><b>Stability: Experimental</b>, like the rules table.
 *
 * @param publish refused requests that publish ({@code ANNOUNCE}, and the requests of a
 *     session that announced)
 * @param read refused requests that read ({@code DESCRIBE}, and the requests of a reader
 *     or an idle session)
 */
public record MountRuleAuthFailures(long publish, long read) {}
