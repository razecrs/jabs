// Differential fixture: every method here must produce bit-identical results
// on the reference JVM and as JABS-generated native code.
public class Arith {
    static int add(int a, int b) { return a + b; }
    static int sub(int a, int b) { return a - b; }
    static int mul(int a, int b) { return a * b; }
    static int div(int a, int b) { return a / b; }
    static int rem(int a, int b) { return a % b; }
    static int neg(int a) { return -a; }
    static int shl(int a, int n) { return a << n; }
    static int shr(int a, int n) { return a >> n; }
    static int ushr(int a, int n) { return a >>> n; }
    static int bits(int a, int b) { return (a & b) ^ (a | ~b); }

    static long ladd(long a, long b) { return a + b; }
    static long lmul(long a, long b) { return a * b; }
    static long ldiv(long a, long b) { return a / b; }
    static long lrem(long a, long b) { return a % b; }
    static long lshr(long a, int n) { return a >> n; }
    static long lushr(long a, int n) { return a >>> n; }
    static long lshl(long a, int n) { return a << n; }
    static int lcmp(long a, long b) { return a < b ? -1 : (a == b ? 0 : 1); }

    static float fdiv(float a, float b) { return a / b; }
    static float frem(float a, float b) { return a % b; }
    static double drem(double a, double b) { return a % b; }
    static double dmix(double a, float b, int c, long d) { return a * b + c - d; }
    static float fneg(float a) { return -a; }
    static int fless(float a, float b) { return a < b ? 1 : 0; }
    static int fgreater(float a, float b) { return a > b ? 1 : 0; }
    static int dequal(double a, double b) { return a == b ? 1 : 0; }

    static int f2i(float a) { return (int) a; }
    static long f2l(float a) { return (long) a; }
    static int d2i(double a) { return (int) a; }
    static long d2l(double a) { return (long) a; }
    static float l2f(long a) { return (float) a; }
    static float d2f(double a) { return (float) a; }
    static int l2i(long a) { return (int) a; }
    static byte i2b(int a) { return (byte) a; }
    static char i2c(int a) { return (char) a; }
    static short i2s(int a) { return (short) a; }

    static int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
    static long sumTo(int n) {
        long total = 0;
        for (int i = 1; i <= n; i++) total += i;
        return total;
    }
    static int collatz(long n) {
        int steps = 0;
        while (n != 1) {
            n = (n & 1) == 0 ? n / 2 : 3 * n + 1;
            steps++;
        }
        return steps;
    }
    static int dense(int x) {
        switch (x) {
            case 0: return 10;
            case 1: return 11;
            case 2: return 12;
            case 3: return 13;
            default: return -1;
        }
    }
    static int sparse(int x) {
        switch (x) {
            case -1000000: return 1;
            case 7: return 2;
            case Integer.MIN_VALUE: return 3;
            case 1 << 20: return 4;
            default: return 0;
        }
    }
    static int gcd(int a, int b) { return b == 0 ? a : gcd(b, a % b); }
    static int divChain(int a, int b) { return add(div(a, b), 1); }
    static double horner(double x) {
        double r = 0;
        for (int i = 10; i >= 0; i--) r = r * x + i * 0.5;
        return r;
    }
}
