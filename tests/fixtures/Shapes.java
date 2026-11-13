// Several classes compiled as one unit: inheritance, overriding, super calls,
// abstract dispatch, instanceof/checkcast, cross-class statics, and the JVM's
// initialization order (superclass first), recorded by every static initializer.
interface Tagged {
    int TAG = 7;
}

class Log {
    static long trace;

    static void hit(int id) { trace = trace * 10 + id; }
}

abstract class Shape implements Tagged {
    static int created;
    static { Log.hit(1); }
    protected int id;

    Shape() { id = ++created; }

    abstract double area();
    double scaled(double k) { return area() * k; }
    int kind() { return 0; }
    static double total(Shape a, Shape b) { return a.area() + b.area(); }
}

class Circle extends Shape {
    static { Log.hit(2); }
    double r;

    Circle(double r) { this.r = r; }

    double area() { return 3.0 * r * r; }
    int kind() { return 1; }
}

class Rect extends Shape {
    static { Log.hit(3); }
    double w, h;

    Rect(double w, double h) { this.w = w; this.h = h; }

    double area() { return w * h; }
    int kind() { return 2 + super.kind(); }
}

final class Square extends Rect {
    static { Log.hit(4); }

    Square(double side) { super(side, side); }

    int kind() { return 10 + super.kind(); }
}

class Boom {
    static int zero = 0;
    static int value = 1 / zero;
}

class Child extends Boom {
    static int get() { return 5; }
}

public class Shapes {
    static long initOrder() { new Square(2); return Log.trace; }
    static Shape make(int which, double x) {
        switch (which) {
            case 0: return new Circle(x);
            case 1: return new Rect(x, x + 1);
            default: return new Square(x);
        }
    }
    static double area(int which, double x) { return make(which, x).area(); }
    static int kind(int which) { return make(which, 1).kind(); }
    static int isRect(int which) { return make(which, 1) instanceof Rect ? 1 : 0; }
    static int castToRect(int which) { Rect r = (Rect) make(which, 2); return (int) r.w; }
    static int ids(int n) {
        int last = 0;
        for (int i = 0; i < n; i++) last = make(i % 3, 1).id;
        return last + Shape.created * 1000;
    }
    static double total(int a, int b) { return Shape.total(make(a, 1.5), make(b, 2.5)); }
    static double scaled(int which, double k) { return make(which, 3).scaled(k); }
    static int tag() { return Tagged.TAG; }
    static long trace() { return Log.trace; }
    static int childGet() { return Child.get(); }
}
