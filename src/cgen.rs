//! C emission for a compilation unit: a closed set of classes compiled together.
//!
//! A method compiles only when every instruction is understood, a bytecode
//! verifier proves every path type-correct against the unit's class hierarchy
//! (references become C pointers, so this is a memory-safety requirement), and
//! everything it depends on also compiles: the methods it may call (for a
//! virtual call, every implementation it can dispatch to) and the classes it
//! may initialize. Classes outside the unit are opaque: their references can be
//! passed around, but nothing is called or read through them. Every method
//! that does not compile is reported with a reason; nothing is guessed.
//!
//! Objects have a class-record header; a subclass's struct embeds its
//! superclass's struct first, and vtables follow Java's override rules. There
//! is no garbage collector yet: objects live until the process exits.

use crate::bytecode::{
    self, BinOp, Compare, Cond, Convert, DecodeError, Insn, Kind, MemberRef, Op, Value,
};
use crate::classfile::{
    ACC_ABSTRACT, ACC_FINAL, ACC_INTERFACE, ACC_NATIVE, ACC_PRIVATE, ACC_STATIC, ACC_SYNCHRONIZED,
    ACC_VOLATILE, ClassFile, Constant, Member,
};
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use std::{
    cell::RefCell,
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Write,
    sync::{Arc, RwLock},
};

mod boot_extras;
mod management;
mod method_handles;
mod modules;
mod net;
mod nio_fs;
mod reflect;
mod stack;
mod symbols;
mod unsafe_rt;
mod zip;

pub const RUNTIME_PRELUDE: &str = include_str!("../runtime/jabs_rt.h");
const OBJECT: &str = "java/lang/Object";
const STRING: &str = "java/lang/String";
const THROWABLE: &str = "java/lang/Throwable";
const CLASS: &str = "java/lang/Class";
const THREAD: &str = "java/lang/Thread";

/// Exceptions the runtime raises itself, in the order of the JABS_VM_* constants.
const VM_EXCEPTIONS: [(&str, &str); 19] = [
    ("java/lang/NullPointerException", "JABS_VM_NULL_POINTER"),
    ("java/lang/ArithmeticException", "JABS_VM_ARITHMETIC"),
    (
        "java/lang/ArrayIndexOutOfBoundsException",
        "JABS_VM_ARRAY_INDEX",
    ),
    (
        "java/lang/NegativeArraySizeException",
        "JABS_VM_NEGATIVE_ARRAY_SIZE",
    ),
    ("java/lang/ArrayStoreException", "JABS_VM_ARRAY_STORE"),
    ("java/lang/ClassCastException", "JABS_VM_CLASS_CAST"),
    (
        "java/lang/IncompatibleClassChangeError",
        "JABS_VM_INCOMPATIBLE_CLASS_CHANGE",
    ),
    ("java/lang/AbstractMethodError", "JABS_VM_ABSTRACT_METHOD"),
    (
        "java/lang/NoClassDefFoundError",
        "JABS_VM_NO_CLASS_DEF_FOUND",
    ),
    (
        "java/lang/ExceptionInInitializerError",
        "JABS_VM_INITIALIZER",
    ),
    ("java/lang/OutOfMemoryError", "JABS_VM_OUT_OF_MEMORY"),
    (
        "java/lang/IllegalMonitorStateException",
        "JABS_VM_ILLEGAL_MONITOR_STATE",
    ),
    (
        "java/lang/CloneNotSupportedException",
        "JABS_VM_CLONE_NOT_SUPPORTED",
    ),
    ("java/io/IOException", "JABS_VM_IO"),
    (
        "java/lang/ClassNotFoundException",
        "JABS_VM_CLASS_NOT_FOUND",
    ),
    ("java/lang/InstantiationException", "JABS_VM_INSTANTIATION"),
    ("java/io/FileNotFoundException", "JABS_VM_FILE_NOT_FOUND"),
    ("java/io/SyncFailedException", "JABS_VM_SYNC_FAILED"),
    ("java/lang/InterruptedException", "JABS_VM_INTERRUPTED"),
];

/// Native methods the runtime implements: (class, name, descriptor) -> C body. `self` and
/// `a0`.. are the receiver and arguments. Bodies marked `needs_classes` use the Class registry.
struct Intrinsic {
    key: (&'static str, &'static str, &'static str),
    body: &'static str,
    needs_classes: bool,
}

const INTRINSICS: &[Intrinsic] = &[
    Intrinsic {
        key: ("java/lang/Object", "hashCode", "()I"),
        body: "return jabs_identity_hash(self);",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
        ),
        body: "return a0 ? jabs_identity_hash(a0) : 0;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
        body: "return jabs_class_of(self);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Object", "clone", "()Ljava/lang/Object;"),
        body: "return jabs_clone(self);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Object", "notify", "()V"),
        body: "if (!jabs_monitor_notify(self)) jabs_pending = jabs_vm_throwable(JABS_VM_ILLEGAL_MONITOR_STATE);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Object", "notifyAll", "()V"),
        body: "if (!jabs_monitor_notify_all(self)) jabs_pending = jabs_vm_throwable(JABS_VM_ILLEGAL_MONITOR_STATE);",
        needs_classes: false,
    },
    // Object.wait has already rejected a negative timeout. An interrupt needs Thread.interrupt
    // and InterruptedException wired to jabs_sync_interrupt, which the threads work owns: until
    // then nothing can interrupt a wait, and a reported interrupt stops rather than guess.
    Intrinsic {
        key: ("java/lang/Object", "wait0", "(J)V"),
        body: "switch (jabs_monitor_wait(self, a0)) { case JABS_WAIT_NOT_OWNER: jabs_pending = jabs_vm_throwable(JABS_VM_ILLEGAL_MONITOR_STATE); return; case JABS_WAIT_INTERRUPTED: jabs_thread_wait_interrupted(); return; default: return; }",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        ),
        body: "jabs_arraycopy(a0, a1, a2, a3, a4);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/System", "currentTimeMillis", "()J"),
        body: "return jabs_current_millis();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/System", "nanoTime", "()J"),
        body: "return jabs_nano_time();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Double", "doubleToRawLongBits", "(D)J"),
        body: "int64_t r; memcpy(&r, &a0, 8); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Double", "longBitsToDouble", "(J)D"),
        body: "double r; memcpy(&r, &a0, 8); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Float", "floatToRawIntBits", "(F)I"),
        body: "int32_t r; memcpy(&r, &a0, 4); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Float", "intBitsToFloat", "(I)F"),
        body: "float r; memcpy(&r, &a0, 4); return r;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Runtime", "availableProcessors", "()I"),
        body: "return jabs_thread_processor_count();",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/Throwable",
            "fillInStackTrace",
            "(I)Ljava/lang/Throwable;",
        ),
        body: "(void)a0; return jabs_fill_in_stack_trace(self);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/StackTraceElement",
            "initStackTraceElements",
            "([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V",
        ),
        body: "jabs_init_stack_trace_elements(a0, a1, a2);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "desiredAssertionStatus0",
            "(Ljava/lang/Class;)Z",
        ),
        body: "return 0; /* assertions disabled, as without -ea */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Class", "isHidden", "()Z"),
        body: "return 0;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/String", "intern", "()Ljava/lang/String;"),
        body: "return jabs_string_intern(self);",
        needs_classes: true,
    },
    // The caller of the @CallerSensitive method, recorded at its call site.
    Intrinsic {
        key: (
            "jdk/internal/reflect/Reflection",
            "getCallerClass",
            "()Ljava/lang/Class;",
        ),
        body: "return jabs_caller ? jabs_class_object(jabs_caller) : NULL;",
        needs_classes: true,
    },
    // A class's access flags as its class file writes them (HotSpot masks to the written flags).
    Intrinsic {
        key: (
            "jdk/internal/reflect/Reflection",
            "getClassAccessFlags",
            "(Ljava/lang/Class;)I",
        ),
        body: "if (!a0) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_class_access_flags(a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "jdk/internal/misc/Unsafe",
            "ensureClassInitialized0",
            "(Ljava/lang/Class;)V",
        ),
        body: "jabs_ensure_initialized(a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/reflect/Array",
            "newArray",
            "(Ljava/lang/Class;I)Ljava/lang/Object;",
        ),
        body: "return jabs_reflect_new_array(a0, a1);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/reflect/Array",
            "getLength",
            "(Ljava/lang/Object;)I",
        ),
        body: "if (!a0) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } if (a0->cls != jabs_array_class()) jabs_not_compiled(\"Array.getLength of a non-array (IllegalArgumentException)\"); return ((jabs_array *)a0)->length;",
        needs_classes: false,
    },
    // Each thread knows its own Thread (jabs_thread.c); the VM creates the main thread's before
    // any Java code runs, JABS on first request.
    Intrinsic {
        key: ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
        body: "return jabs_current_thread();",
        needs_classes: true,
    },
    // System properties the VM supplies (System.initPhase1 -> SystemProps.initProperties).
    Intrinsic {
        key: (
            "jdk/internal/util/SystemProps$Raw",
            "platformProperties",
            "()[Ljava/lang/String;",
        ),
        body: "return jabs_platform_properties();",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "jdk/internal/util/SystemProps$Raw",
            "vmProperties",
            "()[Ljava/lang/String;",
        ),
        body: "return jabs_vm_properties();",
        needs_classes: true,
    },
    // Standard streams: FileDescriptor.in/out/err.
    Intrinsic {
        key: ("java/io/FileDescriptor", "initIDs", "()V"),
        body: "/* field offsets are known at compile time */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/io/FileDescriptor", "getHandle", "(I)J"),
        body: "return jabs_platform_std_handle(a0);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/io/FileDescriptor", "getAppend", "(I)Z"),
        body: "(void)a0; return 0;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/io/FileInputStream", "initIDs", "()V"),
        body: "/* field offsets are known at compile time */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/io/FileOutputStream", "initIDs", "()V"),
        body: "/* field offsets are known at compile time */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("jdk/internal/misc/VM", "initialize", "()V"),
        body: "/* the VM's own startup state is set up by the runtime */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("jdk/internal/misc/VM", "getNanoTimeAdjustment", "(J)J"),
        body: "return jabs_nano_time_adjustment(a0);",
        needs_classes: false,
    },
    // Signals are not wired up yet: every name is unknown, which the JDK handles (Terminator skips
    // the handler). Consequence: Ctrl+C ends the process without running shutdown hooks.
    Intrinsic {
        key: (
            "jdk/internal/misc/Signal",
            "findSignal0",
            "(Ljava/lang/String;)I",
        ),
        body: "(void)a0; return -1;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("sun/io/Win32ErrorMode", "setErrorMode", "(J)J"),
        body: "return jabs_platform_set_error_mode(a0);",
        needs_classes: false,
    },
    // GraphicsEnvironment.isHeadless without java.awt.headless (the 1.7.10 server's snooper asks
    // after Done): Windows' PlatformGraphicsInfo reads it once in its static initializer.
    Intrinsic {
        key: ("sun/awt/PlatformGraphicsInfo", "hasDisplays0", "()Z"),
        body: "return jabs_platform_has_displays();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Shutdown", "beforeHalt", "()V"),
        body: "/* nothing to flush beyond the streams Java code already flushed */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Shutdown", "halt0", "(I)V"),
        body: "fflush(NULL); exit(a0);",
        needs_classes: false,
    },
    // No heap limit: the specification's answer is Long.MAX_VALUE.
    Intrinsic {
        key: ("java/lang/Runtime", "maxMemory", "()J"),
        body: "return jabs_gc_max_memory();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Runtime", "totalMemory", "()J"),
        body: "return jabs_gc_total_memory();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Runtime", "freeMemory", "()J"),
        body: "return jabs_gc_free_memory();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Runtime", "gc", "()V"),
        body: "jabs_gc_collect();",
        needs_classes: false,
    },
    // Thread.start0: jabs_thread.c runs run() on a new OS thread and ends the thread as HotSpot
    // does (reachable() keeps run, dispatchUncaughtException and exit compiled in).
    Intrinsic {
        key: ("java/lang/Thread", "start0", "()V"),
        body: "jabs_thread_start(self);",
        needs_classes: false,
    },
    // Java priorities are not applied to OS threads: HotSpot's default on Linux
    // (ThreadPriorityPolicy=0) does not either, and the Java-visible priority is a Java field.
    Intrinsic {
        key: ("java/lang/Thread", "setPriority0", "(I)V"),
        body: "(void)a0;",
        needs_classes: false,
    },
    // HotSpot names only the calling thread's OS thread (JVM_SetNativeThreadName).
    Intrinsic {
        key: ("java/lang/Thread", "setNativeName", "(Ljava/lang/String;)V"),
        body: "if (!a0 || self != jabs_current_thread()) return; char *name = jabs_string_to_utf8(a0); if (!name) return; jabs_thread_set_native_name(name); free(name);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Thread", "yield0", "()V"),
        body: "jabs_thread_yield();",
        needs_classes: false,
    },
    // The caller cleared the Java interrupt status; the monitors' interrupt mark goes too.
    Intrinsic {
        key: ("java/lang/Thread", "clearInterruptEvent", "()V"),
        body: "jabs_thread_clear_interrupt_event();",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Thread", "interrupt0", "()V"),
        body: "jabs_thread_interrupt(self);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/Thread", "sleepNanos0", "(J)V"),
        body: "jabs_thread_sleep(a0);",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/Thread",
            "ensureMaterializedForStackWalk",
            "(Ljava/lang/Object;)V",
        ),
        body: "(void)a0;",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/Thread",
            "currentCarrierThread",
            "()Ljava/lang/Thread;",
        ),
        body: "return jabs_current_thread();",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Thread", "holdsLock", "(Ljava/lang/Object;)Z"),
        body: "if (!a0) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_monitor_holds(a0);",
        needs_classes: false,
    },
    // ThreadIdentifiers adds to this counter with Unsafe; the main thread already has id 1. Every
    // thread updates it only through Unsafe.getAndAddLong, which is atomic (jabs_sync.c).
    Intrinsic {
        key: ("java/lang/Thread", "getNextThreadIdOffset", "()J"),
        body: "static int64_t next_thread_id = 2; return (int64_t)(intptr_t)&next_thread_id;",
        needs_classes: false,
    },
    // Finalization is disabled (--finalization=disabled); ReferenceHandler and Cleaner handle Reference queues.
    Intrinsic {
        key: ("java/lang/ref/Finalizer", "isFinalizationEnabled", "()Z"),
        body: "return 0;",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/lang/ref/Reference", "hasReferencePendingList", "()Z"),
        body: "return jabs_gc_has_pending_refs();",
        needs_classes: false,
    },
    // Native libraries: JABS links every native statically, so each library is "built in" (the
    // JDK's own path for statically linked libraries). Its natives are JABS implementations, or
    // stop with a diagnostic when called; no DLL or shared object is ever loaded.
    Intrinsic {
        key: (
            "java/lang/System",
            "mapLibraryName",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        body: "return jabs_map_library_name(a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "jdk/internal/loader/NativeLibraries",
            "findBuiltinLib",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        body: "return a0;",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "jdk/internal/loader/NativeLibraries",
            "load",
            "(Ljdk/internal/loader/NativeLibraries$NativeLibraryImpl;Ljava/lang/String;ZZ)Z",
        ),
        body: "(void)a0; (void)a1; (void)a3; return a2 ? 1 : 0; /* built-in libraries are always present */",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "jdk/internal/loader/NativeLibraries",
            "unload",
            "(Ljava/lang/String;ZJ)V",
        ),
        body: "(void)a0; (void)a1; (void)a2;",
        needs_classes: false,
    },
    // BootLoader tracks its unnamed module in BootLoader.UNNAMED_MODULE and named JDK modules
    // in runtime/jabs_modules.c; no separate VM-side unnamed-module registration is needed here.
    Intrinsic {
        key: (
            "jdk/internal/loader/BootLoader",
            "setBootLoaderUnnamedModule0",
            "(Ljava/lang/Module;)V",
        ),
        body: "(void)a0;",
        needs_classes: false,
    },
    // Class data sharing: a native program has no CDS archive, so nothing is shared or dumped.
    Intrinsic {
        key: ("jdk/internal/misc/CDS", "getCDSConfigStatus", "()I"),
        body: "return 0; /* no archive in use, not dumping */",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "jdk/internal/misc/CDS",
            "initializeFromArchive",
            "(Ljava/lang/Class;)V",
        ),
        body: "(void)a0; /* nothing archived: static fields keep their computed values */",
        needs_classes: false,
    },
    Intrinsic {
        key: ("jdk/internal/misc/CDS", "getRandomSeedForDumping", "()J"),
        body: "return 0;",
        needs_classes: false,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "forName0",
            "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
        ),
        body: "(void)a3; return jabs_class_for_name(a0, a1, a2);",
        needs_classes: true,
    },
    // CRC32's natives (zlib's crc32 in the JDK), jabs_crc32 in jabs_rt.h. The JDK checks the
    // bounds before calling; JABS checks again and stops rather than read outside the array.
    Intrinsic {
        key: ("java/util/zip/CRC32", "update", "(II)I"),
        body: "uint8_t b = (uint8_t)a1; return (int32_t)jabs_crc32((uint32_t)a0, &b, 1);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/util/zip/CRC32", "updateBytes0", "(I[BII)I"),
        body: "if (!a1) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } jabs_array *b = (jabs_array *)a1; if (a2 < 0 || a3 < 0 || a2 > b->length - a3) jabs_not_compiled(\"CRC32.updateBytes0 outside the array (the JDK checks the range first)\"); return (int32_t)jabs_crc32((uint32_t)a0, (const uint8_t *)jabs_array_data(b) + a2, (size_t)a3);",
        needs_classes: false,
    },
    Intrinsic {
        key: ("java/util/zip/CRC32", "updateByteBuffer0", "(IJII)I"),
        body: "if (a2 < 0 || a3 < 0) jabs_not_compiled(\"CRC32.updateByteBuffer0 with a negative offset or length\"); return (int32_t)jabs_crc32((uint32_t)a0, (const uint8_t *)(intptr_t)a1 + a2, (size_t)a3);",
        needs_classes: false,
    },
    // TimeZone asks the host for its zone (jabs_platform.c follows the JDK's own lookup).
    Intrinsic {
        key: (
            "java/util/TimeZone",
            "getSystemTimeZoneID",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        body: "char *home = NULL; if (a0) { home = jabs_string_to_utf8(a0); if (!home) return NULL; } char *id = jabs_platform_time_zone_id(home); free(home); if (!id) return NULL; jabs_object *s = jabs_string_from_utf8(id); free(id); return s;",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/util/TimeZone",
            "getSystemGMTOffsetID",
            "()Ljava/lang/String;",
        ),
        body: "char *id = jabs_platform_gmt_offset_id(); if (!id) return NULL; jabs_object *s = jabs_string_from_utf8(id); free(id); return s;",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/ClassLoader",
            "findBootstrapClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        ),
        body: "return jabs_find_bootstrap_class(a0);",
        needs_classes: true,
    },
    // java.security's provider list asks the builtin app loader for providers outside java.base.
    Intrinsic {
        key: (
            "java/lang/ClassLoader",
            "findLoadedClass0",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        ),
        body: "return jabs_find_loaded_class(self, a0);",
        needs_classes: true,
    },
    // jabs.rt.Natives: the app class loader's view of compiled-in classes and embedded resources.
    Intrinsic {
        key: (
            "jabs/rt/Natives",
            "classNamed",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        ),
        body: "return jabs_class_for_binary_name(a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("jabs/rt/Natives", "jabPath", "()Ljava/lang/String;"),
        body: "const char *path = jabs_jab_path(); return path ? jabs_string_from_utf8(path) : NULL;",
        needs_classes: true,
    },
    Intrinsic {
        key: ("jabs/rt/Natives", "resourceCount", "(Ljava/lang/String;)I"),
        body: "return jabs_resource_count(a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("jabs/rt/Natives", "resource", "(Ljava/lang/String;I)[B"),
        body: "return jabs_resource_bytes(a0, a1);",
        needs_classes: true,
    },
    // The registry sets `name` when it creates a Class, so the lazy initializer just returns it.
    Intrinsic {
        key: ("java/lang/Class", "initClassName", "()Ljava/lang/String;"),
        body: "return ((struct jo_java_2flang_2fClass *)self)->f_name;",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Class", "isInstance", "(Ljava/lang/Object;)Z"),
        body: "return a0 ? jabs_class_is_instance(self, a0) : 0;",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "isAssignableFrom",
            "(Ljava/lang/Class;)Z",
        ),
        body: "if (!a0) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_class_assignable_from(self, a0);",
        needs_classes: true,
    },
    Intrinsic {
        key: ("java/lang/Class", "getSuperclass", "()Ljava/lang/Class;"),
        body: "return jabs_class_superclass(self);",
        needs_classes: true,
    },
    Intrinsic {
        key: (
            "java/lang/Class",
            "getPrimitiveClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
        ),
        body: "char name[32]; if (!a0 || !jabs_string_to_ascii(a0, name, sizeof name)) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; } return jabs_class_object(name);",
        needs_classes: true,
    },
];

/// Java (not native) methods JABS replaces, with the reason. Each is behavior-preserving for a
/// program JABS can run today: (class, name, descriptor) -> C body.
const REPLACEMENTS: &[(&str, &str, &str, &str)] = &[
    // The VM sets Class.module. Input classes belong to the app loader's unnamed module; JDK
    // classes belong to their named module (runtime/jabs_modules.c) when module descriptors are
    // present, or the boot loader's unnamed module otherwise.
    (
        "java/lang/Class",
        "getModule",
        "()Ljava/lang/Module;",
        "return jabs_class_module(self);",
    ),
    // VarHandles are made by the runtime: a field offset (or static address, or array layout)
    // plus the value type; accesses compile to memory operations at each call site. Not yet
    // checked: the lookup's access rights. Static handles initialize the class when created.
    (
        "java/lang/invoke/MethodHandles$Lookup",
        "findVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        "(void)self; return jabs_var_handle_field(a0, a1, a2, 0);",
    ),
    (
        "java/lang/invoke/MethodHandles$Lookup",
        "findStaticVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        "(void)self; return jabs_var_handle_field(a0, a1, a2, 1);",
    ),
    (
        "java/lang/invoke/MethodHandles",
        "arrayElementVarHandle",
        "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        "return jabs_var_handle_array(a0);",
    ),
    // byte[] seen as an array of a wider primitive, in either byte order (jdk.internal.util's
    // ByteArray and ByteArrayLittleEndian read DataInputStream's numbers through these).
    (
        "java/lang/invoke/VarHandles",
        "byteArrayViewHandle",
        "(Ljava/lang/Class;Z)Ljava/lang/invoke/VarHandle;",
        "return jabs_var_handle_byte_view(a0, a1);",
    ),
    // The JDK's version goes through Constructor objects and method handles; the effect is the
    // same: run the public no-arg constructor (its exceptions propagate unwrapped).
    // Not yet checked: the caller's access to the class and constructor.
    (
        "java/lang/Class",
        "newInstance",
        "()Ljava/lang/Object;",
        "return jabs_class_new_instance(self);",
    ),
    // Finalization is disabled (isFinalizationEnabled=0), so Finalizer thread is not started.
    (
        "java/lang/ref/Finalizer",
        "startFinalizerThread",
        "(Ljava/lang/ThreadGroup;)V",
        "(void)a0; /* JABS: finalization disabled */",
    ),
];

/// The C body replacing a Java method, if JABS replaces it.
fn replacement(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if let Some(found) = reflect::replacement(key) {
        return Some(found);
    }
    if let Some(found) = management::replacement(key) {
        return Some(found);
    }
    if let Some(found) = method_handles::replacement(key) {
        return Some(found);
    }
    if let Some(found) = modules::replacement(key, classes_available) {
        return Some(found);
    }
    // Without initPhase2, ModuleLayer.boot() is null, so HashedModules.<clinit> would throw NPE on
    // stack trace formatting (StackTraceElement.toString).
    if key.0 == "java/lang/StackTraceElement$HashedModules"
        && key.1 == "hashedModules"
        && key.2 == "()Ljava/util/Set;"
    {
        return Some((
            format!(
                "if ({init}() != 0) return NULL;\n    return {empty}();",
                init = init_symbol("java/util/Collections"),
                empty = symbol("java/util/Collections", "emptySet", "()Ljava/util/Set;")
            ),
            false,
        ));
    }
    // JDK 25 walks the stack with StackWalker (a lambda); JABS walks native frames instead.
    if key.0 == "java/lang/SecurityManager"
        && key.1 == "getClassContext"
        && key.2 == "()[Ljava/lang/Class;"
    {
        return Some((
            format!(
                "(void)self; return jabs_class_context((const void *)&{});",
                symbol(&key.0, &key.1, &key.2)
            ),
            true,
        ));
    }
    REPLACEMENTS
        .iter()
        .find(|r| r.0 == key.0 && r.1 == key.1 && r.2 == key.2)
        .map(|r| {
            (
                r.3.to_string(),
                r.0 == CLASS || r.3.contains("jabs_var_handle"),
            )
        })
}

/// java.io natives: both file systems (Windows and Unix JDKs) and the file streams, all calling
/// the OS-neutral glue in runtime/jabs_io.c. (class, name, descriptor, body).
const IO_NATIVES: &[(&str, &str, &str, &str)] = &[
    (
        "java/io/WinNTFileSystem",
        "initIDs",
        "()V",
        "/* field offsets are known at compile time */",
    ),
    (
        "java/io/WinNTFileSystem",
        "getBooleanAttributes0",
        "(Ljava/io/File;)I",
        "return jabs_io_attributes(a0, 1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "checkAccess0",
        "(Ljava/io/File;I)Z",
        "return jabs_io_check_access(a0, a1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "getLastModifiedTime0",
        "(Ljava/io/File;)J",
        "return jabs_io_last_modified(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "getLength0",
        "(Ljava/io/File;)J",
        "return jabs_io_length(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "setPermission0",
        "(Ljava/io/File;IZZ)Z",
        "return jabs_io_set_permission(a0, a1, a2, a3);",
    ),
    (
        "java/io/WinNTFileSystem",
        "createFileExclusively0",
        "(Ljava/lang/String;)Z",
        "return jabs_io_create_exclusively(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "list0",
        "(Ljava/io/File;)[Ljava/lang/String;",
        "return jabs_io_list(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "createDirectory0",
        "(Ljava/io/File;)Z",
        "return jabs_io_create_directory(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "setLastModifiedTime0",
        "(Ljava/io/File;J)Z",
        "return jabs_io_set_last_modified(a0, a1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "setReadOnly0",
        "(Ljava/io/File;)Z",
        "return jabs_io_set_read_only(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "delete0",
        "(Ljava/io/File;Z)Z",
        "return jabs_io_delete(a0, a1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "delete0",
        "(Ljava/io/File;)Z",
        "return jabs_io_delete(a0, 1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "rename0",
        "(Ljava/io/File;Ljava/io/File;)Z",
        "return jabs_io_rename(a0, a1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "listRoots0",
        "()I",
        "return jabs_fs_roots();",
    ),
    (
        "java/io/WinNTFileSystem",
        "getSpace0",
        "(Ljava/io/File;I)J",
        "return jabs_io_space(a0, a1);",
    ),
    (
        "java/io/WinNTFileSystem",
        "getNameMax0",
        "(Ljava/lang/String;)I",
        "(void)a0; return 255;",
    ),
    (
        "java/io/WinNTFileSystem",
        "canonicalize0",
        "(Ljava/lang/String;)Ljava/lang/String;",
        "return jabs_io_canonicalize(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "getFinalPath0",
        "(Ljava/lang/String;)Ljava/lang/String;",
        "return jabs_io_final_path(a0);",
    ),
    (
        "java/io/WinNTFileSystem",
        "getDriveDirectory",
        "(I)Ljava/lang/String;",
        "return jabs_io_drive_directory(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "initIDs",
        "()V",
        "/* field offsets are known at compile time */",
    ),
    (
        "java/io/UnixFileSystem",
        "getBooleanAttributes0",
        "(Ljava/io/File;)I",
        "return jabs_io_attributes(a0, 0);",
    ),
    (
        "java/io/UnixFileSystem",
        "checkAccess0",
        "(Ljava/io/File;I)Z",
        "return jabs_io_check_access(a0, a1);",
    ),
    (
        "java/io/UnixFileSystem",
        "getLastModifiedTime0",
        "(Ljava/io/File;)J",
        "return jabs_io_last_modified(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "getLength0",
        "(Ljava/io/File;)J",
        "return jabs_io_length(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "setPermission0",
        "(Ljava/io/File;IZZ)Z",
        "return jabs_io_set_permission(a0, a1, a2, a3);",
    ),
    (
        "java/io/UnixFileSystem",
        "createFileExclusively0",
        "(Ljava/lang/String;)Z",
        "return jabs_io_create_exclusively(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "list0",
        "(Ljava/io/File;)[Ljava/lang/String;",
        "return jabs_io_list(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "createDirectory0",
        "(Ljava/io/File;)Z",
        "return jabs_io_create_directory(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "setLastModifiedTime0",
        "(Ljava/io/File;J)Z",
        "return jabs_io_set_last_modified(a0, a1);",
    ),
    (
        "java/io/UnixFileSystem",
        "setReadOnly0",
        "(Ljava/io/File;)Z",
        "return jabs_io_set_read_only(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "delete0",
        "(Ljava/io/File;Z)Z",
        "return jabs_io_delete(a0, a1);",
    ),
    (
        "java/io/UnixFileSystem",
        "delete0",
        "(Ljava/io/File;)Z",
        "return jabs_io_delete(a0, 1);",
    ),
    (
        "java/io/UnixFileSystem",
        "rename0",
        "(Ljava/io/File;Ljava/io/File;)Z",
        "return jabs_io_rename(a0, a1);",
    ),
    (
        "java/io/UnixFileSystem",
        "listRoots0",
        "()I",
        "return jabs_fs_roots();",
    ),
    (
        "java/io/UnixFileSystem",
        "getSpace0",
        "(Ljava/io/File;I)J",
        "return jabs_io_space(a0, a1);",
    ),
    (
        "java/io/UnixFileSystem",
        "getNameMax0",
        "(Ljava/lang/String;)I",
        "(void)a0; return 255;",
    ),
    (
        "java/io/UnixFileSystem",
        "canonicalize0",
        "(Ljava/lang/String;)Ljava/lang/String;",
        "return jabs_io_canonicalize(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "getFinalPath0",
        "(Ljava/lang/String;)Ljava/lang/String;",
        "return jabs_io_final_path(a0);",
    ),
    (
        "java/io/UnixFileSystem",
        "getDriveDirectory",
        "(I)Ljava/lang/String;",
        "return jabs_io_drive_directory(a0);",
    ),
    (
        "java/io/FileInputStream",
        "open0",
        "(Ljava/lang/String;)V",
        "jabs_io_open(self, a0, JABS_OPEN_READ);",
    ),
    (
        "java/io/FileInputStream",
        "read0",
        "()I",
        "return jabs_io_read_byte(self);",
    ),
    (
        "java/io/FileInputStream",
        "readBytes",
        "([BII)I",
        "return jabs_io_read_bytes(self, a0, a1, a2);",
    ),
    (
        "java/io/FileInputStream",
        "length0",
        "()J",
        "return jabs_io_stream_length(self);",
    ),
    (
        "java/io/FileInputStream",
        "position0",
        "()J",
        "return jabs_io_position(self);",
    ),
    (
        "java/io/FileInputStream",
        "skip0",
        "(J)J",
        "return jabs_io_skip(self, a0);",
    ),
    (
        "java/io/FileInputStream",
        "available0",
        "()I",
        "return jabs_io_available(self);",
    ),
    (
        "java/io/FileInputStream",
        "isRegularFile0",
        "(Ljava/io/FileDescriptor;)Z",
        "return jabs_io_is_regular(a0);",
    ),
    (
        "java/io/FileOutputStream",
        "open0",
        "(Ljava/lang/String;Z)V",
        "jabs_io_open(self, a0, JABS_OPEN_WRITE | JABS_OPEN_CREATE | (a1 ? JABS_OPEN_APPEND : JABS_OPEN_TRUNCATE));",
    ),
    (
        "java/io/FileOutputStream",
        "write",
        "(IZ)V",
        "(void)a1; jabs_io_write_byte(self, a0);",
    ),
    (
        "java/io/FileOutputStream",
        "writeBytes",
        "([BIIZ)V",
        "(void)a3; jabs_io_write_bytes(self, a0, a1, a2);",
    ),
    (
        "java/io/RandomAccessFile",
        "initIDs",
        "()V",
        "/* field offsets are known at compile time */",
    ),
    (
        "java/io/RandomAccessFile",
        "open0",
        "(Ljava/lang/String;I)V",
        "jabs_io_open(self, a0, jabs_io_raf_flags(a1));",
    ),
    (
        "java/io/RandomAccessFile",
        "read0",
        "()I",
        "return jabs_io_read_byte(self);",
    ),
    (
        "java/io/RandomAccessFile",
        "readBytes0",
        "([BII)I",
        "return jabs_io_read_bytes(self, a0, a1, a2);",
    ),
    (
        "java/io/RandomAccessFile",
        "write0",
        "(I)V",
        "jabs_io_write_byte(self, a0);",
    ),
    (
        "java/io/RandomAccessFile",
        "writeBytes0",
        "([BII)V",
        "jabs_io_write_bytes(self, a0, a1, a2);",
    ),
    (
        "java/io/RandomAccessFile",
        "getFilePointer",
        "()J",
        "return jabs_io_position(self);",
    ),
    (
        "java/io/RandomAccessFile",
        "seek0",
        "(J)V",
        "jabs_io_seek(self, a0);",
    ),
    (
        "java/io/RandomAccessFile",
        "length0",
        "()J",
        "return jabs_io_stream_length(self);",
    ),
    (
        "java/io/RandomAccessFile",
        "setLength0",
        "(J)V",
        "jabs_io_set_length(self, a0);",
    ),
    (
        "java/io/FileDescriptor",
        "close0",
        "()V",
        "jabs_io_close_descriptor(self);",
    ),
    (
        "java/io/FileDescriptor",
        "sync0",
        "()V",
        "jabs_io_sync(self);",
    ),
    (
        "java/io/FileCleanable",
        "cleanupClose0",
        "(IJ)V",
        "jabs_io_cleanup_close(a0, a1);",
    ),
];

/// The runtime's C body for a native method, if it implements one (and the unit can support it).
fn intrinsic(key: &MethodKey, classes_available: bool) -> Option<(String, bool)> {
    if key.1 == "registerNatives" && key.2 == "()V" {
        return Some(("/* natives are linked statically */".to_string(), false));
    }
    if key.0 == SYSTEM {
        let field = match (key.1.as_str(), key.2.as_str()) {
            ("setIn0", "(Ljava/io/InputStream;)V") => Some("in"),
            ("setOut0", "(Ljava/io/PrintStream;)V") => Some("out"),
            ("setErr0", "(Ljava/io/PrintStream;)V") => Some("err"),
            _ => None,
        };
        if let Some(field) = field {
            return Some((format!("{} = a0;", field_symbol(SYSTEM, field)), false));
        }
    }
    if matches!(
        key.0.as_str(),
        "java/lang/ref/Reference" | "java/lang/ref/PhantomReference"
    ) {
        let referent = format!(
            "(({} *)self)->{}",
            object_struct("java/lang/ref/Reference"),
            instance_field("referent")
        );
        let body = match (key.1.as_str(), key.2.as_str()) {
            ("refersTo0", "(Ljava/lang/Object;)Z") => format!("return {referent} == a0;"),
            ("clear0", "()V") => format!("{referent} = NULL;"),
            // JDK 25.0.5 reads the referent through a native (25.0.4 read the field): with no
            // GC nothing is ever cleared behind the program's back, so it is the field.
            ("get0", "()Ljava/lang/Object;") => format!("return {referent};"),
            _ => String::new(),
        };
        if !body.is_empty() {
            return Some((body, false));
        }
    }
    if key.0 == "java/lang/SecurityManager"
        && key.1 == "getClassContext"
        && key.2 == "()[Ljava/lang/Class;"
    {
        return classes_available.then(|| {
            (
                format!(
                    "(void)self; return jabs_class_context((const void *)&{});",
                    symbol(&key.0, &key.1, &key.2)
                ),
                true,
            )
        });
    }
    if let Some(row) = IO_NATIVES
        .iter()
        .find(|r| r.0 == key.0 && r.1 == key.1 && r.2 == key.2)
    {
        // The glue in jabs_io.c uses the registry's String and array helpers.
        return classes_available.then(|| (row.3.to_string(), true));
    }
    if key.0 == UNSAFE
        && let Some((body, needs_classes)) = unsafe_intrinsic(&key.1, &key.2)
    {
        return (classes_available || !needs_classes).then_some((body, needs_classes));
    }
    if let Some(found) = reflect::intrinsic(key, classes_available) {
        return Some(found);
    }
    INTRINSICS
        .iter()
        .find(|i| i.key.0 == key.0 && i.key.1 == key.1 && i.key.2 == key.2)
        .filter(|i| classes_available || !i.needs_classes)
        .map(|i| (i.body.to_string(), i.needs_classes))
}

const UNSAFE: &str = "jdk/internal/misc/Unsafe";

/// Bodies for jdk.internal.misc.Unsafe natives. Offsets are byte offsets from the object pointer
/// (our struct layouts and array data start), or raw addresses when the object is null, as in
/// HotSpot. Volatile accesses and compare-and-set/exchange are sequentially consistent atomics
/// (Java volatile), on naturally aligned addresses only: an atomic access to a misaligned one
/// stops rather than tear or trap. Plain get/put stay ordinary loads and stores.
fn unsafe_intrinsic(name: &str, descriptor: &str) -> Option<(String, bool)> {
    const TYPES: [(&str, &str, &str); 9] = [
        ("Int", "I", "int32_t"),
        ("Long", "J", "int64_t"),
        ("Reference", "Ljava/lang/Object;", "jabs_object *"),
        ("Boolean", "Z", "uint8_t"),
        ("Byte", "B", "int8_t"),
        ("Short", "S", "int16_t"),
        ("Char", "C", "uint16_t"),
        ("Float", "F", "float"),
        ("Double", "D", "double"),
    ];
    let addr = "((uintptr_t)a0 + (uintptr_t)a1)";
    // The address of an atomic access, checked for natural alignment.
    let atomic_at = |ty: &str| {
        format!(
            "{ty} *p = ({ty} *){addr}; if ((uintptr_t)p % sizeof *p != 0) jabs_not_compiled(\"jdk/internal/misc/Unsafe.{name} at a misaligned address\");"
        )
    };
    for (word, code, ty) in TYPES {
        let value = if code == "Z" {
            "(uint8_t)(a2 & 1)".to_string()
        } else {
            format!("({ty})a2")
        };
        if name == format!("get{word}") && descriptor == format!("(Ljava/lang/Object;J){code}") {
            return Some((
                format!("{{ {ty} v; memcpy(&v, (const void *){addr}, sizeof v); return v; }}"),
                false,
            ));
        }
        if name == format!("put{word}") && descriptor == format!("(Ljava/lang/Object;J{code})V") {
            return Some((
                format!("{{ {ty} v = {value}; memcpy((void *){addr}, &v, sizeof v); }}"),
                false,
            ));
        }
        if name == format!("get{word}Volatile")
            && descriptor == format!("(Ljava/lang/Object;J){code}")
        {
            return Some((
                format!(
                    "{} {ty} v; __atomic_load(p, &v, __ATOMIC_SEQ_CST); return v;",
                    atomic_at(ty)
                ),
                false,
            ));
        }
        if name == format!("put{word}Volatile")
            && descriptor == format!("(Ljava/lang/Object;J{code})V")
        {
            return Some((
                format!(
                    "{} {ty} v = {value}; __atomic_store(p, &v, __ATOMIC_SEQ_CST);",
                    atomic_at(ty)
                ),
                false,
            ));
        }
        if matches!(word, "Int" | "Long" | "Reference") {
            if name == format!("compareAndSet{word}")
                && descriptor == format!("(Ljava/lang/Object;J{code}{code})Z")
            {
                return Some((
                    format!(
                        "{} {ty} e = a2; {ty} n = a3; return __atomic_compare_exchange(p, &e, &n, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);",
                        atomic_at(ty)
                    ),
                    false,
                ));
            }
            if name == format!("compareAndExchange{word}")
                && descriptor == format!("(Ljava/lang/Object;J{code}{code}){code}")
            {
                return Some((
                    format!(
                        "{} {ty} e = a2; {ty} n = a3; (void)__atomic_compare_exchange(p, &e, &n, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST); return e;",
                        atomic_at(ty)
                    ),
                    false,
                ));
            }
        }
    }
    let body = match (name, descriptor) {
        ("fullFence", "()V") => "__atomic_thread_fence(__ATOMIC_SEQ_CST);",
        ("loadFence", "()V") => "__atomic_thread_fence(__ATOMIC_ACQUIRE);",
        ("storeFence", "()V") => "__atomic_thread_fence(__ATOMIC_RELEASE);",
        ("throwException", "(Ljava/lang/Throwable;)V") => {
            "jabs_pending = a0 ? a0 : jabs_vm_throwable(JABS_VM_NULL_POINTER);"
        }
        ("allocateMemory0", "(J)J") => "return (int64_t)(intptr_t)malloc((size_t)a0);",
        ("reallocateMemory0", "(JJ)J") => {
            "return (int64_t)(intptr_t)realloc((void *)(intptr_t)a0, (size_t)a1);"
        }
        ("freeMemory0", "(J)V") => "free((void *)(intptr_t)a0);",
        ("setMemory0", "(Ljava/lang/Object;JJB)V") => {
            "if (a2 > 0) memset((void *)((uintptr_t)a0 + (uintptr_t)a1), (unsigned char)a3, (size_t)a2);"
        }
        ("copyMemory0", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V") => {
            "if (a4 > 0) memmove((void *)((uintptr_t)a2 + (uintptr_t)a3), (const void *)((uintptr_t)a0 + (uintptr_t)a1), (size_t)a4);"
        }
        ("arrayBaseOffset0", "(Ljava/lang/Class;)I") => {
            "return (int32_t)((sizeof(jabs_array) + 7) & ~(size_t)7);"
        }
        ("arrayIndexScale0", "(Ljava/lang/Class;)I") => {
            return Some(("return jabs_array_scale(a0);".into(), true));
        }
        ("objectFieldOffset1", "(Ljava/lang/Class;Ljava/lang/String;)J") => {
            return Some(("return jabs_field_offset(a0, a1);".into(), true));
        }
        // java.util.Random's <clinit> asks for its seed field's offset this way.
        ("objectFieldOffset0", "(Ljava/lang/reflect/Field;)J") => {
            return Some(("return jabs_unsafe_field_offset(a0);".into(), true));
        }
        _ => return None,
    };
    Some((body.to_string(), false))
}

/// Whether the unit has what Class objects need: java.lang.Class and String with the expected fields.
fn classes_available(unit: &Unit) -> bool {
    let has = |class: &str, field: &str, descriptor: &str| {
        unit.get(class).is_some_and(|c| {
            c.fields.iter().any(|f| {
                f.name == field && f.descriptor == descriptor && f.access & ACC_STATIC == 0
            })
        })
    };
    unit.chain(CLASS).is_ok()
        && unit.chain(STRING).is_ok()
        && has(CLASS, "name", "Ljava/lang/String;")
        && has(CLASS, "componentType", "Ljava/lang/Class;")
        && has(CLASS, "modifiers", "C")
        && has(CLASS, "primitive", "Z")
        && STRING_FIELDS.iter().all(|(n, d)| has(STRING, n, d))
}

/// `jabs_pending = <new VM exception>` for one of `VM_EXCEPTIONS`.
fn raise(class: &str) -> String {
    let kind = VM_EXCEPTIONS
        .iter()
        .find(|(c, _)| *c == class)
        .unwrap_or_else(|| panic!("{class} is not a VM exception"))
        .1;
    format!("jabs_pending = jabs_vm_throwable({kind});")
}

/// Fields the JDK 9+ String layout must have for compiled literals: value, coder, hash, hashIsZero.
const STRING_FIELDS: [(&str, &str); 4] = [
    ("value", "[B"),
    ("coder", "B"),
    ("hash", "I"),
    ("hashIsZero", "Z"),
];

#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MethodStatus {
    /// The body is native. `self_contained`: nothing reachable from it hits a not-compiled stub.
    /// `blocked_by`: the blocker categories (see [`blocker_category`]) whose not-compiled code it
    /// can reach; empty exactly when it is self-contained.
    Compiled {
        symbol: String,
        self_contained: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        blocked_by: Vec<String>,
    },
    Unsupported {
        reason: String,
    },
    /// Abstract: no code of its own; calls dispatch to implementations.
    Abstract,
}

#[derive(Debug, Serialize)]
pub struct MethodReport {
    pub class: String,
    pub name: String,
    pub descriptor: String,
    #[serde(flatten)]
    pub status: MethodStatus,
}

/// How the methods of a JVM run's trace (`UnitExtras::executed_methods`) were used. Every
/// traced method lands in exactly one count.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct ExecutedRoots {
    /// Compiled methods of the unit: reachability roots.
    pub roots: usize,
    /// Methods of classes the unit does not contain (other modules or jars, hidden classes).
    pub class_not_in_unit: usize,
    /// Methods the unit's class lacks: another build of the class, or a method the JDK's runtime
    /// image generates (`java/lang/invoke/*$Holder`).
    pub method_not_in_class: usize,
    /// Abstract in the unit, which a run cannot execute: the trace and the unit disagree.
    pub abstract_in_unit: usize,
    /// Methods JABS cannot compile; calling one still stops with the not-compiled diagnostic.
    pub not_compiled: usize,
}

#[derive(Debug, Serialize)]
pub struct CompiledUnit {
    pub schema_version: u32,
    pub classes: usize,
    /// invokedynamic call sites translated at compile time, and those left untranslated.
    pub lambda_sites: usize,
    pub string_concat_sites: usize,
    /// Record methods and pattern switches translated.
    pub other_indy_sites: usize,
    pub invokedynamic_unsupported: usize,
    /// Why call sites were left as invokedynamic, with counts.
    pub invokedynamic_reasons: BTreeMap<String, usize>,
    pub compiled: usize,
    pub unsupported: usize,
    /// Abstract methods (counted in neither `compiled` nor `unsupported`).
    pub abstract_methods: usize,
    /// Compiled methods whose whole reachable call graph and class initialization is native.
    pub self_contained: usize,
    pub methods: Vec<MethodReport>,
    /// What `UnitExtras::executed_methods` made roots and what it set aside, when one is given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_roots: Option<ExecutedRoots>,
    #[serde(skip)]
    pub c_source: String,
    /// The same program split for parallel compilation: (file name, contents). The first is the
    /// shared header every other file includes.
    #[serde(skip)]
    pub c_files: Vec<(String, String)>,
}

/// Definitions of the runtime's globals; exactly one file of a program contains them.
const RUNTIME_GLOBALS: &str = r#"_Thread_local jabs_object *jabs_pending;
_Thread_local uint32_t jabs_hash_next = 0x9e3779b9u;
_Thread_local const char *jabs_where = "";
_Atomic int64_t jabs_heap_bytes;
_Thread_local const char *jabs_caller;
void (*jabs_on_not_compiled)(void);
int jabs_define_count;
const char *const *jabs_defines;
int jabs_set_command_line_properties(int count, const char *const *defines) {
    if (count < 0 || (count > 0 && !defines)) return -1;
    for (int i = 0; i < count; i++) if (!defines[i] || defines[i][0] == '=' || defines[i][0] == 0) return -1;
    jabs_define_count = count;
    jabs_defines = defines;
    return 0;
}
jabs_array_type jabs_array_chunk0[64] = {
    { "[Z", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 1, 0, 0 },
    { "[B", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 1, 0, 0 },
    { "[C", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 2, 1, 0 },
    { "[S", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 2, 1, 0 },
    { "[I", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 4, 2, 0 },
    { "[J", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 8, 3, 0 },
    { "[F", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 4, 2, 0 },
    { "[D", NULL, NULL, JABS_COMPONENT_PRIMITIVE, 8, 3, 0 },
    { "[Ljava/lang/Object;", NULL, NULL, JABS_COMPONENT_ANY, (uint8_t)sizeof(jabs_object *), (uint8_t)(sizeof(jabs_object *) == 8 ? 3 : 2), 1 },
};
jabs_array_type *jabs_array_chunks[1024] = { jabs_array_chunk0 };
static atomic_flag jabs_array_type_lock = ATOMIC_FLAG_INIT;
static uint32_t jabs_array_type_count = 9u;
typedef struct { const char *desc, *comp_desc; const jabs_class *comp; int32_t kind; uint32_t tag; } jabs_array_tls_entry;
static _Thread_local jabs_array_tls_entry jabs_array_tls_cache[8];
static _Thread_local uint32_t jabs_array_tls_next;
uint32_t jabs_array_intern_tag(int32_t kind, const jabs_class *component, const char *descriptor, const char *component_descriptor) {
    for (int i = 0; i < 8; i++) {
        jabs_array_tls_entry *e = &jabs_array_tls_cache[i];
        if (e->desc == descriptor && e->comp == component && e->kind == kind && e->comp_desc == component_descriptor) return e->tag;
    }
    while (atomic_flag_test_and_set_explicit(&jabs_array_type_lock, memory_order_acquire)) {}
    uint32_t found = UINT32_MAX;
    for (uint32_t i = 0; i < jabs_array_type_count; i++) {
        const jabs_array_type *t = &jabs_array_chunks[i >> 6][i & 63u];
        if (t->component_kind == kind && t->component == component && strcmp(t->descriptor, descriptor) == 0
            && ((t->component_descriptor == component_descriptor) || (t->component_descriptor && component_descriptor && strcmp(t->component_descriptor, component_descriptor) == 0))) {
            found = i;
            break;
        }
    }
    if (found == UINT32_MAX) {
        if (jabs_array_type_count >= 65536u) { atomic_flag_clear_explicit(&jabs_array_type_lock, memory_order_release); jabs_not_compiled("too many distinct array types"); }
        uint32_t chunk_idx = jabs_array_type_count >> 6;
        if (!jabs_array_chunks[chunk_idx]) {
            jabs_array_type *chunk = (jabs_array_type *)calloc(64, sizeof(jabs_array_type));
            if (!chunk) { atomic_flag_clear_explicit(&jabs_array_type_lock, memory_order_release); jabs_not_compiled("out of memory registering array type"); }
            __atomic_store_n(&jabs_array_chunks[chunk_idx], chunk, __ATOMIC_RELEASE);
        }
        uint8_t is_ref = (kind != JABS_COMPONENT_PRIMITIVE) ? 1u : 0u;
        uint8_t shift = (sizeof(jabs_object *) == 8u) ? 3u : 2u;
        uint8_t esz = (uint8_t)sizeof(jabs_object *);
        if (!is_ref) {
            switch (descriptor[1]) {
            case 'Z': case 'B': esz = 1; shift = 0; break;
            case 'C': case 'S': esz = 2; shift = 1; break;
            case 'I': case 'F': esz = 4; shift = 2; break;
            default: esz = 8; shift = 3; break;
            }
        }
        found = jabs_array_type_count++;
        jabs_array_chunks[found >> 6][found & 63u] = (jabs_array_type){ descriptor, component_descriptor, component, kind, esz, shift, is_ref };
    }
    const jabs_array_type *res = &jabs_array_chunks[found >> 6][found & 63u];
    uint32_t tag = (res->is_ref ? JABS_ARRAY_TAG_REF : 0u) | ((uint32_t)res->elem_shift << JABS_ARRAY_TAG_SHIFT_POS) | found;
    atomic_flag_clear_explicit(&jabs_array_type_lock, memory_order_release);
    uint32_t slot = jabs_array_tls_next++ & 7u;
    jabs_array_tls_cache[slot] = (jabs_array_tls_entry){ descriptor, component_descriptor, component, kind, tag };
    return tag;
}

"#;

/// Extern declarations for the external definitions in `code` (records, vtables, static fields
/// and non-static functions), so other files can reference them.
fn declarations_for(code: &str) -> String {
    let mut out = String::new();
    for line in code.lines() {
        if line.starts_with("static")
            || line.starts_with(' ')
            || line.starts_with('#')
            || line.starts_with("typedef")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("const jabs_class ") {
            if let Some(name) = rest.split(' ').next() {
                let _ = writeln!(out, "extern const jabs_class {name};");
            }
        } else if let Some(rest) = line.strip_prefix("void (*const ") {
            if let Some(name) = rest.split('[').next() {
                let _ = writeln!(out, "extern void (*const {name}[])(void);");
            }
        } else if line.ends_with(';') && line.contains(" jf_") && !line.contains('(') {
            let _ = writeln!(out, "extern {line}");
        } else if let (Some(open), Some(brace)) = (line.find('('), line.find(") {")) {
            // A function definition on one line ("T name(params) { ... }") or opening a body.
            if open < brace && !line[..open].contains('=') {
                let _ = writeln!(out, "{};", &line[..brace + 1]);
            }
        }
    }
    out
}

/// Split method definitions into IR/CFG cost-balanced groups for parallel compilation.
fn method_chunks(methods: &[(usize, String)], files: usize) -> Vec<String> {
    if methods.is_empty() {
        return Vec::new();
    }
    let total_cost: usize = methods.iter().map(|(c, _)| *c).sum();
    let num_chunks = (total_cost / 128).clamp(1, files.max(1)).min(methods.len());
    if num_chunks == 1 {
        let cap: usize = methods.iter().map(|(_, s)| s.len()).sum();
        let mut out = String::with_capacity(cap);
        for (_, s) in methods {
            out.push_str(s);
        }
        return vec![out];
    }
    // Deterministic LPT assignment: largest methods first, then emit each bucket in original order.
    let mut order: Vec<usize> = (0..methods.len()).collect();
    order.sort_by_key(|&i| (Reverse(methods[i].0), i));
    let mut loads = vec![0usize; num_chunks];
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); num_chunks];
    for idx in order {
        let min_b = (0..num_chunks).min_by_key(|&b| loads[b]).unwrap();
        loads[min_b] += methods[idx].0;
        buckets[min_b].push(idx);
    }
    buckets
        .into_iter()
        .filter(|b| !b.is_empty())
        .map(|mut b| {
            b.sort_unstable();
            let cap: usize = b.iter().map(|&i| methods[i].1.len()).sum();
            let mut out = String::with_capacity(cap);
            for i in b {
                out.push_str(&methods[i].1);
            }
            out
        })
        .collect()
}

/// One type from a descriptor: `code` is I J F D Z B C S, `L` (with `class`) or `V` for void.
#[derive(Debug, Clone, PartialEq)]
pub struct Ty {
    pub code: char,
    pub class: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

/// Stack kind for a type code; boolean/byte/char/short all live as int.
pub fn kind_of(c: char) -> Option<Kind> {
    match c {
        'I' | 'Z' | 'B' | 'C' | 'S' => Some(Kind::I),
        'J' => Some(Kind::J),
        'F' => Some(Kind::F),
        'D' => Some(Kind::D),
        'L' => Some(Kind::A),
        _ => None,
    }
}

/// Parse one field or parameter descriptor (arrays are not compiled yet).
fn parse_type(descriptor: &str, context: &str) -> Result<Ty> {
    match descriptor.as_bytes() {
        [c] if b"IJFDZBCS".contains(c) => Ok(Ty {
            code: *c as char,
            class: None,
        }),
        [b'L', .., b';'] => Ok(Ty {
            code: 'L',
            class: Some(descriptor[1..descriptor.len() - 1].to_string()),
        }),
        [b'[', ..] => {
            parse_type(&descriptor[1..], context)?;
            Ok(Ty {
                code: 'L',
                class: Some(descriptor.to_string()),
            })
        }
        _ => bail!("invalid descriptor {descriptor} in {context}"),
    }
}

/// Escape a class or descriptor name for a C string literal (names may hold any Unicode).
fn c_escape(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out
}

/// C element type of an array, from its descriptor char (`L` or `[` for references).
fn element_type(element: char) -> &'static str {
    match element {
        'Z' => "uint8_t",
        'B' => "int8_t",
        'C' => "uint16_t",
        'S' => "int16_t",
        'I' => "int32_t",
        'J' => "int64_t",
        'F' => "float",
        'D' => "double",
        _ => "jabs_object *",
    }
}

/// Type name of an array element as the verifier interns it: a class name, an array
/// descriptor, or None for primitives.
fn element_class(array: &str) -> Option<String> {
    let element = &array[1..];
    match element.as_bytes().first() {
        Some(b'L') => Some(element[1..element.len() - 1].to_string()),
        Some(b'[') => Some(element.to_string()),
        _ => None,
    }
}

/// The class at the bottom of an array type, however many levels it has: `[[[Lp/C;` gives
/// `p/C`; arrays of primitives give None. multianewarray's record and its RTA entry both use it,
/// so the record it names is always emitted.
fn innermost_class(array: &str) -> Option<&str> {
    array
        .trim_start_matches('[')
        .strip_prefix('L')?
        .strip_suffix(';')
}

/// Split the parameter list of a method descriptor into single-type descriptors.
fn split_params(params: &str) -> Result<Vec<&str>> {
    let mut out = Vec::new();
    let mut rest = params;
    while !rest.is_empty() {
        let mut end = rest.bytes().take_while(|&b| b == b'[').count();
        end += match rest.as_bytes().get(end) {
            Some(b'L') => rest[end..]
                .find(';')
                .map(|i| i + 1)
                .unwrap_or(rest.len() + 1),
            Some(_) => 1,
            None => rest.len() + 1,
        };
        if end > rest.len() {
            bail!("invalid method descriptor parameters {params}");
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    Ok(out)
}

/// Parse a method descriptor.
pub fn parse_signature(descriptor: &str) -> Result<Signature> {
    let Some((params, ret)) = descriptor
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
    else {
        bail!("invalid method descriptor {descriptor}")
    };
    let params = split_params(params)?
        .into_iter()
        .map(|p| parse_type(p, descriptor))
        .collect::<Result<Vec<_>>>()?;
    let ret = if ret == "V" {
        Ty {
            code: 'V',
            class: None,
        }
    } else {
        parse_type(ret, descriptor)?
    };
    Ok(Signature { params, ret })
}

/// C type for a type code in generated prototypes (`void` for V).
pub fn c_type(c: char) -> &'static str {
    match kind_of(c) {
        Some(Kind::I) => "int32_t",
        Some(Kind::J) => "int64_t",
        Some(Kind::F) => "float",
        Some(Kind::D) => "double",
        Some(Kind::A) => "jabs_object *",
        None => "void",
    }
}

/// Member of `jabs_value` that holds a kind.
fn field(kind: Kind) -> &'static str {
    match kind {
        Kind::I => "i",
        Kind::J => "j",
        Kind::F => "f",
        Kind::D => "d",
        Kind::A => "a",
    }
}

/// Append `text` as a C identifier: alphanumerics kept, every other byte as `_xx` hex.
fn mangle(text: &str, out: &mut String) {
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(b as char);
        } else {
            let _ = write!(out, "_{b:02x}");
        }
    }
}

/// `prefix` + mangled parts joined by `__`.
fn mangled(prefix: &str, parts: &[&str]) -> String {
    let mut out = String::from(prefix);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str("__");
        }
        mangle(part, &mut out);
    }
    out
}

/// Stable C symbol for a Java method identity (class, name, descriptor).
pub fn symbol(class: &str, name: &str, descriptor: &str) -> String {
    mangled("jm_", &[class, name, descriptor])
}

/// C symbol for a static field's storage.
pub fn field_symbol(class: &str, name: &str) -> String {
    mangled("jf_", &[class, name])
}

/// C symbol of a class's initialization entry point; returns 0 when the class is ready, -1 with `jabs_pending` set.
pub fn init_symbol(class: &str) -> String {
    let mut out = mangled("jc_", &[class]);
    out.push_str("__init");
    out
}

/// C struct tag for a class's object layout.
fn object_struct(class: &str) -> String {
    format!("struct {}", mangled("jo_", &[class]))
}

/// C name of a class's runtime record.
/// C name of a class's `jabs_class` record.
pub fn class_record(class: &str) -> String {
    mangled("jk_", &[class])
}

/// C name of an instance field inside its declaring class's struct.
fn instance_field(name: &str) -> String {
    mangled("f_", &[name])
}

/// C storage type for a field, sized like the JVM's (so byte fields really hold a byte).
fn storage_type(descriptor: &str) -> Option<&'static str> {
    Some(match descriptor.as_bytes() {
        b"Z" => "uint8_t",
        b"B" => "int8_t",
        b"C" => "uint16_t",
        b"S" => "int16_t",
        b"I" => "int32_t",
        b"J" => "int64_t",
        b"F" => "float",
        b"D" => "double",
        [b'L', .., b';'] | [b'[', ..] => "jabs_object *",
        _ => return None,
    })
}

/// Expression converting a stack value into a field's storage type (put narrowing; boolean keeps bit 0).
fn narrow_for_store(descriptor: &str, value: &str) -> String {
    match descriptor {
        "Z" => format!("(uint8_t)({value} & 1)"),
        "B" => format!("(int8_t)jabs_i2b({value})"),
        "C" => format!("(uint16_t)jabs_i2c({value})"),
        "S" => format!("(int16_t)jabs_i2s({value})"),
        _ => value.to_string(),
    }
}

/// Whether the field a getfield/putfield/getstatic/putstatic resolved to is volatile.
fn field_is_volatile(unit: &Unit, declaring: &str, name: &str, descriptor: &str) -> bool {
    unit.get(declaring).is_some_and(|c| {
        c.fields
            .iter()
            .any(|f| f.name == name && f.descriptor == descriptor && f.access & ACC_VOLATILE != 0)
    })
}

/// C reading field `lvalue` into `target`. A volatile field is read with one sequentially
/// consistent atomic load: Java volatile semantics, and single-copy atomic for long and double
/// (JLS 17.7), which a plain C access does not promise.
fn field_load(lvalue: &str, target: &str, descriptor: &str, volatile: bool) -> String {
    if !volatile {
        return format!("{target} = {lvalue};");
    }
    let ty = storage_type(descriptor).expect("a field type");
    format!("{{ {ty} v; __atomic_load(&{lvalue}, &v, __ATOMIC_SEQ_CST); {target} = v; }}")
}

/// C storing `value` (already narrowed to the field's type) into field `lvalue`; a sequentially
/// consistent atomic store when the field is volatile.
fn field_store(lvalue: &str, value: &str, descriptor: &str, volatile: bool) -> String {
    if !volatile {
        return format!("{lvalue} = {value};");
    }
    let ty = storage_type(descriptor).expect("a field type");
    format!("{{ {ty} v = {value}; __atomic_store(&{lvalue}, &v, __ATOMIC_SEQ_CST); }}")
}

/// Runtime package of a class: its internal name up to the last `/`.
fn package(class: &str) -> &str {
    class.rfind('/').map_or("", |i| &class[..i])
}

/// A method identity: (class, name, descriptor).
pub type MethodKey = (String, String, String);

/// Stable 64-bit ID of an interface method signature; independent of which classes exist.
pub fn selector(name: &str, descriptor: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(descriptor.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().unwrap())
}

/// What an interface call selects on instances of a class (JVMS 5.4.6).
#[derive(Debug, Clone)]
enum ItableTarget {
    Impl(MethodKey),
    /// No implementation: AbstractMethodError.
    Abstract,
    /// Several equally specific default methods: IncompatibleClassChangeError.
    Ambiguous,
}

#[derive(Debug, Clone)]
struct ItableEntry {
    selector: u64,
    name: String,
    descriptor: String,
    target: ItableTarget,
}

/// One vtable slot: the method's name and descriptor, and which class's implementation fills it.
#[derive(Debug, Clone)]
struct Slot {
    name: String,
    descriptor: String,
    owner: String,
}

/// A memoized superclass or superinterface list, or why it could not be built.
type ChainResult<'a> = std::result::Result<Arc<Vec<&'a ClassFile>>, String>;

/// The classes compiled together, with thread-safe memoized hierarchy queries over them.
struct Unit<'a> {
    classes: HashMap<&'a str, &'a ClassFile>,
    /// Class-name interning so verification types stay `Copy`.
    names: RwLock<(Vec<String>, HashMap<String, u32>)>,
    vtables: RwLock<HashMap<String, Arc<Vec<Slot>>>>,
    chains: RwLock<HashMap<String, ChainResult<'a>>>,
    superinterfaces: RwLock<HashMap<String, ChainResult<'a>>>,
    itables: RwLock<HashMap<String, Arc<Vec<ItableEntry>>>>,
    signatures: RwLock<HashMap<String, std::result::Result<Arc<Signature>, String>>>,
    selectors: RwLock<HashMap<String, HashMap<String, u64>>>,
    structural: RwLock<HashMap<&'a str, Option<String>>>,
    caller_sensitive: HashSet<(&'a str, &'a str, &'a str)>,
    caller_sensitive_sigs: HashSet<(&'a str, &'a str)>,
    /// Every class that exists on the program's class path (its jars and the whole JDK), when
    /// known: a class outside it is absent, which the JVM reports as NoClassDefFoundError.
    known: Option<HashSet<String>>,
}

impl<'a> Unit<'a> {
    fn new(classes: &'a [ClassFile]) -> Self {
        let mut caller_sensitive = HashSet::new();
        let mut caller_sensitive_sigs = HashSet::new();
        for c in classes {
            for m in &c.methods {
                if m.annotations
                    .iter()
                    .any(|a| a == "Ljdk/internal/reflect/CallerSensitive;")
                {
                    caller_sensitive.insert((
                        c.name.as_str(),
                        m.name.as_str(),
                        m.descriptor.as_str(),
                    ));
                    caller_sensitive_sigs.insert((m.name.as_str(), m.descriptor.as_str()));
                }
            }
        }
        Unit {
            classes: classes.iter().map(|c| (c.name.as_str(), c)).collect(),
            names: RwLock::new((Vec::new(), HashMap::new())),
            vtables: RwLock::new(HashMap::new()),
            chains: RwLock::new(HashMap::new()),
            superinterfaces: RwLock::new(HashMap::new()),
            itables: RwLock::new(HashMap::new()),
            signatures: RwLock::new(HashMap::new()),
            selectors: RwLock::new(HashMap::new()),
            structural: RwLock::new(HashMap::new()),
            caller_sensitive,
            caller_sensitive_sigs,
            known: None,
        }
    }
    /// Whether `class` exists nowhere the program could load it from: not in the unit and not
    /// among the known class-path classes. False whenever that is not known (no evidence).
    fn absent(&self, class: &str) -> bool {
        !class.starts_with('[')
            && !self.classes.contains_key(class)
            && self
                .known
                .as_ref()
                .is_some_and(|known| !known.contains(class))
    }
    fn get(&self, name: &str) -> Option<&'a ClassFile> {
        self.classes.get(name).copied()
    }
    fn intern(&self, name: &str) -> u32 {
        {
            let names = self.names.read().unwrap();
            if let Some(&id) = names.1.get(name) {
                return id;
            }
        }
        let mut names = self.names.write().unwrap();
        if let Some(&id) = names.1.get(name) {
            return id;
        }
        let id = names.0.len() as u32;
        names.0.push(name.to_string());
        names.1.insert(name.to_string(), id);
        id
    }
    fn name(&self, id: u32) -> String {
        self.names.read().unwrap().0[id as usize].clone()
    }
    fn with_name<R>(&self, id: u32, f: impl FnOnce(&str) -> R) -> R {
        let names = self.names.read().unwrap();
        f(&names.0[id as usize])
    }
    fn with_two_names<R>(&self, x: u32, y: u32, f: impl FnOnce(&str, &str) -> R) -> R {
        let names = self.names.read().unwrap();
        f(&names.0[x as usize], &names.0[y as usize])
    }
    fn selector(&self, name: &str, descriptor: &str) -> u64 {
        {
            let map = self.selectors.read().unwrap();
            if let Some(&s) = map.get(name).and_then(|m| m.get(descriptor)) {
                return s;
            }
        }
        let s = selector(name, descriptor);
        let mut map = self.selectors.write().unwrap();
        *map.entry(name.to_string())
            .or_default()
            .entry(descriptor.to_string())
            .or_insert(s)
    }
    fn signature(&self, descriptor: &str) -> Result<Arc<Signature>> {
        {
            let sigs = self.signatures.read().unwrap();
            if let Some(cached) = sigs.get(descriptor) {
                return cached.clone().map_err(|e| anyhow!(e));
            }
        }
        let computed = parse_signature(descriptor)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        let mut sigs = self.signatures.write().unwrap();
        let entry = sigs
            .entry(descriptor.to_string())
            .or_insert(computed)
            .clone();
        entry.map_err(|e| anyhow!(e))
    }
    fn structural_problem_cached(&self, class: &'a ClassFile) -> Option<String> {
        {
            let cache = self.structural.read().unwrap();
            if let Some(res) = cache.get(class.name.as_str()) {
                return res.clone();
            }
        }
        let computed = if let Err(e) = self.chain(&class.name) {
            Some(format!("class hierarchy incomplete: {e}"))
        } else if let Err(e) = self.superinterfaces_arc(&class.name) {
            Some(format!("class hierarchy incomplete: {e}"))
        } else {
            None
        };
        let mut cache = self.structural.write().unwrap();
        cache.entry(class.name.as_str()).or_insert(computed).clone()
    }
    /// The class and its superclasses up to (not including) java/lang/Object; fails if a link is outside the unit.
    fn chain(&self, name: &str) -> Result<Arc<Vec<&'a ClassFile>>> {
        {
            let chains = self.chains.read().unwrap();
            if let Some(known) = chains.get(name) {
                return known.clone().map_err(|e| anyhow!(e));
            }
        }
        let computed = self
            .compute_chain(name)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        let mut chains = self.chains.write().unwrap();
        let entry = chains.entry(name.to_string()).or_insert(computed).clone();
        entry.map_err(|e| anyhow!(e))
    }
    /// Walk superclass links without memoization.
    fn compute_chain(&self, name: &str) -> Result<Vec<&'a ClassFile>> {
        let mut out = Vec::new();
        let mut current = name;
        while current != OBJECT {
            let Some(class) = self.get(current) else {
                bail!("{current} is not in the compilation unit");
            };
            if out.len() > 10_000 {
                bail!("superclass cycle at {current}");
            }
            out.push(class);
            current = class.super_name.as_deref().unwrap_or(OBJECT);
        }
        Ok(out)
    }
    /// Whether `sub` is `sup` or one of its subclasses; None if the hierarchy is not known.
    fn is_subclass(&self, sub: &str, sup: &str) -> Option<bool> {
        if sub == sup || sup == OBJECT {
            return Some(true);
        }
        Some(self.chain(sub).ok()?.iter().any(|c| c.name == sup))
    }
    fn is_interface(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|c| c.access & ACC_INTERFACE != 0)
    }
    /// Memoized superinterfaces list shared via `Arc`.
    fn superinterfaces_arc(&self, name: &str) -> Result<Arc<Vec<&'a ClassFile>>> {
        {
            let ifaces = self.superinterfaces.read().unwrap();
            if let Some(known) = ifaces.get(name) {
                return known.clone().map_err(|e| anyhow!(e));
            }
        }
        let computed = self
            .compute_superinterfaces(name)
            .map(Arc::new)
            .map_err(|e| e.to_string());
        let mut ifaces = self.superinterfaces.write().unwrap();
        let entry = ifaces.entry(name.to_string()).or_insert(computed).clone();
        entry.map_err(|e| anyhow!(e))
    }
    fn compute_superinterfaces(&self, name: &str) -> Result<Vec<&'a ClassFile>> {
        let mut out: Vec<&'a ClassFile> = Vec::new();
        let mut seen: HashSet<&'a str> = HashSet::new();
        let mut pending: Vec<&'a str> = Vec::new();
        for class in self.chain(name)?.iter() {
            pending.extend(class.interfaces.iter().map(String::as_str));
        }
        let mut i = 0;
        while i < pending.len() {
            let Some(iface) = self.get(pending[i]) else {
                bail!("interface {} is not in the compilation unit", pending[i]);
            };
            if seen.insert(iface.name.as_str()) {
                out.push(iface);
                pending.extend(iface.interfaces.iter().map(String::as_str));
            }
            i += 1;
        }
        Ok(out)
    }
    /// All superinterfaces of a class, direct and indirect, in declaration order without repeats.
    fn superinterfaces(&self, name: &str) -> Result<Vec<&'a ClassFile>> {
        self.superinterfaces_arc(name).map(|v| (*v).clone())
    }
    /// JVMS 5.4.3.3: find a method in the class or its superclasses.
    fn resolve_method(
        &self,
        owner: &str,
        name: &str,
        descriptor: &str,
    ) -> Result<(&'a ClassFile, &'a Member)> {
        for &class in self
            .chain(owner)
            .map_err(|e| anyhow!("call to {owner}.{name}: {e}"))?
            .iter()
        {
            if let Some(m) = class
                .methods
                .iter()
                .find(|m| m.name == name && m.descriptor == descriptor)
            {
                return Ok((class, m));
            }
        }
        if let Some(object) = self.get(OBJECT)
            && let Some(m) = object
                .methods
                .iter()
                .find(|m| m.name == name && m.descriptor == descriptor)
        {
            return Ok((object, m));
        }
        bail!(
            "{owner}.{name}{descriptor} is inherited from outside the unit (e.g. java/lang/Object)"
        )
    }
    /// JVMS 5.4.3.2: find a field in the class, its superinterfaces, then its superclasses.
    fn resolve_field(
        &self,
        owner: &str,
        name: &str,
        descriptor: &str,
    ) -> Result<(&'a ClassFile, &'a Member)> {
        let find = |c: &'a ClassFile| {
            c.fields
                .iter()
                .find(|f| f.name == name && f.descriptor == descriptor)
        };
        let chain = self
            .chain(owner)
            .map_err(|e| anyhow!("field {owner}.{name}: {e}"))?;
        for &class in chain.iter() {
            if let Some(f) = find(class) {
                return Ok((class, f));
            }
            for &iface in self.superinterfaces_arc(&class.name)?.iter() {
                if let Some(f) = find(iface) {
                    return Ok((iface, f));
                }
            }
        }
        bail!("field {owner}.{name} is not declared in the unit")
    }
    /// JVMS 5.4.3.4: find an interface method in the interface or its superinterfaces.
    fn resolve_interface_method(
        &self,
        iface: &str,
        name: &str,
        descriptor: &str,
    ) -> Result<(&'a ClassFile, &'a Member)> {
        let first = self
            .get(iface)
            .ok_or_else(|| anyhow!("{iface} is not in the compilation unit"))?;
        for class in std::iter::once(first).chain(self.superinterfaces_arc(iface)?.iter().copied())
        {
            if let Some(m) = class.methods.iter().find(|m| {
                m.name == name && m.descriptor == descriptor && m.access & ACC_STATIC == 0
            }) {
                return Ok((class, m));
            }
        }
        bail!(
            "{iface}.{name}{descriptor} is inherited from outside the unit (e.g. java/lang/Object)"
        )
    }
    /// Interface dispatch table of a class: for every method of every superinterface, the
    /// implementation selected per JVMS 5.4.6, sorted by selector.
    fn itable(&self, name: &str) -> Arc<Vec<ItableEntry>> {
        {
            let itables = self.itables.read().unwrap();
            if let Some(t) = itables.get(name) {
                return t.clone();
            }
        }
        let mut entries: Vec<ItableEntry> = Vec::new();
        if let (Ok(chain), Ok(ifaces)) = (self.chain(name), self.superinterfaces_arc(name)) {
            let mut seen: HashSet<(&str, &str)> = HashSet::new();
            for iface in ifaces.iter() {
                for m in iface.methods.iter().filter(|m| {
                    m.access & (ACC_STATIC | ACC_PRIVATE) == 0 && !m.name.starts_with('<')
                }) {
                    if !seen.insert((m.name.as_str(), m.descriptor.as_str())) {
                        continue;
                    }
                    // 1. A method in the class or a superclass wins (abstract ones throw when called).
                    let in_class = chain.iter().copied().chain(self.get(OBJECT)).find_map(|c| {
                        c.methods
                            .iter()
                            .find(|x| {
                                x.name == m.name
                                    && x.descriptor == m.descriptor
                                    && x.access & (ACC_STATIC | ACC_PRIVATE) == 0
                            })
                            .map(|x| (c, x))
                    });
                    let target = match in_class {
                        Some((_, x)) if x.access & ACC_ABSTRACT != 0 => ItableTarget::Abstract,
                        Some((c, _)) => ItableTarget::Impl((
                            c.name.clone(),
                            m.name.clone(),
                            m.descriptor.clone(),
                        )),
                        None => {
                            // 2. Otherwise the maximally specific default method, if exactly one.
                            let candidates: Vec<&ClassFile> = ifaces
                                .iter()
                                .copied()
                                .filter(|i| {
                                    i.methods.iter().any(|x| {
                                        x.name == m.name
                                            && x.descriptor == m.descriptor
                                            && x.access & (ACC_STATIC | ACC_PRIVATE | ACC_ABSTRACT)
                                                == 0
                                    })
                                })
                                .collect();
                            let specific: Vec<&ClassFile> = candidates
                                .iter()
                                .copied()
                                .filter(|c| {
                                    !candidates.iter().any(|other| {
                                        other.name != c.name
                                            && self.superinterfaces_arc(&other.name).is_ok_and(
                                                |sup| sup.iter().any(|s| s.name == c.name),
                                            )
                                    })
                                })
                                .collect();
                            match specific.as_slice() {
                                [one] => ItableTarget::Impl((
                                    one.name.clone(),
                                    m.name.clone(),
                                    m.descriptor.clone(),
                                )),
                                [] => ItableTarget::Abstract,
                                _ => ItableTarget::Ambiguous,
                            }
                        }
                    };
                    entries.push(ItableEntry {
                        selector: self.selector(&m.name, &m.descriptor),
                        name: m.name.clone(),
                        descriptor: m.descriptor.clone(),
                        target,
                    });
                }
            }
        }
        entries.sort_by_key(|e| e.selector);
        for pair in entries.windows(2) {
            assert!(
                pair[0].selector != pair[1].selector,
                "interface selector collision in {name}: {} and {}",
                pair[0].name,
                pair[1].name
            );
        }
        let entries = Arc::new(entries);
        let mut itables = self.itables.write().unwrap();
        itables.entry(name.to_string()).or_insert(entries).clone()
    }
    /// Whether a method declared in `owner` with `access` is overridden by a method in class `by` (JVMS 5.4.5).
    fn overrides(owner: &str, access: u16, by: &str) -> bool {
        const PUBLIC_OR_PROTECTED: u16 = 0x0001 | 0x0004;
        access & ACC_PRIVATE == 0
            && (access & PUBLIC_OR_PROTECTED != 0 || package(owner) == package(by))
    }
    /// Vtable of a class: the superclass's slots (overridden where applicable), then new methods.
    fn vtable(&self, name: &str) -> Result<Arc<Vec<Slot>>> {
        {
            let vtables = self.vtables.read().unwrap();
            if let Some(v) = vtables.get(name) {
                return Ok(v.clone());
            }
        }
        let class = self
            .get(name)
            .ok_or_else(|| anyhow!("{name} is not in the compilation unit"))?;
        let mut slots = match class.super_name.as_deref() {
            Some(parent) if parent != OBJECT && class.access & ACC_INTERFACE == 0 => {
                (*self.vtable(parent)?).clone()
            }
            // Directly below Object: start from Object's own slots (hashCode, equals, ...).
            Some(OBJECT) if class.access & ACC_INTERFACE == 0 && self.get(OBJECT).is_some() => {
                (*self.vtable(OBJECT)?).clone()
            }
            _ => Vec::new(),
        };
        if class.access & ACC_INTERFACE == 0 {
            for m in class
                .methods
                .iter()
                .filter(|m| m.access & (ACC_STATIC | ACC_PRIVATE) == 0 && !m.name.starts_with('<'))
            {
                let overridden = slots.iter().position(|s| {
                    s.name == m.name
                        && s.descriptor == m.descriptor
                        && self
                            .get(&s.owner)
                            .and_then(|o| {
                                o.methods
                                    .iter()
                                    .find(|x| x.name == s.name && x.descriptor == s.descriptor)
                            })
                            .is_some_and(|x| Self::overrides(&s.owner, x.access, name))
                });
                let slot = Slot {
                    name: m.name.clone(),
                    descriptor: m.descriptor.clone(),
                    owner: name.to_string(),
                };
                match overridden {
                    Some(i) => slots[i] = slot,
                    None => slots.push(slot),
                }
            }
        }
        let slots = Arc::new(slots);
        let mut vtables = self.vtables.write().unwrap();
        Ok(vtables.entry(name.to_string()).or_insert(slots).clone())
    }
}

/// Verification type of a local variable or stack slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VType {
    /// Unusable (unset, the second half of a long/double, or incompatible types merged).
    Top,
    I,
    J,
    F,
    D,
    Null,
    /// Initialized reference whose static type is the interned class name.
    Obj(u32),
    /// `this` inside a constructor before a superclass or sibling constructor has run.
    UninitThis,
    /// Result of `new` at this pc, before its constructor has run.
    Uninit(u32),
}

impl VType {
    /// Verification type of a primitive kind (references need a class; see `Cx::of_ty`).
    fn prim(kind: Kind) -> VType {
        match kind {
            Kind::I => VType::I,
            Kind::J => VType::J,
            Kind::F => VType::F,
            Kind::D => VType::D,
            Kind::A => unreachable!("references carry a class"),
        }
    }
    fn category(self) -> u16 {
        if matches!(self, VType::J | VType::D) {
            2
        } else {
            1
        }
    }
    fn is_reference(self) -> bool {
        matches!(
            self,
            VType::Null | VType::Obj(_) | VType::UninitThis | VType::Uninit(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Frame {
    stack: Vec<VType>,
    locals: Vec<VType>,
}

/// How a call instruction compiles.
#[derive(Debug, Clone)]
enum CallKind {
    /// Static or non-overridable target: call the implementation's symbol.
    Direct(MethodKey),
    /// Dispatch through vtable slot `slot` of the receiver's class.
    Virtual { slot: usize, target: MethodKey },
    /// `java/lang/Object.<init>()V`: no effect.
    ObjectInit,
    /// Select the implementation from the receiver's interface table by selector.
    Interface { selector: u64, target: MethodKey },
}

/// What verification resolved for one instruction, for emission.
#[derive(Debug, Clone)]
enum Resolved {
    Call(CallKind),
    /// A signature-polymorphic VarHandle access (`get`, `compareAndSet`, ...): compiled to a
    /// memory operation on the address the handle describes.
    VarHandle(String),
    /// Field access resolved to its declaring class.
    Field(String),
    /// Array access: the element descriptor char of the array's static type (`Z` vs `B` matters
    /// for stores), or None when the array is statically null (the access always throws).
    Element(Option<char>),
    /// Member of a class outside the unit: the instruction stops with this diagnostic if it
    /// ever executes (the JVM likewise resolves references lazily, when first executed).
    Unresolved(String),
    /// A class on no class path (proved absent): the instruction throws NoClassDefFoundError when
    /// it executes, as the JVM's lazy resolution does; a type test of null resolves nothing.
    Absent(String),
    /// A type test against a class that exists but is not compiled in: a null value passes as
    /// in HotSpot (nothing is resolved), any other value stops with this diagnostic.
    UnresolvedType(String),
}

/// Something a compiled method needs to be compiled too.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Dep {
    Method(MethodKey),
    Init(String),
}

/// Facts about the method being verified that instruction checks need.
struct Cx<'u, 'a> {
    unit: &'u Unit<'a>,
    class: &'a ClassFile,
    sig: &'u Signature,
    is_constructor: bool,
}

impl Cx<'_, '_> {
    fn describe(&self, v: VType) -> String {
        match v {
            VType::Obj(id) => self.unit.name(id),
            other => format!("{other:?}"),
        }
    }
    /// Verification type of a descriptor type.
    fn of_ty(&self, ty: &Ty) -> VType {
        match &ty.class {
            Some(class) => VType::Obj(self.unit.intern(class)),
            None => VType::prim(kind_of(ty.code).unwrap()),
        }
    }
    /// Whether a value may be used where `target` is expected. Interfaces accept any
    /// reference, as in the JVM verifier; unknown hierarchies are rejected.
    fn assignable(&self, v: VType, target: &str) -> bool {
        match v {
            VType::Null => true,
            VType::Obj(id) => self.unit.with_name(id, |n| self.name_assignable(n, target)),
            _ => false,
        }
    }
    /// Whether a value of static type `name` (class name or array descriptor) fits `target`.
    fn name_assignable(&self, name: &str, target: &str) -> bool {
        if target == OBJECT || name == target {
            return true;
        }
        match (name.starts_with('['), target.starts_with('[')) {
            // Arrays are covariant in their reference element type; primitive arrays must match exactly.
            (true, true) => match (element_class(name), element_class(target)) {
                (Some(a), Some(b)) => self.name_assignable(&a, &b),
                _ => false,
            },
            (true, false) => matches!(target, "java/lang/Cloneable" | "java/io/Serializable"),
            (false, true) => false,
            (false, false) => {
                // A class outside the unit is opaque: nothing native accesses a value through it,
                // so any reference may stand in for it (as the JVM would only check at link time).
                self.unit.get(target).is_none()
                    || self.unit.is_interface(target)
                    || self.unit.is_subclass(name, target) == Some(true)
            }
        }
    }
    /// Least common supertype at a control-flow join (java/lang/Object when nothing closer is known).
    fn merge(&self, a: VType, b: VType) -> Option<VType> {
        match (a, b) {
            (a, b) if a == b => Some(a),
            (VType::Null, o @ VType::Obj(_)) | (o @ VType::Obj(_), VType::Null) => Some(o),
            (VType::Obj(x), VType::Obj(y)) => {
                let common = self
                    .unit
                    .with_two_names(x, y, |nx, ny| self.merge_names(nx, ny, 0));
                Some(VType::Obj(self.unit.intern(&common)))
            }
            _ => None,
        }
    }
    /// The join of two reference types by name (class names or array descriptors), as the
    /// JVM's inference verifier computes it (JVMS 4.10.2.2): two arrays of references join to an
    /// array of their joined element types; an array with anything else, a primitive array that
    /// differs, or a class with an interface joins to java/lang/Object; two classes join to
    /// their nearest common superclass. `depth` bounds the recursion through array dimensions.
    fn merge_names(&self, x: &str, y: &str, depth: u32) -> String {
        const MAX_ARRAY_DIMENSIONS: u32 = 255;
        if x == y {
            return x.to_string();
        }
        if x.starts_with('[') || y.starts_with('[') {
            if depth >= MAX_ARRAY_DIMENSIONS || !x.starts_with('[') || !y.starts_with('[') {
                return OBJECT.to_string();
            }
            let (Some(a), Some(b)) = (element_class(x), element_class(y)) else {
                return OBJECT.to_string();
            };
            let element = self.merge_names(&a, &b, depth + 1);
            return if element.starts_with('[') {
                format!("[{element}")
            } else {
                format!("[L{element};")
            };
        }
        self.unit
            .chain(x)
            .ok()
            .and_then(|chain| {
                chain
                    .iter()
                    .find(|c| self.unit.is_subclass(y, &c.name) == Some(true))
                    .map(|c| c.name.clone())
            })
            .unwrap_or_else(|| OBJECT.to_string())
    }
    /// Merge two frames at a join: stack shapes must agree, locals that disagree become unusable.
    fn merge_frames(&self, a: &Frame, b: &Frame, pc: u32) -> Result<Frame> {
        if a.stack.len() != b.stack.len() {
            bail!("pc {pc}: stack depth differs between paths");
        }
        let stack = a
            .stack
            .iter()
            .zip(&b.stack)
            .map(|(x, y)| {
                self.merge(*x, *y).ok_or_else(|| {
                    anyhow!(
                        "pc {pc}: stack types {} and {} disagree",
                        self.describe(*x),
                        self.describe(*y)
                    )
                })
            })
            .collect::<Result<_>>()?;
        let locals = a
            .locals
            .iter()
            .zip(&b.locals)
            .map(|(x, y)| self.merge(*x, *y).unwrap_or(VType::Top))
            .collect();
        Ok(Frame { stack, locals })
    }
}

impl Frame {
    fn pop(&mut self, pc: u32) -> Result<VType> {
        self.stack
            .pop()
            .ok_or_else(|| anyhow!("pc {pc}: stack underflow"))
    }
    /// Pop one value and require it to have exactly the primitive type `want`.
    fn pop_prim(&mut self, want: Kind, pc: u32) -> Result<()> {
        match self.pop(pc)? {
            v if want != Kind::A && v == VType::prim(want) => Ok(()),
            other => bail!("pc {pc}: expected {want:?} on stack, found {other:?}"),
        }
    }
    /// Pop an initialized reference assignable to `target`.
    fn pop_ref(&mut self, cx: &Cx, target: &str, pc: u32) -> Result<VType> {
        let v = self.pop(pc)?;
        if !cx.assignable(v, target) {
            bail!(
                "pc {pc}: expected a {target} reference, found {} (or its hierarchy is outside the unit)",
                cx.describe(v)
            );
        }
        Ok(v)
    }
    /// Pop a value of a descriptor type.
    fn pop_ty(&mut self, cx: &Cx, ty: &Ty, pc: u32) -> Result<()> {
        match &ty.class {
            Some(class) => self.pop_ref(cx, class, pc).map(|_| ()),
            None => self.pop_prim(kind_of(ty.code).unwrap(), pc),
        }
    }
    /// Pop one value that is not a long/double, as pop/dup/swap require.
    fn pop_cat1(&mut self, pc: u32) -> Result<VType> {
        match self.pop(pc)? {
            v if v.category() == 1 => Ok(v),
            other => bail!("pc {pc}: expected category-1 value, found {other:?}"),
        }
    }
    fn local(&self, n: u16, pc: u32) -> Result<VType> {
        self.locals
            .get(n as usize)
            .copied()
            .ok_or_else(|| anyhow!("pc {pc}: local {n} out of range"))
    }
    /// Store into a local; long/double take two slots and break any pair they overwrite.
    fn set_local(&mut self, n: u16, v: VType, pc: u32) -> Result<()> {
        let n = n as usize;
        if n + v.category() as usize > self.locals.len() {
            bail!("pc {pc}: local {n} out of range");
        }
        if n > 0 && self.locals[n - 1].category() == 2 {
            self.locals[n - 1] = VType::Top;
        }
        self.locals[n] = v;
        if v.category() == 2 {
            self.locals[n + 1] = VType::Top;
        }
        Ok(())
    }
    /// After a constructor call, every copy of the uninitialized object becomes an initialized `class`.
    fn initialize(&mut self, uninit: VType, class: VType) {
        for v in self.stack.iter_mut().chain(self.locals.iter_mut()) {
            if *v == uninit {
                *v = class;
            }
        }
    }
}

/// What one instruction does to control flow and what it depends on.
#[derive(Default)]
struct Step {
    /// Execution can continue with the next instruction.
    falls: bool,
    targets: Vec<u32>,
    deps: Vec<Dep>,
    resolved: Option<Resolved>,
}

/// Verify one instruction against the frame and report its control flow and dependencies.
fn step(insn: &Insn, frame: &mut Frame, cx: &Cx, news: &HashMap<u32, &str>) -> Result<Step> {
    let pc = insn.pc;
    let unit = cx.unit;
    let this = &cx.class.name;
    let mut out = Step {
        falls: true,
        ..Step::default()
    };
    let outside = |class: &str| !class.starts_with('[') && unit.get(class).is_none();
    let unresolved = match &insn.op {
        Op::InvokeStatic {
            class,
            name,
            descriptor,
        } if outside(class) => Some((
            class.as_str(),
            name.as_str(),
            descriptor.as_str(),
            false,
            None,
        )),
        Op::InvokeVirtual(m) | Op::InvokeInterface(m) if outside(&m.class) => Some((
            m.class.as_str(),
            m.name.as_str(),
            m.descriptor.as_str(),
            true,
            None,
        )),
        Op::InvokeSpecial(m) if outside(&m.class) => {
            if m.name == "<init>" {
                let s = unit.signature(&m.descriptor)?;
                for p in s.params.iter().rev() {
                    frame.pop_ty(cx, p, pc)?;
                }
                let parent = cx.class.super_name.as_deref().unwrap_or(OBJECT);
                match frame.pop(pc)? {
                    v @ VType::Uninit(at) if news.get(&at).copied() == Some(m.class.as_str()) => {
                        frame.initialize(v, VType::Obj(unit.intern(&m.class)));
                    }
                    VType::UninitThis
                        if cx.is_constructor && (m.class == *this || m.class == parent) =>
                    {
                        frame.initialize(VType::UninitThis, VType::Obj(unit.intern(this)));
                    }
                    other => bail!(
                        "pc {pc}: constructor of {} called on {}",
                        m.class,
                        cx.describe(other)
                    ),
                }
                out.resolved = Some(if unit.absent(&m.class) {
                    Resolved::Absent(m.class.clone())
                } else {
                    Resolved::Unresolved(format!(
                        "{}.{}{}: {} is not in the compilation unit",
                        m.class, m.name, m.descriptor, m.class
                    ))
                });
                return Ok(out);
            }
            Some((
                m.class.as_str(),
                m.name.as_str(),
                m.descriptor.as_str(),
                true,
                None,
            ))
        }
        Op::GetStatic {
            class,
            name,
            descriptor,
        } if outside(class) => Some((
            class.as_str(),
            name.as_str(),
            descriptor.as_str(),
            false,
            Some(true),
        )),
        Op::PutStatic {
            class,
            name,
            descriptor,
        } if outside(class) => Some((
            class.as_str(),
            name.as_str(),
            descriptor.as_str(),
            false,
            Some(false),
        )),
        Op::GetField(m) if outside(&m.class) => Some((
            m.class.as_str(),
            m.name.as_str(),
            m.descriptor.as_str(),
            true,
            Some(true),
        )),
        Op::PutField(m) if outside(&m.class) => Some((
            m.class.as_str(),
            m.name.as_str(),
            m.descriptor.as_str(),
            true,
            Some(false),
        )),
        _ => None,
    };
    if let Some((class, name, descriptor, receiver, field)) = unresolved {
        match field {
            None => {
                let s = unit.signature(descriptor)?;
                for p in s.params.iter().rev() {
                    frame.pop_ty(cx, p, pc)?;
                }
                if receiver {
                    frame.pop_ref(cx, OBJECT, pc)?;
                }
                if s.ret.code != 'V' {
                    frame.stack.push(cx.of_ty(&s.ret));
                }
            }
            Some(is_get) => {
                let ty = parse_type(descriptor, name)?;
                if !is_get {
                    frame.pop_ty(cx, &ty, pc)?;
                }
                if receiver {
                    frame.pop_ref(cx, OBJECT, pc)?;
                }
                if is_get {
                    frame.stack.push(cx.of_ty(&ty));
                }
            }
        }
        out.resolved = Some(if unit.absent(class) {
            Resolved::Absent(class.to_string())
        } else {
            Resolved::Unresolved(format!(
                "{class}.{name}{descriptor}: {class} is not in the compilation unit"
            ))
        });
        return Ok(out);
    }
    match &insn.op {
        Op::Nop => {}
        Op::Goto(t) => {
            out.falls = false;
            out.targets.push(*t);
        }
        Op::Const(v) => frame.stack.push(VType::prim(v.kind())),
        Op::AConstNull => frame.stack.push(VType::Null),
        Op::Load(Kind::A, n) => {
            let v = frame.local(*n, pc)?;
            if !v.is_reference() {
                bail!("pc {pc}: aload of local {n}, which holds {v:?}");
            }
            frame.stack.push(v);
        }
        Op::Load(k, n) => {
            let v = frame.local(*n, pc)?;
            if v != VType::prim(*k) {
                bail!("pc {pc}: load of {k:?} from local {n}, which holds {v:?}");
            }
            frame.stack.push(v);
        }
        Op::Store(Kind::A, n) => {
            let v = frame.pop(pc)?;
            if !v.is_reference() {
                bail!("pc {pc}: astore of {v:?}");
            }
            frame.set_local(*n, v, pc)?;
        }
        Op::Store(k, n) => {
            frame.pop_prim(*k, pc)?;
            frame.set_local(*n, VType::prim(*k), pc)?;
        }
        Op::Iinc(n, _) => {
            if frame.local(*n, pc)? != VType::I {
                bail!("pc {pc}: iinc of non-int local {n}");
            }
        }
        Op::Pop => {
            frame.pop_cat1(pc)?;
        }
        Op::Pop2 => {
            if frame.stack.last().is_some_and(|v| v.category() == 2) {
                frame.pop(pc)?;
            } else {
                frame.pop_cat1(pc)?;
                frame.pop_cat1(pc)?;
            }
        }
        Op::Dup => {
            let v = frame.pop_cat1(pc)?;
            frame.stack.extend([v, v]);
        }
        Op::DupX1 => {
            let a = frame.pop_cat1(pc)?;
            let b = frame.pop_cat1(pc)?;
            frame.stack.extend([a, b, a]);
        }
        Op::Dup2 => {
            if let Some(&v) = frame.stack.last().filter(|v| v.category() == 2) {
                frame.stack.push(v);
            } else {
                let a = frame.pop_cat1(pc)?;
                let b = frame.pop_cat1(pc)?;
                frame.stack.extend([b, a, b, a]);
            }
        }
        Op::Swap => {
            let a = frame.pop_cat1(pc)?;
            let b = frame.pop_cat1(pc)?;
            frame.stack.extend([a, b]);
        }
        Op::DupX2 | Op::Dup2X1 | Op::Dup2X2 => {
            let categories: Vec<u16> = frame
                .stack
                .iter()
                .rev()
                .take(4)
                .map(|v| v.category())
                .collect();
            let (take, order) = stack_shuffle(&insn.op, &categories)
                .ok_or_else(|| anyhow!("pc {pc}: operand stack does not fit this dup form"))?;
            let at = frame.stack.len() - take;
            let taken: Vec<VType> = frame.stack.drain(at..).collect();
            frame.stack.extend(order.iter().map(|&i| taken[i]));
        }
        Op::Binary(k, op) => {
            let shift = matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr);
            frame.pop_prim(if shift { Kind::I } else { *k }, pc)?;
            frame.pop_prim(*k, pc)?;
            frame.stack.push(VType::prim(*k));
        }
        Op::Neg(k) => {
            frame.pop_prim(*k, pc)?;
            frame.stack.push(VType::prim(*k));
        }
        Op::Convert(c) => {
            let (from, to) = c.kinds();
            frame.pop_prim(from, pc)?;
            frame.stack.push(VType::prim(to));
        }
        Op::Compare(c) => {
            let k = match c {
                Compare::Lcmp => Kind::J,
                Compare::Fcmpl | Compare::Fcmpg => Kind::F,
                Compare::Dcmpl | Compare::Dcmpg => Kind::D,
            };
            frame.pop_prim(k, pc)?;
            frame.pop_prim(k, pc)?;
            frame.stack.push(VType::I);
        }
        Op::If(_, t) => {
            frame.pop_prim(Kind::I, pc)?;
            out.targets.push(*t);
        }
        Op::IfICmp(_, t) => {
            frame.pop_prim(Kind::I, pc)?;
            frame.pop_prim(Kind::I, pc)?;
            out.targets.push(*t);
        }
        Op::IfNull { target, .. } => {
            frame.pop_ref(cx, OBJECT, pc)?;
            out.targets.push(*target);
        }
        Op::IfACmp { target, .. } => {
            frame.pop_ref(cx, OBJECT, pc)?;
            frame.pop_ref(cx, OBJECT, pc)?;
            out.targets.push(*target);
        }
        Op::TableSwitch {
            default, targets, ..
        } => {
            frame.pop_prim(Kind::I, pc)?;
            out.falls = false;
            out.targets.push(*default);
            out.targets.extend(targets);
        }
        Op::LookupSwitch { default, pairs } => {
            frame.pop_prim(Kind::I, pc)?;
            out.falls = false;
            out.targets.push(*default);
            out.targets.extend(pairs.iter().map(|(_, t)| *t));
        }
        Op::Return(k) => {
            if *k != kind_of(cx.sig.ret.code) {
                bail!(
                    "pc {pc}: return kind {k:?} does not match descriptor return {}",
                    cx.sig.ret.code
                );
            }
            if k.is_some() {
                frame.pop_ty(cx, &cx.sig.ret, pc)?;
            }
            if cx.is_constructor && frame.locals.first() == Some(&VType::UninitThis) {
                bail!("pc {pc}: constructor returns before calling a superclass constructor");
            }
            out.falls = false;
        }
        Op::InvokeStatic {
            class: owner,
            name,
            descriptor,
        } => {
            let (declaring, target) = unit.resolve_method(owner, name, descriptor)?;
            if target.access & ACC_STATIC == 0 {
                bail!("pc {pc}: invokestatic of instance method {name}");
            }
            let s = unit.signature(descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), name.clone(), descriptor.clone());
            out.deps
                .extend([Dep::Method(key.clone()), Dep::Init(declaring.name.clone())]);
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
        Op::GetStatic {
            class: owner,
            name,
            descriptor,
        }
        | Op::PutStatic {
            class: owner,
            name,
            descriptor,
        } => {
            let (declaring, f) = unit.resolve_field(owner, name, descriptor)?;
            if f.access & ACC_STATIC == 0 {
                bail!("pc {pc}: static access to instance field {name}");
            }
            let ty = parse_type(descriptor, name)?;
            if matches!(insn.op, Op::GetStatic { .. }) {
                frame.stack.push(cx.of_ty(&ty));
            } else {
                frame.pop_ty(cx, &ty, pc)?;
            }
            out.deps.push(Dep::Init(declaring.name.clone()));
            out.resolved = Some(Resolved::Field(declaring.name.clone()));
        }
        Op::New { class } => {
            let Some(target) = unit.get(class) else {
                if unit.absent(class) {
                    frame.stack.push(VType::Uninit(pc));
                    out.resolved = Some(Resolved::Absent(class.clone()));
                    return Ok(out);
                }
                bail!("pc {pc}: new {class}: class is not in the compilation unit");
            };
            if target.access & (ACC_ABSTRACT | ACC_INTERFACE) != 0 {
                bail!("pc {pc}: new of abstract class or interface {class}");
            }
            unit.chain(class)?;
            frame.stack.push(VType::Uninit(pc));
            out.deps.push(Dep::Init(class.clone()));
        }
        Op::LdcString(_) => {
            let Some(string) = unit.get(STRING) else {
                bail!(
                    "pc {pc}: string constants need java/lang/String in the unit (add the JDK's java.base)"
                );
            };
            for (name, descriptor) in STRING_FIELDS {
                if !string.fields.iter().any(|f| {
                    f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                }) {
                    bail!(
                        "pc {pc}: this JDK's String has no {name}:{descriptor} field; only the JDK 9+ layout is supported"
                    );
                }
            }
            unit.chain(STRING)?;
            frame.stack.push(VType::Obj(unit.intern(STRING)));
            // The JVM initializes String before any literal exists; String's own code relies on it.
            out.deps.push(Dep::Init(STRING.to_string()));
        }
        Op::LdcClass(_) => {
            if !classes_available(unit) {
                bail!(
                    "pc {pc}: class constants need java/lang/Class and String in the unit (add the JDK's java.base)"
                );
            }
            frame.stack.push(VType::Obj(unit.intern(CLASS)));
        }
        Op::NewArray(element) => {
            frame.pop_prim(Kind::I, pc)?;
            frame
                .stack
                .push(VType::Obj(unit.intern(&format!("[{element}"))));
        }
        Op::ANewArray { class } => {
            frame.pop_prim(Kind::I, pc)?;
            let element = if class.starts_with('[') {
                class.clone()
            } else {
                format!("L{class};")
            };
            frame
                .stack
                .push(VType::Obj(unit.intern(&format!("[{element}"))));
        }
        Op::MultiANewArray { class, dimensions } => {
            for _ in 0..*dimensions {
                frame.pop_prim(Kind::I, pc)?;
            }
            frame.stack.push(VType::Obj(unit.intern(class)));
        }
        Op::ArrayLength => {
            let array = frame.pop(pc)?;
            if !matches!(array, VType::Null)
                && !matches!(array, VType::Obj(id) if unit.with_name(id, |n| n.starts_with('[')))
            {
                bail!("pc {pc}: arraylength of {}", cx.describe(array));
            }
            frame.stack.push(VType::I);
        }
        Op::ArrayLoad(family) | Op::ArrayStore(family) => {
            let storing = matches!(insn.op, Op::ArrayStore(_));
            if storing {
                match *family {
                    'A' => {
                        frame.pop_ref(cx, OBJECT, pc)?;
                    }
                    'B' | 'C' | 'S' => frame.pop_prim(Kind::I, pc)?,
                    f => frame.pop_prim(kind_of(f).unwrap(), pc)?,
                }
            }
            frame.pop_prim(Kind::I, pc)?;
            let array = frame.pop(pc)?;
            let mut elem_to_intern: Option<String> = None;
            let element = match array {
                VType::Null => None,
                VType::Obj(id) => unit.with_name(id, |name| {
                    if !name.starts_with('[') {
                        bail!("pc {pc}: array access on {name}");
                    }
                    let e = name.as_bytes()[1] as char;
                    let fits = match *family {
                        'A' => matches!(e, 'L' | '['),
                        'B' => matches!(e, 'B' | 'Z'),
                        f => f == e,
                    };
                    if !fits {
                        bail!("pc {pc}: element access of kind {family} on {name}");
                    }
                    if !storing && *family == 'A' {
                        elem_to_intern = element_class(name);
                    }
                    Ok(Some(e))
                })?,
                other => bail!("pc {pc}: array access on {}", cx.describe(other)),
            };
            if !storing {
                let pushed = match (*family, &array) {
                    ('A', VType::Null) => VType::Null,
                    ('A', VType::Obj(_)) => VType::Obj(unit.intern(&elem_to_intern.unwrap())),
                    ('B' | 'C' | 'S', _) => VType::I,
                    (f, _) => VType::prim(kind_of(f).unwrap()),
                };
                frame.stack.push(pushed);
            }
            out.resolved = Some(Resolved::Element(element));
        }
        Op::CheckCast { class } | Op::InstanceOf { class } => {
            if class.starts_with('[') {
                // Array types: checked at runtime against the array's own descriptor.
                parse_type(class, "cast")?;
                frame.pop_ref(cx, OBJECT, pc)?;
                let result = if matches!(insn.op, Op::CheckCast { .. }) {
                    VType::Obj(unit.intern(class))
                } else {
                    VType::I
                };
                frame.stack.push(result);
                return Ok(out);
            }
            if unit.absent(class) {
                frame.pop_ref(cx, OBJECT, pc)?;
                frame
                    .stack
                    .push(if matches!(insn.op, Op::CheckCast { .. }) {
                        VType::Obj(unit.intern(class))
                    } else {
                        VType::I
                    });
                out.resolved = Some(Resolved::Absent(class.clone()));
                return Ok(out);
            }
            if unit.get(class).is_none() {
                // The class exists somewhere JABS did not compile (another JDK module, another
                // jar): like a member access on it, the test stops only if it executes, so code
                // that never takes this path (Netty's probes) still compiles. A null value
                // resolves nothing, as in HotSpot.
                frame.pop_ref(cx, OBJECT, pc)?;
                frame
                    .stack
                    .push(if matches!(insn.op, Op::CheckCast { .. }) {
                        VType::Obj(unit.intern(class))
                    } else {
                        VType::I
                    });
                out.resolved = Some(Resolved::UnresolvedType(format!(
                    "type test against {class}: {class} is not in the compilation unit"
                )));
                return Ok(out);
            }
            if unit.chain(class).is_err() {
                bail!(
                    "pc {pc}: type test against {class} (outside the unit or an interface) is not compiled yet"
                );
            }
            frame.pop_ref(cx, OBJECT, pc)?;
            let result = if matches!(insn.op, Op::CheckCast { .. }) {
                VType::Obj(unit.intern(class))
            } else {
                VType::I
            };
            frame.stack.push(result);
        }
        Op::GetField(m) | Op::PutField(m) => {
            let (declaring, f) = unit.resolve_field(&m.class, &m.name, &m.descriptor)?;
            if f.access & ACC_STATIC != 0 || unit.is_interface(&declaring.name) {
                bail!("pc {pc}: instance access to static field {}", m.name);
            }
            let ty = parse_type(&m.descriptor, &m.name)?;
            if matches!(insn.op, Op::GetField(_)) {
                frame.pop_ref(cx, &declaring.name, pc)?;
                frame.stack.push(cx.of_ty(&ty));
            } else {
                frame.pop_ty(cx, &ty, pc)?;
                // A constructor may assign its own class's fields before calling super().
                match frame.pop(pc)? {
                    VType::UninitThis if cx.is_constructor && declaring.name == *this => {}
                    v if cx.assignable(v, &declaring.name) => {}
                    v => bail!("pc {pc}: putfield {} on {}", m.name, cx.describe(v)),
                }
            }
            out.resolved = Some(Resolved::Field(declaring.name.clone()));
        }
        // Methods on arrays are Object's (clone is the array copy); arrays have no vtable, so call directly.
        Op::InvokeVirtual(m) if m.class.starts_with('[') => {
            let (declaring, target) = unit.resolve_method(OBJECT, &m.name, &m.descriptor)?;
            if target.access & ACC_STATIC != 0 || m.name.starts_with('<') {
                bail!("pc {pc}: invokevirtual of {} on an array", m.name);
            }
            let s = unit.signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, &m.class, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            out.deps.push(Dep::Method(key.clone()));
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
        Op::InvokeVirtual(m) if m.class == VAR_HANDLE && var_handle_mode(&m.name).is_some() => {
            // Signature polymorphic: the call site's descriptor is the access's type.
            let s = unit.signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, VAR_HANDLE, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            out.resolved = Some(Resolved::VarHandle(m.name.clone()));
            return Ok(out);
        }
        Op::InvokeVirtual(m) => {
            if m.name.starts_with('<') {
                bail!("pc {pc}: invokevirtual of {}", m.name);
            }
            let (declaring, target) = match unit.resolve_method(&m.class, &m.name, &m.descriptor) {
                Ok(found) => found,
                // A default method inherited from a superinterface: dispatch like invokeinterface.
                Err(e) => {
                    let found = unit.superinterfaces_arc(&m.class).ok().and_then(|ifaces| {
                        ifaces.iter().copied().find_map(|i| {
                            i.methods
                                .iter()
                                .find(|x| {
                                    x.name == m.name
                                        && x.descriptor == m.descriptor
                                        && x.access & ACC_STATIC == 0
                                })
                                .map(|x| (i, x))
                        })
                    });
                    let Some((iface, _)) = found else {
                        return Err(e);
                    };
                    let s = unit.signature(&m.descriptor)?;
                    for p in s.params.iter().rev() {
                        frame.pop_ty(cx, p, pc)?;
                    }
                    frame.pop_ref(cx, &m.class, pc)?;
                    if s.ret.code != 'V' {
                        frame.stack.push(cx.of_ty(&s.ret));
                    }
                    let target = (iface.name.clone(), m.name.clone(), m.descriptor.clone());
                    out.resolved = Some(Resolved::Call(CallKind::Interface {
                        selector: unit.selector(&m.name, &m.descriptor),
                        target,
                    }));
                    return Ok(out);
                }
            };
            if target.access & ACC_STATIC != 0 {
                bail!("pc {pc}: invokevirtual of static method {}", m.name);
            }
            let s = unit.signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, &m.class, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            // Private, final and final-class targets cannot be overridden: call them directly.
            let fixed =
                target.access & (ACC_PRIVATE | ACC_FINAL) != 0 || declaring.access & ACC_FINAL != 0;
            if fixed {
                out.deps.push(Dep::Method(key.clone()));
                out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
            } else {
                let vtable = unit.vtable(&m.class)?;
                let slot = vtable
                    .iter()
                    .position(|s| s.name == m.name && s.descriptor == m.descriptor)
                    .ok_or_else(|| anyhow!("pc {pc}: {} has no vtable slot", m.name))?;
                // No dependency on implementations: a slot whose implementation is not compiled
                // holds a stub (see `emit_stubs`), so subclasses never change this decision.
                out.resolved = Some(Resolved::Call(CallKind::Virtual { slot, target: key }));
            }
        }
        Op::MonitorEnter | Op::MonitorExit => {
            frame.pop_ref(cx, OBJECT, pc)?;
        }
        Op::AThrow => {
            if unit.get(THROWABLE).is_none() {
                bail!(
                    "pc {pc}: throw needs java/lang/Throwable in the unit (add the JDK's java.base)"
                );
            }
            frame.pop_ref(cx, THROWABLE, pc)?;
            out.falls = false;
        }
        Op::InvokeInterface(m) => {
            if !unit.is_interface(&m.class) {
                bail!(
                    "pc {pc}: invokeinterface on {}, which is not an interface in the unit",
                    m.class
                );
            }
            let resolved = unit.resolve_interface_method(&m.class, &m.name, &m.descriptor);
            // JVMS 5.4.3.4: otherwise a public instance method of Object (getClass, hashCode...).
            let object_method = resolved.is_err().then(|| {
                unit.get(OBJECT).and_then(|o| {
                    o.methods.iter().find(|x| {
                        x.name == m.name
                            && x.descriptor == m.descriptor
                            && x.access & 0x0001 != 0 // ACC_PUBLIC
                            && x.access & ACC_STATIC == 0
                    })
                })
            });
            let s = unit.signature(&m.descriptor)?;
            if let Some(Some(target)) = object_method {
                for p in s.params.iter().rev() {
                    frame.pop_ty(cx, p, pc)?;
                }
                frame.pop_ref(cx, &m.class, pc)?;
                if s.ret.code != 'V' {
                    frame.stack.push(cx.of_ty(&s.ret));
                }
                let key = (OBJECT.to_string(), m.name.clone(), m.descriptor.clone());
                if target.access & ACC_FINAL != 0 {
                    out.deps.push(Dep::Method(key.clone()));
                    out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
                } else {
                    let slot = unit
                        .vtable(OBJECT)?
                        .iter()
                        .position(|x| x.name == m.name && x.descriptor == m.descriptor)
                        .ok_or_else(|| anyhow!("pc {pc}: Object.{} has no vtable slot", m.name))?;
                    out.resolved = Some(Resolved::Call(CallKind::Virtual { slot, target: key }));
                }
                return Ok(out);
            }
            let (declaring, _) = resolved?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, &m.class, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            // No dependency on implementations: uncompiled ones are stubs in the receiver's table.
            let target = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            out.resolved = Some(Resolved::Call(CallKind::Interface {
                selector: unit.selector(&m.name, &m.descriptor),
                target,
            }));
        }
        Op::InvokeSpecial(m) if unit.is_interface(&m.class) && m.name != "<init>" => {
            // Interface.super.m() or a private interface method: a direct call to that body.
            let (declaring, target) =
                unit.resolve_interface_method(&m.class, &m.name, &m.descriptor)?;
            if target.access & ACC_ABSTRACT != 0 {
                bail!(
                    "pc {pc}: invokespecial of abstract interface method {}",
                    m.name
                );
            }
            let s = unit.signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            frame.pop_ref(cx, this, pc)?;
            if s.ret.code != 'V' {
                frame.stack.push(cx.of_ty(&s.ret));
            }
            let key = (declaring.name.clone(), m.name.clone(), m.descriptor.clone());
            out.deps.push(Dep::Method(key.clone()));
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
        Op::InvokeSpecial(m) => {
            let parent = cx.class.super_name.as_deref().unwrap_or(OBJECT);
            if m.name == "<init>" && m.class == OBJECT && m.descriptor == "()V" {
                // Either `super()` in a direct subclass of Object, or `new Object()`.
                match frame.pop(pc)? {
                    VType::UninitThis if parent == OBJECT => {
                        frame.initialize(VType::UninitThis, VType::Obj(unit.intern(this)));
                    }
                    v @ VType::Uninit(at) if news.get(&at).copied() == Some(OBJECT) => {
                        frame.initialize(v, VType::Obj(unit.intern(OBJECT)));
                    }
                    other => bail!("pc {pc}: Object.<init> called on {}", cx.describe(other)),
                }
                out.resolved = Some(Resolved::Call(CallKind::ObjectInit));
                return Ok(out);
            }
            let s = unit.signature(&m.descriptor)?;
            for p in s.params.iter().rev() {
                frame.pop_ty(cx, p, pc)?;
            }
            let key = if m.name == "<init>" {
                let target = unit.get(&m.class).ok_or_else(|| {
                    anyhow!("pc {pc}: constructor of {} outside the unit", m.class)
                })?;
                if !target
                    .methods
                    .iter()
                    .any(|x| x.name == "<init>" && x.descriptor == m.descriptor)
                {
                    bail!("pc {pc}: no constructor {}{}", m.class, m.descriptor);
                }
                match frame.pop(pc)? {
                    // The constructor must belong to exactly the class that `new` created.
                    v @ VType::Uninit(at) if news.get(&at).copied() == Some(m.class.as_str()) => {
                        frame.initialize(v, VType::Obj(unit.intern(&m.class)));
                    }
                    VType::UninitThis
                        if cx.is_constructor && (m.class == *this || m.class == parent) =>
                    {
                        frame.initialize(VType::UninitThis, VType::Obj(unit.intern(this)));
                    }
                    other => bail!(
                        "pc {pc}: constructor of {} called on {}",
                        m.class,
                        cx.describe(other)
                    ),
                }
                (m.class.clone(), m.name.clone(), m.descriptor.clone())
            } else {
                // Private methods (pre-nestmate style) and super.m() calls: no dynamic dispatch.
                let start = if m.class == *this {
                    this.as_str()
                } else {
                    parent
                };
                let (declaring, target) = unit.resolve_method(start, &m.name, &m.descriptor)?;
                if target.access & ACC_STATIC != 0 {
                    bail!("pc {pc}: invokespecial of static method {}", m.name);
                }
                frame.pop_ref(cx, this, pc)?;
                if s.ret.code != 'V' {
                    frame.stack.push(cx.of_ty(&s.ret));
                }
                (declaring.name.clone(), m.name.clone(), m.descriptor.clone())
            };
            out.deps.push(Dep::Method(key.clone()));
            out.resolved = Some(Resolved::Call(CallKind::Direct(key)));
        }
    }
    Ok(out)
}

/// One exception handler, verified: catch class (None catches everything) and handler pc.
#[derive(Debug, Clone, PartialEq)]
struct Catch {
    class: Option<String>,
    handler: u32,
}

impl Analyzed {
    /// Placeholder analysis for methods whose body the runtime supplies (intrinsics).
    fn empty() -> Analyzed {
        Analyzed {
            exits: HashMap::new(),
            dispatch: Vec::new(),
            insns: Vec::new(),
            stacks: Vec::new(),
            targets: BTreeSet::new(),
            deps: BTreeSet::new(),
            resolved: HashMap::new(),
        }
    }
}

struct Analyzed {
    /// pc -> index into `dispatch` for instructions inside try ranges.
    exits: HashMap<u32, usize>,
    /// Handler chains in table order; each becomes a dispatch block in the C function.
    dispatch: Vec<Vec<Catch>>,
    insns: Vec<Insn>,
    /// Operand stack types before each instruction.
    stacks: Vec<Vec<VType>>,
    targets: BTreeSet<u32>,
    deps: BTreeSet<Dep>,
    resolved: HashMap<u32, Resolved>,
}

/// Decode a method and verify every reachable path, or explain why it cannot be compiled.
fn analyze<'a>(
    unit: &Unit<'a>,
    class: &'a ClassFile,
    method: &Member,
    sig: &Signature,
) -> std::result::Result<Analyzed, String> {
    let code = method
        .code
        .as_ref()
        .ok_or("no bytecode (abstract or native)")?;

    let insns = match bytecode::decode(class, &code.bytecode) {
        Ok(insns) => insns,
        Err(DecodeError::Unsupported(u)) => return Err(u.to_string()),
        Err(DecodeError::Malformed(e)) => return Err(format!("malformed bytecode: {e:#}")),
    };
    // Which class each `new` creates, so constructor calls can be matched to it.
    let news: HashMap<u32, &str> = insns
        .iter()
        .filter_map(|i| match &i.op {
            Op::New { class } => Some((i.pc, class.as_str())),
            _ => None,
        })
        .collect();
    let is_constructor = method.name == "<init>";
    let cx = Cx {
        unit,
        class,
        sig,
        is_constructor,
    };
    let mut entry = Frame {
        stack: Vec::new(),
        locals: vec![VType::Top; code.max_locals as usize],
    };
    let mut slot = 0u16;
    let mut place = |v: VType, frame: &mut Frame| -> std::result::Result<(), String> {
        frame
            .set_local(slot, v, 0)
            .map_err(|_| "parameters exceed max_locals".to_string())?;
        slot += v.category();
        Ok(())
    };
    if method.access & ACC_STATIC == 0 {
        let this = if is_constructor {
            VType::UninitThis
        } else {
            VType::Obj(unit.intern(&class.name))
        };
        place(this, &mut entry)?;
    }
    for p in &sig.params {
        place(cx.of_ty(p), &mut entry)?;
    }

    let index: HashMap<u32, usize> = insns
        .iter()
        .enumerate()
        .map(|(i, insn)| (insn.pc, i))
        .collect();
    // Exception handlers: (start, end, catch, interned thrown id). Catch classes must be Throwables in the unit.
    let mut handlers: Vec<(u32, u32, Catch, u32)> = Vec::new();
    for h in &code.handlers {
        let class = if h.catch_type == 0 {
            None
        } else {
            let name = class
                .class_name(h.catch_type)
                .map_err(|e| format!("{e:#}"))?
                .to_string();
            if unit.get(&name).is_none() || unit.is_subclass(&name, THROWABLE) != Some(true) {
                return Err(format!(
                    "catch type {name} is outside the unit or not a Throwable"
                ));
            }
            Some(name)
        };
        if !index.contains_key(&(h.handler_pc as u32)) {
            return Err(format!("handler pc {} is not an instruction", h.handler_pc));
        }
        let thrown_id = unit.intern(class.as_deref().unwrap_or(THROWABLE));
        handlers.push((
            h.start_pc as u32,
            h.end_pc as u32,
            Catch {
                class,
                handler: h.handler_pc as u32,
            },
            thrown_id,
        ));
    }
    let mut frames: Vec<Option<Frame>> = vec![None; insns.len()];
    let mut targets = BTreeSet::new();
    let mut deps = BTreeSet::new();
    let mut resolved = HashMap::new();
    let mut work = vec![(0usize, entry)];
    while let Some((i, incoming)) = work.pop() {
        let pc = insns[i].pc;
        let frame = match &frames[i] {
            None => incoming,
            Some(existing) => {
                let merged = cx
                    .merge_frames(existing, &incoming, pc)
                    .map_err(|e| format!("{e:#}"))?;
                if merged == *existing {
                    continue;
                }
                merged
            }
        };
        let depth: u32 = frame.stack.iter().map(|v| v.category() as u32).sum();
        if depth > code.max_stack as u32 {
            return Err(format!("pc {pc}: stack exceeds max_stack"));
        }
        frames[i] = Some(frame.clone());
        // Every instruction in a try range may transfer to its handlers with its incoming locals
        // and only the exception on the stack.
        let mut checked_uninit = false;
        for (start, end, catch, thrown_id) in &handlers {
            if *start <= pc && pc < *end {
                if !checked_uninit {
                    if frame
                        .locals
                        .iter()
                        .any(|v| matches!(v, VType::UninitThis | VType::Uninit(_)))
                    {
                        return Err(format!(
                            "pc {pc}: try range covers an object before its constructor ran"
                        ));
                    }
                    checked_uninit = true;
                }
                targets.insert(catch.handler);
                let h_idx = index[&catch.handler];
                if let Some(existing) = &frames[h_idx]
                    && existing.stack.len() == 1
                    && existing.stack[0] == VType::Obj(*thrown_id)
                    && existing
                        .locals
                        .iter()
                        .zip(&frame.locals)
                        .all(|(e, l)| *e == *l || *e == VType::Top)
                {
                    continue;
                }
                let entry = Frame {
                    stack: vec![VType::Obj(*thrown_id)],
                    locals: frame.locals.clone(),
                };
                work.push((h_idx, entry));
            }
        }
        let mut next = frame;
        let step = step(&insns[i], &mut next, &cx, &news).map_err(|e| format!("{e:#}"))?;
        deps.extend(step.deps);
        if let Some(r) = step.resolved {
            resolved.insert(pc, r);
        }
        for t in step.targets {
            let Some(&j) = index.get(&t) else {
                return Err(format!("pc {pc}: branch target {t} is not an instruction"));
            };
            targets.insert(t);
            work.push((j, next.clone()));
        }
        if step.falls {
            if i + 1 >= insns.len() {
                return Err(format!(
                    "pc {pc}: execution falls off the end of the method"
                ));
            }
            work.push((i + 1, next));
        }
    }
    // Unreachable instructions are never emitted.
    let (insns, stacks): (Vec<_>, Vec<_>) = insns
        .into_iter()
        .zip(frames)
        .filter_map(|(insn, f)| f.map(|f| (insn, f.stack)))
        .unzip();
    // Exceptional exits: instructions in try ranges jump to a dispatch block for their handler chain.
    let mut exits = HashMap::new();
    let mut dispatch: Vec<Vec<Catch>> = Vec::new();
    for insn in &insns {
        let chain: Vec<Catch> = handlers
            .iter()
            .filter(|(start, end, _, _)| *start <= insn.pc && insn.pc < *end)
            .map(|(_, _, c, _)| c.clone())
            .collect();
        if chain.is_empty() {
            continue;
        }
        let id = dispatch
            .iter()
            .position(|c| *c == chain)
            .unwrap_or_else(|| {
                dispatch.push(chain);
                dispatch.len() - 1
            });
        exits.insert(insn.pc, id);
    }
    Ok(Analyzed {
        exits,
        dispatch,
        insns,
        stacks,
        targets,
        deps,
        resolved,
    })
}

/// Exact C literal for a constant (floats via bit patterns so NaN/-0.0 survive).
fn value_literal(v: Value) -> String {
    match v {
        Value::I(x) => format!("(int32_t)0x{:08x}u", x as u32),
        Value::J(x) => format!("(int64_t)UINT64_C(0x{:016x})", x as u64),
        Value::F(bits) => format!("jabs_fbits(0x{bits:08x}u)"),
        Value::D(bits) => format!("jabs_dbits(UINT64_C(0x{bits:016x}))"),
    }
}

/// C comparison operator for a branch condition.
fn cond_op(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "==",
        Cond::Ne => "!=",
        Cond::Lt => "<",
        Cond::Ge => ">=",
        Cond::Gt => ">",
        Cond::Le => "<=",
    }
}

/// C parameter list for a method: `self` first for instance methods.
fn c_params(is_static: bool, sig: &Signature, names: bool) -> String {
    let mut params = Vec::new();
    if !is_static {
        params.push(if names {
            "jabs_object *self".to_string()
        } else {
            "jabs_object *".to_string()
        });
    }
    for (i, p) in sig.params.iter().enumerate() {
        params.push(if names {
            format!("{} a{i}", c_type(p.code))
        } else {
            c_type(p.code).to_string()
        });
    }
    if params.is_empty() {
        "void".into()
    } else {
        params.join(", ")
    }
}

/// C function prototype for a compiled method.
fn prototype(class: &str, method: &Member, sig: &Signature) -> String {
    format!(
        "JABS_METHOD {} {}({})",
        c_type(sig.ret.code),
        symbol(class, &method.name, &method.descriptor),
        c_params(method.access & ACC_STATIC != 0, sig, true)
    )
}

const VAR_HANDLE: &str = "java/lang/invoke/VarHandle";

/// What a VarHandle access mode does, and how many values it takes after the coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
enum VarHandleOp {
    Get,
    Set,
    CompareAndSet,
    CompareAndExchange,
    GetAndSet,
    GetAndAdd,
    GetAndBitwise(char),
}

/// The C11 memory orders of an access mode, as (order, failure order of a compare-and-set), or
/// None for the plain `get` and `set`, which compile to ordinary loads and stores. The bare
/// atomic names (compareAndSet, getAndAdd, ...) are volatile, as their Javadoc says.
fn var_handle_order(name: &str) -> Option<(&'static str, &'static str)> {
    if matches!(name, "get" | "set") {
        return None;
    }
    Some(if name.ends_with("Acquire") {
        ("__ATOMIC_ACQUIRE", "__ATOMIC_ACQUIRE")
    } else if name.ends_with("Release") {
        ("__ATOMIC_RELEASE", "__ATOMIC_RELAXED")
    } else if name.ends_with("Opaque") || name.ends_with("Plain") {
        ("__ATOMIC_RELAXED", "__ATOMIC_RELAXED")
    } else {
        ("__ATOMIC_SEQ_CST", "__ATOMIC_SEQ_CST")
    })
}

/// The access mode named `name`: what it does and how many values it takes.
fn var_handle_mode(name: &str) -> Option<(VarHandleOp, usize)> {
    let base = name
        .strip_suffix("Acquire")
        .or_else(|| name.strip_suffix("Release"))
        .or_else(|| name.strip_suffix("Volatile"))
        .or_else(|| name.strip_suffix("Opaque"))
        .or_else(|| name.strip_suffix("Plain"))
        .unwrap_or(name);
    Some(match base {
        "get" => (VarHandleOp::Get, 0),
        "set" => (VarHandleOp::Set, 1),
        "compareAndSet" | "weakCompareAndSet" => (VarHandleOp::CompareAndSet, 2),
        "compareAndExchange" => (VarHandleOp::CompareAndExchange, 2),
        "getAndSet" => (VarHandleOp::GetAndSet, 1),
        "getAndAdd" => (VarHandleOp::GetAndAdd, 1),
        "getAndBitwiseOr" => (VarHandleOp::GetAndBitwise('|'), 1),
        "getAndBitwiseAnd" => (VarHandleOp::GetAndBitwise('&'), 1),
        "getAndBitwiseXor" => (VarHandleOp::GetAndBitwise('^'), 1),
        _ => return None,
    })
}

/// C for one VarHandle access at a call site. The coordinates (none: static field; receiver:
/// instance field; array and index: element) come from the descriptor; the handle's own field
/// type must equal the call site's value type, otherwise the access stops with a diagnostic
/// (JABS does not apply VarHandle's asType conversions yet).
fn var_handle_access(mode: &str, descriptor: &str, d: usize, exit: &str, npe: &str) -> String {
    let (op, values) = var_handle_mode(mode).expect("resolved as a VarHandle mode");
    let sig = parse_signature(descriptor).unwrap();
    let base = d - sig.params.len();
    let handle = base - 1;
    let stop = |why: &str| {
        format!(
            "jabs_not_compiled(\"VarHandle.{mode}{} ({why})\");",
            c_escape(descriptor)
        )
    };
    let Some(coordinates) = sig.params.len().checked_sub(values).filter(|c| *c <= 2) else {
        return stop("unexpected argument count");
    };
    let value_ty = if values > 0 {
        sig.params[coordinates].code
    } else {
        sig.ret.code
    };
    let ret = sig.ret.code;
    let ret_ok = match op {
        VarHandleOp::Set => ret == 'V',
        VarHandleOp::CompareAndSet => ret == 'Z',
        _ => ret == value_ty || ret == 'V',
    };
    let coordinate_ok = match coordinates {
        1 => sig.params[0].code == 'L',
        2 => sig.params[0].code == 'L' && sig.params[1].code == 'I',
        _ => true,
    };
    if !ret_ok
        || !coordinate_ok
        || value_ty == 'V'
        || sig.params[coordinates..].iter().any(|p| p.code != value_ty)
    {
        return stop("call-site types JABS does not convert yet");
    }
    let c = match value_ty {
        'Z' => "uint8_t",
        'B' => "int8_t",
        'C' => "uint16_t",
        'S' => "int16_t",
        'I' => "int32_t",
        'J' => "int64_t",
        'F' => "float",
        'D' => "double",
        _ => "jabs_object *",
    };
    let slot = field(kind_of(value_ty).unwrap());
    let kind = if value_ty == 'L' { 'L' } else { value_ty };
    let value = |i: usize| {
        let v = format!("s[{}].{slot}", base + coordinates + i);
        if value_ty == 'Z' {
            format!("(uint8_t)({v} & 1)")
        } else {
            format!("({c}){v}")
        }
    };
    let object = if coordinates >= 1 {
        format!("s[{base}].a")
    } else {
        "NULL".into()
    };
    let index = if coordinates == 2 {
        format!("s[{}].i", base + 1)
    } else {
        "0".into()
    };
    let store_check = |v: &str| {
        if coordinates == 2 && value_ty == 'L' {
            format!(
                "if (!jabs_array_store_ok((jabs_array *){object}, {v})) {{ jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_STORE); {exit} }} "
            )
        } else {
            String::new()
        }
    };
    let result = |expr: &str| {
        if ret == 'V' {
            format!("(void)({expr});")
        } else {
            format!("s[{handle}].{} = {expr};", field(kind_of(ret).unwrap()))
        }
    };
    // Every mode but plain get/set is an atomic access with its memory order. Reference-array
    // stores are type-checked before the access, compare-and-set included, as the JDK's
    // VarHandleReferences.Array does. Compare-and-set compares bytes, which is VarHandle's
    // bitwise equality for float and double; the weak forms use the strong one (allowed: a weak
    // compare-and-set may fail spuriously, it need not).
    let (order, failure) = var_handle_order(mode).unwrap_or(("", ""));
    let body = match (op, order.is_empty()) {
        (VarHandleOp::Get, true) => result("*p"),
        (VarHandleOp::Set, true) => format!("{c} v = {}; {}*p = v;", value(0), store_check("v")),
        (VarHandleOp::Get, false) => {
            format!("{c} v; __atomic_load(p, &v, {order}); {}", result("v"))
        }
        (VarHandleOp::Set, false) => format!(
            "{c} v = {}; {}__atomic_store(p, &v, {order});",
            value(0),
            store_check("v")
        ),
        (VarHandleOp::CompareAndSet, false) => format!(
            "{c} e = {}; {c} n = {}; {}int32_t r = __atomic_compare_exchange(p, &e, &n, 0, {order}, {failure}); {}",
            value(0),
            value(1),
            store_check("n"),
            result("r")
        ),
        (VarHandleOp::CompareAndExchange, false) => format!(
            "{c} e = {}; {c} n = {}; {}(void)__atomic_compare_exchange(p, &e, &n, 0, {order}, {failure}); {}",
            value(0),
            value(1),
            store_check("n"),
            result("e")
        ),
        (VarHandleOp::GetAndSet, false) => format!(
            "{c} v = {}; {c} old; {}__atomic_exchange(p, &v, &old, {order}); {}",
            value(0),
            store_check("v"),
            result("old")
        ),
        (VarHandleOp::GetAndAdd, false) if value_ty == 'F' => format!(
            "{c} old = jabs_atomic_add_float(p, {}, {order}); {}",
            value(0),
            result("old")
        ),
        (VarHandleOp::GetAndAdd, false) if value_ty == 'D' => format!(
            "{c} old = jabs_atomic_add_double(p, {}, {order}); {}",
            value(0),
            result("old")
        ),
        (VarHandleOp::GetAndAdd, false) if !matches!(value_ty, 'L' | 'Z') => format!(
            "{c} old = __atomic_fetch_add(p, {}, {order}); {}",
            value(0),
            result("old")
        ),
        (VarHandleOp::GetAndBitwise(bit), false) if !matches!(value_ty, 'L' | 'F' | 'D') => {
            let builtin = match bit {
                '|' => "__atomic_fetch_or",
                '&' => "__atomic_fetch_and",
                _ => "__atomic_fetch_xor",
            };
            format!(
                "{c} old = {builtin}(p, {}, {order}); {}",
                value(0),
                result("old")
            )
        }
        _ => return stop("an access mode this type does not support"),
    };
    let view = byte_view_access(mode, op, value_ty, coordinates, base, handle, exit)
        .unwrap_or_else(|| stop("only plain get and set reach a byte-array view handle"));
    format!(
        "{{ if (!s[{handle}].a) {npe} const jabs_vh *h = jabs_vh_of(s[{handle}].a); if (h && h->shape == JABS_VH_BYTE_VIEW) {{ {view} }} else {{ if (!h || h->shape != {coordinates} || h->kind != '{kind}') {} {c} *p = ({c} *)jabs_vh_address(h, {object}, {index}); if (!p) {exit} {body} }} }}",
        stop("a handle whose shape or type differs from the call site")
    )
}

/// The C for a plain get or set through a byte-array view handle (`JABS_VH_BYTE_VIEW`), or None
/// when this call site cannot reach one that way: views have an array and an index as
/// coordinates and a primitive wider than a byte as value. Accesses with a memory order
/// (Volatile, Acquire, ...) and atomic updates need the JVM's alignment check and are not
/// compiled yet.
#[allow(clippy::too_many_arguments)]
fn byte_view_access(
    mode: &str,
    op: VarHandleOp,
    value_ty: char,
    coordinates: usize,
    base: usize,
    handle: usize,
    exit: &str,
) -> Option<String> {
    if coordinates != 2 || !matches!(mode, "get" | "set") {
        return None;
    }
    // Raw bits to the Java value (sign- or zero-extended as the type requires), and back.
    let (from_bits, to_bits) = match value_ty {
        'C' => ("(int32_t)(uint16_t)bits", "(uint64_t)(uint16_t)v"),
        'S' => ("(int32_t)(int16_t)(uint16_t)bits", "(uint64_t)(uint16_t)v"),
        'I' => ("(int32_t)(uint32_t)bits", "(uint64_t)(uint32_t)v"),
        'J' => ("(int64_t)bits", "(uint64_t)v"),
        'F' => (
            "jabs_fbits((uint32_t)bits)",
            "(uint64_t)jabs_float_raw_bits(v)",
        ),
        'D' => ("jabs_dbits(bits)", "jabs_double_raw_bits(v)"),
        _ => return None,
    };
    let mismatch = format!(
        "jabs_not_compiled(\"VarHandle.{mode} (a byte-array view of another type than the call site's)\");"
    );
    let address = format!(
        "if (h->kind != '{value_ty}') {mismatch} uint8_t *p = (uint8_t *)jabs_vh_address(h, s[{base}].a, s[{}].i); if (!p) {exit}",
        base + 1
    );
    let slot = field(kind_of(value_ty)?);
    match op {
        VarHandleOp::Get => Some(format!(
            "{address} uint64_t bits = jabs_vh_view_load(h, p); s[{handle}].{slot} = {from_bits};"
        )),
        VarHandleOp::Set => {
            let c = match value_ty {
                'C' => "uint16_t",
                'S' => "int16_t",
                'I' => "int32_t",
                'J' => "int64_t",
                'F' => "float",
                _ => "double",
            };
            Some(format!(
                "{c} v = ({c})s[{}].{slot}; {address} jabs_vh_view_store(h, p, {to_bits});",
                base + 2
            ))
        }
        _ => None,
    }
}

/// Whether a call targets a `@CallerSensitive` method: those ask `Reflection.getCallerClass`,
/// which JABS answers from `jabs_caller`, set at the call site (the caller is known statically).
fn caller_sensitive(unit: &Unit, op: &Op) -> bool {
    let (class, name, descriptor) = match op {
        Op::InvokeStatic {
            class,
            name,
            descriptor,
            ..
        } => (class, name, descriptor),
        Op::InvokeVirtual(m) | Op::InvokeSpecial(m) | Op::InvokeInterface(m) => {
            (&m.class, &m.name, &m.descriptor)
        }
        _ => return false,
    };
    if !unit
        .caller_sensitive_sigs
        .contains(&(name.as_str(), descriptor.as_str()))
    {
        return false;
    }
    // getCallerClass is itself @CallerSensitive, but it reads the caller; it must not replace it.
    if class == "jdk/internal/reflect/Reflection" && name == "getCallerClass" {
        return false;
    }
    let declared = |c: &ClassFile| {
        c.methods
            .iter()
            .find(|m| &m.name == name && &m.descriptor == descriptor)
            .map(|_| {
                unit.caller_sensitive.contains(&(
                    c.name.as_str(),
                    name.as_str(),
                    descriptor.as_str(),
                ))
            })
    };
    if let Some(found) = unit.get(class).and_then(declared) {
        return found;
    }
    unit.chain(class)
        .ok()
        .and_then(|chain| chain.iter().find_map(|c| declared(c)))
        .unwrap_or(false)
}

/// The forms of dup_x2, dup2_x1 and dup2_x2 (JVMS 6.5). `categories` are the top stack entries
/// computational categories, top first (a long or double is one entry of category 2). Returns how
/// many entries the instruction takes and the new order of those entries (indexes, deepest first).
fn stack_shuffle(op: &Op, categories: &[u16]) -> Option<(usize, Vec<usize>)> {
    let c = |i: usize| categories.get(i).copied();
    Some(match op {
        // ..., v3, v2, v1 -> ..., v1, v3, v2, v1  |  ..., v2(long), v1 -> ..., v1, v2, v1
        Op::DupX2 => match (c(0), c(1), c(2)) {
            (Some(1), Some(1), Some(1)) => (3, vec![2, 0, 1, 2]),
            (Some(1), Some(2), _) => (2, vec![1, 0, 1]),
            _ => return None,
        },
        // ..., v3, v2, v1 -> ..., v2, v1, v3, v2, v1  |  ..., v2, v1(long) -> ..., v1, v2, v1
        Op::Dup2X1 => match (c(0), c(1), c(2)) {
            (Some(1), Some(1), Some(1)) => (3, vec![1, 2, 0, 1, 2]),
            (Some(2), Some(1), _) => (2, vec![1, 0, 1]),
            _ => return None,
        },
        Op::Dup2X2 => match (c(0), c(1), c(2), c(3)) {
            // ..., v4, v3, v2, v1 -> ..., v2, v1, v4, v3, v2, v1
            (Some(1), Some(1), Some(1), Some(1)) => (4, vec![2, 3, 0, 1, 2, 3]),
            // ..., v3, v2, v1(long) -> ..., v1, v3, v2, v1
            (Some(2), Some(1), Some(1), _) => (3, vec![2, 0, 1, 2]),
            // ..., v3(long), v2, v1 -> ..., v2, v1, v3, v2, v1
            (Some(1), Some(1), Some(2), _) => (3, vec![1, 2, 0, 1, 2]),
            // ..., v2(long), v1(long) -> ..., v1, v2, v1
            (Some(2), Some(2), _, _) => (2, vec![1, 0, 1]),
            _ => return None,
        },
        _ => return None,
    })
}

/// `if (init() != 0) return` guarding first use of another class; nothing when `target` is
/// necessarily initialized whenever this code runs (the current class or one of its superclasses).
fn init_guard(unit: &Unit, current: &str, target: &str, ret_default: &str) -> String {
    if !unit.is_interface(target) && unit.is_subclass(current, target) == Some(true) {
        String::new()
    } else {
        format!("if ({}() != 0) {ret_default} ", init_symbol(target))
    }
}

/// Emit one analyzed method as C: locals and stack become `jabs_value` arrays, pcs become labels.
fn emit_method(
    out: &mut String,
    unit: &Unit,
    class: &ClassFile,
    method: &Member,
    sig: &Signature,
    a: &Analyzed,
    literal_ids: &HashMap<Vec<u16>, u32>,
) {
    let code = method.code.as_ref().unwrap();
    let ret_default = if sig.ret.code == 'V' {
        "return;"
    } else {
        "return 0;"
    };
    let _ = writeln!(
        out,
        "/* {}.{}{} */",
        class.name, method.name, method.descriptor
    );
    let synchronized = method.access & ACC_SYNCHRONIZED != 0;
    if synchronized {
        // The body gets an always-inlined helper; the public symbol is the locking wrapper below,
        // so -O2 never leaves __sync un-inlined and missing from jabs_frames[].
        let body = format!(
            "{}__sync",
            symbol(&class.name, &method.name, &method.descriptor)
        );
        let params = c_params(method.access & ACC_STATIC != 0, sig, true);
        let _ = writeln!(
            out,
            "JABS_ALWAYS_INLINE {} {body}({params}) {{",
            c_type(sig.ret.code)
        );
    } else {
        let _ = writeln!(out, "{} {{", prototype(&class.name, method, sig));
    }
    let _ = writeln!(
        out,
        "    jabs_value l[{}] = {{{{0}}}};",
        code.max_locals.max(1)
    );
    let _ = writeln!(
        out,
        "    jabs_value s[{}] = {{{{0}}}};",
        code.max_stack.max(1)
    );
    let _ = writeln!(out, "    (void)l; (void)s;");
    let mut slot = 0usize;
    if method.access & ACC_STATIC == 0 {
        let _ = writeln!(out, "    l[0].a = self;");
        slot = 1;
    }
    for (i, p) in sig.params.iter().enumerate() {
        let k = kind_of(p.code).unwrap();
        let _ = writeln!(out, "    l[{slot}].{} = a{i};", field(k));
        slot += if matches!(k, Kind::J | Kind::D) { 2 } else { 1 };
    }
    let _ = writeln!(out, "    jabs_gc_safepoint();");
    // Raise a VM exception and leave through `exit` (a handler dispatch block or a return).
    // Raises record the method that raised them (stack traces are not captured yet).
    let site = c_escape(&format!(
        "{}.{}{}",
        class.name, method.name, method.descriptor
    ));
    let throw =
        |exc: &str, exit: &str| format!("{{ jabs_where = \"{site}\"; {} {exit} }}", raise(exc));
    // Emit a call: arguments (and the receiver, for instance methods) come off the stack.
    let call = |kind: &CallKind, descriptor: &str, d: usize, exit: &str| -> String {
        let npe = throw("java/lang/NullPointerException", exit);
        let callee = unit.signature(descriptor).unwrap();
        let base = d - callee.params.len();
        let (key, receiver) = match kind {
            CallKind::Direct(key) => {
                let declaring = unit.get(&key.0).unwrap();
                let m = declaring
                    .methods
                    .iter()
                    .find(|m| m.name == key.1 && m.descriptor == key.2)
                    .unwrap();
                (key, m.access & ACC_STATIC == 0)
            }
            CallKind::Virtual { target, .. } | CallKind::Interface { target, .. } => (target, true),
            CallKind::ObjectInit => unreachable!(),
        };
        let result_at = if receiver { base - 1 } else { base };
        let mut args: Vec<String> = Vec::new();
        if receiver {
            args.push(format!("s[{}].a", base - 1));
        }
        args.extend(
            callee
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| format!("s[{}].{}", base + i, field(kind_of(p.code).unwrap()))),
        );
        let function = match kind {
            CallKind::Virtual { slot, .. } => format!(
                "(({} (*)({}))s[{}].a->cls->vtable[{slot}])",
                c_type(callee.ret.code),
                c_params(false, &callee, false),
                base - 1
            ),
            // The guard below looked the method up through the site's inline cache.
            CallKind::Interface { .. } => format!(
                "(({} (*)({}))jabs_ic_fn)",
                c_type(callee.ret.code),
                c_params(false, &callee, false),
            ),
            _ => symbol(&key.0, &key.1, &key.2),
        };
        let guard = if let CallKind::Interface { selector, .. } = kind {
            // The receiver's class must implement the interface (JVMS invokeinterface). One
            // lookup, through this call site's own inline cache (a static in its own block),
            // finds the method or proves it missing.
            format!(
                "if (!s[{r}].a) {npe} {{ static _Atomic(const jabs_itable_entry *) jabs_ic; \
                 void (*jabs_ic_fn)(void) = jabs_itable_cached(s[{r}].a, UINT64_C(0x{selector:016x}), &jabs_ic); \
                 if (!jabs_ic_fn) {} ",
                throw("java/lang/IncompatibleClassChangeError", exit),
                r = base - 1
            )
        } else if receiver {
            format!("if (!s[{}].a) {npe} ", base - 1)
        } else {
            init_guard(unit, &class.name, &key.0, exit)
        };
        let invoke = format!("{function}({})", args.join(", "));
        let assign = match kind_of(callee.ret.code) {
            Some(k) => format!("s[{result_at}].{} = {invoke};", field(k)),
            None => format!("{invoke};"),
        };
        let close = if matches!(kind, CallKind::Interface { .. }) {
            "}"
        } else {
            ""
        };
        format!("{guard}{assign}{close} if (jabs_pending) {exit}")
    };
    for (insn, stack) in a.insns.iter().zip(&a.stacks) {
        if a.targets.contains(&insn.pc) {
            let _ = writeln!(out, "L{}:;", insn.pc);
        }
        let d = stack.len();
        let resolved = a.resolved.get(&insn.pc);
        let exit = match a.exits.get(&insn.pc) {
            Some(n) => format!("goto E{n};"),
            None => ret_default.to_string(),
        };
        let exit = exit.as_str();
        let npe = throw("java/lang/NullPointerException", exit);
        if let Some(Resolved::Unresolved(message)) = resolved {
            let _ = writeln!(out, "    jabs_not_compiled(\"{}\");", c_escape(message));
            continue;
        }
        if let Some(Resolved::UnresolvedType(message)) = resolved {
            let stop = format!("jabs_not_compiled(\"{}\");", c_escape(message));
            let line = match &insn.op {
                Op::InstanceOf { .. } => {
                    format!("if (s[{t}].a == NULL) s[{t}].i = 0; else {stop}", t = d - 1)
                }
                _ => format!("if (s[{}].a != NULL) {stop}", d - 1),
            };
            let _ = writeln!(out, "    {line}");
            continue;
        }
        if let Some(Resolved::Absent(class)) = resolved {
            // HotSpot resolves the class of instanceof/checkcast only for a non-null value; a
            // member access always resolves it.
            let raise = throw("java/lang/NoClassDefFoundError", exit);
            let line = match &insn.op {
                Op::InstanceOf { .. } => {
                    format!(
                        "if (s[{t}].a == NULL) s[{t}].i = 0; else {raise}",
                        t = d - 1
                    )
                }
                Op::CheckCast { .. } => format!("if (s[{}].a != NULL) {raise}", d - 1),
                _ => raise,
            };
            let _ = writeln!(
                out,
                "    {line} /* {} is on no class path */",
                class.replace("*/", "*_/")
            );
            continue;
        }
        let line = match &insn.op {
            Op::Nop | Op::Pop | Op::Pop2 => continue,
            Op::Const(v) => format!("s[{d}].{} = {};", field(v.kind()), value_literal(*v)),
            Op::AConstNull => format!("s[{d}].a = NULL;"),
            Op::Load(k, n) => format!("s[{d}].{f} = l[{n}].{f};", f = field(*k)),
            Op::Store(k, n) => format!("l[{n}].{f} = s[{}].{f};", d - 1, f = field(*k)),
            Op::Iinc(n, c) => format!("l[{n}].i = jabs_iadd(l[{n}].i, {c});"),
            Op::Dup => format!("s[{d}] = s[{}];", d - 1),
            Op::DupX1 => format!(
                "s[{d}] = s[{}]; s[{}] = s[{}]; s[{}] = s[{d}];",
                d - 1,
                d - 1,
                d - 2,
                d - 2
            ),
            Op::DupX2 | Op::Dup2X1 | Op::Dup2X2 => {
                let categories: Vec<u16> =
                    stack.iter().rev().take(4).map(|v| v.category()).collect();
                let (take, order) = stack_shuffle(&insn.op, &categories).expect("verified");
                let base = d - take;
                let mut text = format!("{{ jabs_value t[{take}];");
                for k in 0..take {
                    let _ = write!(text, " t[{k}] = s[{}];", base + k);
                }
                for (j, i) in order.iter().enumerate() {
                    let _ = write!(text, " s[{}] = t[{i}];", base + j);
                }
                text.push_str(" }");
                text
            }
            Op::Dup2 if stack[d - 1].category() == 2 => format!("s[{d}] = s[{}];", d - 1),
            Op::Dup2 => format!("s[{d}] = s[{}]; s[{}] = s[{}];", d - 2, d + 1, d - 1),
            Op::Swap => format!(
                "{{ jabs_value t = s[{}]; s[{}] = s[{}]; s[{}] = t; }}",
                d - 1,
                d - 1,
                d - 2,
                d - 2
            ),
            Op::Binary(k, op) => {
                let (f, a, b) = (field(*k), d - 2, d - 1);
                let rhs = if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                    "i"
                } else {
                    f
                };
                let prefix = match k {
                    Kind::I => "jabs_i",
                    Kind::J => "jabs_l",
                    _ => "",
                };
                let name = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::Mul => "mul",
                    BinOp::Div => "div",
                    BinOp::Rem => "rem",
                    BinOp::Shl => "shl",
                    BinOp::Shr => "shr",
                    BinOp::Ushr => "ushr",
                    BinOp::And => "&",
                    BinOp::Or => "|",
                    BinOp::Xor => "^",
                };
                let expr = match (k, op) {
                    (_, BinOp::And | BinOp::Or | BinOp::Xor) => {
                        format!("s[{a}].{f} {name} s[{b}].{f}")
                    }
                    (Kind::F, BinOp::Rem) => format!("fmodf(s[{a}].f, s[{b}].f)"),
                    (Kind::D, BinOp::Rem) => format!("fmod(s[{a}].d, s[{b}].d)"),
                    (Kind::F | Kind::D, _) => {
                        let sym = match op {
                            BinOp::Add => "+",
                            BinOp::Sub => "-",
                            BinOp::Mul => "*",
                            _ => "/",
                        };
                        format!("s[{a}].{f} {sym} s[{b}].{f}")
                    }
                    _ => format!("{prefix}{name}(s[{a}].{f}, s[{b}].{rhs})"),
                };
                let check = match (k, op) {
                    (Kind::I | Kind::J, BinOp::Div | BinOp::Rem) => {
                        format!(
                            "if (s[{b}].{f} == 0) {} ",
                            throw("java/lang/ArithmeticException", exit)
                        )
                    }
                    _ => String::new(),
                };
                format!("{check}s[{a}].{f} = {expr};")
            }
            Op::Neg(k) => {
                let f = field(*k);
                match k {
                    Kind::I => format!("s[{0}].i = jabs_ineg(s[{0}].i);", d - 1),
                    Kind::J => format!("s[{0}].j = jabs_lneg(s[{0}].j);", d - 1),
                    _ => format!("s[{0}].{f} = -s[{0}].{f};", d - 1),
                }
            }
            Op::Convert(c) => {
                let (from, to) = c.kinds();
                let src = format!("s[{}].{}", d - 1, field(from));
                let expr = match c {
                    Convert::I2l => format!("(int64_t){src}"),
                    Convert::I2f | Convert::L2f | Convert::D2f => format!("(float){src}"),
                    Convert::I2d | Convert::L2d | Convert::F2d => format!("(double){src}"),
                    other => format!("jabs_{}({src})", format!("{other:?}").to_ascii_lowercase()),
                };
                format!("s[{}].{} = {expr};", d - 1, field(to))
            }
            Op::Compare(c) => {
                let f = match c {
                    Compare::Lcmp => "j",
                    Compare::Fcmpl | Compare::Fcmpg => "f",
                    _ => "d",
                };
                let name = format!("{c:?}").to_ascii_lowercase();
                format!(
                    "s[{a}].i = jabs_{name}(s[{a}].{f}, s[{b}].{f});",
                    a = d - 2,
                    b = d - 1
                )
            }
            Op::If(c, t) => {
                if *t <= insn.pc {
                    format!(
                        "if (s[{}].i {} 0) {{ jabs_gc_safepoint(); goto L{t}; }}",
                        d - 1,
                        cond_op(*c)
                    )
                } else {
                    format!("if (s[{}].i {} 0) goto L{t};", d - 1, cond_op(*c))
                }
            }
            Op::IfICmp(c, t) => {
                if *t <= insn.pc {
                    format!(
                        "if (s[{}].i {} s[{}].i) {{ jabs_gc_safepoint(); goto L{t}; }}",
                        d - 2,
                        cond_op(*c),
                        d - 1
                    )
                } else {
                    format!(
                        "if (s[{}].i {} s[{}].i) goto L{t};",
                        d - 2,
                        cond_op(*c),
                        d - 1
                    )
                }
            }
            Op::IfNull { is_null, target } => {
                let cond = format!("s[{}].a {} NULL", d - 1, if *is_null { "==" } else { "!=" });
                if *target <= insn.pc {
                    format!("if ({cond}) {{ jabs_gc_safepoint(); goto L{target}; }}")
                } else {
                    format!("if ({cond}) goto L{target};")
                }
            }
            Op::IfACmp { equal, target } => {
                let cond = format!(
                    "s[{}].a {} s[{}].a",
                    d - 2,
                    if *equal { "==" } else { "!=" },
                    d - 1
                );
                if *target <= insn.pc {
                    format!("if ({cond}) {{ jabs_gc_safepoint(); goto L{target}; }}")
                } else {
                    format!("if ({cond}) goto L{target};")
                }
            }
            Op::Goto(t) => {
                if *t <= insn.pc {
                    format!("{{ jabs_gc_safepoint(); goto L{t}; }}")
                } else {
                    format!("goto L{t};")
                }
            }
            Op::TableSwitch {
                default,
                low,
                targets,
            } => {
                let mut s = format!("switch (s[{}].i) {{", d - 1);
                for (i, t) in targets.iter().enumerate() {
                    let key = *low as i64 + i as i64;
                    if *t <= insn.pc {
                        let _ = write!(
                            s,
                            " case {}: {{ jabs_gc_safepoint(); goto L{t}; }}",
                            int_case(key as i32)
                        );
                    } else {
                        let _ = write!(s, " case {}: goto L{t};", int_case(key as i32));
                    }
                }
                if *default <= insn.pc {
                    let _ = write!(
                        s,
                        " default: {{ jabs_gc_safepoint(); goto L{default}; }} }}"
                    );
                } else {
                    let _ = write!(s, " default: goto L{default}; }}");
                }
                s
            }
            Op::LookupSwitch { default, pairs } => {
                let mut s = format!("switch (s[{}].i) {{", d - 1);
                for (key, t) in pairs {
                    if *t <= insn.pc {
                        let _ = write!(
                            s,
                            " case {}: {{ jabs_gc_safepoint(); goto L{t}; }}",
                            int_case(*key)
                        );
                    } else {
                        let _ = write!(s, " case {}: goto L{t};", int_case(*key));
                    }
                }
                if *default <= insn.pc {
                    let _ = write!(
                        s,
                        " default: {{ jabs_gc_safepoint(); goto L{default}; }} }}"
                    );
                } else {
                    let _ = write!(s, " default: goto L{default}; }}");
                }
                s
            }
            Op::Return(None) => "return;".into(),
            Op::Return(Some(k)) => {
                let v = format!("s[{}].{}", d - 1, field(*k));
                match sig.ret.code {
                    'Z' => format!("return {v} & 1;"),
                    'B' => format!("return jabs_i2b({v});"),
                    'C' => format!("return jabs_i2c({v});"),
                    'S' => format!("return jabs_i2s({v});"),
                    _ => format!("return {v};"),
                }
            }
            Op::MonitorEnter => format!(
                "if (!s[{r}].a) {npe} jabs_monitor_enter(s[{r}].a);",
                r = d - 1
            ),
            Op::MonitorExit => format!(
                "if (!s[{r}].a) {npe} if (!jabs_monitor_exit(s[{r}].a)) {}",
                throw("java/lang/IllegalMonitorStateException", exit),
                r = d - 1
            ),
            // Every reference is an Object: the cast always succeeds and instanceof is a null test.
            Op::CheckCast { class: target } if target == OBJECT => continue,
            Op::InstanceOf { class: target } if target == OBJECT => {
                format!("s[{r}].i = s[{r}].a != NULL;", r = d - 1)
            }
            Op::CheckCast { class: target } if target.starts_with('[') => format!(
                "if (s[{r}].a && !jabs_is_array_instance(s[{r}].a, \"{}\", {})) {}",
                c_escape(target),
                array_component_record(unit, target),
                throw("java/lang/ClassCastException", exit),
                r = d - 1
            ),
            Op::InstanceOf { class: target } if target.starts_with('[') => format!(
                "s[{r}].i = s[{r}].a ? jabs_is_array_instance(s[{r}].a, \"{}\", {}) : 0;",
                c_escape(target),
                array_component_record(unit, target),
                r = d - 1
            ),
            Op::AThrow => format!(
                "if (!s[{r}].a) {npe} jabs_pending = s[{r}].a; {exit}",
                r = d - 1
            ),
            Op::InvokeStatic { descriptor, .. }
            | Op::InvokeVirtual(MemberRef { descriptor, .. })
            | Op::InvokeSpecial(MemberRef { descriptor, .. })
            | Op::InvokeInterface(MemberRef { descriptor, .. }) => match resolved {
                Some(Resolved::Call(CallKind::ObjectInit)) => {
                    "/* java/lang/Object.<init>()V has no effect */".into()
                }
                Some(Resolved::VarHandle(mode)) => {
                    var_handle_access(mode, descriptor, d, exit, &npe)
                }
                Some(Resolved::Call(kind)) if caller_sensitive(unit, &insn.op) => format!(
                    "{{ const char *jabs_saved_caller = jabs_caller; jabs_caller = \"{}\"; {} jabs_caller = jabs_saved_caller; }}",
                    c_escape(&class.name),
                    call(kind, descriptor, d, exit)
                ),
                Some(Resolved::Call(kind)) => call(kind, descriptor, d, exit),
                _ => unreachable!("calls are resolved during verification"),
            },
            Op::GetStatic {
                name, descriptor, ..
            }
            | Op::PutStatic {
                name, descriptor, ..
            } => {
                let Some(Resolved::Field(declaring)) = resolved else {
                    unreachable!()
                };
                let k = kind_of(parse_type(descriptor, name).unwrap().code).unwrap();
                let guard = init_guard(unit, &class.name, declaring, exit);
                let sym = field_symbol(declaring, name);
                let volatile = field_is_volatile(unit, declaring, name, descriptor);
                if matches!(insn.op, Op::GetStatic { .. }) {
                    let load =
                        field_load(&sym, &format!("s[{d}].{}", field(k)), descriptor, volatile);
                    format!("{guard}{load}")
                } else {
                    let value = narrow_for_store(descriptor, &format!("s[{}].{}", d - 1, field(k)));
                    format!("{guard}{}", field_store(&sym, &value, descriptor, volatile))
                }
            }
            Op::New { class: target } => format!(
                "{}s[{d}].a = jabs_new(&{}); if (!s[{d}].a) {exit}",
                init_guard(unit, &class.name, target, exit),
                class_record(target)
            ),
            Op::LdcString(units) => format!(
                "s[{d}].a = jabs_literal({}u); if (!s[{d}].a) {exit}",
                literal_ids[units]
            ),
            Op::LdcClass(name) => format!(
                "s[{d}].a = jabs_class_object(\"{}\"); if (!s[{d}].a) {exit}",
                c_escape(name)
            ),
            Op::NewArray(element) => {
                let r = d - 1;
                format!(
                    "if (s[{r}].i < 0) {} s[{r}].a = jabs_new_array(s[{r}].i, sizeof({}), JABS_COMPONENT_PRIMITIVE, NULL, \"[{element}\", NULL); if (!s[{r}].a) {exit}",
                    throw("java/lang/NegativeArraySizeException", exit),
                    element_type(*element)
                )
            }
            Op::ANewArray { class: component } => {
                let r = d - 1;
                // Record the runtime element class so covariant stores can be checked (ArrayStoreException).
                let element = if component.starts_with('[') {
                    component.clone()
                } else {
                    format!("L{component};")
                };
                // Rows (an array of arrays): the runtime checks a stored row against the row type,
                // exactly for primitive rows and covariantly for reference rows, which needs the
                // record of a one-level row's element class.
                let rows = component.starts_with('[');
                let (kind, record) = if component == OBJECT {
                    ("JABS_COMPONENT_ANY", "NULL".to_string())
                } else if rows {
                    let element = component
                        .strip_prefix("[L")
                        .and_then(|rest| rest.strip_suffix(';'));
                    let record = match element {
                        Some(e)
                            if e != OBJECT && unit.get(e).is_some() && unit.chain(e).is_ok() =>
                        {
                            format!("&{}", class_record(e))
                        }
                        _ => "NULL".to_string(),
                    };
                    ("JABS_COMPONENT_ARRAY", record)
                } else if !component.starts_with('[')
                    && unit.get(component).is_some()
                    && unit.chain(component).is_ok()
                {
                    (
                        "JABS_COMPONENT_CLASS",
                        format!("&{}", class_record(component)),
                    )
                } else {
                    ("JABS_COMPONENT_UNCHECKED", "NULL".to_string())
                };
                let component_descriptor = if rows {
                    format!("\"{}\"", c_escape(component))
                } else {
                    "NULL".to_string()
                };
                format!(
                    "if (s[{r}].i < 0) {} s[{r}].a = jabs_new_array(s[{r}].i, sizeof(jabs_object *), {kind}, {record}, \"[{}\", {component_descriptor}); if (!s[{r}].a) {exit}",
                    throw("java/lang/NegativeArraySizeException", exit),
                    c_escape(&element)
                )
            }
            Op::MultiANewArray {
                class: descriptor,
                dimensions,
            } => {
                let dimensions = usize::from(*dimensions);
                let base = d - dimensions;
                let counts: Vec<String> = (base..d).map(|i| format!("s[{i}].i")).collect();
                let negative: Vec<String> = counts.iter().map(|c| format!("{c} < 0")).collect();
                // The innermost element class's record: stores into the leaf arrays (and into
                // rows of a one-level reference row type) are checked against it.
                let record = match innermost_class(descriptor) {
                    Some(e) if e != OBJECT && unit.get(e).is_some() && unit.chain(e).is_ok() => {
                        format!("&{}", class_record(e))
                    }
                    _ => "NULL".to_string(),
                };
                // JVMS 6.5: every count is checked before anything is allocated.
                format!(
                    "if ({}) {} {{ const int32_t counts[{dimensions}] = {{ {} }}; s[{base}].a = jabs_multi_new_array(\"{}\", {dimensions}, counts, {record}); }} if (!s[{base}].a) {exit}",
                    negative.join(" || "),
                    throw("java/lang/NegativeArraySizeException", exit),
                    counts.join(", "),
                    c_escape(descriptor)
                )
            }
            Op::ArrayLength => {
                let r = d - 1;
                format!("if (!s[{r}].a) {npe} s[{r}].i = ((jabs_array *)s[{r}].a)->length;")
            }
            Op::ArrayLoad(family) | Op::ArrayStore(family) => {
                let storing = matches!(insn.op, Op::ArrayStore(_));
                let Some(Resolved::Element(element)) = resolved else {
                    unreachable!()
                };
                let (array, index) = if storing {
                    (d - 3, d - 2)
                } else {
                    (d - 2, d - 1)
                };
                let checks = format!(
                    "if (!s[{array}].a) {npe} if ((uint32_t)s[{index}].i >= (uint32_t)((jabs_array *)s[{array}].a)->length) {}",
                    throw("java/lang/ArrayIndexOutOfBoundsException", exit)
                );
                match element {
                    // Statically null: the null check always throws.
                    None => checks,
                    Some(e) => {
                        let slot = format!(
                            "(({} *)jabs_array_data((jabs_array *)s[{array}].a))[s[{index}].i]",
                            element_type(*e)
                        );
                        let k = if *family == 'A' {
                            Kind::A
                        } else if matches!(family, 'B' | 'C' | 'S') {
                            Kind::I
                        } else {
                            kind_of(*family).unwrap()
                        };
                        if storing {
                            let value = format!("s[{}].{}", d - 1, field(k));
                            let narrowed = narrow_for_store(&e.to_string(), &value);
                            let store_check = if *family == 'A' {
                                format!(
                                    "if ({value} && !jabs_array_store_ok((jabs_array *)s[{array}].a, {value})) {} ",
                                    throw("java/lang/ArrayStoreException", exit)
                                )
                            } else {
                                String::new()
                            };
                            format!("{checks} {store_check}{slot} = {narrowed};")
                        } else {
                            format!("{checks} s[{array}].{} = {slot};", field(k))
                        }
                    }
                }
            }
            Op::CheckCast { class: target } => format!(
                "if (s[{r}].a && !jabs_is_instance(s[{r}].a, &{})) {}",
                class_record(target),
                throw("java/lang/ClassCastException", exit),
                r = d - 1
            ),
            Op::InstanceOf { class: target } => {
                format!(
                    "s[{r}].i = s[{r}].a ? jabs_is_instance(s[{r}].a, &{}) : 0;",
                    class_record(target),
                    r = d - 1
                )
            }
            Op::GetField(m) | Op::PutField(m) => {
                let Some(Resolved::Field(declaring)) = resolved else {
                    unreachable!()
                };
                let k = kind_of(parse_type(&m.descriptor, &m.name).unwrap().code).unwrap();
                let obj = object_struct(declaring);
                let member = instance_field(&m.name);
                let volatile = field_is_volatile(unit, declaring, &m.name, &m.descriptor);
                if matches!(insn.op, Op::GetField(_)) {
                    let r = d - 1;
                    let load = field_load(
                        &format!("(({obj} *)s[{r}].a)->{member}"),
                        &format!("s[{r}].{}", field(k)),
                        &m.descriptor,
                        volatile,
                    );
                    format!("if (!s[{r}].a) {npe} {load}")
                } else {
                    let (r, v) = (d - 2, d - 1);
                    let value = narrow_for_store(&m.descriptor, &format!("s[{v}].{}", field(k)));
                    let store = field_store(
                        &format!("(({obj} *)s[{r}].a)->{member}"),
                        &value,
                        &m.descriptor,
                        volatile,
                    );
                    format!("if (!s[{r}].a) {npe} {store}")
                }
            }
        };
        let _ = writeln!(out, "    {line}");
    }
    // Handler dispatch: test catch types in table order; the matching handler starts with only
    // the exception on the stack. Unmatched exceptions propagate to the caller.
    for (n, chain) in a.dispatch.iter().enumerate() {
        let _ = writeln!(out, "E{n}:;");
        for catch in chain {
            let test = match &catch.class {
                Some(c) => format!("jabs_is_instance(jabs_pending, &{})", class_record(c)),
                None => "1".to_string(),
            };
            let _ = writeln!(
                out,
                "    if ({test}) {{ s[0].a = jabs_pending; jabs_pending = NULL; jabs_gc_safepoint(); goto L{}; }}",
                catch.handler
            );
        }
        let _ = writeln!(out, "    {ret_default}");
    }
    let _ = writeln!(out, "}}\n");
    if synchronized {
        emit_synchronized_wrapper(out, class, method, sig);
    }
}

/// Lock the receiver (or, for static methods, the class's monitor), run the body, and unlock on
/// both normal and exceptional completion, as the JVM does for ACC_SYNCHRONIZED methods.
fn emit_synchronized_wrapper(
    out: &mut String,
    class: &ClassFile,
    method: &Member,
    sig: &Signature,
) {
    let is_static = method.access & ACC_STATIC != 0;
    let name = symbol(&class.name, &method.name, &method.descriptor);
    let ret_default = if sig.ret.code == 'V' {
        "return;"
    } else {
        "return 0;"
    };
    // Static methods lock the Class object (jabs_static_monitor), found once and cached; the
    // dedicated object is the monitor only in programs without a Class registry.
    let lock = if is_static {
        let monitor = mangled("jcm_", &[&class.name]);
        let cached = mangled("jcl_", &[&class.name]);
        let _ = writeln!(out, "static jabs_object {monitor};");
        let _ = writeln!(out, "static _Atomic(jabs_object *) {cached};");
        format!(
            "atomic_load_explicit(&{cached}, memory_order_acquire);\n    if (!lock) {{ lock = jabs_static_monitor(\"{}\", &{monitor}); if (!lock) {ret_default} atomic_store_explicit(&{cached}, lock, memory_order_release); }}",
            c_escape(&class.name)
        )
    } else {
        "self;".to_string()
    };
    let mut args: Vec<String> = Vec::new();
    if !is_static {
        args.push("self".into());
    }
    args.extend((0..sig.params.len()).map(|i| format!("a{i}")));
    let call = format!("{name}__sync({})", args.join(", "));
    let _ = writeln!(out, "{} {{", prototype(&class.name, method, sig));
    let _ = writeln!(out, "    jabs_object *lock = {lock}");
    let _ = writeln!(out, "    jabs_monitor_enter(lock);");
    if sig.ret.code == 'V' {
        let _ = writeln!(out, "    {call};\n    jabs_monitor_exit(lock);\n}}\n");
    } else {
        let _ = writeln!(out, "    {} result = {call};", c_type(sig.ret.code));
        let _ = writeln!(
            out,
            "    jabs_monitor_exit(lock);\n    return result;\n}}\n"
        );
    }
}

/// Switch case label; INT32_MIN needs the macro since `-2147483648` is not an int literal in C.
fn int_case(key: i32) -> String {
    if key == i32::MIN {
        "INT32_MIN".into()
    } else {
        key.to_string()
    }
}

/// Emit a class's object layout: its superclass's struct (or the header) first, then its own
/// instance fields ordered references-first and descending by alignment to eliminate padding.
fn field_layout_rank(descriptor: &str) -> u8 {
    match storage_type(descriptor) {
        Some("jabs_object *") => 0,
        None => 1,
        Some("int64_t") | Some("double") => 2,
        Some("int32_t") | Some("float") => 3,
        Some("uint16_t") | Some("int16_t") => 4,
        _ => 5,
    }
}

fn emit_object_layout(out: &mut String, class: &ClassFile) {
    let base = match class.super_name.as_deref() {
        Some(parent) if parent != OBJECT => format!("{} super;", object_struct(parent)),
        _ => "jabs_object header;".to_string(),
    };
    let _ = writeln!(out, "{} {{\n    {base}", object_struct(&class.name));
    let mut instance_fields: Vec<&Member> = class
        .fields
        .iter()
        .filter(|f| f.access & ACC_STATIC == 0)
        .collect();
    instance_fields.sort_by_key(|f| field_layout_rank(&f.descriptor));
    for f in instance_fields {
        match storage_type(&f.descriptor) {
            Some(ty) => {
                let _ = writeln!(out, "    {ty} {};", instance_field(&f.name));
            }
            // Present so the layout is complete; no compiled code can access it yet.
            None => {
                let _ = writeln!(
                    out,
                    "    void *{}; /* {}: not accessible yet */",
                    instance_field(&f.name),
                    f.descriptor
                );
            }
        }
    }
    let _ = writeln!(out, "}};\n");
}

/// Text of a `static final String` field's ConstantValue, if it has one.
fn constant_string(class: &ClassFile, field: &Member) -> Option<Vec<u16>> {
    match field
        .constant_value
        .and_then(|i| class.pool.get(i as usize))
    {
        Some(Constant::String(text)) if field.descriptor == "Ljava/lang/String;" => {
            class.string_units.get(text).cloned()
        }
        _ => None,
    }
}

/// Emit a compact string-literal table (`jabs_literals[]`), atomic cache (`jabs_literal_cache[]`),
/// and slow-path constructor (`jabs_literal_get`) shared by all compiled methods and static fields.
fn emit_literals(out: &mut String, literals: &[Vec<u16>], intern_table: bool) {
    if literals.is_empty() {
        out.push_str("const uint32_t jabs_gc_literal_count = 0u;\n_Atomic(jabs_object *) *const jabs_gc_literals = NULL;\n\n");
        return;
    }
    let string = object_struct(STRING);
    let record = class_record(STRING);
    let mut latin1_bytes: Vec<u8> = Vec::new();
    let mut utf16_units: Vec<u16> = Vec::new();
    let mut descs: Vec<(usize, usize, u8)> = Vec::with_capacity(literals.len());
    for units in literals {
        if units.iter().all(|&u| u <= 0xFF) {
            let offset = latin1_bytes.len();
            latin1_bytes.extend(units.iter().map(|&u| u as u8));
            descs.push((offset, units.len(), 0));
        } else {
            let offset = utf16_units.len();
            utf16_units.extend_from_slice(units);
            descs.push((offset, units.len() * 2, 1));
        }
    }
    out.push_str("static const unsigned char jabs_lit_latin1[] = {\n");
    if latin1_bytes.is_empty() {
        out.push_str("    0\n");
    } else {
        for chunk in latin1_bytes.chunks(24) {
            out.push_str("   ");
            for b in chunk {
                let _ = write!(out, " {b},");
            }
            out.push('\n');
        }
    }
    out.push_str("};\nstatic const uint16_t jabs_lit_utf16[] = {\n");
    if utf16_units.is_empty() {
        out.push_str("    0\n");
    } else {
        for chunk in utf16_units.chunks(16) {
            out.push_str("   ");
            for u in chunk {
                let _ = write!(out, " 0x{u:04x}u,");
            }
            out.push('\n');
        }
    }
    out.push_str(
        "};\ntypedef struct { uint32_t offset; uint32_t byte_len; uint8_t coder; } jabs_literal_desc;\nstatic const jabs_literal_desc jabs_literals[] = {\n",
    );
    for (offset, byte_len, coder) in &descs {
        let _ = writeln!(out, "    {{ {offset}u, {byte_len}u, {coder} }},");
    }
    let _ = writeln!(
        out,
        "}};\n_Atomic(jabs_object *) jabs_literal_cache[{count}];\nconst uint32_t jabs_gc_literal_count = {count}u;\n_Atomic(jabs_object *) *const jabs_gc_literals = jabs_literal_cache;",
        count = literals.len()
    );
    let intern_line = if intern_table {
        "    s = jabs_string_intern(s);\n    if (!s) return NULL;\n"
    } else {
        ""
    };
    let _ = write!(
        out,
        r#"
jabs_object *jabs_literal_get(uint32_t id) {{
    jabs_object *cached = atomic_load_explicit(&jabs_literal_cache[id], memory_order_acquire);
    if (cached) return cached;
    if ({init}() != 0) return NULL;
    const jabs_literal_desc *d = &jabs_literals[id];
    jabs_object *value = jabs_new_array((int32_t)d->byte_len, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (!value) return NULL;
    if (d->byte_len > 0) {{
        const void *src = d->coder == 0
            ? (const void *)&jabs_lit_latin1[d->offset]
            : (const void *)&jabs_lit_utf16[d->offset];
        memcpy(jabs_array_data((jabs_array *)value), src, d->byte_len);
    }}
    jabs_object *s = jabs_new(&{record});
    if (!s) return NULL;
    (({string} *)s)->{value_field} = value;
    (({string} *)s)->{coder_field} = d->coder;
{intern_line}    jabs_object *expected = NULL;
    if (!atomic_compare_exchange_strong_explicit(&jabs_literal_cache[id], &expected, s, memory_order_acq_rel, memory_order_acquire)) return expected;
    return s;
}}
"#,
        init = init_symbol(STRING),
        value_field = instance_field("value"),
        coder_field = instance_field("coder"),
    );
    out.push('\n');
}

/// `&record` of a one-dimensional reference array's element class when the unit has it, else NULL.
fn array_component_record(unit: &Unit, array: &str) -> String {
    match element_class(array) {
        Some(c)
            if c != OBJECT
                && !c.starts_with('[')
                && unit.get(&c).is_some()
                && (unit.is_interface(&c) || unit.chain(&c).is_ok()) =>
        {
            format!("&{}", class_record(&c))
        }
        _ => "NULL".to_string(),
    }
}

/// C name of the stub that stands in for an implementation that is not compiled.
fn stub_symbol(key: &MethodKey) -> String {
    mangled("jx_", &[&key.0, &key.1, &key.2])
}

/// C name of the stub for an interface call with no unique implementation on `class`.
fn error_stub_symbol(class: &str, name: &str, descriptor: &str) -> String {
    mangled("jxe_", &[class, name, descriptor])
}

/// Format one typed stub declaration + definition with external linkage (for sharding across `unit_0_stubs_*.c`).
fn format_stub(name: &str, sig: &Signature, body: &str) -> (String, String) {
    let unused: String = (0..sig.params.len())
        .map(|i| format!(" (void)a{i};"))
        .collect();
    let ret_ty = c_type(sig.ret.code);
    let params = c_params(false, sig, true);
    let decl = format!("{ret_ty} {name}({params});\n");
    let def = format!("{ret_ty} {name}({params}) {{ (void)self;{unused} {body} }}\n");
    (decl, def)
}

/// Collect typed stubs for vtable and interface-table entries without live code: `(decl, def)` pairs.
/// Abstract methods throw AbstractMethodError and ambiguous defaults IncompatibleClassChangeError,
/// as Java does; anything else stops with a diagnostic (the JVM backend must run the profile until
/// that method compiles). Stubs keep each class's code independent of which subclasses exist, so
/// per-class caching stays valid when mods add subclasses.
fn collect_stubs(
    unit: &Unit,
    recorded: &[&ClassFile],
    live: &dyn Fn(&str, &MethodKey) -> bool,
) -> Vec<(String, String)> {
    let mut stubs = Vec::new();
    let mut done = BTreeSet::new();
    let mut stub_for = |stubs: &mut Vec<(String, String)>, key: &MethodKey| {
        if !done.insert(key.clone()) {
            return;
        }
        let Ok(sig) = unit.signature(&key.2) else {
            return;
        };
        let is_abstract = unit
            .get(&key.0)
            .and_then(|c| {
                c.methods
                    .iter()
                    .find(|m| m.name == key.1 && m.descriptor == key.2)
            })
            .is_some_and(|m| m.access & ACC_ABSTRACT != 0);
        let ret = if sig.ret.code == 'V' {
            "return;"
        } else {
            "return 0;"
        };
        // JABS's VarHandles are instances of VarHandle itself (var_handle_functions), whose
        // invocation-behavior methods are abstract.
        let behavior = key.0 == VAR_HANDLE
            && key.2 == "()Ljava/lang/invoke/VarHandle;"
            && matches!(
                key.1.as_str(),
                "withInvokeExactBehavior" | "withInvokeBehavior"
            );
        let body = if behavior {
            format!(
                "return jabs_vh_with_exact(self, {});",
                i32::from(key.1 == "withInvokeExactBehavior")
            )
        } else if is_abstract {
            format!("{} {ret}", raise("java/lang/AbstractMethodError"))
        } else {
            format!(
                "jabs_not_compiled(\"{}\");",
                c_escape(&format!("{}.{}{}", key.0, key.1, key.2))
            )
        };
        stubs.push(format_stub(&stub_symbol(key), &sig, &body));
    };
    for class in recorded {
        if let Ok(vt) = unit.vtable(&class.name) {
            for slot in vt.iter() {
                let key = (
                    slot.owner.clone(),
                    slot.name.clone(),
                    slot.descriptor.clone(),
                );
                if !live(&class.name, &key) {
                    stub_for(&mut stubs, &key);
                }
            }
        }
        for entry in unit.itable(&class.name).iter() {
            let Ok(sig) = unit.signature(&entry.descriptor) else {
                continue;
            };
            let ret = if sig.ret.code == 'V' {
                "return;"
            } else {
                "return 0;"
            };
            match &entry.target {
                ItableTarget::Impl(key) if !live(&class.name, key) => stub_for(&mut stubs, key),
                ItableTarget::Impl(_) => {}
                ItableTarget::Abstract | ItableTarget::Ambiguous => {
                    let error = raise(if matches!(entry.target, ItableTarget::Abstract) {
                        "java/lang/AbstractMethodError"
                    } else {
                        "java/lang/IncompatibleClassChangeError"
                    });
                    let name = error_stub_symbol(&class.name, &entry.name, &entry.descriptor);
                    stubs.push(format_stub(&name, &sig, &format!("{error} {ret}")));
                }
            }
        }
    }
    stubs
}

/// Emit the runtime record of an interface (used by instanceof, checkcast and array stores).
/// An interface's runtime record. It lists every superinterface (transitively, as class records
/// do), so an interface type is assignable to its superinterfaces: an `AnnTag[]` is an
/// `Annotation[]`. Only superinterfaces that have records themselves are listed (all of them do
/// when the unit has them: records include every superinterface of a recorded type).
fn emit_interface_record(
    out: &mut String,
    unit: &Unit,
    iface: &ClassFile,
    recorded: &BTreeSet<&str>,
) {
    let supers: Vec<String> = unit
        .superinterfaces_arc(&iface.name)
        .map(|ifaces| {
            ifaces
                .iter()
                .filter(|i| i.name != iface.name && recorded.contains(i.name.as_str()))
                .map(|i| class_record(&i.name))
                .collect()
        })
        .unwrap_or_default();
    let (list, count) = if supers.is_empty() {
        ("NULL".to_string(), 0)
    } else {
        // Records are defined in name order: declare the ones this list points at first.
        for s in &supers {
            let _ = writeln!(out, "extern const jabs_class {s};");
        }
        let name = mangled("jn_", &[&iface.name]);
        let _ = writeln!(
            out,
            "static const jabs_class *const {name}[] = {{ {} }};",
            supers
                .iter()
                .map(|s| format!("&{s}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        (name, supers.len())
    };
    let _ = writeln!(
        out,
        "const jabs_class {} = {{ \"{}\", 0, NULL, NULL, NULL, 0, {list}, {count}, 1, {}, 0, NULL, 0 }};",
        class_record(&iface.name),
        c_escape(&iface.name),
        iface.access
    );
}

/// Emit a class's vtable, interface table, interface list and runtime record; entries
/// without live code point at stubs.
fn emit_class_record(
    out: &mut String,
    unit: &Unit,
    class: &ClassFile,
    live: &dyn Fn(&str, &MethodKey) -> bool,
) {
    let function = |key: &MethodKey| -> Option<String> {
        if live(&class.name, key) {
            Some(format!(
                "(void (*)(void)){}",
                symbol(&key.0, &key.1, &key.2)
            ))
        } else if unit.signature(&key.2).is_ok() {
            Some(format!("(void (*)(void)){}", stub_symbol(key)))
        } else {
            None
        }
    };
    let vtable_name = mangled("jv_", &[&class.name]);
    let entries: Vec<String> = unit
        .vtable(&class.name)
        .map(|slots| {
            slots
                .iter()
                .map(|s| {
                    function(&(s.owner.clone(), s.name.clone(), s.descriptor.clone()))
                        .unwrap_or_else(|| "0".into())
                })
                .collect()
        })
        .unwrap_or_default();
    let entries = if entries.is_empty() {
        "0".to_string()
    } else {
        entries.join(", ")
    };
    // External linkage: records are referenced across the unit and must not trip unused warnings.
    let _ = writeln!(
        out,
        "void (*const {vtable_name}[])(void) = {{ {entries} }};"
    );

    let itable: Vec<String> = unit
        .itable(&class.name)
        .iter()
        .filter_map(|e| {
            let f = match &e.target {
                ItableTarget::Impl(key) => function(key)?,
                _ if unit.signature(&e.descriptor).is_ok() => {
                    format!(
                        "(void (*)(void)){}",
                        error_stub_symbol(&class.name, &e.name, &e.descriptor)
                    )
                }
                _ => return None,
            };
            Some(format!("{{ UINT64_C(0x{:016x}), {f} }}", e.selector))
        })
        .collect();
    let itable_name = mangled("ji_", &[&class.name]);
    let (itable_ref, itable_len) = if itable.is_empty() {
        ("NULL".to_string(), 0)
    } else {
        let _ = writeln!(
            out,
            "static const jabs_itable_entry {itable_name}[] = {{ {} }};",
            itable.join(", ")
        );
        (itable_name, itable.len())
    };
    let ifaces: Vec<String> = unit
        .superinterfaces_arc(&class.name)
        .map(|list| {
            list.iter()
                .map(|i| format!("&{}", class_record(&i.name)))
                .collect()
        })
        .unwrap_or_default();
    let ifaces_name = mangled("jn_", &[&class.name]);
    let (ifaces_ref, ifaces_len) = if ifaces.is_empty() {
        ("NULL".to_string(), 0)
    } else {
        let _ = writeln!(
            out,
            "static const jabs_class *const {ifaces_name}[] = {{ {} }};",
            ifaces.join(", ")
        );
        (ifaces_name, ifaces.len())
    };
    let parent = match class.super_name.as_deref() {
        Some(p) if p != OBJECT => format!("&{}", class_record(p)),
        _ => "NULL".to_string(),
    };
    let chain = unit.chain(&class.name).ok();
    let mut ref_offsets: Vec<String> = Vec::new();
    let mut ref_mask: u16 = 0;
    if let Some(ancestors) = &chain {
        for anc in ancestors.iter() {
            if anc.name == "java/lang/ref/SoftReference" {
                ref_mask |= 1 << 1;
            } else if anc.name == "java/lang/ref/WeakReference" {
                ref_mask |= 1 << 2;
            } else if anc.name == "java/lang/ref/PhantomReference" {
                ref_mask |= 1 << 3;
            }
            for f in anc.fields.iter().filter(|f| f.access & ACC_STATIC == 0) {
                if anc.name == "java/lang/ref/Reference"
                    && (f.name == "referent" || f.name == "discovered")
                {
                    continue;
                }
                if storage_type(&f.descriptor) == Some("jabs_object *") {
                    ref_offsets.push(format!(
                        "(uint32_t)offsetof({}, {})",
                        object_struct(&anc.name),
                        instance_field(&f.name)
                    ));
                }
            }
        }
    }
    let mut gc_flags: u16 = ref_mask;
    if !ref_offsets.is_empty() {
        gc_flags |= 1 << 0;
    }
    if class.name == VAR_HANDLE {
        gc_flags |= 1 << 4;
    }
    let (refs_ref, refs_len) = if ref_offsets.is_empty() {
        ("NULL".to_string(), 0usize)
    } else {
        let rname = mangled("jro_", &[&class.name]);
        let _ = writeln!(
            out,
            "static const uint32_t {rname}[] = {{ {} }};",
            ref_offsets.join(", ")
        );
        (rname, ref_offsets.len())
    };
    let _ = writeln!(
        out,
        "const jabs_class {} = {{ \"{}\", sizeof({}), {vtable_name}, {parent}, {itable_ref}, {itable_len}, {ifaces_ref}, {ifaces_len}, 0, {}, {gc_flags}u, {refs_ref}, {refs_len}u }};\n",
        class_record(&class.name),
        c_escape(&class.name),
        object_struct(&class.name),
        class.access
    );
}

/// Define the runtime's exception hooks: VM exceptions are instances of the JDK classes when
/// their records exist, else of name-only records (catchable only by name-free paths).
fn emit_vm_throwables(out: &mut String, recorded: &BTreeSet<&str>) {
    let mut records = Vec::new();
    for (name, _) in VM_EXCEPTIONS {
        if recorded.contains(name) {
            records.push(format!("&{}", class_record(name)));
        } else {
            let synthetic = mangled("jabs_vmx_", &[name]);
            let _ = writeln!(
                out,
                "const jabs_class {synthetic} = {{ \"{name}\", 0, NULL, NULL, NULL, 0, NULL, 0, 0, 0, 0, NULL, 0 }};"
            );
            records.push(format!("&{synthetic}"));
        }
    }
    let _ = writeln!(out, "jabs_object *jabs_vm_throwable(int kind) {{");
    let _ = writeln!(
        out,
        "    static const jabs_class *const records[] = {{ {} }};",
        records.join(", ")
    );
    let _ = writeln!(out, "    const jabs_class *c = records[kind];");
    let _ = writeln!(
        out,
        "    if (kind == JABS_VM_OUT_OF_MEMORY) return jabs_gc_oom_object(c);"
    );
    let _ = writeln!(
        out,
        "    size_t sz = c->size > sizeof(jabs_object) ? c->size : sizeof(jabs_object);"
    );
    let _ = writeln!(out, "    jabs_object *o = jabs_gc_alloc_obj(sz, c);");
    let _ = writeln!(
        out,
        "    if (!o) return jabs_gc_oom_object(records[JABS_VM_OUT_OF_MEMORY]);\n    return o;\n}}"
    );
    let error_test = if recorded.contains("java/lang/Error") {
        format!("jabs_is_instance(o, &{})", class_record("java/lang/Error"))
    } else {
        "strstr(o->cls->name, \"Error\") != NULL".to_string()
    };
    let _ = writeln!(
        out,
        "int jabs_is_error(const jabs_object *o) {{ return {error_test}; }}\n"
    );
    // Throwable.cause, so ExceptionInInitializerError.getCause() returns the original exception.
    if recorded.contains(THROWABLE) {
        let field = format!(
            "(({} *)o)->{}",
            object_struct(THROWABLE),
            instance_field("cause")
        );
        let _ = writeln!(
            out,
            "void jabs_set_cause(jabs_object *o, jabs_object *cause) {{ {field} = cause; }}"
        );
        let _ = writeln!(
            out,
            "jabs_object *jabs_cause_of(jabs_object *o) {{ return {field} == o ? NULL : {field}; }}\n"
        );
    } else {
        let _ = writeln!(
            out,
            "void jabs_set_cause(jabs_object *o, jabs_object *cause) {{ (void)o; (void)cause; }}"
        );
        let _ = writeln!(
            out,
            "jabs_object *jabs_cause_of(jabs_object *o) {{ (void)o; return NULL; }}\n"
        );
    }
}

/// Emit the Class-object registry: one java.lang.Class per name, created on first use with the
/// fields the JVM itself fills in (name, componentType, modifiers, primitive). Class loaders are
/// not modeled: every class reports the boot loader (null).
/// What the class registry needs from the rest of the compilation.
struct RegistryInputs<'a> {
    app_classes: &'a BTreeSet<String>,
    java_home: Option<&'a str>,
    /// java.home and java.class.path come from the program's .jab at run time.
    jab: bool,
    inits: &'a BTreeSet<String>,
    methods: &'a BTreeSet<MethodKey>,
    interfaces: &'a BTreeSet<&'a str>,
    /// Classes generated for invokedynamic (lambda proxies): hidden classes on the JVM, so their
    /// frames never appear in stack traces.
    hidden: &'a BTreeSet<String>,
    /// Proxy classes generated at build time (proxy.rs).
    proxies: &'a [crate::proxy::Generated],
}

fn emit_class_registry(
    out: &mut String,
    unit: &Unit,
    recorded: &[&ClassFile],
    inputs: &RegistryInputs,
) {
    let RegistryInputs {
        app_classes,
        java_home,
        jab,
        inits,
        methods,
        interfaces,
        hidden,
        proxies,
    } = *inputs;
    let class_struct = object_struct(CLASS);
    let string_struct = object_struct(STRING);
    // Classes and interfaces: lookups by name (Class objects, forName) must find both.
    let records: Vec<String> = recorded
        .iter()
        .map(|c| c.name.as_str())
        .chain(interfaces.iter().copied())
        .map(|name| format!("&{}", class_record(name)))
        .collect();
    let _ = writeln!(
        out,
        "static const jabs_class *const jabs_records[] = {{ {}NULL }};",
        records.iter().map(|r| format!("{r}, ")).collect::<String>()
    );
    // ClassLoadingMXBean's loaded count: every class and interface compiled in is loaded when
    // the program starts (VMManagementImpl.getTotalClassCount, src/cgen/management.rs).
    let _ = writeln!(
        out,
        "const int64_t jabs_class_record_count = {};",
        records.len()
    );
    // Field offsets of every recorded class's own instance fields, for Unsafe.objectFieldOffset.
    let _ = writeln!(
        out,
        "typedef struct {{ const char *class_name, *field_name; size_t offset; }} jabs_field_offset_entry;"
    );
    let _ = writeln!(
        out,
        "static const jabs_field_offset_entry jabs_field_offsets[] = {{"
    );
    for class in recorded {
        for f in class.fields.iter().filter(|f| f.access & ACC_STATIC == 0) {
            let _ = writeln!(
                out,
                "    {{ \"{}\", \"{}\", offsetof({}, {}) }},",
                c_escape(&class.name),
                c_escape(&f.name),
                object_struct(&class.name),
                instance_field(&f.name)
            );
        }
    }
    let _ = writeln!(out, "    {{ NULL, NULL, 0 }}\n}};");
    out.push_str(&format!(r#"
typedef struct {{ char *name; const jabs_class *record; jabs_object *object; }} jabs_class_entry;
/* Every thread shares the registry. The list is read and grown only under JABS_LOCK_CLASSES;
 * each entry is allocated on its own and never changes or moves once listed, so a pointer to one
 * stays valid after the lock is released. */
static jabs_class_entry **jabs_class_entries;
static size_t jabs_class_count, jabs_class_capacity;
static void jabs_set_defining_loader(jabs_object *c, const char *name);

/* Two open-addressing hash indexes over the entries, by name and by Class object: Object.getClass
 * and every Class method look an entry up, so a lookup must not scan the list or take the lock
 * (a scan under JABS_LOCK_CLASSES once took most of the 1.7.10 server's tick: its packet codec
 * calls getClass for every packet). Writers hold JABS_LOCK_CLASSES; readers take no lock. A slot,
 * once filled, never changes, and a grown table is fully built before it is published, so a
 * reader sees either the old table or the new one, each consistent. Old tables are kept (never
 * freed) because a reader may still be probing one. */
typedef struct {{ size_t mask; _Atomic(jabs_class_entry *) slots[]; }} jabs_class_index;
static _Atomic(jabs_class_index *) jabs_class_by_name, jabs_class_by_object;

static size_t jabs_class_hash_name(const char *name) {{
    uint64_t h = 1469598103934665603ULL; /* FNV-1a */
    for (const unsigned char *p = (const unsigned char *)name; *p; p++) h = (h ^ *p) * 1099511628211ULL;
    return (size_t)h;
}}

static size_t jabs_class_hash_object(const jabs_object *c) {{
    uint64_t h = (uint64_t)(uintptr_t)c;
    h ^= h >> 33; h *= 0xff51afd7ed558ccdULL; h ^= h >> 33;
    return (size_t)h;
}}

static void jabs_class_index_put(jabs_class_index *t, jabs_class_entry *e, size_t hash) {{
    for (size_t i = hash & t->mask; ; i = (i + 1) & t->mask) {{
        if (!atomic_load_explicit(&t->slots[i], memory_order_relaxed)) {{
            atomic_store_explicit(&t->slots[i], e, memory_order_release);
            return;
        }}
    }}
}}

/* Lists `e` in both indexes, growing them at half full; the caller holds JABS_LOCK_CLASSES and has
 * already appended `e` to jabs_class_entries. Returns 0 when out of memory. */
static int jabs_class_index_add(jabs_class_entry *e) {{
    jabs_class_index *byname = atomic_load_explicit(&jabs_class_by_name, memory_order_relaxed);
    jabs_class_index *byobject = atomic_load_explicit(&jabs_class_by_object, memory_order_relaxed);
    if (!byname || jabs_class_count * 2 > byname->mask + 1) {{
        size_t size = byname ? (byname->mask + 1) * 2 : 1024;
        while (jabs_class_count * 2 > size) size *= 2;
        jabs_class_index *n1 = (jabs_class_index *)calloc(1, sizeof *n1 + size * sizeof n1->slots[0]);
        jabs_class_index *n2 = (jabs_class_index *)calloc(1, sizeof *n2 + size * sizeof n2->slots[0]);
        if (!n1 || !n2) {{ free(n1); free(n2); return 0; }}
        n1->mask = n2->mask = size - 1;
        for (size_t i = 0; i < jabs_class_count; i++) {{
            jabs_class_index_put(n1, jabs_class_entries[i], jabs_class_hash_name(jabs_class_entries[i]->name));
            jabs_class_index_put(n2, jabs_class_entries[i], jabs_class_hash_object(jabs_class_entries[i]->object));
        }}
        atomic_store_explicit(&jabs_class_by_name, n1, memory_order_release);
        atomic_store_explicit(&jabs_class_by_object, n2, memory_order_release);
        return 1; /* the rebuild listed every entry, `e` included */
    }}
    jabs_class_index_put(byname, e, jabs_class_hash_name(e->name));
    jabs_class_index_put(byobject, e, jabs_class_hash_object(e->object));
    return 1;
}}

/* Listed entry for a name, or NULL. Takes no lock. */
static jabs_class_entry *jabs_class_entry_named(const char *name) {{
    jabs_class_index *t = atomic_load_explicit(&jabs_class_by_name, memory_order_acquire);
    if (!t) return NULL;
    for (size_t i = jabs_class_hash_name(name) & t->mask; ; i = (i + 1) & t->mask) {{
        jabs_class_entry *e = atomic_load_explicit(&t->slots[i], memory_order_acquire);
        if (!e) return NULL;
        if (strcmp(e->name, name) == 0) return e;
    }}
}}

/* The same, for a caller that holds JABS_LOCK_CLASSES (and so sees every entry listed so far). */
static jabs_class_entry *jabs_class_entry_named_locked(const char *name) {{
    return jabs_class_entry_named(name);
}}

/* Record for an internal class name, if the unit compiled one. Throwable.fillInStackTrace asks
 * once per frame, and the 1.7.10 server throws in its tick (UUID.fromString on a tamed animal's
 * owner name), so this is a hash lookup: the table over jabs_records is built on first use and
 * published with one compare-and-swap (a thread that loses the race frees its copy). */
typedef struct {{ size_t mask; const jabs_class *slots[]; }} jabs_record_index;

static const jabs_class *jabs_record_named(const char *name) {{
    static _Atomic(jabs_record_index *) index;
    jabs_record_index *t = atomic_load_explicit(&index, memory_order_acquire);
    if (!t) {{
        size_t n = 0, size = 64;
        while (jabs_records[n]) n++;
        while (size < 2 * n) size *= 2;
        jabs_record_index *built = (jabs_record_index *)calloc(1, sizeof *built + size * sizeof built->slots[0]);
        if (built) {{
            built->mask = size - 1;
            for (size_t r = 0; r < n; r++) {{
                size_t i = jabs_class_hash_name(jabs_records[r]->name) & built->mask;
                while (built->slots[i]) i = (i + 1) & built->mask;
                built->slots[i] = jabs_records[r];
            }}
            jabs_record_index *expected = NULL;
            if (atomic_compare_exchange_strong_explicit(&index, &expected, built, memory_order_acq_rel, memory_order_acquire)) t = built;
            else {{ free(built); t = expected; }}
        }}
    }}
    if (!t) {{ /* out of memory for the table: scan */
        for (const jabs_class *const *r = jabs_records; *r; r++) if (strcmp((*r)->name, name) == 0) return *r;
        return NULL;
    }}
    for (size_t i = jabs_class_hash_name(name) & t->mask; t->slots[i]; i = (i + 1) & t->mask)
        if (strcmp(t->slots[i]->name, name) == 0) return t->slots[i];
    return NULL;
}}

/* Build a String from UTF-8 text (compact Latin-1 when every char fits, else UTF-16). */
jabs_object *jabs_string_from_utf8(const char *text) {{
    size_t n = strlen(text), count = 0;
    uint16_t *units = (uint16_t *)malloc((n + 1) * sizeof(uint16_t));
    if (!units) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
    int latin1 = 1;
    for (size_t i = 0; i < n;) {{
        unsigned char b = (unsigned char)text[i];
        uint32_t cp = b; size_t len = 1;
        if (b >= 0xF0 && i + 3 < n + 1) {{ cp = ((b & 7u) << 18) | ((text[i+1] & 0x3Fu) << 12) | ((text[i+2] & 0x3Fu) << 6) | (text[i+3] & 0x3Fu); len = 4; }}
        else if (b >= 0xE0) {{ cp = ((b & 15u) << 12) | ((text[i+1] & 0x3Fu) << 6) | (text[i+2] & 0x3Fu); len = 3; }}
        else if (b >= 0xC0) {{ cp = ((b & 31u) << 6) | (text[i+1] & 0x3Fu); len = 2; }}
        if (cp > 0xFFFF) {{ cp -= 0x10000; units[count++] = (uint16_t)(0xD800 + (cp >> 10)); units[count++] = (uint16_t)(0xDC00 + (cp & 0x3FF)); latin1 = 0; }}
        else {{ units[count++] = (uint16_t)cp; if (cp > 0xFF) latin1 = 0; }}
        i += len;
    }}
    size_t bytes = latin1 ? count : count * 2;
    jabs_object *value = jabs_new_array((int32_t)bytes, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (!value) {{ free(units); return NULL; }}
    unsigned char *data = (unsigned char *)jabs_array_data((jabs_array *)value);
    for (size_t i = 0; i < count; i++) {{
        if (latin1) data[i] = (unsigned char)units[i];
        else memcpy(data + 2 * i, &units[i], 2); /* UTF-16 in the target's byte order */
    }}
    free(units);
    if ({string_init}() != 0) return NULL;
    jabs_object *s = jabs_new(&{string_record});
    if (!s) return NULL;
    (({string_struct} *)s)->{value} = value;
    (({string_struct} *)s)->{coder} = latin1 ? 0 : 1;
    return s;
}}

/* A String of exactly these UTF-16 code units (what JNI's NewString makes); Latin-1 when every
 * unit fits, as the JDK compacts it. */
jabs_object *jabs_string_from_utf16(const uint16_t *units, size_t count) {{
    if (count > (size_t)INT32_MAX / 2) jabs_not_compiled("a UTF-16 string too long for a Java String (jabs_string_from_utf16)");
    int latin1 = 1;
    for (size_t i = 0; i < count; i++) if (units[i] > 0xFF) {{ latin1 = 0; break; }}
    size_t bytes = latin1 ? count : count * 2;
    jabs_object *value = jabs_new_array((int32_t)bytes, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
    if (!value) return NULL;
    unsigned char *data = (unsigned char *)jabs_array_data((jabs_array *)value);
    for (size_t i = 0; i < count; i++) {{
        if (latin1) data[i] = (unsigned char)units[i];
        else memcpy(data + 2 * i, &units[i], 2);
    }}
    if ({string_init}() != 0) return NULL;
    jabs_object *s = jabs_new(&{string_record});
    if (!s) return NULL;
    (({string_struct} *)s)->{value} = value;
    (({string_struct} *)s)->{coder} = latin1 ? 0 : 1;
    return s;
}}

/* Copy an all-ASCII Latin-1 String into `out`; 0 if it is not one or does not fit. */
int jabs_string_to_ascii(jabs_object *s, char *out, size_t size) {{
    jabs_array *v = (jabs_array *)(({string_struct} *)s)->{value};
    if ((({string_struct} *)s)->{coder} != 0 || !v || (size_t)v->length >= size) return 0;
    memcpy(out, jabs_array_data(v), (size_t)v->length);
    out[v->length] = 0;
    return 1;
}}

static int jabs_is_primitive_name(const char *n) {{
    static const char *const names[] = {{ "boolean", "byte", "char", "short", "int", "long", "float", "double", "void" }};
    for (size_t i = 0; i < sizeof names / sizeof names[0]; i++) if (strcmp(n, names[i]) == 0) return 1;
    return 0;
}}

/* The Class object for an internal name ("java/lang/String"), array descriptor ("[I") or
 * primitive name ("int"). Created once; the same object is returned afterwards. Building one runs
 * Java code (String and Class initialization), so it happens outside the lock; if two threads
 * build the same Class at once, the first one listed is the only one anybody gets. */
jabs_object *jabs_class_object(const char *name) {{
    jabs_class_entry *listed = jabs_class_entry_named(name);
    if (listed) return listed->object;
    if ({class_init}() != 0) return NULL;
    jabs_object *c = jabs_new(&{class_record_name});
    if (!c) return NULL;
    char *binary = (char *)malloc(strlen(name) + 1);
    if (!binary) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
    for (size_t i = 0; ; i++) {{ binary[i] = name[i] == '/' ? '.' : name[i]; if (!name[i]) break; }}
    jabs_object *binary_name = jabs_string_from_utf8(binary);
    free(binary);
    if (!binary_name) return NULL;
    const jabs_class *record = name[0] == '[' ? NULL : jabs_record_named(name);
    (({class_struct} *)c)->{name_field} = binary_name;
    if (jabs_is_primitive_name(name)) {{
        (({class_struct} *)c)->{primitive} = 1;
        (({class_struct} *)c)->{modifiers} = 0x411; /* public final abstract */
    }} else if (name[0] == '[') {{
        (({class_struct} *)c)->{modifiers} = 0x411;
    }} else if (record) {{
        (({class_struct} *)c)->{modifiers} = (uint16_t)jabs_reflect_class_modifiers(name, record->access);
    }}
    if (name[0] == '[') {{
        /* componentType: "[I" -> int, "[Ljava/lang/String;" -> java/lang/String, "[[I" -> [I.
         * Set before the Class is listed, so no thread ever sees an array Class without it. */
        char element[512];
        static const char *const prims[] = {{ "Z", "boolean", "B", "byte", "C", "char", "S", "short", "I", "int", "J", "long", "F", "float", "D", "double" }};
        const char *e = name + 1;
        int named = 0;
        element[0] = 0;
        if (*e == 'L') {{ size_t len = strlen(e) - 2; if (len < sizeof element) {{ memcpy(element, e + 1, len); element[len] = 0; named = 1; }} }}
        else if (*e == '[') {{ if (strlen(e) < sizeof element) {{ strcpy(element, e); named = 1; }} }}
        else {{ for (size_t i = 0; i < 16; i += 2) if (*e == prims[i][0]) {{ strcpy(element, prims[i + 1]); named = 1; }} }}
        if (named) {{
            jabs_object *component = jabs_class_object(element);
            if (!component) return NULL;
            (({class_struct} *)c)->{component} = component;
        }}
    }}
    jabs_class_entry *entry = (jabs_class_entry *)malloc(sizeof *entry);
    char *copy = (char *)malloc(strlen(name) + 1);
    if (!entry || !copy) {{ free(entry); free(copy); jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
    strcpy(copy, name);
    *entry = (jabs_class_entry){{ copy, record, c }};
    jabs_lock(JABS_LOCK_CLASSES);
    listed = jabs_class_entry_named_locked(name);
    if (listed) {{
        /* Another thread listed this Class first: theirs is the one. */
        jabs_unlock(JABS_LOCK_CLASSES);
        free(entry);
        free(copy);
        return listed->object;
    }}
    if (jabs_class_count == jabs_class_capacity) {{
        size_t grown = jabs_class_capacity ? jabs_class_capacity * 2 : 64;
        jabs_class_entry **more = grown > SIZE_MAX / sizeof *more ? NULL : (jabs_class_entry **)realloc(jabs_class_entries, grown * sizeof *more);
        if (!more) {{
            jabs_unlock(JABS_LOCK_CLASSES);
            free(entry);
            free(copy);
            jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
            return NULL;
        }}
        jabs_class_entries = more;
        jabs_class_capacity = grown;
    }}
    jabs_class_entries[jabs_class_count++] = entry;
    if (!jabs_class_index_add(entry)) {{
        jabs_class_count--;
        jabs_unlock(JABS_LOCK_CLASSES);
        free(entry);
        free(copy);
        jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
        return NULL;
    }}
    jabs_set_defining_loader(c, name);
    jabs_unlock(JABS_LOCK_CLASSES);
    return c;
}}

/* Registry entry of a Class object (every Class object comes from jabs_class_object). */
static jabs_class_entry *jabs_class_entry_of(jabs_object *c) {{
    jabs_class_index *t = atomic_load_explicit(&jabs_class_by_object, memory_order_acquire);
    if (!t) return NULL;
    for (size_t i = jabs_class_hash_object(c) & t->mask; ; i = (i + 1) & t->mask) {{
        jabs_class_entry *e = atomic_load_explicit(&t->slots[i], memory_order_acquire);
        if (!e) return NULL;
        if (e->object == c) return e;
    }}
}}

/* Object.getClass. */
jabs_object *jabs_class_of(jabs_object *o) {{
    return jabs_class_object(o->cls == jabs_array_class() ? jabs_array_descriptor((const jabs_array *)o) : o->cls->name);
}}

/* Class.isInstance(o) for non-null o. */
int32_t jabs_class_is_instance(jabs_object *c, jabs_object *o) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || jabs_is_primitive_name(e->name)) return 0;
    if (strcmp(e->name, "java/lang/Object") == 0) return 1;
    if (e->name[0] == '[') {{
        const jabs_class *component = NULL;
        if (e->name[1] == 'L') {{
            char element[512]; size_t len = strlen(e->name) - 3;
            if (len < sizeof element) {{ memcpy(element, e->name + 2, len); element[len] = 0; component = jabs_record_named(element); }}
        }}
        return jabs_is_array_instance(o, e->name, component);
    }}
    if (e->record) return jabs_is_instance(o, e->record);
    jabs_not_compiled_class("Class.isInstance for a class without a compiled record", e->name);
}}

/* Class.isAssignableFrom(other) for non-null other. */
int32_t jabs_class_assignable_from(jabs_object *c, jabs_object *other) {{
    if (c == other) return 1;
    jabs_class_entry *a = jabs_class_entry_of(c), *b = jabs_class_entry_of(other);
    if (!a || !b || jabs_is_primitive_name(a->name) || jabs_is_primitive_name(b->name)) return 0;
    if (strcmp(a->name, "java/lang/Object") == 0) return 1;
    if (b->name[0] == '[') {{
        if (a->name[0] != '[') return strcmp(a->name, "java/lang/Cloneable") == 0 || strcmp(a->name, "java/io/Serializable") == 0;
        jabs_not_compiled_class("Class.isAssignableFrom between two array types", a->name);
    }}
    if (a->name[0] == '[') return 0;
    if (a->record && b->record) return jabs_class_assignable(b->record, a->record);
    jabs_not_compiled_class("Class.isAssignableFrom for a class without a compiled record", a->record ? b->name : a->name);
}}

/* Unsafe.arrayIndexScale0: element size of an array class. */
int32_t jabs_array_scale(jabs_object *c) {{
    jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL;
    if (!e || e->name[0] != '[') {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    switch (e->name[1]) {{
    case 'Z': case 'B': return 1;
    case 'C': case 'S': return 2;
    case 'I': case 'F': return 4;
    case 'J': case 'D': return 8;
    default: return (int32_t)sizeof(jabs_object *);
    }}
}}

/* Unsafe.objectFieldOffset1: byte offset of a declared instance field in our object layout. */
int64_t jabs_field_offset(jabs_object *c, jabs_object *name) {{
    char field[256];
    jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL;
    if (!e || !name || !jabs_string_to_ascii(name, field, sizeof field)) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }}
    for (const jabs_field_offset_entry *f = jabs_field_offsets; f->class_name; f++) {{
        if (strcmp(f->class_name, e->name) == 0 && strcmp(f->field_name, field) == 0) return (int64_t)f->offset;
    }}
    jabs_not_compiled("Unsafe.objectFieldOffset for a field without a compiled layout");
}}

/* Class.getSuperclass: null for Object, interfaces and primitives; Object for arrays. */
jabs_object *jabs_class_superclass(jabs_object *c) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || jabs_is_primitive_name(e->name) || strcmp(e->name, "java/lang/Object") == 0) return NULL;
    if (e->name[0] == '[') return jabs_class_object("java/lang/Object");
    if (!e->record) jabs_not_compiled_class("Class.getSuperclass for a class without a compiled record", e->name);
    if (e->record->is_interface) return NULL;
    return jabs_class_object(e->record->super ? e->record->super->name : "java/lang/Object");
}}
"#,
        string_init = init_symbol(STRING),
        string_record = class_record(STRING),
        class_init = init_symbol(CLASS),
        class_record_name = class_record(CLASS),
        value = instance_field("value"),
        coder = instance_field("coder"),
        name_field = instance_field("name"),
        primitive = instance_field("primitive"),
        modifiers = instance_field("modifiers"),
        component = instance_field("componentType"),
    ));
    out.push_str(&format!(
        r#"
/* The String intern table (String.intern and every literal): open addressing on contents.
 * Every thread shares it; it is read and changed only under JABS_LOCK_STRINGS.
 * Runtime-interned strings are weak to GC: unmarked slots become tombstones during STW. */
#define JABS_INTERN_TOMBSTONE ((jabs_object *)(intptr_t)1)
static jabs_object **jabs_interned;
static size_t jabs_interned_count, jabs_interned_tombstones, jabs_interned_capacity;

static uint64_t jabs_string_contents_hash(jabs_object *s) {{
    jabs_array *v = (jabs_array *)(({string_struct} *)s)->{value};
    uint64_t h = 1469598103934665603ull ^ (uint64_t)(({string_struct} *)s)->{coder};
    const unsigned char *b = (const unsigned char *)jabs_array_data(v);
    for (int32_t i = 0; i < v->length; i++) h = (h ^ b[i]) * 1099511628211ull;
    return h;
}}

static int jabs_string_contents_equal(jabs_object *a, jabs_object *b) {{
    jabs_array *x = (jabs_array *)(({string_struct} *)a)->{value}, *y = (jabs_array *)(({string_struct} *)b)->{value};
    return (({string_struct} *)a)->{coder} == (({string_struct} *)b)->{coder} && x->length == y->length
        && memcmp(jabs_array_data(x), jabs_array_data(y), (size_t)x->length) == 0;
}}

/* The canonical String with these contents: `s` itself if none was interned yet. */
jabs_object *jabs_string_intern(jabs_object *s) {{
    if (!s) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    jabs_lock(JABS_LOCK_STRINGS);
    if (2 * (jabs_interned_count + jabs_interned_tombstones + 1) > jabs_interned_capacity) {{
        size_t grown = jabs_interned_capacity ? jabs_interned_capacity * 2 : 1024;
        if (jabs_interned_capacity && 4 * (jabs_interned_count + 1) <= jabs_interned_capacity) grown = jabs_interned_capacity;
        jabs_object **table = (jabs_object **)calloc(grown, sizeof *table);
        if (!table) {{ jabs_unlock(JABS_LOCK_STRINGS); jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
        for (size_t i = 0; i < jabs_interned_capacity; i++) {{
            jabs_object *e = jabs_interned[i];
            if (!e || e == JABS_INTERN_TOMBSTONE) continue;
            size_t j = (size_t)jabs_string_contents_hash(e) & (grown - 1);
            while (table[j]) j = (j + 1) & (grown - 1);
            table[j] = e;
        }}
        free(jabs_interned);
        jabs_interned = table;
        jabs_interned_capacity = grown;
        jabs_interned_tombstones = 0;
    }}
    size_t j = (size_t)jabs_string_contents_hash(s) & (jabs_interned_capacity - 1);
    size_t first_tomb = SIZE_MAX;
    for (; jabs_interned[j]; j = (j + 1) & (jabs_interned_capacity - 1)) {{
        jabs_object *e = jabs_interned[j];
        if (e == JABS_INTERN_TOMBSTONE) {{ if (first_tomb == SIZE_MAX) first_tomb = j; continue; }}
        if (jabs_string_contents_equal(e, s)) {{
            jabs_unlock(JABS_LOCK_STRINGS);
            return e;
        }}
    }}
    if (first_tomb != SIZE_MAX) {{
        jabs_interned[first_tomb] = s;
        jabs_interned_tombstones--;
    }} else {{
        jabs_interned[j] = s;
    }}
    jabs_interned_count++;
    jabs_unlock(JABS_LOCK_STRINGS);
    return s;
}}

void jabs_gc_sweep_interned(jabs_gc_is_marked_fn is_marked) {{
    for (size_t i = 0; i < jabs_interned_capacity; i++) {{
        jabs_object *e = jabs_interned[i];
        if (e && e != JABS_INTERN_TOMBSTONE && !is_marked(e)) {{
            jabs_interned[i] = JABS_INTERN_TOMBSTONE;
            jabs_interned_count--;
            jabs_interned_tombstones++;
        }}
    }}
}}

void jabs_gc_visit_reflect_roots(jabs_gc_root_fn visit, void *ctx) {{
    for (size_t i = 0; i < jabs_class_count; i++) {{
        if (jabs_class_entries[i] && jabs_class_entries[i]->object)
            visit(jabs_class_entries[i]->object, ctx);
    }}
    jabs_gc_visit_rproxy_roots(visit, ctx);
}}
"#,
        string_struct = object_struct(STRING),
        value = instance_field("value"),
        coder = instance_field("coder"),
    ));
    out.push_str(&io_accessor_functions(unit, recorded));
    out.push_str(&app_loader_functions(unit, recorded, app_classes));
    out.push_str(&module_function(unit, inits));
    out.push_str(&system_properties_functions(
        unit, java_home, jab, inits, methods,
    ));
    out.push_str(&for_name_function(unit, inits));
    out.push_str(&var_handle_functions(unit, recorded, inits));
    out.push_str(&stack_functions(unit, methods, hidden));
    out.push_str(&stack_trace_functions(unit, recorded));
    out.push_str(&new_instance_function(unit, recorded, methods));
    out.push_str(&main_thread_function(unit, recorded));
    out.push_str(&reflect::functions(
        unit,
        &reflect::MetadataInputs {
            recorded,
            interfaces,
            methods,
            inits,
            proxies,
        },
    ));
    out.push('\n');
}

/// The app class loader's runtime side: which classes it defines, creating it, class lookup by
/// name and the embedded resources. Without jabs.rt.AppLoader in the unit, every class belongs to
/// the boot loader (null), as before.
fn app_loader_functions(
    unit: &Unit,
    recorded: &[&ClassFile],
    app_classes: &BTreeSet<String>,
) -> String {
    let mut out = String::new();
    let class_loader_field = unit.get(CLASS).is_some_and(|c| {
        c.fields.iter().any(|f| {
            f.name == "classLoader"
                && f.descriptor == "Ljava/lang/ClassLoader;"
                && f.access & ACC_STATIC == 0
        })
    });
    let recorded_set: HashSet<&str> = recorded.iter().map(|r| r.name.as_str()).collect();
    let have_loader = recorded_set.contains(APP_LOADER)
        && unit.get(APP_LOADER).is_some_and(|c| {
            c.fields
                .iter()
                .any(|f| f.name == "INSTANCE" && f.access & ACC_STATIC != 0)
        });
    // Sorted names, for bsearch.
    let names: Vec<&String> = app_classes
        .iter()
        .filter(|n| recorded_set.contains(n.as_str()))
        .collect();
    let _ = writeln!(out, "static const char *const jabs_app_class_names[] = {{");
    for n in &names {
        let _ = writeln!(out, "    \"{}\",", c_escape(n));
    }
    let _ = writeln!(out, "    NULL\n}};");
    out.push_str(
        "/* Whether the app class loader defines this class. */\n\
static int jabs_compare_names(const void *a, const void *b) { return strcmp(*(const char *const *)a, *(const char *const *)b); }\n",
    );
    let _ = writeln!(
        out,
        "static int jabs_is_app_class(const char *name) {{ return bsearch(&name, jabs_app_class_names, {}, sizeof(char *), jabs_compare_names) != NULL; }}",
        names.len()
    );
    if have_loader && class_loader_field {
        let instance = field_symbol(APP_LOADER, "INSTANCE");
        let _ = write!(
            out,
            r#"/* Set once, under JABS_LOCK_CLASSES together with the loader of every Class listed so far. */
static _Atomic(jabs_object *) jabs_app_loader_object;
extern jabs_object *{instance};

/* The app class loader (jabs.rt.AppLoader.INSTANCE), created on first use. Classes created
 * before it existed get it as their loader once it does. NULL while it is being created on this
 * thread; another thread asking meanwhile waits in AppLoader's initialization lock. */
jabs_object *jabs_app_loader(void) {{
    jabs_object *published = atomic_load_explicit(&jabs_app_loader_object, memory_order_acquire);
    if (published) return published;
    static _Thread_local bool creating;
    if (creating) return NULL;
    creating = true;
    int failed = {init}() != 0;
    creating = false;
    if (failed) return NULL;
    /* Still NULL when this thread is inside AppLoader's own initialization by another path. */
    jabs_object *loader = {instance};
    if (!loader) return NULL;
    jabs_lock(JABS_LOCK_CLASSES);
    if (!atomic_load_explicit(&jabs_app_loader_object, memory_order_relaxed)) {{
        atomic_store_explicit(&jabs_app_loader_object, loader, memory_order_release);
        for (size_t i = 0; i < jabs_class_count; i++) jabs_set_defining_loader(jabs_class_entries[i]->object, jabs_class_entries[i]->name);
    }}
    loader = atomic_load_explicit(&jabs_app_loader_object, memory_order_relaxed);
    jabs_unlock(JABS_LOCK_CLASSES);
    return loader;
}}

/* The app class loader if it exists yet, without creating it. */
jabs_object *jabs_app_loader_created(void) {{ return atomic_load_explicit(&jabs_app_loader_object, memory_order_acquire); }}

/* Class.getClassLoader: the app loader for classes from the inputs, null (boot) for the rest.
 * The caller holds JABS_LOCK_CLASSES. */
static void jabs_set_defining_loader(jabs_object *c, const char *name) {{
    jabs_object *loader = atomic_load_explicit(&jabs_app_loader_object, memory_order_relaxed);
    if (loader && name[0] != '[' && jabs_is_app_class(name)) (({class_struct} *)c)->{field} = loader;
}}
"#,
            init = init_symbol(APP_LOADER),
            class_struct = object_struct(CLASS),
            field = instance_field("classLoader"),
        );
    } else {
        out.push_str(
            "/* No jabs.rt.AppLoader in the unit: every class belongs to the boot loader. */\n\
jabs_object *jabs_app_loader(void) { return NULL; }\n\
jabs_object *jabs_app_loader_created(void) { return NULL; }\n\
static void jabs_set_defining_loader(jabs_object *c, const char *name) { (void)c; (void)name; }\n",
        );
    }
    out.push_str(
        r#"
/* AppLoader.loadClass: the Class for a binary name if the unit compiled that class, else NULL. */
jabs_object *jabs_class_for_binary_name(jabs_object *binary) {
    char name[1024];
    if (!binary) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    if (!jabs_string_to_ascii(binary, name, sizeof name)) return NULL;
    for (char *p = name; *p; p++) if (*p == '.') *p = '/';
    if (name[0] != '[' && !jabs_record_named(name)) return NULL;
    return jabs_class_object(name);
}

/* ClassLoader.findLoadedClass0: the class this loader defined, or null. In a JABS program the app
 * class loader (jabs.rt.AppLoader) defined the input classes and the boot loader the rest; the
 * JDK's own loaders (the builtin app and platform loaders, which code reaches while the VM is
 * not booted) defined none, so they go on to their parents and the boot loader, as on the JVM. */
jabs_object *jabs_find_loaded_class(jabs_object *loader, jabs_object *binary) {
    char name[1024];
    if (!loader || !binary) return NULL;
    if (loader != jabs_app_loader_created()) return NULL;
    if (!jabs_string_to_ascii(binary, name, sizeof name)) return NULL;
    for (char *p = name; *p; p++) if (*p == '.') *p = '/';
    if (name[0] == '[' || !jabs_is_app_class(name) || !jabs_record_named(name)) return NULL;
    return jabs_class_object(name);
}

/* How many embedded resources have this name. */
int32_t jabs_resource_count(jabs_object *resource) {
    char name[1024];
    if (!resource) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }
    if (!jabs_string_to_ascii(resource, name, sizeof name)) return 0;
    int32_t count = 0;
    for (const jabs_resource *r = jabs_resources; r->name; r++) count += strcmp(r->name, name) == 0;
    return count;
}

/* A new byte[] with the index-th embedded resource of this name, or NULL. */
jabs_object *jabs_resource_bytes(jabs_object *resource, int32_t index) {
    char name[1024];
    if (!resource) { jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }
    if (!jabs_string_to_ascii(resource, name, sizeof name)) return NULL;
    for (const jabs_resource *r = jabs_resources; r->name; r++) {
        if (strcmp(r->name, name) != 0 || index-- != 0) continue;
        jabs_object *bytes = jabs_new_array((int32_t)r->length, 1, JABS_COMPONENT_PRIMITIVE, NULL, "[B", NULL);
        if (bytes) memcpy(jabs_array_data((jabs_array *)bytes), r->data, r->length);
        return bytes;
    }
    return NULL;
}
"#,
    );
    out
}

/// The values the VM hands `SystemProps` (`Raw.platformProperties` / `Raw.vmProperties`), and
/// `jabs_vm_startup()`, which runs `System.initPhase1` as the JVM does before `main`. Platform
/// indexes come from `Raw`'s own constants, so they follow the compiled JDK.
fn system_properties_functions(
    unit: &Unit,
    java_home: Option<&str>,
    jab: bool,
    inits: &BTreeSet<String>,
    methods: &BTreeSet<MethodKey>,
) -> String {
    let mut out = String::new();
    let raw = unit.get("jdk/internal/util/SystemProps$Raw");
    let constant = |name: &str| -> Option<i32> {
        let raw = raw?;
        let f = raw.fields.iter().find(|f| f.name == name)?;
        match raw.pool.get(f.constant_value? as usize)? {
            Constant::Integer(v) => Some(*v),
            _ => None,
        }
    };
    let string_array = format!(
        "jabs_new_array({{n}}, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{}, \"[Ljava/lang/String;\", NULL)",
        class_record(STRING)
    );
    out.push_str(
        "/* Store a UTF-8 string into a String[] slot; 0 when out of memory. */\n\
static int jabs_put_utf8(jabs_object *array, int32_t index, const char *value) {\n\
    jabs_object *s = jabs_string_from_utf8(value);\n\
    if (!s) return 0;\n\
    ((jabs_object **)jabs_array_data((jabs_array *)array))[index] = s;\n\
    return 1;\n\
}\n",
    );
    match constant("FIXED_LENGTH") {
        Some(length) => {
            // (Raw index constant, host value) pairs; format and display locale share the host locale.
            let pairs = [
                ("_display_country_NDX", "JABS_PLATFORM_COUNTRY"),
                ("_display_language_NDX", "JABS_PLATFORM_LANGUAGE"),
                ("_format_country_NDX", "JABS_PLATFORM_COUNTRY"),
                ("_format_language_NDX", "JABS_PLATFORM_LANGUAGE"),
                ("_file_separator_NDX", "JABS_PLATFORM_FILE_SEPARATOR"),
                ("_java_io_tmpdir_NDX", "JABS_PLATFORM_TMPDIR"),
                ("_line_separator_NDX", "JABS_PLATFORM_LINE_SEPARATOR"),
                ("_native_encoding_NDX", "JABS_PLATFORM_NATIVE_ENCODING"),
                ("_os_arch_NDX", "JABS_PLATFORM_OS_ARCH"),
                ("_os_name_NDX", "JABS_PLATFORM_OS_NAME"),
                ("_os_version_NDX", "JABS_PLATFORM_OS_VERSION"),
                ("_path_separator_NDX", "JABS_PLATFORM_PATH_SEPARATOR"),
                ("_sun_arch_data_model_NDX", "JABS_PLATFORM_DATA_MODEL"),
                ("_sun_cpu_endian_NDX", "JABS_PLATFORM_ENDIAN"),
                ("_sun_io_unicode_encoding_NDX", "JABS_PLATFORM_UNICODE_ENCODING"),
                ("_sun_jnu_encoding_NDX", "JABS_PLATFORM_JNU_ENCODING"),
                ("_user_dir_NDX", "JABS_PLATFORM_USER_DIR"),
                ("_user_home_NDX", "JABS_PLATFORM_USER_HOME"),
                ("_user_name_NDX", "JABS_PLATFORM_USER_NAME"),
            ];
            let _ = writeln!(
                out,
                "\n/* Raw.platformProperties: host values at the indexes SystemProps$Raw defines. */\njabs_object *jabs_platform_properties(void) {{"
            );
            let _ = writeln!(out, "    jabs_object *a = {};", string_array.replace("{n}", &length.to_string()));
            let _ = writeln!(out, "    if (!a) return NULL;");
            let _ = writeln!(out, "    const char *v;");
            for (name, which) in pairs {
                if let Some(index) = constant(name).filter(|i| (0..length).contains(i)) {
                    let _ = writeln!(
                        out,
                        "    if ((v = jabs_platform_value({which})) && !jabs_put_utf8(a, {index}, v)) return NULL;"
                    );
                }
            }
            let _ = writeln!(out, "    return a;\n}}");
        }
        None => out.push_str(
            "jabs_object *jabs_platform_properties(void) { jabs_not_compiled(\"SystemProps$Raw has no FIXED_LENGTH\"); }\n",
        ),
    }
    // The class file version of Object gives the Java feature release of the compiled JDK.
    let feature = unit.get(OBJECT).map_or(0, |o| o.major.saturating_sub(44));
    // With a .jab both come from it at run time (runtime/jabs_jab.c): java.home is its
    // jabs-home/ (or the build JDK in a recording build) and the class path is the .jab itself.
    let home = match (jab, java_home) {
        (true, _) => "jabs_jab_java_home()".to_string(),
        (false, Some(h)) => format!("\"{}\"", c_escape(h)),
        (false, None) => "jabs_platform_value(JABS_PLATFORM_EXE_DIR)".to_string(),
    };
    let class_path = if jab {
        "jabs_jab_class_path()".to_string()
    } else {
        "\"\"".to_string()
    };
    let vm: Vec<(&str, String)> = vec![
        ("java.home", home),
        (
            "java.vm.specification.name",
            "\"Java Virtual Machine Specification\"".into(),
        ),
        (
            "java.vm.specification.vendor",
            "\"Oracle Corporation\"".into(),
        ),
        ("java.vm.specification.version", format!("\"{feature}\"")),
        ("java.vm.name", "\"JABS native\"".into()),
        ("java.vm.vendor", "\"JABS\"".into()),
        (
            "java.vm.version",
            format!("\"{}\"", env!("CARGO_PKG_VERSION")),
        ),
        ("java.vm.info", "\"compiled to native code\"".into()),
        ("jdk.debug", "\"release\"".into()),
        ("java.class.path", class_path),
        ("java.library.path", "\"\"".into()),
        ("sun.boot.library.path", "\"\"".into()),
        ("sun.java.launcher", "\"JABS\"".into()),
    ];
    let _ = writeln!(
        out,
        "\n/* Raw.vmProperties: key, value pairs the VM defines. */\njabs_object *jabs_vm_properties(void) {{"
    );
    // Command-line -D properties come after the VM's own, so a later one with the same key wins,
    // as SystemProps applies them in order.
    let _ = writeln!(
        out,
        "    if (jabs_define_count < 0 || jabs_define_count > (INT32_MAX - {fixed}) / 2) return NULL;",
        fixed = vm.len() * 2
    );
    let _ = writeln!(
        out,
        "    jabs_object *a = {};",
        string_array.replace("{n}", &format!("{} + 2 * jabs_define_count", vm.len() * 2))
    );
    let _ = writeln!(out, "    if (!a) return NULL;");
    let _ = writeln!(out, "    const char *v;");
    for (i, (key, value)) in vm.iter().enumerate() {
        let _ = writeln!(
            out,
            "    if (!jabs_put_utf8(a, {}, \"{key}\") || ((v = {value}) && !jabs_put_utf8(a, {}, v))) return NULL;",
            2 * i,
            2 * i + 1
        );
    }
    let _ = writeln!(
        out,
        "    for (int i = 0; i < jabs_define_count; i++) {{
        const char *d = jabs_defines[i], *eq = strchr(d, '=');
        size_t key_length = eq ? (size_t)(eq - d) : strlen(d);
        char *key = (char *)malloc(key_length + 1);
        if (!key) return NULL;
        memcpy(key, d, key_length);
        key[key_length] = 0;
        int ok = jabs_put_utf8(a, {fixed} + 2 * i, key) && jabs_put_utf8(a, {fixed} + 2 * i + 1, eq ? eq + 1 : \"\");
        free(key);
        if (!ok) return NULL;
    }}",
        fixed = vm.len() * 2
    );
    let _ = writeln!(out, "    return a;\n}}");
    // Startup, when the unit has it.
    // Only whole programs (with the app loader) compile the startup in.
    let has_startup = methods.contains(&(
        SYSTEM.to_string(),
        "initPhase1".to_string(),
        "()V".to_string(),
    ));
    out.push_str("\n/* What the JVM does before main: System.initPhase1 (properties, standard streams). */\n");
    if has_startup {
        let _ = writeln!(out, "int jabs_vm_startup(void) {{");
        // The .jab first: it is the class path and java.home, and a bad one stops here.
        out.push_str("    if (jabs_jab_start() != 0) return -1;\n");
        // The VM's start for RuntimeMXBean (start time, uptime), before any Java code runs.
        out.push_str("    jabs_platform_record_start();\n");
        out.push_str("    jabs_on_not_compiled = jabs_print_java_stack;\n");
        for class in VM_PREINITIALIZED.iter().filter(|c| inits.contains(**c)) {
            let _ = writeln!(out, "    if ({}() != 0) return -1;", init_symbol(class));
            if *class == "jdk/internal/misc/UnsafeConstants" {
                // Values HotSpot injects after <clinit> set its placeholders.
                for (field, value) in [
                    ("ADDRESS_SIZE0", "(int32_t)sizeof(void *)"),
                    ("PAGE_SIZE", "4096"),
                    (
                        "BIG_ENDIAN",
                        "(*(const uint8_t *)&(const uint16_t){ 1 } == 0)",
                    ),
                    ("UNALIGNED_ACCESS", "JABS_UNALIGNED_ACCESS"),
                    ("DATA_CACHE_LINE_FLUSH_SIZE", "0"),
                ] {
                    if unit
                        .get(class)
                        .is_some_and(|c| c.fields.iter().any(|f| f.name == field))
                    {
                        let _ = writeln!(out, "    {} = {value};", field_symbol(class, field));
                    }
                }
            }
        }
        let _ = writeln!(
            out,
            "    {init}();",
            init = symbol(SYSTEM, "initPhase1", "()V")
        );
        out.push_str("    if (jabs_pending) return -1;\n");
        if inits.contains(APP_LOADER)
            && inits.contains("java/lang/ClassLoader")
            && unit.get("java/lang/ClassLoader").is_some_and(|c| {
                c.fields
                    .iter()
                    .any(|f| f.name == "scl" && f.access & ACC_STATIC != 0)
            })
        {
            let _ = writeln!(
                out,
                "    if ({cl_init}() != 0) return -1;\n\
                 \x20   {{ jabs_object *scl = jabs_app_loader(); if (jabs_pending) return -1; if (scl) {scl_sym} = scl; }}",
                cl_init = init_symbol("java/lang/ClassLoader"),
                scl_sym = field_symbol("java/lang/ClassLoader", "scl"),
            );
            let vm_init_level = (
                "jdk/internal/misc/VM".to_string(),
                "initLevel".to_string(),
                "(I)V".to_string(),
            );
            if methods.contains(&vm_init_level) {
                let _ = writeln!(
                    out,
                    "    {}(4);",
                    symbol("jdk/internal/misc/VM", "initLevel", "(I)V")
                );
            }
        }
        out.push_str("    return jabs_pending ? -1 : 0;\n}\n");
    } else {
        out.push_str(
            "int jabs_vm_startup(void) { jabs_on_not_compiled = jabs_print_java_stack; return 0; }\n",
        );
    }
    out
}

/// `Class.forName`: compiled classes by name, initialized on request. Every class of the unit is
/// listed, so a class that exists but was not compiled into the program stops with a diagnostic
/// instead of a false ClassNotFoundException.
fn for_name_function(unit: &Unit, inits: &BTreeSet<String>) -> String {
    let mut out = String::new();
    let mut known: Vec<&str> = unit.classes.keys().copied().collect();
    known.sort_unstable();
    let _ = writeln!(out, "static const char *const jabs_unit_class_names[] = {{");
    for n in &known {
        let _ = writeln!(out, "    \"{}\",", c_escape(n));
    }
    let _ = writeln!(out, "    NULL\n}};");
    for n in inits {
        let _ = writeln!(out, "int {}(void);", init_symbol(n));
    }
    let _ = writeln!(
        out,
        "typedef struct {{ const char *name; int (*init)(void); }} jabs_init_entry;\nstatic const jabs_init_entry jabs_init_functions[] = {{"
    );
    for n in inits {
        let _ = writeln!(out, "    {{ \"{}\", {} }},", c_escape(n), init_symbol(n));
    }
    let _ = writeln!(out, "    {{ NULL, NULL }}\n}};");
    let _ = write!(
        out,
        r#"static int jabs_compare_init(const void *key, const void *entry) {{ return strcmp((const char *)key, ((const jabs_init_entry *)entry)->name); }}

/* Unsafe.ensureClassInitialized0: run the class's initialization (JVMS 5.5) if it has not run. */
void jabs_ensure_initialized(jabs_object *c) {{
    jabs_class_entry *e = c ? jabs_class_entry_of(c) : NULL;
    if (!e) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    if (e->name[0] == '[' || jabs_is_primitive_name(e->name)) return;
    const jabs_init_entry *i = (const jabs_init_entry *)bsearch(e->name, jabs_init_functions, {init_count}, sizeof *i, jabs_compare_init);
    if (!i) jabs_not_compiled_class("class initialization requested through Unsafe", e->name);
    i->init();
}}

/* java.lang.reflect.Array.newArray(component, length). */
jabs_object *jabs_reflect_new_array(jabs_object *component, int32_t length) {{
    jabs_class_entry *e = component ? jabs_class_entry_of(component) : NULL;
    if (!e) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (length < 0) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NEGATIVE_ARRAY_SIZE); return NULL; }}
    static const char *const prims[] = {{ "boolean", "Z", "byte", "B", "char", "C", "short", "S", "int", "I", "long", "J", "float", "F", "double", "D" }};
    static const size_t sizes[] = {{ 1, 1, 2, 2, 4, 8, 4, 8 }};
    char descriptor[1100];
    for (size_t i = 0; i < 16; i += 2) {{
        if (strcmp(e->name, prims[i]) == 0) {{
            snprintf(descriptor, sizeof descriptor, "[%s", prims[i + 1]);
            jabs_object *c = jabs_class_object(descriptor);
            if (!c) return NULL;
            return jabs_new_array(length, sizes[i / 2], JABS_COMPONENT_PRIMITIVE, NULL, jabs_class_entry_of(c)->name, NULL);
        }}
    }}
    if (strcmp(e->name, "void") == 0) jabs_not_compiled("Array.newArray(void.class) (IllegalArgumentException)");
    if (e->name[0] == '[') snprintf(descriptor, sizeof descriptor, "[%s", e->name);
    else snprintf(descriptor, sizeof descriptor, "[L%s;", e->name);
    jabs_object *c = jabs_class_object(descriptor);
    if (!c) return NULL;
    const char *stable = jabs_class_entry_of(c)->name;
    /* The element checks compiled anewarray uses: any for Object, the class, or the array type. */
    if (strcmp(e->name, "java/lang/Object") == 0)
        return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_ANY, NULL, stable, NULL);
    if (e->name[0] == '[') {{
        /* Rows of references are covariant: keep the record of a one-level row's element class. */
        const char *row = jabs_class_entry_of(component)->name;
        const jabs_class *element = NULL;
        size_t row_length = strlen(row);
        if (row[1] == 'L' && row_length > 3 && row_length < 1024) {{
            char inner[1024];
            memcpy(inner, row + 2, row_length - 3);
            inner[row_length - 3] = 0;
            element = jabs_record_named(inner);
        }}
        return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_ARRAY, element, stable, row);
    }}
    /* Interfaces too, as compiled anewarray does: jabs_is_instance answers for interface
     * records (AnnotationParser makes Annotation[] and arrays of annotation types this way). */
    if (e->record)
        return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_CLASS, e->record, stable, NULL);
    return jabs_new_array(length, sizeof(jabs_object *), JABS_COMPONENT_UNCHECKED, NULL, stable, NULL);
}}

/* Class.forName0(name, initialize, loader, caller). */
jabs_object *jabs_class_for_name(jabs_object *binary, int32_t initialize, jabs_object *loader) {{
    char name[1024];
    if (!binary) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (!jabs_string_to_ascii(binary, name, sizeof name)) {{ jabs_pending = jabs_vm_throwable(JABS_VM_CLASS_NOT_FOUND); return NULL; }}
    for (char *p = name; *p; p++) if (*p == '.') *p = '/';
    if (name[0] == '[') return jabs_class_object(name);
    const char *key = name;
    int known = bsearch(&key, jabs_unit_class_names, {known_count}, sizeof(char *), jabs_compare_names) != NULL;
    /* The boot loader (null) does not see application classes. */
    if (!known || (!loader && jabs_is_app_class(name))) {{ jabs_pending = jabs_vm_throwable(JABS_VM_CLASS_NOT_FOUND); return NULL; }}
    if (!jabs_record_named(name)) {{
        char message[1100];
        snprintf(message, sizeof message, "Class.forName(\"%s\") (the class was not compiled into this program)", name);
        jabs_not_compiled(message);
    }}
    if (initialize) {{
        const jabs_init_entry *e = (const jabs_init_entry *)bsearch(name, jabs_init_functions, {init_count}, sizeof *e, jabs_compare_init);
        if (!e) {{
            char message[1100];
            snprintf(message, sizeof message, "Class.forName(\"%s\", true, ...) (its initialization was not compiled)", name);
            jabs_not_compiled(message);
        }}
        if (e->init() != 0) return NULL;
    }}
    return jabs_class_object(name);
}}

/* ClassLoader.findBootstrapClass(name): the boot loader's class of that binary name without
 * initializing it, or null when it has none. Callers probe candidate names (resource bundles try
 * FormatData_en_US and its fallbacks), so a missing class is null, not an exception, and the
 * application's classes are invisible to the boot loader. A JDK class in the unit that was not
 * compiled in stops instead of answering null, which would quietly change what the program does. */
jabs_object *jabs_find_bootstrap_class(jabs_object *binary) {{
    char name[1024];
    if (!binary) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (!jabs_string_to_ascii(binary, name, sizeof name)) return NULL;
    for (char *p = name; *p; p++) if (*p == '.') *p = '/';
    if (name[0] == '[') return jabs_class_object(name);
    const char *key = name;
    if (!bsearch(&key, jabs_unit_class_names, {known_count}, sizeof(char *), jabs_compare_names)) return NULL;
    if (jabs_is_app_class(name)) return NULL;
    if (!jabs_record_named(name)) {{
        char message[1100];
        snprintf(message, sizeof message, "ClassLoader.findBootstrapClass(\"%s\") (the class was not compiled into this program)", name);
        jabs_not_compiled(message);
    }}
    return jabs_class_object(name);
}}
"#,
        known_count = known.len(),
        init_count = inits.len(),
    );
    out
}

/// `Class.newInstance`: allocate and run the no-arg constructor of a compiled concrete class.
fn new_instance_function(
    unit: &Unit,
    recorded: &[&ClassFile],
    methods: &BTreeSet<MethodKey>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "typedef struct {{ const char *name; const jabs_class *record; void (*ctor)(jabs_object *); }} jabs_ctor_entry;\nstatic const jabs_ctor_entry jabs_ctors[] = {{"
    );
    let mut rows: Vec<&ClassFile> = recorded
        .iter()
        .copied()
        .filter(|c| c.access & (ACC_INTERFACE | ACC_ABSTRACT) == 0)
        .filter(|c| {
            c.methods
                .iter()
                .any(|m| m.name == "<init>" && m.descriptor == "()V")
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    for c in &rows {
        let key = (c.name.clone(), "<init>".to_string(), "()V".to_string());
        let ctor = if methods.contains(&key) {
            format!(
                "(void (*)(jabs_object *)){}",
                symbol(&c.name, "<init>", "()V")
            )
        } else {
            "NULL".to_string()
        };
        let _ = writeln!(
            out,
            "    {{ \"{}\", &{}, {ctor} }},",
            c_escape(&c.name),
            class_record(&c.name)
        );
    }
    let _ = writeln!(out, "    {{ NULL, NULL, NULL }}\n}};");
    let _ = unit;
    let _ = write!(
        out,
        r#"static int jabs_compare_ctor(const void *key, const void *entry) {{ return strcmp((const char *)key, ((const jabs_ctor_entry *)entry)->name); }}

/* Class.newInstance (replacement): InstantiationException for primitives, arrays, interfaces,
 * abstract classes and classes without a no-arg constructor, as on the JVM. */
jabs_object *jabs_class_new_instance(jabs_object *c) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || e->name[0] == '[' || jabs_is_primitive_name(e->name)) {{ jabs_pending = jabs_vm_throwable(JABS_VM_INSTANTIATION); return NULL; }}
    const jabs_ctor_entry *k = (const jabs_ctor_entry *)bsearch(e->name, jabs_ctors, {count}, sizeof *k, jabs_compare_ctor);
    if (!k) {{
        if (!e->record) jabs_not_compiled("Class.newInstance for a class outside the unit");
        jabs_pending = jabs_vm_throwable(JABS_VM_INSTANTIATION);
        return NULL;
    }}
    if (!k->ctor) {{
        char message[1100];
        snprintf(message, sizeof message, "Class.newInstance of %s (its constructor was not compiled into this program)", e->name);
        jabs_not_compiled(message);
    }}
    const jabs_init_entry *i = (const jabs_init_entry *)bsearch(e->name, jabs_init_functions, sizeof jabs_init_functions / sizeof *jabs_init_functions - 1, sizeof *i, jabs_compare_init);
    if (i && i->init() != 0) return NULL;
    jabs_object *o = jabs_new(k->record);
    if (!o) return NULL;
    k->ctor(o);
    return jabs_pending ? NULL : o;
}}
"#,
        count = rows.len(),
    );
    out
}

/// Stack walking: which compiled method each native frame belongs to. Every emitted method's
/// function start is listed with its class; frames whose function is not listed (runtime C
/// helpers, class initialization wrappers) are not Java frames and are skipped.
fn stack_functions(
    unit: &Unit,
    methods: &BTreeSet<MethodKey>,
    hidden: &BTreeSet<String>,
) -> String {
    let mut out = String::from(
        "/* hidden: left out of stack traces, as the JVM leaves out hidden classes' frames. */\ntypedef struct { const void *start; const char *class_name; const char *name; const char *descriptor; int hidden; uint32_t flags; } jabs_frame_entry;\nstatic jabs_frame_entry jabs_frames[] = {\n",
    );
    for (class, name, descriptor) in methods {
        let _ = writeln!(
            out,
            "    {{ (const void *)&{}, \"{}\", \"{}\", \"{}\", {}, 0x{:x}u }},",
            symbol(class, name, descriptor),
            c_escape(class),
            c_escape(name),
            c_escape(descriptor),
            i32::from(hidden.contains(class)),
            stack::method_flags(unit, class, name, descriptor, hidden.contains(class)),
        );
    }
    // Runtime helpers that stand in for a Java method keep hidden = 1 and flags = 0: fillInStackTrace
    // and StackWalker both skip them because the Java wrapper already has its own frame.
    let _ = writeln!(
        out,
        "    {{ (const void *)&jabs_class_new_instance, \"java/lang/Class\", \"newInstance\", \"()Ljava/lang/Object;\", 1, 0u }},\n    {{ NULL, NULL, NULL, NULL, 0, 0 }}\n}};",
    );
    out.push_str(
        r#"static int jabs_compare_frames(const void *a, const void *b) {
    uintptr_t x = (uintptr_t)((const jabs_frame_entry *)a)->start, y = (uintptr_t)((const jabs_frame_entry *)b)->start;
    return x < y ? -1 : x > y;
}

/* The compiled method whose function starts at `start`, or NULL. */
static const jabs_frame_entry *jabs_frame_of(const void *start) {
    /* 0 until sorted; published with release after the sort, so a reader that sees the count
     * also sees the sorted table. */
    static _Atomic size_t sorted;
    size_t count = atomic_load_explicit(&sorted, memory_order_acquire);
    if (!count) {
        jabs_lock(JABS_LOCK_FRAMES);
        count = atomic_load_explicit(&sorted, memory_order_relaxed);
        if (!count) {
            size_t cap = sizeof jabs_frames / sizeof *jabs_frames;
            while (count < cap && jabs_frames[count].start) count++;
            qsort(jabs_frames, count, sizeof *jabs_frames, jabs_compare_frames);
            atomic_store_explicit(&sorted, count, memory_order_release);
        }
        jabs_unlock(JABS_LOCK_FRAMES);
    }
    jabs_frame_entry key = { start, NULL, NULL, NULL, 0, 0 };
    return (const jabs_frame_entry *)bsearch(&key, jabs_frames, count, sizeof key, jabs_compare_frames);
}

/* The class of the compiled method whose function starts at `start`, or NULL. */
static const char *jabs_frame_class(const void *start) {
    const jabs_frame_entry *e = jabs_frame_of(start);
    return e ? e->class_name : NULL;
}

/* The compiled Java methods on this thread's stack, innermost first, to stderr. Frames that are
 * not compiled Java methods (runtime helpers, inlined code) are left out. */
void jabs_print_java_stack(void) {
    const void *starts[1024];
    int n = jabs_platform_stack_functions(starts, 1024);
    int shown = 0;
    for (int i = 0; i < n; i++) {
        const jabs_frame_entry *e = starts[i] ? jabs_frame_of(starts[i]) : NULL;
        if (!e) continue;
        fprintf(stderr, "    at %s.%s%s\n", e->class_name, e->name, e->descriptor);
        shown++;
    }
    if (!shown) fprintf(stderr, "    (no compiled Java frames found on the stack)\n");
    if (n >= 1024) fprintf(stderr, "    (stack cut at 1024 native frames)\n");
}

/* Java classes on the stack below the frame of function `from` (its callers, innermost first):
 * SecurityManager.getClassContext. */
jabs_object *jabs_class_context(const void *from) {
    const void *starts[1024];
    int n = jabs_platform_stack_functions(starts, 1024);
    int i = 0;
    while (i < n && starts[i] != from) i++;
    const char *names[1024];
    int count = 0;
    for (i++; i < n; i++) {
        const char *c = jabs_frame_class(starts[i]);
        if (c) names[count++] = c;
    }
    jabs_object *array = jabs_new_array(count, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &"#,
    );
    let _ = write!(out, "{}", class_record(CLASS));
    out.push_str(
        r#", "[Ljava/lang/Class;", NULL);
    if (!array) return NULL;
    for (int k = 0; k < count; k++) {
        jabs_object *c = jabs_class_object(names[k]);
        if (!c) return NULL;
        ((jabs_object **)jabs_array_data((jabs_array *)array))[k] = c;
    }
    return array;
}
"#,
    );
    out
}

/// `Throwable.fillInStackTrace(int)` and `StackTraceElement.initStackTraceElements`, the way
/// HotSpot fills them: the backtrace is a `long[]` of frame-table entries, innermost first, at most
/// 1024 (the JVM's default MaxJavaStackTraceDepth); `fillInStackTrace` frames and then the
/// `<init>` frames of the throwable's own class and superclasses are left out, as are hidden
/// classes' frames. Elements get the class, its `Class`, the method name, no file name and line
/// -1 (JABS keeps no line tables yet, so `toString` says "Unknown Source").
fn stack_trace_functions(unit: &Unit, recorded: &[&ClassFile]) -> String {
    let throwable = "java/lang/Throwable";
    let element = "java/lang/StackTraceElement";
    let has_field = |class: &str, name: &str, descriptor: &str| {
        unit.get(class).is_some_and(|c| {
            c.fields
                .iter()
                .any(|f| f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0)
        })
    };
    let recorded_class = |c: &str| recorded.iter().any(|r| r.name == c);
    let throwable_ok = recorded_class(throwable)
        && has_field(throwable, "backtrace", "Ljava/lang/Object;")
        && has_field(throwable, "depth", "I");
    let element_ok = recorded_class(element)
        && [
            ("declaringClassObject", "Ljava/lang/Class;"),
            ("declaringClass", "Ljava/lang/String;"),
            ("methodName", "Ljava/lang/String;"),
            ("fileName", "Ljava/lang/String;"),
            ("lineNumber", "I"),
        ]
        .iter()
        .all(|(name, descriptor)| has_field(element, name, descriptor));
    if !throwable_ok || !element_ok {
        // Without StackTraceElement nothing can read a trace, so recording none is exact; a
        // Throwable or element layout JABS does not know stops instead of guessing.
        let fill = if recorded_class(element) {
            "jabs_not_compiled(\"Throwable.fillInStackTrace (unknown Throwable or StackTraceElement layout)\");"
        } else {
            "return throwable;"
        };
        return format!(
            "jabs_object *jabs_fill_in_stack_trace(jabs_object *throwable) {{ {fill} }}\n\
void jabs_init_stack_trace_elements(jabs_object *elements, jabs_object *backtrace, int32_t depth) {{ (void)elements; (void)backtrace; (void)depth; jabs_not_compiled(\"StackTraceElement.initStackTraceElements (unknown layout)\"); }}\n"
        );
    }
    let t = object_struct(throwable);
    let e = object_struct(element);
    format!(
        r#"
#define JABS_MAX_TRACE 1024
jabs_object *jabs_fill_in_stack_trace(jabs_object *throwable) {{
    if (!throwable) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    const void *starts[JABS_MAX_TRACE];
    const jabs_frame_entry *frames[JABS_MAX_TRACE];
    int n = jabs_platform_stack_functions(starts, JABS_MAX_TRACE);
    const jabs_class *base = jabs_record_named("{throwable}");
    int count = 0, skipping_fill = 1, skipping_init = 1;
    for (int i = 0; i < n && count < JABS_MAX_TRACE; i++) {{
        const jabs_frame_entry *e = starts[i] ? jabs_frame_of(starts[i]) : NULL;
        if (!e || e->hidden || (e->flags & 0x{hidden:x}u)) continue;
        const jabs_class *holder = jabs_record_named(e->class_name);
        if (skipping_fill) {{
            if (strcmp(e->name, "fillInStackTrace") == 0 && holder && base && jabs_class_assignable(holder, base)) continue;
            skipping_fill = 0;
        }}
        if (skipping_init) {{
            if (strcmp(e->name, "<init>") == 0 && holder && jabs_class_assignable(throwable->cls, holder)) continue;
            skipping_init = 0;
        }}
        frames[count++] = e;
    }}
    jabs_object *backtrace = jabs_new_array(count, sizeof(int64_t), JABS_COMPONENT_PRIMITIVE, NULL, "[J", NULL);
    if (!backtrace) return NULL;
    int64_t *slots = (int64_t *)jabs_array_data((jabs_array *)backtrace);
    for (int i = 0; i < count; i++) slots[i] = (int64_t)(intptr_t)frames[i];
    (({t} *)throwable)->{backtrace} = backtrace;
    (({t} *)throwable)->{depth} = count;
    return throwable;
}}

void jabs_init_stack_trace_elements(jabs_object *elements, jabs_object *backtrace, int32_t depth) {{
    if (!elements || !backtrace) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
    jabs_array *a = (jabs_array *)elements, *b = (jabs_array *)backtrace;
    /* The backtrace is only ever the long[] jabs_fill_in_stack_trace made. */
    if (elements->cls != jabs_array_class() || !jabs_array_is_ref(a)) jabs_not_compiled("StackTraceElement.initStackTraceElements with non-reference elements array");
    if (backtrace->cls != jabs_array_class() || strcmp(jabs_array_descriptor(b), "[J") != 0) jabs_not_compiled("StackTraceElement.initStackTraceElements with a backtrace JABS did not record");
    if (depth < 0 || depth > a->length || depth > b->length) {{ jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX); return; }}
    const int64_t *slots = (const int64_t *)jabs_array_data(b);
    for (int32_t i = 0; i < depth; i++) {{
        const jabs_frame_entry *f = (const jabs_frame_entry *)(intptr_t)slots[i];
        jabs_object *ste = ((jabs_object **)jabs_array_data(a))[i];
        if (!ste) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return; }}
        jabs_object *cls = jabs_class_object(f->class_name);
        if (!cls) return;
        size_t length = strlen(f->class_name);
        char *dotted = (char *)malloc(length + 1);
        if (!dotted) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return; }}
        for (size_t k = 0; k <= length; k++) dotted[k] = f->class_name[k] == '/' ? '.' : f->class_name[k];
        jabs_object *class_name = jabs_string_from_utf8(dotted);
        free(dotted);
        if (!class_name) {{
            if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
            return;
        }}
        jabs_object *method_name = jabs_string_from_utf8(f->name);
        if (!method_name) {{
            if (!jabs_pending) jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY);
            return;
        }}
        (({e} *)ste)->{class_object} = cls;
        (({e} *)ste)->{declaring} = class_name;
        (({e} *)ste)->{method} = method_name;
        (({e} *)ste)->{file} = NULL;
        (({e} *)ste)->{line} = -1;
    }}
}}
"#,
        backtrace = instance_field("backtrace"),
        depth = instance_field("depth"),
        class_object = instance_field("declaringClassObject"),
        declaring = instance_field("declaringClass"),
        method = instance_field("methodName"),
        file = instance_field("fileName"),
        line = instance_field("lineNumber"),
        hidden = stack::MN_HIDDEN_MEMBER,
    )
}

/// Runtime half of VarHandles: creating handles (instances of VarHandle's record followed by
/// a `jabs_vh`) and turning coordinates into addresses.
fn var_handle_functions(unit: &Unit, recorded: &[&ClassFile], inits: &BTreeSet<String>) -> String {
    let mut out = String::new();
    if !recorded.iter().any(|c| c.name == VAR_HANDLE) {
        out.push_str(
            "const size_t jabs_gc_var_handle_size = 0u;\n\
const jabs_vh *jabs_vh_of(jabs_object *o) { (void)o; return NULL; }\n\
void *jabs_vh_address(const jabs_vh *h, jabs_object *o, int32_t index) { (void)h; (void)o; (void)index; return NULL; }\n\
jabs_object *jabs_var_handle_field(jabs_object *a, jabs_object *b, jabs_object *c, int32_t s) { (void)a; (void)b; (void)c; (void)s; jabs_not_compiled(\"VarHandle (class not in the unit)\"); }\n\
jabs_object *jabs_var_handle_array(jabs_object *a) { (void)a; jabs_not_compiled(\"VarHandle (class not in the unit)\"); }\n\
jabs_object *jabs_var_handle_byte_view(jabs_object *a, int32_t b) { (void)a; (void)b; jabs_not_compiled(\"VarHandle (class not in the unit)\"); }\n",
        );
        return out;
    }
    // Static fields of initialized classes, by name, for findStaticVarHandle.
    let mut rows = Vec::new();
    for class in inits {
        let Some(c) = unit.get(class) else { continue };
        for f in c.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
            if let Some(ty) = storage_type(&f.descriptor) {
                let sym = field_symbol(class, &f.name);
                // Indented: local to this file (declarations_for skips indented lines).
                let _ = writeln!(out, " extern {ty} {sym};");
                rows.push(format!(
                    "    {{ \"{}\", \"{}\", &{sym} }},",
                    c_escape(class),
                    c_escape(&f.name)
                ));
            }
        }
    }
    let _ = writeln!(
        out,
        "typedef struct {{ const char *class_name, *field_name; void *address; }} jabs_static_field;\nstatic const jabs_static_field jabs_static_fields[] = {{\n{}\n    {{ NULL, NULL, NULL }}\n}};",
        rows.join("\n")
    );
    let _ = write!(
        out,
        r#"/* Where a handle's jabs_vh sits: after VarHandle's own (unused) fields. */
static size_t jabs_vh_offset(void) {{ return ({record}.size + 15) & ~(size_t)15; }}
const size_t jabs_gc_var_handle_size = ((sizeof({vh_struct}) + 15) & ~(size_t)15) + sizeof(jabs_vh);

static jabs_object *jabs_vh_new(char kind, uint8_t shape, int64_t offset, void *address, int32_t scale, bool big_endian) {{
    size_t at = jabs_vh_offset();
    jabs_object *o = jabs_gc_alloc_obj(at + sizeof(jabs_vh), &{record});
    if (!o) return NULL;
    jabs_vh *h = (jabs_vh *)((char *)o + at);
    h->kind = kind; h->shape = shape; h->offset = offset; h->address = address; h->scale = scale; h->big_endian = big_endian;
    return o;
}}

const jabs_vh *jabs_vh_of(jabs_object *o) {{
    return o->cls == &{record} ? (const jabs_vh *)((char *)o + jabs_vh_offset()) : NULL;
}}

/* VarHandle.withInvokeExactBehavior / withInvokeBehavior of a JABS handle (an instance of
 * VarHandle itself, so the abstract methods have no implementation): the handle itself when it
 * already behaves so, else a copy with VarHandle.exact set as asked, which is what
 * hasInvokeExactBehavior reports. JABS resolves every access through a handle when it compiles
 * it, so an exact handle does not add the JVM's WrongMethodTypeException checks.
 * (sun.security.provider.ByteArrayAccess makes its handles exact this way.) */
jabs_object *jabs_vh_with_exact(jabs_object *o, int32_t exact) {{
    if (!o || !jabs_vh_of(o)) jabs_not_compiled("VarHandle.withInvokeExactBehavior on a handle JABS did not make (jabs_vh_with_exact)");
    if ((({vh_struct} *)o)->{exact} == (uint8_t)(exact != 0)) return o;
    size_t size = jabs_vh_offset() + sizeof(jabs_vh);
    jabs_object *copy = jabs_gc_alloc_obj(size, &{record});
    if (!copy) return NULL;
    memcpy(copy, o, size);
    /* A new object: its own lock word and identity hash. */
    copy->monitor = 0;
    atomic_store_explicit(&copy->hash, 0, memory_order_relaxed);
    (({vh_struct} *)copy)->{exact} = (uint8_t)(exact != 0);
    return copy;
}}

void *jabs_vh_address(const jabs_vh *h, jabs_object *o, int32_t index) {{
    switch (h->shape) {{
    case JABS_VH_STATIC: return h->address;
    case JABS_VH_FIELD:
        if (!o) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
        return (char *)o + h->offset;
    case JABS_VH_ELEMENT: {{
        if (!o) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
        jabs_array *a = (jabs_array *)o;
        if (index < 0 || index >= a->length) {{ jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX); return NULL; }}
        return (char *)jabs_array_data(a) + (size_t)index * (size_t)h->scale;
    }}
    case JABS_VH_BYTE_VIEW: {{
        /* The JDK's ArrayHandle order: cast to byte[] (ClassCastException; null passes), read
         * the length (NullPointerException), then Preconditions.checkIndex(index,
         * length - (size - 1)) (ArrayIndexOutOfBoundsException). */
        if (o && (o->cls != jabs_array_class() || strcmp(jabs_array_descriptor((const jabs_array *)o), "[B") != 0)) {{
            jabs_pending = jabs_vm_throwable(JABS_VM_CLASS_CAST);
            return NULL;
        }}
        if (!o) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
        jabs_array *a = (jabs_array *)o;
        int64_t limit = (int64_t)a->length - (int64_t)(h->scale - 1);
        if (index < 0 || (int64_t)index >= limit) {{ jabs_pending = jabs_vm_throwable(JABS_VM_ARRAY_INDEX); return NULL; }}
        return (char *)jabs_array_data(a) + (size_t)index;
    }}
    default: break;
    }}
    jabs_not_compiled("VarHandle access through a handle of unknown shape");
}}

/* The descriptor character of a Class used as a value type: 'I' for int.class, 'L' otherwise. */
static char jabs_value_kind(jabs_object *type) {{
    static const char *const names[] = {{ "boolean", "Z", "byte", "B", "char", "C", "short", "S", "int", "I", "long", "J", "float", "F", "double", "D" }};
    jabs_class_entry *e = jabs_class_entry_of(type);
    for (size_t i = 0; e && i < 16; i += 2) if (strcmp(e->name, names[i]) == 0) return names[i + 1][0];
    return 'L';
}}

/* findVarHandle / findStaticVarHandle. */
jabs_object *jabs_var_handle_field(jabs_object *declaring, jabs_object *name, jabs_object *type, int32_t is_static) {{
    if (!declaring || !name || !type) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    char kind = jabs_value_kind(type);
    if (!is_static) {{
        int64_t offset = jabs_field_offset(declaring, name);
        return jabs_pending ? NULL : jabs_vh_new(kind, JABS_VH_FIELD, offset, NULL, 0, false);
    }}
    char field[256];
    jabs_class_entry *e = jabs_class_entry_of(declaring);
    if (!e || !jabs_string_to_ascii(name, field, sizeof field)) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    const jabs_init_entry *i = (const jabs_init_entry *)bsearch(e->name, jabs_init_functions, sizeof jabs_init_functions / sizeof *jabs_init_functions - 1, sizeof *i, jabs_compare_init);
    if (i && i->init() != 0) return NULL;
    for (const jabs_static_field *f = jabs_static_fields; f->class_name; f++) {{
        if (strcmp(f->class_name, e->name) == 0 && strcmp(f->field_name, field) == 0) return jabs_vh_new(kind, JABS_VH_STATIC, 0, f->address, 0, false);
    }}
    jabs_not_compiled("findStaticVarHandle for a static field of a class whose initialization was not compiled");
}}

/* MethodHandles.arrayElementVarHandle. */
jabs_object *jabs_var_handle_array(jabs_object *array_class) {{
    jabs_class_entry *e = array_class ? jabs_class_entry_of(array_class) : NULL;
    if (!e) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (e->name[0] != '[') jabs_not_compiled("arrayElementVarHandle of a non-array class (IllegalArgumentException)");
    char kind = (e->name[1] == 'L' || e->name[1] == '[') ? 'L' : e->name[1];
    return jabs_vh_new(kind, JABS_VH_ELEMENT, 0, NULL, jabs_array_scale(array_class), false);
}}

/* VarHandles.byteArrayViewHandle, behind MethodHandles.byteArrayViewVarHandle (which has
 * already rejected a null ByteOrder and turned it into `big_endian`). */
jabs_object *jabs_var_handle_byte_view(jabs_object *view_array_class, int32_t big_endian) {{
    jabs_class_entry *e = view_array_class ? jabs_class_entry_of(view_array_class) : NULL;
    if (!e) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return NULL; }}
    if (e->name[0] != '[') jabs_not_compiled("byteArrayViewVarHandle of a non-array class (IllegalArgumentException)");
    int32_t size = 0;
    switch (e->name[1]) {{
    case 'C': case 'S': size = 2; break;
    case 'I': case 'F': size = 4; break;
    case 'J': case 'D': size = 8; break;
    default: jabs_not_compiled("byteArrayViewVarHandle of boolean[], byte[] or a reference array (UnsupportedOperationException)");
    }}
    return jabs_vh_new(e->name[1], JABS_VH_BYTE_VIEW, 0, NULL, size, big_endian != 0);
}}
"#,
        record = class_record(VAR_HANDLE),
        vh_struct = object_struct(VAR_HANDLE),
        exact = instance_field("exact"),
    );
    out
}

/// Object-layout accessors the java.io glue (runtime/jabs_io.c) uses, so that glue stays
/// independent of any one JDK's field layout. Missing classes or fields give a diagnostic stop
/// (or nothing, for optional fields) instead of a compile error.
fn io_accessor_functions(unit: &Unit, recorded: &[&ClassFile]) -> String {
    let has = |class: &str, field: &str, descriptor: &str| {
        recorded.iter().any(|c| c.name == class)
            && unit.get(class).is_some_and(|c| {
                c.fields.iter().any(|f| {
                    f.name == field && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
            })
    };
    let at = |class: &str, object: &str, field: &str| {
        format!(
            "(({} *){object})->{}",
            object_struct(class),
            instance_field(field)
        )
    };
    let mut out = String::from("\n/* ---- java.io glue accessors ---- */\n");
    // Unsafe.objectFieldOffset0(Field): the offset of the field a Field object names (its
    // declaring class and name), as objectFieldOffset1 finds it. Unsafe.objectFieldOffset has
    // already refused a null Field and hidden or record classes.
    let reflect_field = "java/lang/reflect/Field";
    if has(reflect_field, "clazz", "Ljava/lang/Class;")
        && has(reflect_field, "name", "Ljava/lang/String;")
    {
        let _ = writeln!(
            out,
            "int64_t jabs_unsafe_field_offset(jabs_object *f) {{ if (!f) {{ jabs_pending = jabs_vm_throwable(JABS_VM_NULL_POINTER); return 0; }} return jabs_field_offset({}, {}); }}",
            at(reflect_field, "f", "clazz"),
            at(reflect_field, "f", "name")
        );
    } else {
        out.push_str("int64_t jabs_unsafe_field_offset(jabs_object *f) { (void)f; jabs_not_compiled(\"Unsafe.objectFieldOffset0 (java.lang.reflect.Field is not in the program)\"); }\n");
    }
    let file = "java/io/File";
    if has(file, "path", "Ljava/lang/String;") {
        let _ = writeln!(
            out,
            "jabs_object *jabs_file_path_of(jabs_object *f) {{ return {}; }}",
            at(file, "f", "path")
        );
    } else {
        out.push_str("jabs_object *jabs_file_path_of(jabs_object *f) { (void)f; jabs_not_compiled(\"java.io.File without a path field\"); }\n");
    }
    out.push_str("jabs_object **jabs_stream_fd_slot(jabs_object *s) {\n");
    for stream in [
        "java/io/FileInputStream",
        "java/io/FileOutputStream",
        "java/io/RandomAccessFile",
    ] {
        if has(stream, "fd", "Ljava/io/FileDescriptor;") {
            let _ = writeln!(
                out,
                "    if (jabs_is_instance(s, &{})) return &{};",
                class_record(stream),
                at(stream, "s", "fd")
            );
        }
    }
    out.push_str("    jabs_not_compiled(\"a file stream class without a compiled FileDescriptor field\");\n}\n");
    let fd = "java/io/FileDescriptor";
    if has(fd, "fd", "I") {
        let _ = writeln!(
            out,
            "int32_t *jabs_descriptor_fd_slot(jabs_object *d) {{ return &{}; }}",
            at(fd, "d", "fd")
        );
    } else {
        out.push_str("int32_t *jabs_descriptor_fd_slot(jabs_object *d) { (void)d; jabs_not_compiled(\"FileDescriptor without an fd field\"); }\n");
    }
    // Windows FileDescriptors also carry a HANDLE; JABS's java.io uses the C runtime
    // descriptor instead, java.nio's files (CreateFile0) the HANDLE.
    if has(fd, "handle", "J") {
        let _ = writeln!(
            out,
            "void jabs_descriptor_set_handle(jabs_object *d, int64_t h) {{ {} = h; }}",
            at(fd, "d", "handle")
        );
        let _ = writeln!(
            out,
            "int64_t jabs_descriptor_handle(jabs_object *d) {{ return {}; }}",
            at(fd, "d", "handle")
        );
    } else {
        out.push_str(
            "void jabs_descriptor_set_handle(jabs_object *d, int64_t h) { (void)d; (void)h; }\n",
        );
        out.push_str("int64_t jabs_descriptor_handle(jabs_object *d) { (void)d; return -1; }\n");
    }
    if has(fd, "append", "Z") {
        let _ = writeln!(
            out,
            "void jabs_descriptor_set_append(jabs_object *d, int32_t a) {{ {} = (uint8_t)a; }}",
            at(fd, "d", "append")
        );
    } else {
        out.push_str(
            "void jabs_descriptor_set_append(jabs_object *d, int32_t a) { (void)d; (void)a; }\n",
        );
    }
    let _ = writeln!(
        out,
        "jabs_object *jabs_new_string_array(int32_t n) {{ return jabs_new_array(n, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{}, \"[Ljava/lang/String;\", NULL); }}",
        class_record(STRING)
    );
    // An exception the runtime raises, with a message (detailMessage), as the JDK's natives do.
    if has(THROWABLE, "detailMessage", "Ljava/lang/String;") {
        let _ = writeln!(
            out,
            "void jabs_raise_message(int kind, const char *utf8) {{ jabs_object *e = jabs_vm_throwable(kind); jabs_object *m = utf8 ? jabs_string_from_utf8(utf8) : NULL; if (m) {} = m; jabs_pending = e; }}",
            at(THROWABLE, "e", "detailMessage")
        );
    } else {
        out.push_str("void jabs_raise_message(int kind, const char *utf8) { (void)utf8; jabs_pending = jabs_vm_throwable(kind); }\n");
    }
    let _ = write!(
        out,
        r#"/* A String's contents as malloc'd UTF-8 (unpaired surrogates become U+FFFD). */
char *jabs_string_to_utf8(jabs_object *s) {{
    if (!s) return NULL;
    jabs_array *v = (jabs_array *)(({st} *)s)->{value};
    int utf16 = (({st} *)s)->{coder} != 0;
    int32_t n = utf16 ? v->length / 2 : v->length;
    const unsigned char *b = (const unsigned char *)jabs_array_data(v);
    char *out = (char *)malloc((size_t)n * 3 + 1);
    if (!out) return NULL;
    size_t o = 0;
    for (int32_t i = 0; i < n; i++) {{
        uint32_t c;
        if (!utf16) c = b[i];
        else {{
            uint16_t u; memcpy(&u, b + 2 * i, 2); c = u;
            if (c >= 0xD800 && c <= 0xDBFF && i + 1 < n) {{
                uint16_t lo; memcpy(&lo, b + 2 * (i + 1), 2);
                if (lo >= 0xDC00 && lo <= 0xDFFF) {{ c = 0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00); i++; }}
                else c = 0xFFFD;
            }} else if (c >= 0xD800 && c <= 0xDFFF) c = 0xFFFD;
        }}
        if (c < 0x80) out[o++] = (char)c;
        else if (c < 0x800) {{ out[o++] = (char)(0xC0 | (c >> 6)); out[o++] = (char)(0x80 | (c & 0x3F)); }}
        else if (c < 0x10000) {{ out[o++] = (char)(0xE0 | (c >> 12)); out[o++] = (char)(0x80 | ((c >> 6) & 0x3F)); out[o++] = (char)(0x80 | (c & 0x3F)); }}
        else {{ out[o++] = (char)(0xF0 | (c >> 18)); out[o++] = (char)(0x80 | ((c >> 12) & 0x3F)); out[o++] = (char)(0x80 | ((c >> 6) & 0x3F)); out[o++] = (char)(0x80 | (c & 0x3F)); }}
    }}
    out[o] = 0;
    return out;
}}
"#,
        st = object_struct(STRING),
        value = instance_field("value"),
        coder = instance_field("coder"),
    );
    out
}

/// `Class.getModule` (replacement): the app loader's unnamed module for input classes, the boot
/// loader's for the rest (arrays and primitives follow their element type / java.base).
fn module_function(unit: &Unit, inits: &BTreeSet<String>) -> String {
    let has_static = |class: &str, field: &str| {
        inits.contains(class)
            && unit.get(class).is_some_and(|c| {
                c.fields
                    .iter()
                    .any(|f| f.name == field && f.access & ACC_STATIC != 0)
            })
    };
    let has_field = |class: &str, field: &str| {
        unit.chain(class).is_ok()
            && unit.get(class).is_some_and(|c| {
                c.fields
                    .iter()
                    .any(|f| f.name == field && f.access & ACC_STATIC == 0)
            })
    };
    let boot = if has_static(BOOT_LOADER, "UNNAMED_MODULE") {
        format!(
            "if ({}() != 0) return NULL; return {};",
            init_symbol(BOOT_LOADER),
            field_symbol(BOOT_LOADER, "UNNAMED_MODULE")
        )
    } else {
        "jabs_not_compiled(\"Class.getModule (BootLoader is not in the program)\");".to_string()
    };
    let app = if has_field("java/lang/ClassLoader", "unnamedModule") {
        format!(
            "jabs_object *loader = jabs_app_loader(); if (jabs_pending) return NULL; if (loader) return (({} *)loader)->{};",
            object_struct("java/lang/ClassLoader"),
            instance_field("unnamedModule")
        )
    } else {
        String::new()
    };
    format!(
        r#"
/* Reflection.getClassAccessFlags: the class file's flags; public final abstract for arrays and primitives. */
int32_t jabs_class_access_flags(jabs_object *c) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    if (!e || !e->record) return 0x411;
    return (int32_t)(e->record->access & 0x7FFF);
}}

/* Class.getModule. Only an array's name is a descriptor: `[...Lpkg/Name;` names its element
   class, `[...I` a primitive. A plain name is the class itself, even one that starts with 'L',
   such as a default-package `Launcher`. Primitives and their arrays are in java.base, as on the
   JVM. An input class is in the app loader's unnamed module; a JDK class in its named module
   (runtime/jabs_modules.c); anything else (no module data, a package of no JDK module) in the
   boot loader's unnamed module. */
jabs_object *jabs_class_module(jabs_object *c) {{
    jabs_class_entry *e = jabs_class_entry_of(c);
    const char *name = e ? e->name : "";
    int array = 0;
    while (name[0] == '[') {{ name++; array = 1; }}
    /* The class whose module this is. */
    const char *owner = NULL;
    char element[1024];
    char *heap_element = NULL;
    if (!array) {{
        owner = jabs_is_primitive_name(name) ? "java/lang/Object" : name;
    }} else {{
        size_t len = strlen(name);
        if (len >= 3 && name[0] == 'L' && name[len - 1] == ';') {{
            size_t n = len - 2;
            char *buf = n < sizeof element ? element : (heap_element = (char *)malloc(n + 1));
            if (!buf) {{ jabs_pending = jabs_vm_throwable(JABS_VM_OUT_OF_MEMORY); return NULL; }}
            memcpy(buf, name + 1, n);
            buf[n] = 0;
            owner = buf;
        }} else if (len == 1) {{
            owner = "java/lang/Object";
        }}
    }}
    int is_app = owner && jabs_is_app_class(owner);
    jabs_object *named = (!is_app && owner) ? jabs_module_of_class(owner) : NULL;
    free(heap_element);
    if (named || jabs_pending) return named;
    if (is_app) {{
        {app}
    }}
    {boot}
}}
"#
    )
}

fn format_resource_entry(index: usize, data: &[u8]) -> String {
    let mut out = String::with_capacity(64 + data.len() * 4 + (data.len() / 4096) * 3);
    let _ = write!(
        out,
        "static const unsigned char jabs_resource_{index}[{}] = \"",
        data.len() + 1
    );
    for (j, &b) in data.iter().enumerate() {
        if j > 0 && j % 4096 == 0 {
            out.push_str("\"\n\"");
        }
        out.push('\\');
        out.push((b'0' + (b >> 6)) as char);
        out.push((b'0' + ((b >> 3) & 7)) as char);
        out.push((b'0' + (b & 7)) as char);
    }
    out.push_str("\";\n");
    out
}

/// `resources.c`: the embedded resource table (`jabs_resources`, ended by a NULL name) and what
/// runtime/jabs_jab.c needs to know about the program's .jab (mode 0 when it has none).
pub fn resources_file(
    resources: &[(String, Vec<u8>)],
    jab: Option<&JabConfig>,
    java_home: Option<&str>,
) -> String {
    let total_bytes: usize = resources.iter().map(|(_, d)| d.len()).sum();
    let encoded: Vec<String> = if resources.len() >= 8 || total_bytes >= 65_536 {
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(resources.len().max(1));
        let chunk_size = resources.len().div_ceil(threads).max(1);
        let mut results: Vec<String> = vec![String::new(); resources.len()];
        std::thread::scope(|scope| {
            let mut rest = results.as_mut_slice();
            for (chunk_idx, chunk) in resources.chunks(chunk_size).enumerate() {
                let base = chunk_idx * chunk_size;
                let (dst, tail) = rest.split_at_mut(chunk.len());
                rest = tail;
                scope.spawn(move || {
                    for (offset, (_, data)) in chunk.iter().enumerate() {
                        dst[offset] = format_resource_entry(base + offset, data);
                    }
                });
            }
        });
        results
    } else {
        resources
            .iter()
            .enumerate()
            .map(|(i, (_, data))| format_resource_entry(i, data))
            .collect()
    };
    let cap: usize = 64 + encoded.iter().map(String::len).sum::<usize>() + resources.len() * 64;
    let mut out = String::with_capacity(cap);
    out.push_str("#include \"jabs_runtime.h\"\n");
    let (mode, id, home, log) = match jab {
        None => (0, "", "", ""),
        Some(JabConfig {
            id,
            record_log: None,
        }) => (1, id.as_str(), "", ""),
        Some(JabConfig {
            id,
            record_log: Some(log),
        }) => (2, id.as_str(), java_home.unwrap_or(""), log.as_str()),
    };
    let _ = writeln!(
        out,
        "const int32_t jabs_jab_mode = {mode};\nconst char jabs_jab_id[] = \"{}\";\nconst char jabs_jab_record_home[] = \"{}\";\nconst char jabs_jab_record_log[] = \"{}\";",
        c_escape(id),
        c_escape(home),
        c_escape(log)
    );
    for s in encoded {
        out.push_str(&s);
    }
    out.push_str("const jabs_resource jabs_resources[] = {\n");
    for (i, (name, data)) in resources.iter().enumerate() {
        let _ = writeln!(
            out,
            "    {{ \"{}\", jabs_resource_{i}, {} }},",
            c_escape(name),
            data.len()
        );
    }
    out.push_str("    { NULL, NULL, 0 }\n};\n");
    out
}

/// `jabs_main_thread()`: the main Thread the VM would create at startup (thread groups "system"
/// and "main", priority 5, not a daemon, RUNNABLE), built once without running constructors.
/// Only fields the JDK's classes declare are set. Only the main OS thread ever calls it (every
/// other thread knows its Thread from the start), so its static is never shared.
fn main_thread_function(unit: &Unit, recorded: &[&ClassFile]) -> String {
    let (thread, holder, group) = (
        "java/lang/Thread",
        "java/lang/Thread$FieldHolder",
        "java/lang/ThreadGroup",
    );
    let have = |c: &str| recorded.iter().any(|r| r.name == c);
    if ![thread, holder, group, OBJECT].iter().all(|c| have(c)) {
        return "jabs_object *jabs_main_thread(void) { jabs_not_compiled(\"Thread.currentThread (thread classes are not in the unit)\"); }\n".to_string();
    }
    // `object.field = value` if the class declares that instance field with that descriptor.
    let set = |class: &str, object: &str, name: &str, descriptor: &str, value: &str| -> String {
        let declared = unit.get(class).is_some_and(|c| {
            c.fields
                .iter()
                .any(|f| f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0)
        });
        if declared {
            format!(
                "    (({} *){object})->{} = {value};\n",
                object_struct(class),
                instance_field(name)
            )
        } else {
            format!("    /* {class} has no {name}:{descriptor} */\n")
        }
    };
    let mut body = String::from(
        "jabs_object *jabs_main_thread(void) {\n    static jabs_object *main_thread;\n    if (main_thread) return main_thread;\n",
    );
    let _ = writeln!(body, "    if ({}() != 0) return NULL;", init_symbol(thread));
    let _ = writeln!(
        body,
        "    jabs_object *system = jabs_new(&{g}), *group = jabs_new(&{g}), *holder = jabs_new(&{h}), *t = jabs_new(&{t}), *lock = jabs_new(&{o});",
        g = class_record(group),
        h = class_record(holder),
        t = class_record(thread),
        o = class_record(OBJECT)
    );
    body.push_str("    jabs_object *system_name = jabs_string_from_utf8(\"system\"), *main_name = jabs_string_from_utf8(\"main\");\n");
    body.push_str("    if (!system || !group || !holder || !t || !lock || !system_name || !main_name) return NULL;\n");
    body.push_str(&set(
        group,
        "system",
        "name",
        "Ljava/lang/String;",
        "system_name",
    ));
    body.push_str(&set(group, "system", "maxPriority", "I", "10"));
    body.push_str(&set(
        group,
        "group",
        "parent",
        "Ljava/lang/ThreadGroup;",
        "system",
    ));
    body.push_str(&set(
        group,
        "group",
        "name",
        "Ljava/lang/String;",
        "main_name",
    ));
    body.push_str(&set(group, "group", "maxPriority", "I", "10"));
    body.push_str(&set(
        holder,
        "holder",
        "group",
        "Ljava/lang/ThreadGroup;",
        "group",
    ));
    body.push_str(&set(holder, "holder", "priority", "I", "5"));
    body.push_str(&set(holder, "holder", "daemon", "Z", "0"));
    body.push_str(&set(
        holder,
        "holder",
        "threadStatus",
        "I",
        "5 /* JVMTI alive | runnable */",
    ));
    body.push_str(&set(thread, "t", "tid", "J", "1"));
    body.push_str(&set(thread, "t", "name", "Ljava/lang/String;", "main_name"));
    body.push_str(&set(
        thread,
        "t",
        "holder",
        "Ljava/lang/Thread$FieldHolder;",
        "holder",
    ));
    body.push_str(&set(
        thread,
        "t",
        "interruptLock",
        "Ljava/lang/Object;",
        "lock",
    ));
    body.push_str("    main_thread = t;\n    jabs_thread_attach_main(t);\n");
    // The context class loader of the main thread is the application class loader.
    body.push_str(
        "    jabs_object *loader = jabs_app_loader();\n    if (jabs_pending) return NULL;\n",
    );
    body.push_str(&set(
        thread,
        "t",
        "contextClassLoader",
        "Ljava/lang/ClassLoader;",
        "loader",
    ));
    body.push_str("    return t;\n}\n");
    body
}

/// What runtime/jabs_thread.c needs from java.lang.Thread, emitted in every program (the thread
/// runtime is always linked): where its fields live, and the main Thread when the registry that
/// builds it is absent. Anything the unit lacks stops with a diagnostic when it is reached.
fn thread_glue_functions(unit: &Unit, registry: bool, methods: &BTreeSet<MethodKey>) -> String {
    const HOLDER: &str = "java/lang/Thread$FieldHolder";
    let mut out =
        String::from("\n/* Glue for jabs_thread.c (see thread_glue_functions in cgen.rs). */\n");
    if !registry {
        out.push_str("jabs_object *jabs_main_thread(void) { jabs_not_compiled(\"Thread.currentThread (the Class registry is not in the unit)\"); }\n");
    }
    let laid_out = unit
        .get(THREAD)
        .is_some_and(|c| c.access & ACC_INTERFACE == 0)
        && unit.chain(THREAD).is_ok();
    let declared_in = |class: &str, name: &str, descriptor: &str| {
        laid_out
            && unit.chain(class).is_ok()
            && unit.get(class).is_some_and(|c| {
                c.fields.iter().any(|f| {
                    f.name == name && f.descriptor == descriptor && f.access & ACC_STATIC == 0
                })
            })
    };
    let declared = |name: &str, descriptor: &str| declared_in(THREAD, name, descriptor);
    if declared("eetop", "J") {
        let _ = writeln!(
            out,
            "int64_t *jabs_thread_eetop_slot(jabs_object *thread) {{ return &(({} *)thread)->{}; }}",
            object_struct(THREAD),
            instance_field("eetop")
        );
    } else {
        out.push_str("int64_t *jabs_thread_eetop_slot(jabs_object *thread) { (void)thread; jabs_not_compiled(\"Thread.eetop (java.lang.Thread without that field is not supported)\"); }\n");
    }
    // holder.threadStatus and holder.daemon: where JDK 25 keeps a platform thread's state.
    if declared("interrupted", "Z") {
        let _ = writeln!(
            out,
            "uint8_t *jabs_thread_interrupted_slot(jabs_object *thread) {{ return &(({} *)thread)->{}; }}",
            object_struct(THREAD),
            instance_field("interrupted")
        );
    } else {
        out.push_str("uint8_t *jabs_thread_interrupted_slot(jabs_object *thread) { (void)thread; jabs_not_compiled(\"Thread.interrupted (java.lang.Thread without that field is not supported)\"); }\n");
    }
    let holder_fields = declared("holder", "Ljava/lang/Thread$FieldHolder;")
        && declared_in(HOLDER, "threadStatus", "I")
        && declared_in(HOLDER, "daemon", "Z");
    if holder_fields {
        let holder = format!(
            "(({} *)((({} *)thread)->{}))",
            object_struct(HOLDER),
            object_struct(THREAD),
            instance_field("holder")
        );
        let _ = writeln!(
            out,
            "int32_t *jabs_thread_status_slot(jabs_object *thread) {{ if (!(({} *)thread)->{}) return NULL; return &{holder}->{}; }}",
            object_struct(THREAD),
            instance_field("holder"),
            instance_field("threadStatus")
        );
        let _ = writeln!(
            out,
            "bool jabs_thread_is_daemon(jabs_object *thread) {{ if (!(({} *)thread)->{}) jabs_not_compiled(\"Thread.start0 of a Thread without a FieldHolder\"); return {holder}->{} != 0; }}",
            object_struct(THREAD),
            instance_field("holder"),
            instance_field("daemon")
        );
    } else {
        out.push_str("int32_t *jabs_thread_status_slot(jabs_object *thread) { (void)thread; jabs_not_compiled(\"Thread.holder.threadStatus (java.lang.Thread without a FieldHolder is not supported)\"); }\n");
        out.push_str("bool jabs_thread_is_daemon(jabs_object *thread) { (void)thread; jabs_not_compiled(\"Thread.holder.daemon (java.lang.Thread without a FieldHolder is not supported)\"); }\n");
    }
    // run() through Thread's vtable slot: a subclass keeps the slot index of the method it overrides.
    let run_slot = if laid_out {
        unit.vtable(THREAD).ok().and_then(|v| {
            v.iter()
                .position(|s| s.name == "run" && s.descriptor == "()V")
        })
    } else {
        None
    };
    match run_slot {
        Some(slot) => {
            let _ = writeln!(
                out,
                "void jabs_thread_run_java(jabs_object *thread) {{ ((void (*)(jabs_object *))thread->cls->vtable[{slot}])(thread); }}"
            );
        }
        None => out.push_str("void jabs_thread_run_java(jabs_object *thread) { (void)thread; jabs_not_compiled(\"Thread.run (java.lang.Thread is not in the unit)\"); }\n"),
    }
    let dispatch = (
        THREAD.to_string(),
        "dispatchUncaughtException".to_string(),
        "(Ljava/lang/Throwable;)V".to_string(),
    );
    if methods.contains(&dispatch) {
        let _ = writeln!(
            out,
            "bool jabs_thread_dispatch_uncaught(jabs_object *thread, jabs_object *uncaught) {{ {}(thread, uncaught); return true; }}",
            symbol(&dispatch.0, &dispatch.1, &dispatch.2)
        );
    } else {
        out.push_str("bool jabs_thread_dispatch_uncaught(jabs_object *thread, jabs_object *uncaught) { (void)thread; (void)uncaught; return false; }\n");
    }
    let exit = (THREAD.to_string(), "exit".to_string(), "()V".to_string());
    if methods.contains(&exit) {
        let _ = writeln!(
            out,
            "bool jabs_thread_exit_java(jabs_object *thread) {{ {}(thread); return true; }}",
            symbol(&exit.0, &exit.1, &exit.2)
        );
    } else {
        out.push_str(
            "bool jabs_thread_exit_java(jabs_object *thread) { (void)thread; return false; }\n",
        );
    }
    if registry {
        out.push_str("jabs_object *jabs_static_monitor(const char *name, jabs_object *fallback) { (void)fallback; return jabs_class_object(name); }\n");
    } else {
        out.push_str("jabs_object *jabs_static_monitor(const char *name, jabs_object *fallback) { (void)name; return fallback; }\n");
    }
    if registry {
        out.push_str("void jabs_thread_raise(int kind, const char *message) { jabs_raise_message(kind, message); }\n");
    } else {
        out.push_str("void jabs_thread_raise(int kind, const char *message) { (void)message; jabs_pending = jabs_vm_throwable(kind); }\n");
    }
    out
}

/// Classes whose initialization must precede this one's: the superclass, then (for classes)
/// every superinterface that declares a non-abstract instance method.
fn init_prerequisites(unit: &Unit, class: &ClassFile) -> Vec<String> {
    if class.access & ACC_INTERFACE != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(parent) = class.super_name.as_deref().filter(|p| *p != OBJECT) {
        out.push(parent.to_string());
    }
    if let Ok(ifaces) = unit.superinterfaces_arc(&class.name) {
        for iface in ifaces.iter() {
            if iface
                .methods
                .iter()
                .any(|m| m.access & (ACC_STATIC | ACC_ABSTRACT) == 0)
            {
                out.push(iface.name.clone());
            }
        }
    }
    out
}

/// Emit a class's static field storage and initialization function (JVMS 5.5). The state and
/// its lock live in the runtime (`jabs_class_init` in jabs_rt.h, the procedure in
/// jabs_thread.c); an initialized class costs one acquire load.
fn emit_statics_and_init(
    out: &mut String,
    unit: &Unit,
    class: &ClassFile,
    literal_ids: &HashMap<Vec<u16>, u32>,
) {
    let init = init_symbol(&class.name);
    let state = format!("{init}_state");
    let mut constants = String::new();
    for f in class.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
        let Some(ty) = storage_type(&f.descriptor) else {
            continue;
        };
        let sym = field_symbol(&class.name, &f.name);
        // External linkage: fields only read through inlined constants must not trip unused warnings.
        let _ = writeln!(out, "{ty} {sym};");
        // ConstantValue fields are set by the initialization procedure itself, before <clinit> runs.
        let value = match f.constant_value.and_then(|i| class.pool.get(i as usize)) {
            Some(Constant::Integer(v)) => Some(Value::I(*v)),
            Some(Constant::Long(v)) => Some(Value::J(*v)),
            Some(Constant::Float(v)) => Some(Value::F(*v)),
            Some(Constant::Double(v)) => Some(Value::D(*v)),
            _ => None,
        };
        if let Some(value) = value {
            let literal = narrow_for_store(&f.descriptor, &value_literal(value));
            let _ = writeln!(constants, "    {sym} = {literal};");
        } else if let Some(units) = constant_string(class, f) {
            let _ = writeln!(
                constants,
                "    {sym} = jabs_literal({}u); if (!{sym}) {{ jabs_class_init_end(&{state}, false); return -1; }}",
                literal_ids[&units]
            );
        }
    }
    let _ = writeln!(out, "\njabs_class_init {state};");
    let _ = writeln!(out, "int {init}(void) {{");
    let _ = writeln!(out, "    if (jabs_class_initialized(&{state})) return 0;");
    let _ = writeln!(out, "    int step = jabs_class_init_begin(&{state});");
    let _ = writeln!(out, "    if (step == JABS_INIT_DONE) return 0;");
    let _ = writeln!(out, "    if (step != JABS_INIT_RUN) return -1;");
    // Superclass first, then superinterfaces with default methods (JVMS 5.5 step 7).
    for prerequisite in init_prerequisites(unit, class) {
        let _ = writeln!(
            out,
            "    if ({}() != 0) {{ jabs_class_init_end(&{state}, false); return -1; }}",
            init_symbol(&prerequisite)
        );
    }
    out.push_str(&constants);
    if class.methods.iter().any(|m| m.name == "<clinit>") {
        let _ = writeln!(out, "    {}();", symbol(&class.name, "<clinit>", "()V"));
        let _ = writeln!(
            out,
            "    if (jabs_pending) {{ jabs_pending = jabs_initializer_failure(jabs_pending); jabs_class_init_end(&{state}, false); return -1; }}"
        );
    }
    let _ = writeln!(out, "    jabs_class_init_end(&{state}, true);");
    let _ = writeln!(out, "    return 0;\n}}\n");
}

/// Why a class's hierarchy keeps it from being initialized, or None if it is complete in the unit.
fn structural_problem<'a>(unit: &Unit<'a>, class: &'a ClassFile) -> Option<String> {
    unit.structural_problem_cached(class)
}

/// Classes whose init function emitted code may call: the methods' own classes, their init
/// dependencies, and the prerequisites of every class initialized.
fn emitted_init_targets(
    unit: &Unit,
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = emission.inits.clone();
    // Instance methods assume their class is initialized; runtime-created objects need the init.
    out.extend(emission.methods.iter().map(|k| k.0.clone()));
    for key in &emission.methods {
        if let Some(Ok((_, a))) = results.get(key) {
            out.extend(a.deps.iter().filter_map(|d| match d {
                Dep::Init(c) => Some(c.clone()),
                Dep::Method(_) => None,
            }));
        }
    }
    for class in emission.inits.iter().filter_map(|c| unit.get(c)) {
        out.extend(init_prerequisites(unit, class));
    }
    out
}

/// Emit stand-ins for what emitted code may call but the unit could not compile: a function with
/// the callee's exact signature (under its real symbol) or a class init function, each stopping
/// with the reason; plus static field storage for classes whose initialization is a stub, so
/// field references still link (the guard stops before they are touched).
/// Methods emitted code calls directly that are not emitted themselves (they get stubs).
fn reached_stub_targets(
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
) -> BTreeSet<MethodKey> {
    let mut calls = BTreeSet::new();
    for key in &emission.methods {
        let Some(Ok((_, a))) = results.get(key) else {
            continue;
        };
        for resolved in a.resolved.values() {
            if let Resolved::Call(CallKind::Direct(target)) = resolved
                && !emission.methods.contains(target)
            {
                calls.insert(target.clone());
            }
        }
    }
    calls
}

fn reached_stub_statics(
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
) -> BTreeSet<String> {
    let mut statics: BTreeSet<String> = BTreeSet::new();
    for key in &emission.methods {
        let Some(Ok((_, a))) = results.get(key) else {
            continue;
        };
        for (pc, resolved) in &a.resolved {
            if let Resolved::Field(declaring) = resolved
                && !emission.inits.contains(declaring)
            {
                let is_static = a.insns.iter().any(|i| {
                    i.pc == *pc && matches!(i.op, Op::GetStatic { .. } | Op::PutStatic { .. })
                });
                if is_static {
                    statics.insert(declaring.clone());
                }
            }
        }
    }
    statics
}

fn emit_reached_stubs(
    out: &mut String,
    unit: &Unit,
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    emission: &Emission,
    init_ok: &HashMap<String, std::result::Result<(), String>>,
) {
    let mut calls: BTreeSet<MethodKey> = BTreeSet::new();
    for key in &emission.methods {
        let Some(Ok((_, a))) = results.get(key) else {
            continue;
        };
        for resolved in a.resolved.values() {
            if let Resolved::Call(CallKind::Direct(target)) = resolved
                && !emission.methods.contains(target)
            {
                calls.insert(target.clone());
            }
        }
    }
    let statics = reached_stub_statics(results, emission);
    for target in &calls {
        let Some(class) = unit.get(&target.0) else {
            continue;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| m.name == target.1 && m.descriptor == target.2)
        else {
            continue;
        };
        let Ok(sig) = unit.signature(&target.2) else {
            continue;
        };
        let reason = match results.get(target) {
            Some(Err(reason)) => reason.clone(),
            _ => "not reached by reachability analysis".to_string(),
        };
        let mut unused = String::new();
        if method.access & ACC_STATIC == 0 {
            unused.push_str(" (void)self;");
        }
        for i in 0..sig.params.len() {
            let _ = write!(unused, " (void)a{i};");
        }
        let message = c_escape(&format!("{}.{}{} ({reason})", target.0, target.1, target.2));
        let _ = writeln!(
            out,
            "{} {{{unused} jabs_not_compiled(\"{message}\"); }}",
            prototype(&target.0, method, &sig)
        );
    }
    for class in emitted_init_targets(unit, results, emission).difference(&emission.inits) {
        let reason = match init_ok.get(class) {
            Some(Err(reason)) => reason.clone(),
            _ => "not in the compilation unit".to_string(),
        };
        let message = c_escape(&format!(
            "class {class} cannot be initialized natively ({reason})"
        ));
        let _ = writeln!(
            out,
            "int {}(void) {{ jabs_not_compiled(\"{message}\"); }}",
            init_symbol(class)
        );
    }
    for class in statics.iter().filter_map(|c| unit.get(c)) {
        for f in class.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
            if let Some(ty) = storage_type(&f.descriptor) {
                let _ = writeln!(out, "{ty} {};", field_symbol(&class.name, &f.name));
            }
        }
    }
    out.push('\n');
}

/// Collapse per-method details (pcs, names, descriptors) so reasons group into categories.
pub fn blocker_category(reason: &str) -> String {
    let reason = reason
        .split_once(": ")
        .filter(|(pc, _)| pc.starts_with("pc "))
        .map_or(reason, |(_, r)| r);
    let rules: [(&str, &str); 17] = [
        (
            "class hierarchy incomplete",
            "superclass/interface outside the unit (JDK or other jars)",
        ),
        (
            "is inherited from outside the unit",
            "call inherited from outside the unit (e.g. Object methods)",
        ),
        (
            "is not in the compilation unit",
            "uses a class outside the unit (JDK or other jars)",
        ),
        (
            "or its hierarchy is outside the unit",
            "type relationship outside the unit",
        ),
        (
            "which cannot be initialized",
            "initializes a class that cannot be initialized",
        ),
        ("static initializer of", "static initializer not compiled"),
        (
            "type test against",
            "instanceof/checkcast against an interface or outside class",
        ),
        ("arrays in", "arrays"),
        ("interface instance methods", "interface instance methods"),
        ("synchronized", "synchronized methods"),
        ("exception handlers", "exception handlers (try/catch)"),
        ("no bytecode", "abstract or native methods"),
        (
            "native method without",
            "native method without a JABS implementation",
        ),
        ("calls ", "calls a method that is not compiled"),
        (
            "not declared in the unit",
            "field declared outside the unit",
        ),
        ("super calls", "super calls"),
        ("constructor", "constructor call shape"),
    ];
    if let Some((_, category)) = rules.iter().find(|(needle, _)| reason.contains(needle)) {
        return category.to_string();
    }
    if let Some(op) = reason.strip_prefix("opcode ") {
        return format!("opcode {}", op.split(" (").next().unwrap_or(op));
    }
    reason.to_string()
}

/// Blocker category of a seed that is not a method with its own reason.
const MISSING_METHOD: &str = "calls a method outside the unit or missing";
const OUTSIDE_CLASS: &str = "initializes a class outside the unit";

/// A node of the dependency graph: a method, or a class's initialization.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Node {
    Method(MethodKey),
    Class(String),
}

/// The node's index, adding it on first sight.
fn intern_node(
    node: Node,
    index: &mut HashMap<Node, usize>,
    nodes: &mut Vec<Node>,
    dependents: &mut Vec<Vec<usize>>,
) -> usize {
    if let Some(&at) = index.get(&node) {
        return at;
    }
    let at = nodes.len();
    nodes.push(node.clone());
    dependents.push(Vec::new());
    index.insert(node, at);
    at
}

type MethodOutcome = std::result::Result<(Signature, Analyzed, Option<(String, bool)>), String>;

/// Analyze one method against `unit`: pure per method, safe to call in parallel.
fn analyze_method<'a>(
    unit: &Unit<'a>,
    have_classes: bool,
    class: &'a ClassFile,
    method: &'a Member,
) -> (MethodKey, MethodOutcome) {
    let key = (
        class.name.clone(),
        method.name.clone(),
        method.descriptor.clone(),
    );
    let result = (|| {
        if let Some(reason) = structural_problem(unit, class) {
            return Err(reason);
        }
        let sig = (*unit
            .signature(&method.descriptor)
            .map_err(|e| e.to_string())?)
        .clone();
        if let Some(body) = replacement(&key, have_classes) {
            return Ok((sig, Analyzed::empty(), Some(body)));
        }
        if method.access & ACC_NATIVE != 0 {
            let found = management::intrinsic(unit, &key, have_classes)
                .or_else(|| net::intrinsic(&key, have_classes))
                .or_else(|| nio_fs::intrinsic(&key, have_classes))
                .or_else(|| modules::intrinsic(&key))
                .or_else(|| stack::intrinsic(&key, have_classes))
                .or_else(|| method_handles::intrinsic(&key, have_classes))
                .or_else(|| unsafe_rt::intrinsic(&key, have_classes))
                .or_else(|| zip::intrinsic(&key, have_classes))
                .or_else(|| boot_extras::intrinsic(&key, have_classes))
                .or_else(|| intrinsic(&key, have_classes));
            let Some(body) = found else {
                return Err("native method without a JABS implementation".to_string());
            };
            return Ok((sig, Analyzed::empty(), Some(body)));
        }
        let analyzed = analyze(unit, class, method, &sig)?;
        Ok((sig, analyzed, None))
    })();
    (key, result)
}

/// Demand-driven, wave-parallel method analysis and class-initialization cache.
struct AnalysisCache<'a> {
    unit: &'a Unit<'a>,
    have_classes: bool,
    results: BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    intrinsics: HashMap<MethodKey, (String, bool)>,
    init_ok: HashMap<String, std::result::Result<(), String>>,
}

impl<'a> AnalysisCache<'a> {
    fn new(unit: &'a Unit<'a>, have_classes: bool) -> Self {
        AnalysisCache {
            unit,
            have_classes,
            results: BTreeMap::new(),
            intrinsics: HashMap::new(),
            init_ok: HashMap::new(),
        }
    }

    fn insert_outcome(&mut self, key: MethodKey, outcome: MethodOutcome) {
        match outcome {
            Ok((sig, analyzed, intrinsic)) => {
                if let Some(body) = intrinsic {
                    self.intrinsics.insert(key.clone(), body);
                }
                self.results.insert(key, Ok((sig, analyzed)));
            }
            Err(reason) => {
                self.results.insert(key, Err(reason));
            }
        }
    }

    /// Analyze all `(&ClassFile, &Member)` pairs in parallel using `std::thread::scope`.
    fn analyze_batch(&mut self, batch: Vec<(&'a ClassFile, &'a Member)>) {
        if batch.is_empty() {
            return;
        }
        let unit = self.unit;
        let have_classes = self.have_classes;
        if batch.len() < 16 {
            for (class, method) in batch {
                let (key, outcome) = analyze_method(unit, have_classes, class, method);
                self.insert_outcome(key, outcome);
            }
            return;
        }
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(batch.len());
        let chunk_size = batch.len().div_ceil(threads).max(1);
        let chunk_results: Vec<Vec<_>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|&(class, method)| {
                                analyze_method(unit, have_classes, class, method)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for chunk in chunk_results {
            for (key, outcome) in chunk {
                self.insert_outcome(key, outcome);
            }
        }
    }

    /// Ensure every method in `keys` (that exists in `unit`) is analyzed, running in parallel.
    fn ensure_analyzed(&mut self, keys: &[MethodKey]) {
        let mut seen: HashSet<(&str, &str, &str)> = HashSet::new();
        let mut batch: Vec<(&'a ClassFile, &'a Member)> = Vec::new();
        for key in keys {
            if self.results.contains_key(key)
                || !seen.insert((key.0.as_str(), key.1.as_str(), key.2.as_str()))
            {
                continue;
            }
            if let Some(class) = self.unit.get(&key.0)
                && let Some(method) = class
                    .methods
                    .iter()
                    .find(|m| m.name == key.1 && m.descriptor == key.2)
            {
                batch.push((class, method));
            }
        }
        self.analyze_batch(batch);
    }

    fn ensure_one(&mut self, key: &MethodKey) {
        if self.results.contains_key(key) {
            return;
        }
        if let Some(class) = self.unit.get(&key.0)
            && let Some(method) = class
                .methods
                .iter()
                .find(|m| m.name == key.1 && m.descriptor == key.2)
        {
            let (k, outcome) = analyze_method(self.unit, self.have_classes, class, method);
            self.insert_outcome(k, outcome);
        }
    }

    fn is_compiled(&mut self, key: &MethodKey) -> bool {
        self.ensure_one(key);
        matches!(self.results.get(key), Some(Ok(_)))
    }

    /// Memoized class-initializability check (replaces the full-unit fixpoint scan).
    fn check_init(&mut self, name: &str) -> bool {
        if let Some(res) = self.init_ok.get(name) {
            return res.is_ok();
        }
        let Some(class) = self.unit.get(name) else {
            self.init_ok.insert(
                name.to_string(),
                Err(format!("{name} is not in the compilation unit")),
            );
            return false;
        };
        if let Some(reason) = structural_problem(self.unit, class) {
            self.init_ok.insert(name.to_string(), Err(reason));
            return false;
        }
        if class
            .methods
            .iter()
            .any(|m| m.name == "<clinit>" && m.descriptor == "()V")
        {
            let clinit = (name.to_string(), "<clinit>".to_string(), "()V".to_string());
            self.ensure_one(&clinit);
            if let Some(Err(reason)) = self.results.get(&clinit) {
                self.init_ok.insert(
                    name.to_string(),
                    Err(format!(
                        "static initializer of {name} not compiled: {reason}"
                    )),
                );
                return false;
            }
        }
        for p in init_prerequisites(self.unit, class) {
            if !self.check_init(&p) {
                let reason = self
                    .init_ok
                    .get(&p)
                    .and_then(|r| r.as_ref().err())
                    .cloned()
                    .unwrap_or_else(|| format!("{p} is not in the compilation unit"));
                self.init_ok.insert(name.to_string(), Err(reason));
                return false;
            }
        }
        self.init_ok.insert(name.to_string(), Ok(()));
        true
    }

    /// Pre-analyze all `<clinit>()V` methods reachable along `init_prerequisites` of `names` in parallel.
    fn ensure_inits_batch(&mut self, names: &[String]) {
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = Vec::new();
        let mut clinits: Vec<MethodKey> = Vec::new();
        for n in names {
            if !self.init_ok.contains_key(n) && visited.insert(n.clone()) {
                stack.push(n.clone());
            }
        }
        while let Some(name) = stack.pop() {
            let Some(class) = self.unit.get(&name) else {
                continue;
            };
            if structural_problem(self.unit, class).is_some() {
                continue;
            }
            if class
                .methods
                .iter()
                .any(|m| m.name == "<clinit>" && m.descriptor == "()V")
            {
                clinits.push((name.clone(), "<clinit>".to_string(), "()V".to_string()));
            }
            for p in init_prerequisites(self.unit, class) {
                if !self.init_ok.contains_key(&p) && visited.insert(p.clone()) {
                    stack.push(p);
                }
            }
        }
        self.ensure_analyzed(&clinits);
        for n in names {
            self.check_init(n);
        }
    }
}

/// Flood blocker categories through the dependency graph built from `nodes` and `dependents`.
fn flood_poisoned(
    unit: &Unit,
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    init_ok: &HashMap<String, std::result::Result<(), String>>,
    nodes: Vec<Node>,
    dependents: Vec<Vec<usize>>,
    ok_keys: impl IntoIterator<Item = MethodKey>,
) -> BTreeMap<MethodKey, Vec<String>> {
    let mut seeds: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (at, node) in nodes.iter().enumerate() {
        let category = match node {
            Node::Method(m) => match results.get(m) {
                Some(Ok(_)) => continue,
                Some(Err(reason)) => blocker_category(reason),
                None => MISSING_METHOD.to_string(),
            },
            Node::Class(c) => match init_ok.get(c) {
                Some(Ok(())) => continue,
                Some(Err(reason)) => blocker_category(reason),
                None if unit.get(c).is_some() => unreachable!("class init_ok missing for {c}"),
                None => OUTSIDE_CLASS.to_string(),
            },
        };
        seeds.entry(category).or_default().push(at);
    }

    let mut out: BTreeMap<MethodKey, Vec<String>> = BTreeMap::new();
    for key in ok_keys {
        out.insert(key, Vec::new());
    }
    let mut seen = vec![false; nodes.len()];
    let mut work: Vec<usize> = Vec::new();
    for (category, starts) in &seeds {
        seen.fill(false);
        work.clear();
        work.extend(starts);
        while let Some(at) = work.pop() {
            if seen[at] {
                continue;
            }
            seen[at] = true;
            if let Node::Method(key) = &nodes[at]
                && let Some(categories) = out.get_mut(key)
            {
                categories.push(category.clone());
            }
            for &next in &dependents[at] {
                if !seen[next] {
                    work.push(next);
                }
            }
        }
    }
    out
}

/// For every compiled method, the blocker categories whose not-compiled code it can reach,
/// sorted. Empty means self-contained: every method it may call directly and every class it may
/// initialize (with its whole initializer) is native too.
fn poisoned_by(
    unit: &Unit,
    classes: &[ClassFile],
    results: &BTreeMap<MethodKey, std::result::Result<(Signature, Analyzed), String>>,
    init_ok: &HashMap<String, std::result::Result<(), String>>,
) -> BTreeMap<MethodKey, Vec<String>> {
    let mut index: HashMap<Node, usize> = HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut dependents: Vec<Vec<usize>> = Vec::new();
    for (key, result) in results {
        let Ok((_, a)) = result else { continue };
        let me = intern_node(
            Node::Method(key.clone()),
            &mut index,
            &mut nodes,
            &mut dependents,
        );
        if key.1 != "<clinit>" {
            let class = intern_node(
                Node::Class(key.0.clone()),
                &mut index,
                &mut nodes,
                &mut dependents,
            );
            dependents[class].push(me);
        }
        for dep in &a.deps {
            let from = match dep {
                Dep::Method(m) => Node::Method(m.clone()),
                Dep::Init(c) => Node::Class(c.clone()),
            };
            let from = intern_node(from, &mut index, &mut nodes, &mut dependents);
            dependents[from].push(me);
        }
    }
    for class in classes {
        let me = intern_node(
            Node::Class(class.name.clone()),
            &mut index,
            &mut nodes,
            &mut dependents,
        );
        let clinit = (
            class.name.clone(),
            "<clinit>".to_string(),
            "()V".to_string(),
        );
        if results.contains_key(&clinit) {
            let from = intern_node(
                Node::Method(clinit),
                &mut index,
                &mut nodes,
                &mut dependents,
            );
            dependents[from].push(me);
        }
        for p in init_prerequisites(unit, class) {
            let from = intern_node(Node::Class(p), &mut index, &mut nodes, &mut dependents);
            dependents[from].push(me);
        }
    }
    let ok_keys = results
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(k, _)| k.clone());
    flood_poisoned(unit, results, init_ok, nodes, dependents, ok_keys)
}

/// Demand-driven `poisoned_by` scoped to the transitive dependency closure of `scope_ok`.
fn poisoned_by_scoped(
    cache: &mut AnalysisCache,
    scope_ok: &[MethodKey],
) -> BTreeMap<MethodKey, Vec<String>> {
    let mut index: HashMap<Node, usize> = HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut dependents: Vec<Vec<usize>> = Vec::new();
    let mut seen_methods: HashSet<MethodKey> = HashSet::new();
    let mut seen_classes: HashSet<String> = HashSet::new();
    let mut method_work: Vec<MethodKey> = scope_ok.to_vec();
    let mut class_work: Vec<String> = Vec::new();

    while !method_work.is_empty() || !class_work.is_empty() {
        while let Some(key) = method_work.pop() {
            if !seen_methods.insert(key.clone()) {
                continue;
            }
            cache.ensure_one(&key);
            let me = intern_node(
                Node::Method(key.clone()),
                &mut index,
                &mut nodes,
                &mut dependents,
            );
            let deps: Vec<Dep> = match cache.results.get(&key) {
                Some(Ok((_, a))) => a.deps.iter().cloned().collect(),
                _ => continue,
            };
            if key.1 != "<clinit>" {
                let class = intern_node(
                    Node::Class(key.0.clone()),
                    &mut index,
                    &mut nodes,
                    &mut dependents,
                );
                dependents[class].push(me);
                if !seen_classes.contains(&key.0) {
                    class_work.push(key.0.clone());
                }
            }
            for dep in deps {
                let from = match &dep {
                    Dep::Method(m) => {
                        if !seen_methods.contains(m) {
                            method_work.push(m.clone());
                        }
                        Node::Method(m.clone())
                    }
                    Dep::Init(c) => {
                        if !seen_classes.contains(c) {
                            class_work.push(c.clone());
                        }
                        Node::Class(c.clone())
                    }
                };
                let from = intern_node(from, &mut index, &mut nodes, &mut dependents);
                dependents[from].push(me);
            }
        }
        while let Some(c) = class_work.pop() {
            if !seen_classes.insert(c.clone()) {
                continue;
            }
            cache.check_init(&c);
            let me = intern_node(
                Node::Class(c.clone()),
                &mut index,
                &mut nodes,
                &mut dependents,
            );
            let Some(class) = cache.unit.get(&c) else {
                continue;
            };
            if class
                .methods
                .iter()
                .any(|m| m.name == "<clinit>" && m.descriptor == "()V")
            {
                let clinit = (c.clone(), "<clinit>".to_string(), "()V".to_string());
                cache.ensure_one(&clinit);
                if cache.results.contains_key(&clinit) {
                    if !seen_methods.contains(&clinit) {
                        method_work.push(clinit.clone());
                    }
                    let from = intern_node(
                        Node::Method(clinit),
                        &mut index,
                        &mut nodes,
                        &mut dependents,
                    );
                    dependents[from].push(me);
                }
            }
            for p in init_prerequisites(cache.unit, class) {
                if !seen_classes.contains(&p) {
                    class_work.push(p.clone());
                }
                let from = intern_node(Node::Class(p), &mut index, &mut nodes, &mut dependents);
                dependents[from].push(me);
            }
        }
    }

    flood_poisoned(
        cache.unit,
        &cache.results,
        &cache.init_ok,
        nodes,
        dependents,
        scope_ok.iter().cloned(),
    )
}

/// Compile a set of classes together into one C unit.
pub fn compile_unit(classes: &[ClassFile]) -> CompiledUnit {
    compile_unit_rooted(classes, None)
}

/// What emission needs: the methods to emit, the classes needing a runtime record, the classes
/// that are instantiated (whose vtables must hold real code), and the classes to initialize.
struct Emission {
    methods: BTreeSet<MethodKey>,
    records: BTreeSet<String>,
    instantiated: BTreeSet<String>,
    inits: BTreeSet<String>,
}

/// Where reachability starts, and what it needs beyond the compiled methods.
struct ReachFrom<'a> {
    roots: &'a BTreeSet<String>,
    extras: &'a UnitExtras,
    /// The compiled methods of `extras.executed_methods` (see executed_roots).
    executed: &'a [MethodKey],
    /// Generated proxy classes from `proxy::generate`.
    proxies: &'a [crate::proxy::Generated],
}

/// Demand-driven, wave-parallel reachability from `roots` (only analyzes queried methods).
fn reachable<'a>(
    unit: &'a Unit<'a>,
    classes: &'a [ClassFile],
    cache: &RefCell<AnalysisCache<'a>>,
    from: &ReachFrom<'_>,
) -> Emission {
    let ReachFrom {
        roots,
        extras,
        executed,
        proxies,
    } = *from;
    let loaded = &extras.loaded_classes;
    let startup = extras.startup;
    let mut out = Emission {
        methods: BTreeSet::new(),
        records: BTreeSet::new(),
        instantiated: BTreeSet::new(),
        inits: BTreeSet::new(),
    };
    let mut work: Vec<MethodKey> = Vec::new();
    let mut pending_inits: Vec<String> = Vec::new();
    let mut startup_enqueued = false;
    let enqueue_startup =
        |work: &mut Vec<MethodKey>, pending_inits: &mut Vec<String>, done: &mut bool| {
            if *done {
                return;
            }
            *done = true;
            let startup_key = (
                SYSTEM.to_string(),
                "initPhase1".to_string(),
                "()V".to_string(),
            );
            if cache.borrow_mut().is_compiled(&startup_key) {
                work.push(startup_key);
                pending_inits.push(SYSTEM.to_string());
                pending_inits.extend(VM_PREINITIALIZED.iter().map(|c| c.to_string()));
                pending_inits.push(BOOT_LOADER.to_string());
                pending_inits.push("java/lang/ClassLoader".to_string());
                work.push((
                    "jdk/internal/misc/VM".to_string(),
                    "initLevel".to_string(),
                    "(I)V".to_string(),
                ));
            }
        };
    if startup || roots.contains(APP_LOADER) {
        enqueue_startup(&mut work, &mut pending_inits, &mut startup_enqueued);
    }
    for class in classes.iter().filter(|c| roots.contains(&c.name)) {
        pending_inits.push(class.name.clone());
        for m in &class.methods {
            let key = (class.name.clone(), m.name.clone(), m.descriptor.clone());
            if matches!(cache.borrow().results.get(&key), Some(Ok(_))) {
                work.push(key);
            }
        }
    }
    // A record for `class` also needs records for its superclasses and superinterfaces.
    let add_records = |name: &str, records: &mut BTreeSet<String>| {
        if unit.get(name).is_some() {
            records.insert(name.to_string());
        }
        if let Ok(chain) = unit.chain(name) {
            for c in chain.iter() {
                records.insert(c.name.clone());
            }
        }
        if let Ok(ifaces) = unit.superinterfaces_arc(name) {
            for i in ifaces.iter() {
                records.insert(i.name.clone());
            }
        }
    };
    // Reverse index for Rapid Type Analysis: `(name, descriptor) -> [MethodKey]` of instantiated classes.
    let mut called: HashSet<(String, String)> = HashSet::new();
    let mut impls_by_sig: HashMap<(String, String), Vec<MethodKey>> = HashMap::new();
    let mut seen_impls: HashSet<MethodKey> = HashSet::new();
    let instantiate = |class: &str,
                       instantiated: &mut BTreeSet<String>,
                       called: &HashSet<(String, String)>,
                       impls_by_sig: &mut HashMap<(String, String), Vec<MethodKey>>,
                       seen_impls: &mut HashSet<MethodKey>,
                       work: &mut Vec<MethodKey>| {
        if !instantiated.insert(class.to_string()) {
            return;
        }
        let mut register = |key: MethodKey| {
            if seen_impls.insert(key.clone()) {
                let sig = (key.1.clone(), key.2.clone());
                if called.contains(&sig) {
                    work.push(key.clone());
                }
                impls_by_sig.entry(sig).or_default().push(key);
            }
        };
        if let Ok(vt) = unit.vtable(class) {
            for slot in vt.iter() {
                register((
                    slot.owner.clone(),
                    slot.name.clone(),
                    slot.descriptor.clone(),
                ));
            }
        }
        for e in unit.itable(class).iter() {
            if let ItableTarget::Impl(k) = &e.target {
                register(k.clone());
            }
        }
    };
    let call_selector = |name: String,
                         descriptor: String,
                         called: &mut HashSet<(String, String)>,
                         impls_by_sig: &HashMap<(String, String), Vec<MethodKey>>,
                         work: &mut Vec<MethodKey>| {
        let sig = (name, descriptor);
        if called.insert(sig.clone())
            && let Some(impls) = impls_by_sig.get(&sig)
        {
            work.extend(impls.iter().cloned());
        }
    };

    for key in executed.iter() {
        add_records(&key.0, &mut out.records);
        pending_inits.push(key.0.clone());
        if key.1 != "<clinit>" {
            work.push(key.clone());
        }
    }
    if roots.contains(APP_LOADER) && unit.get(VAR_HANDLE).is_some() {
        add_records(VAR_HANDLE, &mut out.records);
    }
    let mut loaded = loaded.clone();
    if startup || roots.contains(APP_LOADER) {
        loaded.extend(
            classes
                .iter()
                .filter(|c| c.name.starts_with("sun/nio/cs/") && !c.name[11..].contains('/'))
                .filter(|c| unit.is_subclass(&c.name, "java/nio/charset/Charset") == Some(true))
                .map(|c| c.name.clone()),
        );
    }
    for name in loaded.iter().map(String::as_str) {
        let Some(class) = unit.get(name) else {
            continue;
        };
        if unit.chain(name).is_err() {
            continue;
        }
        add_records(name, &mut out.records);
        pending_inits.push(name.to_string());
        if class.access & (ACC_INTERFACE | ACC_ABSTRACT) == 0 {
            work.push((name.to_string(), "<init>".to_string(), "()V".to_string()));
            instantiate(
                name,
                &mut out.instantiated,
                &called,
                &mut impls_by_sig,
                &mut seen_impls,
                &mut work,
            );
        }
    }
    for (name, _) in VM_EXCEPTIONS {
        if unit.get(name).is_some() && unit.chain(name).is_ok() {
            add_records(name, &mut out.records);
            instantiate(
                name,
                &mut out.instantiated,
                &called,
                &mut impls_by_sig,
                &mut seen_impls,
                &mut work,
            );
        }
    }
    let mut needs_classes = false;
    let mut needs_classes_done = false;
    let mut unsafe_init_reached = false;
    let mut unsafe_alloc_reached = false;
    let mut annotation_loads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut annotation_loads_done: HashSet<String> = HashSet::new();
    let mut annotation_parser_seen = false;
    loop {
        if needs_classes && !needs_classes_done {
            needs_classes_done = true;
            let thread_classes = [
                "java/lang/Thread",
                "java/lang/Thread$FieldHolder",
                "java/lang/ThreadGroup",
                OBJECT,
            ];
            for name in [CLASS, STRING].into_iter().chain(thread_classes) {
                if unit.get(name).is_none() {
                    continue;
                }
                add_records(name, &mut out.records);
                let is_new = !out.instantiated.contains(name);
                instantiate(
                    name,
                    &mut out.instantiated,
                    &called,
                    &mut impls_by_sig,
                    &mut seen_impls,
                    &mut work,
                );
                if is_new {
                    pending_inits.push(name.to_string());
                }
            }
        }
        if !pending_inits.is_empty() {
            let batch_inits: Vec<String> = pending_inits
                .drain(..)
                .filter(|c| !out.inits.contains(c))
                .collect();
            if !batch_inits.is_empty() {
                cache.borrow_mut().ensure_inits_batch(&batch_inits);
                let mut stack = batch_inits;
                while let Some(class) = stack.pop() {
                    if !cache.borrow_mut().check_init(&class) || !out.inits.insert(class.clone()) {
                        continue;
                    }
                    work.push((class.clone(), "<clinit>".to_string(), "()V".to_string()));
                    if let Some(c) = unit.get(&class) {
                        stack.extend(init_prerequisites(unit, c));
                    }
                }
            }
        }
        if work.is_empty() {
            if !annotation_parser_seen {
                annotation_parser_seen = out
                    .methods
                    .iter()
                    .any(|m| m.0 == "sun/reflect/annotation/AnnotationParser");
            }
            if annotation_parser_seen {
                for name in &out.records {
                    if annotation_loads_done.insert(name.clone())
                        && let Some(class) = unit.get(name)
                    {
                        let names = crate::proxy::annotation_loaded_classes(class);
                        if !names.is_empty() {
                            annotation_loads.insert(name.clone(), names);
                        }
                    }
                }
            }
            let reached = {
                let compiled_fn = |k: &MethodKey| cache.borrow_mut().is_compiled(k);
                let reach_inputs = reflect::ReachInputs {
                    proxies,
                    annotation_loads: &annotation_loads,
                };
                reflect::reach(
                    unit,
                    &out.records,
                    &out.instantiated,
                    &out.methods,
                    &compiled_fn,
                    &reach_inputs,
                )
            };
            let mut stack_records = stack::reach(unit, &out.records, &out.methods);
            stack_records.extend(boot_extras::reach(unit, &out.records, &out.methods));
            stack_records.extend(method_handles::reach(unit, &out.records, &out.methods));
            let mut progressed = !reached.is_empty() || !stack_records.is_empty();
            for class in &stack_records {
                add_records(class, &mut out.records);
            }
            for class in &reached.recorded {
                add_records(class, &mut out.records);
            }
            for class in reached.instantiated {
                add_records(&class, &mut out.records);
                pending_inits.push(class.clone());
                instantiate(
                    &class,
                    &mut out.instantiated,
                    &called,
                    &mut impls_by_sig,
                    &mut seen_impls,
                    &mut work,
                );
            }
            work.extend(reached.methods);
            if unsafe_init_reached || unsafe_alloc_reached {
                let recorded: Vec<String> = out.records.iter().cloned().collect();
                let mut check_candidates = Vec::new();
                for class in &recorded {
                    if !out.inits.contains(class) {
                        check_candidates.push(class.clone());
                    }
                }
                if !check_candidates.is_empty() {
                    cache.borrow_mut().ensure_inits_batch(&check_candidates);
                }
                for class in recorded {
                    if cache.borrow_mut().check_init(&class) && !out.inits.contains(&class) {
                        pending_inits.push(class.clone());
                        progressed = true;
                    }
                    if unsafe_alloc_reached
                        && let Some(c) = unit.get(&class)
                        && c.access & (ACC_INTERFACE | ACC_ABSTRACT) == 0
                        && unit.chain(&class).is_ok()
                        && !out.instantiated.contains(&class)
                    {
                        progressed = true;
                        instantiate(
                            &class,
                            &mut out.instantiated,
                            &called,
                            &mut impls_by_sig,
                            &mut seen_impls,
                            &mut work,
                        );
                    }
                }
            }
            if !progressed {
                break;
            }
            continue;
        }
        // Wave-parallel analysis of all pending methods in `work`.
        let mut wave: Vec<MethodKey> = Vec::new();
        let mut seen_wave: HashSet<MethodKey> = HashSet::new();
        for key in work.drain(..) {
            if !out.methods.contains(&key) && seen_wave.insert(key.clone()) {
                wave.push(key);
            }
        }
        cache.borrow_mut().ensure_analyzed(&wave);
        for key in wave {
            let compiled = matches!(cache.borrow().results.get(&key), Some(Ok(_)));
            if !compiled || !out.methods.insert(key.clone()) {
                continue;
            }
            if key.0 == "java/lang/StackTraceElement$HashedModules"
                && key.1 == "hashedModules"
                && key.2 == "()Ljava/util/Set;"
            {
                work.push((
                    "java/util/Collections".to_string(),
                    "emptySet".to_string(),
                    "()Ljava/util/Set;".to_string(),
                ));
            }
            if key.0 == CLASS && key.1 == "getModule" && key.2 == "()Ljava/lang/Module;" {
                enqueue_startup(&mut work, &mut pending_inits, &mut startup_enqueued);
                if unit.get(BOOT_LOADER).is_some() {
                    pending_inits.push(BOOT_LOADER.to_string());
                }
                if !extras.app_classes.is_empty() && unit.get(APP_LOADER).is_some() {
                    add_records(APP_LOADER, &mut out.records);
                    pending_inits.push(APP_LOADER.to_string());
                }
            }
            if (key.0 == UNSAFE || key.0 == "sun/misc/Unsafe")
                && matches!(
                    (key.1.as_str(), key.2.as_str()),
                    ("ensureClassInitialized", "(Ljava/lang/Class;)V")
                        | ("ensureClassInitialized0", "(Ljava/lang/Class;)V")
                        | ("shouldBeInitialized", "(Ljava/lang/Class;)Z")
                        | ("shouldBeInitialized0", "(Ljava/lang/Class;)Z")
                        | ("allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;")
                )
            {
                unsafe_init_reached = true;
                if key.1 == "allocateInstance" {
                    unsafe_alloc_reached = true;
                }
            }
            if key.0 == THREAD && key.1 == "start0" && key.2 == "()V" {
                for (name, descriptor) in [
                    ("exit", "()V"),
                    ("dispatchUncaughtException", "(Ljava/lang/Throwable;)V"),
                ] {
                    work.push((THREAD.to_string(), name.to_string(), descriptor.to_string()));
                }
                call_selector(
                    "run".to_string(),
                    "()V".to_string(),
                    &mut called,
                    &impls_by_sig,
                    &mut work,
                );
            }
            let mut from_c = net::called_by(&key);
            from_c.extend(nio_fs::called_by(&key));
            from_c.extend(modules::called_by(&key, !extras.jdk_modules.is_empty()));
            from_c.extend(stack::called_by(&key));
            from_c.extend(method_handles::called_by(&key));
            from_c.extend(unsafe_rt::called_by(&key));
            from_c.extend(boot_extras::called_by(&key));
            from_c.extend(zip::called_by(&key));
            if let Some((class, descriptor)) = management::constructed_by(&key) {
                from_c.push((class, "<init>", descriptor));
            }
            for (class, name, descriptor) in from_c {
                if unit.get(class).is_none() || unit.chain(class).is_err() {
                    continue;
                }
                add_records(class, &mut out.records);
                pending_inits.push(class.to_string());
                work.push((class.to_string(), name.to_string(), descriptor.to_string()));
                if name == "<init>" {
                    instantiate(
                        class,
                        &mut out.instantiated,
                        &called,
                        &mut impls_by_sig,
                        &mut seen_impls,
                        &mut work,
                    );
                }
            }
            let cache_ref = cache.borrow();
            let Some(Ok((_, analyzed))) = cache_ref.results.get(&key) else {
                continue;
            };
            if cache_ref.intrinsics.get(&key).is_some_and(|(_, nc)| *nc) {
                needs_classes = true;
            }
            for catch in analyzed.dispatch.iter().flatten() {
                if let Some(c) = &catch.class {
                    add_records(c, &mut out.records);
                }
            }
            pending_inits.push(key.0.clone());
            for dep in &analyzed.deps {
                match dep {
                    Dep::Method(m) => work.push(m.clone()),
                    Dep::Init(c) => pending_inits.push(c.clone()),
                }
            }
            for resolved in analyzed.resolved.values() {
                let target = match resolved {
                    Resolved::Call(
                        CallKind::Virtual { target, .. } | CallKind::Interface { target, .. },
                    ) => target,
                    _ => continue,
                };
                call_selector(
                    target.1.clone(),
                    target.2.clone(),
                    &mut called,
                    &impls_by_sig,
                    &mut work,
                );
            }
            if analyzed.insns.iter().any(|i| {
                matches!(&i.op, Op::InvokeVirtual(m) | Op::InvokeSpecial(m)
                if (m.class == UNSAFE || m.class == "sun/misc/Unsafe")
                && matches!(
                    (m.name.as_str(), m.descriptor.as_str()),
                    ("ensureClassInitialized", "(Ljava/lang/Class;)V")
                        | ("ensureClassInitialized0", "(Ljava/lang/Class;)V")
                        | ("shouldBeInitialized", "(Ljava/lang/Class;)Z")
                        | ("shouldBeInitialized0", "(Ljava/lang/Class;)Z")
                        | ("allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;")
                ))
            }) {
                unsafe_init_reached = true;
            }
            if analyzed.insns.iter().any(|i| {
                matches!(&i.op, Op::InvokeVirtual(m) | Op::InvokeSpecial(m)
                    if (m.class == UNSAFE || m.class == "sun/misc/Unsafe")
                    && m.name == "allocateInstance"
                    && m.descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;")
            }) {
                unsafe_alloc_reached = true;
            }
            for insn in &analyzed.insns {
                if caller_sensitive(unit, &insn.op) && unit.get(&key.0).is_some() {
                    add_records(&key.0, &mut out.records);
                }
                match &insn.op {
                    Op::New { class } => {
                        if unit.get(class).is_some() {
                            add_records(class, &mut out.records);
                            instantiate(
                                class,
                                &mut out.instantiated,
                                &called,
                                &mut impls_by_sig,
                                &mut seen_impls,
                                &mut work,
                            );
                        }
                    }
                    Op::LdcClass(name) => {
                        needs_classes = true;
                        if let Some(c) = name
                            .strip_prefix('[')
                            .and_then(|_| element_class(name))
                            .or_else(|| (!name.starts_with('[')).then(|| name.clone()))
                            .filter(|c| !c.starts_with('[') && unit.get(c).is_some())
                        {
                            add_records(&c, &mut out.records);
                        }
                    }
                    Op::LdcString(_) => {
                        add_records(STRING, &mut out.records);
                        instantiate(
                            STRING,
                            &mut out.instantiated,
                            &called,
                            &mut impls_by_sig,
                            &mut seen_impls,
                            &mut work,
                        );
                    }
                    Op::CheckCast { class } | Op::InstanceOf { class }
                        if class.starts_with('[') =>
                    {
                        if let Some(c) = element_class(class)
                            .filter(|c| !c.starts_with('[') && unit.get(c).is_some())
                        {
                            add_records(&c, &mut out.records);
                        }
                    }
                    Op::MultiANewArray { class, .. } => {
                        if let Some(c) = innermost_class(class).filter(|c| unit.get(c).is_some()) {
                            add_records(c, &mut out.records);
                        }
                    }
                    Op::ANewArray { class }
                    | Op::CheckCast { class }
                    | Op::InstanceOf { class }
                        if !class.starts_with('[') && unit.get(class).is_some() =>
                    {
                        add_records(class, &mut out.records);
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// The JABS runtime's application class loader (runtime/java/jabs/rt/AppLoader.java).
const APP_LOADER: &str = "jabs/rt/AppLoader";
const SYSTEM: &str = "java/lang/System";
const BOOT_LOADER: &str = "jdk/internal/loader/BootLoader";

/// Classes HotSpot initializes itself before `System.initPhase1`, in its order
/// (`Threads::initialize_java_lang_classes`). Finalizer's initialization registers the
/// JavaLangRefAccess that initPhase1 uses; UnsafeConstants receives VM-injected values.
const VM_PREINITIALIZED: &[&str] = &[
    "java/lang/String",
    "java/lang/System",
    "java/lang/Class",
    "java/lang/ThreadGroup",
    "java/lang/Thread",
    "java/lang/Module",
    "jdk/internal/misc/UnsafeConstants",
    "java/lang/reflect/Method",
    "java/lang/ref/Finalizer",
    "java/lang/OutOfMemoryError",
    "java/lang/NullPointerException",
    "java/lang/ClassCastException",
    "java/lang/ArrayStoreException",
    "java/lang/ArithmeticException",
    "java/lang/StackOverflowError",
    "java/lang/IllegalMonitorStateException",
    "java/lang/IllegalArgumentException",
];

/// Host queries and standard-stream I/O (handwritten, OS-specific).
const PLATFORM_C: &str = include_str!("../runtime/jabs_platform.c");
/// Files and descriptors (handwritten, OS-specific).
const FILES_C: &str = include_str!("../runtime/jabs_files.c");
/// The program's .jab and the virtual tree the file layer serves from it (handwritten).
const JAB_C: &str = include_str!("../runtime/jabs_jab.c");
/// java.io natives' Java-side glue (handwritten, OS-neutral; needs the class registry).
const IO_C: &str = include_str!("../runtime/jabs_io.c");
/// Monitors and atomic accesses (handwritten, OS-specific).
const SYNC_C: &str = include_str!("../runtime/jabs_sync.c");
/// Java threads on OS threads (handwritten, OS-specific).
const THREAD_C: &str = include_str!("../runtime/jabs_thread.c");
/// java.lang.management natives' glue (handwritten, OS-neutral; needs the class registry).
const MANAGEMENT_C: &str = include_str!("../runtime/jabs_management.c");
/// java.net and java.nio natives' glue: resolver, sockets, selectors (handwritten, OS-specific;
/// needs the class registry).
const NET_C: &str = include_str!("../runtime/jabs_net.c");
/// java.nio.file natives (handwritten, OS-specific; needs the class registry).
const NIO_FS_C: &str = include_str!("../runtime/jabs_nio_fs.c");
/// java.util.zip natives: Inflater and Deflater (handwritten, OS-neutral; needs the class registry).
const ZIP_C: &str = include_str!("../runtime/jabs_zip.c");
/// Cooperative Stop-The-World Immix + Large Object Space Garbage Collector (handwritten).
const GC_C: &str = include_str!("../runtime/jabs_gc.c");
/// Reflection: member listings and reflective calls (handwritten).
const REFLECT_C: &str = include_str!("../runtime/jabs_reflect.c");
/// Reflection's metadata layout and entry points (handwritten; part of every program's header).
const REFLECT_H: &str = include_str!("../runtime/jabs_reflect.h");
/// Named modules: building Module objects from the generated tables (handwritten).
const MODULES_C: &str = include_str!("../runtime/jabs_modules.c");
/// Named modules' tables and glue (handwritten; part of every program's header).
const MODULES_H: &str = include_str!("../runtime/jabs_modules.h");

/// What a program needs beyond its classes.
pub struct UnitExtras {
    /// Classes the application class loader defines (the rest belong to the boot loader).
    pub app_classes: BTreeSet<String>,
    /// Non-class files from the input jars, embedded for the app class loader: (name, contents).
    pub resources: Vec<(String, Vec<u8>)>,
    /// `java.home` for the program: the JDK whose java.base was compiled in (its lib/ files are
    /// still read from there). None means the executable's directory.
    pub java_home: Option<String>,
    /// Classes a real JVM run of this program loaded (`-Xlog:class+load`). Each may be loaded by
    /// name, so its record, initializer and no-arg constructor are kept and it may be instantiated.
    pub loaded_classes: BTreeSet<String>,
    /// Methods a real JVM run of this program executed (`jabs::jar::read_method_trace`). Code
    /// may reach a method only by a name computed at run time (Method.invoke), which no
    /// analysis of the instructions can find, so each one the unit compiles is a root, with
    /// its class recorded and initialized ([`ExecutedRoots`] counts what is set aside).
    ///
    /// No class is marked instantiated because of the trace. An executed instance method proves
    /// that an object of its class *or of any subclass* existed, so it cannot say which class
    /// needs a real vtable: marking the declaring class would give real vtables to bases only
    /// ever instantiated through subclasses (HashMap when only LinkedHashMap was made), and to
    /// abstract classes and interfaces, which have none. Natively an object exists only through
    /// a creation path JABS compiles, and each already marks its class: `new` (RTA),
    /// Constructor.newInstance (reflect::reach instantiates every recorded concrete class
    /// whose constructor is reachable, which includes the classes of executed constructors
    /// once reflection is in the program), `loaded_classes`, VM exceptions and the management
    /// glue.
    pub executed_methods: BTreeSet<MethodKey>,
    /// Compile in the JVM's startup (System.initPhase1, run by `jabs_vm_startup()`). Implied by
    /// a whole program (one with the app class loader).
    pub startup: bool,
    /// The program has a .jab (src/jab.rs): its class path and java.home come from it at run
    /// time (runtime/jabs_jab.c) and `resources` should be empty (the .jab holds them).
    pub jab: Option<JabConfig>,
    /// Every class on the program's class path: its input jars and every module of the JDK it
    /// runs on (`jdk::all_class_names`). With it, a class the code names that is in neither is
    /// proved absent, and an instruction naming it throws NoClassDefFoundError as on the JVM
    /// (Netty probes for SLF4J this way). Without it, such an instruction stops, as before.
    pub known_classes: Option<BTreeSet<String>>,
    /// The JDK's modules, from its module-info.class files (`jdk::modules`): with them a JDK
    /// class's Class.getModule is its named module, with the exports, opens and reads its
    /// declaration gives (src/cgen/modules.rs). Empty: every class is in its loader's unnamed
    /// module (no module encapsulation).
    pub jdk_modules: Vec<crate::jdk::JdkModule>,
    /// Whether to also assemble the monolithic `CompiledUnit::c_source` string.
    /// Defaults to `true`; CLI `--out-dir` without `--out` sets this to `false`.
    pub emit_single_source: bool,
}

impl Default for UnitExtras {
    fn default() -> Self {
        UnitExtras {
            app_classes: BTreeSet::new(),
            resources: Vec::new(),
            java_home: None,
            loaded_classes: BTreeSet::new(),
            executed_methods: BTreeSet::new(),
            startup: false,
            jab: None,
            known_classes: None,
            jdk_modules: Vec::new(),
            emit_single_source: true,
        }
    }
}

/// A program's .jab, as the generated C knows it.
#[derive(Clone, Debug, Default)]
pub struct JabConfig {
    /// The archive's id (`jab::Builder::finish`): the runtime refuses any other .jab.
    pub id: String,
    /// A recording build: java.home stays `UnitExtras::java_home` (the build JDK) and every path
    /// the run touches under it is appended to this file, the list `--java-home-files` reads.
    pub record_log: Option<String>,
}

/// Split a JVM run's executed methods into the compiled ones of the unit, which become roots,
/// and counts of the rest. A hidden class's name never matches a unit class, so its methods
/// count as outside the unit (read_method_trace already sets them aside and counts them).
fn executed_roots(
    unit: &Unit,
    executed: &BTreeSet<MethodKey>,
    is_compiled: &mut dyn FnMut(&MethodKey) -> bool,
) -> (Vec<MethodKey>, ExecutedRoots) {
    let mut roots = Vec::new();
    let mut counts = ExecutedRoots::default();
    for key in executed {
        let Some(class) = unit.get(&key.0) else {
            counts.class_not_in_unit += 1;
            continue;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| m.name == key.1 && m.descriptor == key.2)
        else {
            counts.method_not_in_class += 1;
            continue;
        };
        if method.access & ACC_ABSTRACT != 0 {
            counts.abstract_in_unit += 1;
            continue;
        }
        if !is_compiled(key) {
            counts.not_compiled += 1;
            continue;
        }
        counts.roots += 1;
        roots.push(key.clone());
    }
    (roots, counts)
}

/// Group items into up to `max_shards` non-empty strings, keeping at least `min_per_shard` items each.
fn shard_strings(items: &[String], max_shards: usize, min_per_shard: usize) -> Vec<String> {
    if items.is_empty() {
        return Vec::new();
    }
    let shards = (items.len() / min_per_shard.max(1))
        .clamp(1, max_shards.max(1))
        .min(items.len());
    let per = items.len().div_ceil(shards);
    items
        .chunks(per)
        .map(|chunk| {
            let cap: usize = chunk.iter().map(String::len).sum();
            let mut s = String::with_capacity(cap);
            for item in chunk {
                s.push_str(item);
            }
            s
        })
        .collect()
}

fn strip_includes(text: &str) -> String {
    text.replacen("#include \"jabs_unit.h\"\n", "", 1)
        .replacen("#include \"jabs_runtime.h\"\n", "", 1)
        .replacen("#include \"jabs_unit_internal.h\"\n", "", 1)
}

/// Compile a set of classes together into one C unit, emitting only what is reachable from
/// `roots` (all classes when None). Every method's status is reported either way.
pub fn compile_unit_rooted(
    classes: &[ClassFile],
    roots: Option<&BTreeSet<String>>,
) -> CompiledUnit {
    compile_unit_with(classes, roots, &UnitExtras::default())
}

/// `compile_unit_rooted` plus class-loader identity and embedded resources.
pub fn compile_unit_with(
    classes: &[ClassFile],
    roots: Option<&BTreeSet<String>>,
    extras: &UnitExtras,
) -> CompiledUnit {
    compile_unit_owned(classes.to_vec(), roots, extras)
}

/// Like `compile_unit_with`, taking ownership of `classes` to avoid cloning the entire unit.
pub fn compile_unit_owned(
    mut owned: Vec<ClassFile>,
    roots: Option<&BTreeSet<String>>,
    extras: &UnitExtras,
) -> CompiledUnit {
    // invokedynamic call sites become calls into generated classes (see indy.rs).
    let indy = crate::indy::desugar(&mut owned);
    // java.lang.reflect.Proxy classes, generated for every interface list the program can ask
    // for (see proxy.rs).
    let proxies = crate::proxy::generate(&mut owned);
    let classes: &[ClassFile] = &owned;
    // A generated class is defined by its call site's class loader. desugar records the call
    // site of each one; the class name is not parsed, so no kind can be missed and no input
    // class that merely contains `$$Lambda$` can be mistaken for one.
    let mut app_classes = extras.app_classes.clone();
    for (generated, call_site) in &indy.defined_by {
        if extras.app_classes.contains(call_site) {
            app_classes.insert(generated.clone());
        }
    }
    // The JVM defines a proxy in the loader Proxy.newProxyInstance is given, which must see
    // every interface: the app loader when one of them is an application class.
    for proxy in &proxies.proxies {
        if proxy
            .interfaces
            .iter()
            .any(|i| extras.app_classes.contains(i))
        {
            app_classes.insert(proxy.name.clone());
        }
    }
    let mut unit = Unit::new(classes);
    unit.known = extras
        .known_classes
        .as_ref()
        .map(|known| known.iter().cloned().collect());
    // The app class loader is created by the runtime itself, so it is always reachable.
    let mut roots = roots.cloned();
    if let Some(roots) = roots.as_mut()
        && unit.get(APP_LOADER).is_some()
    {
        roots.insert(APP_LOADER.to_string());
    }
    let roots = roots.as_ref();
    let have_classes = classes_available(&unit);
    let cache = RefCell::new(AnalysisCache::new(&unit, have_classes));

    let (executed_counts, emission, poisoned, self_contained) = match roots {
        None => {
            let all_methods: Vec<(&ClassFile, &Member)> = classes
                .iter()
                .flat_map(|c| c.methods.iter().map(move |m| (c, m)))
                .collect();
            {
                let mut c = cache.borrow_mut();
                c.analyze_batch(all_methods);
                for class in classes {
                    c.check_init(&class.name);
                }
            }
            let (_executed, executed_counts) =
                executed_roots(&unit, &extras.executed_methods, &mut |k| {
                    matches!(cache.borrow().results.get(k), Some(Ok(_)))
                });
            let c = cache.borrow();
            let compiled: BTreeSet<MethodKey> = c
                .results
                .iter()
                .filter(|(_, r)| r.is_ok())
                .map(|(k, _)| k.clone())
                .collect();
            let initializable: BTreeSet<String> = classes
                .iter()
                .filter(|cls| c.init_ok.get(&cls.name).is_some_and(Result::is_ok))
                .map(|cls| cls.name.clone())
                .collect();
            let poisoned = poisoned_by(&unit, classes, &c.results, &c.init_ok);
            let mut self_contained: BTreeSet<MethodKey> = BTreeSet::new();
            for (key, categories) in &poisoned {
                if categories.is_empty() {
                    self_contained.insert(key.clone());
                }
            }
            let laid_out_all: Vec<&ClassFile> = classes
                .iter()
                .filter(|cls| cls.access & ACC_INTERFACE == 0 && unit.chain(&cls.name).is_ok())
                .collect();
            let emission = Emission {
                methods: compiled,
                records: laid_out_all
                    .iter()
                    .map(|cls| cls.name.clone())
                    .chain(
                        classes
                            .iter()
                            .filter(|cls| cls.access & ACC_INTERFACE != 0)
                            .map(|cls| cls.name.clone()),
                    )
                    .collect(),
                instantiated: laid_out_all.iter().map(|cls| cls.name.clone()).collect(),
                inits: initializable,
            };
            (executed_counts, emission, poisoned, self_contained)
        }
        Some(roots) => {
            // Demand-driven analysis: seed with root classes' methods + non-abstract executed methods.
            let mut seed_keys: Vec<MethodKey> = Vec::new();
            for class in classes.iter().filter(|c| roots.contains(&c.name)) {
                for m in &class.methods {
                    seed_keys.push((class.name.clone(), m.name.clone(), m.descriptor.clone()));
                }
            }
            for key in &extras.executed_methods {
                if let Some(class) = unit.get(&key.0)
                    && let Some(m) = class
                        .methods
                        .iter()
                        .find(|m| m.name == key.1 && m.descriptor == key.2)
                    && m.access & ACC_ABSTRACT == 0
                {
                    seed_keys.push(key.clone());
                }
            }
            cache.borrow_mut().ensure_analyzed(&seed_keys);
            let (executed, executed_counts) =
                executed_roots(&unit, &extras.executed_methods, &mut |k| {
                    cache.borrow_mut().is_compiled(k)
                });
            let emission = reachable(
                &unit,
                classes,
                &cache,
                &ReachFrom {
                    roots,
                    extras,
                    executed: &executed,
                    proxies: &proxies.proxies,
                },
            );
            let mut scope_ok: Vec<MethodKey> = Vec::new();
            {
                let mut seen_ok = HashSet::new();
                let c = cache.borrow();
                for k in emission.methods.iter().chain(executed.iter()) {
                    if matches!(c.results.get(k), Some(Ok(_))) && seen_ok.insert(k.clone()) {
                        scope_ok.push(k.clone());
                    }
                }
            }
            let poisoned = poisoned_by_scoped(&mut cache.borrow_mut(), &scope_ok);
            let mut self_contained: BTreeSet<MethodKey> = BTreeSet::new();
            for (key, categories) in &poisoned {
                if categories.is_empty() {
                    self_contained.insert(key.clone());
                }
            }
            (executed_counts, emission, poisoned, self_contained)
        }
    };
    let cache = cache.into_inner();
    let results = &cache.results;
    let intrinsics = &cache.intrinsics;
    let init_ok = &cache.init_ok;

    let mut laid_out: Vec<&ClassFile> = classes
        .iter()
        .filter(|c| c.access & ACC_INTERFACE == 0 && unit.chain(&c.name).is_ok())
        .collect();
    laid_out.sort_by_key(|c| {
        (
            unit.chain(&c.name).map(|ch| ch.len()).unwrap_or(0),
            c.name.clone(),
        )
    });
    let recorded: Vec<&ClassFile> = laid_out
        .iter()
        .copied()
        .filter(|c| emission.records.contains(&c.name))
        .collect();
    let recorded_names: BTreeSet<&str> = recorded.iter().map(|c| c.name.as_str()).collect();
    let mut interface_records: BTreeSet<&str> = emission
        .records
        .iter()
        .map(String::as_str)
        .filter(|n| unit.is_interface(n))
        .collect();
    for class in &recorded {
        if let Ok(ifaces) = unit.superinterfaces_arc(&class.name) {
            for i in ifaces.iter() {
                interface_records.insert(&i.name);
            }
        }
    }

    // Collect string literals before generating code so `jabs_runtime.h` and emitters share IDs.
    let mut literal_set: BTreeSet<Vec<u16>> = emission
        .methods
        .iter()
        .filter_map(|k| results.get(k)?.as_ref().ok())
        .flat_map(|(_, a)| a.insns.iter())
        .filter_map(|i| match &i.op {
            Op::LdcString(units) => Some(units.clone()),
            _ => None,
        })
        .collect();
    for class in classes.iter().filter(|c| emission.inits.contains(&c.name)) {
        literal_set.extend(
            class
                .fields
                .iter()
                .filter(|f| f.access & ACC_STATIC != 0)
                .filter_map(|f| constant_string(class, f)),
        );
    }
    let literals: Vec<Vec<u16>> = literal_set.into_iter().collect();
    let literal_ids: HashMap<Vec<u16>, u32> = literals
        .iter()
        .enumerate()
        .map(|(i, u)| (u.clone(), i as u32))
        .collect();

    let registry =
        have_classes && emission.records.contains(CLASS) && emission.records.contains(STRING);
    let registry_emitted = registry;

    // 1. jabs_runtime.h (shared prelude without unit-specific layouts/prototypes).
    let mut runtime_header = String::from("#ifndef JABS_RUNTIME_H\n#define JABS_RUNTIME_H\n");
    let _ = writeln!(
        runtime_header,
        "/* Generated by JABS from {} classes. */",
        classes.len()
    );
    runtime_header.push_str(RUNTIME_PRELUDE);
    runtime_header.push('\n');
    if registry {
        runtime_header.push_str("#define JABS_HAVE_REFLECTION 1\n");
    }
    runtime_header.push_str(REFLECT_H);
    runtime_header.push('\n');
    runtime_header.push_str(MODULES_H);
    runtime_header.push('\n');
    if !literals.is_empty() {
        let _ = write!(
            runtime_header,
            "extern _Atomic(jabs_object *) jabs_literal_cache[{count}];\n\
             jabs_object *jabs_literal_get(uint32_t id);\n\
             static inline jabs_object *jabs_literal(uint32_t id) {{\n\
                 jabs_object *cached = atomic_load_explicit(&jabs_literal_cache[id], memory_order_acquire);\n\
                 return cached ? cached : jabs_literal_get(id);\n\
             }}\n",
            count = literals.len()
        );
    }
    runtime_header.push_str("#endif\n");

    // 2. Unit-specific declarations for jabs_unit.h.
    let mut unit_decls = String::from("#include \"jabs_runtime.h\"\n");
    for class in &laid_out {
        emit_object_layout(&mut unit_decls, class);
    }
    for class in classes {
        for method in &class.methods {
            let key = (
                class.name.clone(),
                method.name.clone(),
                method.descriptor.clone(),
            );
            if let (true, Some(Ok((sig, _)))) = (emission.methods.contains(&key), results.get(&key))
            {
                let _ = writeln!(unit_decls, "{};", prototype(&class.name, method, sig));
            }
        }
    }
    for class in emitted_init_targets(&unit, results, &emission) {
        let _ = writeln!(unit_decls, "int {}(void);", init_symbol(&class));
    }
    for key in reached_stub_targets(results, &emission) {
        let Some(class) = unit.get(&key.0) else {
            continue;
        };
        let Some(method) = class
            .methods
            .iter()
            .find(|m| m.name == key.1 && m.descriptor == key.2)
        else {
            continue;
        };
        if let Ok(sig) = unit.signature(&key.2) {
            let _ = writeln!(unit_decls, "{};", prototype(&key.0, method, &sig));
        }
    }

    // 3. Sharded unit_0_* sections.
    let live = |class: &str, key: &MethodKey| {
        emission.instantiated.contains(class) && emission.methods.contains(key)
    };
    let stub_pairs = collect_stubs(&unit, &recorded, &live);
    let mut internal_decls = String::new();
    let mut stub_defs: Vec<String> = Vec::with_capacity(stub_pairs.len());
    for (decl, def) in stub_pairs {
        internal_decls.push_str(&decl);
        stub_defs.push(def);
    }
    let mut reached_stubs_text = String::new();
    emit_reached_stubs(&mut reached_stubs_text, &unit, results, &emission, init_ok);
    if !reached_stubs_text.trim().is_empty() {
        stub_defs.push(reached_stubs_text);
    }
    let stub_shards = shard_strings(&stub_defs, 16, 200);

    let mut record_items: Vec<String> =
        Vec::with_capacity(interface_records.len() + recorded.len());
    for name in &interface_records {
        let mut buf = String::new();
        emit_interface_record(&mut buf, &unit, unit.get(name).unwrap(), &interface_records);
        record_items.push(buf);
    }
    for class in &recorded {
        let mut buf = String::new();
        emit_class_record(&mut buf, &unit, class, &live);
        record_items.push(buf);
    }
    let record_shards = shard_strings(&record_items, 16, 150);

    let mut init_items: Vec<String> = Vec::with_capacity(emission.inits.len());
    for class in classes.iter().filter(|c| emission.inits.contains(&c.name)) {
        let _ = writeln!(
            internal_decls,
            "extern jabs_class_init {}_state;",
            init_symbol(&class.name)
        );
        let mut buf = String::new();
        emit_statics_and_init(&mut buf, &unit, class, &literal_ids);
        init_items.push(buf);
    }
    let init_shards = shard_strings(&init_items, 16, 150);

    let mut core_body = String::from(RUNTIME_GLOBALS);
    let array_vtable = if recorded_names.contains(OBJECT) {
        mangled("jv_", &[OBJECT])
    } else {
        "NULL".to_string()
    };
    let _ = writeln!(
        core_body,
        "const jabs_class jabs_array_class_record = {{ \"[\", 0, {array_vtable}, NULL, NULL, 0, NULL, 0, 0, 0, 0, NULL, 0 }};"
    );
    emit_vm_throwables(&mut core_body, &recorded_names);
    emit_literals(&mut core_body, &literals, registry);
    core_body.push_str(&thread_glue_functions(&unit, registry, &emission.methods));
    core_body.push_str(&management::glue(
        &unit,
        &emission.methods,
        &emission.records,
        &emission.inits,
    ));
    let ref_class = unit.get("java/lang/ref/Reference").filter(|c| {
        recorded_names.contains("java/lang/ref/Reference")
            && c.fields.iter().any(|f| f.name == "referent")
            && c.fields.iter().any(|f| f.name == "discovered")
    });
    if ref_class.is_some() {
        let rs = object_struct("java/lang/ref/Reference");
        let _ = writeln!(
            core_body,
            "const uint32_t jabs_gc_ref_referent_offset = (uint32_t)offsetof({rs}, {});\nconst uint32_t jabs_gc_ref_discovered_offset = (uint32_t)offsetof({rs}, {});",
            instance_field("referent"),
            instance_field("discovered"),
        );
    } else {
        core_body.push_str(
            "const uint32_t jabs_gc_ref_referent_offset = 0u;\nconst uint32_t jabs_gc_ref_discovered_offset = 0u;\n",
        );
    }
    let stub_statics = reached_stub_statics(results, &emission);
    let mut static_ref_syms = Vec::new();
    for cls_name in emission.inits.iter().chain(stub_statics.iter()) {
        let Some(c) = unit.get(cls_name) else {
            continue;
        };
        for f in c.fields.iter().filter(|f| f.access & ACC_STATIC != 0) {
            if storage_type(&f.descriptor) == Some("jabs_object *") {
                let sym = field_symbol(cls_name, &f.name);
                let _ = writeln!(core_body, "extern jabs_object *{sym};");
                static_ref_syms.push(format!("&{sym}"));
            }
        }
    }
    let _ = writeln!(
        core_body,
        "jabs_object **const jabs_gc_static_refs[] = {{ {}NULL }};",
        if static_ref_syms.is_empty() {
            String::new()
        } else {
            format!("{}, ", static_ref_syms.join(", "))
        }
    );
    if !registry {
        core_body.push_str(
            r#"const size_t jabs_gc_var_handle_size = 0u;
void jabs_gc_visit_reflect_roots(jabs_gc_root_fn visit, void *ctx) { (void)visit; (void)ctx; }
void jabs_gc_visit_module_roots(jabs_gc_root_fn visit, void *ctx) { (void)visit; (void)ctx; }
void jabs_gc_visit_rproxy_roots(jabs_gc_root_fn visit, void *ctx) { (void)visit; (void)ctx; }
void jabs_gc_sweep_mh_roots(jabs_gc_is_marked_fn is_marked) { (void)is_marked; }
void jabs_gc_visit_runtime_roots(jabs_gc_root_fn visit, void *ctx) {
    for (uint32_t i = 0; i < jabs_gc_literal_count; i++) {
        jabs_object *lit = atomic_load_explicit(&jabs_gc_literals[i], memory_order_acquire);
        if (lit) visit(lit, ctx);
    }
}
void jabs_gc_sweep_weak_roots(jabs_gc_is_marked_fn is_marked) { (void)is_marked; }
"#,
        );
    } else {
        core_body.push_str(
            r#"void jabs_gc_sweep_interned(jabs_gc_is_marked_fn is_marked);
jabs_object *jabs_app_loader_created(void);
void jabs_gc_visit_runtime_roots(jabs_gc_root_fn visit, void *ctx) {
    for (uint32_t i = 0; i < jabs_gc_literal_count; i++) {
        jabs_object *lit = atomic_load_explicit(&jabs_gc_literals[i], memory_order_acquire);
        if (lit) visit(lit, ctx);
    }
    jabs_object *loader = jabs_app_loader_created();
    if (loader) visit(loader, ctx);
    jabs_gc_visit_reflect_roots(visit, ctx);
    jabs_gc_visit_module_roots(visit, ctx);
}
void jabs_gc_sweep_weak_roots(jabs_gc_is_marked_fn is_marked) {
    jabs_gc_sweep_interned(is_marked);
    jabs_gc_sweep_mh_roots(is_marked);
}
"#,
        );
    }
    if !registry {
        core_body.push_str(&boot_extras::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
            false,
        ));
    }

    let reflect_body = if registry {
        let mut r = String::new();
        emit_class_registry(
            &mut r,
            &unit,
            &recorded,
            &RegistryInputs {
                app_classes: &app_classes,
                java_home: extras.java_home.as_deref(),
                jab: extras.jab.is_some(),
                inits: &emission.inits,
                methods: &emission.methods,
                interfaces: &interface_records,
                hidden: &indy
                    .defined_by
                    .iter()
                    .map(|(generated, _)| generated.clone())
                    .chain(proxies.proxies.iter().map(|p| p.name.clone()))
                    .collect(),
                proxies: &proxies.proxies,
            },
        );
        r.push_str(&boot_extras::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
            true,
        ));
        r.push_str(&modules::tables(&extras.jdk_modules, classes, &app_classes));
        r.push_str(&modules::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
        ));
        r.push_str(&net::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
        ));
        r.push_str(&nio_fs::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
        ));
        r.push_str(&stack::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
        ));
        r.push_str(&method_handles::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
        ));
        r.push_str(&unsafe_rt::glue(&emission.inits));
        r.push_str(&zip::glue(
            &unit,
            &emission.methods,
            &emission.records,
            &emission.inits,
        ));
        Some(r)
    } else {
        None
    };

    struct EmitItem<'a> {
        class: &'a ClassFile,
        method: &'a Member,
        sig: &'a Signature,
        analyzed: &'a Analyzed,
        intrinsic: Option<&'a (String, bool)>,
    }

    // 4. Parallel method emission + scoped MethodReport construction.
    let executed_set: HashSet<&MethodKey> = extras.executed_methods.iter().collect();
    let mut to_emit: Vec<EmitItem<'_>> = Vec::new();
    let mut methods = Vec::new();
    for class in classes {
        let is_root_class = roots.is_some_and(|r| r.contains(&class.name));
        for method in &class.methods {
            let key = (
                class.name.clone(),
                method.name.clone(),
                method.descriptor.clone(),
            );
            let emitted = emission.methods.contains(&key);
            if roots.is_some() && !is_root_class && !emitted && !executed_set.contains(&key) {
                continue;
            }
            let status = match results.get(&key) {
                Some(_) if method.access & ACC_ABSTRACT != 0 => MethodStatus::Abstract,
                Some(Ok((sig, analyzed))) => {
                    if emitted {
                        to_emit.push(EmitItem {
                            class,
                            method,
                            sig,
                            analyzed,
                            intrinsic: intrinsics.get(&key),
                        });
                    }
                    MethodStatus::Compiled {
                        symbol: symbol(&key.0, &key.1, &key.2),
                        self_contained: self_contained.contains(&key),
                        blocked_by: poisoned.get(&key).cloned().unwrap_or_default(),
                    }
                }
                Some(Err(reason)) => MethodStatus::Unsupported {
                    reason: reason.clone(),
                },
                None => MethodStatus::Unsupported {
                    reason: "not analyzed".to_string(),
                },
            };
            methods.push(MethodReport {
                class: key.0,
                name: key.1,
                descriptor: key.2,
                status,
            });
        }
    }

    let emit_one = |item: &EmitItem<'_>| -> (usize, String) {
        if let Some((body, _)) = item.intrinsic {
            let mut unused = String::new();
            if item.method.access & ACC_STATIC == 0 {
                unused.push_str(" (void)self;");
            }
            for i in 0..item.sig.params.len() {
                let _ = write!(unused, " (void)a{i};");
            }
            let mut text = String::new();
            let _ = writeln!(
                text,
                "/* native {}.{}{} */",
                item.class.name, item.method.name, item.method.descriptor
            );
            let _ = writeln!(
                text,
                "{} {{{unused} {body} }}\n",
                prototype(&item.class.name, item.method, item.sig)
            );
            (1, text)
        } else {
            let calls = item
                .analyzed
                .resolved
                .values()
                .filter(|r| matches!(r, Resolved::Call(_)))
                .count();
            let cost =
                (item.analyzed.insns.len() + item.analyzed.dispatch.len() * 8 + calls * 4).max(1);
            let mut text = String::new();
            emit_method(
                &mut text,
                &unit,
                item.class,
                item.method,
                item.sig,
                item.analyzed,
                &literal_ids,
            );
            (cost, text)
        }
    };

    let method_texts: Vec<(usize, String)> = if to_emit.len() >= 16 {
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(to_emit.len());
        let chunk_size = to_emit.len().div_ceil(threads).max(1);
        let chunks: Vec<Vec<(usize, String)>> = std::thread::scope(|scope| {
            let handles: Vec<_> = to_emit
                .chunks(chunk_size)
                .map(|chunk| scope.spawn(|| chunk.iter().map(emit_one).collect::<Vec<_>>()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        chunks.into_iter().flatten().collect()
    } else {
        to_emit.iter().map(emit_one).collect()
    };

    let compiled = methods
        .iter()
        .filter(|m| matches!(m.status, MethodStatus::Compiled { .. }))
        .count();
    let abstract_methods = methods
        .iter()
        .filter(|m| m.status == MethodStatus::Abstract)
        .count();
    let self_contained_count = methods
        .iter()
        .filter(|m| {
            matches!(
                m.status,
                MethodStatus::Compiled {
                    self_contained: true,
                    ..
                }
            )
        })
        .count();

    let mut extern_decls = String::new();
    for s in &record_shards {
        extern_decls.push_str(&declarations_for(s));
    }
    for s in &init_shards {
        extern_decls.push_str(&declarations_for(s));
    }
    for s in &stub_shards {
        extern_decls.push_str(&declarations_for(s));
    }
    extern_decls.push_str(&declarations_for(&core_body));
    if let Some(ref reflect) = reflect_body {
        extern_decls.push_str(&declarations_for(reflect));
    }

    let unit_header = format!(
        "#ifndef JABS_UNIT_H\n#define JABS_UNIT_H\n{unit_decls}\n/* Definitions elsewhere in this program. */\n{extern_decls}#endif\n"
    );
    let internal_header = format!(
        "#ifndef JABS_UNIT_INTERNAL_H\n#define JABS_UNIT_INTERNAL_H\n#include \"jabs_unit.h\"\n{internal_decls}#endif\n"
    );
    let resources_c = resources_file(
        &extras.resources,
        extras.jab.as_ref(),
        extras.java_home.as_deref(),
    );

    let c_files = {
        let mut files = vec![
            ("jabs_unit.h".to_string(), unit_header),
            ("jabs_runtime.h".to_string(), runtime_header.clone()),
            ("jabs_unit_internal.h".to_string(), internal_header),
            (
                "unit_0_core.c".to_string(),
                format!("#include \"jabs_unit_internal.h\"\n{core_body}"),
            ),
        ];
        for (i, s) in stub_shards.iter().enumerate() {
            files.push((
                format!("unit_0_stubs_{i}.c"),
                format!("#include \"jabs_unit_internal.h\"\n{s}"),
            ));
        }
        for (i, s) in record_shards.iter().enumerate() {
            files.push((
                format!("unit_0_records_{i}.c"),
                format!("#include \"jabs_unit_internal.h\"\n{s}"),
            ));
        }
        for (i, s) in init_shards.iter().enumerate() {
            files.push((
                format!("unit_0_inits_{i}.c"),
                format!("#include \"jabs_unit_internal.h\"\n{s}"),
            ));
        }
        if let Some(ref reflect) = reflect_body {
            files.push((
                "unit_0_reflect.c".to_string(),
                format!("#include \"jabs_unit_internal.h\"\n{reflect}"),
            ));
        }
        files.push(("resources.c".to_string(), resources_c.clone()));
        files.push(("jabs_jab.c".to_string(), JAB_C.to_string()));
        files.push(("jabs_platform.c".to_string(), PLATFORM_C.to_string()));
        files.push(("jabs_files.c".to_string(), FILES_C.to_string()));
        files.push(("jabs_sync.c".to_string(), SYNC_C.to_string()));
        files.push(("jabs_thread.c".to_string(), THREAD_C.to_string()));
        files.push(("jabs_gc.c".to_string(), GC_C.to_string()));
        files.push(("jabs_reflect.c".to_string(), REFLECT_C.to_string()));
        if registry_emitted {
            files.push(("jabs_io.c".to_string(), IO_C.to_string()));
            files.push(("jabs_management.c".to_string(), MANAGEMENT_C.to_string()));
            files.push(("jabs_modules.c".to_string(), MODULES_C.to_string()));
            files.push(("jabs_net.c".to_string(), NET_C.to_string()));
            files.push(("jabs_nio_fs.c".to_string(), NIO_FS_C.to_string()));
            files.push(("jabs_zip.c".to_string(), ZIP_C.to_string()));
        }
        for (i, chunk) in method_chunks(&method_texts, 48).into_iter().enumerate() {
            files.push((
                format!("unit_{}.c", i + 1),
                format!("#include \"jabs_unit.h\"\n{chunk}"),
            ));
        }
        files
    };
    // Every symbol the C refers to must be defined: a miss is an emitter bug, which would
    // otherwise show up only as a C compiler or linker error far from its cause.
    if let Err(problem) = symbols::check(&c_files) {
        panic!("jabs: {problem}");
    }

    let c_source = if extras.emit_single_source {
        let mut s = String::new();
        s.push_str(&runtime_header);
        s.push_str(&strip_includes(&unit_decls));
        s.push_str(&extern_decls);
        s.push_str(&internal_decls);
        s.push_str(&core_body);
        for shard in &stub_shards {
            s.push_str(shard);
        }
        for shard in &record_shards {
            s.push_str(shard);
        }
        for shard in &init_shards {
            s.push_str(shard);
        }
        if let Some(ref reflect) = reflect_body {
            s.push_str(reflect);
        }
        for (_, text) in &method_texts {
            s.push_str(text);
        }
        s.push_str(&strip_includes(&resources_c));
        // First of the handwritten files: on Windows, winsock2.h must come before the
        // first windows.h.
        if registry_emitted {
            s.push_str(&strip_includes(NET_C));
        }
        s.push_str(&strip_includes(PLATFORM_C));
        s.push_str(&strip_includes(FILES_C));
        s.push_str(&strip_includes(JAB_C));
        s.push_str(&strip_includes(SYNC_C));
        s.push_str(&strip_includes(THREAD_C));
        s.push_str(&strip_includes(GC_C));
        s.push_str(&strip_includes(REFLECT_C));
        if registry_emitted {
            s.push_str(&strip_includes(IO_C));
            s.push_str(&strip_includes(MANAGEMENT_C));
            s.push_str(&strip_includes(MODULES_C));
            s.push_str(&strip_includes(NIO_FS_C));
            s.push_str(&strip_includes(ZIP_C));
        }
        s
    } else {
        String::new()
    };

    CompiledUnit {
        schema_version: 1,
        lambda_sites: indy.lambdas,
        string_concat_sites: indy.concats,
        other_indy_sites: indy.others,
        invokedynamic_unsupported: indy.skipped.len(),
        invokedynamic_reasons: indy
            .skipped
            .iter()
            .fold(BTreeMap::new(), |mut m, (_, _, why)| {
                *m.entry(why.clone()).or_insert(0) += 1;
                m
            }),
        classes: classes.len(),
        compiled,
        unsupported: methods.len() - compiled - abstract_methods,
        abstract_methods,
        self_contained: self_contained_count,
        methods,
        executed_roots: (!extras.executed_methods.is_empty()).then_some(executed_counts),
        c_source,
        c_files,
    }
}
