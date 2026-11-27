//! Differential and unit tests for `src/cgen/boot_extras.rs`:
//! `jdk.internal.perf.Perf`, `java.lang.Class.getPermittedSubclasses0`,
//! `jdk.internal.loader.BootLoader.getSystemPackageLocation`,
//! `jdk.internal.jimage.NativeImageBuffer.getNativeMap`, and
//! `jdk.internal.loader.NativeLibrary.findEntry0`.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip locally and fail when
//! `JABS_REQUIRE_TOOLS=1`.

mod common;

use common::{Built, clang_target_args, native_run, outcome, run, tools_ready};
use jabs::{cgen, classfile::ClassFile};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const FIXTURE: &str = "tests/fixtures/BootExtrasCases.java";
const MAIN_CLASS: &str = "BootExtrasCases";
const LOOK: &str = "src/cgen/boot_extras.rs";

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build_fixture).map_err(|e| {
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

fn build_fixture() -> Option<Built> {
    if !tools_ready(FIXTURE) {
        return None;
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-BootExtrasCases"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    run(Command::new("javac")
        .args([
            "--add-exports",
            "java.base/jdk.internal.perf=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.loader=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.module=ALL-UNNAMED",
            "-nowarn",
            "-d",
        ])
        .arg(&classes)
        .arg(&source));

    // Delete BootExtrasCases$Ghost.class before both native compile and JVM execution so both test
    // getPermittedSubclasses0 filtering out an unloadable subclass.
    fs::remove_file(classes.join("BootExtrasCases$Ghost.class")).unwrap();

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

    let mut executed = BTreeSet::new();
    executed.insert((
        "jdk/internal/loader/NativeLibrary".to_string(),
        "findEntry0".to_string(),
        "(JLjava/lang/String;)J".to_string(),
    ));
    let extras = cgen::UnitExtras {
        startup: true,
        java_home: Some(home.display().to_string()),
        executed_methods: executed,
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

    for (class, name, desc) in [
        (
            "jdk/internal/perf/Perf",
            "createLong",
            "(Ljava/lang/String;IIJ)Ljava/nio/ByteBuffer;",
        ),
        (
            "jdk/internal/perf/Perf",
            "createByteArray",
            "(Ljava/lang/String;II[BI)Ljava/nio/ByteBuffer;",
        ),
        ("jdk/internal/perf/Perf", "highResCounter", "()J"),
        ("jdk/internal/perf/Perf", "highResFrequency", "()J"),
        (
            "jdk/internal/perf/Perf",
            "attach0",
            "(I)Ljava/nio/ByteBuffer;",
        ),
        (
            "jdk/internal/perf/Perf",
            "detach",
            "(Ljava/nio/ByteBuffer;)V",
        ),
        ("jdk/internal/perf/Perf", "registerNatives", "()V"),
        (
            "java/lang/Class",
            "getPermittedSubclasses0",
            "()[Ljava/lang/Class;",
        ),
        (
            "jdk/internal/loader/BootLoader",
            "getSystemPackageLocation",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        (
            "jdk/internal/jimage/NativeImageBuffer",
            "getNativeMap",
            "(Ljava/lang/String;)Ljava/nio/ByteBuffer;",
        ),
        (
            "jdk/internal/loader/NativeLibrary",
            "findEntry0",
            "(JLjava/lang/String;)J",
        ),
    ] {
        let found = compiled
            .methods
            .iter()
            .find(|m| m.class == class && m.name == name && m.descriptor == desc)
            .unwrap_or_else(|| panic!("missing method report for {class}.{name}{desc}"));
        assert!(
            matches!(found.status, cgen::MethodStatus::Compiled { .. }),
            "expected {class}.{name}{desc} to compile, got {:?}",
            found.status
        );
    }

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
            init = cgen::init_symbol(MAIN_CLASS),
            main = cgen::symbol(MAIN_CLASS, "main", "([Ljava/lang/String;)V"),
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
        main_class: MAIN_CLASS,
        fixture: FIXTURE,
        executed_roots: compiled.executed_roots,
    })
}

fn build_modules_fixture() -> Option<Built> {
    if !tools_ready(FIXTURE) {
        return None;
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-BootExtrasCases-modules"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = manifest.join(FIXTURE);
    run(Command::new("javac")
        .args([
            "--add-exports",
            "java.base/jdk.internal.perf=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.loader=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.module=ALL-UNNAMED",
            "-XDstringConcat=inline",
            "-nowarn",
            "-d",
        ])
        .arg(&classes)
        .arg(&source)
        .arg(manifest.join("runtime/java/jabs/rt/AppLoader.java"))
        .arg(manifest.join("runtime/java/jabs/rt/Natives.java")));

    let mut units = Vec::new();
    let mut stack = vec![classes.clone()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "class") {
                units.push(ClassFile::parse(&fs::read(p).unwrap()).unwrap());
            }
        }
    }
    let roots: BTreeSet<String> = units
        .iter()
        .filter(|c| !c.name.starts_with("jabs/rt/"))
        .map(|c| c.name.clone())
        .collect();
    let app_classes = roots.clone();
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let (library, _, unparsable) = jabs::jdk::read_module(&home, "java.base").unwrap();
    assert_eq!(unparsable, 0, "java.base classes ClassFile::parse rejects");
    units.extend(library);

    let extras = cgen::UnitExtras {
        app_classes,
        startup: true,
        java_home: Some(home.display().to_string()),
        jdk_modules: jabs::jdk::modules(&home).unwrap(),
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
        "modules fixture methods JABS could not compile: {unsupported:#?}"
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
            init = cgen::init_symbol(MAIN_CLASS),
            main = cgen::symbol(MAIN_CLASS, "main", "([Ljava/lang/String;)V"),
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
        main_class: MAIN_CLASS,
        fixture: FIXTURE,
        executed_roots: compiled.executed_roots,
    })
}

fn built_modules() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build_modules_fixture).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building {FIXTURE} (modules) natively failed:\n{message}"),
    }
}

fn same_as_jvm_boot_on(built: Option<&'static Built>, mechanism: &str) {
    let Some(built) = built else {
        return;
    };
    let (jvm, _) = outcome(
        Command::new("java")
            .args([
                "--add-exports",
                "java.base/jdk.internal.perf=ALL-UNNAMED",
                "--add-exports",
                "java.base/jdk.internal.loader=ALL-UNNAMED",
                "--add-exports",
                "java.base/jdk.internal.module=ALL-UNNAMED",
                "--add-opens",
                "java.base/jdk.internal.perf=ALL-UNNAMED",
                "--add-opens",
                "java.base/jdk.internal.loader=ALL-UNNAMED",
                "--add-opens",
                "java.base/jdk.internal.jimage=ALL-UNNAMED",
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
    let (native, stop) = native_run(built, mechanism);
    assert_eq!(
        native,
        jvm,
        "{mechanism} ({}): native differs from JVM\nlook at: {LOOK}\njabs stop: {}",
        built.fixture,
        stop.as_deref().unwrap_or("none")
    );
}

fn same_as_jvm_boot(mechanism: &str) {
    same_as_jvm_boot_on(built(), mechanism);
}

#[test]
fn classfile_parses_permitted_subclasses() {
    let Some(built) = built() else {
        return;
    };
    let shape =
        ClassFile::parse(&fs::read(built.classes.join("BootExtrasCases$Shape.class")).unwrap())
            .unwrap();
    assert_eq!(
        shape.reflect.permitted_subclasses,
        Some(vec![
            "BootExtrasCases$Circle".to_string(),
            "BootExtrasCases$Rect".to_string(),
        ])
    );
    let plain =
        ClassFile::parse(&fs::read(built.classes.join("BootExtrasCases$Plain.class")).unwrap())
            .unwrap();
    assert_eq!(plain.reflect.permitted_subclasses, None);

    // JVMS 4.7.31: PermittedSubclasses on major < 61 is ignored; on ACC_FINAL it is rejected.
    let mut old_bytes = fs::read(built.classes.join("BootExtrasCases$Shape.class")).unwrap();
    old_bytes[6] = 0;
    old_bytes[7] = 60;
    let old = ClassFile::parse(&old_bytes).unwrap();
    assert_eq!(old.reflect.permitted_subclasses, None);

    let final_with_perm = synthetic_permitted_class(0x0011, 1, &[1]);
    assert!(
        ClassFile::parse(&final_with_perm).is_err(),
        "ACC_FINAL class with PermittedSubclasses must be rejected"
    );
    let module_with_perm = synthetic_permitted_class(0x8000, 1, &[1]);
    assert!(
        ClassFile::parse(&module_with_perm).is_err(),
        "ACC_MODULE class with PermittedSubclasses must be rejected"
    );
    let empty_perm = synthetic_permitted_class(0x0001, 1, &[]);
    let parsed_empty = ClassFile::parse(&empty_perm).unwrap();
    assert_eq!(parsed_empty.reflect.permitted_subclasses, Some(vec![]));
}

fn synthetic_permitted_class(access: u16, attr_repeats: u16, entries: &[u16]) -> Vec<u8> {
    let mut out = Vec::from(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 65][..]);
    out.extend_from_slice(&6u16.to_be_bytes()); // cp_count = 6 (entries 1..=5)
    out.extend_from_slice(&[7, 0, 2]); // #1 Class "F"
    out.extend_from_slice(&[1, 0, 1, b'F']); // #2 Utf8 "F"
    out.extend_from_slice(&[7, 0, 4]); // #3 Class "java/lang/Object"
    out.extend_from_slice(&[1, 0, 16]);
    out.extend_from_slice(b"java/lang/Object"); // #4 Utf8
    let attr_name = b"PermittedSubclasses";
    out.extend_from_slice(&[1]);
    out.extend_from_slice(&(attr_name.len() as u16).to_be_bytes());
    out.extend_from_slice(attr_name); // #5 Utf8
    out.extend_from_slice(&access.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // this = #1
    out.extend_from_slice(&3u16.to_be_bytes()); // super = #3
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces = 0
    out.extend_from_slice(&0u16.to_be_bytes()); // fields = 0
    out.extend_from_slice(&0u16.to_be_bytes()); // methods = 0
    out.extend_from_slice(&attr_repeats.to_be_bytes());
    let payload_len = (2 + entries.len() * 2) as u32;
    for _ in 0..attr_repeats {
        out.extend_from_slice(&5u16.to_be_bytes()); // #5 PermittedSubclasses
        out.extend_from_slice(&payload_len.to_be_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for entry in entries {
            out.extend_from_slice(&entry.to_be_bytes());
        }
    }
    out
}

#[test]
fn perf_create_long_and_byte_array() {
    same_as_jvm_boot("perfBuffers");
}

#[test]
fn perf_null_arguments() {
    same_as_jvm_boot("perfNulls");
}

#[test]
fn perf_high_res_clock() {
    same_as_jvm_boot("perfClock");
}

#[test]
fn perf_detach_noop() {
    same_as_jvm_boot("perfDetach");
}

#[test]
fn class_get_permitted_subclasses() {
    same_as_jvm_boot("sealedClasses");
}

#[test]
fn boot_loader_native_image_and_native_library() {
    same_as_jvm_boot("bootLoaderAndLibs");
}

#[test]
fn boot_loader_packages_and_app_loader() {
    same_as_jvm_boot_on(built_modules(), "bootPackages");
}

#[test]
fn perf_validation_and_detach_null() {
    same_as_jvm_boot("perfEdgeErrors");
}

#[test]
fn classfile_rejects_duplicate_and_truncated_permitted_subclasses() {
    let base = synthetic_permitted_class(0x0011, 1, &[1]);
    // Truncate attribute length from 4 to 2 (leaving only count, fewer than declared entries).
    let mut trunc = base.clone();
    let len = trunc.len();
    trunc[len - 5] = 2; // attribute_length = 2
    trunc.truncate(len - 2);
    assert!(
        ClassFile::parse(&trunc).is_err(),
        "truncated PermittedSubclasses attribute must be rejected"
    );

    let dup = synthetic_permitted_class(0x0001, 2, &[1]);
    assert!(
        ClassFile::parse(&dup).is_err(),
        "duplicate PermittedSubclasses attribute must be rejected"
    );
}

#[test]
fn jabs_perf_frequency_and_attach_throw() {
    let Some(built) = built() else {
        return;
    };
    let (native, stop) = native_run(built, "jabsPerfExtras");
    assert_eq!(native.status, Some(0), "stop: {stop:?}");
    assert_eq!(
        native.stdout,
        "freq=1000000000\nattach0=IOException\nbadNeg2=IAE\ndetachNull=ok\n"
    );
}
