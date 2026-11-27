import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.LongBuffer;
import jdk.internal.loader.BootLoader;
import jdk.internal.module.Modules;
import jdk.internal.perf.Perf;

public class BootExtrasCases {
    sealed abstract static class Shape permits Circle, Rect {}
    static final class Circle extends Shape {}
    static non-sealed class Rect extends Shape {}

    sealed interface Node permits Leaf, Branch {}
    static final class Leaf implements Node {}
    sealed interface Branch extends Node permits SubBranch {}
    static final class SubBranch implements Branch {}

    sealed abstract static class GhostShape permits Ghost, GhostRect {}
    static final class Ghost extends GhostShape {}
    static non-sealed class GhostRect extends GhostShape {}

    static class Plain {}

    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        switch (mechanism) {
            case "perfBuffers" -> perfBuffers();
            case "perfNulls" -> perfNulls();
            case "perfClock" -> perfClock();
            case "perfDetach" -> perfDetach();
            case "perfEdgeErrors" -> perfEdgeErrors();
            case "sealedClasses" -> sealedClasses();
            case "bootLoaderAndLibs" -> bootLoaderAndLibs();
            case "bootPackages" -> bootPackages();
            case "jabsPerfExtras" -> jabsPerfExtras();
            default -> throw new IllegalArgumentException("unknown mechanism: " + mechanism);
        }
    }

    static void perfBuffers() {
        Perf p = Perf.getPerf();
        System.out.println("perfNonNull=" + (p != null));

        long initial = 0x0102030405060708L;
        ByteBuffer lb = p.createLong("jabs.test.long", 1, 1, initial);
        System.out.println("longDirect=" + lb.isDirect() + " cap=" + lb.capacity());
        System.out.println("defaultOrder=" + lb.order());
        lb.order(ByteOrder.nativeOrder());
        System.out.println("initialNative=0x" + Long.toHexString(lb.getLong(0)));

        LongBuffer view = lb.asLongBuffer();
        view.put(0, -123456789012345L);
        System.out.println("updatedLong=" + lb.getLong(0) + " view=" + view.get(0));

        byte[] payload = new byte[] { 10, 20, 30, 0 };
        ByteBuffer ba = p.createByteArray("jabs.test.bytes", 3, 5, payload, 8);
        System.out.println("baDirect=" + ba.isDirect() + " capGe=" + (ba.capacity() >= payload.length));
        System.out.println(
            "baBytes=" + ba.get(0) + "," + ba.get(1) + "," + ba.get(2) + "," + ba.get(3)
        );

        ByteBuffer sb = p.createString("jabs.test.str", 1, 5, "hello");
        System.out.println("strDirect=" + sb.isDirect() + " capGe=" + (sb.capacity() >= 6));
        StringBuilder decoded = new StringBuilder();
        for (int i = 0; i < 5; i++) {
            decoded.append((char) sb.get(i));
        }
        System.out.println("str=" + decoded + " nul=" + (sb.get(5) == 0));
    }

    static void perfNulls() {
        Perf p = Perf.getPerf();
        try {
            p.createLong(null, 1, 1, 42L);
            System.out.println("createLongNull=ok");
        } catch (NullPointerException e) {
            System.out.println("createLongNull=NPE");
        }
        try {
            p.createByteArray(null, 1, 5, new byte[] { 1, 0 }, 2);
            System.out.println("createBANameNull=ok");
        } catch (NullPointerException e) {
            System.out.println("createBANameNull=NPE");
        }
        try {
            p.createByteArray("n", 1, 5, null, 0);
            System.out.println("createBADataNull=ok");
        } catch (NullPointerException e) {
            System.out.println("createBADataNull=NPE");
        }
    }

    static void perfClock() {
        Perf p = Perf.getPerf();
        long freq = p.highResFrequency();
        long c1 = p.highResCounter();
        long c2 = p.highResCounter();
        System.out.println("freqPos=" + (freq > 0));
        System.out.println("counterPos=" + (c1 > 0) + " monotonic=" + (c2 >= c1));
    }

    static void perfDetach() throws Exception {
        Perf p = Perf.getPerf();
        ByteBuffer lb = p.createLong("jabs.test.detach", 1, 1, 7L);
        Method detach = Perf.class.getDeclaredMethod("detach", ByteBuffer.class);
        detach.setAccessible(true);
        detach.invoke(p, lb);
        System.out.println("detach=ok");
    }

    static void perfEdgeErrors() {
        Perf p = Perf.getPerf();
        System.out.println("negMaxCap=" + p.createByteArray("badMax", 3, 5, new byte[] { 1 }, -1).capacity());
        try {
            p.createByteArray("badMono", 2, 5, new byte[] { 1 }, 4);
            System.out.println("badMono=ok");
        } catch (IllegalArgumentException e) {
            System.out.println("badMono=IAE");
        }
        try {
            p.createByteArray("badBAUnits", 1, 1, new byte[] { 1 }, 4);
            System.out.println("badBAUnits=ok");
        } catch (IllegalArgumentException e) {
            System.out.println("badBAUnits=IAE");
        }
        try {
            p.createLong("badVar", 0, 1, 0L);
            System.out.println("badVar=ok");
        } catch (IllegalArgumentException e) {
            System.out.println("badVar=IAE");
        }
        try {
            p.createLong("badUnits", 1, 0, 0L);
            System.out.println("badUnits=ok");
        } catch (IllegalArgumentException e) {
            System.out.println("badUnits=IAE");
        }
        try {
            p.createLong("badU7", 1, 7, 0L);
            System.out.println("badU7=ok");
        } catch (IllegalArgumentException e) {
            System.out.println("badU7=IAE");
        }
        ByteBuffer trunc = p.createByteArray("trunc", 3, 5, new byte[] { 'a', 'b', 'c', 0 }, 2);
        System.out.println("truncCap=" + trunc.capacity() + " b0=" + trunc.get(0) + " b2=" + trunc.get(2));
    }

    static void sealedClasses() {
        printPermitted("Shape", Shape.class);
        printPermitted("GhostShape", GhostShape.class);
        printPermitted("Node", Node.class);
        Class<?>[] nodeSubs = Node.class.getPermittedSubclasses();
        printPermitted("Branch", nodeSubs[1]);
        printPermitted("Plain", Plain.class);
        printPermitted("int", int.class);
        printPermitted("String[]", String[].class);
    }

    static void printPermitted(String label, Class<?> cls) {
        Class<?>[] subs = cls.getPermittedSubclasses();
        if (subs == null) {
            System.out.println(label + ": sealed=" + cls.isSealed() + " permitted=null");
            return;
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < subs.length; i++) {
            if (i > 0) sb.append(',');
            sb.append(subs[i].getSimpleName());
        }
        System.out.println(label + ": sealed=" + cls.isSealed() + " permitted=[" + sb + "]");
    }

    static void bootLoaderAndLibs() throws Exception {
        System.out.println("missingPkg=" + BootLoader.getDefinedPackage("no.such.pkg"));

        Method pkgLoc = BootLoader.class.getDeclaredMethod("getSystemPackageLocation", String.class);
        pkgLoc.setAccessible(true);
        System.out.println("pkgLocMissing=" + pkgLoc.invoke(null, "no/such/pkg"));

        Class<?> nib = Class.forName("jdk.internal.jimage.NativeImageBuffer");
        Method getNativeMap = nib.getDeclaredMethod("getNativeMap", String.class);
        getNativeMap.setAccessible(true);
        System.out.println("nativeMap=" + getNativeMap.invoke(null, "no_such_image"));

        Class<?> nl = Class.forName("jdk.internal.loader.NativeLibrary");
        Method findEntry = nl.getDeclaredMethod("findEntry0", long.class, String.class);
        findEntry.setAccessible(true);
        System.out.println("findEntry0=" + findEntry.invoke(null, 0L, "no_such_symbol"));
    }

    static void bootPackages() {
        System.out.println("missingPkg=" + BootLoader.getDefinedPackage("no.such.pkg"));
        Package javaLang = BootLoader.getDefinedPackage("java.lang");
        Package strPkg = String.class.getPackage();
        System.out.println("javaLang=" + (javaLang != null ? javaLang.getName() : null));
        System.out.println("strPkg=" + (strPkg != null ? strPkg.getName() : null)
            + " same=" + (javaLang == strPkg)
            + " mod=" + String.class.getModule().getName());
        System.out.println("appPkg=" + BootExtrasCases.class.getPackage().getName());
        System.out.println("modBase=" + Modules.findLoadedModule("java.base").isPresent());
        System.out.println("modNone=" + Modules.findLoadedModule("no.such.module").isPresent());
        try {
            Modules.findLoadedModule(null);
            System.out.println("modNull=ok");
        } catch (NullPointerException e) {
            System.out.println("modNull=NPE");
        }
    }

    static void jabsPerfExtras() throws Exception {
        Perf p = Perf.getPerf();
        System.out.println("freq=" + p.highResFrequency());
        try {
            p.attach(0);
            System.out.println("attach0=ok");
        } catch (java.io.IOException e) {
            System.out.println("attach0=IOException");
        }
        try {
            p.createByteArray("badNeg2", 3, 5, new byte[] { 1 }, -2);
            System.out.println("badNeg2=ok");
        } catch (IllegalArgumentException e) {
            System.out.println("badNeg2=IAE");
        }
        Method detach = Perf.class.getDeclaredMethod("detach", ByteBuffer.class);
        detach.setAccessible(true);
        detach.invoke(p, (Object) null);
        System.out.println("detachNull=ok");
    }
}
