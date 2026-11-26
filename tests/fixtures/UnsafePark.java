// LockSupport, Reference, and lambda differential cases for tests/unsafe_park.rs:
// `UnsafePark <mechanism>` runs on the reference JVM and natively so stdout, exit status, and
// uncaught exceptions are compared line by line.
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

class UnsafePark {
    static volatile boolean ready;
    static volatile boolean done;
    static volatile boolean workerInterrupted;

    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("unparkBeforePark")) unparkBeforePark();
        else if (mechanism.equals("parkAndUnpark")) parkAndUnpark();
        else if (mechanism.equals("parkNanosAndUntil")) parkNanosAndUntil();
        else if (mechanism.equals("parkInterrupt")) parkInterrupt();
        else if (mechanism.equals("locksAndReferences")) locksAndReferences();
        else throw new IllegalArgumentException("unknown mechanism: " + mechanism);
    }

    static void unparkBeforePark() throws Exception {
        Thread main = Thread.currentThread();
        // Multiple unparks coalesce into a single permit.
        LockSupport.unpark(main);
        LockSupport.unpark(main);
        LockSupport.park();
        // Permit was consumed, so a timed park waits for the timeout.
        long t0 = System.nanoTime();
        LockSupport.parkNanos(120_000_000L);
        long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("consumed permit: " + (elapsedMs >= 50));

        // unpark(null) and unpark(unstartedThread) are safe no-ops.
        LockSupport.unpark(null);
        final boolean[] waited = new boolean[1];
        Thread unstarted = new Thread(() -> {
            long s = System.nanoTime();
            LockSupport.parkNanos(120_000_000L);
            waited[0] = (System.nanoTime() - s) / 1_000_000L >= 50;
        });
        LockSupport.unpark(unstarted);
        unstarted.start();
        unstarted.join();
        System.out.println("unstarted ignored: " + waited[0]);

        // unpark on a terminated thread is also a safe no-op.
        LockSupport.unpark(unstarted);
        System.out.println("terminated ignored: ok");
    }

    static void parkAndUnpark() throws Exception {
        ready = false;
        done = false;
        Thread worker = new Thread(() -> {
            ready = true;
            while (!done) {
                LockSupport.park();
            }
        }, "parker");
        worker.start();
        while (!ready || worker.getState() != Thread.State.WAITING) {
            Thread.sleep(1);
        }
        done = true;
        LockSupport.unpark(worker);
        worker.join();
        System.out.println("worker unparked: " + done);

        final Thread main = Thread.currentThread();
        ready = false;
        done = false;
        Thread unparker = new Thread(() -> {
            while (!ready) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException ignored) {
                }
            }
            try {
                Thread.sleep(20);
            } catch (InterruptedException ignored) {
            }
            done = true;
            LockSupport.unpark(main);
        }, "unparker");
        unparker.start();
        ready = true;
        while (!done) {
            LockSupport.park();
        }
        unparker.join();
        System.out.println("main unparked: " + done);
    }

    static void parkNanosAndUntil() throws Exception {
        LockSupport.parkNanos(0L);
        LockSupport.parkNanos(-100L);
        long t0 = System.nanoTime();
        LockSupport.parkNanos(120_000_000L);
        System.out.println("parkNanos waited: " + ((System.nanoTime() - t0) >= 50_000_000L));

        LockSupport.parkUntil(0L);
        LockSupport.parkUntil(System.currentTimeMillis() - 100L);
        long startMs = System.currentTimeMillis();
        long deadline = startMs + 150L;
        LockSupport.parkUntil(deadline);
        System.out.println("parkUntil reached: " + (System.currentTimeMillis() >= deadline - 10L));

        final Thread main = Thread.currentThread();
        ready = false;
        Thread waker = new Thread(() -> {
            while (!ready) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException ignored) {
                }
            }
            try {
                Thread.sleep(20);
            } catch (InterruptedException ignored) {
            }
            LockSupport.unpark(main);
        });
        waker.start();
        long far = System.currentTimeMillis() + 10_000L;
        long before = System.nanoTime();
        ready = true;
        LockSupport.parkUntil(far);
        waker.join();
        System.out.println("parkUntil woken early: " + ((System.nanoTime() - before) < 5_000_000_000L));
    }

    static void parkInterrupt() throws Exception {
        Thread.currentThread().interrupt();
        LockSupport.park();
        System.out.println("pre-interrupted kept: " + Thread.currentThread().isInterrupted());
        System.out.println("cleared: " + Thread.interrupted() + " " + Thread.currentThread().isInterrupted());

        ready = false;
        workerInterrupted = false;
        Thread worker = new Thread(() -> {
            ready = true;
            LockSupport.park();
            workerInterrupted = Thread.currentThread().isInterrupted();
            // Confirm interrupt status is still intact for subsequent blocking operations.
            try {
                Thread.sleep(10);
                workerInterrupted = false;
            } catch (InterruptedException expected) {
            }
        }, "interrupted-parker");
        worker.start();
        while (!ready || worker.getState() != Thread.State.WAITING) {
            Thread.sleep(1);
        }
        worker.interrupt();
        worker.join();
        System.out.println("interrupted while parked: " + workerInterrupted);
    }

    static void locksAndReferences() throws Exception {
        final ReentrantLock lock = new ReentrantLock();
        final CountDownLatch latch = new CountDownLatch(2);
        final int[] sum = new int[1];
        for (int i = 0; i < 2; i++) {
            final int add = i + 10;
            new Thread(() -> {
                lock.lock();
                try {
                    sum[0] += add;
                } finally {
                    lock.unlock();
                }
                latch.countDown();
            }).start();
        }
        latch.await();
        System.out.println("lock sum: " + sum[0]);

        ReferenceQueue<Object> queue = new ReferenceQueue<>();
        Object referent = new Object();
        WeakReference<Object> ref = new WeakReference<>(referent, queue);
        System.out.println("ref present: " + (ref.get() == referent) + " polled: " + (queue.poll() == null));
    }
}
