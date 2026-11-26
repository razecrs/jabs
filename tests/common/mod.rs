//! Shared by the behavior tests that build one native program from a fixture (with java.base,
//! as in production) and compare each mechanism's run with the reference JVM's:
//! `<MainClass> <mechanism>` on both, then stdout, exit status and the class of an uncaught
//! exception must agree. A failure names the mechanism, the first differing line and where to
//! look. Needs `javac`, `java` and `clang`; missing tools skip, unless JABS_REQUIRE_TOOLS=1
//! (as CI sets).
#![allow(dead_code)]

use jabs::{cgen, classfile::ClassFile};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

/// Whether a tool runs successfully with the given probe argument.
pub fn tool_available(name: &str, arg: &str) -> bool {
    Command::new(name)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run a command, panic with its output on failure, and return stdout.
pub fn run(cmd: &mut Command) -> String {
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    assert!(
        output.status.success(),
        "{cmd:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

/// Clang target arguments that can build a C program: the default target, then MinGW on
/// Windows hosts without the MSVC SDK. JABS_CLANG_ARGS overrides (as in tests/differential.rs).
pub fn clang_target_args(dir: &Path) -> Vec<String> {
    if let Ok(args) = std::env::var("JABS_CLANG_ARGS") {
        return args.split_whitespace().map(String::from).collect();
    }
    let probe = dir.join("probe.c");
    fs::write(
        &probe,
        "#include <math.h>\n#include <stdio.h>\nint main(void) { return (int)fmod(1.0, 2.0); }\n",
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
    panic!("clang cannot build a C program for any known target; set JABS_CLANG_ARGS");
}

/// A fixture program, built once for every test of a file: its class directory and native
/// executable, and the class whose `main` runs a mechanism.
pub struct Built {
    pub classes: PathBuf,
    pub native: PathBuf,
    pub main_class: &'static str,
    /// The fixture's path, for failure messages.
    pub fixture: &'static str,
    /// What the build's `UnitExtras::executed_methods` made roots, when it had any.
    pub executed_roots: Option<cgen::ExecutedRoots>,
}

/// The native entry point: start the VM, pass the arguments as `String[]`, run `main`, and
/// report an escaping exception as the JVM does (status 1).
fn harness(class: &str) -> String {
    let record = cgen::class_record("java/lang/String");
    format!(
        r#"#include "jabs_unit.h"
extern const jabs_class {record};
int main(int argc, char **argv) {{
    if (jabs_vm_startup() != 0) {{ fprintf(stderr, "startup failed: %s\n", jabs_pending->cls->name); return 2; }}
    jabs_object *args = jabs_new_array(argc - 1, sizeof(jabs_object *), JABS_COMPONENT_CLASS, &{record}, "[Ljava/lang/String;", NULL);
    if (!args) {{ fprintf(stderr, "out of memory\n"); return 2; }}
    for (int i = 1; i < argc; i++) {{
        jabs_object *s = jabs_string_from_utf8(argv[i]);
        if (!s) {{ fprintf(stderr, "out of memory\n"); return 2; }}
        ((jabs_object **)jabs_array_data((jabs_array *)args))[i - 1] = s;
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

/// Build `fixture` (relative to the crate) once per test binary into `cell`, or None when tools
/// are missing and not required. A failed build is remembered: every test then fails with its
/// message instead of each one building again (a OnceLock initializer that panics would rerun
/// for the next test).
pub fn built_once(
    cell: &'static OnceLock<Result<Option<Built>, String>>,
    fixture: &'static str,
    main_class: &'static str,
) -> Option<&'static Built> {
    let result = cell.get_or_init(|| {
        std::panic::catch_unwind(|| build(fixture, main_class)).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building {fixture} natively failed:\n{message}"),
    }
}

/// Compile the fixture with javac (release 8 class files, like Minecraft 1.7.10's) and JABS
/// (with java.base, reachable code only), and link it.
fn build(fixture: &'static str, main_class: &'static str) -> Option<Built> {
    let extras = cgen::UnitExtras {
        startup: true,
        ..Default::default()
    };
    build_native(fixture, main_class, "", None, extras)
}

/// [`build`] with the unit's extras given (they should set `startup`, as a JVM runs it), roots
/// limited to `roots` (None: every fixture class), and its own build directory per `variant`
/// (so two builds of one fixture can coexist).
pub fn build_native(
    fixture: &'static str,
    main_class: &'static str,
    variant: &str,
    roots: Option<&[&str]>,
    extras: cgen::UnitExtras,
) -> Option<Built> {
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
        eprintln!("skipping {fixture} behavior tests; missing {missing:?}");
        return None;
    }
    // Kept for the whole test run: every test reads the classes and runs the executable.
    let dir = fresh_build_dir(fixture, variant);
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
    run(Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(&classes)
        .arg(&source));
    let mut units: Vec<ClassFile> = fs::read_dir(&classes)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "class"))
        .map(|p| ClassFile::parse(&fs::read(p).unwrap()).unwrap())
        .collect();
    let roots: std::collections::BTreeSet<String> = match roots {
        Some(named) => named.iter().map(|n| n.to_string()).collect(),
        None => units.iter().map(|c| c.name.clone()).collect(),
    };
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let (library, _, unparsable) = jabs::jdk::read_module(&home, "java.base").unwrap();
    assert_eq!(unparsable, 0, "java.base classes ClassFile::parse rejects");
    units.extend(library);
    let compiled = cgen::compile_unit_with(&units, Some(&roots), &extras);
    let unsupported: Vec<_> = compiled
        .methods
        .iter()
        .filter(|m| {
            roots.contains(&m.class) && matches!(m.status, cgen::MethodStatus::Unsupported { .. })
        })
        .collect();
    assert!(
        unsupported.is_empty(),
        "fixture methods JABS could not compile: {unsupported:#?}"
    );
    let mut sources = Vec::new();
    for (name, contents) in &compiled.c_files {
        fs::write(dir.join(name), contents).unwrap();
        if name.ends_with(".c") {
            sources.push(dir.join(name));
        }
    }
    let main_c = dir.join("harness.c");
    fs::write(&main_c, harness(main_class)).unwrap();
    sources.push(main_c);
    let target = clang_target_args(&dir);
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
    let objects: Vec<PathBuf> = std::thread::scope(|scope| {
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
    let native = dir.join(if cfg!(windows) {
        "fixture.exe"
    } else {
        "fixture"
    });
    run(Command::new("clang")
        .args(&target)
        .args(&objects)
        .arg("-o")
        .arg(&native)
        .args(if cfg!(windows) { None } else { Some("-lm") }));
    Some(Built {
        classes,
        native,
        main_class,
        fixture,
        executed_roots: compiled.executed_roots,
    })
}

/// What a finished run left behind: stdout, exit status, the internal name of an exception
/// that escaped `main`, and the line a JABS stop printed (`jabs: ...`).
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub status: Option<i32>,
    pub uncaught: Option<String>,
}

/// Run a command; also returns its JABS stop line, if any.
pub fn outcome(cmd: &mut Command, uncaught_prefix: &str) -> (Outcome, Option<String>) {
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let uncaught = stderr.lines().find_map(|line| {
        let rest = line.strip_prefix(uncaught_prefix)?;
        let name = rest.split(':').next().unwrap_or(rest).trim();
        Some(name.replace('.', "/"))
    });
    // A JABS stop ("... is not compiled to native code") is the most useful thing to show.
    let stop = stderr
        .lines()
        .find(|l| l.starts_with("jabs: "))
        .map(str::to_string);
    if stop.is_some() {
        eprintln!("native stderr:\n{stderr}");
    }
    (
        Outcome {
            stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            status: output.status.code(),
            uncaught,
        },
        stop,
    )
}

/// Run `<main class> <mechanism>` natively only; for mechanisms whose native result differs
/// from the JVM's on purpose (a JABS stop).
pub fn native_run(built: &Built, mechanism: &str) -> (Outcome, Option<String>) {
    outcome(Command::new(&built.native).arg(mechanism), "uncaught ")
}

/// Run `<main class> <mechanism>` on the JVM and natively and require identical outcomes.
/// `look` says where the mechanism lives, for the failure message.
pub fn same_as_jvm(built: Option<&Built>, mechanism: &str, look: &str) {
    let Some(built) = built else { return };
    let (jvm, _) = outcome(
        Command::new("java")
            .arg("-cp")
            .arg(&built.classes)
            .args([built.main_class, mechanism]),
        "Exception in thread \"main\" ",
    );
    assert!(
        jvm.status == Some(0) || jvm.uncaught.is_some(),
        "{mechanism}: the reference JVM run itself failed: {jvm:?}"
    );
    let (native, stop) = native_run(built, mechanism);
    if native == jvm {
        return;
    }
    let first_difference = jvm
        .stdout
        .lines()
        .zip(native.stdout.lines())
        .position(|(j, n)| j != n)
        .unwrap_or_else(|| {
            jvm.stdout
                .lines()
                .count()
                .min(native.stdout.lines().count())
        });
    let jvm_line = jvm.stdout.lines().nth(first_difference).unwrap_or("<end>");
    let native_line = native
        .stdout
        .lines()
        .nth(first_difference)
        .unwrap_or("<end>");
    panic!(
        "{mechanism} ({}): native differs from the JVM at output line {}\n  jvm:    {jvm_line}\n  native: {native_line}\n\
         look at: {look}\njabs stop: {}\njvm run: {jvm:#?}\nnative run: {native:#?}",
        built.fixture,
        first_difference + 1,
        stop.as_deref().unwrap_or("none")
    );
}

/// The build directory for one fixture of this checkout, replaced on every run: the newest build
/// stays for debugging, and runs do not pile up (a kept tempdir per run left about 216 MB behind
/// each time and filled the disk). Keyed by the checkout, so worktrees never share one.
fn fresh_build_dir(fixture: &str, variant: &str) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let stem = Path::new(fixture)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("fixture");
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-{stem}{variant}"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap_or_else(|e| {
            panic!("cannot clear the previous build in {}: {e}", dir.display())
        });
    }
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    dir
}

/// Build the method-trace agent (tools/method-trace/jabs_trace.c) as a shared library, trying
/// the host's default clang target, then MinGW.
pub fn build_agent(dir: &Path) -> PathBuf {
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let include = home.join("include");
    let platform = include.join(if cfg!(windows) { "win32" } else { "linux" });
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/method-trace/jabs_trace.c");
    let library = dir.join(if cfg!(windows) {
        "jabs_trace.dll"
    } else {
        "libjabs_trace.so"
    });
    let mut targets = vec![None];
    if cfg!(windows) {
        targets.push(Some("--target=x86_64-w64-windows-gnu"));
    }
    let mut last = String::new();
    for target in targets {
        let output = Command::new("clang")
            .args(target)
            .args(["-shared", "-O2", "-Wall", "-Wextra", "-Werror"])
            .args(if cfg!(windows) { None } else { Some("-fPIC") })
            .arg(format!("-I{}", include.display()))
            .arg(format!("-I{}", platform.display()))
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        if output.status.success() {
            return library;
        }
        last = String::from_utf8_lossy(&output.stderr).into_owned();
    }
    panic!("clang cannot build the trace agent:\n{last}");
}
