//! Garbage collector correctness, Weak/PhantomReference clearing, Cleaner execution,
//! bounded-heap churn reclamation, and cooperative multithreaded STW safepoints.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

mod common;

use common::{Built, clang_target_args, native_run, run, same_as_jvm, tool_available};
use jabs::{cgen, classfile::ClassFile};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const FIXTURE: &str = "tests/fixtures/GcStress.java";

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build_gc_fixture).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building {FIXTURE} natively failed:\n{message}"),
    }
}

fn build_gc_fixture() -> Option<Built> {
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
        eprintln!("skipping {FIXTURE} GC tests; missing {missing:?}");
        return None;
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-GcStress"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    run(Command::new("javac")
        .args(["--release", "21", "-nowarn", "-d"])
        .arg(&classes)
        .arg(&source));

    let mut units: Vec<ClassFile> = fs::read_dir(&classes)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "class"))
        .map(|p| ClassFile::parse(&fs::read(p).unwrap()).unwrap())
        .collect();
    let roots: BTreeSet<String> = units.iter().map(|c| c.name.clone()).collect();
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let (library, _, unparsable) = jabs::jdk::read_module(&home, "java.base").unwrap();
    assert_eq!(unparsable, 0, "java.base classes ClassFile::parse rejects");
    units.extend(library);

    let extras = cgen::UnitExtras {
        startup: true,
        java_home: Some(home.display().to_string()),
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
    let record = cgen::class_record("java/lang/String");
    let main_c = dir.join("harness.c");
    fs::write(
        &main_c,
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
            init = cgen::init_symbol("GcStressCases"),
            main = cgen::symbol("GcStressCases", "main", "([Ljava/lang/String;)V"),
        ),
    )
    .unwrap();
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
            .map(|src| {
                let obj = src.with_extension("o");
                let target = &target;
                scope.spawn(move || {
                    run(Command::new("clang")
                        .args(target)
                        .args(flags)
                        .arg("-c")
                        .arg(src)
                        .arg("-o")
                        .arg(&obj));
                    obj
                })
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap()).collect()
    });
    let native = dir.join(if cfg!(windows) {
        "gc_stress.exe"
    } else {
        "gc_stress"
    });
    run(Command::new("clang")
        .args(&target)
        .args(&objects)
        .arg("-o")
        .arg(&native)
        .args(if cfg!(windows) {
            &["-lws2_32", "-liphlpapi"][..]
        } else {
            &["-pthread", "-lm"][..]
        }));

    Some(Built {
        classes,
        native,
        main_class: "GcStressCases",
        fixture: FIXTURE,
        executed_roots: compiled.executed_roots,
    })
}

#[test]
fn churn_preserves_live_objects_and_matches_jvm() {
    same_as_jvm(
        built(),
        "churn",
        "jabs_gc_alloc_obj, Immix block reuse, and LOS sweep in runtime/jabs_gc.c",
    );
}

#[test]
fn churn_reclaims_memory_under_bound() {
    let Some(built) = built() else {
        return;
    };
    let (native, stop) = native_run(built, "churnBounded");
    assert_eq!(native.status, Some(0), "stop={stop:?}, run={native:#?}");
    assert!(
        native.stdout.contains("ok=true "),
        "native GC failed bounded-memory check: stdout={}",
        native.stdout
    );
}

#[test]
fn weak_and_phantom_references_clear_and_enqueue() {
    same_as_jvm(
        built(),
        "weakQueue",
        "gc_process_references and ReferenceHandler queue in runtime/jabs_gc.c",
    );
}

#[test]
fn cleaner_runs_when_referent_collected() {
    same_as_jvm(
        built(),
        "cleaner",
        "java.lang.ref.Cleaner and PhantomReference cleanup in runtime/jabs_gc.c",
    );
}

/// The JVM with `-Xmx<size>` against the native program with `JABS_MAX_HEAP=<size>`: the same
/// maxMemory, and the same answer to whether 200 MB of small arrays fit.
fn heap_limit_matches_jvm(size: &str) {
    let Some(built) = built() else {
        return;
    };
    let (jvm, _) = common::outcome(
        Command::new("java")
            .arg(format!("-Xmx{size}"))
            .arg("-cp")
            .arg(&built.classes)
            .args([built.main_class, "heapLimit"]),
        "Exception in thread \"main\" ",
    );
    assert_eq!(
        jvm.status,
        Some(0),
        "the reference JVM run itself failed: {jvm:?}"
    );
    let (native, stop) = common::outcome(
        Command::new(&built.native)
            .env("JABS_MAX_HEAP", size)
            .arg("heapLimit"),
        "uncaught ",
    );
    assert_eq!(
        native, jvm,
        "heapLimit with {size}: native differs from the JVM\nlook at: gc_choose_max_heap and \
         jabs_gc_max_memory in runtime/jabs_gc.c\njabs stop: {stop:?}"
    );
}

#[test]
fn small_heap_limit_throws_out_of_memory_like_the_jvm() {
    heap_limit_matches_jvm("96m");
}

#[test]
fn large_heap_limit_holds_what_the_jvm_holds() {
    heap_limit_matches_jvm("512m");
}

/// A heap that is not a power of two: mmap then reserves on a page, not a 64 KB block boundary,
/// and the arena must still start on one (blocks are found by masking addresses). Before the
/// rounding in gc_init_impl, sizes like 3999m segfaulted on Linux while 4000m ran.
#[test]
fn odd_heap_size_keeps_blocks_aligned() {
    let Some(built) = built() else {
        return;
    };
    for size in ["3999m", "777m"] {
        let (native, stop) = common::outcome(
            Command::new(&built.native)
                .env("JABS_MAX_HEAP", size)
                .arg("churnBounded"),
            "uncaught ",
        );
        assert_eq!(
            native.status,
            Some(0),
            "{size}: stop={stop:?}, run={native:#?}\nlook at: the arena reservation in gc_init_impl \
             (runtime/jabs_gc.c)"
        );
        assert!(
            native.stdout.contains("ok=true "),
            "{size}: {}",
            native.stdout
        );
    }
}

#[test]
fn multithreaded_stw_safepoints_across_waiters() {
    same_as_jvm(
        built(),
        "multithreadedStw",
        "jabs_gc_safepoint_slow and enter/exit_native in runtime/jabs_gc.c",
    );
}
