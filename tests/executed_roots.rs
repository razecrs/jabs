//! Executed methods as reachability roots (`UnitExtras::executed_methods`, `jabs compile
//! --executed-methods`): a method that code reaches only by a name computed at run time is
//! compiled when a JVM run's method trace (tools/method-trace) shows it ran.
//!
//! - unit level: what the trace names outside the unit, missing from its class, abstract or not
//!   compilable is set aside and counted (`executed_roots` in src/cgen.rs); the rest are roots.
//! - end to end: tests/fixtures/ExecutedRoots.java calls a factory through Method.invoke by a
//!   computed name. Built natively with its JVM trace it must match the JVM; built without, it
//!   must stop with the not-compiled diagnostic for that method, which shows the trace is what
//!   kept it. Refusing an incomplete trace is tested in tests/trace.rs.

mod common;

use common::{build_agent, build_native, native_run, run, same_as_jvm, tool_available};
use jabs::{
    cgen::{self, ExecutedRoots},
    classfile::ClassFile,
};
use std::{collections::BTreeSet, path::Path, process::Command};

const FIXTURE: &str = "tests/fixtures/ExecutedRoots.java";
const MAIN: &str = "ExecutedRootsMain";
const FACTORY: &str = "ExecutedRootsFactory";
const MAKE: (&str, &str) = ("make", "(Ljava/lang/String;)Ljava/lang/String;");

fn key(class: &str, name: &str, descriptor: &str) -> cgen::MethodKey {
    (class.to_string(), name.to_string(), descriptor.to_string())
}

/// Tools the tests need, or None (after asserting they are not required) to skip.
fn tools_present(what: &str) -> Option<()> {
    let missing: Vec<_> = [
        ("javac", "-version"),
        ("java", "-version"),
        ("clang", "--version"),
    ]
    .into_iter()
    .filter(|(t, a)| !tool_available(t, a))
    .map(|(t, _)| t)
    .collect();
    if missing.is_empty() {
        return Some(());
    }
    assert!(
        std::env::var_os("JABS_REQUIRE_TOOLS").is_none(),
        "required tools missing: {missing:?}"
    );
    eprintln!("skipping {what}; missing {missing:?}");
    None
}

#[test]
fn executed_methods_outside_the_unit_missing_abstract_or_uncompilable_are_counted_not_rooted() {
    if tools_present("the executed-roots unit test").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    run(Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(dir.path())
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)));
    let classes: Vec<ClassFile> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "class"))
        .map(|p| ClassFile::parse(&std::fs::read(p).unwrap()).unwrap())
        .collect();
    // The fixture alone, without java.base: `twice` needs nothing else, so it compiles.
    let twice = key(FACTORY, "twice", "(I)I");
    let executed: BTreeSet<cgen::MethodKey> = [
        twice.clone(),
        key("not/in/the/Unit", "run", "()V"),
        // A hidden class (read_method_trace sets these aside; given directly, no class matches).
        key("ExecutedRootsMain$$Lambda.0x0000000801001234", "run", "()V"),
        key(FACTORY, "absent", "()V"),
        key("ExecutedRootsShape", "sides", "()I"),
        key(FACTORY, "unimplemented", "()V"),
    ]
    .into_iter()
    .collect();
    // No root classes: whatever is emitted comes from the executed methods.
    let roots = BTreeSet::new();
    let symbol = cgen::symbol(&twice.0, &twice.1, &twice.2);
    let record = cgen::class_record(FACTORY);

    let without = cgen::compile_unit_with(&classes, Some(&roots), &cgen::UnitExtras::default());
    assert!(
        without.executed_roots.is_none(),
        "no trace given, yet compile_unit_with (src/cgen.rs) reports executed roots"
    );
    assert!(
        !without.c_source.contains(&symbol),
        "{symbol} is emitted with no root reaching it: check reachable() in src/cgen.rs"
    );

    let extras = cgen::UnitExtras {
        executed_methods: executed,
        ..Default::default()
    };
    let with = cgen::compile_unit_with(&classes, Some(&roots), &extras);
    assert_eq!(
        with.executed_roots,
        Some(ExecutedRoots {
            roots: 1,
            class_not_in_unit: 2,
            method_not_in_class: 1,
            abstract_in_unit: 1,
            not_compiled: 1,
        }),
        "executed_roots in src/cgen.rs sorted the traced methods wrongly"
    );
    assert!(
        with.c_source.contains(&format!("{symbol}(")),
        "the executed method {symbol} is not emitted: check the executed roots in reachable() \
         (src/cgen.rs)"
    );
    assert!(
        with.c_source.contains(&record),
        "the executed method's class has no record ({record}), so reflection cannot list it: \
         check add_records for executed roots in reachable() (src/cgen.rs)"
    );
}

#[test]
fn a_method_reached_only_by_a_computed_name_is_compiled_from_the_jvm_trace() {
    if tools_present("the executed-roots end-to-end test").is_none() {
        return;
    }
    let startup = || cgen::UnitExtras {
        startup: true,
        ..Default::default()
    };
    // Without a trace: Method.invoke finds the method (its class has a record, from the class
    // literal) but nothing compiled it, so the run stops naming it.
    let untraced = build_native(FIXTURE, MAIN, "-untraced", Some(&[MAIN]), startup())
        .expect("tools are present");
    let (outcome, stop) = native_run(&untraced, "run");
    let expected = format!(
        "Method.invoke of {FACTORY}.{}{} (it is not compiled into this program)",
        MAKE.0, MAKE.1
    );
    assert!(
        stop.as_deref().is_some_and(|s| s.contains(&expected)),
        "without a trace the native run should stop with `{expected}` (runtime/jabs_reflect.c \
         not_compiled_member); if it ran instead, something other than the trace keeps \
         {FACTORY}.{} (reachable() or reflect::reach in src/cgen/reflect.rs)\nstop: {stop:?}\n\
         run: {outcome:#?}",
        MAKE.0
    );

    // The JVM run, traced by the agent; the classes are the ones the native build compiled.
    let dir = tempfile::tempdir().unwrap();
    let agent = build_agent(dir.path());
    let trace_path = dir.path().join("trace.txt");
    let output = Command::new("java")
        .arg(format!(
            "-agentpath:{}={}",
            agent.display(),
            trace_path.display()
        ))
        .arg("-cp")
        .arg(&untraced.classes)
        .args([MAIN, "run"])
        .output()
        .unwrap();
    assert!(output.status.success(), "traced JVM run failed: {output:?}");
    let trace = jabs::jar::read_method_trace(&trace_path).unwrap();
    let make = key(FACTORY, MAKE.0, MAKE.1);
    assert!(
        trace.methods.contains(&make),
        "the agent (tools/method-trace/jabs_trace.c) did not record {FACTORY}.{}",
        MAKE.0
    );

    let extras = cgen::UnitExtras {
        executed_methods: trace.methods.clone(),
        ..startup()
    };
    let traced =
        build_native(FIXTURE, MAIN, "-traced", Some(&[MAIN]), extras).expect("tools are present");
    let counts = traced
        .executed_roots
        .as_ref()
        .expect("a trace was given, so compile_unit_with reports executed roots");
    let total = counts.roots
        + counts.class_not_in_unit
        + counts.method_not_in_class
        + counts.abstract_in_unit
        + counts.not_compiled;
    assert_eq!(
        total,
        trace.methods.len(),
        "executed_roots (src/cgen.rs) must put every traced method in one count: {counts:?}"
    );
    eprintln!("executed roots of the fixture's trace: {counts:?}");
    same_as_jvm(
        Some(&traced),
        "run",
        "UnitExtras::executed_methods and the executed roots in reachable() (src/cgen.rs)",
    );
}
