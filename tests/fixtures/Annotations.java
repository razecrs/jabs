// Annotation and proxy fixtures for tests/annotations.rs: `AnnotationCases <mechanism>` runs one
// mechanism and prints what the JVM reports, so the native program's output can be compared
// line by line. The JDK makes every annotation instance with java.lang.reflect.Proxy; JABS
// generates those proxy classes at build time (src/proxy.rs). Proxy class names differ between
// JVMs (and from JABS), so no case prints one. Member listings are sorted: HotSpot's order is
// unspecified.
import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

enum AnnColor { RED, GREEN, BLUE }

@Retention(RetentionPolicy.RUNTIME)
@interface AnnInner {
    String value() default "inner";
    int weight() default 1;
}

@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.TYPE, ElementType.METHOD, ElementType.FIELD, ElementType.PARAMETER, ElementType.CONSTRUCTOR})
@interface AnnEvery {
    boolean z();
    byte b();
    char c();
    short s();
    int i();
    long j();
    float f();
    double d();
    String str();
    Class<?> cls();
    AnnColor color();
    AnnInner inner();
    int[] ints();
    String[] strs();
    Class<?>[] classes();
    AnnColor[] colors();
    AnnInner[] inners();
    long[] longs() default {1L, 2L};
}

@Retention(RetentionPolicy.RUNTIME)
@interface AnnDefaults {
    boolean z() default true;
    char c() default 'q';
    int i() default -5;
    long j() default Long.MIN_VALUE;
    float f() default 1.5f;
    double d() default Double.NaN;
    String str() default "dflt";
    Class<?> cls() default void.class;
    AnnColor color() default AnnColor.BLUE;
    AnnInner inner() default @AnnInner(value = "from default", weight = 9);
    int[] ints() default {};
    Class<?>[] classes() default {int[].class, String.class};
    AnnInner[] inners() default {@AnnInner, @AnnInner("second")};
}

@Retention(RetentionPolicy.RUNTIME)
@interface AnnMarker {}

@Retention(RetentionPolicy.RUNTIME)
@Inherited
@interface AnnInheritable { String value(); }

@Retention(RetentionPolicy.RUNTIME)
@interface AnnTag { String value(); }

@AnnEvery(z = true, b = -7, c = 'x', s = 300, i = 123456, j = 1L << 40, f = 2.5f, d = -0.0,
        str = "text \"quoted\" \t tab", cls = Map.Entry.class, color = AnnColor.GREEN,
        inner = @AnnInner(value = "nested", weight = 42),
        ints = {3, 1, 2}, strs = {"a", "b"}, classes = {int.class, String[].class, void.class},
        colors = {AnnColor.RED, AnnColor.BLUE}, inners = {@AnnInner("one"), @AnnInner(weight = 2)})
@AnnDefaults
@AnnMarker
@AnnInheritable("base")
class AnnTarget {
    @AnnTag("field") @AnnMarker public int field;
    @AnnDefaults(str = "on a field", ints = {9}) public String other;

    @AnnTag("ctor") AnnTarget(@AnnTag("ctor param") int x) {}

    AnnTarget() {}

    @AnnTag("method") @AnnDefaults(z = false)
    public void method(@AnnTag("p0") @AnnMarker int a, String unannotated, @AnnTag("p2") long c) {}

    @AnnTag("twin") public void twinA() {}
    @AnnTag("twin") public void twinB() {}
    @AnnTag("other") public void different() {}
}

@AnnTag("sub")
class AnnSub extends AnnTarget {}

// A class implementing an annotation interface by hand: equals between it and a JDK-made
// annotation goes through the JDK's reflective path (AnnotationInvocationHandler.equalsImpl).
class AnnTagImpl implements AnnTag {
    private final String value;
    AnnTagImpl(String value) { this.value = value; }
    public String value() { return value; }
    public Class<? extends Annotation> annotationType() { return AnnTag.class; }
    public int hashCode() { return (127 * "value".hashCode()) ^ value.hashCode(); }
    public boolean equals(Object o) { return o instanceof AnnTag && value.equals(((AnnTag) o).value()); }
    public String toString() { return "@AnnTag(\"" + value + "\")"; }
}

// ---- log4j-shaped plugins: classes found by name and read through their annotations ----

@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
@interface AnnPlugin {
    String name();
    String category();
    String elementType() default "";
    boolean printObject() default false;
    boolean deferChildren() default false;
}

@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.METHOD)
@interface AnnPluginFactory {}

@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.PARAMETER)
@interface AnnPluginAttribute {
    String value();
    String defaultString() default "";
}

@AnnPlugin(name = "Console", category = "Core", elementType = "appender", printObject = true)
class AnnConsoleAppender {
    final String name;
    final String target;
    private AnnConsoleAppender(String name, String target) { this.name = name; this.target = target; }

    @AnnPluginFactory
    public static AnnConsoleAppender createAppender(@AnnPluginAttribute("name") String name,
            @AnnPluginAttribute(value = "target", defaultString = "SYSTEM_OUT") String target) {
        return new AnnConsoleAppender(name, target);
    }

    public String toString() { return "Console[" + name + " -> " + target + "]"; }
}

@AnnPlugin(name = "PatternLayout", category = "Core", elementType = "layout")
class AnnPatternLayout {
    @AnnPluginFactory
    public static AnnPatternLayout createLayout(@AnnPluginAttribute(value = "pattern", defaultString = "%m%n") String pattern) {
        return new AnnPatternLayout();
    }
    public String toString() { return "PatternLayout"; }
}

class AnnNotAPlugin {}

// ---- A user-built proxy over a constant interface list ----

interface AnnGreeter {
    String greet(String who);
    long add(int a, long b, double c);
    void touch();
    int fail(int kind) throws java.io.IOException;
}

interface AnnCounter {
    int count();
    boolean flag(boolean in, char c, byte b, short s, float f);
}

class AnnHandler implements InvocationHandler {
    final List<String> log = new ArrayList<>();

    public Object invoke(Object proxy, Method method, Object[] args) throws Throwable {
        log.add(method.getDeclaringClass().getName() + "." + method.getName() + " "
                + (args == null ? "null" : Arrays.toString(args)));
        switch (method.getName()) {
            case "greet": return "hi " + args[0];
            case "add": return ((Integer) args[0]) + ((Long) args[1]) + (long) (double) (Double) args[2];
            case "touch": return null;
            case "count": return 7;
            case "flag": return !((Boolean) args[0]);
            case "hashCode": return 99;
            case "equals": return proxy == args[0];
            case "toString": return "proxy!";
            case "fail":
                int kind = (Integer) args[0];
                if (kind == 0) throw new java.io.IOException("declared");
                if (kind == 1) throw new IllegalStateException("runtime");
                if (kind == 2) throw new Exception("undeclared checked");
                return null; // null for a primitive result: NullPointerException
            default: throw new AssertionError(method.getName());
        }
    }
}

class AnnotationCases {
    static String show(Object v) {
        if (v instanceof Object[]) return Arrays.deepToString((Object[]) v);
        if (v instanceof int[]) return Arrays.toString((int[]) v);
        if (v instanceof long[]) return Arrays.toString((long[]) v);
        if (v instanceof Class) return "class " + ((Class<?>) v).getName();
        return String.valueOf(v);
    }

    static List<String> sorted(Annotation[] annotations) {
        List<String> out = new ArrayList<>();
        for (Annotation a : annotations) out.add(a.toString());
        Collections.sort(out);
        return out;
    }

    static Method method(Class<?> c, String name) {
        for (Method m : c.getDeclaredMethods()) if (m.getName().equals(name)) return m;
        throw new AssertionError(name);
    }

    static void everyMemberKind() {
        AnnEvery e = AnnTarget.class.getAnnotation(AnnEvery.class);
        System.out.println("z=" + e.z() + " b=" + e.b() + " c=" + e.c() + " s=" + e.s() + " i=" + e.i());
        System.out.println("j=" + e.j() + " f=" + e.f() + " d=" + e.d() + " str=" + e.str());
        System.out.println("cls=" + e.cls().getName() + " color=" + e.color() + " " + (e.color() == AnnColor.GREEN));
        System.out.println("inner=" + e.inner().value() + "/" + e.inner().weight() + " type=" + e.inner().annotationType().getName());
        System.out.println("ints=" + Arrays.toString(e.ints()) + " strs=" + Arrays.toString(e.strs()));
        Class<?>[] classes = e.classes();
        StringBuilder names = new StringBuilder();
        for (Class<?> c : classes) names.append(c.getName()).append(' ');
        System.out.println("classes=" + names + "colors=" + Arrays.toString(e.colors()));
        AnnInner[] inners = e.inners();
        System.out.println("inners=" + inners.length + " " + inners[0].value() + "/" + inners[0].weight() + " " + inners[1].value() + "/" + inners[1].weight());
        System.out.println("longs (default)=" + Arrays.toString(e.longs()));
        // Arrays are copied on every call: changing one does not change the annotation.
        e.ints()[0] = 100;
        System.out.println("ints again=" + Arrays.toString(e.ints()));
    }

    static void defaultsFillMissingMembers() {
        AnnDefaults d = AnnTarget.class.getAnnotation(AnnDefaults.class);
        System.out.println(d.z() + " " + d.c() + " " + d.i() + " " + d.j() + " " + d.f() + " " + d.d() + " " + d.str());
        System.out.println(d.cls().getName() + " " + d.color() + " " + d.inner().value() + "/" + d.inner().weight());
        System.out.println(d.ints().length + " " + d.classes()[0].getName() + " " + d.classes()[1].getName()
                + " " + d.inners()[0].value() + " " + d.inners()[1].value());
        AnnDefaults onMethod = method(AnnTarget.class, "method").getAnnotation(AnnDefaults.class);
        System.out.println("method: z=" + onMethod.z() + " str=" + onMethod.str());
        List<String> defaults = new ArrayList<>();
        for (Method m : AnnDefaults.class.getDeclaredMethods()) defaults.add(m.getName() + "=" + show(m.getDefaultValue()));
        Collections.sort(defaults);
        for (String s : defaults) System.out.println("default " + s);
        System.out.println("no default: " + show(method(AnnTag.class, "value").getDefaultValue()));
    }

    static void identityMethods() throws Exception {
        AnnTag twinA = method(AnnTarget.class, "twinA").getAnnotation(AnnTag.class);
        AnnTag twinB = method(AnnTarget.class, "twinB").getAnnotation(AnnTag.class);
        AnnTag different = method(AnnTarget.class, "different").getAnnotation(AnnTag.class);
        System.out.println("annotationType=" + twinA.annotationType().getName());
        System.out.println("same=" + (twinA == twinB) + " equals=" + twinA.equals(twinB) + " different=" + twinA.equals(different));
        System.out.println("hash " + twinA.hashCode() + " " + twinB.hashCode() + " " + different.hashCode());
        System.out.println("toString " + twinA);
        System.out.println("equals null=" + twinA.equals(null) + " string=" + twinA.equals("twin"));
        AnnTagImpl byHand = new AnnTagImpl("twin");
        System.out.println("hand-made: " + twinA.equals(byHand) + " " + byHand.equals(twinA) + " " + (twinA.hashCode() == byHand.hashCode()));
        AnnEvery e = AnnTarget.class.getAnnotation(AnnEvery.class);
        // Class and enum members hash by identity, which differs between runs and JVMs: only
        // annotations made of primitives, Strings and arrays of them print their hash.
        System.out.println("every hash stable " + (e.hashCode() == e.hashCode()) + " equals itself " + e.equals(e));
        System.out.println("every " + e);
        AnnDefaults d = AnnTarget.class.getAnnotation(AnnDefaults.class);
        AnnDefaults onField = AnnTarget.class.getDeclaredField("other").getAnnotation(AnnDefaults.class);
        System.out.println("defaults equal " + d.equals(onField) + " " + d.equals(AnnTarget.class.getAnnotation(AnnDefaults.class)));
        System.out.println("defaults " + d);
        System.out.println("marker " + AnnTarget.class.getAnnotation(AnnMarker.class) + " " + AnnTarget.class.getAnnotation(AnnMarker.class).hashCode());
    }

    static void declaredAnnotationsEverywhere() throws Exception {
        System.out.println("class " + sorted(AnnTarget.class.getDeclaredAnnotations()));
        System.out.println("field " + sorted(AnnTarget.class.getDeclaredField("field").getDeclaredAnnotations()));
        System.out.println("other " + sorted(AnnTarget.class.getDeclaredField("other").getAnnotations()));
        Method m = method(AnnTarget.class, "method");
        System.out.println("method " + sorted(m.getDeclaredAnnotations()));
        Annotation[][] params = m.getParameterAnnotations();
        for (int i = 0; i < params.length; i++) System.out.println("param " + i + " " + sorted(params[i]));
        Constructor<?> c = AnnTarget.class.getDeclaredConstructor(int.class);
        System.out.println("ctor " + sorted(c.getDeclaredAnnotations()));
        System.out.println("ctor param " + sorted(c.getParameterAnnotations()[0]));
        System.out.println("plain ctor " + sorted(AnnTarget.class.getDeclaredConstructor().getDeclaredAnnotations()));
        System.out.println("present " + AnnTarget.class.isAnnotationPresent(AnnMarker.class) + " "
                + AnnTarget.class.isAnnotationPresent(AnnPlugin.class) + " " + AnnTarget.class.getAnnotation(AnnPlugin.class));
        System.out.println("byType " + AnnTarget.class.getAnnotationsByType(AnnTag.class).length + " "
                + m.getAnnotationsByType(AnnTag.class)[0].value());
        System.out.println("meta " + sorted(AnnEvery.class.getAnnotations()));
    }

    static void inheritedAnnotations() {
        System.out.println("sub declared " + sorted(AnnSub.class.getDeclaredAnnotations()));
        System.out.println("sub all " + sorted(AnnSub.class.getAnnotations()));
        System.out.println("inherited " + AnnSub.class.getAnnotation(AnnInheritable.class).value()
                + " not inherited " + AnnSub.class.getAnnotation(AnnMarker.class));
    }

    static void proxyClassAndHandler() {
        AnnTag tag = method(AnnTarget.class, "twinA").getAnnotation(AnnTag.class);
        System.out.println("isProxyClass annotation=" + Proxy.isProxyClass(tag.getClass())
                + " String=" + Proxy.isProxyClass(String.class) + " Proxy=" + Proxy.isProxyClass(Proxy.class));
        InvocationHandler h = Proxy.getInvocationHandler(tag);
        System.out.println("handler " + h.getClass().getName());
        System.out.println("instanceof " + (tag instanceof Proxy) + " " + (tag instanceof AnnTag) + " " + (tag instanceof Annotation));
        Class<?> pc = tag.getClass();
        System.out.println("modifiers final=" + Modifier.isFinal(pc.getModifiers()) + " superclass=" + pc.getSuperclass().getName()
                + " interfaces=" + Arrays.toString(pc.getInterfaces()));
        try {
            Proxy.getInvocationHandler("not a proxy");
        } catch (IllegalArgumentException e) {
            System.out.println("IAE " + e.getMessage());
        }
    }

    static void userProxy() throws Exception {
        AnnHandler handler = new AnnHandler();
        Object p = Proxy.newProxyInstance(AnnotationCases.class.getClassLoader(),
                new Class<?>[] { AnnGreeter.class, AnnCounter.class }, handler);
        AnnGreeter g = (AnnGreeter) p;
        AnnCounter c = (AnnCounter) p;
        System.out.println(g.greet("bob") + " " + g.add(1, 2L, 3.9) + " " + c.count() + " " + c.flag(true, 'k', (byte) 3, (short) -4, 0.5f));
        g.touch();
        System.out.println(p.hashCode() + " " + p.equals(p) + " " + p.equals("x") + " " + p);
        try { g.fail(0); } catch (java.io.IOException e) { System.out.println("declared passes: " + e.getMessage()); }
        try { g.fail(1); } catch (IllegalStateException e) { System.out.println("runtime passes: " + e.getMessage()); }
        try { g.fail(2); } catch (UndeclaredThrowableException e) {
            System.out.println("wrapped: " + e.getUndeclaredThrowable().getClass().getName() + " " + e.getUndeclaredThrowable().getMessage());
        }
        try { g.fail(3); } catch (NullPointerException e) { System.out.println("null for int: NullPointerException"); }
        System.out.println("isProxyClass " + Proxy.isProxyClass(p.getClass()) + " handler same " + (Proxy.getInvocationHandler(p) == handler));
        Object again = Proxy.newProxyInstance(AnnotationCases.class.getClassLoader(),
                new Class<?>[] { AnnGreeter.class, AnnCounter.class }, handler);
        System.out.println("one class per list " + (again.getClass() == p.getClass()));
        for (String line : handler.log) System.out.println("log " + line);
    }

    static void unknownInterfaceList() {
        // Built at run time: the build cannot see this list, so JABS has no proxy class for it.
        List<Class<?>> list = new ArrayList<>();
        list.add(Runnable.class);
        list.add(Comparable.class);
        Class<?>[] interfaces = list.toArray(new Class<?>[0]);
        Object p = Proxy.newProxyInstance(AnnotationCases.class.getClassLoader(), interfaces, new AnnHandler());
        System.out.println("made " + Proxy.isProxyClass(p.getClass()));
    }

    // log4j's PluginManager reads @Plugin from candidate classes, then PluginBuilder finds the
    // @PluginFactory method and fills its parameters from their @PluginAttribute annotations.
    static void log4jShapedPluginLookup() throws Exception {
        Map<String, String> config = new HashMap<>();
        config.put("name", "STDOUT");
        config.put("pattern", "%d %m%n");
        // log4j loads the candidates by name from its plugin cache; class constants keep this
        // fixture independent of which classes a by-name lookup can find.
        Class<?>[] candidates = { AnnConsoleAppender.class, AnnNotAPlugin.class, AnnPatternLayout.class };
        for (Class<?> c : candidates) {
            String name = c.getName();
            AnnPlugin plugin = c.getAnnotation(AnnPlugin.class);
            if (plugin == null) {
                System.out.println(name + ": not a plugin");
                continue;
            }
            System.out.println(name + ": " + plugin.name() + " category=" + plugin.category() + " elementType=" + plugin.elementType()
                    + " printObject=" + plugin.printObject() + " deferChildren=" + plugin.deferChildren());
            for (Method m : c.getDeclaredMethods()) {
                if (!m.isAnnotationPresent(AnnPluginFactory.class)) continue;
                Annotation[][] params = m.getParameterAnnotations();
                Object[] args = new Object[params.length];
                for (int i = 0; i < params.length; i++) {
                    for (Annotation a : params[i]) {
                        if (a instanceof AnnPluginAttribute) {
                            AnnPluginAttribute attr = (AnnPluginAttribute) a;
                            String v = config.get(attr.value());
                            args[i] = v != null ? v : attr.defaultString();
                        }
                    }
                }
                System.out.println("  factory " + m.getName() + " -> " + m.invoke(null, args));
            }
        }
    }

    public static void main(String[] args) throws Exception {
        switch (args[0]) {
            case "get_annotation_reads_every_member_kind": everyMemberKind(); break;
            case "defaults_fill_missing_members": defaultsFillMissingMembers(); break;
            case "annotation_identity_methods_match_the_jvm": identityMethods(); break;
            case "declared_annotations_on_every_element": declaredAnnotationsEverywhere(); break;
            case "inherited_annotations_come_from_the_superclass": inheritedAnnotations(); break;
            case "proxy_class_and_invocation_handler": proxyClassAndHandler(); break;
            case "user_proxy_over_a_constant_interface_list": userProxy(); break;
            case "interface_list_not_generated_at_build_time": unknownInterfaceList(); break;
            case "log4j_shaped_plugin_lookup": log4jShapedPluginLookup(); break;
            default: throw new IllegalArgumentException("unknown mechanism " + args[0]);
        }
    }
}
