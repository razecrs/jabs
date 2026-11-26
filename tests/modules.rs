//! Module encapsulation: tests/fixtures/Modules.java is compiled once with java.base and the
//! JDK's module declarations (`jdk::modules`, as `jabs compile --jdk` passes them), and each test
//! runs one mechanism (`ModulesCases <mechanism>`) on the reference JVM and natively; stdout,
//! exit status and an escaping exception must agree. A failure names the mechanism, the first
//! differing line and where to look.
//!
//! The Module objects come from runtime/jabs_modules.c (built on first Class.getModule of a JDK
//! class) with the tables and glue of src/cgen/modules.rs; Class.getModule itself is
//! jabs_class_module (module_function in src/cgen.rs). The access decisions are the JDK's own
//! Java code (AccessibleObject.checkCanSetAccessible, Reflection.verifyModuleAccess), so a
//! difference usually means a Module field (reads, exportedPackages, openPackages, descriptor)
//! is not what Module.defineModules would have set.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip, unless JABS_REQUIRE_TOOLS=1.

mod common;

use common::{Built, build_native_release, same_as_jvm};
use std::{path::Path, sync::OnceLock};

const FIXTURE: &str = "tests/fixtures/Modules.java";
const MAIN: &str = "ModulesCases";
const TABLES: &str = "the module tables and glue (src/cgen/modules.rs)";
const BUILD: &str = "the module build (runtime/jabs_modules.c)";

static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();

fn built() -> Option<&'static Built> {
    let result = BUILT.get_or_init(|| {
        std::panic::catch_unwind(|| {
            if !common::tools_ready(FIXTURE) {
                return None;
            }
            let home = jabs::jdk::java_home(Path::new("java")).unwrap();
            let extras = jabs::cgen::UnitExtras {
                startup: true,
                java_home: Some(home.display().to_string()),
                // invokeNotExported loads jdk.internal.misc.VM by name, as on the JVM.
                loaded_classes: ["jdk/internal/misc/VM".to_string()].into_iter().collect(),
                jdk_modules: jabs::jdk::modules(&home).unwrap(),
                ..Default::default()
            };
            build_native_release(FIXTURE, MAIN, "", None, extras, "21")
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

#[test]
fn set_accessible_on_a_jdk_private_field_throws() {
    same_as_jvm(
        built(),
        "setAccessibleJdkInternal",
        &format!(
            "AccessibleObject.checkCanSetAccessible asks Module.isOpen(\"java.nio\", caller): java.base's openPackages ({BUILD}), the caller's module (jabs_class_module, src/cgen.rs)"
        ),
    );
}

#[test]
fn try_set_accessible_follows_exports_and_opens() {
    same_as_jvm(
        built(),
        "trySetAccessible",
        &format!(
            "Module.isExported / isOpen for java.lang and java.nio: exportedPackages and openPackages set by Module.initExports ({BUILD})"
        ),
    );
}

#[test]
fn own_private_field_is_accessible() {
    same_as_jvm(
        built(),
        "ownPrivateField",
        "the program's own classes are in an unnamed module (jabs_class_module, src/cgen.rs)",
    );
}

#[test]
fn module_queries_match() {
    same_as_jvm(
        built(),
        "moduleQueries",
        &format!(
            "which module a class gets (jabs_class_module, src/cgen.rs; the package table of {TABLES}) and Module.reads ({BUILD})"
        ),
    );
}

#[test]
fn descriptor_is_java_base_module_info() {
    same_as_jvm(
        built(),
        "descriptor",
        &format!(
            "the embedded module-info bytes ({TABLES}; jdk::modules in src/jdk.rs) and ModuleDescriptor.read"
        ),
    );
}

#[test]
fn netty_probe_takes_the_jvm_branch() {
    same_as_jvm(
        built(),
        "nettyProbe",
        &format!("the setAccessible refusal Netty's PlatformDependent0 relies on: {BUILD}"),
    );
}

#[test]
fn invoke_of_a_non_exported_class_is_refused() {
    same_as_jvm(
        built(),
        "invokeNotExported",
        &format!(
            "Reflection.verifyModuleAccess asks Module.isExported(\"jdk.internal.misc\", caller): java.base's exportedPackages ({BUILD})"
        ),
    );
}

/// Every JDK module's declaration parses, and java.base's has the shape JDK 25 gives it; a
/// failure here is in the Module attribute parsing (src/classfile.rs) or jdk::modules
/// (src/jdk.rs), before any native code is involved.
#[test]
fn jdk_module_declarations_parse() {
    if !common::tool_available("java", "-version") {
        assert!(
            std::env::var_os("JABS_REQUIRE_TOOLS").is_none(),
            "java is required"
        );
        return;
    }
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let modules = jabs::jdk::modules(&home).unwrap();
    let base = modules
        .iter()
        .find(|m| m.declaration.name == "java.base")
        .expect("java.base among the JDK's modules (jdk::modules, src/jdk.rs)");
    assert!(
        base.declaration.requires.is_empty(),
        "java.base requires nothing"
    );
    assert!(
        base.packages.iter().any(|p| p == "java/lang"),
        "ModulePackages of java.base lists java/lang (parse_module_packages, src/classfile.rs)"
    );
    let lang = base
        .declaration
        .exports
        .iter()
        .find(|e| e.package == "java/lang")
        .expect("java.base exports java/lang (parse_module, src/classfile.rs)");
    assert!(lang.targets.is_empty(), "java/lang is exported to everyone");
    let misc = base
        .declaration
        .exports
        .iter()
        .find(|e| e.package == "jdk/internal/misc")
        .expect("java.base exports jdk/internal/misc to some modules");
    assert!(
        misc.targets.iter().any(|t| t == "jdk.unsupported"),
        "jdk/internal/misc is exported to jdk.unsupported (sun.misc.Unsafe's module)"
    );
    // Every requires names a module of this JDK, so readability is computed from complete data.
    for module in &modules {
        for requires in &module.declaration.requires {
            assert!(
                modules
                    .iter()
                    .any(|m| m.declaration.name == requires.module),
                "{} requires {}, which this JDK lacks",
                module.declaration.name,
                requires.module
            );
        }
    }
}
