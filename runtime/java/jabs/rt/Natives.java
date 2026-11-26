package jabs.rt;

/** Services the JABS native runtime implements in C. */
final class Natives {
    private Natives() {}

    /** The compiled-in class with this binary name ({@code a.b.C}, {@code [La.b.C;}), or null. */
    static native Class<?> classNamed(String binaryName);

    /** The program's .jab (its class path), or null when it was compiled without one and its
     * resources are embedded instead (runtime/jabs_jab.c). */
    static native String jabPath();

    /** How many embedded resources have this name (one per input jar that contains it). */
    static native int resourceCount(String name);

    /** Contents of the index-th embedded resource with this name, or null. */
    static native byte[] resource(String name, int index);
}
