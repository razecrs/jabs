// try/catch/finally over runtime-raised exceptions: exact and superclass catches, nesting,
// finally on both paths, rethrow, identity of caught objects and uncaught propagation.
public class Exceptions {
    static int divide(int a, int b) { try { return a / b; } catch (ArithmeticException e) { return -1; } }
    static int index(int i) { int[] a = new int[3]; try { return a[i]; } catch (ArrayIndexOutOfBoundsException e) { return -2; } }
    static int broad(int n) { try { return new int[n].length; } catch (RuntimeException e) { return -3; } }
    static int nested(int a, int b) {
        try {
            try { return a / b; } catch (NullPointerException e) { return -4; }
        } catch (ArithmeticException e) { return -5; }
    }
    static int finallyCounts(int b) {
        int x = 0;
        try { x = 10 / b; } catch (ArithmeticException e) { x = -1; } finally { x += 100; }
        return x;
    }
    static int inner(int b, int[] counter) { try { return 10 / b; } finally { counter[0]++; } }
    static int finallyRethrows(int b) {
        int[] counter = new int[1];
        try { return inner(b, counter) + counter[0] * 1000; } catch (ArithmeticException e) { return counter[0] * 1000 - 1; }
    }
    static int rethrow(int b) { try { return 10 / b; } catch (ArithmeticException e) { throw e; } }
    static int caughtIdentity(int b) {
        ArithmeticException first = null;
        try { return 10 / b; } catch (ArithmeticException e) { first = e; }
        try { throw first; } catch (ArithmeticException again) { return again == first ? 1 : 0; }
    }
    static int npeCatch(int v) { Node n = v > 0 ? new Node(v, null) : null; try { return n.value; } catch (NullPointerException e) { return 7; } }
    static int instanceOfCaught(int b) { try { return 1 / b; } catch (RuntimeException e) { return e instanceof ArithmeticException ? 1 : 2; } }
    static int loopWithCatch(int n) {
        int s = 0;
        for (int i = -n; i <= n; i++) {
            try { s += 100 / i; } catch (ArithmeticException e) { s += 1000; }
        }
        return s;
    }
    static int throwNull() { RuntimeException e = null; throw e; }
    static int passesThrough(int b) { try { return 10 / b; } catch (NullPointerException e) { return -9; } }
}
