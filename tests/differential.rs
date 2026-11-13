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
const FIXTURE_SOURCES: [&str; 8] = [
    "Arith", "Statics", "BadInit", "Node", "Shapes", "Arrays", "Strings", "Ifaces",
];

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
static int check(void) { if (jabs_pending) { printf("throw %s\n", jabs_pending); jabs_pending = 0; return 1; } return 0; }
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
    let compiled = cgen::compile_unit_rooted(&classes, Some(&roots));
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
    let mut c = compiled.c_source.clone();
    c.push_str(C_PRINTERS);
    c.push_str("int main(void) {\n");

    let all = cases();
    for (class_name, name, args) in &all {
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
        let _ = writeln!(
            c,
            "    if ({}() != 0) check(); else {{ {} r = {}({}); if (!check()) {p}(r); }}",
            cgen::init_symbol(class_name),
            cgen::c_type(sig.ret.code),
            cgen::symbol(class_name, name, &desc),
            cargs.join(", ")
        );
    }
    java.push_str("    }\n}\n");
    c.push_str("    return 0;\n}\n");

    fs::write(root.join("Runner.java"), java).unwrap();
    run(Command::new("javac")
        .args(["-nowarn", "-cp"])
        .arg(root)
        .arg("-d")
        .arg(root)
        .arg(root.join("Runner.java")));
    let reference = run(Command::new("java").arg("-cp").arg(root).arg("Runner"));

    let c_path = root.join("fixtures.c");
    fs::write(&c_path, c).unwrap();
    let exe = root.join(if cfg!(windows) {
        "fixtures.exe"
    } else {
        "fixtures"
    });
    run(Command::new("clang")
        .args(clang_target_args(root))
        .args([
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wno-unused-function",
            "-Wno-unused-label",
        ])
        .arg(&c_path)
        .arg("-o")
        .arg(&exe)
        .arg(if cfg!(windows) {
            "-D_CRT_SECURE_NO_WARNINGS"
        } else {
            "-lm"
        }));
    let native = run(&mut Command::new(&exe));

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
        .map(|((class, name, _), (r, n))| format!("{class}.{name}: jvm `{r}` native `{n}`"))
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
}
