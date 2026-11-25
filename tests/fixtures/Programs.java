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
