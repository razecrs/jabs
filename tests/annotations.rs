//! Annotations and java.lang.reflect.Proxy. The JDK makes every annotation instance with
//! Proxy.newProxyInstance; JABS generates the proxy classes at build time (src/proxy.rs) and
//! finds them at run time (Proxy.getProxyConstructor and ProxyBuilder.isProxyClass, replaced in
//! src/cgen/reflect.rs, answered by runtime/jabs_reflect.c). Two layers:
//! - build level: which interface lists src/proxy.rs decides, and what the generated class
//!   looks like;
//! - behavior: tests/fixtures/Annotations.java is compiled once into a native program (with
//!   java.base, as in production), and each test runs one mechanism
//!   (`AnnotationCases <mechanism>`) on the reference JVM and natively and compares the
//!   outcomes (tests/common/mod.rs).
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

mod common;

use common::{Built, built_once, native_run, same_as_jvm, tool_available};
use jabs::classfile::ClassFile;
use std::{fs, path::Path, process::Command, sync::OnceLock};

const FIXTURE: &str = "tests/fixtures/Annotations.java";

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    built_once(&BUILT, FIXTURE, "AnnotationCases")
}

// ---- Build level ----

/// The fixture's classes, compiled by javac, with java.base (every annotation interface
/// extends java.lang.annotation.Annotation), or None when javac is missing and optional.
fn fixture_classes() -> Option<Vec<ClassFile>> {
    if !tool_available("javac", "-version") {
        assert!(
            std::env::var_os("JABS_REQUIRE_TOOLS").is_none(),
            "javac is required (JABS_REQUIRE_TOOLS is set)"
        );
        eprintln!("javac unavailable; skipping");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let status = Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(dir.path())
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success(), "javac failed on {FIXTURE}");
    let mut classes: Vec<ClassFile> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "class"))
        .map(|p| ClassFile::parse(&fs::read(p).unwrap()).unwrap())
        .collect();
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let (library, _, _) = jabs::jdk::read_module(&home, "java.base").unwrap();
    classes.extend(library);
    Some(classes)
}

#[test]
fn build_decides_annotation_and_constant_interface_lists() {
    let Some(mut classes) = fixture_classes() else {
        return;
    };
    let report = jabs::proxy::generate(&mut classes);
    let lists: Vec<Vec<String>> = report
        .proxies
        .iter()
        .map(|p| p.interfaces.clone())
        .collect();
    let has = |names: &[&str]| lists.iter().any(|l| l == names);
    // Annotations on classes, members, parameters, nested in values and in defaults.
    for annotation in [
        "AnnEvery",
        "AnnDefaults",
        "AnnInner",
        "AnnMarker",
        "AnnTag",
        "AnnPlugin",
        "AnnPluginFactory",
        "AnnPluginAttribute",
        "AnnInheritable",
    ] {
        assert!(
            has(&[annotation]),
            "no proxy for annotation {annotation} (annotation_interfaces in src/proxy.rs); lists: {lists:?}"
        );
    }
    assert!(
        has(&["AnnGreeter", "AnnCounter"]),
        "no proxy for the constant Class[] at the Proxy.newProxyInstance call (literal_lists in src/proxy.rs); lists: {lists:?}"
    );
    assert!(
        !has(&["java/lang/Runnable", "java/lang/Comparable"]),
        "a list built at run time cannot be known at build time"
    );
    let refused: Vec<_> = report
        .skipped
        .iter()
        .filter(|(list, _)| list.iter().any(|i| i.starts_with("Ann")))
        .collect();
    assert!(
        refused.is_empty(),
        "fixture lists src/proxy.rs refused (proxy_methods/catch_list): {refused:?}"
    );
    for proxy in &report.proxies {
        assert!(
            classes.iter().any(|c| c.name == proxy.name),
            "generate() must append the class it reports: {}",
            proxy.name
        );
    }
}

#[test]
fn generated_proxy_class_has_proxy_generators_shape() {
    let Some(mut classes) = fixture_classes() else {
        return;
    };
    let report = jabs::proxy::generate(&mut classes);
    let tag = report
        .proxies
        .iter()
        .find(|p| p.interfaces == ["AnnTag"])
        .expect("proxy for AnnTag");
    let class = classes.iter().find(|c| c.name == tag.name).unwrap();
    // AnnTag is package-private: its proxy lives in its package (the default one) and is final.
    assert!(tag.name.starts_with("$Proxy"), "name {}", tag.name);
    assert_eq!(
        class.access & 0x0011,
        0x0010,
        "final, not public (proxy_package)"
    );
    assert_eq!(class.super_name.as_deref(), Some("java/lang/reflect/Proxy"));
    let methods: Vec<(&str, &str, u16)> = class
        .methods
        .iter()
        .map(|m| (m.name.as_str(), m.descriptor.as_str(), m.access))
        .collect();
    // The constructor, then hashCode, equals, toString from Object first, then value() and
    // annotationType() (from java.lang.annotation.Annotation), then <clinit>.
    assert_eq!(
        methods[0],
        ("<init>", "(Ljava/lang/reflect/InvocationHandler;)V", 0x0001)
    );
    assert_eq!(methods[1], ("hashCode", "()I", 0x0011));
    assert_eq!(methods[2], ("equals", "(Ljava/lang/Object;)Z", 0x0011));
    assert_eq!(methods[3], ("toString", "()Ljava/lang/String;", 0x0011));
    assert!(
        methods.contains(&("value", "()Ljava/lang/String;", 0x0011)),
        "the annotation's own element (proxy_methods in src/proxy.rs): {methods:?}"
    );
    assert!(
        methods.contains(&("annotationType", "()Ljava/lang/Class;", 0x0011)),
        "the superinterface's method (proxy_methods in src/proxy.rs): {methods:?}"
    );
    assert_eq!(
        methods.len(),
        7,
        "Annotation's own equals/hashCode/toString are Object's: {methods:?}"
    );
    assert_eq!(methods.last().unwrap().0, "<clinit>");
    // One Method field per proxied method.
    assert_eq!(class.fields.len(), methods.len() - 2);
    // Every proxied method catches Error and RuntimeException, then wraps the rest.
    let hash = class.methods.iter().find(|m| m.name == "hashCode").unwrap();
    let handlers = &hash.code.as_ref().unwrap().handlers;
    assert_eq!(handlers.len(), 3, "catch_list in src/proxy.rs");
    assert_eq!(
        handlers[2].catch_type, 0,
        "the last handler catches anything"
    );
}

// ---- Behavior against the JVM ----

const ANNOTATIONS: &str = "AnnotationParser runs in Java; the proxy classes (proxy_class in src/proxy.rs), \
                           jabs_reflect_proxy_constructor in runtime/jabs_reflect.c, and the annotation \
                           types reach() records in src/cgen/reflect.rs";

#[test]
fn get_annotation_reads_every_member_kind() {
    same_as_jvm(
        built(),
        "get_annotation_reads_every_member_kind",
        &format!(
            "{ANNOTATIONS}; unboxing in proxy_method (src/proxy.rs); the ClassValue/EnumType records added by reach()"
        ),
    );
}

#[test]
fn defaults_fill_missing_members() {
    same_as_jvm(
        built(),
        "defaults_fill_missing_members",
        &format!("{ANNOTATIONS}; AnnotationDefault blobs (functions() in src/cgen/reflect.rs)"),
    );
}

#[test]
fn annotation_identity_methods_match_the_jvm() {
    same_as_jvm(
        built(),
        "annotation_identity_methods_match_the_jvm",
        &format!(
            "{ANNOTATIONS}; the m0..m2 Object methods and <clinit> (static_initializer in src/proxy.rs); \
             jabs_reflect_is_proxy_class for equals with a hand-made annotation"
        ),
    );
}

#[test]
fn declared_annotations_on_every_element() {
    same_as_jvm(
        built(),
        "declared_annotations_on_every_element",
        &format!(
            "{ANNOTATIONS}; the field/method/parameter blobs in functions() in src/cgen/reflect.rs"
        ),
    );
}

#[test]
fn inherited_annotations_come_from_the_superclass() {
    same_as_jvm(
        built(),
        "inherited_annotations_come_from_the_superclass",
        &format!(
            "{ANNOTATIONS}; the @Inherited meta-annotation must be recorded (reach() in src/cgen/reflect.rs)"
        ),
    );
}

#[test]
fn proxy_class_and_invocation_handler() {
    same_as_jvm(
        built(),
        "proxy_class_and_invocation_handler",
        "jabs_reflect_is_proxy_class (runtime/jabs_reflect.c) behind ProxyBuilder.isProxyClass; \
         proxy_package (modifiers) and the Proxy.h field in src/proxy.rs",
    );
}

#[test]
fn user_proxy_over_a_constant_interface_list() {
    same_as_jvm(
        built(),
        "user_proxy_over_a_constant_interface_list",
        "literal_lists (which lists), proxy_method (boxing, unboxing, catch_list and the \
         UndeclaredThrowableException wrapping) in src/proxy.rs; the constructor cache in \
         jabs_reflect_proxy_constructor (runtime/jabs_reflect.c)",
    );
}

#[test]
fn log4j_shaped_plugin_lookup() {
    same_as_jvm(
        built(),
        "log4j_shaped_plugin_lookup",
        &format!("{ANNOTATIONS}; parameter annotations and Method.invoke of the factory"),
    );
}

#[test]
fn interface_list_not_generated_at_build_time_stops_naming_it() {
    let Some(built) = built() else { return };
    let (native, stop) = native_run(built, "interface_list_not_generated_at_build_time");
    let stop = stop.unwrap_or_else(|| {
        panic!(
            "a proxy for a list built at run time must stop with a jabs: message \
             (proxy_not_generated in runtime/jabs_reflect.c); got {native:?}"
        )
    });
    assert!(
        stop.contains(
            "java.lang.reflect.Proxy for interfaces [java.lang.Runnable, java.lang.Comparable]"
        ),
        "the stop must name the interfaces in order (proxy_not_generated in runtime/jabs_reflect.c): {stop}"
    );
    assert_ne!(native.status, Some(0), "a stop is not a normal exit");
    assert!(
        !native.stdout.contains("made"),
        "nothing may run past the stop"
    );
}
