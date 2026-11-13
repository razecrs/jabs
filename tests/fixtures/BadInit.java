// A static initializer that throws: the first use must raise ExceptionInInitializerError
// and every later use NoClassDefFoundError, as the JVM does.
public class BadInit {
    static int zero = 0;
    static int x = 1 / zero;

    static int get() { return x; }
}
