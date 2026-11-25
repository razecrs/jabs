// Reflection fixtures for tests/reflection.rs: `ReflectCases <mechanism>` runs one mechanism and
// prints what the JVM reports, so the native program's output can be compared line by line.
// HotSpot's member order is unspecified (it sorts by symbol address), so listings are sorted.
import java.io.IOException;
import java.lang.reflect.*;
import java.util.*;

interface ReflectGreeter {
    String greet(String who);

    default int version() { return 1; }
}

abstract class ReflectBase implements ReflectGreeter {
    protected int baseField;
    public long publicBaseField;

    public static String baseStatic() { return "base"; }

    public String greet(String who) { return "hello " + who; }

    abstract int size();
}

class ReflectSub extends ReflectBase implements Comparable<ReflectSub>, Runnable {
    public static final int CONSTANT = 7;
    private String name = "sub";
    volatile long counter;
    transient double ratio;
    public List<String> tags;

    public ReflectSub() {}

    private ReflectSub(String name) { this.name = name; }

    protected ReflectSub(int a, long b) throws IOException {}

    int size() { return 3; }

    public int compareTo(ReflectSub o) { return 0; }

    public void run() {}

    private static int secret(int x) { return x * 2; }

    // No wildcard: WildcardTypeImpl/TypeVariableImpl bounds test instanceof against an interface
    // array type, which JABS does not compile yet (a compiler gap, not reflection).
    public synchronized <T extends Number> List<T> generic(Map<String, T> m, int... rest)
            throws IllegalStateException {
        return null;
    }

    public static class Nested {}

    private class Inner {}

    static Class<?> innerClass() { return Inner.class; }

    protected static final class Guarded {}
}

class ReflectTarget implements ReflectGreeter {
    static final int CONSTANT = 1;
    static int shared;
    static long flagged;
    int value;
    long big;
    double ratio;
    boolean on;
    char letter;
    String label;
    final int fixed = 1;
    private int secret = 42;

    ReflectTarget(int value) {
        if (value < 0) throw new IllegalStateException("negative " + value);
        this.value = value;
    }

    static int add(int a, int b) { return a + b; }

    long scaled(long by) { return value * by; }

    private String hidden() { return "hidden " + value; }

    void nothing() {}

    static String text(String s, char c) { return s + c; }

    static String wide(long a, double b, int c) { return a + " " + b + " " + c; }

    static boolean flag(boolean b) { return !b; }

    static String small(short s, byte b, char c, float f) { return s + " " + b + " " + c + " " + f; }

    static String over(int x) { return "int"; }

    static String over(Integer x) { return "Integer"; }

    static String over(Object x) { return "Object"; }

    static String join(String sep, String... parts) { return String.join(sep, parts); }

    static int fail(int how) {
        if (how == 1) throw new UnsupportedOperationException("one");
        if (how == 2) throw new StackOverflowError("two");
        return Integer.parseInt("x" + how);
    }

    public String greet(String who) { return "target greets " + who; }

    public String toString() { return "ReflectTarget(" + value + ")"; }
}

enum ReflectColor { RED, GREEN, BLUE }

class ReflectEntry {
    static int created;
    private String name;
    private int level;
    private double score;
    private boolean op;
    private transient int cache = 99;

    private ReflectEntry() { created++; }

    public String toString() { return name + "/" + level + "/" + score + "/" + op + "/" + cache; }

    static class Inner {}
}

class ReflectCases {
    static <T> List<String> sorted(T[] items) {
        List<String> out = new ArrayList<>();
        for (T t : items) out.add(String.valueOf(t));
        Collections.sort(out);
        return out;
    }

    static void print(Object value) { System.out.println(value); }

    // ---- (a) listings ----

    static void declaredMethodsExcludeInherited() {
        for (String m : sorted(ReflectSub.class.getDeclaredMethods())) print(m);
    }

    static void declaredConstructorsReportEveryVisibility() {
        for (String c : sorted(ReflectSub.class.getDeclaredConstructors())) print(c);
        print(ReflectSub.class.getConstructors().length);
    }

    static void declaredFieldsReportModifiersAndTypes() {
        for (String f : sorted(ReflectSub.class.getDeclaredFields())) print(f);
        for (Field f : ReflectSub.class.getDeclaredFields()) {
            if (f.getName().equals("tags")) print(f.toGenericString());
        }
    }

    static void publicMembersIncludeInherited() {
        List<String> methods = new ArrayList<>();
        for (Method m : ReflectSub.class.getMethods()) {
            methods.add(m.getDeclaringClass().getName() + "." + m.getName() + " bridge=" + m.isBridge());
        }
        Collections.sort(methods);
        for (String m : methods) print(m);
        for (String f : sorted(ReflectSub.class.getFields())) print(f);
    }

    static void getMethodMatchesParameterTypes() throws Exception {
        print(ReflectSub.class.getMethod("greet", String.class));
        print(ReflectSub.class.getMethod("baseStatic"));
        print(ReflectSub.class.getMethod("version"));
        print(ReflectSub.class.getDeclaredMethod("secret", int.class));
        print(ReflectSub.class.getDeclaredConstructor(String.class));
        print(ReflectSub.class.getDeclaredField("counter"));
        print(ReflectSub.class.getField("publicBaseField"));
    }

    static void getMethodRejectsWhatItCannotSee() {
        try {
            ReflectSub.class.getMethod("secret", int.class);
            print("found a private method");
        } catch (NoSuchMethodException e) {
            print("NoSuchMethodException " + e.getMessage());
        }
        try {
            ReflectSub.class.getDeclaredMethod("greet", String.class, int.class);
            print("found a missing overload");
        } catch (NoSuchMethodException e) {
            print("NoSuchMethodException " + e.getMessage());
        }
        try {
            ReflectSub.class.getField("name");
            print("found a private field");
        } catch (NoSuchFieldException e) {
            print("NoSuchFieldException " + e.getMessage());
        }
    }

    static void genericSignaturesReachTypes() throws Exception {
        Method m = ReflectSub.class.getDeclaredMethod("generic", Map.class, int[].class);
        print(m.getGenericReturnType() + " " + Arrays.toString(m.getGenericParameterTypes()));
        print(m.getTypeParameters()[0].getName() + " " + m.toString());
        print(m.isVarArgs() + " " + Modifier.toString(m.getModifiers()));
        print(Arrays.toString(m.getExceptionTypes()));
        // Not getGenericInterfaces: it returns a Type[] (an interface array), whose verification
        // JABS does not support yet (a compiler gap, not reflection).
        print(ReflectSub.class.getDeclaredField("tags").getGenericType());
        print(ReflectSub.class.getGenericSuperclass());
    }

    // ---- class attributes ----

    static void classAttributesFromTheClassFile() throws Exception {
        print(Arrays.toString(ReflectSub.class.getInterfaces()));
        print(Arrays.toString(int[].class.getInterfaces()));
        print(int.class.getInterfaces().length);
        for (String c : sorted(ReflectSub.class.getDeclaredClasses())) print(c);
        print(ReflectSub.Nested.class.getDeclaringClass());
        print(ReflectSub.class.getDeclaringClass());
        Runnable anonymous = new Runnable() { public void run() {} };
        class Local {}
        print(ReflectSub.Nested.class.getSimpleName() + " [" + anonymous.getClass().getSimpleName() + "] "
                + Local.class.getSimpleName());
        print(ReflectSub.Nested.class.isMemberClass() + " " + Local.class.isLocalClass() + " "
                + anonymous.getClass().isAnonymousClass());
        print(Local.class.getEnclosingClass());
        print(int.class.getDeclaredMethods().length + " " + String[].class.getDeclaredFields().length);
    }

    static void classModifiersComeFromInnerClasses() throws Exception {
        print(Modifier.toString(ReflectSub.Nested.class.getModifiers()));
        print(Modifier.toString(ReflectSub.innerClass().getModifiers()));
        print(Modifier.toString(ReflectSub.Guarded.class.getModifiers()));
        print(ReflectSub.class.getModifiers());
        print(Modifier.toString(ReflectGreeter.class.getModifiers()));
        print(ReflectBase.class.getModifiers());
    }

    // ---- (b) calling ----

    static String describe(Object o) {
        return o == null ? "null" : o.getClass().getName() + " " + o;
    }

    static void call(Method m, Object receiver, Object... args) {
        try {
            print(m.getName() + " -> " + describe(m.invoke(receiver, args)));
        } catch (InvocationTargetException e) {
            print(m.getName() + " threw InvocationTargetException(" + e.getCause().getClass().getName() + ": "
                    + e.getCause().getMessage() + ")");
        } catch (IllegalArgumentException e) {
            // A NullPointerException cause's message is HotSpot's helpful text, which JABS does not
            // produce: report only its class.
            Throwable cause = e.getCause();
            print(m.getName() + " IllegalArgumentException " + (cause != null ? "caused by " + cause.getClass().getName() : e.getMessage()));
        } catch (Exception e) {
            print(m.getName() + " " + e.getClass().getName());
        }
    }

    static void invokeCallsStaticInstanceAndPrivateMethods() throws Exception {
        ReflectTarget t = new ReflectTarget(5);
        call(ReflectTarget.class.getDeclaredMethod("add", int.class, int.class), null, 2, 3);
        call(ReflectTarget.class.getDeclaredMethod("scaled", long.class), t, 7L);
        Method hidden = ReflectTarget.class.getDeclaredMethod("hidden");
        hidden.setAccessible(true);
        call(hidden, t);
        call(ReflectTarget.class.getDeclaredMethod("nothing"), t);
        call(ReflectTarget.class.getDeclaredMethod("text", String.class, char.class), null, "ab", 'c');
        call(Object.class.getMethod("toString"), new ReflectTarget(1));
    }

    static void invokeDispatchesVirtuallyAndThroughInterfaces() throws Exception {
        ReflectBase base = new ReflectSub();
        call(ReflectBase.class.getMethod("greet", String.class), base, "x");
        call(ReflectGreeter.class.getMethod("greet", String.class), new ReflectTarget(0), "y");
        call(ReflectGreeter.class.getMethod("version"), new ReflectTarget(0));
        call(ReflectGreeter.class.getMethod("version"), base);
        call(ReflectBase.class.getDeclaredMethod("size"), base);
    }

    static void invokeUnboxesAndWidensLikeTheJvm() throws Exception {
        Method wide = ReflectTarget.class.getDeclaredMethod("wide", long.class, double.class, int.class);
        call(wide, null, 1, 2, 'c');
        call(wide, null, (byte) 1, 2.5f, (short) 3);
        call(wide, null, 1L, 2L, (byte) 4);
        Method flag = ReflectTarget.class.getDeclaredMethod("flag", boolean.class);
        call(flag, null, true);
        Method small = ReflectTarget.class.getDeclaredMethod("small", short.class, byte.class, char.class, float.class);
        call(small, null, (short) -2, (byte) -3, 'z', 1);
        Object a = ReflectTarget.class.getDeclaredMethod("add", int.class, int.class).invoke(null, 1, 2);
        Object b = ReflectTarget.class.getDeclaredMethod("add", int.class, int.class).invoke(null, 1, 2);
        print("small results are cached boxes: " + (a == b));
    }

    static void invokeSelectsOverloadsAndVarargs() throws Exception {
        call(ReflectTarget.class.getDeclaredMethod("over", int.class), null, 1);
        call(ReflectTarget.class.getDeclaredMethod("over", Integer.class), null, 1);
        call(ReflectTarget.class.getDeclaredMethod("over", Object.class), null, "s");
        Method varargs = ReflectTarget.class.getDeclaredMethod("join", String.class, String[].class);
        print(varargs.isVarArgs());
        call(varargs, null, "-", new String[] {"a", "b"});
        call(varargs, null, "-", new String[0]);
    }

    static void constructorNewInstanceRunsTheConstructor() throws Exception {
        Constructor<ReflectTarget> c = ReflectTarget.class.getDeclaredConstructor(int.class);
        print(c.newInstance(9).value);
        Constructor<ReflectSub> hidden = ReflectSub.class.getDeclaredConstructor(String.class);
        hidden.setAccessible(true);
        print(hidden.newInstance("made").greet("y"));
        print(ReflectSub.class.getConstructor().newInstance().size());
        try {
            ReflectTarget.class.getDeclaredConstructor(int.class).newInstance(-1);
        } catch (InvocationTargetException e) {
            print("InvocationTargetException(" + e.getCause().getClass().getName() + ": " + e.getCause().getMessage() + ")");
        }
        try {
            ReflectBase.class.getDeclaredConstructor().newInstance();
        } catch (InstantiationException e) {
            print("InstantiationException");
        }
    }

    // ---- (c) failures ----

    static void invokeRejectsBadArgumentsLikeTheJvm() throws Exception {
        Method add = ReflectTarget.class.getDeclaredMethod("add", int.class, int.class);
        call(add, null, 1);
        call(add, null, 1, 2, 3);
        call(add, null, 1, "two");
        call(add, null, 1, null);
        call(add, null, 1, 2L);
        call(add, null);
        Method text = ReflectTarget.class.getDeclaredMethod("text", String.class, char.class);
        call(text, null, 1, 'c');
        call(text, null, "a", (byte) 1);
        call(text, null, null, 'q');
    }

    static void invokeChecksTheReceiver() throws Exception {
        Method scaled = ReflectTarget.class.getDeclaredMethod("scaled", long.class);
        call(scaled, null, 1L);
        call(scaled, "not a target", 1L);
        call(ReflectTarget.class.getDeclaredMethod("add", int.class, int.class), "ignored", 1, 2);
    }

    static void invokeWrapsThrownExceptions() throws Exception {
        call(ReflectTarget.class.getDeclaredMethod("fail", int.class), null, 1);
        call(ReflectTarget.class.getDeclaredMethod("fail", int.class), null, 2);
        call(ReflectTarget.class.getDeclaredMethod("fail", int.class), null, 3);
    }

    static void accessChecksStayInJava() throws Exception {
        Method hidden = ReflectTarget.class.getDeclaredMethod("hidden");
        try {
            hidden.invoke(new ReflectTarget(1));
            print("private method invoked without setAccessible");
        } catch (IllegalAccessException e) {
            print("IllegalAccessException " + e.getMessage());
        }
        Field secret = ReflectTarget.class.getDeclaredField("secret");
        try {
            secret.get(new ReflectTarget(1));
            print("private field read without setAccessible");
        } catch (IllegalAccessException e) {
            print("IllegalAccessException " + e.getMessage());
        }
        print(ReflectTarget.class.getDeclaredMethod("add", int.class, int.class).invoke(null, 4, 5));
    }

    // ---- fields ----

    static void fieldGetAndSetConvertLikeTheJvm() throws Exception {
        ReflectTarget t = new ReflectTarget(3);
        Field value = ReflectTarget.class.getDeclaredField("value");
        print(describe(value.get(t)) + " " + value.getLong(t) + " " + value.getDouble(t));
        value.set(t, (short) 12);
        value.setByte(t, (byte) 13);
        print(value.getInt(t));
        value.setChar(t, 'A');
        print(value.getInt(t));
        Field big = ReflectTarget.class.getDeclaredField("big");
        big.setInt(t, -5);
        print(big.getLong(t) + " " + big.getFloat(t));
        Field ratio = ReflectTarget.class.getDeclaredField("ratio");
        ratio.set(t, 3);
        print(ratio.get(t));
        Field on = ReflectTarget.class.getDeclaredField("on");
        on.setBoolean(t, true);
        print(on.get(t) + " " + on.getBoolean(t));
        Field letter = ReflectTarget.class.getDeclaredField("letter");
        letter.set(t, 'x');
        print(letter.getChar(t) + " " + letter.getInt(t));
        Field label = ReflectTarget.class.getDeclaredField("label");
        label.set(t, "hi");
        print(label.get(t));
        Field shared = ReflectTarget.class.getDeclaredField("shared");
        shared.set(null, 41);
        print(shared.getInt(null) + " " + ReflectTarget.shared);
        Field flag = ReflectTarget.class.getDeclaredField("flagged");
        flag.setLong(null, 1L << 40);
        print(ReflectTarget.flagged);
    }

    static String failure(Exception e) {
        return e.getClass().getName() + (e instanceof NullPointerException ? "" : ": " + e.getMessage());
    }

    static void fieldAccessRejectsWhatTheJvmRejects() throws Exception {
        ReflectTarget t = new ReflectTarget(3);
        Field value = ReflectTarget.class.getDeclaredField("value");
        Field label = ReflectTarget.class.getDeclaredField("label");
        Field fixed = ReflectTarget.class.getDeclaredField("fixed");
        Field constant = ReflectTarget.class.getDeclaredField("CONSTANT");
        try { value.getBoolean(t); } catch (Exception e) { print("getBoolean of int: " + failure(e)); }
        try { value.set(t, 1L); } catch (Exception e) { print("set long into int: " + failure(e)); }
        try { value.set(t, "x"); } catch (Exception e) { print("set String into int: " + failure(e)); }
        try { value.set(t, null); } catch (Exception e) { print("set null into int: " + failure(e)); }
        try { label.set(t, 5); } catch (Exception e) { print("set Integer into String: " + failure(e)); }
        try { value.get(null); } catch (Exception e) { print("get with null receiver: " + failure(e)); }
        try { value.get("other"); } catch (Exception e) { print("get with wrong receiver: " + failure(e)); }
        try { value.setInt("other", 1); } catch (Exception e) { print("set with wrong receiver: " + failure(e)); }
        try { fixed.setInt(t, 2); } catch (Exception e) { print("final field without setAccessible: " + failure(e)); }
        fixed.setAccessible(true);
        fixed.setInt(t, 2);
        print("final instance field after setAccessible: " + fixed.getInt(t));
        constant.setAccessible(true);
        try { constant.setInt(null, 2); } catch (Exception e) { print("static final after setAccessible: " + failure(e)); }
    }

    // ---- (d) real-world paths ----

    static void enumConstantsThroughReflection() {
        EnumMap<ReflectColor, String> map = new EnumMap<>(ReflectColor.class);
        map.put(ReflectColor.GREEN, "g");
        map.put(ReflectColor.RED, "r");
        print(map);
        print(EnumSet.allOf(ReflectColor.class));
        print(EnumSet.range(ReflectColor.GREEN, ReflectColor.BLUE));
        print(Enum.valueOf(ReflectColor.class, "BLUE").ordinal());
        print(ReflectColor.valueOf("RED"));
        try {
            ReflectColor.valueOf("PINK");
        } catch (IllegalArgumentException e) {
            print("IllegalArgumentException " + e.getMessage());
        }
        print(Arrays.toString(ReflectColor.class.getEnumConstants()));
        print(ReflectColor.class.isEnum() + " " + ReflectColor.GREEN.getDeclaringClass());
    }

    static void annotationsAbsentReadAsNull() throws Exception {
        print(ReflectTarget.class.getDeclaredField("value").getAnnotation(Deprecated.class));
        print(ReflectTarget.class.getDeclaredMethod("nothing").getDeclaredAnnotations().length);
        print(ReflectTarget.class.isAnnotationPresent(Deprecated.class));
    }

    // Gson's path without Gson: find the no-arg constructor, make the object, set the private
    // fields named by a "JSON" map, read every field back.
    static void gsonShapedRoundTrip() throws Exception {
        Map<String, Object> json = new LinkedHashMap<>();
        json.put("name", "Steve");
        json.put("level", 30);
        json.put("score", 12.5);
        json.put("op", true);
        Constructor<ReflectEntry> make = ReflectEntry.class.getDeclaredConstructor();
        make.setAccessible(true);
        ReflectEntry entry = make.newInstance();
        for (Map.Entry<String, Object> e : json.entrySet()) {
            Field f = ReflectEntry.class.getDeclaredField(e.getKey());
            f.setAccessible(true);
            f.set(entry, e.getValue());
        }
        List<String> out = new ArrayList<>();
        for (Field f : ReflectEntry.class.getDeclaredFields()) {
            if (Modifier.isStatic(f.getModifiers()) || Modifier.isTransient(f.getModifiers())) continue;
            f.setAccessible(true);
            out.add(f.getName() + "=" + f.get(entry));
        }
        Collections.sort(out);
        print(out);
        print(entry);
        print(Modifier.isStatic(ReflectEntry.Inner.class.getModifiers()) + " " + ReflectEntry.Inner.class.isMemberClass());
    }

    public static void main(String[] args) throws Exception {
        switch (args[0]) {
            case "declared_methods_exclude_inherited": declaredMethodsExcludeInherited(); break;
            case "declared_constructors_report_every_visibility": declaredConstructorsReportEveryVisibility(); break;
            case "declared_fields_report_modifiers_and_types": declaredFieldsReportModifiersAndTypes(); break;
            case "public_members_include_inherited": publicMembersIncludeInherited(); break;
            case "get_method_matches_parameter_types": getMethodMatchesParameterTypes(); break;
            case "get_method_rejects_what_it_cannot_see": getMethodRejectsWhatItCannotSee(); break;
            case "generic_signatures_reach_types": genericSignaturesReachTypes(); break;
            case "class_attributes_from_the_class_file": classAttributesFromTheClassFile(); break;
            case "class_modifiers_come_from_inner_classes": classModifiersComeFromInnerClasses(); break;
            case "invoke_calls_static_instance_and_private_methods": invokeCallsStaticInstanceAndPrivateMethods(); break;
            case "invoke_dispatches_virtually_and_through_interfaces": invokeDispatchesVirtuallyAndThroughInterfaces(); break;
            case "invoke_unboxes_and_widens_like_the_jvm": invokeUnboxesAndWidensLikeTheJvm(); break;
            case "invoke_selects_overloads_and_varargs": invokeSelectsOverloadsAndVarargs(); break;
            case "constructor_new_instance_runs_the_constructor": constructorNewInstanceRunsTheConstructor(); break;
            case "invoke_rejects_bad_arguments_like_the_jvm": invokeRejectsBadArgumentsLikeTheJvm(); break;
            case "invoke_checks_the_receiver": invokeChecksTheReceiver(); break;
            case "invoke_wraps_thrown_exceptions": invokeWrapsThrownExceptions(); break;
            case "access_checks_stay_in_java": accessChecksStayInJava(); break;
            case "field_get_and_set_convert_like_the_jvm": fieldGetAndSetConvertLikeTheJvm(); break;
            case "field_access_rejects_what_the_jvm_rejects": fieldAccessRejectsWhatTheJvmRejects(); break;
            case "enum_constants_through_reflection": enumConstantsThroughReflection(); break;
            case "absent_annotations_read_as_empty": annotationsAbsentReadAsNull(); break;
            case "gson_shaped_round_trip": gsonShapedRoundTrip(); break;
            default: throw new IllegalArgumentException("unknown mechanism " + args[0]);
        }
    }
}
