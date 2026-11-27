import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

public class MethodHandleCases {
    interface Greeter {
        String greet(String who);

        default String tag() {
            return "greeter-default";
        }

        static int version() {
            return 25;
        }
    }

    static class Base {
        public int baseField = 11;
        public static int baseStatic = 22;
        protected int protField = 33;

        public Base() {}

        public String baseMethod(int x) {
            return "base:" + x;
        }

        public static int add(int a, int b) {
            return a + b;
        }

        public final void finalMethod() {}
    }

    static class Sub extends Base implements Greeter {
        private int privField = 44;
        public volatile long volField = 55L;
        public final int finalField = 66;
        public static String staticStr = "hello";
        public static final int CONST = 77;

        public Sub() {}

        private Sub(int x) {
            this.privField = x;
        }

        @Override
        public String greet(String who) {
            return "hi," + who;
        }

        @Override
        public String baseMethod(int x) {
            return "sub:" + x;
        }

        private String secret(int a, String b) {
            return a + ":" + b;
        }

        public static String join(String sep, String... parts) {
            return sep + parts.length;
        }
    }

    private int ownPrivField = 88;

    private MethodHandleCases() {}

    private MethodHandleCases(int x) {
        this.ownPrivField = x;
    }

    private String ownSecret(int a, String b) {
        return a + ":" + b;
    }

    public static void main(String[] args) throws Throwable {
        if (args.length != 1) {
            throw new IllegalArgumentException("usage: MethodHandleCases <mechanism>");
        }
        switch (args[0]) {
            case "find_static":
                findStatic();
                break;
            case "find_virtual":
                findVirtual();
                break;
            case "find_constructor_and_special":
                findConstructorAndSpecial();
                break;
            case "find_fields":
                findFields();
                break;
            case "unreflect":
                unreflect();
                break;
            case "missing_and_access_errors":
                missingAndAccessErrors();
                break;
            default:
                throw new IllegalArgumentException("unknown mechanism: " + args[0]);
        }
    }

    private static void describeHandle(String label, MethodHandles.Lookup lookup, MethodHandle mh) {
        MethodHandleInfo info = lookup.revealDirect(mh);
        System.out.println(
            label
                + " type=" + mh.type()
                + " kind=" + MethodHandleInfo.referenceKindToString(info.getReferenceKind())
                + " decl=" + info.getDeclaringClass().getName()
                + " name=" + info.getName()
                + " mtype=" + info.getMethodType()
                + " mods=" + info.getModifiers()
                + " varargs=" + info.isVarArgs()
        );
    }

    static void findStatic() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        describeHandle(
            "Base.add",
            lookup,
            lookup.findStatic(Base.class, "add", MethodType.methodType(int.class, int.class, int.class))
        );
        describeHandle(
            "Sub.add(inherited)",
            lookup,
            lookup.findStatic(Sub.class, "add", MethodType.methodType(int.class, int.class, int.class))
        );
        describeHandle(
            "Greeter.version",
            lookup,
            lookup.findStatic(Greeter.class, "version", MethodType.methodType(int.class))
        );
        describeHandle(
            "Sub.join(varargs)",
            lookup,
            lookup.findStatic(Sub.class, "join", MethodType.methodType(String.class, String.class, String[].class))
        );
    }

    static void findVirtual() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        describeHandle(
            "Base.baseMethod",
            lookup,
            lookup.findVirtual(Base.class, "baseMethod", MethodType.methodType(String.class, int.class))
        );
        describeHandle(
            "Sub.baseMethod",
            lookup,
            lookup.findVirtual(Sub.class, "baseMethod", MethodType.methodType(String.class, int.class))
        );
        describeHandle(
            "Base.finalMethod",
            lookup,
            lookup.findVirtual(Base.class, "finalMethod", MethodType.methodType(void.class))
        );
        describeHandle(
            "Greeter.greet",
            lookup,
            lookup.findVirtual(Greeter.class, "greet", MethodType.methodType(String.class, String.class))
        );
        describeHandle(
            "Sub.tag(default)",
            lookup,
            lookup.findVirtual(Sub.class, "tag", MethodType.methodType(String.class))
        );
        describeHandle(
            "Greeter.toString",
            lookup,
            lookup.findVirtual(Greeter.class, "toString", MethodType.methodType(String.class))
        );
        describeHandle(
            "MethodHandleCases.ownSecret(private)",
            lookup,
            lookup.findVirtual(MethodHandleCases.class, "ownSecret", MethodType.methodType(String.class, int.class, String.class))
        );
    }

    static void findConstructorAndSpecial() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        describeHandle(
            "Sub()",
            lookup,
            lookup.findConstructor(Sub.class, MethodType.methodType(void.class))
        );
        describeHandle(
            "MethodHandleCases(int)",
            lookup,
            lookup.findConstructor(MethodHandleCases.class, MethodType.methodType(void.class, int.class))
        );
        describeHandle(
            "Base.baseMethod(special)",
            lookup,
            lookup.findSpecial(Base.class, "baseMethod", MethodType.methodType(String.class, int.class), Sub.class)
        );
    }

    static void findFields() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        describeHandle("Sub.baseField.get", lookup, lookup.findGetter(Sub.class, "baseField", int.class));
        describeHandle("Sub.baseField.set", lookup, lookup.findSetter(Sub.class, "baseField", int.class));
        describeHandle("MethodHandleCases.ownPrivField.get", lookup, lookup.findGetter(MethodHandleCases.class, "ownPrivField", int.class));
        describeHandle("Sub.volField.get", lookup, lookup.findGetter(Sub.class, "volField", long.class));
        describeHandle("Sub.volField.set", lookup, lookup.findSetter(Sub.class, "volField", long.class));
        describeHandle("Sub.finalField.get", lookup, lookup.findGetter(Sub.class, "finalField", int.class));
        describeHandle("Sub.staticStr.get", lookup, lookup.findStaticGetter(Sub.class, "staticStr", String.class));
        describeHandle("Sub.staticStr.set", lookup, lookup.findStaticSetter(Sub.class, "staticStr", String.class));
        describeHandle("Sub.baseStatic.get", lookup, lookup.findStaticGetter(Sub.class, "baseStatic", int.class));
        describeHandle("Sub.CONST.get", lookup, lookup.findStaticGetter(Sub.class, "CONST", int.class));
    }

    static void unreflect() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        Method mAdd = Base.class.getDeclaredMethod("add", int.class, int.class);
        Method mSecret = MethodHandleCases.class.getDeclaredMethod("ownSecret", int.class, String.class);
        Method mGreet = Greeter.class.getDeclaredMethod("greet", String.class);
        Method mBase = Base.class.getDeclaredMethod("baseMethod", int.class);
        Constructor<MethodHandleCases> ctor = MethodHandleCases.class.getDeclaredConstructor(int.class);
        Field fPriv = MethodHandleCases.class.getDeclaredField("ownPrivField");
        Field fStat = Sub.class.getDeclaredField("staticStr");

        describeHandle("unreflect(add)", lookup, lookup.unreflect(mAdd));
        describeHandle("unreflect(secret)", lookup, lookup.unreflect(mSecret));
        describeHandle("unreflect(greet)", lookup, lookup.unreflect(mGreet));
        describeHandle("unreflectSpecial(base)", lookup, lookup.unreflectSpecial(mBase, Sub.class));
        describeHandle("unreflectConstructor", lookup, lookup.unreflectConstructor(ctor));
        describeHandle("unreflectGetter(priv)", lookup, lookup.unreflectGetter(fPriv));
        describeHandle("unreflectSetter(priv)", lookup, lookup.unreflectSetter(fPriv));
        describeHandle("unreflectGetter(stat)", lookup, lookup.unreflectGetter(fStat));
        describeHandle("unreflectSetter(stat)", lookup, lookup.unreflectSetter(fStat));
    }

    private static String fail(ThrowingRunnable r) {
        try {
            r.run();
            return "OK";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    interface ThrowingRunnable {
        void run() throws Throwable;
    }

    static void missingAndAccessErrors() {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandles.Lookup pub = MethodHandles.publicLookup();
        System.out.println("missingStatic=" + fail(() ->
            lookup.findStatic(Base.class, "noSuchMethod", MethodType.methodType(void.class))));
        System.out.println("missingVirtual=" + fail(() ->
            lookup.findVirtual(Sub.class, "noSuchMethod", MethodType.methodType(void.class))));
        System.out.println("missingCtor=" + fail(() ->
            lookup.findConstructor(Sub.class, MethodType.methodType(void.class, String.class))));
        System.out.println("missingField=" + fail(() ->
            lookup.findGetter(Sub.class, "noSuchField", int.class)));
        System.out.println("staticAsVirtual=" + fail(() ->
            lookup.findVirtual(Base.class, "add", MethodType.methodType(int.class, int.class, int.class))));
        System.out.println("virtualAsStatic=" + fail(() ->
            lookup.findStatic(Base.class, "baseMethod", MethodType.methodType(String.class, int.class))));
        System.out.println("staticFieldAsInst=" + fail(() ->
            lookup.findGetter(Sub.class, "staticStr", String.class)));
        System.out.println("instFieldAsStatic=" + fail(() ->
            lookup.findStaticGetter(Sub.class, "baseField", int.class)));
        System.out.println("finalSetter=" + fail(() ->
            lookup.findSetter(Sub.class, "finalField", int.class)));
        System.out.println("staticFinalSetter=" + fail(() ->
            lookup.findStaticSetter(Sub.class, "CONST", int.class)));
        System.out.println("publicLookupPrivate=" + fail(() ->
            pub.findVirtual(Sub.class, "secret", MethodType.methodType(String.class, int.class, String.class))));
        System.out.println("publicLookupCS=" + fail(() ->
            pub.findStatic(MethodHandles.class, "lookup", MethodType.methodType(MethodHandles.Lookup.class))));
        System.out.println("publicLookupJavaLangPkg=" + fail(() ->
            pub.findVirtual(String.class, "isLatin1", MethodType.methodType(boolean.class))));
    }
}
