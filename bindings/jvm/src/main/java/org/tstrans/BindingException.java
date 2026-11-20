package org.tstrans;

/**
 * Base for all checked exceptions thrown across the JNI boundary. Every subclass is
 * final and carries a {@code Kind} enum; {@link #kind()} reads it through this type,
 * so a handler that catches {@code BindingException} needs no cast:
 * {@code catch (BindingException e) { log(e.kind().name()); }}.
 */
public abstract class BindingException extends Exception {
    private static final long serialVersionUID = 1L;

    protected BindingException(String message) {
        super(message);
    }

    /**
     * The error discriminant: a member of the concrete class's own {@code Kind} enum
     * (each subclass overrides this with that type). To keep or compare it outside
     * the process, use {@link Enum#name()}, not the ordinal: members are added in
     * later releases.
     *
     * @return the kind, never {@code null}
     */
    public abstract Enum<?> kind();
}
