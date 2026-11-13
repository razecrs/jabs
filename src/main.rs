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
    Coverage { jar: PathBuf },
    /// Compile class files (or every class in jars) together as one unit; reports every unsupported method.
    Compile {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
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
        Action::Coverage { jar } => print_json(jabs::jar::coverage(&jar)?),
        Action::Compile { inputs, out } => {
            let classes = jabs::jar::load_classes(&inputs)?;
            let compiled = jabs::cgen::compile_unit(&classes);
            if let Some(out) = out {
                std::fs::write(&out, &compiled.c_source)
                    .with_context(|| format!("write {}", out.display()))?;
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
                "unimplemented": ["client_launch", "mod_loader_launch", "cross_class_compilation", "arrays", "garbage_collection", "exception_handlers", "benchmark_capture"],
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
