// Fixture for tests/executed_roots.rs. ExecutedRootsMain reaches ExecutedRootsFactory.make only
// through Method.invoke with a name computed at run time (as log4j calls its plugins' factory
// methods), so no instruction names it: only a method trace of a JVM run can keep it.
import java.lang.reflect.Method;

class ExecutedRootsFactory {
    static String make(String what) {
        return "made " + what;
    }

    // Compiles without java.base in the unit: the unit-level test makes it its one root.
    static int twice(int x) {
        return x * 2;
    }

    // Never runs; a native JABS has no implementation of, so it cannot compile.
    static native void unimplemented();
}

abstract class ExecutedRootsShape {
    abstract int sides();
}

class ExecutedRootsMain {
    public static void main(String[] args) throws Exception {
        // A direct call first, so the factory class is initialized natively either way (as
        // log4j's converter classes are) and the reflective call is the only thing missing.
        System.out.println(ExecutedRootsFactory.twice(21));
        String name = new StringBuilder("ekam").reverse().toString();
        Method make = ExecutedRootsFactory.class.getDeclaredMethod(name, String.class);
        System.out.println(make.invoke(null, "x"));
    }
}
