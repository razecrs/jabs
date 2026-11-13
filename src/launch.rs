//! `jabs run` for the JVM backend: verified artifacts, explicit Java version, measured startup.

use crate::bundler;
use crate::profile::{self, Backend, LoaderKind, Optimization, Role, Side};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub struct RunOptions {
    /// Stop the server (via its `stop` command) as soon as it reports ready.
    pub until_ready: bool,
    /// Kill the game if it has not exited this long after launch.
    pub timeout: Duration,
    /// Build a missing AOT cache during this run (only `jabs prepare` sets this).
    pub train_aot: bool,
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub profile: String,
    pub minecraft_version: String,
    pub backend: &'static str,
    pub side: Side,
    pub java: String,
    pub java_major: u32,
    pub main_class: String,
    pub working_directory: PathBuf,
    /// Wall-clock milliseconds from process spawn to the server's `Done (...)` line.
    pub ready_ms: Option<u64>,
    pub ready_line: Option<String>,
    pub stopped_by_jabs: bool,
    pub killed_after_timeout: bool,
    /// What each requested optimization actually did on this run.
    pub optimizations: Vec<OptimizationOutcome>,
    pub exit_code: Option<i32>,
    pub total_ms: u64,
    pub measurement: &'static str,
}

#[derive(Debug, Serialize)]
pub struct OptimizationOutcome {
    pub name: Optimization,
    pub status: String,
}

/// Full `java -version` text, part of the AOT cache key (a cache only works with the exact JDK build).
fn java_identity(java: &Path) -> Result<String> {
    let output = Command::new(java).arg("-version").output()?;
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

/// Major version of a Java executable, from `java.specification.version` (`1.8` -> 8, `25` -> 25).
pub fn java_major(java: &Path) -> Result<u32> {
    let output = Command::new(java)
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .with_context(|| format!("run {}", java.display()))?;
    let text = String::from_utf8_lossy(&output.stderr);
    let Some(value) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("java.specification.version = "))
    else {
        bail!(
            "{} did not report java.specification.version",
            java.display()
        );
    };
    let value = value.trim();
    let major = value.strip_prefix("1.").unwrap_or(value);
    major
        .parse()
        .with_context(|| format!("unrecognized Java version {value}"))
}

/// `Main-Class` from a JAR manifest, joining the format's 72-column continuation lines.
pub fn manifest_main_class(jar: &Path) -> Result<String> {
    let mut archive =
        zip::ZipArchive::new(File::open(jar).with_context(|| format!("open {}", jar.display()))?)?;
    let mut text = String::new();
    archive
        .by_name("META-INF/MANIFEST.MF")
        .context("game artifact has no META-INF/MANIFEST.MF")?
        .take(1024 * 1024)
        .read_to_string(&mut text)?;
    let mut joined: Vec<String> = Vec::new();
    for line in text.lines() {
        match (line.strip_prefix(' '), joined.last_mut()) {
            (Some(rest), Some(previous)) => previous.push_str(rest),
            _ => joined.push(line.to_string()),
        }
    }
    joined
        .iter()
        .find_map(|line| {
            line.strip_prefix("Main-Class:")
                .map(|v| v.trim().to_string())
        })
        .filter(|v| !v.is_empty())
        .context("game artifact manifest has no Main-Class; set launch.main_class")
}

/// Whether the server's `eula.txt` records acceptance. JABS never writes this file.
fn eula_accepted(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("eula.txt"))
        .map(|text| {
            text.lines()
                .any(|l| l.trim().eq_ignore_ascii_case("eula=true"))
        })
        .unwrap_or(false)
}

/// The server's readiness line, e.g. `Done (2.345s)! For help, type "help"`.
fn is_ready_line(line: &str) -> bool {
    line.contains("Done (") && line.contains("For help")
}

/// Verify the profile, launch it on the JVM, forward its output to stderr and report timings.
pub fn run(profile_path: &Path, options: &RunOptions) -> Result<RunReport> {
    // Rehash every artifact first: a launch never uses unverified files.
    profile::inspect(profile_path)?;
    let profile = profile::load(profile_path)?;
    if !matches!(profile.backend, Backend::Jvm) {
        bail!("the native backend cannot run games yet; set \"backend\": \"jvm\"");
    }
    if profile.loader.kind != LoaderKind::Vanilla {
        bail!("launching through a mod loader is not implemented yet; only vanilla profiles run");
    }
    let launch = profile
        .launch
        .clone()
        .context("profile has no \"launch\" section; it is analysis-only")?;
    if launch.side == Side::Client {
        bail!(
            "client launch is not implemented yet: it needs the asset index, natives and account sign-in"
        );
    }
    let base = profile_path.parent().unwrap_or(Path::new("."));
    let working_directory = match &launch.working_directory {
        Some(dir) => base.join(dir),
        None => {
            if !profile
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                || profile.name.starts_with('.')
            {
                bail!(
                    "profile name {:?} is not a safe directory name; set launch.working_directory",
                    profile.name
                );
            }
            base.join("run").join(&profile.name)
        }
    };
    std::fs::create_dir_all(&working_directory)
        .with_context(|| format!("create {}", working_directory.display()))?;
    if launch.side == Side::Server && !eula_accepted(&working_directory) {
        bail!(
            "the Minecraft EULA has not been accepted for this server. Read https://aka.ms/MinecraftEULA, \
             then put `eula=true` in {}",
            working_directory.join("eula.txt").display()
        );
    }

    let java = launch.java.clone().map(|j| {
        if j.components().count() > 1 {
            base.join(j)
        } else {
            j
        }
    });
    let java = java.unwrap_or_else(|| PathBuf::from("java"));
    let major = java_major(&java)?;
    if major != profile.java_major {
        bail!(
            "{} is Java {major} but the profile requires Java {}; set launch.java to a matching installation",
            java.display(),
            profile.java_major
        );
    }
    let artifact_path = |role: Role| {
        profile
            .artifacts
            .iter()
            .filter(move |a| a.role == role)
            .map(|a| base.join(&a.path))
    };
    let game = artifact_path(Role::Game)
        .next()
        .context("profile has no game artifact")?;
    let wants = |o: Optimization| launch.optimizations.contains(&o);
    // Absolute: the JVM resolves paths from the game's working directory, not ours.
    let jabs_dir = std::path::absolute(working_directory.join(".jabs"))?;
    let mut outcomes = Vec::new();
    let mut main_class = match &launch.main_class {
        Some(main) => main.clone(),
        None => manifest_main_class(&game)?,
    };
    // Absolute paths: the game runs in its own working directory.
    let mut game_jars = vec![std::path::absolute(&game)?];
    if wants(Optimization::StripSignatures) && !wants(Optimization::Unbundle) {
        bail!(
            "strip_signatures needs unbundle (signatures are stripped from the unbundled copies)"
        );
    }
    if wants(Optimization::Unbundle) {
        if bundler::is_bundler(&game)? {
            let sha = &profile
                .artifacts
                .iter()
                .find(|a| a.role == Role::Game)
                .unwrap()
                .sha256;
            let strip = wants(Optimization::StripSignatures);
            let unbundled = bundler::unbundle(&game, sha, &jabs_dir.join("unbundled"), strip)?;
            if launch.main_class.is_none() {
                main_class = unbundled.main_class;
            }
            game_jars = unbundled.classpath;
            let how = if unbundled.extracted {
                "extracted and verified"
            } else {
                "verified and reused"
            };
            outcomes.push(OptimizationOutcome {
                name: Optimization::Unbundle,
                status: how.into(),
            });
            if strip {
                outcomes.push(OptimizationOutcome {
                    name: Optimization::StripSignatures,
                    status: "game jars run from local unsigned copies".into(),
                });
            }
        } else {
            outcomes.push(OptimizationOutcome {
                name: Optimization::Unbundle,
                status: "skipped: game artifact is not a bundler jar".into(),
            });
        }
    }
    let classpath: Vec<PathBuf> = game_jars
        .into_iter()
        .chain(
            artifact_path(Role::Library)
                .chain(artifact_path(Role::Loader))
                .map(|p| std::path::absolute(&p).unwrap_or(p)),
        )
        .collect();

    let mut aot_args = Vec::new();
    let mut aot_training: Option<(PathBuf, PathBuf)> = None;
    if wants(Optimization::AotCache) {
        if major < 25 {
            bail!(
                "aot_cache needs Java 25+ (single-step AOT training, JEP 514); this profile uses Java {major}"
            );
        }
        // The cache is only valid for the exact JDK, JVM flags, main class and classpath contents.
        let mut key = Sha256::new();
        key.update(java_identity(&java)?);
        key.update(launch.jvm_args.join("\n"));
        key.update(&main_class);
        for jar in &classpath {
            key.update(profile::hash_file(jar)?.1);
        }
        let key = format!("{:x}", key.finalize());
        let cache = jabs_dir.join("aot").join(format!("{key}.aot"));
        fs::create_dir_all(cache.parent().unwrap())?;
        if cache.exists() {
            aot_args.push(format!("-XX:AOTCache={}", cache.display()));
            outcomes.push(OptimizationOutcome {
                name: Optimization::AotCache,
                status: "used existing cache".into(),
            });
        } else if options.train_aot {
            let partial = cache.with_file_name(format!("{key}.training.aot"));
            aot_args.push(format!("-XX:AOTCacheOutput={}", partial.display()));
            aot_training = Some((partial, cache));
        } else {
            // Never train implicitly: a training start is several times slower than a normal one.
            outcomes.push(OptimizationOutcome {
                name: Optimization::AotCache,
                status: "no cache for this exact JDK and classpath; started without it (`jabs prepare` builds it)".into(),
            });
        }
    }
    let classpath = std::env::join_paths(&classpath)
        .context("artifact path contains the classpath separator")?;

    let mut command = Command::new(&java);
    command
        .args(&launch.jvm_args)
        .args(&aot_args)
        .arg("-cp")
        .arg(&classpath)
        .arg(&main_class)
        .args(&launch.args)
        .current_dir(&working_directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if options.until_ready {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    let started = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", java.display()))?;

    // Both streams go to our stderr so stdout stays a clean JSON report; stdout lines are also checked for readiness.
    let (lines_tx, lines) = mpsc::channel::<String>();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let forward_out = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            eprintln!("{line}");
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let forward_err = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("{line}");
        }
    });

    let (mut ready_ms, mut ready_line, mut stopped_by_jabs, mut killed) =
        (None, None, false, false);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > options.timeout {
            child.kill().ok();
            killed = true;
            break child.wait()?;
        }
        match lines.recv_timeout(Duration::from_millis(50)) {
            Ok(line) if ready_ms.is_none() && is_ready_line(&line) => {
                ready_ms = Some(started.elapsed().as_millis() as u64);
                ready_line = Some(line);
                if options.until_ready
                    && let Some(stdin) = child.stdin.as_mut()
                {
                    stdin.write_all(b"stop\n")?;
                    stdin.flush()?;
                    stopped_by_jabs = true;
                }
            }
            _ => {}
        }
    };
    let total_ms = started.elapsed().as_millis() as u64;
    // The JVM writes the cache at exit; only a clean exit publishes it.
    if let Some((partial, cache)) = aot_training {
        let status = if status.success() && partial.exists() {
            fs::rename(&partial, &cache)?;
            "trained during this run (slower); later runs use it"
        } else {
            fs::remove_file(&partial).ok();
            "training did not complete; no cache written"
        };
        outcomes.push(OptimizationOutcome {
            name: Optimization::AotCache,
            status: status.into(),
        });
    }
    forward_out.join().ok();
    forward_err.join().ok();
    // Drain readiness lines that arrived just before exit.
    for line in lines.try_iter() {
        if ready_ms.is_none() && is_ready_line(&line) {
            ready_ms = Some(total_ms);
            ready_line = Some(line);
        }
    }
    Ok(RunReport {
        schema_version: 1,
        profile: profile.name,
        minecraft_version: profile.minecraft_version,
        backend: "jvm",
        side: launch.side,
        java: java.display().to_string(),
        java_major: major,
        main_class,
        working_directory: std::path::absolute(&working_directory).unwrap_or(working_directory),
        ready_ms,
        ready_line,
        stopped_by_jabs,
        killed_after_timeout: killed,
        optimizations: outcomes,
        exit_code: status.code(),
        total_ms,
        measurement: "one run; wall clock from process spawn; disk cache and world state not controlled",
    })
}
