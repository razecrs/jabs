//! java.lang.management natives: what `ManagementFactory`'s platform beans ask the VM
//! (sun.management.VMManagementImpl, ThreadImpl). Each answer is what HotSpot reports on the same
//! machine where JABS can know it, a documented fact about JABS where the HotSpot answer
//! describes machinery JABS does not have (no JIT, no GC, no safepoints), and otherwise nothing:
//! a native left out here stops the program with a diagnostic when it is called, rather than
//! invent a number. The C side is runtime/jabs_management.c, the thread counts are kept in
//! runtime/jabs_thread.c and the clock and process id come from runtime/jabs_platform.c.
//!
//! Left out on purpose (they stop when called):
//! - `getLoadedClassSize`: the bytes HotSpot's class metadata takes; JABS has no such metadata
//!   apart from the executable's data, and no count of it.
//! - `getClassInitializationTime`: not measured.
//! - `getTotalApplicationNonStoppedTime`: HotSpot's ticks between safepoints; no such clock.
//! - the AOT cache natives (`getAOTMode`, `isAOTRecording`, `getAOTRecordingDuration`,
//!   `endAOTRecording`): HotSpot reports its AOT mode setting ("auto"), which JABS does not have.

use super::*;

const VM_MANAGEMENT: &str = "sun/management/VMManagementImpl";
const THREAD_IMPL: &str = "sun/management/ThreadImpl";
const MEMORY_IMPL: &str = "sun/management/MemoryImpl";
const MEMORY_USAGE_DESCRIPTOR: &str = "(Z)Ljava/lang/management/MemoryUsage;";
const MEMORY_USAGE: &str = "java/lang/management/MemoryUsage";
const MEMORY_USAGE_INIT: &str = "(JJJJ)V";

/// An object a management native constructs from C (no Java instruction names it), so
/// reachable() keeps its class and constructor: (class, constructor descriptor).
pub(super) fn constructed_by(key: &MethodKey) -> Option<(&'static str, &'static str)> {
    let memory = key.0 == MEMORY_IMPL && key.1 == "getMemoryUsage0";
    (memory && key.2 == MEMORY_USAGE_DESCRIPTOR).then_some((MEMORY_USAGE, MEMORY_USAGE_INIT))
}

/// C glue the management natives call: jabs_management_memory_usage builds a MemoryUsage with
/// its own constructor (which checks the four values as on the JVM). Without the class, its
/// constructor or its initialization in the program, it stops when called.
pub(super) fn glue(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    records: &BTreeSet<String>,
    inits: &BTreeSet<String>,
) -> String {
    let signature = "jabs_object *jabs_management_memory_usage(int64_t init, int64_t used, int64_t committed, int64_t max)";
    let ctor = (
        MEMORY_USAGE.to_string(),
        "<init>".to_string(),
        MEMORY_USAGE_INIT.to_string(),
    );
    let complete = unit.get(MEMORY_USAGE).is_some()
        && records.contains(MEMORY_USAGE)
        && inits.contains(MEMORY_USAGE)
        && methods.contains(&ctor);
    if !complete {
        return format!(
            "\n{signature} {{ (void)init; (void)used; (void)committed; (void)max; jabs_not_compiled(\"MemoryUsage (its class or constructor is not in the program)\"); }}\n"
        );
    }
    format!(
        "\n/* MemoryImpl.getMemoryUsage0's result (see src/cgen/management.rs). */\n{signature} {{\n    if ({init}() != 0) return NULL;\n    jabs_object *usage = jabs_new(&{record});\n    if (!usage) return NULL;\n    {ctor}(usage, init, used, committed, max);\n    return jabs_pending ? NULL : usage;\n}}\n",
        init = init_symbol(MEMORY_USAGE),
        record = class_record(MEMORY_USAGE),
        ctor = symbol(MEMORY_USAGE, "<init>", MEMORY_USAGE_INIT),
    )
}

/// The JVM management interface version HotSpot reports: JDK 25.0.4's VMManagementImpl.version
/// is "4.0" (read on the reference JVM; tests/management.rs compares it on every run).
const MANAGEMENT_VERSION: &str = "4.0";

/// VMManagementImpl's optional-support flags, all false for JABS, with why (HotSpot 25.0.4 on
/// the reference machine reports all but gcNotificationSupport true). The platform beans refuse the matching operations
/// (UnsupportedOperationException) instead of calling natives JABS does not have.
const SUPPORT_FLAGS: [(&str, &str); 9] = [
    (
        "compTimeMonitoringSupport",
        "no JIT: nothing is compiled at run time, so there is no compilation time",
    ),
    (
        "threadContentionMonitoringSupport",
        "blocked and waited times per thread are not recorded",
    ),
    (
        "currentThreadCpuTimeSupport",
        "per-thread CPU time natives (ThreadImpl.getThreadTotalCpuTime0) are not implemented",
    ),
    (
        "otherThreadCpuTimeSupport",
        "per-thread CPU time natives are not implemented",
    ),
    (
        "objectMonitorUsageSupport",
        "thread dumps with the monitors each thread holds are not implemented",
    ),
    (
        "synchronizerUsageSupport",
        "thread dumps with ownable synchronizers are not implemented",
    ),
    (
        "threadAllocatedMemorySupport",
        "bytes allocated per thread are not recorded",
    ),
    (
        "gcNotificationSupport",
        "no garbage collector, so no GC notifications (HotSpot's answer is false too)",
    ),
    (
        "remoteDiagnosticCommandsSupport",
        "no diagnostic commands (DiagnosticCommandImpl natives); false keeps its MBean away",
    ),
];

/// Java methods of the management classes JABS replaces: (C body, needs the class registry).
///
/// `ManagementFactoryHelper$LoggingMXBeanAccess.loadLoggingClass` finds a java.logging class
/// through `ModuleLayer.boot()`, which is null in JABS (the module system's boot, initPhase2,
/// does not run), so every ManagementFactory call failed with an ExceptionInInitializerError.
/// The replacement keeps the JDK's meaning: the class if java.logging is in the program, else
/// null (PlatformLoggingMXBean unavailable). A class with that name is compiled in exactly when
/// java.logging's is, and Class.forName(Module, String) does not initialize it, which
/// jabs_class_for_binary_name does not either.
pub(super) fn replacement(key: &MethodKey) -> Option<(String, bool)> {
    let logging_access = key.0 == "sun/management/ManagementFactoryHelper$LoggingMXBeanAccess";
    if logging_access
        && key.1 == "loadLoggingClass"
        && key.2 == "(Ljava/lang/String;)Ljava/lang/Class;"
    {
        return Some(("return jabs_class_for_binary_name(a0);".into(), true));
    }
    None
}

/// The body of a management native, if JABS implements it: (C body, needs the class registry).
pub(super) fn intrinsic(
    unit: &Unit,
    key: &MethodKey,
    classes_available: bool,
) -> Option<(String, bool)> {
    if key.0 == THREAD_IMPL {
        return match (key.1.as_str(), key.2.as_str()) {
            ("resetPeakThreadCount0", "()V") => Some(("jabs_thread_reset_peak();".into(), false)),
            _ => None,
        };
    }
    // MemoryMXBean's heap usage. JABS's heap is every object malloc'd so far (jabs_heap_bytes);
    // with no GC nothing is returned, so all of it is both used and committed. There is no
    // initial size and no limit: both are undefined (-1), as MemoryUsage allows. Non-heap memory
    // (code and class data) is the executable itself, which JABS does not measure: it stops.
    if key.0 == MEMORY_IMPL && key.1 == "getMemoryUsage0" && key.2 == MEMORY_USAGE_DESCRIPTOR {
        return Some((
            "if (!a0) jabs_not_compiled(\"MemoryImpl.getMemoryUsage0(false): non-heap memory usage is not measured\"); int64_t used = atomic_load_explicit(&jabs_heap_bytes, memory_order_relaxed); return jabs_management_memory_usage(-1, used, used, -1);".into(),
            false,
        ));
    }
    // OperatingSystemMXBean (jdk.management): initialize0 prepares the host queries of the
    // natives behind getProcessCpuLoad, getTotalMemorySize and so on. JABS implements none of
    // those (each stops when called), so there is nothing to prepare.
    if key.0 == "com/sun/management/internal/OperatingSystemImpl"
        && key.1 == "initialize0"
        && key.2 == "()V"
    {
        return Some((
            "/* nothing: the host queries it prepares are not implemented */".into(),
            false,
        ));
    }
    // OperatingSystemMXBean.getSystemLoadAverage, through Unsafe.getLoadAverage (which checked
    // 1 <= a1 <= 3 and the array's length): the host's load averages, or -1 where the host has
    // none (Windows, as the reference JVM reports there).
    if key.0 == "jdk/internal/misc/Unsafe" && key.1 == "getLoadAverage0" && key.2 == "([DI)I" {
        return Some((
            "if (!a0 || a1 < 1 || a1 > 3 || ((jabs_array *)a0)->length < a1) jabs_not_compiled(\"Unsafe.getLoadAverage0 with arguments Unsafe.getLoadAverage rejects\"); return jabs_platform_load_average((double *)jabs_array_data((jabs_array *)a0), a1);".into(),
            false,
        ));
    }
    if key.0 != VM_MANAGEMENT {
        return None;
    }
    let (body, needs_classes): (String, bool) = match (key.1.as_str(), key.2.as_str()) {
        ("getVersion0", "()Ljava/lang/String;") => (
            format!("return jabs_string_from_utf8(\"{MANAGEMENT_VERSION}\");"),
            true,
        ),
        ("initOptionalSupportFields", "()V") => (support_fields_body(unit), false),
        // The matching support flags are false, so nothing can enable these.
        ("isThreadContentionMonitoringEnabled", "()Z")
        | ("isThreadCpuTimeEnabled", "()Z")
        | ("isThreadAllocatedMemoryEnabled", "()Z") => ("return 0;".into(), false),
        // No -verbose:class or -verbose:gc: a JABS program takes only -D options.
        ("getVerboseClass", "()Z") | ("getVerboseGC", "()Z") => ("return 0;".into(), false),
        ("getTotalClassCount", "()J") => ("return jabs_class_record_count;".into(), true),
        // Compiled-in classes are never unloaded.
        ("getUnloadedClassCount", "()J") => ("return 0;".into(), false),
        ("getInitializedClassCount", "()J") => {
            ("return jabs_class_initialized_count();".into(), false)
        }
        ("getProcessId", "()I") => (
            "int64_t pid = jabs_platform_process_id(); if (pid < 0 || pid > INT32_MAX) jabs_not_compiled(\"VMManagementImpl.getProcessId: a process id outside int\"); return (int32_t)pid;".into(),
            false,
        ),
        ("getVmArguments0", "()[Ljava/lang/String;") => {
            ("return jabs_management_vm_arguments();".into(), true)
        }
        ("getStartupTime", "()J") => ("return jabs_platform_start_millis();".into(), false),
        ("getUptime0", "()J") => ("return jabs_platform_uptime_millis();".into(), false),
        ("getAvailableProcessors", "()I") => {
            ("return jabs_thread_processor_count();".into(), false)
        }
        ("getTotalThreadCount", "()J") => (
            "return jabs_thread_management_counts().started;".into(),
            false,
        ),
        ("getLiveThreadCount", "()I") => (thread_count_body("live"), false),
        ("getPeakThreadCount", "()I") => (thread_count_body("peak"), false),
        ("getDaemonThreadCount", "()I") => (thread_count_body("daemon"), false),
        // No JIT: no compilation ever runs. (CompilationMXBean itself is absent: JABS does not
        // set sun.management.compiler, as a VM without a JIT does not.)
        ("getTotalCompileTime", "()J") => ("return 0;".into(), false),
        // JABS never stops all threads at a safepoint: there is no GC, deoptimization or
        // class redefinition to stop them for.
        ("getSafepointCount", "()J")
        | ("getTotalSafepointTime", "()J")
        | ("getSafepointSyncTime", "()J") => ("return 0;".into(), false),
        // Nothing is unloaded, no method profiles exist (no JIT), every class is verified
        // and loaded when the program is built, so none of these take bytes or time at run time.
        ("getUnloadedClassSize", "()J")
        | ("getMethodDataSize", "()J")
        | ("getClassVerificationTime", "()J")
        | ("getClassLoadingTime", "()J") => ("return 0;".into(), false),
        _ => return None,
    };
    (classes_available || !needs_classes).then_some((body, needs_classes))
}

/// One of the thread counts as a Java int; more than an int can hold stops (HotSpot's counts are
/// ints too, and no process gets there).
fn thread_count_body(field: &str) -> String {
    format!(
        "int64_t n = jabs_thread_management_counts().{field}; if (n < 0 || n > INT32_MAX) jabs_not_compiled(\"VMManagementImpl thread count outside int (jabs_thread_management_counts in runtime/jabs_thread.c)\"); return (int32_t)n;"
    )
}

/// initOptionalSupportFields: every flag in SUPPORT_FLAGS stored false. A flag JABS has not
/// decided (a JDK that adds one) or one missing from the class stops when the class
/// initializes, naming it, rather than leave a guess in place.
fn support_fields_body(unit: &Unit) -> String {
    let Some(class) = unit.get(VM_MANAGEMENT) else {
        return "jabs_not_compiled(\"VMManagementImpl.initOptionalSupportFields: the class is not in the program\");".into();
    };
    let is_flag = |f: &&crate::classfile::Member| {
        f.access & ACC_STATIC != 0 && f.descriptor == "Z" && f.name.ends_with("Support")
    };
    for f in class.fields.iter().filter(is_flag) {
        if !SUPPORT_FLAGS.iter().any(|(name, _)| *name == f.name) {
            return format!(
                "jabs_not_compiled(\"VMManagementImpl.initOptionalSupportFields: support flag {} is not decided (SUPPORT_FLAGS in src/cgen/management.rs)\");",
                f.name
            );
        }
    }
    let mut body = String::new();
    for (name, why) in SUPPORT_FLAGS {
        if !class.fields.iter().filter(is_flag).any(|f| f.name == name) {
            return format!(
                "jabs_not_compiled(\"VMManagementImpl.initOptionalSupportFields: no static boolean {name} (SUPPORT_FLAGS in src/cgen/management.rs)\");"
            );
        }
        let _ = write!(
            body,
            "{} = 0; /* {why} */ ",
            field_symbol(VM_MANAGEMENT, name)
        );
    }
    body
}
