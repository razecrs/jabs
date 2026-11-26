//! The .jab archive (src/jab.rs): what `jabs compile --jab` writes and `jabs pack` appends.
//! Each test checks one property; a failure names the function to look at. The JVM is the
//! reference reader: a .jab must be a jar it runs the program from, and a packed executable a
//! zip with a stub in front, as it reads self-extracting archives.

mod common;

use jabs::jab;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// The fixture program (tests/fixtures/JabFiles.java and JabPlugins.java) as a jar made by the
/// JDK's own `jar` tool, with a manifest and two data files: (jar, work directory). None when
/// the tools are missing and not required.
fn fixture_jar(dir: &Path) -> Option<PathBuf> {
    let required = std::env::var_os("JABS_REQUIRE_TOOLS").is_some();
    for (tool, arg) in [
        ("javac", "-version"),
        ("java", "-version"),
        ("jar", "--version"),
    ] {
        if !common::tool_available(tool, arg) {
            assert!(!required, "required tool missing: {tool}");
            eprintln!("skipping .jab tests: {tool} missing");
            return None;
        }
    }
    let classes = dir.join("classes");
    fs::create_dir_all(classes.join("jabfix/data")).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    common::run(
        Command::new("javac")
            .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
            .arg(&classes)
            .arg(fixtures.join("JabFiles.java"))
            .arg(fixtures.join("JabPlugins.java")),
    );
    fs::write(
        classes.join("jabfix/data/hello.txt"),
        "hello from the class path\n",
    )
    .unwrap();
    fs::write(classes.join("jabfix/data/other.txt"), "other\n").unwrap();
    let manifest = dir.join("manifest.txt");
    fs::write(&manifest, "Main-Class: jabfix.JabFiles\n").unwrap();
    let jar = dir.join("fixture.jar");
    common::run(
        Command::new("jar")
            .arg("--create")
            .arg("--file")
            .arg(&jar)
            .arg("--manifest")
            .arg(&manifest)
            .arg("-C")
            .arg(&classes)
            .arg("."),
    );
    Some(jar)
}

/// The .jab of the fixture jar alone (no java.home files).
fn fixture_jab(jar: &Path) -> (Vec<u8>, String) {
    let mut builder = jab::Builder::new();
    builder
        .add_inputs(&[jar.to_path_buf()])
        .expect("jab::Builder::add_inputs on the fixture jar");
    builder.finish().expect("jab::Builder::finish")
}

/// `java -cp <class path> jabfix.JabFiles <mechanism>`: stdout, failing on a nonzero exit.
fn jvm(class_path: &Path, mechanism: &str) -> String {
    common::run(
        Command::new("java")
            .arg("-cp")
            .arg(class_path)
            .args(["jabfix.JabFiles", mechanism]),
    )
}

#[test]
fn jab_written_twice_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fixture_jar(dir.path()) else {
        return;
    };
    let (first, first_id) = fixture_jab(&jar);
    let (second, second_id) = fixture_jab(&jar);
    assert!(
        first == second && first_id == second_id,
        "two .jabs of the same inputs differ: entry order or timestamps are not fixed \
         (jab::Builder::finish, jab::write)"
    );
}

#[test]
fn jab_reads_back_in_an_independent_zip_reader() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fixture_jar(dir.path()) else {
        return;
    };
    let (bytes, _) = fixture_jab(&jar);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .expect("the zip crate cannot open the .jab (jab::write's headers)");
    let mut original = zip::ZipArchive::new(fs::File::open(&jar).unwrap()).unwrap();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        assert_eq!(
            entry.compression(),
            zip::CompressionMethod::Stored,
            "{} is not STORED (jab::write)",
            entry.name()
        );
        let name = entry.name().to_string();
        let mut data = Vec::new();
        // The zip crate checks each entry's CRC as it reads it.
        std::io::Read::read_to_end(&mut entry, &mut data)
            .unwrap_or_else(|e| panic!("{name}: {e} (jab::write's CRC or sizes)"));
        if name.ends_with('/') || name.starts_with("META-INF/jabs/") || name == jab::HOME_DIR {
            continue;
        }
        let mut source = original
            .by_name(&name)
            .unwrap_or_else(|_| panic!("{name} is in the .jab but not in the jar"));
        let mut expected = Vec::new();
        std::io::Read::read_to_end(&mut source, &mut expected).unwrap();
        assert_eq!(data, expected, "{name}: contents differ from the jar's");
    }
    for index in 0..original.len() {
        let name = original.by_index(index).unwrap().name().to_string();
        assert!(
            archive.by_name(&name).is_ok(),
            "{name} is in the jar but not in the .jab (jab::Builder::add_inputs)"
        );
    }
}

#[test]
fn jvm_runs_the_program_from_its_jab_as_from_its_jar() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fixture_jar(dir.path()) else {
        return;
    };
    let (bytes, _) = fixture_jab(&jar);
    let path = dir.path().join("fixture.jab");
    fs::write(&path, bytes).unwrap();
    for mechanism in ["scan", "jarurl", "stream", "resources"] {
        assert_eq!(
            jvm(&path, mechanism),
            jvm(&jar, mechanism),
            "{mechanism}: the JVM reads the .jab differently from the jar it was made from \
             (jab::Builder: directory entries, manifest position, signature files)"
        );
    }
}

#[test]
fn jvm_runs_the_program_from_a_packed_executable() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fixture_jar(dir.path()) else {
        return;
    };
    let (bytes, _) = fixture_jab(&jar);
    let jab_path = dir.path().join("fixture.jab");
    fs::write(&jab_path, bytes).unwrap();
    // A stand-in for the linked program: what precedes the archive does not matter to a zip
    // reader, which finds the archive from its end.
    let stub = dir.path().join("stub.exe");
    let mut stub_bytes = b"MZ".to_vec();
    stub_bytes.extend((0..5000u32).map(|i| (i * 7 % 251) as u8));
    fs::write(&stub, &stub_bytes).unwrap();
    let packed = dir.path().join("packed.exe");
    jab::pack(&stub, &jab_path, &packed).expect("jab::pack");
    let joined = fs::read(&packed).unwrap();
    assert!(
        joined.starts_with(&stub_bytes),
        "jab::pack changed the executable's bytes"
    );
    let archive = jab::read(&joined)
        .expect("jab::read of the packed program")
        .expect("no archive found at the end of the packed program");
    assert_eq!(
        archive.base,
        stub_bytes.len() as u64,
        "the archive does not start right after the executable (jab::pack)"
    );
    // Random access (JarFile) works through the stub; the sequential scan does not, which is
    // why the native runtime serves a packed .jab as its own file (runtime/jabs_jab.c).
    for mechanism in ["jarurl", "stream", "resources"] {
        assert_eq!(
            jvm(&packed, mechanism),
            jvm(&jar, mechanism),
            "{mechanism}: the JVM reads the packed executable differently from the jar \
             (jab::write's offsets must count from the archive's start)"
        );
    }
    let error = jab::pack(&packed, &jab_path, &dir.path().join("twice.exe"))
        .expect_err("jab::pack packed a program twice");
    assert!(
        error.to_string().contains("already ends"),
        "unexpected message: {error}"
    );
}

/// Flip one byte at `at`.
fn damaged(bytes: &[u8], at: usize) -> Vec<u8> {
    let mut copy = bytes.to_vec();
    copy[at] ^= 0x55;
    copy
}

#[test]
fn damaged_jabs_are_refused_by_the_reader() {
    let mut builder = jab::Builder::new();
    builder
        .add_file("a/one.txt", b"first".to_vec(), "t")
        .unwrap();
    builder
        .add_file("a/two.txt", b"other".to_vec(), "t")
        .unwrap();
    let (bytes, _) = builder.finish().unwrap();
    assert!(jab::read(&bytes).unwrap().is_some());
    let expect = |copy: &[u8], what: &str, needle: &str| {
        let error = match jab::read(copy) {
            Ok(Some(_)) => panic!("jab::read accepted a .jab with {what}"),
            Ok(None) => "no end record".to_string(),
            Err(e) => e.to_string(),
        };
        assert!(
            error.contains(needle),
            "{what}: message `{error}` does not say `{needle}`"
        );
    };
    // Truncated: the end record is gone.
    expect(
        &bytes[..bytes.len() - 7],
        "the end cut off",
        "no end record",
    );
    // Truncated in front: the directory's offsets point before the file.
    expect(&bytes[40..], "the start cut off", "before the start");
    // A data byte changed: the CRC no longer matches.
    let data = bytes.windows(5).position(|w| w == b"first").unwrap();
    expect(&damaged(&bytes, data), "a changed data byte", "bad CRC");
    // One name made equal to another in both headers: a duplicate entry.
    let mut duplicate = bytes.clone();
    let mut positions = Vec::new();
    for at in 0..duplicate.len() - 9 {
        if &duplicate[at..at + 9] == b"a/two.txt" {
            positions.push(at);
        }
    }
    assert_eq!(
        positions.len(),
        2,
        "expected the name in the local and central headers"
    );
    for at in positions {
        duplicate[at + 2..at + 5].copy_from_slice(b"one");
    }
    expect(&duplicate, "a duplicate entry", "appears twice");
    // Not a .jab: a jar without the marker.
    let plain = jab::write(&[("x.txt", &b"x"[..])]).unwrap();
    expect(&plain, "no marker", "not a .jab");
}

#[test]
fn java_home_list_copies_files_directories_and_listings() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("jdk");
    fs::create_dir_all(home.join("lib/security")).unwrap();
    fs::create_dir_all(home.join("conf/sub")).unwrap();
    fs::write(home.join("lib/tzdb.dat"), b"tz").unwrap();
    fs::write(home.join("lib/unused.dat"), b"no").unwrap();
    fs::write(home.join("lib/security/cacerts"), b"certs").unwrap();
    fs::write(home.join("conf/a.properties"), b"a").unwrap();
    let list = dir.path().join("java-home.txt");
    fs::write(
        &list,
        "# recorded\nlib/tzdb.dat\nlib/tzdb.dat\nlib/security/\nconf/*\nlib/not-there.dat\n",
    )
    .unwrap();
    let (entries, missing) = jab::java_home_entries(&home, &list).expect("jab::java_home_entries");
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "jabs-home/conf/",
            "jabs-home/conf/a.properties",
            "jabs-home/conf/sub/",
            "jabs-home/lib/security/",
            "jabs-home/lib/tzdb.dat"
        ],
        "jab::java_home_entries: files copied, directories as entries, `dir/*` lists one level"
    );
    assert_eq!(missing, 1, "lib/not-there.dat is the one missing path");
    fs::write(&list, "../outside\n").unwrap();
    assert!(
        jab::java_home_entries(&home, &list).is_err(),
        "jab::java_home_entries accepted a path that leaves java.home"
    );
}

// ---- The native program and its .jab (runtime/jabs_jab.c, jabs.rt.AppLoader) ----

/// The fixture compiled natively with a .jab, built once per test binary: the recording build
/// runs first and its log is the java.home list of the final build, as in production.
struct NativeJab {
    dir: PathBuf,
    jar: PathBuf,
    /// The final program and its .jab beside it (standard mode).
    native: PathBuf,
    jab: PathBuf,
    /// The recording build's log: the java.home files the run touched.
    recorded: String,
    /// The final program's C files.
    c_files: Vec<(String, String)>,
    home: PathBuf,
}

static NATIVE: std::sync::OnceLock<Result<Option<NativeJab>, String>> = std::sync::OnceLock::new();

/// Mechanisms whose output must match the JVM's; `missing` and `readonly` differ on purpose.
const SAME_AS_JVM: [&str; 5] = ["scan", "jarurl", "stream", "resources", "home"];

fn native() -> Option<&'static NativeJab> {
    let result = NATIVE.get_or_init(|| {
        std::panic::catch_unwind(build_native_jab).map_err(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string())
        })
    });
    match result {
        Ok(built) => built.as_ref(),
        Err(message) => panic!("building the .jab fixture natively failed:\n{message}"),
    }
}

/// One build of the unit with this .jab configuration; the .jab goes beside the executable.
fn build_with_jab(
    dir: &Path,
    classes: &[jabs::classfile::ClassFile],
    inputs: &[PathBuf],
    home: &Path,
    home_list: Option<&Path>,
    record_log: Option<&Path>,
) -> (PathBuf, PathBuf, Vec<(String, String)>) {
    let mut builder = jab::Builder::new();
    builder
        .add_inputs(inputs)
        .expect("jab::Builder::add_inputs");
    if let Some(list) = home_list {
        let (entries, _) = jab::java_home_entries(home, list).expect("jab::java_home_entries");
        jab::add_java_home(&mut builder, entries, "list").unwrap();
    }
    let (bytes, id) = builder.finish().expect("jab::Builder::finish");
    let app_classes: std::collections::BTreeSet<String> = jabs::jar::load_classes(inputs)
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    let roots: std::collections::BTreeSet<String> = ["jabfix/JabFiles".to_string()].into();
    let extras = jabs::cgen::UnitExtras {
        app_classes,
        java_home: record_log.map(|_| home.display().to_string()),
        // Loaded by name after the scan finds them, as log4j loads its plugins.
        loaded_classes: [
            "jabfix/plugins/JabPlugins",
            "jabfix/plugins/FirstPlugin",
            "jabfix/plugins/SecondPlugin",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        startup: true,
        jab: Some(jabs::cgen::JabConfig {
            id,
            record_log: record_log.map(|l| l.display().to_string()),
        }),
        ..Default::default()
    };
    let compiled = jabs::cgen::compile_unit_with(classes, Some(&roots), &extras);
    let native = common::link_native(dir, &compiled.c_files, "jabfix/JabFiles", "fixture");
    let jab_path = dir.join("fixture.jab");
    fs::write(&jab_path, bytes).unwrap();
    (native, jab_path, compiled.c_files)
}

fn build_native_jab() -> Option<NativeJab> {
    let required = std::env::var_os("JABS_REQUIRE_TOOLS").is_some();
    if !common::tool_available("clang", "--version") {
        assert!(!required, "required tool missing: clang");
        return None;
    }
    let dir = common::fresh_build_dir("tests/fixtures/JabFiles.java", "-jab");
    let jar = fixture_jar(&dir)?;
    // JABS's own app class loader, as tools/native-boot.sh compiles it.
    let rt = dir.join("rt");
    let sources: Vec<PathBuf> =
        fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/java/jabs/rt"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
    common::run(
        Command::new("javac")
            .args([
                "-XDstringConcat=inline",
                "-nowarn",
                "--add-exports",
                "java.base/sun.net.www=ALL-UNNAMED",
                "-d",
            ])
            .arg(&rt)
            .args(&sources),
    );
    let mut inputs = vec![jar.clone()];
    let mut stack = vec![rt.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "class") {
                inputs.push(p);
            }
        }
    }
    inputs.sort();
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let mut classes = jabs::jar::load_classes(&inputs).unwrap();
    let (library, _, _) = jabs::jdk::read_module(&home, "java.base").unwrap();
    classes.extend(library);
    // 1. The recording build: java.home is the JDK; every path the run touches is logged.
    let log = dir.join("java-home-recorded.txt");
    let (recorder, _, _) = build_with_jab(&dir, &classes, &inputs, &home, None, Some(&log));
    for mechanism in SAME_AS_JVM {
        let (outcome, stop) = common::outcome(Command::new(&recorder).arg(mechanism), "uncaught ");
        assert!(
            outcome.status == Some(0),
            "the recording build failed on {mechanism}: {outcome:?} {stop:?}"
        );
    }
    let recorded = fs::read_to_string(&log)
        .expect("the recording build wrote no log (jab_record, runtime/jabs_jab.c)");
    // 2. The final build: java.home is the .jab's jabs-home/, holding exactly what was recorded.
    let (native, jab_path, c_files) =
        build_with_jab(&dir, &classes, &inputs, &home, Some(&log), None);
    Some(NativeJab {
        dir,
        jar,
        native,
        jab: jab_path,
        recorded,
        c_files,
        home,
    })
}

/// stdout of the native program, failing with stderr when it does not exit 0.
fn native_output(program: &Path, mechanism: &str) -> String {
    let (outcome, stop) = common::outcome(Command::new(program).arg(mechanism), "uncaught ");
    assert!(
        outcome.status == Some(0),
        "{mechanism}: the native program failed: {outcome:?}\njabs stop: {stop:?}"
    );
    outcome.stdout
}

#[test]
fn recording_lists_the_java_home_files_the_run_read() {
    let Some(built) = native() else { return };
    for file in ["lib/tzdb.dat", "conf/security/java.security"] {
        assert!(
            built.recorded.lines().any(|l| l == file),
            "the recording does not list {file} (jab_record in runtime/jabs_jab.c, called from \
             jabs_vfs_find):\n{}",
            built.recorded
        );
    }
    if cfg!(windows) {
        assert!(
            built.recorded.lines().any(|l| l == "lib/tzmappings"),
            "the recording does not list lib/tzmappings (jabs_platform_time_zone_id reads it \
             through jabs_fd_open):\n{}",
            built.recorded
        );
    }
    assert!(
        built.recorded.lines().any(|l| l == "conf/security/*"),
        "the recording does not note the listing of conf/security (jabs_vfs_note_listing):\n{}",
        built.recorded
    );
}

#[test]
fn native_reads_its_jab_as_the_jvm_reads_the_jar() {
    let Some(built) = native() else { return };
    for mechanism in SAME_AS_JVM {
        let jvm_out = jvm(&built.jar, mechanism);
        let native_out = native_output(&built.native, mechanism);
        assert_eq!(
            native_out, jvm_out,
            "{mechanism}: the native program from its .jab differs from the JVM from the jar \
             (jabs.rt.AppLoader for resources, runtime/jabs_jab.c and jabs_files.c / jabs_nio_fs.c \
             for files)"
        );
    }
}

#[test]
fn final_program_never_names_the_build_jdk() {
    let Some(built) = native() else { return };
    let home = built.home.display().to_string();
    let forms = [
        home.clone(),
        home.replace('\\', "/"),
        home.replace('\\', "\\\\"),
    ];
    for (name, text) in &built.c_files {
        let lower = text.to_lowercase();
        for form in &forms {
            assert!(
                !lower.contains(&form.to_lowercase()),
                "{name} names the build JDK ({form}): the final program could reach it \
                 (src/main.rs and UnitExtras::java_home must be None with a .jab)"
            );
        }
    }
    let exe = fs::read(&built.native).unwrap();
    assert!(
        !exe.windows(home.len())
            .any(|w| w.eq_ignore_ascii_case(home.as_bytes())),
        "the executable contains the build JDK's path"
    );
}

#[test]
fn java_home_file_not_in_the_jab_is_missing() {
    let Some(built) = native() else { return };
    let reason = if cfg!(windows) {
        "(The system cannot find the file specified)"
    } else {
        "(No such file or directory)"
    };
    assert_eq!(
        native_output(&built.native, "missing"),
        format!("jvm.cfg exists false\nFileNotFoundException {reason}\nNoSuchFileException\n"),
        "lib/jvm.cfg is in the JDK but was never read, so the .jab does not hold it: it must be \
         missing, not read from the build machine (jabs_vfs_find in runtime/jabs_jab.c)"
    );
}

#[test]
fn java_home_is_read_only() {
    let Some(built) = native() else { return };
    let reason = if cfg!(windows) {
        "(The media is write protected)"
    } else {
        "(Read-only file system)"
    };
    assert_eq!(
        native_output(&built.native, "readonly"),
        format!("FileNotFoundException {reason}\nmkdir false, delete false\n"),
        "writing under java.home must fail as on a read-only volume (jabs_fd_open and \
         files_virtual in runtime/jabs_files.c)"
    );
}

#[test]
fn portable_program_runs_as_the_standard_one() {
    let Some(built) = native() else { return };
    let dir = built.dir.join("portable");
    fs::create_dir_all(&dir).unwrap();
    let packed = dir.join(if cfg!(windows) {
        "portable.exe"
    } else {
        "portable"
    });
    jab::pack(&built.native, &built.jab, &packed).expect("jab::pack");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&packed, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(
        !dir.join("portable.jab").exists(),
        "the portable program must not find a .jab beside it"
    );
    for mechanism in SAME_AS_JVM {
        assert_eq!(
            native_output(&packed, mechanism),
            native_output(&built.native, mechanism),
            "{mechanism}: the packed program differs from the standard one (the portable \
             search and the virtual archive file in runtime/jabs_jab.c)"
        );
    }
}

/// Run a copy of the native program beside `jab_bytes` (None: no .jab) and return stderr,
/// requiring exit status 1.
fn refused(built: &NativeJab, case: &str, jab_bytes: Option<&[u8]>) -> String {
    let dir = built.dir.join("refused").join(case);
    fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(built.native.file_name().unwrap());
    fs::copy(&built.native, &exe).unwrap();
    let jab_path = dir.join("fixture.jab");
    let _ = fs::remove_file(&jab_path);
    if let Some(bytes) = jab_bytes {
        fs::write(&jab_path, bytes).unwrap();
    }
    let (stdout, stderr, status) = common::watched(Command::new(&exe).arg("stream"));
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    assert_eq!(
        status.code(),
        Some(1),
        "{case}: the program ran with a bad .jab (jab_check in runtime/jabs_jab.c)\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&stdout)
    );
    assert!(
        stdout.is_empty(),
        "{case}: the program printed before refusing its .jab"
    );
    stderr
}

#[test]
fn damaged_or_missing_jab_stops_the_program() {
    let Some(built) = native() else { return };
    let good = fs::read(&built.jab).unwrap();
    let expect = |case: &str, bytes: Option<&[u8]>, needle: &str| {
        let stderr = refused(built, case, bytes);
        assert!(
            stderr.contains(needle),
            "{case}: the message does not say `{needle}` (jab_check in runtime/jabs_jab.c):\n{stderr}"
        );
    };
    expect("missing", None, "has no .jab appended");
    expect(
        "truncated",
        Some(&good[..good.len() - 10]),
        "no zip end record",
    );
    let data = good
        .windows(25)
        .position(|w| w == b"hello from the class path")
        .expect("hello.txt's contents in the .jab");
    let mut bad_crc = good.clone();
    bad_crc[data] ^= 0x20;
    expect(
        "bad-crc",
        Some(&bad_crc),
        "bad CRC for jabfix/data/hello.txt",
    );
    let mut duplicate = good.clone();
    let positions: Vec<usize> = (0..duplicate.len() - 21)
        .filter(|&at| &duplicate[at..at + 21] == b"jabfix/data/other.txt")
        // Only names in headers: the fixture's class file has the string too.
        .filter(|&at| {
            (at >= 30 && duplicate[at - 30..at - 26] == [0x50, 0x4b, 3, 4])
                || (at >= 46 && duplicate[at - 46..at - 42] == [0x50, 0x4b, 1, 2])
        })
        .collect();
    assert_eq!(
        positions.len(),
        2,
        "other.txt's name in the local and central headers"
    );
    for at in positions {
        duplicate[at + 12..at + 17].copy_from_slice(b"hello");
    }
    expect("duplicate", Some(&duplicate), "duplicate entry");
    let mut other = jab::Builder::new();
    other.add_file("x.txt", b"x".to_vec(), "t").unwrap();
    let (other_bytes, _) = other.finish().unwrap();
    expect(
        "other-build",
        Some(&other_bytes),
        "belongs to another build",
    );
}
