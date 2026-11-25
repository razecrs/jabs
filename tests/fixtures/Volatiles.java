// Volatile fields: getfield/putfield/getstatic/putstatic on ACC_VOLATILE fields compile to
// sequentially consistent atomic loads and stores (field_load/field_store in src/cgen.rs). One
// thread cannot show ordering, so these pin the values (narrowing on store, long/double bits,
// compound updates, inherited and static fields); the differential test also checks that the
// generated C for readLong/writeLong really is atomic.
public class Volatiles {
    volatile long l;
    volatile double d;
    volatile int i;
    volatile byte b;
    volatile char c;
    volatile short s;
    volatile boolean z;
    volatile float f;
    volatile Object ref;
    static volatile long counter;
    static volatile String name;

    static class Child extends Volatiles {
        volatile int own;
    }

    long readLong() { return l; }
    void writeLong(long v) { l = v; }

    static long longRoundTrip(long v) {
        Volatiles x = new Volatiles();
        x.writeLong(v);
        return x.readLong();
    }
    static long doubleBits(long bits) {
        Volatiles x = new Volatiles();
        x.d = Double.longBitsToDouble(bits);
        return Double.doubleToRawLongBits(x.d);
    }
    /** Stores narrow to the field's type, as for plain fields. */
    static long narrowStores(int v) {
        Volatiles x = new Volatiles();
        x.b = (byte) v;
        x.c = (char) v;
        x.s = (short) v;
        x.z = (v & 1) != 0;
        x.f = v;
        long r = x.b;
        r = r * 31 + x.c;
        r = r * 31 + x.s;
        r = r * 31 + (x.z ? 1 : 0);
        return r * 31 + Float.floatToRawIntBits(x.f);
    }
    /** v++ on a volatile is a read then a write (not atomic as a whole, as in Java). */
    static long compoundUpdates(int n) {
        Volatiles x = new Volatiles();
        for (int k = 0; k < n; k++) {
            x.i++;
            x.l += k;
            counter += 2;
        }
        return x.i * 1_000_000L + x.l + counter;
    }
    static int inheritedAndStatic() {
        Child child = new Child();
        child.i = 4;
        child.own = 5;
        Volatiles asParent = child;
        name = "volatile";
        child.ref = name;
        return asParent.i * 100 + child.own * 10 + (asParent.ref == name ? 1 : 0) + name.length() * 1000;
    }
    static int nullReceiver() {
        Volatiles x = null;
        return x.i;
    }
}
