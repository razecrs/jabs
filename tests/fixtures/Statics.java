// Static fields of every primitive type, ConstantValue constants and a <clinit>
// whose effects must be visible exactly as on the JVM, including state kept across calls.
public class Statics {
    static final int K = 41;
    static final long BIG_K = 1L << 50;
    static final double HALF = 0.5;
    static final float NAN = Float.NaN;
    static int counter;
    static long big = 1L << 40;
    static double scale;
    static byte b = (byte) 200;
    static char ch = 65535;
    static short sh;
    static boolean flag;

    static {
        scale = 1.5;
        for (int i = 0; i < 3; i++) counter += i;
        sh = (short) 70000;
        flag = counter == 3;
    }

    static int next() { return ++counter; }
    static long bigPlus(long x) { return big + x + BIG_K; }
    static int k() { return K; }
    static int b() { return b; }
    static int ch() { return ch; }
    static int sh() { return sh; }
    static int setByte(int v) { b = (byte) v; return b; }
    static int setChar(int v) { ch = (char) v; return ch; }
    static int flag() { return flag ? 1 : 0; }
    static double scaled(double x) { return x * scale + HALF; }
    static float nan() { return NAN; }
}
