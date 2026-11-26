//! java.net and java.nio: tests/fixtures/Networking.java is compiled once into a native program
//! (with java.base, as in production), and each test runs one mechanism
//! (`NetworkingCases <mechanism>`) on the reference JVM and natively and compares stdout, exit
//! status and an escaping exception (tests/common/mod.rs, which kills a run that hangs). Loopback
//! only: the only names looked up are the machine's own and "localhost". A failure names the
//! mechanism, the first differing line and the function to look at.
//!
//! The program is built as the server is: with the classes and methods a JVM run of every
//! mechanism loaded and executed (`-Xlog:class+load` and tools/method-trace), since the JDK's
//! networking code loads some classes only by name (ThreadLocalRandom$Access, for one).
//!
//! Needs `javac`, `java` and `clang`; missing tools skip these tests locally, and fail them
//! when JABS_REQUIRE_TOOLS=1 (as CI sets).

mod common;

use common::{Built, build_agent, build_native, run, same_as_jvm, tool_available, watched};
use std::{collections::BTreeSet, fs, path::Path, process::Command, sync::OnceLock};

const FIXTURE: &str = "tests/fixtures/Networking.java";
const MAIN: &str = "NetworkingCases";

/// Every mechanism, with where to look when it differs. The build traces each of them.
const MECHANISMS: &[(&str, &str)] = &[
    (
        "localHostName",
        "jabs_net_local_host_name (gethostname) and jabs_net_lookup in runtime/jabs_net.c",
    ),
    (
        "localhostByName",
        "jabs_net_lookup and address_array (IPv4-first order) in runtime/jabs_net.c; jabs_net_new_inet4/6 in src/cgen/net.rs",
    ),
    (
        "ownAddresses",
        "keep_address (duplicates) and address_array (order, IPv6 scope ids) in runtime/jabs_net.c",
    ),
    (
        "unknownHost",
        "the getaddrinfo failure in jabs_net_lookup (net_raise_last_error on Windows, gai_strerror on Linux) in runtime/jabs_net.c; jabs_net_enhanced_host_info and jabs_net_raise in src/cgen/net.rs",
    ),
    (
        "literalAddresses",
        "jabs_net_ipv6_available and jabs_net_address_is_bound (NetworkInterface.boundInetAddress0) in runtime/jabs_net.c",
    ),
    (
        "hostByAddress",
        "jabs_net_host_by_addr (getnameinfo, NI_NAMEREQD) in runtime/jabs_net.c",
    ),
    (
        "blockingEcho",
        "jabs_net_socket/bind/listen/accept/connect, jabs_net_read/write and sockaddr_to_inet in runtime/jabs_net.c",
    ),
    (
        "bindInUse",
        "jabs_net_bind (SO_EXCLUSIVEADDRUSE on Windows) and net_throw_new / socket_error in runtime/jabs_net.c",
    ),
    (
        "connectRefused",
        "jabs_net_connect (fast_loopback_connect on Windows) and net_throw_new / socket_error in runtime/jabs_net.c",
    ),
    (
        "shutdownOutputIsEndOfStream",
        "jabs_net_shutdown and the end-of-stream return of jabs_net_read in runtime/jabs_net.c",
    ),
    (
        "socketOptions",
        "jabs_net_get_option / jabs_net_set_option (SO_LINGER, Linux's halved buffer sizes) in runtime/jabs_net.c",
    ),
    (
        "selectorEcho",
        "the selector: jabs_net_epoll_* (WSAPoll emulation of WEPoll on Windows, epoll on Linux), jabs_net_configure_blocking, and on Windows the wakeup pipe's jabs_net_uds_* in runtime/jabs_net.c",
    ),
    (
        "selectorWakeup",
        "Selector.wakeup through the wakeup pipe (jabs_net_uds_* and jabs_net_write1/drain on Windows, jabs_net_eventfd* on Linux) and jabs_net_epoll_wait's timeout in runtime/jabs_net.c",
    ),
    (
        "classicEcho",
        "java.net.Socket/ServerSocket over NioSocketImpl: jabs_net_accept/read/write in runtime/jabs_net.c",
    ),
    (
        "classicBindInUse",
        "NioSocketImpl's bind: jabs_net_bind and net_throw_new / socket_error in runtime/jabs_net.c",
    ),
    (
        "classicReadTimeout",
        "NioSocketImpl's timed read: jabs_net_poll (select on Windows, poll on Linux) in runtime/jabs_net.c",
    ),
    #[cfg(windows)]
    (
        "networkInterfaces",
        "jabs_net_interfaces / ni_object (GetIfTable2, ConvertInterfaceLuidToNameA, GetAdaptersAddresses) and jabs_net_interface_mac / is_loopback in runtime/jabs_net.c",
    ),
    (
        "nioFiles",
        "the java.nio.file natives in runtime/jabs_nio_fs.c (WindowsNativeDispatcher / UnixNativeDispatcher bodies in src/cgen/nio_fs.rs)",
    ),
];

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

/// Trace every mechanism on the JVM (classes loaded, methods executed), then build the fixture
/// with those as the server's build has them.
fn build() -> Option<Built> {
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
        eprintln!("skipping {FIXTURE} behavior tests; missing {missing:?}");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let classes = dir.path().join("classes");
    fs::create_dir_all(&classes).unwrap();
    run(Command::new("javac")
        .args(["--release", "8", "-nowarn", "-Xlint:-options", "-d"])
        .arg(&classes)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)));
    let agent = build_agent(dir.path());
    let mut executed = BTreeSet::new();
    let mut loaded = BTreeSet::new();
    for (mechanism, _) in MECHANISMS {
        let trace = dir.path().join(format!("{mechanism}.trace"));
        let log = dir.path().join(format!("{mechanism}.classes"));
        let (_, stderr, status) = watched(
            Command::new("java")
                .arg(format!(
                    "-agentpath:{}={}",
                    agent.display(),
                    trace.display()
                ))
                .arg(format!("-Xlog:class+load:file={}", log.display()))
                .arg("-cp")
                .arg(&classes)
                .args([MAIN, mechanism]),
        );
        assert!(
            status.success(),
            "the traced JVM run of {mechanism} failed:\n{}",
            String::from_utf8_lossy(&stderr)
        );
        let methods = jabs::jar::read_method_trace(&trace)
            .unwrap_or_else(|e| panic!("the trace of {mechanism} is unreadable: {e:#}"));
        executed.extend(methods.methods);
        loaded.extend(loaded_classes(&log));
    }
    // java.home names the JDK compiled in, as `jabs compile --jdk` sets it: the security
    // providers (SecureRandom under the selector and the resolver) read conf/security there.
    let home = jabs::jdk::java_home(Path::new("java")).unwrap();
    let extras = jabs::cgen::UnitExtras {
        startup: true,
        java_home: Some(home.display().to_string()),
        executed_methods: executed,
        loaded_classes: loaded,
        ..Default::default()
    };
    build_native(FIXTURE, MAIN, "", None, extras)
}

/// The internal names a `-Xlog:class+load` file lists ("[...][class,load] a.B source: ..."),
/// hidden classes (whose names carry "/0x...") left out, as the server's list leaves them.
fn loaded_classes(log: &Path) -> BTreeSet<String> {
    let text = fs::read_to_string(log)
        .unwrap_or_else(|e| panic!("cannot read the class-load log {}: {e}", log.display()));
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let Some(at) = line.find("[class,load] ") else {
            continue;
        };
        let rest = &line[at + "[class,load] ".len()..];
        let Some(name) = rest.split(' ').next() else {
            continue;
        };
        if name.contains('/') {
            continue;
        }
        names.insert(name.replace('.', "/"));
    }
    assert!(
        !names.is_empty(),
        "no classes in the class-load log {}",
        log.display()
    );
    names
}

fn check(mechanism: &str) {
    let Some((_, look)) = MECHANISMS.iter().find(|(m, _)| *m == mechanism) else {
        panic!("{mechanism} is not in MECHANISMS (tests/networking.rs)");
    };
    same_as_jvm(built(), mechanism, look);
}

// ---- The resolver ----

#[test]
fn local_host_name() {
    check("localHostName");
}

#[test]
fn localhost_by_name() {
    check("localhostByName");
}

#[test]
fn own_addresses() {
    check("ownAddresses");
}

#[test]
fn unknown_host() {
    check("unknownHost");
}

#[test]
fn literal_addresses() {
    check("literalAddresses");
}

#[test]
fn host_by_address() {
    check("hostByAddress");
}

// ---- NIO sockets ----

#[test]
fn blocking_echo() {
    check("blockingEcho");
}

#[test]
fn bind_in_use() {
    check("bindInUse");
}

#[test]
fn connect_refused() {
    check("connectRefused");
}

#[test]
fn shutdown_output_is_end_of_stream() {
    check("shutdownOutputIsEndOfStream");
}

#[test]
fn socket_options() {
    check("socketOptions");
}

// ---- The selector ----

#[test]
fn selector_echo() {
    check("selectorEcho");
}

#[test]
fn selector_wakeup() {
    check("selectorWakeup");
}

// ---- java.net.Socket and ServerSocket ----

#[test]
fn classic_echo() {
    check("classicEcho");
}

#[test]
fn classic_bind_in_use() {
    check("classicBindInUse");
}

#[test]
fn classic_read_timeout() {
    check("classicReadTimeout");
}

// ---- NetworkInterface (Windows: SecureRandom's seed hashes the list) and java.nio.file ----

#[cfg(windows)]
#[test]
fn network_interfaces() {
    check("networkInterfaces");
}

#[test]
fn nio_files() {
    check("nioFiles");
}
