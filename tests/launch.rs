//! `jabs run` against a fake server jar that behaves like Minecraft's console:
//! prints a `Done (...)` line, then exits when it reads `stop`.

use jabs::launch::{self, RunOptions};
use serde_json::json;
use std::{fs, io::Write, path::Path, process::Command, time::Duration};
use zip::write::SimpleFileOptions;

const FAKE_SERVER: &str = r#"
public class FakeServer {
    public static void main(String[] args) throws Exception {
        System.out.println("[Server thread/INFO]: Starting minecraft server version fake");
        System.out.println("[Server thread/INFO]: Done (0.001s)! For help, type \"help\"");
        java.io.BufferedReader in = new java.io.BufferedReader(new java.io.InputStreamReader(System.in));
        String line;
        while ((line = in.readLine()) != null) {
            if (line.trim().equals("stop")) {
                System.out.println("[Server thread/INFO]: Stopping server");
                return;
            }
        }
    }
}
"#;

/// Compile the fake server and package it as a jar whose manifest wraps `Main-Class` across lines.
fn fake_server_jar(dir: &Path) -> Option<std::path::PathBuf> {
    let javac_ok = Command::new("javac")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    let java_ok = Command::new("java")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !(javac_ok && java_ok) {
        assert!(
            std::env::var_os("JABS_REQUIRE_TOOLS").is_none(),
            "javac/java required"
        );
        eprintln!("skipping launch test; javac/java unavailable");
        return None;
    }
    fs::write(dir.join("FakeServer.java"), FAKE_SERVER).unwrap();
    let status = Command::new("javac")
        .arg("-d")
        .arg(dir)
        .arg(dir.join("FakeServer.java"))
        .status()
        .unwrap();
    assert!(status.success());
    let jar = dir.join("server.jar");
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar).unwrap());
    zip.start_file("META-INF/MANIFEST.MF", SimpleFileOptions::default())
        .unwrap();
    // "FakeServer" split by a manifest continuation line (leading space).
    zip.write_all(b"Manifest-Version: 1.0\r\nMain-Class: FakeSe\r\n rver\r\n\r\n")
        .unwrap();
    zip.start_file("FakeServer.class", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&fs::read(dir.join("FakeServer.class")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    Some(jar)
}

/// Write a server profile for `jar` requiring the given Java major version.
fn write_profile(dir: &Path, jar: &Path, java_major: u32) -> std::path::PathBuf {
    let (_, sha256) = jabs::profile::hash_file(jar).unwrap();
    let profile = json!({
        "schema_version": 1, "name": "fake-server", "minecraft_version": "test", "java_major": java_major,
        "loader": {"kind": "vanilla"}, "backend": "jvm",
        "artifacts": [{"id": "server", "role": "game", "path": "server.jar", "sha256": sha256}],
        "launch": {"side": "server", "args": ["nogui"]}
    });
    let path = dir.join("profile.json");
    fs::write(&path, serde_json::to_vec_pretty(&profile).unwrap()).unwrap();
    path
}

fn options() -> RunOptions {
    RunOptions {
        until_ready: true,
        timeout: Duration::from_secs(60),
    }
}

#[test]
fn manifest_continuation_lines_are_joined() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fake_server_jar(dir.path()) else {
        return;
    };
    assert_eq!(launch::manifest_main_class(&jar).unwrap(), "FakeServer");
}

#[test]
fn refuses_without_eula_and_never_writes_it() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fake_server_jar(dir.path()) else {
        return;
    };
    let major = launch::java_major(Path::new("java")).unwrap();
    let profile = write_profile(dir.path(), &jar, major);
    let error = launch::run(&profile, &options()).unwrap_err().to_string();
    assert!(error.contains("EULA"), "{error}");
    assert!(!dir.path().join("run/fake-server/eula.txt").exists());
}

#[test]
fn rejects_mismatched_java() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fake_server_jar(dir.path()) else {
        return;
    };
    let major = launch::java_major(Path::new("java")).unwrap();
    let profile = write_profile(dir.path(), &jar, major + 1);
    fs::create_dir_all(dir.path().join("run/fake-server")).unwrap();
    fs::write(dir.path().join("run/fake-server/eula.txt"), "eula=true\n").unwrap();
    let error = launch::run(&profile, &options()).unwrap_err().to_string();
    assert!(
        error.contains(&format!("requires Java {}", major + 1)),
        "{error}"
    );
}

#[test]
fn runs_until_ready_and_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let Some(jar) = fake_server_jar(dir.path()) else {
        return;
    };
    let major = launch::java_major(Path::new("java")).unwrap();
    let profile = write_profile(dir.path(), &jar, major);
    fs::create_dir_all(dir.path().join("run/fake-server")).unwrap();
    fs::write(dir.path().join("run/fake-server/eula.txt"), "eula=true\n").unwrap();
    let report = launch::run(&profile, &options()).unwrap();
    assert_eq!(report.main_class, "FakeServer");
    assert!(report.ready_ms.is_some(), "{report:?}");
    assert!(report.stopped_by_jabs && !report.killed_after_timeout);
    assert_eq!(report.exit_code, Some(0));
}
