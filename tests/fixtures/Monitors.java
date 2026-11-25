// Object.wait/notify/notifyAll on one thread, against the JVM: who may call them
// (IllegalMonitorStateException), timeouts (IllegalArgumentException for negative ones), and
// that a timed wait gives the monitor back at the same recursion depth. If these differ, look at
// jabs_monitor_wait/jabs_monitor_notify in runtime/jabs_sync.c and the Object.wait0/notify
// intrinsics in src/cgen.rs.
public class Monitors {
    int value;

    /** A timed wait with nobody to notify returns once the time is up, still owning the lock. */
    static int timedWaitReturnsOwningTheLock() throws InterruptedException {
        Object o = new Object();
        synchronized (o) {
            o.wait(5);
            return Thread.holdsLock(o) ? 1 : 0;
        }
    }
    /** wait releases every level and restores all of them: two exits are still needed after. */
    static int monitorReentryRestoresCountAfterWait() throws InterruptedException {
        Object o = new Object();
        int r;
        synchronized (o) {
            synchronized (o) {
                o.wait(2);
                r = Thread.holdsLock(o) ? 1 : 0;
            }
            r = r * 10 + (Thread.holdsLock(o) ? 1 : 0);
        }
        return r * 10 + (Thread.holdsLock(o) ? 1 : 0);
    }
    /** wait(millis, nanos) with only nanos still waits (rounds up to a millisecond). */
    static int waitWithNanosReturns() throws InterruptedException {
        Object o = new Object();
        synchronized (o) { o.wait(0, 500000); }
        return 1;
    }
    /** A notify with nobody waiting is not remembered: the next timed wait still times out. */
    static int notifyBeforeWaitIsNotRemembered() throws InterruptedException {
        Object o = new Object();
        synchronized (o) {
            o.notify();
            o.notifyAll();
            long start = System.nanoTime();
            o.wait(20);
            return System.nanoTime() - start >= 20_000_000L ? 1 : 0;
        }
    }
    /** wait in a synchronized method waits on the receiver's monitor. */
    synchronized int waitInSynchronizedMethod() throws InterruptedException {
        wait(1);
        notify();
        return ++value;
    }
    static int synchronizedMethodWaits() throws InterruptedException {
        return new Monitors().waitInSynchronizedMethod();
    }

    // Misuse, each failing the way the JVM fails.
    static int waitWithoutLockThrows() throws InterruptedException { new Object().wait(1); return 1; }
    static int waitForeverWithoutLockThrows() throws InterruptedException { new Object().wait(); return 1; }
    static int waitAfterExitThrows() throws InterruptedException {
        Object o = new Object();
        synchronized (o) { o.wait(1); }
        o.wait(1);
        return 1;
    }
    /** Holding another object's lock does not help. */
    static int waitOnOtherObjectThrows() throws InterruptedException {
        Object a = new Object(), b = new Object();
        synchronized (a) { b.wait(1); }
        return 1;
    }
    /** The negative timeout is rejected before ownership is checked. */
    static int waitNegativeWithoutLockThrows() throws InterruptedException { new Object().wait(-1); return 1; }
    static int waitNegativeThrows() throws InterruptedException {
        Object o = new Object();
        synchronized (o) { o.wait(-1); }
        return 1;
    }
    static int waitBadNanosThrows() throws InterruptedException {
        Object o = new Object();
        synchronized (o) { o.wait(1, 1_000_000); }
        return 1;
    }
    static int notifyOnOtherObjectThrows() {
        Object a = new Object(), b = new Object();
        synchronized (a) { b.notify(); }
        return 1;
    }
    static int notifyAllAfterExitThrows() {
        Object o = new Object();
        synchronized (o) { o.notifyAll(); }
        o.notifyAll();
        return 1;
    }
    static int waitOnNullThrows() throws InterruptedException { Object o = null; o.wait(1); return 1; }
}
