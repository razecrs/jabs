//! StackWalker / StackStreamFactory and jdk.internal.vm.Continuation.pin/unpin behavior:
//! tests/fixtures/StackWalk.java (`StackWalkCases <mechanism>`) is compiled against `java.base`
//! (needs JDK 22+ for `Option.DROP_METHOD_INFO`), linked natively, and compared line-by-line
//! against the reference JVM.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip locally and fail when
//! JABS_REQUIRE_TOOLS=1.

mod common;

use common::{Built, clang_target_args, run, same_as_jvm, tool_available};
use jabs::{cgen, classfile::ClassFile};
use std::{collections::BTreeSet, fs, path::Path, process::Command, sync::OnceLock};

const FIXTURE: &str = "tests/fixtures/StackWalk.java";
const MAIN_CLASS: &str = "StackWalkCases";
const LOOK: &str = "src/cgen/stack.rs";

static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();

fn built() -> Option<&'static Built> {
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(|| build_fixture(FIXTURE, MAIN_CLASS, "StackWalk", &[])).map_err(
            |e| {
                e.downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".to_string())
            },
        )
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building {FIXTURE} natively failed:\n{message}"),
    }
}

fn build_fixture(
    fixture: &'static str,
    main_class: &'static str,
    tag: &str,
    javac_extra: &[&str],
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

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-{tag}"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
    run(Command::new("javac")
        .args(["-nowarn"])
        .args(javac_extra)
        .arg("-d")
        .arg(&classes)
        .arg(&source));

    build_from_classes(&dir, &classes, fixture, main_class)
}

fn build_from_classes(
    dir: &Path,
    classes: &Path,
    fixture: &'static str,
    main_class: &'static str,
) -> Option<Built> {
    let mut units: Vec<ClassFile> = fs::read_dir(classes)
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
            init = cgen::init_symbol(main_class),
            main = cgen::symbol(main_class, "main", "([Ljava/lang/String;)V"),
        ),
    )
    .unwrap();
    sources.push(main_c);

    let target = clang_target_args(dir);
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
        classes: classes.to_path_buf(),
        native,
        main_class,
        fixture,
        executed_roots: compiled.executed_roots,
    })
}

#[test]
fn basic_walk_and_stack_trace_element() {
    same_as_jvm(built(), "basicWalk", LOOK);
}

#[test]
fn for_each_walk() {
    same_as_jvm(built(), "forEachWalk", LOOK);
}

#[test]
fn retain_class_reference_and_get_caller_class() {
    same_as_jvm(built(), "retainClassReference", LOOK);
}

#[test]
fn get_caller_class_direct_in_main_throws_illegal_caller() {
    same_as_jvm(built(), "getCallerClassDirectInMain", LOOK);
}

#[test]
fn show_reflect_frames_option() {
    same_as_jvm(built(), "showReflectFrames", LOOK);
}

#[test]
fn show_hidden_frames_option() {
    same_as_jvm(built(), "showHiddenFrames", LOOK);
}

#[test]
fn estimate_depth_and_multi_batch_cursor() {
    same_as_jvm(built(), "estimateDepth", LOOK);
}

#[test]
fn nested_stack_walks_and_closed_stream() {
    same_as_jvm(built(), "nestedWalk", LOOK);
}

#[test]
fn drop_method_info_uses_class_frame_info() {
    same_as_jvm(built(), "dropMethodInfo", LOOK);
}

#[test]
fn log4j2_style_caller_lookup() {
    same_as_jvm(built(), "log4jCallerLookup", LOOK);
}

#[test]
fn continuation_pin_and_unpin_are_no_ops() {
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
        eprintln!("skipping Continuation.pin/unpin test; missing {missing:?}");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let classes = dir.path().join("classes");
    fs::create_dir_all(&classes).unwrap();
    let source = dir.path().join("ContPin.java");
    fs::write(
        &source,
        "import jdk.internal.vm.Continuation;\n\
         import jdk.internal.vm.ContinuationSupport;\n\
         class ContPin {\n\
             public static void main(String[] args) {\n\
                 ContinuationSupport.pinIfSupported();\n\
                 ContinuationSupport.unpinIfSupported();\n\
                 if (ContinuationSupport.isSupported()) {\n\
                     Continuation.pin();\n\
                     Continuation.unpin();\n\
                 } else {\n\
                     try {\n\
                         Continuation.pin();\n\
                         throw new AssertionError(\"expected ExceptionInInitializerError\");\n\
                     } catch (ExceptionInInitializerError expected) {\n\
                         if (!(expected.getCause() instanceof UnsupportedOperationException)) {\n\
                             throw expected;\n\
                         }\n\
                     }\n\
                 }\n\
                 System.out.println(\"pinned-ok\");\n\
             }\n\
         }\n",
    )
    .unwrap();
    run(Command::new("javac")
        .args([
            "-nowarn",
            "--add-exports=java.base/jdk.internal.vm=ALL-UNNAMED",
            "-d",
        ])
        .arg(&classes)
        .arg(&source));

    let built = build_from_classes(dir.path(), &classes, "ContPin.java", "ContPin").unwrap();
    let (jvm, _) = common::outcome(
        Command::new("java")
            .args(["--add-exports=java.base/jdk.internal.vm=ALL-UNNAMED", "-cp"])
            .arg(&built.classes)
            .arg("ContPin"),
        "Exception in thread \"main\" ",
    );
    let (native, _) = common::native_run(&built, "ignored");
    assert_eq!(native, jvm, "Continuation.pin/unpin differed from JVM");
}
