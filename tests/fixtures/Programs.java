// Whole programs for the differential test's main() mode: each class below is run with
// arguments on the JVM and as a native executable, and their stdout, exit status and uncaught
// exception class must match.
public class Programs {
    static int sumTo(int n) {
        int s = 0;
        for (int i = 1; i <= n; i++) s += i;
        return s;
    }
}

// Arguments arrive as a String[] in order, empty strings and spaces included.
class EchoArgs {
    public static void main(String[] args) {
        System.out.println(args.length);
        for (int i = 0; i < args.length; i++) {
            System.out.println(i + ": [" + args[i] + "] " + args[i].length());
        }
        System.out.println(Programs.sumTo(args.length * 10));
    }
}

// System.exit ends the program with that status after what was already printed.
class ExitCode {
    public static void main(String[] args) {
        System.out.println("exiting");
        System.exit(args.length == 0 ? 3 : Integer.parseInt(args[0]));
        System.out.println("unreachable");
    }
}

// An exception escaping main ends the program with status 1.
class Uncaught {
    public static void main(String[] args) {
        System.out.println("before");
        throw new IllegalStateException("boom");
    }
}

// So does one the JVM raises itself.
class UncaughtNull {
    static String nothing(int n) { return n > 0 ? "x" : null; }

    public static void main(String[] args) {
        System.out.println("length " + nothing(args.length).length());
    }
}

// System properties from the command line (-Dkey=value), as the java launcher passes them:
// split at the first '=', a bare key is an empty value, and an argument after the class (or
// after "--" natively) that looks like -D is a program argument, not a property.
class Props {
    public static void main(String[] args) {
        System.out.println(System.getProperty("jabs.test.value"));
        System.out.println("[" + System.getProperty("jabs.test.empty") + "]");
        System.out.println(Boolean.getBoolean("jabs.test.flag"));
        System.out.println(System.getProperty("jabs.test.missing", "default"));
        System.out.println(args.length + " " + (args.length > 0 ? args[0] : ""));
    }
}

// Stack traces as the JVM records them: innermost first, without fillInStackTrace or the
// throwable's own constructor frames, without lambda proxy frames, ending at main.
class Traces {
    static class Oops extends RuntimeException {
        Oops() { super("x"); }
    }

    static String frame(Throwable t, int depth) {
        StackTraceElement e = t.getStackTrace()[depth];
        return e.getClassName() + "." + e.getMethodName();
    }

    static String where(int depth) { return frame(new Throwable(), depth); }

    static String nested(int depth) { return where(depth); }

    public static void main(String[] args) {
        for (int d = 0; d < 3; d++) System.out.println(nested(d));
        System.out.println(frame(new Oops(), 0));
        System.out.println(new Throwable().getStackTrace().length);
        Runnable r = () -> System.out.println(frame(new Throwable(), 1));
        r.run();
        try {
            Integer.parseInt("x");
        } catch (NumberFormatException e) {
            System.out.println(frame(e, 0));
            System.out.println(e.getStackTrace()[e.getStackTrace().length - 1].getMethodName());
        }
    }
}

// The default time zone: from the host (TimeZone.getSystemTimeZoneID, which reads the OS
// setting the way the JDK does) unless user.timezone names one. On the same machine the ID
// and the offset must be the JVM's.
class TimeZones {
    public static void main(String[] args) {
        java.util.TimeZone zone = java.util.TimeZone.getDefault();
        System.out.println(zone.getID());
        System.out.println(zone.getRawOffset());
    }
}

// CRC32 as zlib computes it: whole arrays, ranges, single bytes and direct buffers, continuing
// from earlier input; the values must be the JVM's bit for bit.
class Checksums {
    public static void main(String[] args) {
        java.util.zip.CRC32 crc = new java.util.zip.CRC32();
        System.out.println(Long.toHexString(crc.getValue()));
        crc.update("123456789".getBytes());
        System.out.println(Long.toHexString(crc.getValue()));
        crc.reset();
        byte[] all = new byte[256];
        for (int i = 0; i < all.length; i++) all[i] = (byte) i;
        crc.update(all, 10, 200);
        System.out.println(Long.toHexString(crc.getValue()));
        crc.update(0xff);
        crc.update(-1);
        System.out.println(Long.toHexString(crc.getValue()));
        java.nio.ByteBuffer direct = java.nio.ByteBuffer.allocateDirect(64);
        for (int i = 0; i < 64; i++) direct.put((byte) (i * 7));
        direct.flip();
        crc.update(direct);
        System.out.println(Long.toHexString(crc.getValue()));
    }
}

// A class on no class path, probed the way Netty probes for SLF4J: the JVM resolves a class
// only when an instruction naming it executes (a type test of null resolves nothing) and then
// throws NoClassDefFoundError, which the probe catches and carries on. The differential test
// deletes GoneProbe.class after javac, so it is missing on the JVM and natively alike.
class AbsentProbe {
    public static void main(String[] args) {
        Object something = "x";
        Object nothing = null;
        System.out.println(nothing instanceof GoneProbe);
        System.out.println((GoneProbe) nothing == null);
        // java.sql.Timestamp exists (module java.sql) but is not in the test's unit: a null
        // test resolves nothing, and the method testing a real value compiles (it would stop
        // only if it ran), as Netty's PlatformDependent0 needs for sun.misc.Unsafe.
        System.out.println(nothing instanceof java.sql.Timestamp);
        try {
            System.out.println(something instanceof GoneProbe);
        } catch (NoClassDefFoundError e) {
            System.out.println("instanceof: " + e.getClass().getName());
        }
        try {
            GoneProbe.hello();
        } catch (NoClassDefFoundError e) {
            System.out.println("static call: " + e.getClass().getName());
        }
        System.out.println("after the probes");
    }

    static boolean neverCalled(Object o) {
        return o instanceof java.sql.Timestamp;
    }
}

class GoneProbe {
    static void hello() { System.out.println("hello"); }
}
