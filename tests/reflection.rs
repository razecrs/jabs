//! Reflection, in two layers:
//! - class-file level: what `src/classfile.rs` keeps for java.lang.reflect (Signature,
//!   Exceptions, raw annotation attributes, InnerClasses, EnclosingMethod, NestHost, Record);
//! - behavior: tests/fixtures/Reflect.java is compiled once into a native program (with
//!   java.base, as in production), and each test below runs one mechanism
//!   (`ReflectCases <mechanism>`) on the reference JVM and natively and compares stdout, exit
//!   status and the class of an uncaught exception. A failure names the mechanism, the first
//!   differing line and where to look.
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

mod common;

use common::{Built, built_once};
use jabs::classfile::{AnnotationAttribute, ClassFile, annotation_pool_indices};
use std::{fs, path::Path, process::Command, sync::OnceLock};

const SOURCE: &str = r#"
import java.lang.annotation.*;
import java.util.List;

@Retention(RetentionPolicy.RUNTIME) @interface Tag { String value() default "dflt"; int n() default 7; }
@Retention(RetentionPolicy.CLASS) @interface Invisible {}

@Tag("outer")
public class Meta<T extends Number> {
    @Tag(value = "field", n = 3) public List<T> items;
    @Invisible int hidden;
    public <E extends Exception> T pick(@Tag("p0") T a, List<? super T> b) throws E, java.io.IOException { return a; }
    static class Nested {}
    Runnable local() { class Local implements Runnable { public void run() {} } return new Local(); }
}
"#;

/// Compile SOURCE with javac (release 8 class files, like Minecraft 1.7.10's), or None when
/// javac is missing and tools are optional.
fn compile(dir: &Path) -> Option<()> {
    let source = dir.join("Meta.java");
    fs::write(&source, SOURCE).unwrap();
    let status = Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(dir)
        .arg(&source)
        .status();
    let Ok(status) = status else {
        assert!(
            std::env::var_os("JABS_REQUIRE_TOOLS").is_none(),
            "javac is required (JABS_REQUIRE_TOOLS is set)"
        );
        eprintln!("javac unavailable; skipping");
        return None;
    };
    assert!(status.success(), "javac failed on the reflection fixture");
    Some(())
}

fn parse(dir: &Path, name: &str) -> ClassFile {
    let bytes = fs::read(dir.join(format!("{name}.class"))).unwrap();
    ClassFile::parse(&bytes)
        .unwrap_or_else(|e| panic!("ClassFile::parse({name}) in src/classfile.rs: {e}"))
}

#[test]
fn classfile_keeps_generic_signatures_and_exceptions() {
    let dir = tempfile::tempdir().unwrap();
    if compile(dir.path()).is_none() {
        return;
    }
    let meta = parse(dir.path(), "Meta");
    assert_eq!(
        meta.reflect.signature.as_deref(),
        Some("<T:Ljava/lang/Number;>Ljava/lang/Object;"),
        "class Signature attribute (ClassFile::parse, class attributes, src/classfile.rs)"
    );
    let pick = meta.methods.iter().find(|m| m.name == "pick").unwrap();
    assert_eq!(
        pick.reflect.signature.as_deref(),
        Some("<E:Ljava/lang/Exception;>(TT;Ljava/util/List<-TT;>;)TT;^TE;^Ljava/io/IOException;"),
        "method Signature attribute (member attributes in ClassFile::parse)"
    );
    // The Exceptions attribute names the erased type variable's bound, in source order.
    assert_eq!(
        pick.reflect.exceptions,
        ["java/lang/Exception", "java/io/IOException"],
        "Exceptions attribute (parse_exceptions in src/classfile.rs)"
    );
    let items = meta.fields.iter().find(|f| f.name == "items").unwrap();
    assert_eq!(
        items.reflect.signature.as_deref(),
        Some("Ljava/util/List<TT;>;"),
        "field Signature attribute"
    );
}

#[test]
fn classfile_keeps_only_runtime_visible_annotations_raw() {
    let dir = tempfile::tempdir().unwrap();
    if compile(dir.path()).is_none() {
        return;
    }
    let meta = parse(dir.path(), "Meta");
    let raw = meta
        .reflect
        .annotations
        .as_ref()
        .expect("class RuntimeVisibleAnnotations kept raw (ClassFile::parse class attributes)");
    // u2 count = 1, then the annotation's type index, one element pair.
    assert_eq!(&raw[..2], &[0, 1], "one class annotation");
    let indices = annotation_pool_indices(raw, AnnotationAttribute::Annotations).unwrap();
    assert_eq!(
        indices.len(),
        3,
        "type, element name and string value indices (walk_annotation in src/classfile.rs)"
    );
    assert_eq!(meta.utf8(indices[0]).unwrap(), "LTag;");
    assert_eq!(meta.utf8(indices[2]).unwrap(), "outer");
    let hidden = meta.fields.iter().find(|f| f.name == "hidden").unwrap();
    assert!(
        hidden.reflect.annotations.is_none(),
        "a CLASS-retention annotation is RuntimeInvisible and must not be reported"
    );
    let pick = meta.methods.iter().find(|m| m.name == "pick").unwrap();
    let params = pick.reflect.parameter_annotations.as_ref().expect(
        "RuntimeVisibleParameterAnnotations kept raw (member attributes in ClassFile::parse)",
    );
    assert_eq!(params[0], 2, "one entry per parameter");
    let tag = parse(dir.path(), "Tag");
    let value = tag.methods.iter().find(|m| m.name == "value").unwrap();
    let default = value
        .reflect
        .annotation_default
        .as_ref()
        .expect("AnnotationDefault kept raw");
    assert_eq!(default[0], b's', "a String default is a 's' element_value");
    let n = tag.methods.iter().find(|m| m.name == "n").unwrap();
    let indices = annotation_pool_indices(
        n.reflect.annotation_default.as_ref().unwrap(),
        AnnotationAttribute::Default,
    )
    .unwrap();
    assert_eq!(
        indices.len(),
        1,
        "an int default names one Integer constant"
    );
}

#[test]
fn classfile_keeps_inner_class_and_enclosing_method_entries() {
    let dir = tempfile::tempdir().unwrap();
    if compile(dir.path()).is_none() {
        return;
    }
    let meta = parse(dir.path(), "Meta");
    let nested = meta
        .reflect
        .inner_classes
        .iter()
        .find(|i| i.inner == "Meta$Nested")
        .expect("InnerClasses entry for Meta$Nested (parse_inner_classes in src/classfile.rs)");
    assert_eq!(nested.outer.as_deref(), Some("Meta"));
    assert_eq!(nested.simple_name.as_deref(), Some("Nested"));
    assert_eq!(nested.access, 0x0008, "static (inner_class_access_flags)");
    let local = parse(dir.path(), "Meta$1Local");
    assert_eq!(
        local.reflect.enclosing_method,
        Some((
            "Meta".to_string(),
            Some(("local".to_string(), "()Ljava/lang/Runnable;".to_string()))
        )),
        "EnclosingMethod (parse_enclosing_method in src/classfile.rs)"
    );
    let own = local
        .reflect
        .inner_classes
        .iter()
        .find(|i| i.inner == "Meta$1Local")
        .unwrap();
    assert_eq!(own.outer, None, "a local class has no outer class");
    assert_eq!(own.simple_name.as_deref(), Some("Local"));
    assert_eq!(meta.reflect.nest_host, None, "release 8 has no NestHost");
    assert!(!meta.reflect.is_record);
}

// ---- Behavior against the JVM ----

fn built() -> Option<&'static Built> {
    static BUILT: OnceLock<Result<Option<Built>, String>> = OnceLock::new();
    built_once(&BUILT, "tests/fixtures/Reflect.java", "ReflectCases")
}

/// Run `ReflectCases <mechanism>` on the JVM and natively and require identical outcomes
/// (tests/common/mod.rs). `look` says where the mechanism lives, for the failure message.
fn same_as_jvm(mechanism: &str, look: &str) {
    common::same_as_jvm(built(), mechanism, look);
}

const LISTINGS: &str = "declared_executables/jabs_reflect_declared_fields/new_method/new_field in \
                        runtime/jabs_reflect.c; the metadata rows in functions() in src/cgen/reflect.rs";

#[test]
fn declared_fields_report_modifiers_and_types() {
    same_as_jvm("declared_fields_report_modifiers_and_types", LISTINGS);
}

#[test]
fn public_members_include_inherited() {
    same_as_jvm(
        "public_members_include_inherited",
        "the public_only filter in runtime/jabs_reflect.c (listed), jabs_reflect_interfaces \
         (Class.getMethods walks superinterfaces)",
    );
}

#[test]
fn class_attributes_from_the_class_file() {
    same_as_jvm(
        "class_attributes_from_the_class_file",
        "jabs_reflect_interfaces/declared_classes/declaring_class/enclosing_method/\
         simple_binary_name in runtime/jabs_reflect.c; jabs_rinners rows in src/cgen/reflect.rs",
    );
}

#[test]
fn class_modifiers_come_from_inner_classes() {
    same_as_jvm(
        "class_modifiers_come_from_inner_classes",
        "class_modifiers in src/cgen/reflect.rs (HotSpot's compute_modifier_flags), used by \
         jabs_class_object through jabs_reflect_class_modifiers",
    );
}

#[test]
fn declared_methods_exclude_inherited() {
    same_as_jvm("declared_methods_exclude_inherited", LISTINGS);
}

#[test]
fn declared_constructors_report_every_visibility() {
    same_as_jvm("declared_constructors_report_every_visibility", LISTINGS);
}

#[test]
fn get_method_matches_parameter_types() {
    same_as_jvm(
        "get_method_matches_parameter_types",
        "parameter_classes/descriptor_class in runtime/jabs_reflect.c (Class.getMethod compares \
         the Class objects they make)",
    );
}

#[test]
fn get_method_rejects_what_it_cannot_see() {
    same_as_jvm(
        "get_method_rejects_what_it_cannot_see",
        "the public_only filter (listed) in runtime/jabs_reflect.c",
    );
}

#[test]
fn generic_signatures_reach_types() {
    same_as_jvm(
        "generic_signatures_reach_types",
        "Signature/Exceptions rows (functions() in src/cgen/reflect.rs), new_method's signature \
         and exceptionTypes, jabs_reflect_generic_signature in runtime/jabs_reflect.c",
    );
}

#[test]
fn invoke_calls_static_instance_and_private_methods() {
    same_as_jvm(
        "invoke_calls_static_instance_and_private_methods",
        "jabs_reflect_invoke/convert_arguments/widen/box_result in runtime/jabs_reflect.c; the code pointers and jabs_rcallers in functions() in src/cgen/reflect.rs",
    );
}

#[test]
fn invoke_dispatches_virtually_and_through_interfaces() {
    same_as_jvm(
        "invoke_dispatches_virtually_and_through_interfaces",
        "the vtable_slot/selector rows in functions() in src/cgen/reflect.rs; dispatch in jabs_reflect_invoke (runtime/jabs_reflect.c)",
    );
}

#[test]
fn invoke_unboxes_and_widens_like_the_jvm() {
    same_as_jvm(
        "invoke_unboxes_and_widens_like_the_jvm",
        "convert_argument/widen/box_value/box_result in runtime/jabs_reflect.c",
    );
}

#[test]
fn invoke_selects_overloads_and_varargs() {
    same_as_jvm(
        "invoke_selects_overloads_and_varargs",
        "jabs_reflect_invoke/convert_arguments/widen/box_result in runtime/jabs_reflect.c",
    );
}

#[test]
fn constructor_new_instance_runs_the_constructor() {
    same_as_jvm(
        "constructor_new_instance_runs_the_constructor",
        "jabs_reflect_new_instance in runtime/jabs_reflect.c",
    );
}

#[test]
fn invoke_rejects_bad_arguments_like_the_jvm() {
    same_as_jvm(
        "invoke_rejects_bad_arguments_like_the_jvm",
        "convert_arguments (message and order) in runtime/jabs_reflect.c",
    );
}

#[test]
fn invoke_checks_the_receiver() {
    same_as_jvm(
        "invoke_checks_the_receiver",
        "the receiver checks in jabs_reflect_invoke (runtime/jabs_reflect.c)",
    );
}

#[test]
fn invoke_wraps_thrown_exceptions() {
    same_as_jvm(
        "invoke_wraps_thrown_exceptions",
        "wrap_pending_in_ite in runtime/jabs_reflect.c",
    );
}

#[test]
fn access_checks_stay_in_java() {
    same_as_jvm(
        "access_checks_stay_in_java",
        "Reflection.verifyMemberAccess runs in Java; jabs_reflect_nest_mates and the Method modifiers in runtime/jabs_reflect.c",
    );
}

#[test]
fn field_get_and_set_convert_like_the_jvm() {
    same_as_jvm(
        "field_get_and_set_convert_like_the_jvm",
        "the accessor leaves in replacement() (src/cgen/reflect.rs), jabs_reflect_field_slot and the field rows' offset/address",
    );
}

#[test]
fn field_access_rejects_what_the_jvm_rejects() {
    same_as_jvm(
        "field_access_rejects_what_the_jvm_rejects",
        "jabs_reflect_field_refuse_set/jabs_reflect_field_set_object/jabs_reflect_field_slot in runtime/jabs_reflect.c",
    );
}

#[test]
fn enum_constants_through_reflection() {
    same_as_jvm(
        "enum_constants_through_reflection",
        "enum values() kept by reach() in src/cgen/reflect.rs; jabs_reflect_invoke for the static values() call in runtime/jabs_reflect.c (Class.getEnumConstantsShared calls it)",
    );
}

#[test]
fn absent_annotations_read_as_empty() {
    same_as_jvm(
        "absent_annotations_read_as_empty",
        "the annotations blob offsets (0 = absent) in functions() in src/cgen/reflect.rs; blob_array in runtime/jabs_reflect.c",
    );
}

#[test]
fn gson_shaped_round_trip() {
    same_as_jvm(
        "gson_shaped_round_trip",
        "jabs_reflect_new_instance, the field accessor leaves and class_modifiers (Gson skips non-static member classes) in src/cgen/reflect.rs and runtime/jabs_reflect.c",
    );
}
