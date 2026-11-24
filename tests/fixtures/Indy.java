import java.io.Serializable;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.function.Function;
import java.util.function.IntBinaryOperator;
import java.util.function.IntSupplier;
import java.util.function.IntUnaryOperator;
import java.util.function.Supplier;
import java.util.function.ToIntFunction;
import java.util.function.ToLongFunction;

// invokedynamic, which JABS translates at compile time (compiled with --release 21): lambdas and
// method references (captures, boxing, unboxing, widening, constructors, altMetafactory), string
// concatenation (with recipe constants), records' equals/hashCode/toString and pattern switches.
public class Indy {
    record Point(int x, double y, String name) {}
    record Empty() {}
    sealed interface Shape permits Sq, Ci {}
    record Sq(int side) implements Shape {}
    record Ci(int r) implements Shape {}
    enum Color { RED, GREEN, BLUE }

    static Supplier<String> site() { return () -> "x"; }
    // A non-capturing lambda yields one shared instance per call site on HotSpot.
    static int sameInstance() { return site() == site() ? 1 : 0; }
    static int capture(int k) {
        IntUnaryOperator f = x -> x * k + 1;
        return f.applyAsInt(5);
    }
    static int boxing() {
        Function<Integer, Integer> f = x -> x + 1;
        return f.apply(41);
    }
    static int staticRef() {
        IntBinaryOperator m = Math::max;
        return m.applyAsInt(3, 9);
    }
    static int boundRef() {
        String s = "hello";
        IntSupplier len = s::length;
        return len.getAsInt();
    }
    static int unboundRef() {
        ToIntFunction<String> len = String::length;
        return len.applyAsInt("abcd");
    }
    static int constructorRef() {
        Supplier<ArrayList<String>> make = ArrayList::new;
        ArrayList<String> list = make.get();
        list.add("a");
        list.add("b");
        return list.size();
    }
    // The implementation returns int; the interface returns long: LambdaMetafactory widens.
    static long widening() {
        ToLongFunction<Integer> f = Integer::intValue;
        return f.applyAsLong(1 << 30) * 4;
    }
    static int serializable() {
        Comparator<String> c = (Comparator<String> & Serializable) (a, b) -> a.length() - b.length();
        return c.compare("aaa", "b") + (c instanceof Serializable ? 10 : 0);
    }
    static int sort() {
        List<Integer> l = Arrays.asList(3, 1, 2);
        l.sort((a, b) -> b - a);
        return l.get(0) * 100 + l.get(1) * 10 + l.get(2);
    }
    static int concat() {
        int i = -7;
        long j = 1L << 40;
        char c = '€';
        double d = -0.0;
        boolean z = true;
        Object o = null;
        float f = 1.5f;
        byte b = -3;
        String s = "i=" + i + " j=" + j + c + d + z + o + f + b + ' ' + new Point(1, 2.0, "p");
        return s.hashCode() * 31 + s.length();
    }
    // Text containing \u0001 or \u0002 travels as a recipe constant, not literally.
    static int concatConstants(int x) {
        String s = "\u0001" + x + "\u0002" + x;
        return s.length() * 1000 + s.charAt(0) * 10 + s.charAt(2);
    }
    static int recordToString() { return new Point(1, 2.5, "p").toString().hashCode(); }
    static int recordToStringLength() { return new Point(-1, 0.1, null).toString().length() * 100 + new Empty().toString().length(); }
    static int recordHash() { return new Point(1, 2.5, "p").hashCode() ^ new Point(0, -0.0, null).hashCode(); }
    static int recordEquals() {
        Point a = new Point(1, 2.5, "p");
        return (a.equals(new Point(1, 2.5, "p")) ? 1 : 0)
                + (a.equals(new Point(2, 2.5, "p")) ? 10 : 0)
                + (a.equals(null) ? 100 : 0)
                + (a.equals("p") ? 1000 : 0)
                + (new Point(1, -0.0, null).equals(new Point(1, 0.0, null)) ? 10000 : 0)
                + (new Point(1, Double.NaN, null).equals(new Point(1, Double.NaN, null)) ? 100000 : 0)
                + (a.equals(a) ? 1000000 : 0);
    }
    static int describe(Object o) {
        return switch (o) {
            case null -> -1;
            case Integer i when i > 10 -> 1;
            case Integer i -> 2;
            case String s -> s.length() * 10;
            case Sq sq -> 100 + sq.side();
            default -> 9;
        };
    }
    static int patterns() {
        return describe(null) + describe(42) * 10 + describe(3) * 100 + describe("abc") * 1000
                + describe(new Sq(4)) * 100000 + describe(new Ci(1)) * 10000000;
    }
    static int area(Shape s) {
        return switch (s) {
            case Sq q -> q.side() * q.side();
            case Ci c -> 3 * c.r() * c.r();
        };
    }
    static int sealedSwitch() { return area(new Sq(3)) * 100 + area(new Ci(2)); }
    static int color(Color c) {
        return switch (c) {
            case RED -> 1;
            case Color k when k.ordinal() > 1 -> 2;
            default -> 3;
        };
    }
    static int enumPatterns() { return color(Color.RED) * 100 + color(Color.GREEN) * 10 + color(Color.BLUE); }
    // A default-package class whose name starts with 'L' is a class, not an `L...;` descriptor.
    static int moduleOfLNamedClass() {
        Module here = Indy.class.getModule();
        return (Launcher.class.getModule() == here ? 1 : 0)
                + (Launcher[].class.getModule() == here ? 10 : 0)
                + (Launcher[][].class.getModule() == here ? 100 : 0);
    }
}

class Launcher {}
