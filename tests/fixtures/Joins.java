// Thread programs that need real monitors (threads-sync): join, Object.wait and interrupting it.
// As in Threads.java, main prints only after the other threads have ended.

// join() returns once run() has ended, also when run() threw: the end-of-thread step notifies
// join() waiters on the Thread's monitor after marking it TERMINATED.
class JoinReturnsAfterRunThrows {
    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(50);
            } catch (InterruptedException e) {
                return;
            }
            throw new IllegalStateException("late");
        }, "worker");
        t.start();
        t.join();
        System.out.println("joined " + t.getState() + " " + t.isAlive());
    }
}

// join() waits for run() to finish its work, and sees what it wrote.
class JoinSeesResult {
    static long total;

    public static void main(String[] args) throws Exception {
        Thread[] threads = new Thread[4];
        final long[] parts = new long[threads.length];
        for (int i = 0; i < threads.length; i++) {
            final int k = i;
            threads[i] = new Thread(() -> {
                long s = 0;
                for (int n = 0; n < 1000000; n++) s += n % (k + 2);
                parts[k] = s;
            }, "part" + i);
            threads[i].start();
        }
        for (Thread t : threads) t.join();
        for (long p : parts) total += p;
        System.out.println(total);
    }
}

// Thread.interrupt ends an Object.wait with InterruptedException and clears the interrupt status.
class WaitInterrupted {
    static final Object lock = new Object();
    static String result;

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                    result = "woke without an interrupt";
                } catch (InterruptedException e) {
                    result = "InterruptedException " + Thread.currentThread().isInterrupted();
                }
            }
        }, "waiter");
        t.start();
        t.interrupt();
        t.join();
        System.out.println(result);
    }
}

// Producer and consumer hand values over with wait/notify on a shared monitor; the sum is fixed.
class HandOffWithNotify {
    static final Object lock = new Object();
    static int slot = -1;

    public static void main(String[] args) throws Exception {
        final long[] sum = new long[1];
        Thread consumer = new Thread(() -> {
            try {
                for (int i = 0; i < 1000; i++) {
                    synchronized (lock) {
                        while (slot < 0) lock.wait();
                        sum[0] += slot;
                        slot = -1;
                        lock.notifyAll();
                    }
                }
            } catch (InterruptedException e) {
                sum[0] = -1;
            }
        }, "consumer");
        consumer.start();
        for (int i = 0; i < 1000; i++) {
            synchronized (lock) {
                while (slot >= 0) lock.wait();
                slot = i;
                lock.notifyAll();
            }
        }
        consumer.join();
        System.out.println(sum[0]);
    }
}

// Contended synchronized blocks exclude: four threads each add 100000 under one lock.
class ContendedSynchronizedCounter {
    static final Object lock = new Object();
    static int count;

    public static void main(String[] args) throws Exception {
        Thread[] threads = new Thread[4];
        for (int i = 0; i < threads.length; i++) {
            threads[i] = new Thread(() -> {
                for (int n = 0; n < 100000; n++) {
                    synchronized (lock) {
                        count++;
                    }
                }
            }, "adder" + i);
            threads[i].start();
        }
        for (Thread t : threads) t.join();
        System.out.println(count);
    }
}

// notify wakes exactly one of several waiters; notifyAll wakes the rest.
class NotifyWakesOne {
    static final Object lock = new Object();
    static int waiting;
    static int woken;

    public static void main(String[] args) throws Exception {
        Thread[] threads = new Thread[4];
        for (int i = 0; i < threads.length; i++) {
            threads[i] = new Thread(() -> {
                synchronized (lock) {
                    waiting++;
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                    woken++;
                }
            }, "waiter" + i);
            threads[i].start();
        }
        while (true) {
            synchronized (lock) {
                if (waiting == threads.length) break;
            }
            Thread.sleep(1);
        }
        synchronized (lock) {
            lock.notify();
        }
        // Wait until the notified waiter has really run, then give any wrongly woken extra
        // waiter time to show. A bare sleep was load-sensitive: on a busy machine the woken
        // thread might not run within it, on either the JVM or natively, and read as 0. Now load
        // can only hide a wrong extra wake-up, never invent a failure.
        for (int spins = 0; spins < 10000; spins++) {
            synchronized (lock) {
                if (woken > 0) break;
            }
            Thread.sleep(1);
        }
        Thread.sleep(200);
        int afterOne;
        synchronized (lock) {
            afterOne = woken;
            lock.notifyAll();
        }
        for (Thread t : threads) t.join();
        System.out.println(afterOne + " " + woken);
    }
}

// A timed wait ends early when notified (well before its minute is up).
class TimedWaitWokenEarly {
    static final Object lock = new Object();
    static boolean waiting;
    static long waitedMillis;

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            synchronized (lock) {
                waiting = true;
                long start = System.nanoTime();
                try {
                    lock.wait(60000);
                } catch (InterruptedException e) {
                    return;
                }
                waitedMillis = (System.nanoTime() - start) / 1000000;
            }
        }, "waiter");
        t.start();
        while (true) {
            synchronized (lock) {
                if (waiting) {
                    lock.notify();
                    break;
                }
            }
            Thread.sleep(1);
        }
        t.join();
        System.out.println(waitedMillis < 30000 ? "woken early" : "waited " + waitedMillis + " ms");
    }
}

// AtomicInteger.incrementAndGet from four threads loses no update (Unsafe compare-and-set).
class AtomicIntegerContention {
    public static void main(String[] args) throws Exception {
        final java.util.concurrent.atomic.AtomicInteger counter = new java.util.concurrent.atomic.AtomicInteger();
        Thread[] threads = new Thread[4];
        for (int i = 0; i < threads.length; i++) {
            threads[i] = new Thread(() -> {
                for (int n = 0; n < 100000; n++) counter.incrementAndGet();
            }, "atomic" + i);
            threads[i].start();
        }
        for (Thread t : threads) t.join();
        System.out.println(counter.get());
    }
}

// A volatile write publishes the plain writes before it to a thread that reads the volatile.
class VolatilePublication {
    static int data;
    static volatile boolean ready;

    public static void main(String[] args) throws Exception {
        final int[] seen = new int[1];
        Thread reader = new Thread(() -> {
            while (!ready) Thread.yield();
            seen[0] = data;
        }, "reader");
        reader.start();
        data = 42;
        ready = true;
        reader.join();
        System.out.println(seen[0]);
    }
}

// A static synchronized method locks its Class object: while main holds synchronized (X.class),
// another thread calling the static synchronized method waits.
class StaticSyncLocksClass {
    static class Guarded {
        static int calls;

        static synchronized void call() {
            calls++;
        }
    }

    static volatile boolean trying;

    public static void main(String[] args) throws Exception {
        Thread t;
        int duringHold;
        synchronized (Guarded.class) {
            t = new Thread(() -> {
                trying = true;
                Guarded.call();
            }, "caller");
            t.start();
            while (!trying) Thread.sleep(1);
            Thread.sleep(200);
            duringHold = Guarded.calls;
        }
        t.join();
        System.out.println(duringHold + " " + Guarded.calls + " " + Thread.holdsLock(Guarded.class));
    }
}
