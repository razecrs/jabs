//! java.lang.management: tests/fixtures/Management.java is compiled once into a native program
//! (with java.base, java.management and jdk.management, as the server's unit has them), and
//! each test runs one mechanism (`ManagementCases <mechanism>`) on the reference JVM and
//! natively. Deterministic output is compared line by line; clocks and ids are printed by the
//! program as the properties they must have, or checked here (the process id). A failure names
//! the mechanism, the first differing line and where to look.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

use jabs::{cgen, classfile::ClassFile};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

/// What the native natives live in, for failure messages.
const NATIVES: &str = "src/cgen/management.rs";
/// The fixture uses sun.management directly (javac needs it exported) and one private method
/// reflectively (the JVM needs it opened; opening exports too). JABS does not enforce module
/// access. Only mechanisms that do not print the input arguments run the JVM with it.
const EXPORT_SUN_MANAGEMENT: &str = "java.management/sun.management=ALL-UNNAMED";
const OPEN_SUN_MANAGEMENT: &str = "--add-opens=java.management/sun.management=ALL-UNNAMED";

fn tool_available(name: &str, arg: &str) -> bool {
    Command::new(name)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run a command, panic with its output on failure.
fn run(cmd: &mut Command) {
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    assert!(
        output.status.success(),
        "{cmd:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Clang target arguments that can build a C program (as in tests/reflection.rs).
fn clang_target_args(dir: &Path) -> Vec<String> {
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

struct Built {
    classes: PathBuf,
    native: PathBuf,
}

/// The native entry point, shaped like the `java` launcher (as program_harness in
/// tests/differential.rs): leading -D arguments are properties, `--` ends them; start the VM,
/// run main, report an escaping exception, then wait for non-daemon threads (DestroyJavaVM).
fn harness(class: &str) -> String {
    let record = cgen::class_record("java/lang/String");
    format!(
        r#"#include "jabs_unit.h"
extern const jabs_class {record};
int main(int argc, char **argv) {{
    if (argc < 1) return 2;
    static const char *defines[64];
    int first = 1, count = 0;
    while (first < argc && strncmp(argv[first], "-D", 2) == 0) {{
        if (count == 64) {{ fprintf(stderr, "too many -D properties\n"); return 2; }}
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
    int status = 0;
    if (jabs_pending) {{ fprintf(stderr, "uncaught %s\n", jabs_pending->cls->name); status = 1; }}
    jabs_destroy_java_vm();
    return status;
}}
"#,
        init = cgen::init_symbol(class),
        main = cgen::symbol(class, "main", "([Ljava/lang/String;)V"),
    )
}

/// Compile the fixture once for every test; a failed build is remembered (see tests/reflection.rs).
fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => {
            panic!("building tests/fixtures/Management.java natively failed:\n{message}")
        }
    }
}

fn build() -> Option<Built> {
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
        eprintln!("skipping management tests; missing {missing:?}");
        return None;
    }
    let dir = fresh_build_dir("tests/fixtures/Management.java");
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/Management.java");
    run(Command::new("javac")
        .args(["-nowarn", "--add-exports", EXPORT_SUN_MANAGEMENT, "-d"])
        .arg(&classes)
        .arg(&fixture));
    let mut units: Vec<ClassFile> = fs::read_dir(&classes)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "class"))
        .map(|p| ClassFile::parse(&fs::read(p).unwrap()).unwrap())
        .collect();
    let roots: std::collections::BTreeSet<String> = units.iter().map(|c| c.name.clone()).collect();
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    for module in ["java.base", "java.management", "jdk.management"] {
        let (library, _, unparsable) = jabs::jdk::read_module(&home, module).unwrap();
        assert_eq!(unparsable, 0, "{module} classes ClassFile::parse rejects");
        units.extend(library);
    }
    let extras = cgen::UnitExtras {
        startup: true,
        ..Default::default()
    };
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
    fs::write(&main_c, harness("ManagementCases")).unwrap();
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
        "management.exe"
    } else {
        "management"
    });
    run(Command::new("clang")
        .args(&target)
        .args(&objects)
        .arg("-o")
        .arg(&native)
        .args(if cfg!(windows) {
            &[][..]
        } else {
            &["-pthread", "-lm"][..]
        }));
    Some(Built { classes, native })
}

/// What a finished run left behind, and the id of the process that ran it.
#[derive(Debug)]
struct Outcome {
    stdout: String,
    status: Option<i32>,
    uncaught: Option<String>,
    pid: u32,
}

impl PartialEq for Outcome {
    /// Two runs agree on what they printed and how they ended (never on their process ids).
    fn eq(&self, other: &Self) -> bool {
        self.stdout == other.stdout
            && self.status == other.status
            && self.uncaught == other.uncaught
    }
}

fn outcome(cmd: &mut Command, uncaught_prefix: &str) -> Outcome {
    let child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"));
    let pid = child.id();
    let output = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("wait for {cmd:?}: {e}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let uncaught = stderr.lines().find_map(|line| {
        let rest = line.strip_prefix(uncaught_prefix)?;
        let name = rest.split(':').next().unwrap_or(rest).trim();
        Some(name.replace('.', "/"))
    });
    // A JABS stop ("... without a JABS implementation") is the most useful thing to show.
    if let Some(stop) = stderr.lines().find(|l| l.starts_with("jabs: ")) {
        eprintln!("native stderr: {stop}\n{stderr}");
    }
    Outcome {
        stdout: String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        status: output.status.code(),
        uncaught,
        pid,
    }
}

/// One mechanism on the JVM (with `jvm_options`) and natively, both with the -D `properties`.
fn runs(mechanism: &str, properties: &[&str], jvm_options: &[&str]) -> Option<(Outcome, Outcome)> {
    let built = built()?;
    let properties: Vec<String> = properties.iter().map(|p| format!("-D{p}")).collect();
    let jvm = outcome(
        Command::new("java")
            .args(jvm_options)
            .args(&properties)
            .arg("-cp")
            .arg(&built.classes)
            .args(["ManagementCases", mechanism]),
        "Exception in thread \"main\" ",
    );
    assert!(
        jvm.status == Some(0),
        "{mechanism}: the reference JVM run itself failed: {jvm:#?}"
    );
    let native = outcome(
        Command::new(&built.native)
            .args(&properties)
            .arg("--")
            .arg(mechanism),
        "uncaught ",
    );
    Some((jvm, native))
}

/// Require the native run to print what the JVM printed; `look` says where the mechanism lives.
fn same_as_jvm(mechanism: &str, properties: &[&str], jvm_options: &[&str], look: &str) {
    let Some((jvm, native)) = runs(mechanism, properties, jvm_options) else {
        return;
    };
    if native == jvm {
        return;
    }
    let line = jvm
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
    panic!(
        "{mechanism}: native differs from the JVM at output line {}\n  jvm:    {}\n  native: {}\n\
         look at: {look}\njvm run: {jvm:#?}\nnative run: {native:#?}",
        line + 1,
        jvm.stdout.lines().nth(line).unwrap_or("<end>"),
        native.stdout.lines().nth(line).unwrap_or("<end>"),
    );
}

#[test]
fn start_time_and_input_arguments_follow_the_jvm() {
    same_as_jvm(
        "startTimeAndInputArguments",
        &["jabs.mgmt.value=a=b c", "jabs.mgmt.flag"],
        &[],
        &format!(
            "getStartupTime/getVmArguments0 in {NATIVES}; jabs_platform_record_start in \
             runtime/jabs_platform.c (called first in jabs_vm_startup); \
             jabs_management_vm_arguments in runtime/jabs_management.c"
        ),
    );
}

#[test]
fn input_arguments_are_empty_without_properties() {
    same_as_jvm(
        "startTimeAndInputArguments",
        &[],
        &[],
        "jabs_management_vm_arguments in runtime/jabs_management.c (an empty String[])",
    );
}

#[test]
fn management_version_is_the_jvms() {
    same_as_jvm(
        "managementVersion",
        &[],
        &[],
        &format!("MANAGEMENT_VERSION (getVersion0) in {NATIVES}"),
    );
}

#[test]
fn uptime_is_monotonic_and_follows_the_wall_clock() {
    same_as_jvm(
        "uptimeAdvances",
        &[],
        &[],
        "jabs_platform_uptime_millis / jabs_platform_start_millis in runtime/jabs_platform.c",
    );
}

#[test]
fn thread_counts_follow_starts_and_joins() {
    same_as_jvm(
        "threadCounts",
        &[],
        &[],
        "jabs_mgmt_* counts in jabs_thread_start / jabs_thread_terminate and \
         jabs_thread_reset_peak in runtime/jabs_thread.c",
    );
}

#[test]
fn class_counts_are_consistent_and_initialization_counts() {
    same_as_jvm(
        "classCounts",
        &[],
        &[OPEN_SUN_MANAGEMENT],
        &format!(
            "getTotalClassCount (jabs_class_record_count, emit_class_registry in src/cgen.rs) \
             and getInitializedClassCount (jabs_classes_initialized in jabs_class_init_end, \
             runtime/jabs_thread.c) in {NATIVES}"
        ),
    );
}

#[test]
fn heap_usage_follows_allocation() {
    same_as_jvm(
        "heapUsage",
        &[],
        &[],
        &format!(
            "MemoryImpl.getMemoryUsage0 in {NATIVES} (jabs_heap_bytes) and \
             jabs_management_memory_usage (management::glue, constructed_by in reachable())"
        ),
    );
}

#[test]
fn operating_system_basics_follow_the_jvm() {
    same_as_jvm(
        "operatingSystem",
        &[],
        &[],
        &format!(
            "OperatingSystemImpl.initialize0 and Unsafe.getLoadAverage0 in {NATIVES}; \
             jabs_platform_load_average in runtime/jabs_platform.c; getAvailableProcessors \
             (jabs_thread_processor_count)"
        ),
    );
}

#[test]
fn process_id_is_the_process_that_runs() {
    let Some((jvm, native)) = runs("processId", &[], &[OPEN_SUN_MANAGEMENT]) else {
        return;
    };
    // Guard that the mechanism reports the process id on the JVM itself.
    assert_eq!(
        jvm.stdout.trim(),
        jvm.pid.to_string(),
        "the JVM's getProcessId"
    );
    assert_eq!(
        (native.status, native.stdout.trim()),
        (Some(0), native.pid.to_string().as_str()),
        "VMManagementImpl.getProcessId natively: jabs_platform_process_id in \
         runtime/jabs_platform.c\nnative run: {native:#?}"
    );
}

/// JABS's own answers (the JVM's differ: HotSpot supports all but GC notifications). Each is
/// explained in SUPPORT_FLAGS.
#[test]
fn support_flags_are_jabs_decisions() {
    let Some((jvm, native)) = runs("supportFlags", &[], &[OPEN_SUN_MANAGEMENT]) else {
        return;
    };
    // Guard that the fixture reads all eight flags on the JVM.
    assert_eq!(
        jvm.stdout
            .lines()
            .filter(|l| l.ends_with(" true") || l.ends_with(" false"))
            .count(),
        8,
        "the fixture reads the flags on the JVM: {jvm:#?}"
    );
    let expected = [
        "isCompilationTimeMonitoringSupported false",
        "isThreadContentionMonitoringSupported false",
        "isCurrentThreadCpuTimeSupported false",
        "isOtherThreadCpuTimeSupported false",
        "isObjectMonitorUsageSupported false",
        "isSynchronizerUsageSupported false",
        "isThreadAllocatedMemorySupported false",
        "isRemoteDiagnosticCommandsSupported false",
    ]
    .map(|l| format!("{l}\n"))
    .concat();
    assert_eq!(
        (native.status, native.stdout.as_str()),
        (Some(0), expected.as_str()),
        "initOptionalSupportFields (SUPPORT_FLAGS, support_fields_body) in {NATIVES}\n\
         native run: {native:#?}"
    );
}

/// The build directory for one fixture of this checkout, replaced on every run: the newest build
/// stays for debugging, and runs do not pile up (a kept tempdir per run left about 216 MB behind
/// each time and filled the disk). Keyed by the checkout, so worktrees never share one.
fn fresh_build_dir(fixture: &str) -> PathBuf {
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
        .join(format!("{hash:016x}-{stem}"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap_or_else(|e| {
            panic!("cannot clear the previous build in {}: {e}", dir.display())
        });
    }
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    dir
}
