// java.lang.management on the reference JVM and natively (tests/management.rs): each mechanism is
// one run, `ManagementCases <mechanism>`. Deterministic values are printed as they are and
// compared with the JVM; clocks and ids are printed as the properties they must have.
// sun.management is used directly (compiled and run with it exported, see tests/management.rs):
// JABS compiles in only code something calls, so reflection could not find it.
import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryUsage;
import java.lang.management.OperatingSystemMXBean;
import java.lang.management.RuntimeMXBean;
import java.lang.management.ThreadMXBean;
import java.lang.reflect.Method;
import java.util.List;
import sun.management.ManagementFactoryHelper;
import sun.management.VMManagement;

class ManagementCases {
    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("startTimeAndInputArguments")) startTimeAndInputArguments();
        else if (mechanism.equals("managementVersion")) managementVersion();
        else if (mechanism.equals("uptimeAdvances")) uptimeAdvances();
        else if (mechanism.equals("supportFlags")) supportFlags();
        else if (mechanism.equals("processId")) processId(mechanism.isEmpty());
        else if (mechanism.equals("threadCounts")) threadCounts();
        else if (mechanism.equals("classCounts")) classCounts();
        else if (mechanism.equals("heapUsage")) heapUsage();
        else if (mechanism.equals("operatingSystem")) operatingSystem();
        else throw new IllegalArgumentException("no mechanism " + mechanism);
    }

    // log4j's OnStartupTriggeringPolicy and Minecraft's snooper: the JVM's start time and the
    // -D options it was given ("-Dkey=value", "-Dkey"), in order.
    static void startTimeAndInputArguments() {
        long now = System.currentTimeMillis();
        RuntimeMXBean runtime = ManagementFactory.getRuntimeMXBean();
        long start = runtime.getStartTime();
        System.out.println("start before now, within a minute: " + (start <= now && now - start < 60000));
        List<String> arguments = runtime.getInputArguments();
        System.out.println(arguments.size() + " " + arguments);
    }

    static void managementVersion() {
        System.out.println(ManagementFactory.getRuntimeMXBean().getManagementSpecVersion());
    }

    // Uptime is monotonic, follows a sleep, and is about the wall time since the start time
    // (HotSpot counts it from slightly before the start time, so allow a second).
    static void uptimeAdvances() throws InterruptedException {
        RuntimeMXBean runtime = ManagementFactory.getRuntimeMXBean();
        long first = runtime.getUptime();
        Thread.sleep(100);
        long second = runtime.getUptime();
        long wall = System.currentTimeMillis() - runtime.getStartTime();
        System.out.println("non-negative: " + (first >= 0));
        // A sleep lasts only about its time on a loaded machine, on the JVM too: ask for half.
        System.out.println("advances by the sleep: " + (second - first >= 50));
        System.out.println("close to wall time since start: " + (second <= wall + 1000 && second >= wall - 1000));
    }

    // The optional-support flags initOptionalSupportFields sets, through VMManagement's getters.
    // (isGcNotificationSupported is left out: JDK 25 answers it by looking for
    // com.sun.management.GarbageCollectorMXBean, not from the flag.)
    static void supportFlags() {
        VMManagement vm = ManagementFactoryHelper.getVMManagement();
        System.out.println("isCompilationTimeMonitoringSupported " + vm.isCompilationTimeMonitoringSupported());
        System.out.println("isThreadContentionMonitoringSupported " + vm.isThreadContentionMonitoringSupported());
        System.out.println("isCurrentThreadCpuTimeSupported " + vm.isCurrentThreadCpuTimeSupported());
        System.out.println("isOtherThreadCpuTimeSupported " + vm.isOtherThreadCpuTimeSupported());
        System.out.println("isObjectMonitorUsageSupported " + vm.isObjectMonitorUsageSupported());
        System.out.println("isSynchronizerUsageSupported " + vm.isSynchronizerUsageSupported());
        System.out.println("isThreadAllocatedMemorySupported " + vm.isThreadAllocatedMemorySupported());
        System.out.println("isRemoteDiagnosticCommandsSupported " + vm.isRemoteDiagnosticCommandsSupported());
    }

    // The pid the VM reports (VMManagementImpl.getProcessId, private: reached by reflection); the
    // harness compares it with the id of the process it started. getVmId is its only caller and
    // needs the host name (networking), so it is only named here, to compile getProcessId in.
    static void processId(boolean never) throws Exception {
        VMManagement vm = ManagementFactoryHelper.getVMManagement();
        if (never) System.out.println(vm.getVmId());
        Method m = vm.getClass().getDeclaredMethod("getProcessId");
        m.setAccessible(true);
        System.out.println(m.invoke(vm));
    }

    static final Object gate = new Object();
    static int waiting;
    static boolean open;

    // Named: an unnamed Thread numbers itself through Unsafe.staticFieldBase, which JABS lacks.
    static Thread blocked(boolean daemon, String name) {
        Thread t = new Thread(() -> {
            synchronized (gate) {
                waiting++;
                gate.notifyAll();
                while (!open) {
                    try { gate.wait(); } catch (InterruptedException e) { return; }
                }
            }
        }, name);
        t.setDaemon(daemon);
        return t;
    }

    // ThreadMXBean counts around 3 started threads (1 daemon) that are alive at once, then joined.
    // The JVM has system threads JABS does not, so differences from the start are printed.
    static void threadCounts() throws InterruptedException {
        ThreadMXBean threads = ManagementFactory.getThreadMXBean();
        threads.resetPeakThreadCount();
        int live = threads.getThreadCount(), daemon = threads.getDaemonThreadCount();
        long started = threads.getTotalStartedThreadCount();
        System.out.println("peak after reset is live: " + (threads.getPeakThreadCount() == live));
        Thread[] all = { blocked(false, "first"), blocked(true, "daemon"), blocked(false, "third") };
        for (Thread t : all) t.start();
        synchronized (gate) {
            while (waiting < all.length) gate.wait();
        }
        System.out.println("running: live +" + (threads.getThreadCount() - live)
            + " daemon +" + (threads.getDaemonThreadCount() - daemon)
            + " peak +" + (threads.getPeakThreadCount() - live)
            + " started +" + (threads.getTotalStartedThreadCount() - started));
        synchronized (gate) {
            open = true;
            gate.notifyAll();
        }
        for (Thread t : all) t.join();
        System.out.println("joined: live +" + (threads.getThreadCount() - live)
            + " daemon +" + (threads.getDaemonThreadCount() - daemon)
            + " peak +" + (threads.getPeakThreadCount() - live)
            + " started +" + (threads.getTotalStartedThreadCount() - started));
    }

    static byte[] kept;

    // MemoryMXBean's heap usage is consistent and follows a large allocation that stays reachable.
    static void heapUsage() {
        MemoryUsage before = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        kept = new byte[16 << 20];
        MemoryUsage after = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        System.out.println("committed covers used: " + (after.getCommitted() >= after.getUsed()));
        System.out.println("max undefined or covers committed: " + (after.getMax() == -1 || after.getMax() >= after.getCommitted()));
        System.out.println("used grows by the allocation: " + (after.getUsed() - before.getUsed() >= (8 << 20)) + " " + kept.length);
    }

    // OperatingSystemMXBean's basics: the system properties, the processor count Runtime gives,
    // and whether the host keeps a load average (Windows does not: -1).
    static void operatingSystem() {
        OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        System.out.println(os.getName().equals(System.getProperty("os.name")) + " " + os.getArch().equals(System.getProperty("os.arch"))
            + " " + os.getVersion().equals(System.getProperty("os.version")));
        System.out.println("processors as Runtime: " + (os.getAvailableProcessors() == Runtime.getRuntime().availableProcessors()));
        double load = os.getSystemLoadAverage();
        System.out.println("load average: " + (load < 0 ? "none " + load : "non-negative"));
    }

    static class Fresh {
        static int value = Integer.parseInt("7");
    }

    // Loaded classes are counted and none unloaded; initializing a class adds to the
    // initialized count (VMManagementImpl.getInitializedClassCount).
    static void classCounts() throws Exception {
        ClassLoadingMXBean classes = ManagementFactory.getClassLoadingMXBean();
        long loaded = classes.getLoadedClassCount(), total = classes.getTotalLoadedClassCount();
        System.out.println("loaded: " + (loaded > 0) + " unloaded: " + classes.getUnloadedClassCount()
            + " total is loaded + unloaded: " + (total == loaded + classes.getUnloadedClassCount()));
        VMManagement vm = ManagementFactoryHelper.getVMManagement();
        long before = vm.getInitializedClassCount();
        int value = Fresh.value;
        long after = vm.getInitializedClassCount();
        System.out.println("initialized grows: " + (before > 0 && after > before) + " " + value);
    }
}
