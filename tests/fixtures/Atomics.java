import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

// Atomic accesses on one thread, against the JVM: Unsafe compare-and-set/exchange and volatile
// get/put (through java.util.concurrent.atomic), and every VarHandle access mode with a memory
// order, for each value type. One thread cannot show ordering; these pin the results (old
// values, success flags, bitwise float equality, store checks). Threads are covered by
// tests/sync.rs. If these differ, look at unsafe_intrinsic and var_handle_access (with
// var_handle_order) in src/cgen.rs and jabs_atomic_add_float/double in runtime/jabs_rt.h.
public class Atomics {
    int i;
    long j;
    float f;
    double d;
    boolean z;
    byte b;
    short s;
    char c;
    Object ref;
    static long staticLong;

    static final VarHandle I, J, F, D, Z, B, S, C, REF, STATIC_LONG, OBJECTS, LONGS;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            I = l.findVarHandle(Atomics.class, "i", int.class);
            J = l.findVarHandle(Atomics.class, "j", long.class);
            F = l.findVarHandle(Atomics.class, "f", float.class);
            D = l.findVarHandle(Atomics.class, "d", double.class);
            Z = l.findVarHandle(Atomics.class, "z", boolean.class);
            B = l.findVarHandle(Atomics.class, "b", byte.class);
            S = l.findVarHandle(Atomics.class, "s", short.class);
            C = l.findVarHandle(Atomics.class, "c", char.class);
            REF = l.findVarHandle(Atomics.class, "ref", Object.class);
            STATIC_LONG = l.findStaticVarHandle(Atomics.class, "staticLong", long.class);
            OBJECTS = MethodHandles.arrayElementVarHandle(Object[].class);
            LONGS = MethodHandles.arrayElementVarHandle(long[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // java.util.concurrent.atomic: Unsafe compareAndSet/compareAndExchange/getAndAdd loops and
    // volatile get/put.
    static long atomicIntegerUpdates(int start) {
        AtomicInteger a = new AtomicInteger(start);
        long r = a.getAndIncrement();
        r = r * 31 + a.incrementAndGet();
        r = r * 31 + a.getAndAdd(-7);
        r = r * 31 + (a.compareAndSet(start, 1) ? 1 : 0);
        r = r * 31 + (a.compareAndSet(start - 5, 99) ? 1 : 0);
        r = r * 31 + a.compareAndExchange(99, 100);
        r = r * 31 + a.compareAndExchange(5, 6);
        r = r * 31 + a.getAndSet(Integer.MIN_VALUE);
        r = r * 31 + a.decrementAndGet();
        r = r * 31 + a.accumulateAndGet(3, Math::max);
        a.setRelease(17);
        r = r * 31 + a.getAcquire();
        a.lazySet(18);
        r = r * 31 + a.getPlain() + a.getOpaque();
        return r;
    }
    static long atomicLongUpdates(long start) {
        AtomicLong a = new AtomicLong(start);
        long r = a.getAndIncrement();
        r = r * 31 + a.addAndGet(Long.MAX_VALUE);
        r = r * 31 + (a.compareAndSet(start, 3) ? 1 : 0);
        r = r * 31 + a.compareAndExchange(a.get(), -9);
        r = r * 31 + a.getAndUpdate(x -> x * 3);
        r = r * 31 + (a.weakCompareAndSetVolatile(-27, 4) ? 1 : 0);
        return r * 31 + a.get();
    }
    static int atomicReferenceUpdates() {
        String first = "first", second = "second";
        AtomicReference<String> a = new AtomicReference<>(first);
        int r = a.compareAndSet(first, second) ? 1 : 0;
        r = r * 10 + (a.compareAndSet(first, null) ? 1 : 0);
        r = r * 10 + (a.compareAndExchange(second, null) == second ? 1 : 0);
        r = r * 10 + (a.getAndSet(first) == null ? 1 : 0);
        return r * 10 + (a.get() == first ? 1 : 0);
    }
    static int atomicBooleanAndArray() {
        AtomicBoolean flag = new AtomicBoolean();
        int r = flag.compareAndSet(false, true) ? 1 : 0;
        r = r * 10 + (flag.compareAndSet(false, true) ? 1 : 0);
        r = r * 10 + (flag.getAndSet(false) ? 1 : 0);
        AtomicIntegerArray array = new AtomicIntegerArray(3);
        array.set(1, 40);
        r = r * 100 + array.getAndAdd(1, 2) + array.incrementAndGet(2);
        return r * 10 + (array.compareAndSet(1, 42, 0) ? 1 : 0);
    }
    static int atomicIntegerArrayOutOfBounds(int index) { return new AtomicIntegerArray(2).getAndIncrement(index); }

    // VarHandle access modes with memory orders, per value type.
    static long varHandleIntModes(int start) {
        Atomics a = new Atomics();
        I.setVolatile(a, start);
        long r = (int) I.getAcquire(a);
        I.setRelease(a, start + 1);
        r = r * 31 + (int) I.getOpaque(a);
        I.setOpaque(a, start + 2);
        r = r * 31 + (int) I.getVolatile(a);
        r = r * 31 + (I.compareAndSet(a, start + 2, 7) ? 1 : 0);
        r = r * 31 + (I.weakCompareAndSetPlain(a, 0, 1) ? 1 : 0);
        r = r * 31 + (int) I.compareAndExchangeAcquire(a, 7, 8);
        r = r * 31 + (int) I.compareAndExchangeRelease(a, 0, 9);
        r = r * 31 + (int) I.getAndAddAcquire(a, Integer.MAX_VALUE);
        r = r * 31 + (int) I.getAndBitwiseXorRelease(a, 0x5a5a5a5a);
        r = r * 31 + (int) I.getAndBitwiseOr(a, 0x10);
        r = r * 31 + (int) I.getAndBitwiseAndAcquire(a, 0xff00ff);
        r = r * 31 + (int) I.getAndSetRelease(a, -1);
        return r * 31 + a.i;
    }
    static long varHandleLongModes(long start) {
        Atomics a = new Atomics();
        J.setRelease(a, start);
        long r = (long) J.getAndAdd(a, 1L << 40);
        r = r * 31 + (long) J.compareAndExchange(a, start + (1L << 40), -1L);
        r = r * 31 + (J.weakCompareAndSetRelease(a, -1L, Long.MIN_VALUE) ? 1 : 0);
        r = r * 31 + (long) J.getAndBitwiseAnd(a, 0xffffL);
        STATIC_LONG.setVolatile(start);
        r = r * 31 + (long) STATIC_LONG.getAndAddRelease(5L);
        r = r * 31 + (STATIC_LONG.compareAndSet(start + 5, 0L) ? 1 : 0);
        return r * 31 + a.j + staticLong;
    }
    /** Narrow types: getAndAdd wraps in the field's width; booleans take the bitwise modes. */
    static long varHandleNarrowModes() {
        Atomics a = new Atomics();
        B.setVolatile(a, (byte) 120);
        long r = (byte) B.getAndAdd(a, (byte) 10);
        r = r * 31 + a.b;
        S.setVolatile(a, (short) -2);
        r = r * 31 + (short) S.getAndBitwiseXor(a, (short) 0x7fff);
        r = r * 31 + a.s;
        C.setVolatile(a, (char) 0xfffe);
        r = r * 31 + (char) C.getAndAdd(a, (char) 3);
        r = r * 31 + a.c;
        r = r * 31 + ((boolean) Z.getAndBitwiseOr(a, true) ? 1 : 0);
        r = r * 31 + ((boolean) Z.getAndBitwiseXor(a, true) ? 1 : 0);
        r = r * 31 + (Z.compareAndSet(a, false, true) ? 1 : 0);
        return r * 31 + (a.z ? 1 : 0);
    }
    /** Float/double compare-and-set compares bits: -0.0 is not 0.0, a NaN matches itself. */
    static long varHandleFloatingModes() {
        Atomics a = new Atomics();
        D.setVolatile(a, -0.0);
        long r = D.compareAndSet(a, 0.0, 1.0) ? 1 : 0;
        r = r * 10 + (D.compareAndSet(a, -0.0, Double.NaN) ? 1 : 0);
        r = r * 10 + (D.compareAndSet(a, Double.NaN, 2.5) ? 1 : 0);
        r = r * 1000 + (long) (double) D.getAndAdd(a, 0.25);
        r = r * 1000 + Double.doubleToRawLongBits((double) D.getAcquire(a)) % 997;
        F.setRelease(a, 1.5f);
        r = r * 1000 + Float.floatToRawIntBits((float) F.getAndAddAcquire(a, -4f)) % 991;
        r = r * 1000 + Float.floatToRawIntBits((float) F.compareAndExchange(a, -2.5f, Float.NaN)) % 983;
        return r * 10 + (Float.isNaN(a.f) ? 1 : 0);
    }
    static int varHandleReferenceModes() {
        Atomics a = new Atomics();
        Object x = new Object(), y = "y";
        REF.setRelease(a, x);
        int r = REF.compareAndSet(a, y, x) ? 1 : 0;
        r = r * 10 + (REF.compareAndExchangeAcquire(a, x, y) == x ? 1 : 0);
        r = r * 10 + (REF.getAndSet(a, null) == y ? 1 : 0);
        r = r * 10 + (REF.getVolatile(a) == null ? 1 : 0);
        Object[] objects = new Object[2];
        r = r * 10 + (OBJECTS.compareAndSet(objects, 1, null, x) ? 1 : 0);
        return r * 10 + (OBJECTS.getAcquire(objects, 1) == x ? 1 : 0);
    }
    /** A compare-and-set into a String[] seen as Object[] checks the new value's type even when
     * the comparison would fail: ArrayStoreException. */
    static int varHandleArrayStoreChecked() {
        Object[] strings = new String[1];
        return OBJECTS.compareAndSet(strings, 0, "not there", Integer.valueOf(1)) ? 1 : 0;
    }
    static long varHandleArrayIndexChecked(int index) { return (long) LONGS.getAndAdd(new long[2], index, 1L); }
}
