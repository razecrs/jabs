use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use jabs::{benchmark, preparation, profile};
use serde::Serialize;
use std::{
    fs::File,
    path::PathBuf,
    process::{Command, ExitCode},
};

#[derive(Parser)]
#[command(version, about = "JABS profile integrity and benchmark foundations")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Check pinned local artifact hashes (not runtime compatibility).
    Inspect { profile: PathBuf },
    /// Cache verified artifact metadata. Does not compile Minecraft yet.
    Prepare {
        profile: PathBuf,
        #[arg(long, default_value = ".jabs/cache")]
        cache: PathBuf,
    },
    /// Recheck source artifacts and prepared metadata without writing.
    Verify {
        profile: PathBuf,
        #[arg(long, default_value = ".jabs/cache")]
        cache: PathBuf,
    },
    /// Report host platform, tool availability and implementation limits.
    Doctor,
    /// Summarize supplied frame/tick measurements; does not run Minecraft.
    Bench { measurements: PathBuf },
    /// Compute a local artifact digest for a profile.
    Hash { file: PathBuf },
    /// Inventory a JAR: class-file versions, loader metadata, mixins, natives.
    InspectJar { jar: PathBuf },
    /// Measure how much of a JAR compiles and rank what blocks the rest.
    Coverage {
        jar: PathBuf,
        /// Also compile this Java installation's java.base in the same unit (e.g. `java`).
        #[arg(long)]
        jdk: Option<PathBuf>,
    },
    /// Compile class files (or every class in jars) together as one unit; reports every unsupported method.
    Compile {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Also put this Java installation's java.base in the unit; only code reachable from the inputs is emitted.
        #[arg(long)]
        jdk: Option<PathBuf>,
        /// Write the program as one C file.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write the program as a header plus many C files, for parallel compilation.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Emit only code reachable from these classes (internal names; default: every input class).
        #[arg(long)]
        root: Vec<String>,
    },
    /// Download (verified against Mojang's hashes) and launch a vanilla client on the JVM, offline sign-in.
    Client {
        /// Minecraft version id, e.g. `1.21.1`.
        version: String,
        /// Offline player name (servers must run with `online-mode=false`).
        #[arg(long)]
        username: String,
        /// Join this server on startup (`host[:port]`).
        #[arg(long)]
        server: Option<String>,
        /// Where versions, libraries and assets are stored.
        #[arg(long, default_value = "local/minecraft")]
        root: PathBuf,
        /// Game directory (saves, options, logs); default `<root>/instances/<version>`.
        #[arg(long)]
        game_dir: Option<PathBuf>,
        /// Java executable; default: the first installed Java matching the version's requirement.
        #[arg(long)]
        java: Option<PathBuf>,
        #[arg(long, default_value = "4G")]
        max_memory: String,
        /// Only download and verify; do not launch.
        #[arg(long)]
        install_only: bool,
    },
    /// Launch a profile on the JVM after verifying its artifacts; prints a JSON timing report.
    Run {
        profile: PathBuf,
        /// Stop the server once it reports ready (startup measurement).
        #[arg(long)]
        until_ready: bool,
        /// Kill the game if still running after this many seconds.
        #[arg(long, default_value_t = 1800)]
        timeout_secs: u64,
    },
}

fn print_json(value: impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Run one CLI subcommand.
fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Action::Inspect { profile: path } => print_json(profile::inspect(&path)?),
        Action::Prepare { profile, cache } => {
            let integrity = preparation::prepare(&profile, &cache, false)?;
            // Launch caches (the JVM AOT cache) are built here so `run` never pays for training.
            let wants_aot = profile::load(&profile)?
                .launch
                .is_some_and(|l| l.optimizations.contains(&profile::Optimization::AotCache));
            let training = if wants_aot {
                let options = jabs::launch::RunOptions {
                    until_ready: true,
                    timeout: std::time::Duration::from_secs(1800),
                    train_aot: true,
                };
                Some(jabs::launch::run(&profile, &options)?)
            } else {
                None
            };
            print_json(serde_json::json!({ "integrity": integrity, "launch_training": training }))
        }
        Action::Verify { profile, cache } => {
            print_json(preparation::prepare(&profile, &cache, true)?)
        }
        Action::InspectJar { jar } => print_json(jabs::jar::inspect_jar(&jar)?),
        Action::Coverage { jar, jdk } => print_json(jabs::jar::coverage(&jar, jdk.as_deref())?),
        Action::Compile {
            inputs,
            jdk,
            out,
            out_dir,
            root,
        } => {
            let mut classes = jabs::jar::load_classes(&inputs)?;
            let roots: std::collections::BTreeSet<String> =
                classes.iter().map(|c| c.name.clone()).collect();
            let inputs_named = roots.clone();
            let roots: std::collections::BTreeSet<String> = if root.is_empty() {
                roots
            } else {
                root.into_iter().collect()
            };
            if let Some(java) = jdk {
                let home = jabs::jdk::java_home(&java)?;
                let (library, _, _) = jabs::jdk::read_module(&home, "java.base")?;
                classes.extend(
                    library
                        .into_iter()
                        .filter(|c| !inputs_named.contains(&c.name)),
                );
            }
            let compiled = jabs::cgen::compile_unit_rooted(&classes, Some(&roots));
            if let Some(out) = out {
                std::fs::write(&out, &compiled.c_source)
                    .with_context(|| format!("write {}", out.display()))?;
            }
            if let Some(dir) = out_dir {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("create {}", dir.display()))?;
                for (name, contents) in &compiled.c_files {
                    std::fs::write(dir.join(name), contents)
                        .with_context(|| format!("write {}", dir.join(name).display()))?;
                }
            }
            print_json(compiled)
        }
        Action::Hash { file } => {
            let (bytes, sha256) = profile::hash_file(&file)?;
            print_json(serde_json::json!({ "bytes": bytes, "sha256": sha256 }))
        }
        Action::Bench { measurements } => {
            let data = serde_json::from_reader(
                File::open(&measurements)
                    .with_context(|| format!("open {}", measurements.display()))?,
            )
            .context("invalid measurements JSON")?;
            print_json(benchmark::summarize(data)?)
        }
        Action::Run {
            profile,
            until_ready,
            timeout_secs,
        } => {
            let options = jabs::launch::RunOptions {
                until_ready,
                timeout: std::time::Duration::from_secs(timeout_secs),
                train_aot: false,
            };
            print_json(jabs::launch::run(&profile, &options)?)
        }
        Action::Client {
            version,
            username,
            server,
            root,
            game_dir,
            java,
            max_memory,
            install_only,
        } => {
            if install_only {
                let report = jabs::client::install(&root, &version)?;
                eprintln!(
                    "{} installed ({} files downloaded); needs Java {}",
                    report.version, report.downloaded, report.java_major
                );
                return Ok(());
            }
            let code = jabs::client::run(&jabs::client::ClientOptions {
                root: &root,
                version: &version,
                username: &username,
                game_dir: game_dir.as_deref(),
                server: server.as_deref(),
                java: java.as_deref(),
                max_memory: &max_memory,
            })?;
            eprintln!("Minecraft exited with code {code}");
            Ok(())
        }
        Action::Doctor => {
            let tools: Vec<_> = [("java", "-version"), ("javac", "-version"), ("clang", "--version"), ("rustc", "--version")]
                .into_iter().map(|(name, arg)| {
                    match Command::new(name).arg(arg).output() {
                        Ok(output) => {
                            let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                            serde_json::json!({ "tool": name, "available": output.status.success(), "version_output": text.trim() })
                        },
                        Err(error) => serde_json::json!({ "tool": name, "available": false, "error": error.to_string() }),
                    }
                }).collect();
            print_json(
                serde_json::json!({ "schema_version": 1, "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH, "tools": tools,
                "capabilities": ["profile_integrity", "metadata_cache", "measurement_summary", "jar_inventory", "single_class_compilation_with_objects", "vanilla_server_launch"],
                "unimplemented": ["mod_loader_launch", "class_loaders", "threads", "unsafe", "invokedynamic", "cross_class_compilation", "arrays", "garbage_collection", "exception_handlers", "benchmark_capture"],
                "note": "tool discovery does not establish target or Java-version compatibility" }),
            )
        }
    }
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
