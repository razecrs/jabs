// Exercises java.lang.StackWalker and StackStreamFactory behavior for tests/stack_walk.rs
// (`StackWalkCases <mechanism>`). Needs JDK 22+ for `Option.DROP_METHOD_INFO`.
import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;
import java.util.stream.Stream;

class StackWalkCases {
    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        switch (mechanism) {
            case "basicWalk":
                basicWalk();
                break;
            case "forEachWalk":
                forEachWalk();
                break;
            case "retainClassReference":
                retainClassReference();
                break;
            case "getCallerClassDirectInMain":
                System.out.println(
                        StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass());
                break;
            case "showReflectFrames":
                showReflectFrames();
                break;
            case "showHiddenFrames":
                showHiddenFrames();
                break;
            case "estimateDepth":
                estimateDepth();
                break;
            case "nestedWalk":
                nestedWalk();
                break;
            case "dropMethodInfo":
                dropMethodInfo();
                break;
            case "log4jCallerLookup":
                log4jCallerLookup();
                break;
            default:
                throw new IllegalArgumentException("no mechanism " + mechanism);
        }
    }

    private static String frameTag(StackFrame f) {
        return f.getClassName() + "." + f.getMethodName() + "(native=" + f.isNativeMethod() + ")";
    }

    static void basicWalk() {
        basicA();
    }

    static void basicA() {
        basicB();
    }

    static void basicB() {
        List<String> tags = StackWalker.getInstance().walk(s -> {
            List<String> out = new ArrayList<>();
            s.forEach(f -> {
                out.add(frameTag(f));
                StackTraceElement ste = f.toStackTraceElement();
                if (!ste.getClassName().equals(f.getClassName())
                        || !ste.getMethodName().equals(f.getMethodName())) {
                    throw new AssertionError("StackTraceElement mismatch: " + ste);
                }
            });
            return out;
        });
        for (String tag : tags) {
            System.out.println(tag);
        }
        StackFrame first = StackWalker.getInstance().walk(s -> s.findFirst().get());
        try {
            first.getDeclaringClass();
            System.out.println("unexpected getDeclaringClass");
        } catch (UnsupportedOperationException expected) {
            System.out.println("getDeclaringClass UOE");
        }
        try {
            first.getMethodType();
            System.out.println("unexpected getMethodType");
        } catch (UnsupportedOperationException expected) {
            System.out.println("getMethodType UOE");
        }
        try {
            first.getDescriptor();
            System.out.println("unexpected getDescriptor");
        } catch (UnsupportedOperationException expected) {
            System.out.println("getDescriptor UOE");
        }
    }

    static void forEachWalk() {
        forEachInner(2);
    }

    static void forEachInner(int depth) {
        if (depth > 0) {
            forEachInner(depth - 1);
            return;
        }
        StackWalker.getInstance().forEach(f -> System.out.println(frameTag(f)));
    }

    static void retainClassReference() {
        try {
            StackWalker.getInstance().getCallerClass();
            System.out.println("unexpected getCallerClass without RETAIN_CLASS_REFERENCE");
        } catch (UnsupportedOperationException expected) {
            System.out.println("getCallerClass UOE");
        }
        CallerHelper.callFromRetain();
        CallerHelper.withDescriptorClasses(null);
    }

    static final class UnusedParam {}

    static final class UnusedReturn {}

    static class CallerHelper {
        static UnusedReturn withDescriptorClasses(UnusedParam unused) {
            List<String> info = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).walk(s -> {
                List<String> out = new ArrayList<>();
                s.forEach(f -> out.add(
                        f.getDeclaringClass().getName()
                                + "."
                                + f.getMethodName()
                                + ":"
                                + f.getMethodType().descriptorString()
                                + ":"
                                + f.getDescriptor()));
                return out;
            });
            for (String line : info) {
                System.out.println(line);
            }
            return null;
        }

        static void callFromRetain() {
            Class<?> caller =
                    StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
            System.out.println("caller=" + caller.getName());
            indirectCaller();
        }

        static void indirectCaller() {
            Class<?> caller =
                    StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
            System.out.println("indirectCaller=" + caller.getName());
        }
    }

    static void showReflectFrames() throws Exception {
        Method m = StackWalkCases.class.getDeclaredMethod("reflectTarget");
        m.invoke(null);
    }

    static void reflectTarget() {
        List<String> def = StackWalker.getInstance().walk(s -> {
            List<String> out = new ArrayList<>();
            s.forEach(f -> out.add(frameTag(f)));
            return out;
        });
        System.out.println("default:");
        for (String line : def) {
            System.out.println("  " + line);
        }

        List<String> refl = StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES).walk(s -> {
            List<String> out = new ArrayList<>();
            s.forEach(f -> out.add(f.getClassName() + "." + f.getMethodName()));
            return out;
        });
        System.out.println("hasMethodInvoke=" + refl.contains("java.lang.reflect.Method.invoke"));
        System.out.println(
                "hasReflectTarget=" + refl.contains("StackWalkCases.reflectTarget"));
        System.out.println("hasShowReflectFrames=" + refl.contains("StackWalkCases.showReflectFrames"));
        System.out.println("hasMain=" + refl.contains("StackWalkCases.main"));
        boolean hasInternalReflect = false;
        for (String s : refl) {
            if (s.startsWith("jdk.internal.reflect.")) {
                hasInternalReflect = true;
            }
        }
        System.out.println("hasInternalReflect=" + hasInternalReflect);
    }

    static void showHiddenFrames() {
        Runnable r = () -> {
            List<String> def = StackWalker.getInstance().walk(s -> {
                List<String> out = new ArrayList<>();
                s.forEach(f -> out.add(f.getClassName() + "." + f.getMethodName()));
                return out;
            });
            System.out.println("defaultHasLambdaClass=" + containsLambda(def));
            for (String line : def) {
                System.out.println("  def: " + line);
            }

            List<String> hidden = StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES).walk(s -> {
                List<String> out = new ArrayList<>();
                s.forEach(f -> out.add(f.getClassName() + "." + f.getMethodName()));
                return out;
            });
            System.out.println("hiddenHasLambdaClass=" + containsLambda(hidden));
            System.out.println(
                    "hiddenHasCaller=" + hidden.contains("StackWalkCases.showHiddenFrames"));
            System.out.println("hiddenHasMain=" + hidden.contains("StackWalkCases.main"));

            Class<?> caller =
                    StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
            System.out.println("lambdaCaller=" + caller.getName());
            lambdaTarget();
        };
        r.run();
    }

    static void lambdaTarget() {
        Class<?> caller =
                StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
        System.out.println("lambdaTargetCaller=" + caller.getName());
    }

    private static boolean containsLambda(List<String> frames) {
        for (String f : frames) {
            if (f.contains("$$Lambda")) {
                return true;
            }
        }
        return false;
    }

    static void estimateDepth() {
        recurseAndWalk(12);
    }

    static void recurseAndWalk(int n) {
        if (n > 0) {
            recurseAndWalk(n - 1);
            return;
        }
        for (int est : new int[] {1, 2, 7, 64}) {
            StackWalker w = StackWalker.getInstance(EnumSet.noneOf(Option.class), est);
            List<String> frames = w.walk(s -> {
                List<String> out = new ArrayList<>();
                s.forEach(f -> out.add(f.getClassName() + "." + f.getMethodName()));
                return out;
            });
            System.out.println("est=" + est + " count=" + frames.size());
            if (est == 1) {
                for (String f : frames) {
                    System.out.println("  " + f);
                }
            }
        }
        try {
            StackWalker.getInstance(EnumSet.noneOf(Option.class), 0);
            System.out.println("unexpected est=0");
        } catch (IllegalArgumentException expected) {
            System.out.println("est=0 IAE");
        }
    }

    static void nestedWalk() {
        StackWalker w1 = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
        StackWalker w2 = StackWalker.getInstance();
        String result = w1.walk(s1 -> {
            List<String> outerFirst = new ArrayList<>();
            s1.limit(2).forEach(f -> outerFirst.add(f.getClassName() + "." + f.getMethodName()));
            List<String> inner = w2.walk(s2 -> {
                List<String> out = new ArrayList<>();
                s2.forEach(f -> out.add(f.getClassName() + "." + f.getMethodName()));
                return out;
            });
            boolean sawOuterClass = inner.contains("StackWalkCases.nestedWalk")
                    && inner.contains("StackWalkCases.main");
            return outerFirst + "|sawOuter=" + sawOuterClass;
        });
        System.out.println(result);

        // Escaping stream must fail with IllegalStateException once walk() closes the anchor.
        Stream<StackFrame> escaped = StackWalker.getInstance().walk(s -> s);
        try {
            escaped.count();
            System.out.println("unexpected escaped stream count");
        } catch (IllegalStateException expected) {
            System.out.println("escaped stream ISE");
        }
    }

    static void dropMethodInfo() {
        StackWalker w = StackWalker.getInstance(
                EnumSet.of(Option.RETAIN_CLASS_REFERENCE, Option.DROP_METHOD_INFO));
        List<String> classes = w.walk(s -> {
            List<String> out = new ArrayList<>();
            s.forEach(f -> {
                if (out.isEmpty()) {
                    try {
                        f.getMethodName();
                        System.out.println("unexpected getMethodName");
                    } catch (UnsupportedOperationException expected) {
                        System.out.println("dropMethodInfo getMethodName UOE");
                    }
                }
                out.add(f.getDeclaringClass().getName());
            });
            return out;
        });
        for (String c : classes) {
            System.out.println(c);
        }
    }

    // ---- Log4j2-style caller lookup ----
    static void log4jCallerLookup() {
        MyAppService.handleRequest();
    }

    static class MyAppService {
        private static final Logger LOG = new LoggerWrapper();

        static void handleRequest() {
            LOG.info("hello");
            LOG.debug("world");
        }
    }

    interface Logger {
        void info(String msg);
        void debug(String msg);
    }

    static abstract class AbstractLogger implements Logger {
        protected void logIfEnabled(String fqcn, String level, String msg) {
            logMessage(fqcn, level, msg);
        }

        protected void logMessage(String fqcn, String level, String msg) {
            StackFrame loc = StackLocator.calcLocation(fqcn);
            Class<?> caller0 = StackLocator.getCallerClass(1);
            Class<?> callerFromAnchor = StackLocator.getCallerClass(LoggerWrapper.class);
            System.out.println(
                    level
                            + " loc="
                            + loc.getClassName()
                            + "."
                            + loc.getMethodName()
                            + " decl="
                            + loc.getDeclaringClass().getName()
                            + " c1="
                            + caller0.getName()
                            + " anchorCaller="
                            + callerFromAnchor.getName());
        }
    }

    static class LoggerWrapper extends AbstractLogger {
        private static final String FQCN = LoggerWrapper.class.getName();

        @Override
        public void info(String msg) {
            logIfEnabled(FQCN, "INFO", msg);
        }

        @Override
        public void debug(String msg) {
            logIfEnabled(FQCN, "DEBUG", msg);
        }
    }

    static final class StackLocator {
        private static final StackWalker WALKER =
                StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);

        static StackFrame calcLocation(String fqcnOfLogger) {
            return WALKER.walk(s -> s.dropWhile(f -> !f.getClassName().equals(fqcnOfLogger))
                    .dropWhile(f -> f.getClassName().equals(fqcnOfLogger))
                    .findFirst()
                    .orElse(null));
        }

        static Class<?> getCallerClass(int depth) {
            return WALKER.walk(s -> s.skip(depth)
                    .map(StackFrame::getDeclaringClass)
                    .findFirst()
                    .orElse(null));
        }

        static Class<?> getCallerClass(Class<?> sentinel) {
            return WALKER.walk(s -> s.dropWhile(f -> f.getDeclaringClass() != sentinel)
                    .dropWhile(f -> f.getDeclaringClass() == sentinel)
                    .map(StackFrame::getDeclaringClass)
                    .findFirst()
                    .orElse(null));
        }
    }
}
