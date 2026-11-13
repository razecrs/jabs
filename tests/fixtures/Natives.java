// Runtime-implemented native methods and the java.lang.Object/Class root, checked through the
// JDK's own code: arraycopy (overlap, type, bounds, covariant partial copies), identity hashes,
// getClass/class literals, Class queries, clone, float/double bits and clocks.
public class Natives {
    static int copyInts(int from, int to, int len) {
        int[] a = {1, 2, 3, 4, 5, 6};
        System.arraycopy(a, from, a, to, len);
        int s = 0;
        for (int i = 0; i < a.length; i++) s = s * 10 + a[i];
        return s;
    }
    static int copyMismatch() { System.arraycopy(new int[2], 0, new long[2], 0, 1); return 0; }
    static int copyNull() { System.arraycopy(null, 0, new int[2], 0, 1); return 0; }
    static int copyCovariant(int which) {
        Shape[] src = { new Rect(1, 1), which == 0 ? (Shape) new Rect(2, 2) : new Circle(1), new Rect(3, 3) };
        Rect[] dst = new Rect[3];
        try {
            System.arraycopy(src, 0, dst, 0, 3);
            return 3;
        } catch (ArrayStoreException e) {
            int copied = 0;
            for (Rect r : dst) if (r != null) copied++;
            return -copied;
        }
    }
    static int hashStable() { Object o = new Object(); return o.hashCode() == o.hashCode() && System.identityHashCode(o) == o.hashCode() ? 1 : 0; }
    static int hashNull() { return System.identityHashCode(null); }
    static int classIdentity() {
        Object a = new Rect(1, 2), b = new Rect(3, 4), c = new Circle(1);
        return (a.getClass() == b.getClass() ? 1 : 0) + (a.getClass() != c.getClass() ? 10 : 0) + (Rect.class == a.getClass() ? 100 : 0)
                + (int[].class == new int[0].getClass() ? 1000 : 0);
    }
    static int className() { return String.class.getName().length() * 100 + int[].class.getName().length(); }
    static int classQueries() {
        return (Shape.class.isInstance(new Circle(2)) ? 1 : 0) + (Rect.class.isInstance(new Circle(2)) ? 10 : 0)
                + (Shape.class.isAssignableFrom(Square.class) ? 100 : 0) + (Rect.class.getSuperclass() == Shape.class ? 1000 : 0);
    }
    static int cloneArray() { int[] a = {5, 6, 7}; int[] b = a.clone(); b[0] = 9; return a[0] * 100 + b[0] * 10 + (a != b ? 1 : 0); }
    static long doubleBits(double d) { return Double.doubleToRawLongBits(d) ^ Double.doubleToRawLongBits(Double.longBitsToDouble(Double.doubleToRawLongBits(d))); }
    static int floatBits(float f) { return Float.floatToRawIntBits(Float.intBitsToFloat(Float.floatToRawIntBits(f))); }
    static int clocks() {
        long a = System.nanoTime(), b = System.nanoTime();
        return (b >= a ? 1 : 0) + (System.currentTimeMillis() > 1_600_000_000_000L ? 10 : 0);
    }
}
