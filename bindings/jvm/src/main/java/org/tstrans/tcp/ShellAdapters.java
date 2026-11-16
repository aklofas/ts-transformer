package org.tstrans.tcp;

import org.tstrans.shells.CancelHandle;

/**
 * Wraps the native keys the {@code org.tstrans.tcp} classes obtain into the
 * {@code org.tstrans.shells} classes their methods return. The shell constructors are
 * {@code protected}, so each wrapper is an anonymous subclass created here.
 */
final class ShellAdapters {
    private ShellAdapters() {}

    static CancelHandle cancelHandle(long key) { return new CancelHandle(key) {}; }
}
