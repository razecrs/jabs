import java.lang.ref.Cleaner;
import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.LockSupport;

class GcStressCases {
    static final class Box {
        final int v;
        final String s;
        Box(int v, String s) {
            this.v = v;
            this.s = s;
        }
    }

    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("churn")) {
            churn();
        } else if (mechanism.equals("churnBounded")) {
            churnBounded();
        } else if (mechanism.equals("weakQueue")) {
            weakQueue();
        } else if (mechanism.equals("cleaner")) {
            cleaner();
        } else if (mechanism.equals("multithreadedStw")) {
            multithreadedStw();
        } else {
            throw new IllegalArgumentException("unknown mechanism: " + mechanism);
        }
    }

    static long runChurnWorkload() {
        byte[][] largeRing = new byte[16][];
        Box[] smallRing = new Box[64];
        for (int i = 0; i < 1800; i++) {
            byte[] buf = new byte[96 * 1024];
            buf[0] = (byte) i;
            buf[buf.length - 1] = (byte) (i >> 3);
            largeRing[i & 15] = buf;
            for (int j = 0; j < 400; j++) {
                smallRing[(i + j) & 63] = new Box(i * 400 + j, "k" + (j & 31));
            }
        }
        long sum = 0;
        for (byte[] b : largeRing) {
            sum += (b[0] & 0xff) + (b[b.length - 1] & 0xff) + b.length;
        }
        for (Box b : smallRing) {
            sum += b.v + b.s.length();
        }
        return sum;
    }

    static void churn() {
        long sum = runChurnWorkload();
        System.gc();
        System.out.println("churn " + sum);
    }

    static void churnBounded() {
        long sum = runChurnWorkload();
        System.gc();
        Runtime rt = Runtime.getRuntime();
        long total = rt.totalMemory();
        long free = rt.freeMemory();
        long max = rt.maxMemory();
        boolean ok = total > 0 && total < 96L * 1024L * 1024L && max >= total && total >= free;
        System.out.println("churnBounded " + sum + " ok=" + ok + " totalMB=" + (total >> 20));
    }

    static WeakReference<byte[]> makeWeak(ReferenceQueue<byte[]> q, boolean[] seenAlive) {
        byte[] payload = new byte[128 * 1024];
        payload[0] = 42;
        WeakReference<byte[]> ref = new WeakReference<>(payload, q);
        seenAlive[0] = (ref.get() != null && ref.get()[0] == 42);
        return ref;
    }

    static PhantomReference<byte[]> makePhantom(ReferenceQueue<byte[]> q) {
        byte[] payload = new byte[64 * 1024];
        return new PhantomReference<>(payload, q);
    }

    static void clearStackFrame() {
        byte[] pad = new byte[256];
        pad[0] = 1;
        if (pad[1] == 99) System.out.println("never");
    }

    static void weakQueue() throws Exception {
        ReferenceQueue<byte[]> wq = new ReferenceQueue<>();
        ReferenceQueue<byte[]> pq = new ReferenceQueue<>();
        boolean[] seenAlive = new boolean[1];
        WeakReference<byte[]> wr = makeWeak(wq, seenAlive);
        PhantomReference<byte[]> pr = makePhantom(pq);
        clearStackFrame();

        for (int i = 0; i < 64 && wr.get() != null; i++) {
            byte[] trash = new byte[256 * 1024];
            if (trash.length == 0) System.out.println("never");
            System.gc();
            Thread.sleep(10);
        }
        Reference<? extends byte[]> wPolled = wq.remove(1500);
        Reference<? extends byte[]> pPolled = null;
        for (int i = 0; i < 64 && pPolled == null; i++) {
            byte[] trash = new byte[128 * 1024];
            if (trash.length == 0) System.out.println("never");
            System.gc();
            Thread.sleep(10);
            pPolled = pq.poll();
        }
        System.out.println(
            "weak " + seenAlive[0] + " cleared=" + (wr.get() == null) + " enqueued=" + (wPolled == wr) + " phantom=" + (pPolled == pr)
        );
    }

    static void registerCleaned(Cleaner cleaner, AtomicBoolean cleaned) {
        Object resource = new Object();
        cleaner.register(resource, () -> cleaned.set(true));
    }

    static void cleaner() throws Exception {
        Cleaner cleaner = Cleaner.create();
        AtomicBoolean cleaned = new AtomicBoolean(false);
        registerCleaned(cleaner, cleaned);
        clearStackFrame();

        long deadline = System.currentTimeMillis() + 2000L;
        while (!cleaned.get() && System.currentTimeMillis() < deadline) {
            byte[] trash = new byte[256 * 1024];
            if (trash.length == 0) System.out.println("never");
            System.gc();
            Thread.sleep(20);
        }
        System.out.println("cleaner=" + cleaned.get());
    }

    static void multithreadedStw() throws Exception {
        final AtomicBoolean stop = new AtomicBoolean(false);
        final Object monitor = new Object();
        final long[] sleeperSum = new long[1];

        Thread allocator = new Thread(() -> {
            byte[][] keep = new byte[8][];
            int idx = 0;
            while (!stop.get()) {
                byte[] b = new byte[16 * 1024];
                b[0] = (byte) idx;
                keep[idx & 7] = b;
                String s = "msg-" + (idx & 255);
                if (s.length() < 0) System.out.println(s);
                idx++;
            }
            if (keep[0] == null && idx < 0) System.out.println("impossible");
        }, "gc-alloc");

        Thread sleeper = new Thread(() -> {
            long sum = 0;
            for (int i = 1; i <= 40; i++) {
                sum += i;
                try {
                    Thread.sleep(3);
                } catch (InterruptedException ignored) {}
            }
            sleeperSum[0] = sum;
        }, "gc-sleep");

        Thread parker = new Thread(() -> {
            while (!stop.get()) {
                LockSupport.parkNanos(2_000_000L);
            }
        }, "gc-park");

        Thread waiter = new Thread(() -> {
            synchronized (monitor) {
                while (!stop.get()) {
                    try {
                        monitor.wait(3);
                    } catch (InterruptedException ignored) {}
                }
            }
        }, "gc-wait");

        allocator.start();
        sleeper.start();
        parker.start();
        waiter.start();

        sleeper.join();
        stop.set(true);
        LockSupport.unpark(parker);
        synchronized (monitor) {
            monitor.notifyAll();
        }
        allocator.join();
        parker.join();
        waiter.join();

        System.out.println("stw sum=" + sleeperSum[0]);
    }
}
