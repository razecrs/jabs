// Thread programs for the differential test's main() mode, one mechanism each. Results are
// printed by main after the other threads have ended, so the output never depends on scheduling.
// Waiting for a thread polls isAlive (the VM's alive marker, Thread.eetop) rather than join, so
// these do not depend on monitors and wait/notify.
final class Threads {
    static void awaitEnd(Thread t) throws InterruptedException {
        while (t.isAlive()) Thread.sleep(1);
    }
}

// Thread.start runs run() on another thread, whose currentThread() is its own Thread; the alive
// marker and getState follow the thread from NEW through TERMINATED.
class StartRunsOnOwnThread {
    static Thread seen;

    public static void main(String[] args) throws Exception {
        Thread main = Thread.currentThread();
        Thread t = new Thread(() -> seen = Thread.currentThread(), "worker");
        System.out.println(t.getState() + " " + t.isAlive());
        t.start();
        Threads.awaitEnd(t);
        System.out.println(t.getState() + " " + t.isAlive());
        System.out.println((seen == t) + " " + (seen != main) + " " + seen.getName());
        System.out.println(main.getName() + " " + main.isAlive() + " " + (Thread.currentThread() == main));
    }
}

// Starting a thread twice throws IllegalThreadStateException, while it runs and after it ended.
class StartTwiceThrows {
    static volatile boolean release;

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            while (!release) {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "worker");
        t.start();
        try {
            t.start();
            System.out.println("started twice while running");
        } catch (IllegalThreadStateException e) {
            System.out.println("running: IllegalThreadStateException");
        }
        release = true;
        Threads.awaitEnd(t);
        try {
            t.start();
            System.out.println("started again after the end");
        } catch (IllegalThreadStateException e) {
            System.out.println("ended: IllegalThreadStateException");
        }
    }
}

// An exception escaping another thread's run() is printed by the uncaught-exception handler
// ("Exception in thread "worker" ...") and ends only that thread: main goes on, status 0.
class UncaughtInOtherThread {
    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            throw new IllegalStateException("boom");
        }, "worker");
        t.start();
        Threads.awaitEnd(t);
        System.out.println("main continues " + t.getState());
    }
}

// The process waits for a non-daemon thread after main returns (DestroyJavaVM); main's own
// Thread has ended by then (not alive), which the worker waits to see.
class NonDaemonOutlivesMain {
    public static void main(String[] args) {
        final Thread main = Thread.currentThread();
        Thread t = new Thread(() -> {
            try {
                Threads.awaitEnd(main);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("worker saw main end");
        }, "worker");
        t.start();
        System.out.println("main returns");
    }
}

// A daemon thread does not keep the process alive: it ends when main returns (this one would
// otherwise sleep forever).
class DaemonDoesNotKeepProcess {
    public static void main(String[] args) {
        Thread t = new Thread(() -> {
            while (true) {
                try {
                    Thread.sleep(10);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "daemon");
        t.setDaemon(true);
        t.start();
        System.out.println("main returns");
    }
}

// Thread.interrupt ends a sleep with InterruptedException("sleep interrupted") and clears the
// interrupt status, whether it comes before or during the sleep.
class SleepInterrupted {
    static String result;

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(60000);
                result = "slept a minute";
            } catch (InterruptedException e) {
                result = e.getMessage() + " " + Thread.currentThread().isInterrupted();
            }
        }, "sleeper");
        t.start();
        t.interrupt();
        Threads.awaitEnd(t);
        System.out.println(result);
    }
}

// Class initialization (JVMS 5.5) runs once although several threads need the class at once;
// the others wait for it and all see its result.
class ClassInitOnceUnderThreads {
    static class Slow {
        static int runs;
        static final int VALUE;

        static {
            runs++;
            try {
                Thread.sleep(100);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            VALUE = 42;
        }
    }

    public static void main(String[] args) throws Exception {
        final int[] seen = new int[8];
        Thread[] threads = new Thread[seen.length];
        for (int i = 0; i < threads.length; i++) {
            final int k = i;
            threads[i] = new Thread(() -> seen[k] = Slow.VALUE, "reader" + i);
        }
        for (Thread t : threads) t.start();
        for (Thread t : threads) Threads.awaitEnd(t);
        StringBuilder all = new StringBuilder();
        for (int v : seen) all.append(v).append(' ');
        System.out.println(all + "runs=" + Slow.runs);
    }
}

// A class whose initialization failed on one thread is erroneous for every thread: the next
// thread gets NoClassDefFoundError, not a second run.
class ClassInitFailureOtherThread {
    // Outside Fails, which is unusable once its initialization failed.
    static int runs;

    static class Fails {
        static final int VALUE;

        static {
            runs++;
            VALUE = 1 / (runs - 1);
        }
    }

    static String other;

    public static void main(String[] args) throws Exception {
        try {
            System.out.println(Fails.VALUE);
        } catch (ExceptionInInitializerError e) {
            System.out.println("main: ExceptionInInitializerError");
        }
        Thread t = new Thread(() -> {
            try {
                other = "value " + Fails.VALUE;
            } catch (NoClassDefFoundError e) {
                other = "NoClassDefFoundError";
            }
        }, "worker");
        t.start();
        Threads.awaitEnd(t);
        System.out.println("worker: " + other + " runs=" + runs);
    }
}

// Stress: many short-lived threads, each writing its own slot; a fixed count, results read after
// every thread has ended.
class ManyShortThreads {
    public static void main(String[] args) throws Exception {
        final long[] squares = new long[200];
        Thread[] threads = new Thread[squares.length];
        for (int i = 0; i < threads.length; i++) {
            final int k = i;
            threads[i] = new Thread(() -> squares[k] = (long) k * k, "short" + i);
            threads[i].start();
        }
        long sum = 0;
        for (int i = 0; i < threads.length; i++) {
            Threads.awaitEnd(threads[i]);
            sum += squares[i];
        }
        System.out.println(threads.length + " threads, sum " + sum);
    }
}

// Runtime.availableProcessors reports the machine's processors (at least one).
class AvailableProcessors {
    public static void main(String[] args) {
        System.out.println(Runtime.getRuntime().availableProcessors() >= 1);
    }
}
