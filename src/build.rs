//! Parallel, PCH-enabled, cached C11 build driver for JABS-generated `out_dir` programs.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    cmp::Reverse,
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};

/// C optimization level for a translation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum OptLevel {
    O0,
    O1,
    #[default]
    O2,
    O3,
}

impl OptLevel {
    pub fn flag(self) -> &'static str {
        match self {
            Self::O0 => "-O0",
            Self::O1 => "-O1",
            Self::O2 => "-O2",
            Self::O3 => "-O3",
        }
    }

    /// Java floating-point semantics require disabling FMA contraction at `-O1` and above.
    pub fn needs_fp_contract_off(self) -> bool {
        !matches!(self, Self::O0)
    }
}

/// Optimization tier of a translation unit inside `out_dir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuTier {
    /// Pure static data tables (`resources.c`, `unit_0_records*.c`, `unit_0_reflect*.c`, `unit_0_modules*.c`).
    Data,
    /// Cold one-shot initialization and stub TUs (`unit_0_inits*.c`, `unit_0_stubs*.c`).
    Cold,
    /// Hot generated Java methods, runtime C files, and entry point (`unit_*.c`, `jabs_*.c`, `entry.c`).
    Hot,
}

/// Configuration for [`build_out_dir`].
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Optimization level for hot code and runtime TUs (default: `OptLevel::O2`).
    pub opt_level: OptLevel,
    /// Optimization level for pure-data TUs (default: `OptLevel::O0`).
    pub data_opt_level: OptLevel,
    /// Optimization level for cold one-shot TUs (default: `OptLevel::O1`).
    pub cold_opt_level: OptLevel,
    /// Output binary path (default: `<out_dir>/program.exe` on Windows, `<out_dir>/program` elsewhere).
    pub output: Option<PathBuf>,
    /// Optional content-addressed `.o` cache directory shared across builds.
    pub cache_dir: Option<PathBuf>,
    /// Maximum parallel compile workers (default: host parallelism).
    pub jobs: Option<usize>,
    /// Precompile `jabs_unit.h` when it carries a `JABS_UNIT_H` include guard (default: `true`).
    pub use_pch: bool,
    /// Compile with `-Wall -Wextra -Werror` (plus unused function/label suppressions) (default: `true`).
    pub strict_warnings: bool,
    /// Extra flags appended to every C compile command.
    pub extra_cflags: Vec<String>,
    /// Extra flags appended to the link command.
    pub extra_ldflags: Vec<String>,
    /// Clang executable name or path (default: `"clang"`).
    pub clang: PathBuf,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            opt_level: OptLevel::O2,
            data_opt_level: OptLevel::O0,
            cold_opt_level: OptLevel::O1,
            output: None,
            cache_dir: None,
            jobs: None,
            use_pch: true,
            strict_warnings: true,
            extra_cflags: Vec::new(),
            extra_ldflags: Vec::new(),
            clang: PathBuf::from("clang"),
        }
    }
}

/// Summary of a native build produced by [`build_out_dir`].
#[derive(Debug, Clone, Serialize)]
pub struct BuildReport {
    pub output: PathBuf,
    pub output_bytes: u64,
    pub target_args: Vec<String>,
    pub used_lld: bool,
    pub used_pch: bool,
    pub compiled: usize,
    pub cached: usize,
    pub total_sources: usize,
    pub jobs: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone)]
struct CompileTask {
    file_name: String,
    source: PathBuf,
    object: PathBuf,
    tier: TuTier,
    opt_level: OptLevel,
    use_pch: bool,
    size_bytes: u64,
    cache_key: Option<String>,
}

/// Classify a `.c` filename into its optimization tier.
pub fn classify_tu(file_name: &str) -> TuTier {
    if file_name == "resources.c"
        || file_name.starts_with("unit_0_records")
        || file_name.starts_with("unit_0_reflect")
        || file_name.starts_with("unit_0_modules")
    {
        TuTier::Data
    } else if file_name.starts_with("unit_0_inits") || file_name.starts_with("unit_0_stubs") {
        TuTier::Cold
    } else {
        TuTier::Hot
    }
}

/// Whether `file_name` is a handwritten runtime TU (`jabs_*.c`), which sets OS/POSIX macros or
/// includes platform headers before `"jabs_unit.h"` and therefore must never receive `-include-pch`.
pub fn is_runtime_tu(file_name: &str) -> bool {
    file_name.starts_with("jabs_")
}

/// Detect whether `jabs_unit.h` has a full-file include guard (`#ifndef JABS_UNIT_H` or `#pragma once`)
/// before any embedded `#ifndef JABS_RT_H` block.
pub fn has_unit_header_guard(header: &str) -> bool {
    let mut saw_ifndef = false;
    for line in header.lines() {
        let trimmed = line.trim();
        if trimmed == "#pragma once" {
            return true;
        }
        if trimmed == "#ifndef JABS_UNIT_H" || trimmed == "#if !defined(JABS_UNIT_H)" {
            saw_ifndef = true;
        } else if saw_ifndef && trimmed == "#define JABS_UNIT_H" {
            return true;
        } else if trimmed.starts_with("#ifndef JABS_RT_H") {
            return false;
        }
    }
    false
}

/// Compile and link all C translation units in `out_dir` (plus an optional `entry_c` outside `out_dir`)
/// in parallel using LPT scheduling, tiered optimization, optional PCH, and an optional `.o` cache.
pub fn build_out_dir(
    out_dir: &Path,
    entry_c: Option<&Path>,
    options: &BuildOptions,
) -> Result<BuildReport> {
    let started = Instant::now();
    if !out_dir.is_dir() {
        bail!("output directory does not exist: {}", out_dir.display());
    }

    let clang_version = query_clang_version(&options.clang)?;
    let (target_args, used_lld) = probe_toolchain(&options.clang, out_dir)?;
    let sources = collect_sources(out_dir, entry_c)?;
    if sources.is_empty() {
        bail!("no .c source files found in {}", out_dir.display());
    }

    let unit_h_path = out_dir.join("jabs_unit.h");
    let (unit_header_hash, can_pch) = if unit_h_path.is_file() {
        let header_bytes =
            fs::read(&unit_h_path).with_context(|| format!("reading {}", unit_h_path.display()))?;
        let hash = sha256_bytes(&header_bytes);
        let guarded =
            options.use_pch && std::str::from_utf8(&header_bytes).is_ok_and(has_unit_header_guard);
        (Some(hash), guarded)
    } else {
        (None, false)
    };
    let runtime_h_path = out_dir.join("jabs_runtime.h");
    let runtime_header_hash = if runtime_h_path.is_file() {
        Some(sha256_bytes(&fs::read(&runtime_h_path).with_context(
            || format!("reading {}", runtime_h_path.display()),
        )?))
    } else {
        None
    };

    if let Some(cache_dir) = &options.cache_dir {
        fs::create_dir_all(cache_dir)
            .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
    }

    let mut tasks = Vec::with_capacity(sources.len());
    for (file_name, source, object) in sources {
        let tier = classify_tu(&file_name);
        let opt_level = match tier {
            TuTier::Data => options.data_opt_level,
            TuTier::Cold => options.cold_opt_level,
            TuTier::Hot => options.opt_level,
        };
        // Clang rejects -include-pch when the TU's optimization level differs from the PCH's.
        let use_pch = can_pch && !is_runtime_tu(&file_name) && opt_level == options.opt_level;
        let size_bytes = fs::metadata(&source)
            .with_context(|| format!("stat {}", source.display()))?
            .len();

        let header_hash_for_tu = if (is_runtime_tu(&file_name) || file_name == "resources.c")
            && runtime_header_hash.is_some()
        {
            runtime_header_hash.as_ref()
        } else {
            unit_header_hash.as_ref()
        };

        let cache_key = if options.cache_dir.is_some() {
            let src_bytes =
                fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
            let flags = compile_flags(opt_level, options);
            Some(compute_cache_key(
                &clang_version,
                &target_args,
                &flags,
                use_pch,
                header_hash_for_tu,
                &src_bytes,
            ))
        } else {
            None
        };

        tasks.push(CompileTask {
            file_name,
            source,
            object,
            tier,
            opt_level,
            use_pch,
            size_bytes,
            cache_key,
        });
    }

    let total_sources = tasks.len();
    let all_objects: Vec<PathBuf> = tasks.iter().map(|t| t.object.clone()).collect();

    let mut compile_queue = Vec::new();
    let mut cached_count = 0usize;
    for task in tasks {
        if let (Some(cache_dir), Some(key)) = (&options.cache_dir, &task.cache_key) {
            let cached_obj = cache_dir.join(format!("{key}.o"));
            if cached_obj.is_file() && fs::copy(&cached_obj, &task.object).is_ok() {
                cached_count += 1;
                continue;
            }
        }
        compile_queue.push(task);
    }

    let need_pch = can_pch && compile_queue.iter().any(|t| t.use_pch);
    let pch_path = out_dir.join("jabs_unit.h.pch");
    if need_pch {
        build_pch(
            &options.clang,
            &target_args,
            out_dir,
            &unit_h_path,
            &pch_path,
            options,
        )?;
    }

    sort_compile_tasks(&mut compile_queue);

    let default_jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let jobs = options.jobs.unwrap_or(default_jobs).max(1);
    let worker_count = if compile_queue.is_empty() {
        0
    } else {
        jobs.min(compile_queue.len())
    };

    if worker_count > 0 {
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let pch_arg = need_pch.then_some(pch_path.as_path());
        std::thread::scope(|scope| -> Result<()> {
            let handles: Vec<_> = (0..worker_count)
                .map(|worker_id| {
                    let next = &next;
                    let failed = &failed;
                    let queue = &compile_queue;
                    let target_args = &target_args;
                    scope.spawn(move || -> Result<()> {
                        loop {
                            if failed.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            let Some(task) = queue.get(idx) else {
                                return Ok(());
                            };
                            if let Err(err) = compile_tu(
                                &options.clang,
                                target_args,
                                out_dir,
                                pch_arg,
                                task,
                                options,
                                worker_id,
                            ) {
                                failed.store(true, Ordering::Relaxed);
                                return Err(err);
                            }
                        }
                    })
                })
                .collect();

            let mut first_err: Option<anyhow::Error> = None;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                    Err(_) => {
                        if first_err.is_none() {
                            first_err = Some(anyhow::anyhow!("compiler worker panicked"));
                        }
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
            Ok(())
        })?;
    }

    let output = resolve_output_path(out_dir, options.output.as_deref())?;
    link_objects(
        &options.clang,
        &target_args,
        used_lld,
        out_dir,
        &all_objects,
        &output,
        &options.extra_ldflags,
    )?;

    let output_bytes = fs::metadata(&output)
        .with_context(|| format!("stat {}", output.display()))?
        .len();

    Ok(BuildReport {
        output,
        output_bytes,
        target_args,
        used_lld,
        used_pch: need_pch,
        compiled: compile_queue.len(),
        cached: cached_count,
        total_sources,
        jobs,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Sort tasks by LPT order: non-data code TUs before data TUs, then descending by file size,
/// with deterministic filename tie-breaking.
fn sort_compile_tasks(tasks: &mut [CompileTask]) {
    tasks.sort_by(|a, b| {
        (
            a.tier == TuTier::Data,
            Reverse(a.size_bytes),
            a.file_name.as_str(),
        )
            .cmp(&(
                b.tier == TuTier::Data,
                Reverse(b.size_bytes),
                b.file_name.as_str(),
            ))
    });
}

fn compile_flags(opt_level: OptLevel, options: &BuildOptions) -> Vec<String> {
    let mut flags = vec!["-std=c11".to_string(), opt_level.flag().to_string()];
    if opt_level.needs_fp_contract_off() {
        flags.push("-ffp-contract=off".to_string());
    }
    if options.strict_warnings {
        flags.extend([
            "-Wall".to_string(),
            "-Wextra".to_string(),
            "-Werror".to_string(),
            "-Wno-unused-function".to_string(),
            "-Wno-unused-label".to_string(),
        ]);
    } else {
        flags.push("-w".to_string());
    }
    flags.push("-D_CRT_SECURE_NO_WARNINGS".to_string());
    flags.extend(options.extra_cflags.iter().cloned());
    flags
}

fn compute_cache_key(
    clang_version: &str,
    target_args: &[String],
    flags: &[String],
    use_pch: bool,
    unit_header_sha256: Option<&[u8; 32]>,
    source_bytes: &[u8],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"jabs-obj-v1\0");
    hasher.update(clang_version.as_bytes());
    hasher.update(b"\0");
    hasher.update((target_args.len() as u64).to_le_bytes());
    for arg in target_args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update((flags.len() as u64).to_le_bytes());
    for flag in flags {
        hasher.update(flag.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update([u8::from(use_pch)]);
    match unit_header_sha256 {
        Some(digest) => {
            hasher.update([1u8]);
            hasher.update(digest);
        }
        None => hasher.update([0u8]),
    }
    hasher.update((source_bytes.len() as u64).to_le_bytes());
    hasher.update(source_bytes);
    hex_lower(&hasher.finalize())
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn query_clang_version(clang: &Path) -> Result<String> {
    let output = Command::new(clang)
        .arg("--version")
        .output()
        .with_context(|| format!("spawning {} --version", clang.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version failed: {}",
            clang.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn probe_toolchain(clang: &Path, out_dir: &Path) -> Result<(Vec<String>, bool)> {
    let probe_dir = out_dir.join(".jabs-probe");
    fs::create_dir_all(&probe_dir)
        .with_context(|| format!("creating probe dir {}", probe_dir.display()))?;
    let result = probe_toolchain_in(clang, &probe_dir);
    let _ = fs::remove_dir_all(&probe_dir);
    result
}

fn probe_toolchain_in(clang: &Path, probe_dir: &Path) -> Result<(Vec<String>, bool)> {
    let probe_c = probe_dir.join("probe.c");
    let probe_exe = probe_dir.join(if cfg!(windows) { "probe.exe" } else { "probe" });
    fs::write(
        &probe_c,
        "#include <math.h>\n#include <stdio.h>\nint main(void) { return (int)fmod(1.0, 2.0); }\n",
    )
    .with_context(|| format!("writing {}", probe_c.display()))?;

    let target_args = if let Ok(args) = std::env::var("JABS_CLANG_ARGS") {
        let parsed: Vec<String> = args.split_whitespace().map(String::from).collect();
        if !parsed.is_empty() {
            parsed
        } else {
            detect_target_args(clang, &probe_c, &probe_exe)?
        }
    } else {
        detect_target_args(clang, &probe_c, &probe_exe)?
    };

    let used_lld = probe_link_ok(
        clang,
        &target_args,
        Some("-fuse-ld=lld"),
        &probe_c,
        &probe_exe,
    );
    Ok((target_args, used_lld))
}

fn detect_target_args(clang: &Path, probe_c: &Path, probe_exe: &Path) -> Result<Vec<String>> {
    let mut candidates = vec![Vec::new()];
    if cfg!(windows) {
        candidates.push(vec!["--target=x86_64-w64-windows-gnu".to_string()]);
    }
    for args in candidates {
        if probe_link_ok(clang, &args, None, probe_c, probe_exe) {
            return Ok(args);
        }
    }
    bail!(
        "{} cannot build a C program with <math.h> for any known target; set JABS_CLANG_ARGS",
        clang.display()
    );
}

fn probe_link_ok(
    clang: &Path,
    target_args: &[String],
    extra_flag: Option<&str>,
    probe_c: &Path,
    probe_exe: &Path,
) -> bool {
    let mut cmd = Command::new(clang);
    cmd.args(target_args);
    if let Some(flag) = extra_flag {
        cmd.arg(flag);
    }
    cmd.arg(probe_c).arg("-o").arg(probe_exe);
    if !cfg!(windows) {
        cmd.arg("-lm");
    }
    cmd.output().is_ok_and(|o| o.status.success())
}

fn collect_sources(
    out_dir: &Path,
    entry_c: Option<&Path>,
) -> Result<Vec<(String, PathBuf, PathBuf)>> {
    let mut entries: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    for entry in fs::read_dir(out_dir).with_context(|| format!("reading {}", out_dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "c") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        let obj = out_dir.join(format!("{}.o", name.trim_end_matches(".c")));
        seen_names.insert(name.clone());
        entries.push((name, path, obj));
    }

    if let Some(entry_path) = entry_c {
        if !entry_path.is_file() {
            bail!("entry C file does not exist: {}", entry_path.display());
        }
        let name = entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("invalid entry C filename: {}", entry_path.display()))?;
        let obj = out_dir.join(format!("{}.o", name.trim_end_matches(".c")));
        if seen_names.contains(&name) {
            // Replace any same-named entry already in out_dir so main() is never linked twice.
            entries.retain(|(existing, _, _)| existing != &name);
        }
        entries.push((name, entry_path.to_path_buf(), obj));
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn build_pch(
    clang: &Path,
    target_args: &[String],
    out_dir: &Path,
    unit_h: &Path,
    unit_pch: &Path,
    options: &BuildOptions,
) -> Result<()> {
    let flags = compile_flags(options.opt_level, options);
    let mut cmd = Command::new(clang);
    cmd.args(target_args)
        .args(&flags)
        .arg("-I")
        .arg(out_dir)
        .arg("-x")
        .arg("c-header")
        .arg(unit_h)
        .arg("-o")
        .arg(unit_pch);
    run_checked(&mut cmd, "precompiling jabs_unit.h")
}

fn compile_tu(
    clang: &Path,
    target_args: &[String],
    out_dir: &Path,
    pch_path: Option<&Path>,
    task: &CompileTask,
    options: &BuildOptions,
    worker_id: usize,
) -> Result<()> {
    let flags = compile_flags(task.opt_level, options);
    let mut cmd = Command::new(clang);
    cmd.args(target_args).args(&flags).arg("-I").arg(out_dir);
    if task.use_pch
        && let Some(pch) = pch_path
    {
        cmd.arg("-include-pch").arg(pch);
    }
    cmd.arg("-c").arg(&task.source).arg("-o").arg(&task.object);
    run_checked(&mut cmd, &format!("compiling {}", task.file_name))?;

    if let (Some(cache_dir), Some(key)) = (&options.cache_dir, &task.cache_key) {
        let final_path = cache_dir.join(format!("{key}.o"));
        if !final_path.is_file() {
            let tmp_path =
                cache_dir.join(format!("{key}.o.tmp.{}-{}", std::process::id(), worker_id));
            if fs::copy(&task.object, &tmp_path).is_ok()
                && fs::rename(&tmp_path, &final_path).is_err()
            {
                let _ = fs::remove_file(&tmp_path);
            }
        }
    }
    Ok(())
}

fn resolve_output_path(out_dir: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    let raw = match explicit {
        Some(path) => path.to_path_buf(),
        None => out_dir.join(if cfg!(windows) {
            "program.exe"
        } else {
            "program"
        }),
    };
    if raw.is_absolute() {
        Ok(raw)
    } else {
        Ok(std::env::current_dir()?.join(raw))
    }
}

fn link_objects(
    clang: &Path,
    target_args: &[String],
    used_lld: bool,
    out_dir: &Path,
    objects: &[PathBuf],
    output: &Path,
    extra_ldflags: &[String],
) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output parent dir {}", parent.display()))?;
    }

    // Use a response file with out_dir-relative names to avoid Windows CreateProcess length limits.
    let rsp_path = out_dir.join(".jabs-link.rsp");
    let mut rsp_body = String::new();
    for obj in objects {
        let rel = obj.strip_prefix(out_dir).unwrap_or(obj);
        let text = rel.to_string_lossy().replace('\\', "/");
        if text.contains(char::is_whitespace) {
            rsp_body.push('"');
            rsp_body.push_str(&text);
            rsp_body.push_str("\"\n");
        } else {
            rsp_body.push_str(&text);
            rsp_body.push('\n');
        }
    }
    fs::write(&rsp_path, rsp_body).with_context(|| format!("writing {}", rsp_path.display()))?;

    let mut cmd = Command::new(clang);
    cmd.current_dir(out_dir).args(target_args);
    if used_lld {
        cmd.arg("-fuse-ld=lld");
    }
    cmd.arg("@.jabs-link.rsp").arg("-o").arg(output);
    if cfg!(windows) {
        cmd.args(["-lws2_32", "-liphlpapi"]);
    } else {
        cmd.args(["-pthread", "-lm"]);
    }
    cmd.args(extra_ldflags);

    let link_res = run_checked(&mut cmd, "linking native binary");
    let _ = fs::remove_file(&rsp_path);
    link_res
}

fn run_checked(cmd: &mut Command, what: &str) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("{what}: failed to spawn {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{what} failed ({:?}):\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiered_classification_matches_jabs_layout() {
        assert_eq!(classify_tu("resources.c"), TuTier::Data);
        assert_eq!(classify_tu("unit_0_records.c"), TuTier::Data);
        assert_eq!(classify_tu("unit_0_records_12.c"), TuTier::Data);
        assert_eq!(classify_tu("unit_0_reflect.c"), TuTier::Data);
        assert_eq!(classify_tu("unit_0_reflect_2.c"), TuTier::Data);
        assert_eq!(classify_tu("unit_0_modules.c"), TuTier::Data);

        assert_eq!(classify_tu("unit_0_inits.c"), TuTier::Cold);
        assert_eq!(classify_tu("unit_0_inits_1.c"), TuTier::Cold);
        assert_eq!(classify_tu("unit_0_stubs.c"), TuTier::Cold);

        assert_eq!(classify_tu("unit_0.c"), TuTier::Hot);
        assert_eq!(classify_tu("unit_1.c"), TuTier::Hot);
        assert_eq!(classify_tu("jabs_platform.c"), TuTier::Hot);
        assert_eq!(classify_tu("jabs_net.c"), TuTier::Hot);
        assert_eq!(classify_tu("entry.c"), TuTier::Hot);
        assert_eq!(classify_tu("harness.c"), TuTier::Hot);

        assert!(is_runtime_tu("jabs_platform.c"));
        assert!(is_runtime_tu("jabs_net.c"));
        assert!(!is_runtime_tu("unit_1.c"));
        assert!(!is_runtime_tu("entry.c"));
        assert!(!is_runtime_tu("harness.c"));
    }

    #[test]
    fn pch_guard_detection_requires_outer_guard() {
        let guarded = "#ifndef JABS_UNIT_H\n#define JABS_UNIT_H\n#ifndef JABS_RT_H\n#define JABS_RT_H\n#endif\n#endif\n";
        assert!(has_unit_header_guard(guarded));

        let alt_guarded = "/* comment */\n#if !defined(JABS_UNIT_H)\n#define JABS_UNIT_H\n#endif\n";
        assert!(has_unit_header_guard(alt_guarded));

        let pragma_guarded = "#pragma once\n#ifndef JABS_RT_H\n#endif\n";
        assert!(has_unit_header_guard(pragma_guarded));

        let unguarded =
            "/* jabs_rt.h */\n#ifndef JABS_RT_H\n#define JABS_RT_H\n#endif\nvoid foo(void);\n";
        assert!(!has_unit_header_guard(unguarded));
        assert!(!has_unit_header_guard(""));
    }

    #[test]
    fn lpt_ordering_prioritizes_non_data_then_descending_size() {
        let mut tasks = vec![
            CompileTask {
                file_name: "resources.c".into(),
                source: PathBuf::from("resources.c"),
                object: PathBuf::from("resources.o"),
                tier: TuTier::Data,
                opt_level: OptLevel::O0,
                use_pch: false,
                size_bytes: 5_000_000,
                cache_key: None,
            },
            CompileTask {
                file_name: "unit_2.c".into(),
                source: PathBuf::from("unit_2.c"),
                object: PathBuf::from("unit_2.o"),
                tier: TuTier::Hot,
                opt_level: OptLevel::O2,
                use_pch: true,
                size_bytes: 100,
                cache_key: None,
            },
            CompileTask {
                file_name: "unit_1.c".into(),
                source: PathBuf::from("unit_1.c"),
                object: PathBuf::from("unit_1.o"),
                tier: TuTier::Hot,
                opt_level: OptLevel::O2,
                use_pch: true,
                size_bytes: 500,
                cache_key: None,
            },
            CompileTask {
                file_name: "unit_0_inits.c".into(),
                source: PathBuf::from("unit_0_inits.c"),
                object: PathBuf::from("unit_0_inits.o"),
                tier: TuTier::Cold,
                opt_level: OptLevel::O1,
                use_pch: false,
                size_bytes: 300,
                cache_key: None,
            },
            CompileTask {
                file_name: "unit_0_records.c".into(),
                source: PathBuf::from("unit_0_records.c"),
                object: PathBuf::from("unit_0_records.o"),
                tier: TuTier::Data,
                opt_level: OptLevel::O0,
                use_pch: false,
                size_bytes: 200,
                cache_key: None,
            },
        ];

        sort_compile_tasks(&mut tasks);
        let names: Vec<&str> = tasks.iter().map(|t| t.file_name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "unit_1.c",
                "unit_0_inits.c",
                "unit_2.c",
                "resources.c",
                "unit_0_records.c",
            ]
        );
    }

    #[test]
    fn cache_key_is_deterministic_and_sensitive_to_inputs() {
        let hdr = sha256_bytes(b"#ifndef JABS_UNIT_H\n#define JABS_UNIT_H\n#endif\n");
        let target = vec!["--target=x86_64-w64-windows-gnu".to_string()];
        let flags = vec!["-std=c11".to_string(), "-O2".to_string()];
        let src = b"int x(void) { return 42; }\n";

        let base = compute_cache_key("clang 22", &target, &flags, true, Some(&hdr), src);
        let again = compute_cache_key("clang 22", &target, &flags, true, Some(&hdr), src);
        assert_eq!(base, again);
        assert_eq!(base.len(), 64);

        assert_ne!(
            base,
            compute_cache_key("clang 23", &target, &flags, true, Some(&hdr), src)
        );
        assert_ne!(
            base,
            compute_cache_key("clang 22", &[], &flags, true, Some(&hdr), src)
        );
        assert_ne!(
            base,
            compute_cache_key(
                "clang 22",
                &target,
                &["-std=c11".into(), "-O0".into()],
                true,
                Some(&hdr),
                src
            )
        );
        assert_ne!(
            base,
            compute_cache_key("clang 22", &target, &flags, false, Some(&hdr), src)
        );
        assert_ne!(
            base,
            compute_cache_key("clang 22", &target, &flags, true, None, src)
        );
        assert_ne!(
            base,
            compute_cache_key(
                "clang 22",
                &target,
                &flags,
                true,
                Some(&hdr),
                b"int x(void) { return 7; }\n"
            )
        );
    }

    #[test]
    fn builds_and_caches_with_clang_when_available() {
        let clang_ok = Command::new("clang")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !clang_ok {
            if std::env::var_os("JABS_REQUIRE_TOOLS").is_some() {
                panic!("required tool missing: clang");
            }
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let out1 = tmp.path().join("out1");
        let out2 = tmp.path().join("out2");
        let entry_dir = tmp.path().join("entry");
        let cache_dir = tmp.path().join("cache");
        fs::create_dir_all(&out1).unwrap();
        fs::create_dir_all(&out2).unwrap();
        fs::create_dir_all(&entry_dir).unwrap();

        let files = [
            (
                "jabs_unit.h",
                "#ifndef JABS_UNIT_H\n#define JABS_UNIT_H\n#include <stdint.h>\nint32_t hot_fn(int32_t x);\nint32_t cold_fn(void);\nint32_t rt_fn(void);\nextern const int32_t data_val;\n#endif\n",
            ),
            (
                "unit_1.c",
                "#include \"jabs_unit.h\"\nint32_t hot_fn(int32_t x) { return x + data_val; }\n",
            ),
            (
                "unit_0_inits.c",
                "#include \"jabs_unit.h\"\nint32_t cold_fn(void) { return 10; }\n",
            ),
            (
                "resources.c",
                "#include \"jabs_unit.h\"\nconst int32_t data_val = 20;\n",
            ),
            (
                // Simulate a runtime TU defining a feature macro before including jabs_unit.h.
                "jabs_fake_rt.c",
                "#ifndef _WIN32\n#define _POSIX_C_SOURCE 200809L\n#endif\n#include \"jabs_unit.h\"\nint32_t rt_fn(void) { return 12; }\n",
            ),
        ];

        for dir in [&out1, &out2] {
            for (name, content) in files {
                fs::write(dir.join(name), content).unwrap();
            }
        }
        let entry_c = entry_dir.join("entry.c");
        fs::write(
            &entry_c,
            "#include \"jabs_unit.h\"\n#include <stdio.h>\nint main(void) { printf(\"%d\", hot_fn(cold_fn()) + rt_fn()); return 0; }\n",
        )
        .unwrap();

        let opts = BuildOptions {
            cache_dir: Some(cache_dir),
            ..Default::default()
        };

        let report1 = build_out_dir(&out1, Some(&entry_c), &opts).unwrap();
        assert!(report1.used_pch);
        assert_eq!(report1.total_sources, 5);
        assert_eq!(report1.compiled, 5);
        assert_eq!(report1.cached, 0);
        assert!(out1.join("jabs_unit.h.pch").is_file());

        let run1 = Command::new(&report1.output).output().unwrap();
        assert!(run1.status.success());
        assert_eq!(String::from_utf8_lossy(&run1.stdout), "42");

        // Rebuilding in a different directory with the same cache_dir reuses all cached objects.
        let report2 = build_out_dir(&out2, Some(&entry_c), &opts).unwrap();
        assert!(!report2.used_pch);
        assert_eq!(report2.total_sources, 5);
        assert_eq!(report2.compiled, 0);
        assert_eq!(report2.cached, 5);

        let run2 = Command::new(&report2.output).output().unwrap();
        assert!(run2.status.success());
        assert_eq!(String::from_utf8_lossy(&run2.stdout), "42");

        // Unguarded jabs_unit.h falls back cleanly without PCH.
        let out3 = tmp.path().join("out3");
        fs::create_dir_all(&out3).unwrap();
        fs::write(
            out3.join("jabs_unit.h"),
            "#include <stdint.h>\nint32_t hot_fn(int32_t x);\n",
        )
        .unwrap();
        fs::write(
            out3.join("unit_1.c"),
            "#include \"jabs_unit.h\"\nint32_t hot_fn(int32_t x) { return x + 1; }\nint main(void) { return hot_fn(4) - 5; }\n",
        )
        .unwrap();
        let report3 = build_out_dir(&out3, None, &BuildOptions::default()).unwrap();
        assert!(!report3.used_pch);
        assert_eq!(report3.compiled, 1);
        assert!(Command::new(&report3.output).status().unwrap().success());
    }
}
