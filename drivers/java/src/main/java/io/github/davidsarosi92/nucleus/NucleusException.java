package io.github.davidsarosi92.nucleus;

/** An error from the server ({@link #code()} is the SQLSTATE) or the driver. */
public class NucleusException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final String code;

    public NucleusException(String message, String code) {
        super(code == null ? message : message + " (" + code + ")");
        this.code = code;
    }

    public NucleusException(String message, Throwable cause) {
        super(message, cause);
        this.code = null;
    }

    /** The SQLSTATE, or null for a driver-side error. */
    public String code() {
        return code;
    }
}
