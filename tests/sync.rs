//! Monitors and atomics at the C level: runtime/jabs_sync.c linked with a C harness
//! (tests/sync/harness.c) whose raw OS threads hammer it, with no Java code involved. Each test
//! runs one harness test in its own process under a watchdog, so a deadlock fails within
//! seconds and names the test instead of hanging CI.
//!
//! Needs `clang` on PATH. A missing clang skips these tests locally; set JABS_REQUIRE_TOOLS=1
//! (as CI does) to make that a failure.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

/// How long one harness test may run before it counts as hung. The harness's own watchdog
/// fires first (20 s); this one catches a harness that cannot even report.
const TEST_LIMIT: Duration = Duration::from_secs(40);
/// How often the watchdog looks at the child.
const POLL: Duration = Duration::from_millis(20);

fn tool_available(name: &str, arg: &str) -> bool {
    Command::new(name)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Clang target arguments that can build a C program with threads: the default target first,
/// then MinGW on Windows hosts without the MSVC SDK. JABS_CLANG_ARGS overrides.
fn clang_target_args(dir: &Path) -> Vec<String> {
    if let Ok(args) = std::env::var("JABS_CLANG_ARGS") {
        return args.split_whitespace().map(String::from).collect();
    }
    let probe = dir.join("probe.c");
    std::fs::write(
        &probe,
        "#include <math.h>\n#include <stdio.h>\nint main(void) { return (int)fmod(1.0, 2.0); }\n",
    )
    .unwrap();
    let mut candidates = vec![Vec::new()];
    if cfg!(windows) {
        candidates.push(vec!["--target=x86_64-w64-windows-gnu".to_string()]);
    }
    for args in candidates {
        let ok = Command::new("clang")
            .args(&args)
            .arg(&probe)
            .arg("-o")
            .arg(dir.join("probe.exe"))
            .args(if cfg!(windows) { None } else { Some("-lm") })
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            return args;
        }
    }
    panic!(
        "clang cannot build a C program with <math.h> for any known target; set JABS_CLANG_ARGS"
    );
}

/// The harness executable, built once for all tests: an empty unit (runtime only, no classes)
/// gives jabs_unit.h and the runtime files, compiled with the flags generated code gets.
/// None when clang is missing and tools are not required.
fn harness() -> Option<&'static Path> {
    static BUILT: OnceLock<Option<(tempfile::TempDir, PathBuf)>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let required = std::env::var_os("JABS_REQUIRE_TOOLS").is_some();
            if !tool_available("clang", "--version") {
                assert!(!required, "required tool missing: clang");
                eprintln!("skipping sync tests; clang missing");
                return None;
            }
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let unit = jabs::cgen::compile_unit_rooted(&[], None);
            let mut sources = Vec::new();
            for (name, contents) in &unit.c_files {
                std::fs::write(root.join(name), contents).unwrap();
                if name.ends_with(".c") {
                    sources.push(root.join(name));
                }
            }
            let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sync");
            for name in ["harness.c", "os_threads.c", "os_threads.h"] {
                std::fs::copy(here.join(name), root.join(name)).unwrap();
            }
            sources.push(root.join("harness.c"));
            sources.push(root.join("os_threads.c"));
            let exe = root.join(if cfg!(windows) {
                "harness.exe"
            } else {
                "harness"
            });
            let output = Command::new("clang")
                .args(clang_target_args(root))
                .args([
                    "-std=c11",
                    "-O2",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    "-Wno-unused-function",
                    "-Wno-unused-label",
                    "-D_CRT_SECURE_NO_WARNINGS",
                ])
                .args(&sources)
                .arg("-o")
                .arg(&exe)
                .args(if cfg!(windows) {
                    Vec::new()
                } else {
                    vec!["-lm", "-lpthread"]
                })
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "building the sync harness failed (runtime/jabs_sync.c or tests/sync/*.c):\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Some((dir, exe))
        })
        .as_ref()
        .map(|(_, exe)| exe.as_path())
}

/// Run one harness test in its own process; fail with its report, or name it if it hangs.
fn run(test: &str) {
    let Some(exe) = harness() else { return };
    let mut child = Command::new(exe)
        .arg(test)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn the sync harness for {test}: {e}"));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > TEST_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{test}: still running after {TEST_LIMIT:?} and killed: a deadlock or a lost \
                 wakeup in runtime/jabs_sync.c (the harness watchdog did not fire either)"
            );
        }
        std::thread::sleep(POLL);
    };
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        status.success() && stdout.trim() == format!("ok {test}"),
        "{test} failed ({status}):\n{stderr}{stdout}"
    );
}

#[test]
fn monitor_excludes_concurrent_owners() {
    run("monitor_excludes_concurrent_owners");
}

#[test]
fn monitor_first_use_race_installs_one_monitor() {
    run("monitor_first_use_race_installs_one_monitor");
}

#[test]
fn monitor_reentry_counts_levels() {
    run("monitor_reentry_counts_levels");
}

#[test]
fn monitor_unlocked_object_is_not_owned() {
    run("monitor_unlocked_object_is_not_owned");
}

#[test]
fn monitor_exit_by_non_owner_is_refused() {
    run("monitor_exit_by_non_owner_is_refused");
}

#[test]
fn monitor_blocks_until_owner_exits() {
    run("monitor_blocks_until_owner_exits");
}

#[test]
fn wait_releases_all_levels_and_restores_count() {
    run("wait_releases_all_levels_and_restores_count");
}

#[test]
fn wait_and_notify_need_ownership() {
    run("wait_and_notify_need_ownership");
}

#[test]
fn timed_wait_times_out_after_the_limit() {
    run("timed_wait_times_out_after_the_limit");
}

#[test]
fn notify_before_wait_is_not_remembered() {
    run("notify_before_wait_is_not_remembered");
}

#[test]
fn notify_wakes_exactly_one_waiter() {
    run("notify_wakes_exactly_one_waiter");
}

#[test]
fn notify_all_wakes_every_waiter() {
    run("notify_all_wakes_every_waiter");
}

#[test]
fn wait_notify_ping_pong_loses_no_wakeups() {
    run("wait_notify_ping_pong_loses_no_wakeups");
}

#[test]
fn interrupt_wakes_a_waiting_thread() {
    run("interrupt_wakes_a_waiting_thread");
}

#[test]
fn interrupt_before_wait_returns_at_once() {
    run("interrupt_before_wait_returns_at_once");
}

#[test]
fn atomic_float_add_loses_no_updates() {
    run("atomic_float_add_loses_no_updates");
}

#[test]
fn atomic_compare_and_set_loses_no_updates() {
    run("atomic_compare_and_set_loses_no_updates");
}

#[test]
fn volatile_long_is_never_torn() {
    run("volatile_long_is_never_torn");
}
