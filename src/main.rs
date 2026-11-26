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
        /// Classes a JVM run loaded (one internal name per line, from `-Xlog:class+load`): also
        /// report the counts over only those classes.
        #[arg(long)]
        loaded_classes: Option<PathBuf>,
        /// A method trace from a JVM run with `tools/method-trace` attached: also report what
        /// that run executed that JABS does not compile.
        #[arg(long)]
        executed_methods: Option<PathBuf>,
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
        /// Classes a JVM run loaded (one internal name per line, from `-Xlog:class+load`): they
        /// may be loaded by name, so they are kept with their initializers and no-arg constructors.
        #[arg(long)]
        loaded_classes: Option<PathBuf>,
        /// A whole method trace from a JVM run with `tools/method-trace` attached: every traced
        /// method the unit compiles is kept, since code may call it only by a computed name.
        #[arg(long)]
        executed_methods: Option<PathBuf>,
    },
    /// Download (verified against Mojang's hashes) and launch a vanilla client on the JVM, offline sign-in.
    Client {
        /// Minecraft version id, e.g. `1.21.1` (or omit and pass --instance).
        version: Option<String>,
        /// A CurseForge instance folder: its version, mod loader, memory and game directory are used.
        #[arg(long)]
        instance: Option<PathBuf>,
        /// The install root that holds the instance's mod loader (default: CurseForge's, found automatically).
        #[arg(long)]
        loader_root: Option<PathBuf>,
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
        /// Maximum heap, e.g. `4G` (default: the instance's setting, else 4G).
        #[arg(long)]
        max_memory: Option<String>,
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
        Action::Coverage {
            jar,
            jdk,
            loaded_classes,
            executed_methods,
        } => {
            let loaded = match loaded_classes {
                Some(path) => Some(jabs::jar::read_class_list(&path)?),
                None => None,
            };
            let executed = match executed_methods {
                Some(path) => Some(jabs::jar::read_method_trace(&path)?),
                None => None,
            };
            print_json(jabs::jar::coverage_with(
                &jar,
                jdk.as_deref(),
                loaded.as_ref(),
                executed.as_ref(),
            )?)
        }
        Action::Compile {
            inputs,
            jdk,
            out,
            out_dir,
            root,
            loaded_classes,
            executed_methods,
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
            let loaded_classes = match loaded_classes {
                Some(path) => jabs::jar::read_class_list(&path)?,
                None => std::collections::BTreeSet::new(),
            };
            // Read strictly: a trace cut short would silently leave out what the run did last.
            let trace = match executed_methods {
                Some(path) => Some(jabs::jar::read_method_trace(&path)?),
                None => None,
            };
            let mut java_home = None;
            if let Some(java) = jdk {
                let home = jabs::jdk::java_home(&java)?;
                java_home = Some(home.clone());
                // java.base, plus every module a JVM run of the program loaded classes from or
                // executed code of (as `coverage` counts them).
                let mut used = loaded_classes.clone();
                if let Some(trace) = &trace {
                    used.extend(trace.methods.iter().map(|(class, _, _)| class.clone()));
                }
                let mut modules = vec!["java.base".to_string()];
                for m in jabs::jdk::modules_containing(&home, &used)? {
                    if !modules.contains(&m) {
                        modules.push(m);
                    }
                }
                eprintln!("JDK modules in the unit: {}", modules.join(", "));
                for module in &modules {
                    let (library, _, _) = jabs::jdk::read_module(&home, module)?;
                    let known: std::collections::HashSet<String> =
                        classes.iter().map(|c| c.name.clone()).collect();
                    classes.extend(library.into_iter().filter(|c| !known.contains(&c.name)));
                }
            }
            // Input classes belong to the app class loader; their jars' other files are its resources.
            let extras = jabs::cgen::UnitExtras {
                app_classes: inputs_named.clone(),
                resources: jabs::jar::load_resources(&inputs)?,
                java_home: java_home.map(|h| h.display().to_string()),
                loaded_classes,
                executed_methods: trace
                    .as_ref()
                    .map(|t| t.methods.clone())
                    .unwrap_or_default(),
                startup: false,
            };
            let compiled = jabs::cgen::compile_unit_with(&classes, Some(&roots), &extras);
            if let (Some(trace), Some(counts)) = (&trace, &compiled.executed_roots) {
                eprintln!(
                    "executed methods: {} roots; set aside: {} in hidden classes, {} of classes \
                     outside the unit, {} missing from their class, {} abstract, {} not compiled",
                    counts.roots,
                    trace.hidden_class_methods,
                    counts.class_not_in_unit,
                    counts.method_not_in_class,
                    counts.abstract_in_unit,
                    counts.not_compiled
                );
            }
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
            instance,
            loader_root,
        } => {
            let cf = instance
                .as_deref()
                .map(|dir| {
                    jabs::client::curseforge_instance_with_loader_root(dir, loader_root.as_deref())
                })
                .transpose()?;
            let version = match (&cf, version) {
                (Some(cf), None) => cf.version.clone(),
                (None, Some(v)) => v,
                _ => anyhow::bail!("pass either a version or --instance"),
            };
            let loader_root =
                loader_root.or_else(|| cf.as_ref().and_then(|c| c.loader_root.clone()));
            let game_dir = game_dir.or_else(|| instance.clone());
            let max_memory = max_memory
                .or_else(|| cf.as_ref().map(|c| c.max_memory.clone()))
                .unwrap_or_else(|| "4G".into());
            if install_only {
                let base = cf
                    .as_ref()
                    .map_or(version.as_str(), |c| c.game_version.as_str());
                let report = jabs::client::install(&root, base)?;
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
                loader_root: loader_root.as_deref(),
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
                "capabilities": ["profile_integrity", "metadata_cache", "measurement_summary", "jar_inventory", "unit_compilation", "objects", "arrays", "exception_handlers", "synchronized_single_threaded", "invokedynamic_lambdas_concat_records_switches", "java_base_compilation", "coverage_blast_radius", "vanilla_server_launch", "vanilla_client_launch", "mod_loader_launch_on_jvm"],
                "partial": ["unsafe", "reflection", "class_loaders"],
                "unimplemented": ["threads", "garbage_collection", "runtime_class_definition", "native_minecraft_execution", "benchmark_capture"],
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
