// Arrays of every element type, 2-D arrays, reference arrays with covariant
// stores, and every array exception in the JVM's order of checks.
public class Arrays {
    static long sumSquares(int n) {
        int[] a = new int[n];
        for (int i = 0; i < n; i++) a[i] = i * i;
        long total = 0;
        for (int i = 0; i < a.length; i++) total += a[i];
        return total;
    }
    static int bytes(int v) {
        byte[] b = new byte[4];
        b[0] = (byte) v; b[1] = (byte) (v >> 8); b[2] = (byte) 200; b[3] = -1;
        return b[0] + b[1] * 3 + b[2] * 5 + b[3] * 7;
    }
    static int booleans(int v) {
        boolean[] f = new boolean[3];
        f[0] = v > 0; f[2] = (v & 1) == 1;
        return (f[0] ? 1 : 0) + (f[1] ? 10 : 0) + (f[2] ? 100 : 0);
    }
    static int chars(int v) { char[] c = new char[2]; c[0] = (char) v; c[1] = (char) -v; return c[0] + c[1]; }
    static int shorts(int v) { short[] s = new short[1]; s[0] = (short) v; return s[0]; }
    static double doubles(int n) {
        double[] d = new double[n];
        for (int i = 0; i < n; i++) d[i] = 1.0 / (i + 1);
        double t = 0;
        for (double x : d) t += x;
        return t;
    }
    static float floats(float x) { float[] f = {x, x * 2, -x}; return f[0] + f[1] * f[2]; }
    static long longs(long x) { long[] l = new long[3]; l[1] = x; l[2] = l[1] * 3; return l[0] + l[1] + l[2]; }
    static int grid(int n) {
        int[][] g = new int[n][];
        for (int i = 0; i < n; i++) {
            g[i] = new int[i + 1];
            for (int j = 0; j <= i; j++) g[i][j] = i * 10 + j;
        }
        int t = 0;
        for (int[] row : g) for (int x : row) t += x;
        return t + g.length * 1000;
    }
    static double shapes(int n) {
        Shape[] s = new Shape[n];
        for (int i = 0; i < n; i++) s[i] = Shapes.make(i % 3, i + 1);
        double t = 0;
        for (Shape x : s) t += x.area();
        return t;
    }
    static int objects(int n) {
        Object[] o = new Object[n];
        for (int i = 0; i < n; i++) o[i] = i % 2 == 0 ? new Circle(i) : null;
        int count = 0;
        for (Object x : o) if (x != null) count++;
        return count;
    }
    static int covariantStore(int which) {
        Shape[] s = new Rect[1];        // runtime type Rect[]
        s[0] = Shapes.make(which, 1);   // a Circle must fail with ArrayStoreException
        return s[0].kind();
    }
    // Arrays whose rows are reference arrays: a row store is checked covariantly, as instanceof
    // is (java.util.Locale's LanguageTag builds a String[][] like rowsOfStrings).
    static int rowsOfStrings(int n) {
        String[][] t = new String[n][];
        for (int i = 0; i < n; i++) t[i] = new String[] { "a", String.valueOf(i) };
        int total = 0;
        for (String[] row : t) total += row[1].length();
        return total;
    }
    static int rowStore(int which) {
        Object[][] rows = new CharSequence[1][];     // runtime type CharSequence[][]
        Object[] row = which == 0 ? new String[1]        // a String[] is a CharSequence[]
                     : which == 1 ? new StringBuilder[1] // so is a StringBuilder[]
                     : new Integer[1];                   // ArrayStoreException
        rows[0] = row;
        return rows[0].length + which;
    }
    static int reflectRows(int which) {
        Object[][] rows = (Object[][]) java.lang.reflect.Array.newInstance(Object[].class, 1);
        rows[0] = which == 0 ? new String[2] : new Object[3]; // both are Object[]
        return rows[0].length;
    }
    // multianewarray: every level allocated at once, a partial last level, the leaf element
    // type checked on stores, and every count checked before anything is allocated.
    static long matrix(int a, int b) {
        long[][] m = new long[a][b];
        long t = m.length * 100L;
        for (int i = 0; i < a; i++) for (int j = 0; j < b; j++) { m[i][j] = i * 7L + j; t += m[i][j] + m[i].length; }
        return t;
    }
    static int partialCube(int a, int b) {
        int[][][] c = new int[a][b][];
        int nulls = 0;
        for (int[][] plane : c) for (int[] row : plane) if (row == null) nulls++;
        if (a > 0 && b > 0) c[0][0] = new int[] {5};
        return nulls * 10 + (a > 0 && b > 0 ? c[0][0][0] : 0);
    }
    static int multiStore(int which) {
        Object[][] g = new String[2][3];
        if (which == 2) { g[1] = new Integer[1]; return -2; } // ArrayStoreException: Integer[] is no String[]
        Object[] row = g[0];
        row[1] = which == 0 ? "s" : new StringBuilder("b"); // ArrayStoreException for the StringBuilder
        return g[0][1].toString().length() + g[1].length;
    }
    // Object's methods called virtually on arrays (an Object-typed receiver): arrays dispatch
    // through Object's vtable. Identity hashes differ between runs, so only their use is checked.
    static int objectMethodsOnArrays(int n) {
        Object a = new int[n];
        Object b = new String[n];
        java.util.Set<Object> set = new java.util.HashSet<>();
        set.add(a);
        set.add(b);
        int r = set.contains(a) && set.contains(b) && !set.contains(new int[n]) ? 1 : 0;
        r += a.equals(a) && !a.equals(b) ? 10 : 0;
        r += a.hashCode() == a.hashCode() ? 100 : 0;
        r += a.toString().startsWith("[I@") && b.toString().startsWith("[Ljava.lang.String;@") ? 1000 : 0;
        return r;
    }
    // A class named only by a three-level multianewarray: the allocation names the record of
    // the innermost class, so that record must be emitted (it once was not, and the C did not
    // compile).
    static int onlyInDeepArrays(int a, int b) {
        ArraysOnlyDeep[][][] grid = new ArraysOnlyDeep[a][b][];
        int nulls = 0;
        for (ArraysOnlyDeep[][] plane : grid) for (ArraysOnlyDeep[] row : plane) if (row == null) nulls++;
        return grid.length * 100 + nulls;
    }
    static int multiNegative(int a, int b) { return new int[a][b].length; }
    static int negative(int n) { return new int[n].length; }
    static int outOfBounds(int i) { int[] a = new int[3]; return a[i]; }
    static int nullArray(int i) { int[] a = i > 100 ? new int[1] : null; return a[0]; }
    static int nullLength() { int[] a = null; return a.length; }
}

// Named only by Arrays.onlyInDeepArrays.
class ArraysOnlyDeep {}
