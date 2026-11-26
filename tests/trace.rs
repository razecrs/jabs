//! The method-trace agent (`tools/method-trace/jabs_trace.c`) and the reader that refuses any
//! trace that is not whole.

mod common;

use common::{build_agent, tool_available};
use std::path::{Path, PathBuf};
use std::process::Command;

const HEADER: &str = "# jabs method trace 1\n";

fn write(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("trace.txt");
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn a_whole_trace_is_read_and_hidden_classes_are_set_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(
        dir.path(),
        &format!(
            "{HEADER}a/B\tm\t()V\njava/lang/Shutdown\thalt0\t(I)V\na/B$$Lambda.0x01\trun\t()V\n# complete 3\n"
        ),
    );
    let trace = jabs::jar::read_method_trace(&path).unwrap();
    assert_eq!(trace.methods.len(), 2);
    assert_eq!(trace.hidden_class_methods, 1);
    assert!(
        trace
            .methods
            .contains(&("java/lang/Shutdown".into(), "halt0".into(), "(I)V".into()))
    );
}

#[test]
fn a_trace_that_is_not_whole_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("a/B\tm\t()V\n# complete 1\n", "not a JABS method trace"),
        (
            &format!("{HEADER}a/B\tm\t()V\n") as &str,
            "incomplete trace",
        ),
        (&format!("{HEADER}a/B\tm\t()V\n# complete 2\n"), "says 2"),
        (
            &format!("{HEADER}a/B\tm\t()V\na/B\tm\t()V\n# complete 2\n"),
            "appears twice",
        ),
        (&format!("{HEADER}a/B\tm\n# complete 1\n"), "expected class"),
        (&format!("{HEADER}a/B\tm\tV\n# complete 1\n"), "malformed"),
        (
            &format!("{HEADER}a/B\tm\t()V\n# complete 1\nx/Y\tn\t()V\n"),
            "after the `# complete`",
        ),
        (&format!("{HEADER}# complete lots\n"), "bad method count"),
    ];
    for (text, expected) in cases {
        let path = write(dir.path(), text);
        let error = format!("{:#}", jabs::jar::read_method_trace(&path).unwrap_err());
        assert!(error.contains(expected), "{text:?}: {error}");
    }
}

#[test]
fn the_agent_records_what_a_run_executed_and_coverage_reads_it() {
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
        eprintln!("skipping trace test; missing {missing:?}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let agent = build_agent(root);
    let classes = root.join("classes");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/Programs.java");
    let status = Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(&classes)
        .arg(&fixture)
        .status()
        .unwrap();
    assert!(status.success());

    // ExitCode prints, then System.exit(5): the agent must keep the status and still finish
    // its trace, and record the native the exit goes through.
    let trace_path = root.join("trace.txt");
    let output = Command::new("java")
        .arg(format!(
            "-agentpath:{}={}",
            agent.display(),
            trace_path.display()
        ))
        .arg("-cp")
        .arg(&classes)
        .args(["ExitCode", "5"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let trace = jabs::jar::read_method_trace(&trace_path).unwrap();
    let ran = |class: &str, name: &str, descriptor: &str| {
        trace
            .methods
            .contains(&(class.into(), name.into(), descriptor.into()))
    };
    assert!(ran("ExitCode", "main", "([Ljava/lang/String;)V"));
    assert!(
        ran("java/lang/Shutdown", "halt0", "(I)V"),
        "natives are traced"
    );
    assert!(
        !ran("Uncaught", "main", "([Ljava/lang/String;)V"),
        "never ran"
    );
    assert!(!ran("Programs", "sumTo", "(I)I"), "never ran");

    // Coverage of the fixture jar (no JDK in the unit) against that trace.
    let jar = root.join("programs.jar");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&jar).unwrap());
    for entry in std::fs::read_dir(&classes).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, &std::fs::read(&path).unwrap()).unwrap();
    }
    zip.finish().unwrap();
    let report = jabs::jar::coverage_with(&jar, None, None, Some(&trace)).unwrap();
    let executed = report.executed.expect("an executed section");
    assert_eq!(executed.methods_traced, trace.methods.len());
    // Only ExitCode.main ran from the jar, and without java.base in the unit it cannot compile
    // (System.out is outside), so it is the one executed method to fix.
    assert_eq!(
        (
            executed.jar.executed,
            executed.jar.compiled,
            executed.jar.unsupported
        ),
        (1, 0, 1)
    );
    let blocked = &executed.jar.unsupported_methods[0];
    assert_eq!(
        (blocked.class.as_str(), blocked.name.as_str()),
        ("ExitCode", "main")
    );
    assert_eq!(executed.jar.blockers, [(blocked.category.clone(), 1)]);
    assert_eq!(
        (
            executed.jar.method_not_in_class,
            executed.jar.abstract_in_unit
        ),
        (0, 0)
    );
    assert_eq!(executed.methods_not_in_unit, trace.methods.len() - 1);
    assert!(executed.jdk.is_none());
}
