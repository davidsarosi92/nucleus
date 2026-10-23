package io.github.davidsarosi92.nucleus;

/**
 * An error from the server ({@link #code()} is the SQLSTATE, {@link #hint()} what to do
 * about it) or the driver.
 */
public class NucleusException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final String code;
    private final String hint;

    public NucleusException(String message, String code) {
        this(message, code, null);
    }

    public NucleusException(String message, String code, String hint) {
        super(code == null ? message : message + " (" + code + ")");
        this.code = code;
        this.hint = hint;
    }

    public NucleusException(String message, Throwable cause) {
        super(message, cause);
        this.code = null;
        this.hint = null;
    }

    /** The SQLSTATE, or null for a driver-side error. */
    public String code() {
        return code;
    }

    /**
     * What to do about it — the name meant, the cast that would work (protocol version 5),
     * or null.
     */
    public String hint() {
        return hint;
    }
}
