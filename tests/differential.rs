//! Differential conformance: the reference JVM and JABS-generated native code
//! must print bit-identical results (or the same exception) for every case.
//!
//! Needs `javac`, `java` and `clang` on PATH. Missing tools skip the test
//! locally; set JABS_REQUIRE_TOOLS=1 (as CI does) to make that a failure.

use jabs::{cgen, classfile::ClassFile};
use std::{fmt::Write, fs, path::Path, process::Command};

#[derive(Clone, Copy)]
enum Arg {
    I(i32),
    J(i64),
    F(f32),
    D(f64),
}
use Arg::*;

/// Whether a tool runs successfully with the given probe argument.
fn tool_available(name: &str, arg: &str) -> bool {
    Command::new(name)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run a command, panic with its output on failure, and return stdout with normalized newlines.
fn run(cmd: &mut Command) -> String {
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    assert!(
        output.status.success(),
        "{cmd:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .replace("\r\n", "\n")
}

/// Java source expression for an argument, bit-exact and cast to the parameter type.
fn java_literal(arg: Arg, param: char) -> String {
    match arg {
        I(v) => match param {
            'B' => format!("(byte)0x{:08x}", v as u32),
            'C' => format!("(char)0x{:08x}", v as u32),
            'S' => format!("(short)0x{:08x}", v as u32),
            _ => format!("0x{:08x}", v as u32),
        },
        J(v) => format!("0x{:016x}L", v as u64),
        F(v) => format!("Float.intBitsToFloat(0x{:08x})", v.to_bits()),
        D(v) => format!("Double.longBitsToDouble(0x{:016x}L)", v.to_bits()),
    }
}

/// C expression for the same argument bits.
fn c_literal(arg: Arg) -> String {
    match arg {
        I(v) => format!("(int32_t)0x{:08x}u", v as u32),
        J(v) => format!("(int64_t)UINT64_C(0x{:016x})", v as u64),
        F(v) => format!("jabs_fbits(0x{:08x}u)", v.to_bits()),
        D(v) => format!("jabs_dbits(UINT64_C(0x{:016x}))", v.to_bits()),
    }
}

/// Edge-case inputs per fixture method: overflow, MIN/-1, shift masks, NaN, infinities, saturation.
fn arith_cases() -> Vec<(&'static str, Vec<Arg>)> {
    let (imin, imax) = (i32::MIN, i32::MAX);
    let (lmin, lmax) = (i64::MIN, i64::MAX);
    let floats = [
        0.0f32,
        -0.0,
        1.5,
        -2.5,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        3e9,
        -3e9,
        1e20,
        f32::MIN_POSITIVE,
    ];
    let doubles = [
        0.0f64,
        -0.0,
        2.75,
        -1e300,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        3e9,
        1e19,
        -1e19,
        4.9e-324,
    ];
    let ints = [0, 1, -1, 7, -7, 31, 32, 33, -33, imin, imax, 0x55aa_33cc];
    let mut out: Vec<(&'static str, Vec<Arg>)> = Vec::new();
    for name in [
        "add", "sub", "mul", "div", "rem", "bits", "shl", "shr", "ushr", "gcd", "divChain",
    ] {
        for &a in &ints {
            for &b in &[0, 1, -1, 3, -3, 31, 32, 33, imin, imax] {
                if name == "gcd" && (a == imin || b == imin) {
                    continue; // Math.abs-free gcd recursion is fine, but keep cases bounded
                }
                out.push((name, vec![I(a), I(b)]));
            }
        }
    }
    for &a in &ints {
        out.push(("neg", vec![I(a)]));
        for name in ["i2b", "i2c", "i2s", "dense", "sparse"] {
            out.push((name, vec![I(a)]));
        }
    }
    for k in [-1_000_000, 1, 2, 3, 4, 5, 1 << 20] {
        out.push(("dense", vec![I(k)]));
        out.push(("sparse", vec![I(k)]));
    }
    let longs = [
        0,
        1,
        -1,
        3,
        -3,
        lmin,
        lmax,
        0x1234_5678_9abc_def0,
        -(1 << 40),
    ];
    for &a in &longs {
        out.push(("l2i", vec![J(a)]));
        out.push(("l2f", vec![J(a)]));
        for &n in &[0, 1, 63, 64, 65, -1] {
            for name in ["lshr", "lushr", "lshl"] {
                out.push((name, vec![J(a), I(n)]));
            }
        }
        for &b in &longs {
            for name in ["ladd", "lmul", "ldiv", "lrem", "lcmp"] {
                out.push((name, vec![J(a), J(b)]));
            }
        }
        out.push(("ldiv", vec![J(a), J(0)]));
    }
    for &a in &floats {
        out.push(("fneg", vec![F(a)]));
        out.push(("f2i", vec![F(a)]));
        out.push(("f2l", vec![F(a)]));
        for &b in &floats {
            for name in ["fdiv", "frem", "fless", "fgreater"] {
                out.push((name, vec![F(a), F(b)]));
            }
        }
    }
    for &a in &doubles {
        for name in ["d2i", "d2l", "d2f", "horner"] {
            out.push((name, vec![D(a)]));
        }
        for &b in &doubles {
            out.push(("drem", vec![D(a), D(b)]));
            out.push(("dequal", vec![D(a), D(b)]));
        }
        out.push(("dmix", vec![D(a), F(1.25), I(-7), J(1 << 53)]));
    }
    for n in [0, 1, 2, 10, 20] {
        out.push(("fib", vec![I(n)]));
    }
    for n in [0, 1, 100, 65_536, 1_000_000] {
        out.push(("sumTo", vec![I(n)]));
    }
    for n in [1i64, 27, 837_799, 63_728_127] {
        out.push(("collatz", vec![J(n)]));
    }
    out
}

/// Fixture sources; every class they define is compiled together as one unit.
const FIXTURE_SOURCES: [&str; 15] = [
    "Arith",
    "Statics",
    "BadInit",
    "Node",
    "Shapes",
    "Arrays",
    "Strings",
    "Ifaces",
    "Exceptions",
    "Sync",
    "Monitors",
    "Volatiles",
    "Natives",
    "StackOps",
    "Programs",
];

/// Whole programs from `Programs.java`: (class, `-D` properties, arguments).
const PROGRAMS: [(&str, &[&str], &[&str]); 9] = [
    ("EchoArgs", &[], &[]),
    ("EchoArgs", &[], &["one", "two words", ""]),
    ("ExitCode", &[], &[]),
    ("ExitCode", &[], &["7"]),
    ("Uncaught", &[], &[]),
    ("UncaughtNull", &[], &[]),
    ("Props", &[], &[]),
    (
        "Props",
        &[
            "jabs.test.value=a=b c",
            "jabs.test.empty",
            "jabs.test.flag=true",
        ],
        &["-Dnot.a.property"],
    ),
    ("Traces", &[], &[]),
];

/// The prefix the JVM's launcher puts before an exception that escaped `main`.
const JVM_UNCAUGHT: &str = "Exception in thread \"main\" ";

/// What a finished program left behind: stdout with normalized newlines, the exit status, and
/// the class of an exception that escaped `main` (internal name), if one did.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    stdout: String,
    status: Option<i32>,
    uncaught: Option<String>,
}

/// Run a program to completion, whatever its exit status. `uncaught` is read from the first
/// stderr line starting with `prefix`, up to the message.
fn run_program(cmd: &mut Command, prefix: &str) -> Outcome {
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut uncaught = None;
    for line in stderr.lines() {
        let Some(rest) = line.strip_prefix(prefix) else {
            continue;
        };
        let name = rest.split(':').next().unwrap_or(rest).trim();
        uncaught = Some(name.replace('.', "/"));
        break;
    }
    Outcome {
        stdout: String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        status: output.status.code(),
        uncaught,
    }
}

/// The native entry point for one program, shaped like the `java` launcher: leading `-Dkey=value`
/// arguments are system properties and `--` ends them; then start the VM, build `String[] args`
/// from the rest, initialize the class and call `main`, and report an escaping exception the
/// way the JVM does (status 1).
fn program_harness(class: &str) -> String {
    let record = cgen::class_record("java/lang/String");
    format!(
        r#"#include "jabs_unit.h"
extern const jabs_class {record};
int main(int argc, char **argv) {{
    if (argc < 1) return 2;
    static const char *defines[256];
    int first = 1, count = 0;
    while (first < argc && strncmp(argv[first], "-D", 2) == 0) {{
        if (count == 256) {{ fprintf(stderr, "too many -D properties\n"); return 2; }}
        defines[count++] = argv[first++] + 2;
    }}
    if (first < argc && strcmp(argv[first], "--") == 0) first++;
    if (jabs_set_command_line_properties(count, defines) != 0) {{ fprintf(stderr, "a -D property has no key\n"); return 2; }}
    if (jabs_vm_startup() != 0) {{ fprintf(stderr, "startup failed: %s\n", jabs_pending->cls->name); return 2; }}
    jabs_object *args = jabs_new_array(argc - first, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{record}, "[Ljava/lang/String;", NULL);
    if (!args) {{ fprintf(stderr, "out of memory\n"); return 2; }}
    for (int i = first; i < argc; i++) {{
        jabs_object *s = jabs_string_from_utf8(argv[i]);
        if (!s) {{ fprintf(stderr, "out of memory\n"); return 2; }}
        ((jabs_object **)jabs_array_data((jabs_array *)args))[i - first] = s;
    }}
    if ({init}() == 0) {main}(args);
    fflush(stdout);
    if (jabs_pending) {{ fprintf(stderr, "uncaught %s\n", jabs_pending->cls->name); return 1; }}
    return 0;
}}
"#,
        init = cgen::init_symbol(class),
        main = cgen::symbol(class, "main", "([Ljava/lang/String;)V"),
    )
}

/// Fixtures that need a newer language level (records, pattern switches, indy string concat).
const MODERN_FIXTURE_SOURCES: [&str; 3] = ["Indy", "Views", "Atomics"];

/// Every call in order as (class, method, args); order matters because static state persists across calls.
fn cases() -> Vec<(&'static str, &'static str, Vec<Arg>)> {
    let mut out: Vec<_> = arith_cases()
        .into_iter()
        .map(|(name, args)| ("Arith", name, args))
        .collect();
    let statics: [(&str, Vec<Arg>); 16] = [
        ("next", vec![]),
        ("next", vec![]),
        ("k", vec![]),
        ("bigPlus", vec![J(5)]),
        ("b", vec![]),
        ("ch", vec![]),
        ("sh", vec![]),
        ("flag", vec![]),
        ("setByte", vec![I(300)]),
        ("b", vec![]),
        ("setChar", vec![I(-1)]),
        ("ch", vec![]),
        ("scaled", vec![D(2.0)]),
        ("nan", vec![]),
        ("next", vec![]),
        ("setByte", vec![I(-129)]),
    ];
    out.extend(
        statics
            .into_iter()
            .map(|(name, args)| ("Statics", name, args)),
    );
    // Objects: every case calls a static entry point that builds and uses Node instances.
    for n in [0, 1, 2, 5, 100] {
        out.push(("Node", "build", vec![I(n)]));
        out.push(("Node", "totalWeight", vec![I(n)]));
        if n > 0 {
            out.push(("Node", "lengthOf", vec![I(n)]));
        }
    }
    for v in [0, 1, 7, 128, 255, -129, i32::MIN] {
        out.push(("Node", "tagOf", vec![I(v)]));
        out.push(("Node", "marked", vec![I(v)]));
        out.push(("Node", "privateCall", vec![I(v)]));
        out.push(("Node", "sameObject", vec![I(v)]));
    }
    out.push(("Node", "defaults", vec![]));
    out.push(("Node", "nullField", vec![]));
    out.push(("Node", "nullCall", vec![I(0)]));
    out.push(("Node", "nullCall", vec![I(3)]));
    out.push(("Node", "relink", vec![I(5)]));
    out.push(("Node", "lengthOf", vec![I(0)]));
    // Hierarchy: the first case triggers Shape, Rect, Square initialization in that order.
    out.push(("Shapes", "initOrder", vec![]));
    for which in [0, 1, 2] {
        out.push(("Shapes", "area", vec![I(which), D(1.5)]));
        out.push(("Shapes", "kind", vec![I(which)]));
        out.push(("Shapes", "isRect", vec![I(which)]));
        out.push(("Shapes", "castToRect", vec![I(which)]));
        out.push(("Shapes", "scaled", vec![I(which), D(-0.5)]));
        for other in [0, 1, 2] {
            out.push(("Shapes", "total", vec![I(which), I(other)]));
        }
    }
    out.push(("Shapes", "ids", vec![I(7)]));
    out.push(("Shapes", "tag", vec![]));
    out.push(("Shapes", "trace", vec![]));
    // A superclass whose initializer throws: EIIE first, then NoClassDefFoundError.
    out.push(("Shapes", "childGet", vec![]));
    out.push(("Shapes", "childGet", vec![]));
    // Arrays: every element type, 2-D, covariance and each exception.
    for n in [0, 1, 7, 1000] {
        out.push(("Arrays", "sumSquares", vec![I(n)]));
        out.push(("Arrays", "doubles", vec![I(n)]));
        out.push(("Arrays", "grid", vec![I(n.min(50))]));
        out.push(("Arrays", "shapes", vec![I(n.min(50))]));
        out.push(("Arrays", "objects", vec![I(n)]));
    }
    for v in [0, 1, -1, 255, 70000, i32::MIN, i32::MAX] {
        for name in ["bytes", "booleans", "chars", "shorts"] {
            out.push(("Arrays", name, vec![I(v)]));
        }
    }
    for x in [0.5f32, -3.0, f32::NAN] {
        out.push(("Arrays", "floats", vec![F(x)]));
    }
    for x in [0i64, i64::MAX, -7] {
        out.push(("Arrays", "longs", vec![J(x)]));
    }
    for which in [0, 1, 2] {
        out.push(("Arrays", "covariantStore", vec![I(which)]));
    }
    for n in [-1, 0, 5, i32::MIN] {
        out.push(("Arrays", "negative", vec![I(n)]));
    }
    for i in [-1, 0, 2, 3, i32::MIN, i32::MAX] {
        out.push(("Arrays", "outOfBounds", vec![I(i)]));
    }
    out.push(("Arrays", "nullArray", vec![I(0)]));
    out.push(("Arrays", "nullLength", vec![]));
    // Strings: literals built in the JDK's own layout, run through the JDK's own String code.
    for name in [
        "length",
        "lengthUtf16",
        "emptyLength",
        "same",
        "different",
        "sameAcross",
        "loneSurrogate",
    ] {
        out.push(("Strings", name, vec![]));
    }
    out.push(("Strings", "isEmpty", vec![I(0)]));
    out.push(("Strings", "isEmpty", vec![I(1)]));
    // Interfaces: dispatch, default selection, type tests.
    for w in [0, 1, 2] {
        for name in ["legs", "eyes", "describe", "isLoud", "castLoud"] {
            out.push(("Ifaces", name, vec![I(w)]));
        }
        for other in [0, 1, 2] {
            out.push(("Ifaces", "count", vec![I(w), I(other)]));
        }
    }
    for n in [0, 1, 7] {
        out.push(("Ifaces", "arrayOfAnimals", vec![I(n)]));
    }
    for name in ["dogEyes", "arraysAreCloneable", "nullReceiver"] {
        out.push(("Ifaces", name, vec![]));
    }
    // Exceptions: try/catch/finally over runtime-raised exceptions.
    for b in [0, 1, 3] {
        for name in [
            "finallyCounts",
            "finallyRethrows",
            "rethrow",
            "caughtIdentity",
            "instanceOfCaught",
            "passesThrough",
        ] {
            out.push(("Exceptions", name, vec![I(b)]));
        }
        out.push(("Exceptions", "divide", vec![I(7), I(b)]));
        out.push(("Exceptions", "nested", vec![I(7), I(b)]));
        out.push(("Exceptions", "npeCatch", vec![I(b)]));
    }
    for i in [-1, 0, 2, 3] {
        out.push(("Exceptions", "index", vec![I(i)]));
        out.push(("Exceptions", "broad", vec![I(i)]));
    }
    out.push(("Exceptions", "loopWithCatch", vec![I(5)]));
    out.push(("Exceptions", "throwNull", vec![]));
    // Monitors and array casts.
    for n in [0, 1, 5] {
        out.push(("Sync", "block", vec![I(n)]));
        out.push(("Sync", "blockThrows", vec![I(n)]));
        out.push(("Sync", "reentrant", vec![I(n)]));
        out.push(("Sync", "twice", vec![I(n)]));
    }
    out.push(("Sync", "nullLock", vec![]));
    for name in [
        "holdsLockFollowsLevels",
        "holdsLockAfterSynchronizedMethodThrows",
        "notifyAfterExitThrows",
        "notifyAllNeverLockedThrows",
        "cloneHasItsOwnMonitor",
        "holdsLockNull",
    ] {
        out.push(("Sync", name, vec![]));
    }
    for w in [0, 1, 2] {
        out.push(("Sync", "castIntArray", vec![I(w)]));
        out.push(("Sync", "arrayKinds", vec![I(w)]));
        out.push(("Sync", "castShapes", vec![I(w)]));
    }
    out.push(("Sync", "rows", vec![]));
    // Runtime-implemented natives and the Object/Class root.
    for (from, to, len) in [
        (0, 2, 3),
        (2, 0, 3),
        (0, 0, 6),
        (1, 1, 0),
        (-1, 0, 1),
        (0, 4, 3),
        (0, 0, -1),
    ] {
        out.push(("Natives", "copyInts", vec![I(from), I(to), I(len)]));
    }
    for name in [
        "copyMismatch",
        "copyNull",
        "hashStable",
        "hashNull",
        "classIdentity",
        "className",
        "classQueries",
        "cloneArray",
        "reflectArray",
        "clocks",
    ] {
        out.push(("Natives", name, vec![]));
    }
    out.push(("Natives", "copyCovariant", vec![I(0)]));
    out.push(("Natives", "copyCovariant", vec![I(1)]));
    for d in [0.0f64, -0.0, 1.5, f64::NAN, f64::INFINITY] {
        out.push(("Natives", "doubleBits", vec![D(d)]));
    }
    for f in [0.0f32, -2.5, f32::NAN] {
        out.push(("Natives", "floatBits", vec![F(f)]));
    }
    for name in [
        "sameInstance",
        "boxing",
        "staticRef",
        "boundRef",
        "unboundRef",
        "constructorRef",
        "widening",
        "serializable",
        "sort",
        "concat",
        "recordToString",
        "recordToStringLength",
        "recordHash",
        "recordEquals",
        "patterns",
        "sealedSwitch",
        "enumPatterns",
        "moduleOfLNamedClass",
    ] {
        out.push(("Indy", name, vec![]));
    }
    for x in [0, 7, -5, i32::MIN] {
        out.push(("StackOps", "intIntoArray", vec![I(x)]));
    }
    for v in [0i64, 1 << 40, -3, i64::MAX] {
        out.push(("StackOps", "longIntoField", vec![J(v)]));
        out.push(("StackOps", "longIntoArray", vec![J(v)]));
        out.push(("StackOps", "increments", vec![J(v)]));
    }
    for v in [0.5f64, -0.0, f64::NAN] {
        out.push(("StackOps", "doubleIntoArray", vec![D(v)]));
    }
    for k in [0, 3, -2] {
        out.push(("Indy", "capture", vec![I(k)]));
    }
    for x in [0, 7, -123] {
        out.push(("Indy", "concatConstants", vec![I(x)]));
    }
    // Atomic accesses: Unsafe through java.util.concurrent.atomic, and VarHandle access modes.
    for start in [0, 5, -1, i32::MAX] {
        out.push(("Atomics", "atomicIntegerUpdates", vec![I(start)]));
        out.push(("Atomics", "varHandleIntModes", vec![I(start)]));
    }
    for start in [0i64, 3, i64::MIN, -27] {
        out.push(("Atomics", "atomicLongUpdates", vec![J(start)]));
        out.push(("Atomics", "varHandleLongModes", vec![J(start)]));
    }
    for name in [
        "atomicReferenceUpdates",
        "atomicBooleanAndArray",
        "varHandleNarrowModes",
        "varHandleFloatingModes",
        "varHandleReferenceModes",
        "varHandleArrayStoreChecked",
    ] {
        out.push(("Atomics", name, vec![]));
    }
    for index in [1, 2, -1] {
        out.push(("Atomics", "atomicIntegerArrayOutOfBounds", vec![I(index)]));
        out.push(("Atomics", "varHandleArrayIndexChecked", vec![I(index)]));
    }
    // Volatile fields.
    for v in [0i64, 1, -1, i64::MIN, 0x7ff8_0000_0000_0001] {
        out.push(("Volatiles", "longRoundTrip", vec![J(v)]));
        out.push(("Volatiles", "doubleBits", vec![J(v)]));
    }
    for v in [0, 1, -1, 0x1_2345, i32::MIN] {
        out.push(("Volatiles", "narrowStores", vec![I(v)]));
    }
    for n in [0, 1, 1000] {
        out.push(("Volatiles", "compoundUpdates", vec![I(n)]));
    }
    out.push(("Volatiles", "inheritedAndStatic", vec![]));
    out.push(("Volatiles", "nullReceiver", vec![]));
    // wait/notify on one thread.
    for name in [
        "timedWaitReturnsOwningTheLock",
        "monitorReentryRestoresCountAfterWait",
        "waitWithNanosReturns",
        "notifyBeforeWaitIsNotRemembered",
        "synchronizedMethodWaits",
        "waitWithoutLockThrows",
        "waitForeverWithoutLockThrows",
        "waitAfterExitThrows",
        "waitOnOtherObjectThrows",
        "waitNegativeWithoutLockThrows",
        "waitNegativeThrows",
        "waitBadNanosThrows",
        "notifyOnOtherObjectThrows",
        "notifyAllAfterExitThrows",
        "waitOnNullThrows",
    ] {
        out.push(("Monitors", name, vec![]));
    }
    // Byte-array view VarHandles: every view type and byte order at the first, an unaligned,
    // the last legal and the first illegal offset (10-byte array), plus -1.
    for (method, size) in [
        ("viewShortBigEndianGet", 2),
        ("viewShortBigEndianSet", 2),
        ("viewShortLittleEndianGet", 2),
        ("viewShortLittleEndianSet", 2),
        ("viewCharBigEndianGet", 2),
        ("viewCharBigEndianSet", 2),
        ("viewCharLittleEndianGet", 2),
        ("viewCharLittleEndianSet", 2),
        ("viewIntBigEndianGet", 4),
        ("viewIntBigEndianSet", 4),
        ("viewIntLittleEndianGet", 4),
        ("viewIntLittleEndianSet", 4),
        ("viewLongBigEndianGet", 8),
        ("viewLongBigEndianSet", 8),
        ("viewLongLittleEndianGet", 8),
        ("viewLongLittleEndianSet", 8),
        ("viewFloatBigEndianGet", 4),
        ("viewFloatBigEndianSet", 4),
        ("viewFloatLittleEndianGet", 4),
        ("viewFloatLittleEndianSet", 4),
        ("viewDoubleBigEndianGet", 8),
        ("viewDoubleBigEndianSet", 8),
        ("viewDoubleLittleEndianGet", 8),
        ("viewDoubleLittleEndianSet", 8),
    ] {
        for offset in [0, 1, 10 - size, 11 - size, -1] {
            out.push(("Views", method, vec![I(offset)]));
        }
    }
    out.push(("Views", "viewGetOnNullArray", vec![]));
    for offset in [0, 100] {
        out.push(("Views", "viewGetOnIntArray", vec![I(offset)]));
    }
    for offset in [0, -1] {
        out.push(("Views", "viewIntGetOnTwoBytes", vec![I(offset)]));
        out.push(("Views", "viewLongSetOnEmpty", vec![I(offset)]));
    }
    for seed in [0i64, 1, -1, 0x7fc0_0001, i64::MIN, 0x1234_5678_9abc_def0] {
        out.push(("Views", "dataStreamRoundTrip", vec![J(seed)]));
    }
    for skip in [0, 1, 4] {
        out.push(("Views", "dataStreamReadsPattern", vec![I(skip)]));
    }
    // Kept last: the assertion below checks these two outputs.
    // First use: ExceptionInInitializerError. Every later use: NoClassDefFoundError.
    out.extend([("BadInit", "get", vec![]), ("BadInit", "get", vec![])]);
    out
}

const JAVA_PRINTERS: &str = r#"
    static void pi(int v) { System.out.println(String.format("I %08x", v)); }
    static void pj(long v) { System.out.println(String.format("J %016x", v)); }
    static void pf(float v) { System.out.println(String.format("F %08x", Float.floatToIntBits(v))); }
    static void pd(double v) { System.out.println(String.format("D %016x", Double.doubleToLongBits(v))); }
    static void pt(Throwable t) { System.out.println("throw " + t.getClass().getName().replace('.', '/')); }
"#;

const C_PRINTERS: &str = r#"
#include <stdio.h>
#include <inttypes.h>
static void pi(int32_t v) { printf("I %08" PRIx32 "\n", (uint32_t)v); }
static void pj(int64_t v) { printf("J %016" PRIx64 "\n", (uint64_t)v); }
static void pf(float v) { uint32_t b; if (v != v) b = 0x7fc00000u; else memcpy(&b, &v, 4); printf("F %08" PRIx32 "\n", b); }
static void pd(double v) { uint64_t b; if (v != v) b = UINT64_C(0x7ff8000000000000); else memcpy(&b, &v, 8); printf("D %016" PRIx64 "\n", b); }
static int check(void) { if (jabs_pending) { printf("throw %s\n", jabs_pending->cls->name); jabs_pending = 0; return 1; } return 0; }
"#;

/// Clang target arguments that can find a C library: the default target first,
/// then MinGW on Windows hosts without the MSVC SDK. JABS_CLANG_ARGS overrides.
fn clang_target_args(dir: &Path) -> Vec<String> {
    if let Ok(args) = std::env::var("JABS_CLANG_ARGS") {
        return args.split_whitespace().map(String::from).collect();
    }
    let probe = dir.join("probe.c");
    fs::write(
        &probe,
        "#include <math.h>
#include <stdio.h>
int main(void) { return (int)fmod(1.0, 2.0); }
",
    )
    .unwrap();
    let mut candidates = vec![Vec::new()];
    if cfg!(windows) {
        candidates.push(vec!["--target=x86_64-w64-windows-gnu".to_string()]);
    }
    for args in candidates {
        let ok = Command::new("clang")
            .args(&args)
            .arg(&probe)
            .arg("-o")
            .arg(dir.join("probe.exe"))
            .args(if cfg!(windows) { None } else { Some("-lm") })
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            return args;
        }
    }
    panic!(
        "clang cannot build a C program with <math.h> for any known target; set JABS_CLANG_ARGS"
    );
}

/// For fixtures that test one runtime mechanism: the code to look at when a case differs.
fn where_to_look(class: &str) -> &'static str {
    match class {
        "Views" => {
            " (byte-array view VarHandles: jabs_vh_address/jabs_var_handle_byte_view and \
             byte_view_access in src/cgen.rs, jabs_vh_view_load/store in runtime/jabs_rt.h)"
        }
        "Atomics" => {
            " (atomics: unsafe_intrinsic and var_handle_access/var_handle_order in src/cgen.rs, \
             jabs_atomic_add_float/double in runtime/jabs_rt.h)"
        }
        "Volatiles" => " (volatile fields: field_load/field_store in src/cgen.rs)",
        "Monitors" => {
            " (wait/notify: jabs_monitor_wait/jabs_monitor_notify in runtime/jabs_sync.c, the \
             Object.wait0/notify/notifyAll intrinsics in src/cgen.rs)"
        }
        "Sync" => {
            " (monitors: runtime/jabs_sync.c; monitorenter/monitorexit, emit_synchronized_wrapper \
             and the notify/holdsLock intrinsics in src/cgen.rs)"
        }
        _ => "",
    }
}

/// Name of the printer function (in both runners) for a return type.
fn printer(ret: char) -> &'static str {
    match ret {
        'J' => "pj",
        'F' => "pf",
        'D' => "pd",
        _ => "pi",
    }
}

#[test]
fn native_matches_reference_jvm() {
    let required = std::env::var_os("JABS_REQUIRE_TOOLS").is_some();
    let missing: Vec<_> = [
        ("javac", "-version"),
        ("java", "-version"),
        ("clang", "--version"),
    ]
    .into_iter()
    .filter(|(t, a)| !tool_available(t, a))
    .map(|(t, _)| t)
    .collect();
    if !missing.is_empty() {
        assert!(!required, "required tools missing: {missing:?}");
        eprintln!("skipping differential test; missing {missing:?}");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    run(Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(root)
        .args(
            FIXTURE_SOURCES
                .iter()
                .map(|c| fixtures.join(format!("{c}.java"))),
        ));
    run(Command::new("javac")
        .args(["--release", "21", "-nowarn", "-d"])
        .arg(root)
        .args(
            MODERN_FIXTURE_SOURCES
                .iter()
                .map(|c| fixtures.join(format!("{c}.java"))),
        ));

    let mut java = String::from("public class Runner {\n");
    java.push_str(JAVA_PRINTERS);
    java.push_str("    public static void main(String[] args) {\n");
    let mut classes: Vec<ClassFile> = fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "class"))
        .map(|p| ClassFile::parse(&fs::read(p).unwrap()).unwrap())
        .collect();
    let roots: std::collections::BTreeSet<String> =
        classes.iter().map(|c| c.name.clone()).collect();
    // Compile against the real class library, as in production: java.base joins the unit and only
    // code reachable from the fixtures is emitted.
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let (library, _, unparsable) = jabs::jdk::read_module(&home, "java.base").unwrap();
    assert_eq!(unparsable, 0);
    classes.extend(library);
    let phase = std::time::Instant::now();
    // Like a JVM, the program runs the JDK's startup (System.initPhase1) before any case.
    let extras = cgen::UnitExtras {
        startup: true,
        ..Default::default()
    };
    let compiled = cgen::compile_unit_with(&classes, Some(&roots), &extras);
    eprintln!("compile_unit: {:.1?}", phase.elapsed());
    // Every fixture method must compile: unsupported is a test failure here.
    let unsupported: Vec<_> = compiled
        .methods
        .iter()
        .filter(|m| {
            roots.contains(&m.class) && matches!(m.status, cgen::MethodStatus::Unsupported { .. })
        })
        .collect();
    assert!(
        unsupported.is_empty(),
        "unexpectedly unsupported: {unsupported:#?}"
    );
    // One thread cannot tell a volatile access from a plain one, so check the generated C: the
    // volatile long field must be read and written with sequentially consistent atomics.
    for (method, descriptor, ret, wanted) in [
        ("readLong", "()J", "int64_t", "__atomic_load(&"),
        ("writeLong", "(J)V", "void", "__atomic_store(&"),
    ] {
        let start = format!(
            "JABS_METHOD {ret} {}(",
            cgen::symbol("Volatiles", method, descriptor)
        );
        // The definition, not a prototype: its first line opens the body.
        let at = compiled
            .c_source
            .match_indices(&start)
            .map(|(i, _)| i)
            .find(|i| {
                let line = &compiled.c_source[*i..];
                line[..line.find('\n').unwrap_or(line.len())].ends_with('{')
            })
            .unwrap_or_else(|| panic!("Volatiles.{method} not defined in the generated C"));
        let body = &compiled.c_source[at..];
        let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
        assert!(
            body.contains(wanted) && body.contains("__ATOMIC_SEQ_CST"),
            "Volatiles.{method}: a volatile field access is not a seq_cst atomic \
             (field_load/field_store in src/cgen.rs); generated:\n{body}"
        );
    }
    // The harness is its own file next to the compiled program's split files.
    let mut c = String::from("#include \"jabs_unit.h\"\n");
    c.push_str(C_PRINTERS);
    // One small non-inlined function per case: a single huge main makes clang -O2 crawl.
    let mut case_functions = String::new();
    let mut case_calls = String::new();

    let all = cases();
    for (n, (class_name, name, args)) in all.iter().enumerate() {
        let class = classes.iter().find(|c| c.name == *class_name).unwrap();
        let desc = class
            .methods
            .iter()
            .find(|m| m.name == *name)
            .unwrap()
            .descriptor
            .clone();
        let sig = cgen::parse_signature(&desc).unwrap();
        assert_eq!(sig.params.len(), args.len(), "{class_name}.{name}");
        let jargs: Vec<_> = args
            .iter()
            .zip(&sig.params)
            .map(|(a, p)| java_literal(*a, p.code))
            .collect();
        let cargs: Vec<_> = args.iter().map(|a| c_literal(*a)).collect();
        let p = printer(sig.ret.code);
        let _ = writeln!(
            java,
            "        try {{ {p}({class_name}.{name}({})); }} catch (Throwable t) {{ pt(t); }}",
            jargs.join(", ")
        );
        // Like invokestatic, each call first ensures the class is initialized.
        let _ = writeln!(case_calls, "    case_{n}();");
        let _ = writeln!(
            case_functions,
            "__attribute__((noinline)) static void case_{n}(void) {{ if ({}() != 0) check(); else {{ {} r = {}({}); if (!check()) {p}(r); }} }}",
            cgen::init_symbol(class_name),
            cgen::c_type(sig.ret.code),
            cgen::symbol(class_name, name, &desc),
            cargs.join(", ")
        );
    }
    java.push_str("    }\n}\n");
    c.push_str(&case_functions);
    c.push_str("int main(void) {\n");
    c.push_str("    if (jabs_vm_startup() != 0) { printf(\"JVM startup failed: %s\\n\", jabs_pending->cls->name); return 1; }\n");
    c.push_str(&case_calls);
    c.push_str("    return 0;\n}\n");

    fs::write(root.join("Runner.java"), java).unwrap();
    run(Command::new("javac")
        .args(["-nowarn", "-cp"])
        .arg(root)
        .arg("-d")
        .arg(root)
        .arg(root.join("Runner.java")));
    let phase = std::time::Instant::now();
    let reference = run(Command::new("java").arg("-cp").arg(root).arg("Runner"));
    eprintln!("reference JVM run: {:.1?}", phase.elapsed());

    // Compile the program's files in parallel (the reachable JDK code is large), then link.
    let mut sources = Vec::new();
    for (name, contents) in &compiled.c_files {
        fs::write(root.join(name), contents).unwrap();
        if name.ends_with(".c") {
            sources.push(root.join(name));
        }
    }
    let harness = root.join("harness.c");
    fs::write(&harness, c).unwrap();
    sources.push(harness);
    let exe = root.join(if cfg!(windows) {
        "fixtures.exe"
    } else {
        "fixtures"
    });
    let target = clang_target_args(root);
    let flags = [
        "-std=c11",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wno-unused-function",
        "-Wno-unused-label",
        "-D_CRT_SECURE_NO_WARNINGS",
    ];
    let phase = std::time::Instant::now();
    let objects: Vec<_> = std::thread::scope(|scope| {
        let jobs: Vec<_> = sources
            .iter()
            .map(|source| {
                let object = source.with_extension("o");
                let target = &target;
                scope.spawn(move || {
                    run(Command::new("clang")
                        .args(target)
                        .args(flags)
                        .arg("-c")
                        .arg(source)
                        .arg("-o")
                        .arg(&object));
                    object
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    run(Command::new("clang")
        .args(&target)
        .args(&objects)
        .arg("-o")
        .arg(&exe)
        .args(if cfg!(windows) { None } else { Some("-lm") }));
    eprintln!(
        "clang ({} files, parallel): {:.1?}",
        sources.len(),
        phase.elapsed()
    );
    let phase = std::time::Instant::now();
    let native = run(&mut Command::new(&exe));
    eprintln!("native run: {:.1?}", phase.elapsed());

    let reference: Vec<_> = reference.lines().collect();
    let native: Vec<_> = native.lines().collect();
    assert_eq!(reference.len(), all.len());
    // Guard that the object fixture really reaches NullPointerException on the JVM (build(0),
    // nullField, nullCall(0), lengthOf(0)), so those paths are compared rather than skipped.
    let npes = all
        .iter()
        .zip(&reference)
        .filter(|((class, _, _), line)| {
            *class == "Node" && **line == "throw java/lang/NullPointerException"
        })
        .count();
    assert_eq!(
        npes, 4,
        "expected exactly four NullPointerExceptions from the Node fixture"
    );
    // Guard the hierarchy fixture's key JVM behaviors so they are compared, not skipped.
    let at = |class: &str, name: &str, args_first: Option<i32>| {
        all.iter()
            .position(|(c, n, a)| {
                *c == class
                    && *n == name
                    && args_first.is_none_or(|v| matches!(a.first(), Some(I(x)) if *x == v))
            })
            .unwrap()
    };
    // Shape(1), then Rect(3), then Square(4): superclasses initialize first.
    assert_eq!(
        reference[at("Shapes", "initOrder", None)],
        format!("J {:016x}", 134)
    );
    assert_eq!(
        reference[at("Shapes", "castToRect", Some(0))],
        "throw java/lang/ClassCastException"
    );
    let child = at("Shapes", "childGet", None);
    assert_eq!(
        reference[child..child + 2],
        [
            "throw java/lang/ExceptionInInitializerError",
            "throw java/lang/NoClassDefFoundError"
        ]
    );
    // Array exceptions: a Circle into a Rect[] seen as Shape[], negative sizes, bad indexes.
    assert_eq!(
        reference[at("Arrays", "covariantStore", Some(0))],
        "throw java/lang/ArrayStoreException"
    );
    assert_eq!(
        reference[at("Arrays", "negative", Some(-1))],
        "throw java/lang/NegativeArraySizeException"
    );
    assert_eq!(
        reference[at("Arrays", "outOfBounds", Some(3))],
        "throw java/lang/ArrayIndexOutOfBoundsException"
    );
    assert_eq!(
        reference[at("Arrays", "nullLength", None)],
        "throw java/lang/NullPointerException"
    );
    // Interfaces: Spider is not Loud (ClassCastException); Puppy's Pet.super.eyes() + 1 = 4.
    assert_eq!(
        reference[at("Ifaces", "castLoud", Some(1))],
        "throw java/lang/ClassCastException"
    );
    assert_eq!(
        reference[at("Ifaces", "eyes", Some(2))],
        format!("I {:08x}", 4)
    );
    assert_eq!(
        reference[at("Ifaces", "dogEyes", None)],
        format!("I {:08x}", 3)
    );
    // Exceptions: finally ran once before the rethrow reached the outer catch (1000 - 1).
    assert_eq!(
        reference[at("Exceptions", "finallyRethrows", Some(0))],
        format!("I {:08x}", 999)
    );
    assert_eq!(
        reference[at("Exceptions", "rethrow", Some(0))],
        "throw java/lang/ArithmeticException"
    );
    assert_eq!(
        reference[at("Exceptions", "caughtIdentity", Some(0))],
        format!("I {:08x}", 1)
    );
    assert_eq!(
        reference[at("Exceptions", "throwNull", None)],
        "throw java/lang/NullPointerException"
    );
    // Monitors: the lock was released when the exception left the block, so it is taken again.
    assert_eq!(
        reference[at("Sync", "blockThrows", Some(0))],
        format!("I {:08x}", -1i32 as u32)
    );
    assert_eq!(
        reference[at("Sync", "nullLock", None)],
        "throw java/lang/NullPointerException"
    );
    assert_eq!(
        reference[at("Sync", "castShapes", Some(1))],
        "throw java/lang/ClassCastException"
    );
    assert_eq!(
        reference[at("Sync", "arrayKinds", Some(0))],
        format!("I {:08x}", 111)
    );
    assert_eq!(
        reference[at("Atomics", "varHandleArrayStoreChecked", None)],
        "throw java/lang/ArrayStoreException"
    );
    assert_eq!(
        reference[at("Atomics", "varHandleArrayIndexChecked", Some(2))],
        "throw java/lang/ArrayIndexOutOfBoundsException"
    );
    assert_eq!(
        reference[at("Monitors", "monitorReentryRestoresCountAfterWait", None)],
        format!("I {:08x}", 110)
    );
    assert_eq!(
        reference[at("Monitors", "waitForeverWithoutLockThrows", None)],
        "throw java/lang/IllegalMonitorStateException"
    );
    assert_eq!(
        reference[at("Monitors", "waitNegativeWithoutLockThrows", None)],
        "throw java/lang/IllegalArgumentException"
    );
    assert_eq!(
        reference[at("Sync", "holdsLockFollowsLevels", None)],
        format!("I {:08x}", 1110)
    );
    assert_eq!(
        reference[at("Sync", "notifyAfterExitThrows", None)],
        "throw java/lang/IllegalMonitorStateException"
    );
    // Natives: a Circle stops the covariant copy after one element; overlapping copies shift.
    assert_eq!(
        reference[at("Natives", "copyCovariant", Some(1))],
        format!("I {:08x}", -1i32 as u32)
    );
    assert_eq!(
        reference[at("Natives", "copyMismatch", None)],
        "throw java/lang/ArrayStoreException"
    );
    assert_eq!(
        reference[at("Natives", "classIdentity", None)],
        format!("I {:08x}", 1111)
    );
    assert_eq!(
        reference[at("Natives", "className", None)],
        format!("I {:08x}", 1602)
    );
    // Byte-array views: a known big-endian read, and the JVM's exceptions for an offset past the
    // last whole element, a non-byte[] array, a null array and a short stream.
    assert_eq!(
        reference[at("Views", "viewIntBigEndianGet", Some(0))],
        "I 81a6cbf0"
    );
    assert_eq!(
        reference[at("Views", "viewIntBigEndianGet", Some(7))],
        "throw java/lang/ArrayIndexOutOfBoundsException"
    );
    assert_eq!(
        reference[at("Views", "viewGetOnIntArray", Some(100))],
        "throw java/lang/ClassCastException"
    );
    assert_eq!(
        reference[at("Views", "viewGetOnNullArray", None)],
        "throw java/lang/NullPointerException"
    );
    assert_eq!(
        reference[at("Views", "dataStreamReadsPattern", Some(1))],
        "throw java/io/EOFException"
    );
    // Guard that the fixture really exercises the failed-initialization path on the JVM.
    assert_eq!(
        reference[reference.len() - 2..],
        [
            "throw java/lang/ExceptionInInitializerError",
            "throw java/lang/NoClassDefFoundError"
        ]
    );
    let mismatches: Vec<String> = all
        .iter()
        .zip(reference.iter().zip(&native))
        .filter(|(_, (r, n))| r != n)
        .take(20)
        .map(|((class, name, _), (r, n))| {
            format!(
                "{class}.{name}: jvm `{r}` native `{n}`{}",
                where_to_look(class)
            )
        })
        .collect();
    assert!(
        mismatches.is_empty(),
        "{} cases; mismatches:\n{}",
        all.len(),
        mismatches.join("\n")
    );
    assert_eq!(reference.len(), native.len());
    eprintln!(
        "{} cases bit-identical between the JVM and native code",
        all.len()
    );

    // main() mode: link each program's entry point against the same objects, without the case
    // harness, and compare whole runs.
    let harness_object = root.join("harness.o");
    let shared: Vec<_> = objects.iter().filter(|o| **o != harness_object).collect();
    assert_eq!(shared.len() + 1, objects.len(), "harness object not found");
    let phase = std::time::Instant::now();
    let mut jvm_runs = Vec::with_capacity(PROGRAMS.len());
    for (n, (class, defines, args)) in PROGRAMS.iter().enumerate() {
        let properties: Vec<String> = defines.iter().map(|d| format!("-D{d}")).collect();
        let source = root.join(format!("program_{n}.c"));
        fs::write(&source, program_harness(class)).unwrap();
        let program = root.join(if cfg!(windows) {
            format!("program_{n}.exe")
        } else {
            format!("program_{n}")
        });
        run(Command::new("clang")
            .args(&target)
            .args(flags)
            .arg(&source)
            .args(&shared)
            .arg("-o")
            .arg(&program)
            .args(if cfg!(windows) { None } else { Some("-lm") }));
        let jvm = run_program(
            Command::new("java")
                .args(&properties)
                .arg("-cp")
                .arg(root)
                .arg(class)
                .args(*args),
            JVM_UNCAUGHT,
        );
        let native = run_program(
            Command::new(&program)
                .args(&properties)
                .arg("--")
                .args(*args),
            "uncaught ",
        );
        assert_eq!(
            native, jvm,
            "{class} {defines:?} {args:?}: native differs from the JVM"
        );
        jvm_runs.push(jvm);
    }
    // Guard that the programs really reach each ending (normal return, System.exit, a thrown
    // and a JVM-raised exception), so all of them are compared rather than skipped.
    assert_eq!(
        jvm_runs[1].stdout,
        "3\n0: [one] 3\n1: [two words] 9\n2: [] 0\n465\n"
    );
    assert_eq!(jvm_runs[0].status, Some(0));
    assert_eq!(jvm_runs[2].status, Some(3));
    assert_eq!(jvm_runs[3].status, Some(7));
    assert_eq!(
        jvm_runs[4].uncaught.as_deref(),
        Some("java/lang/IllegalStateException")
    );
    assert_eq!(
        jvm_runs[5].uncaught.as_deref(),
        Some("java/lang/NullPointerException")
    );
    assert_eq!(jvm_runs[6].stdout, "null\n[null]\nfalse\ndefault\n0 \n");
    assert_eq!(
        jvm_runs[7].stdout,
        "a=b c\n[]\ntrue\ndefault\n1 -Dnot.a.property\n"
    );
    // Every trace rule is exercised: depths, a constructor chain, main as the last frame, a
    // lambda proxy that must stay hidden, and an exception thrown inside the JDK.
    assert_eq!(
        jvm_runs[8].stdout,
        concat!(
            "Traces.where\n",
            "Traces.nested\n",
            "Traces.main\n",
            "Traces.main\n",
            "1\n",
            "Traces.main\n",
            "java.lang.NumberFormatException.forInputString\n",
            "main\n"
        )
    );
    eprintln!(
        "{} programs identical between the JVM and native code: {:.1?}",
        PROGRAMS.len(),
        phase.elapsed()
    );
}
