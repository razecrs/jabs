//! Differential tests for `jdk.internal.misc.Unsafe` (`park`, `unpark`, `allocateInstance`,
//! `shouldBeInitialized0`), `java.lang.ref.Reference` pending-list stubs, `PreviewFeatures`, and
//! `LambdaProxyClassArchive`.
//!
//! `tests/fixtures/UnsafePark.java` tests `LockSupport`, `ReentrantLock`, `CountDownLatch`,
//! `WeakReference`, and lambdas on `--release 8`; a second generated fixture compiled with
//! `--add-exports java.base/jdk.internal.misc=ALL-UNNAMED` tests `jdk.internal.misc.Unsafe` and
//! `PreviewFeatures` directly against the reference JVM.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

mod common;

use common::{Built, built_once, clang_target_args, outcome, run, same_as_jvm, tool_available};
use jabs::{cgen, classfile::ClassFile};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const FIXTURE: &str = "tests/fixtures/UnsafePark.java";

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    built_once(&BUILT, FIXTURE, "UnsafePark")
}

const INTERNAL_FIXTURE: &str = r#"import jdk.internal.misc.PreviewFeatures;
import jdk.internal.misc.Unsafe;

class UnsafeInternalCases {
    static class Concrete {
        static int clinitCount;
        static {
            clinitCount++;
        }
        int x;
        String label;

        Concrete(int x) {
            throw new AssertionError("<init> must not run in allocateInstance");
        }

        String describe() {
            return x + ":" + label + ":" + clinitCount;
        }
    }

    static abstract class AbstractWithClinit {
        static {
            System.out.println("AbstractWithClinit.<clinit> should not run");
        }
    }

    interface InterfaceWithClinit {
        int VALUE = recordInit();
        static int recordInit() {
            System.out.println("InterfaceWithClinit.<clinit> should not run");
            return 1;
        }
    }

    static class UninitCheck {
        static boolean duringClinit;
        static {
            duringClinit = Unsafe.getUnsafe().shouldBeInitialized(UninitCheck.class);
        }
    }

    static class LazyClass {
        static int v = 7;
    }

    static class FailingClinit {
        static {
            if (true) throw new RuntimeException("boom");
        }
    }

    static class CrossMethodLazy {
        static int touched;
        static {
            touched = 42;
        }
    }

    static class CrossMethodAlloc {
        static int seq;
        static {
            seq = 9;
        }
        int val;
        CrossMethodAlloc() {
            throw new AssertionError("<init> must not run");
        }
        int report() {
            return val + seq;
        }
    }

    static Class<?> crossLazyClass() {
        return CrossMethodLazy.class;
    }

    static Class<?> crossAllocClass() {
        return CrossMethodAlloc.class;
    }

    static Object allocViaParam(Unsafe u, Class<?> c) throws Exception {
        return u.allocateInstance(c);
    }

    public static void main(String[] args) throws Exception {
        String mechanism = args.length > 0 ? args[0] : "";
        if (mechanism.equals("allocateInstance")) allocateInstance();
        else if (mechanism.equals("allocateNull")) allocateNull();
        else if (mechanism.equals("shouldBeInitialized")) shouldBeInitialized();
        else if (mechanism.equals("internalPark")) internalPark();
        else if (mechanism.equals("previewFeatures")) previewFeatures();
        else throw new IllegalArgumentException("unknown mechanism: " + mechanism);
    }

    static void allocateInstance() throws Exception {
        Unsafe u = Unsafe.getUnsafe();
        System.out.println("before alloc: " + u.shouldBeInitialized(Concrete.class));
        Concrete c = (Concrete) u.allocateInstance(Concrete.class);
        System.out.println("after alloc: " + u.shouldBeInitialized(Concrete.class));
        System.out.println("instance: " + c.describe());

        Class<?> crossCls = crossAllocClass();
        System.out.println("cross before: " + u.shouldBeInitialized(crossCls));
        CrossMethodAlloc cross = (CrossMethodAlloc) allocViaParam(u, crossCls);
        System.out.println("cross after: " + u.shouldBeInitialized(crossCls));
        System.out.println("cross report: " + cross.report());

        for (Class<?> bad : new Class<?>[] {
            int.class, void.class, int[].class, String[].class,
            AbstractWithClinit.class, InterfaceWithClinit.class
        }) {
            try {
                u.allocateInstance(bad);
                System.out.println("unexpectedly allocated: " + bad.getName());
            } catch (InstantiationException expected) {
                System.out.println("rejected");
            }
        }
    }

    static void allocateNull() throws Exception {
        Unsafe.getUnsafe().allocateInstance(null);
    }

    static void shouldBeInitialized() {
        Unsafe u = Unsafe.getUnsafe();
        System.out.println("prim: " + u.shouldBeInitialized(int.class));
        System.out.println("primArr: " + u.shouldBeInitialized(int[].class));
        System.out.println("refArr: " + u.shouldBeInitialized(String[].class));
        System.out.println("lazy before: " + u.shouldBeInitialized(LazyClass.class));
        int v = LazyClass.v;
        System.out.println("lazy after (" + v + "): " + u.shouldBeInitialized(LazyClass.class));
        System.out.println("during clinit: " + UninitCheck.duringClinit);
        System.out.println("after clinit: " + u.shouldBeInitialized(UninitCheck.class));
        try {
            u.ensureClassInitialized(FailingClinit.class);
        } catch (Throwable ignored) {
        }
        System.out.println("failed clinit: " + u.shouldBeInitialized(FailingClinit.class));
        Class<?> lazyCross = crossLazyClass();
        System.out.println("cross lazy before: " + u.shouldBeInitialized(lazyCross));
        u.ensureClassInitialized(lazyCross);
        System.out.println("cross lazy after: " + u.shouldBeInitialized(lazyCross) + ":" + CrossMethodLazy.touched);
    }

    static void internalPark() {
        Unsafe u = Unsafe.getUnsafe();
        Thread t = Thread.currentThread();
        // Negative relative time and zero absolute time do not wait. On Windows they also leave an
        // existing permit; on Linux HotSpot takes it first, so the last park waits its 10s there.
        u.unpark(t);
        u.park(false, -1L);
        u.park(true, 0L);
        long t0 = System.nanoTime();
        u.park(false, 10_000_000_000L);
        System.out.println("permit preserved: " + ((System.nanoTime() - t0) < 2_000_000_000L));
        long t1 = System.nanoTime();
        u.park(false, 120_000_000L);
        System.out.println("permit consumed: " + ((System.nanoTime() - t1) >= 50_000_000L));

        // When both permit and interrupt are present, permit is consumed and interrupt stays set.
        u.unpark(t);
        t.interrupt();
        u.park(false, 0L);
        System.out.println("still interrupted: " + Thread.interrupted());
        long t2 = System.nanoTime();
        u.park(false, 120_000_000L);
        System.out.println("permit consumed with interrupt: " + ((System.nanoTime() - t2) >= 50_000_000L));
    }

    static void previewFeatures() {
        System.out.println("preview: " + PreviewFeatures.isEnabled());
        try {
            PreviewFeatures.ensureEnabled();
            System.out.println("unexpectedly enabled");
        } catch (UnsupportedOperationException expected) {
            System.out.println("unsupported");
        }
    }
}
"#;

fn built_internal() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build_internal).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building UnsafeInternalCases natively failed:\n{message}"),
    }
}

fn build_internal() -> Option<Built> {
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
        eprintln!("skipping UnsafeInternalCases tests; missing {missing:?}");
        return None;
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-UnsafeInternalCases"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let source = dir.join("UnsafeInternalCases.java");
    fs::write(&source, INTERNAL_FIXTURE).unwrap();

    run(Command::new("javac")
        .args([
            "--add-exports",
            "java.base/jdk.internal.misc=ALL-UNNAMED",
            "-nowarn",
            "-d",
        ])
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
        "UnsafeInternalCases methods JABS could not compile: {unsupported:#?}"
    );
    let mut sources = Vec::new();
    for (name, contents) in &compiled.c_files {
        fs::write(dir.join(name), contents).unwrap();
        if name.ends_with(".c") {
            sources.push(dir.join(name));
        }
    }
    let record = cgen::class_record("java/lang/String");
    let harness = format!(
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
        init = cgen::init_symbol("UnsafeInternalCases"),
        main = cgen::symbol("UnsafeInternalCases", "main", "([Ljava/lang/String;)V"),
    );
    let main_c = dir.join("harness.c");
    fs::write(&main_c, harness).unwrap();
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
        .args(if cfg!(windows) {
            &["-lws2_32", "-liphlpapi"][..]
        } else {
            &["-lm"][..]
        }));
    Some(Built {
        classes,
        native,
        main_class: "UnsafeInternalCases",
        fixture: "UnsafeInternalCases.java",
        executed_roots: compiled.executed_roots,
    })
}

fn same_as_jvm_internal(mechanism: &str, look: &str) {
    let Some(built) = built_internal() else {
        return;
    };
    let (jvm, _) = outcome(
        Command::new("java")
            .args([
                "--add-exports",
                "java.base/jdk.internal.misc=ALL-UNNAMED",
                "-cp",
            ])
            .arg(&built.classes)
            .args([built.main_class, mechanism]),
        "Exception in thread \"main\" ",
    );
    assert!(
        jvm.status == Some(0) || jvm.uncaught.is_some(),
        "{mechanism}: the reference JVM run itself failed: {jvm:?}"
    );
    let (native, stop) = common::native_run(built, mechanism);
    assert_eq!(
        native,
        jvm,
        "{mechanism} ({}): native differs from JVM\nlook at: {look}\njabs stop: {}",
        built.fixture,
        stop.as_deref().unwrap_or("none")
    );
}

#[test]
fn lock_support_unpark_before_park() {
    same_as_jvm(
        built(),
        "unparkBeforePark",
        "jabs_thread_park and jabs_thread_unpark in runtime/jabs_thread.c",
    );
}

#[test]
fn lock_support_park_and_unpark() {
    same_as_jvm(
        built(),
        "parkAndUnpark",
        "jabs_thread_park and jabs_thread_unpark in runtime/jabs_thread.c",
    );
}

#[test]
fn lock_support_park_nanos_and_until() {
    same_as_jvm(
        built(),
        "parkNanosAndUntil",
        "jabs_thread_park relative/absolute timeout handling in runtime/jabs_thread.c",
    );
}

#[test]
fn lock_support_interrupt_preserves_status() {
    same_as_jvm(
        built(),
        "parkInterrupt",
        "jabs_thread_park interrupt check in runtime/jabs_thread.c",
    );
}

#[test]
fn lock_support_reentrant_lock_and_reference() {
    same_as_jvm(
        built(),
        "locksAndReferences",
        "LockSupport, Reference pending-list stubs, and LambdaProxyClassArchive in src/cgen/unsafe_rt.rs",
    );
}

#[test]
fn unsafe_allocate_instance() {
    same_as_jvm_internal(
        "allocateInstance",
        "jabs_unsafe_allocate_instance in src/cgen/unsafe_rt.rs",
    );
}

#[test]
fn unsafe_allocate_instance_null_throws_npe() {
    let Some(built) = built_internal() else {
        return;
    };
    let (native, stop) = common::native_run(built, "allocateNull");
    assert_eq!(native.status, Some(1), "stop: {stop:?}");
    assert_eq!(
        native.uncaught.as_deref(),
        Some("java/lang/NullPointerException"),
        "native run: {native:#?}"
    );
}

#[test]
fn unsafe_should_be_initialized() {
    same_as_jvm_internal(
        "shouldBeInitialized",
        "jabs_unsafe_should_be_initialized in src/cgen/unsafe_rt.rs",
    );
}

#[test]
fn unsafe_internal_park_edge_cases() {
    same_as_jvm_internal(
        "internalPark",
        "jabs_thread_park / jabs_thread_unpark in runtime/jabs_thread.c",
    );
}

#[test]
fn preview_features_disabled() {
    same_as_jvm_internal(
        "previewFeatures",
        "PreviewFeatures.isPreviewEnabled in src/cgen/unsafe_rt.rs",
    );
}
