//! GraphicsEnvironment.isHeadless natively against the JVM: the java.awt.headless property both
//! ways, and the platform's answer when it is unset (on Windows the native
//! sun.awt.PlatformGraphicsInfo.hasDisplays0, `jabs_platform_has_displays` in
//! runtime/jabs_platform.c, wired in src/cgen.rs). The 1.7.10 server's snooper asks right after
//! Done.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip locally and fail when
//! `JABS_REQUIRE_TOOLS=1`.

mod common;

use common::{Built, fresh_build_dir, link_native, run, same_as_jvm, tools_ready};
use jabs::{cgen, classfile::ClassFile};
use std::{collections::BTreeSet, fs, path::Path, process::Command, sync::OnceLock};

const FIXTURE: &str = "tests/fixtures/Headless.java";
const MAIN_CLASS: &str = "Headless";
const LOOK: &str = "jabs_platform_has_displays (runtime/jabs_platform.c) and the hasDisplays0 \
                    Intrinsic (src/cgen.rs); with the property set, GraphicsEnvironment's own code";

/// Built once per test binary; a failed build fails every test with its message.
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
        Err(message) => panic!("building {FIXTURE} natively failed:\n{message}"),
    }
}

/// The fixture with java.base and java.desktop, where GraphicsEnvironment lives.
fn build() -> Option<Built> {
    if !tools_ready(FIXTURE) {
        return None;
    }
    let dir = fresh_build_dir(FIXTURE, "");
    let classes = dir.join("classes");
    fs::create_dir_all(&classes).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    run(Command::new("javac")
        .args(["-nowarn", "-d"])
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
    for module in ["java.base", "java.desktop"] {
        let (library, _, unparsable) = jabs::jdk::read_module(&home, module).unwrap();
        assert_eq!(unparsable, 0, "{module} classes ClassFile::parse rejects");
        units.extend(library);
    }
    let extras = cgen::UnitExtras {
        startup: true,
        java_home: Some(home.display().to_string()),
        ..Default::default()
    };
    let compiled = cgen::compile_unit_with(&units, Some(&roots), &extras);
    let native = link_native(&dir, &compiled.c_files, MAIN_CLASS, "fixture");
    Some(Built {
        classes,
        native,
        main_class: MAIN_CLASS,
        fixture: FIXTURE,
        executed_roots: compiled.executed_roots,
    })
}

#[test]
fn unset_property_asks_the_platform() {
    same_as_jvm(built(), "platformDefault", LOOK);
}

#[test]
fn property_true_is_headless() {
    same_as_jvm(built(), "propertyTrue", LOOK);
}

#[test]
fn property_false_is_not_headless() {
    same_as_jvm(built(), "propertyFalse", LOOK);
}
