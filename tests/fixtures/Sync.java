// Monitors (synchronized methods and blocks, reentrancy, release on exceptions, null locks)
// and casts/instanceof against array types, including covariant reference arrays.
public class Sync {
    int count;

    synchronized int inc() { return ++count; }
    synchronized int recurse(int n) { return n == 0 ? count : recurse(n - 1) + 1; }
    static synchronized int twice(int x) { return x * 2; }

    static int block(int n) {
        Sync s = new Sync();
        int t = 0;
        for (int i = 0; i < n; i++) {
            synchronized (s) { t += s.inc(); }
        }
        return t;
    }
    static int blockThrows(int b) {
        Sync s = new Sync();
        try {
            synchronized (s) { return 10 / b; }
        } catch (ArithmeticException e) {
            synchronized (s) { return -s.inc(); }
        }
    }
    static int nullLock() { Object o = null; synchronized (o) { return 1; } }
    static int reentrant(int n) {
        Sync s = new Sync();
        synchronized (s) { synchronized (s) { return s.recurse(n) + twice(n); } }
    }

    synchronized int divideInside() { return 10 / count; }

    // Ownership as Thread.holdsLock sees it (runtime/jabs_sync.c, jabs_monitor_holds).
    /** Digits, outermost first: before, in the outer block, the inner one, back out, after. */
    static int holdsLockFollowsLevels() {
        Object o = new Object();
        int r = Thread.holdsLock(o) ? 1 : 0;
        synchronized (o) {
            r = r * 10 + (Thread.holdsLock(o) ? 1 : 0);
            synchronized (o) { r = r * 10 + (Thread.holdsLock(o) ? 1 : 0); }
            r = r * 10 + (Thread.holdsLock(o) ? 1 : 0);
        }
        return r * 10 + (Thread.holdsLock(o) ? 1 : 0);
    }
    /** A synchronized method that throws still releases its receiver's monitor. */
    static int holdsLockAfterSynchronizedMethodThrows() {
        Sync s = new Sync();
        try { s.divideInside(); } catch (ArithmeticException e) { return Thread.holdsLock(s) ? 1 : 2; }
        return 3;
    }
    /** notify needs ownership: fine inside the block, IllegalMonitorStateException after it. */
    static int notifyAfterExitThrows() {
        Object o = new Object();
        synchronized (o) { o.notify(); o.notifyAll(); }
        o.notify();
        return 1;
    }
    static int notifyAllNeverLockedThrows() { new Object().notifyAll(); return 1; }
    /** A clone does not share the original's monitor (jabs_clone resets the header). */
    static int cloneHasItsOwnMonitor() {
        int[] a = new int[1];
        synchronized (a) {
            int[] b = a.clone();
            return (Thread.holdsLock(a) ? 10 : 0) + (Thread.holdsLock(b) ? 1 : 0);
        }
    }
    static int holdsLockNull() { return Thread.holdsLock(null) ? 1 : 0; }

    static int castIntArray(int which) {
        Object o = which == 0 ? (Object) new int[4] : new long[2];
        return ((int[]) o).length;
    }
    static int arrayKinds(int which) {
        Object o = which == 0 ? (Object) new Rect[1] : which == 1 ? (Object) new Shape[2] : new Object[3];
        return (o instanceof Shape[] ? 1 : 0) + (o instanceof Rect[] ? 10 : 0) + (o instanceof Object[] ? 100 : 0)
                + (o instanceof int[] ? 1000 : 0);
    }
    static int castShapes(int which) {
        Object o = which == 0 ? (Object) new Rect[1] : new Circle[1];
        Rect[] r = (Rect[]) o;
        return r.length;
    }
    static int rows() {
        int[][] g = new int[2][];
        g[1] = new int[3];
        Object o = g;
        int[][] back = (int[][]) o;
        return back[1].length + (o instanceof Object[] ? 100 : 0) + (o instanceof Cloneable[] ? 1000 : 0);
    }
}
