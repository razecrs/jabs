//! java.util.zip: tests/fixtures/Zip.java is compiled once into a native program (with
//! java.base, as in production), and each test runs one mechanism (`ZipCases <mechanism>`) on
//! the reference JVM and natively and compares stdout, exit status and an escaping exception
//! (tests/common/mod.rs). `built_once` also checks that every method in the fixture compiles
//! cleanly with zero unsupported methods.
//!
//! tests/fixtures/ZipBuffers.java is compiled separately with `--release 21` to exercise the
//! JDK 11+ `Deflater` / `Inflater` `ByteBuffer` overloads (`ZipBufferCases directByteBuffers`)
//! while keeping `Zip.java` on `--release 8`.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

mod common;

use common::{Built, build_native, clang_target_args, run, same_as_jvm, tool_available};
use jabs::{cgen, classfile::ClassFile};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const FIXTURE: &str = "tests/fixtures/Zip.java";
const BUFFERS_FIXTURE: &str = "tests/fixtures/ZipBuffers.java";

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(|| {
            let home = jabs::jdk::java_home(Path::new("java")).unwrap();
            let extras = cgen::UnitExtras {
                startup: true,
                java_home: Some(home.display().to_string()),
                ..Default::default()
            };
            build_native(FIXTURE, "ZipCases", "", None, extras)
        })
        .map_err(|e| {
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

fn built_buffers() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(build_buffers).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building {BUFFERS_FIXTURE} natively failed:\n{message}"),
    }
}

/// Build tests/fixtures/ZipBuffers.java with `--release 21` (common::build_native hardcodes
/// `--release 8`) using the same clang/harness conventions as tests/common/mod.rs.
fn build_buffers() -> Option<Built> {
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
        eprintln!("skipping {BUFFERS_FIXTURE} behavior tests; missing {missing:?}");
        return None;
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in env!("CARGO_MANIFEST_DIR").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let dir = std::env::temp_dir()
        .join("jabs-behavior")
        .join(format!("{hash:016x}-ZipBuffers"));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(BUFFERS_FIXTURE);
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
            init = cgen::init_symbol("ZipBufferCases"),
            main = cgen::symbol("ZipBufferCases", "main", "([Ljava/lang/String;)V"),
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
        main_class: "ZipBufferCases",
        fixture: BUFFERS_FIXTURE,
        executed_roots: compiled.executed_roots,
    })
}

#[test]
fn adler32_updates_and_bounds() {
    same_as_jvm(
        built(),
        "adler32",
        "java/util/zip/Adler32 natives (update, updateBytes, updateByteBuffer) in \
         src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn deflater_inflater_zlib_round_trip() {
    same_as_jvm(
        built(),
        "deflaterInflaterZlib",
        "java/util/zip/Deflater and Inflater zlib mode (nowrap=false), small-slice streaming, \
         levels/strategies, getBytesRead/Written, getAdler, and reset in src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn deflater_inflater_raw_round_trip() {
    same_as_jvm(
        built(),
        "deflaterInflaterRaw",
        "java/util/zip/Deflater(6, true) and Inflater(true) raw DEFLATE mode (nowrap=true) in \
         src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn deflater_flushes_mid_stream() {
    same_as_jvm(
        built(),
        "deflaterFlushes",
        "Deflater NO_FLUSH, SYNC_FLUSH, and FULL_FLUSH handling in deflateBytesBytes / \
         runtime/jabs_zip.c",
    );
}

#[test]
fn deflater_params_change_mid_stream() {
    same_as_jvm(
        built(),
        "deflaterParamsChange",
        "Deflater.setLevel / setStrategy mid-stream parameter updates (deflateParams) in \
         src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn dictionary_zlib_and_raw() {
    same_as_jvm(
        built(),
        "dictionary",
        "Deflater.setDictionary and Inflater.needsDictionary / setDictionary / getAdler in zlib \
         and raw modes in src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn direct_buffers_crc32_and_adler32() {
    same_as_jvm(
        built(),
        "directBuffers",
        "CRC32.updateByteBuffer0 (src/cgen.rs, jabs_crc32 in runtime/jabs_rt.h) and \
         Adler32.updateByteBuffer on direct slices and read-only buffers",
    );
}

#[test]
fn gzip_stream_round_trip() {
    same_as_jvm(
        built(),
        "gzipRoundTrip",
        "GZIPOutputStream / GZIPInputStream over Deflater / Inflater and CRC32 in src/cgen and \
         runtime/jabs_zip.c",
    );
}

#[test]
fn zip_archive_round_trip() {
    same_as_jvm(
        built(),
        "zipArchiveRoundTrip",
        "ZipOutputStream / ZipInputStream DEFLATED and STORED entries over Deflater / Inflater \
         and CRC32 in src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn corrupt_and_truncated_input() {
    same_as_jvm(
        built(),
        "corruptInput",
        "Inflater DataFormatException handling, getBytesRead/Written, getRemaining, getAdler, \
         and finished on corrupt or truncated streams in src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn closed_state_and_double_end() {
    same_as_jvm(
        built(),
        "closedState",
        "Deflater/Inflater idempotent end() and post-end IllegalStateException checks, plus \
         stream wrapper double close() in src/cgen and runtime/jabs_zip.c",
    );
}

#[test]
fn direct_byte_buffers_inflater_deflater() {
    same_as_jvm(
        built_buffers(),
        "directByteBuffers",
        "Deflater and Inflater ByteBuffer overloads (deflateBytesBuffer, deflateBufferBytes, \
         deflateBufferBuffer, inflateBytesBuffer, inflateBufferBytes, inflateBufferBuffer, \
         setDictionary) in src/cgen/zip.rs and runtime/jabs_zip.c",
    );
}
