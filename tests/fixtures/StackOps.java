// The stack-shuffling forms javac emits for chained assignments: dup_x2 (int into an int array),
// dup2_x1 (long into a field), dup2_x2 (long or double into an array).
public class StackOps {
    long field;
    static double dfield;
    static int intIntoArray(int x) {
        int[] a = new int[3];
        int r = a[1] = x;
        return r * 10 + a[1];
    }
    static long longIntoField(long v) {
        StackOps o = new StackOps();
        long r = o.field = v;
        return r - o.field + r;
    }
    static long longIntoArray(long v) {
        long[] a = new long[2];
        long r = a[1] = v;
        return r + a[1];
    }
    static double doubleIntoArray(double v) {
        double[] a = new double[2];
        double r = a[0] = v;
        return r * 2 + a[0];
    }
    static long increments(long v) {
        long[] a = {v};
        long before = a[0]++;
        long after = ++a[0];
        return before * 1000 + after;
    }
}
