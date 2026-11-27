//! Version-agnostic JAR and class-file inventory.
//!
//! Works across Minecraft eras: Java 5-8 class files used by 1.7.10/1.12.2
//! Forge, through modern Java 21/25 Fabric and NeoForge jars. Reads metadata only;
//! nothing from the archive is executed or extracted to disk.

use crate::classfile::{ACC_INTERFACE, ACC_NATIVE, ClassFile, Constant};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

/// Upper bounds against malformed or hostile archives.
pub const MAX_ENTRIES: usize = 500_000;
pub const MAX_CLASS_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_REPORTED_ERRORS: usize = 20;

#[derive(Debug, Serialize, PartialEq)]
pub struct ClassInfo {
    pub name: String,
    pub super_name: Option<String>,
    pub interfaces: Vec<String>,
    pub major: u16,
    pub minor: u16,
    pub is_interface: bool,
    pub fields: usize,
    pub methods: usize,
    pub native_methods: usize,
    pub uses_invokedynamic: bool,
    pub uses_constant_dynamic: bool,
    pub uses_method_handles: bool,
    pub is_module_info: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum LoaderMetadata {
    /// `fabric.mod.json`
    Fabric,
    /// `quilt.mod.json`
    Quilt,
    /// `META-INF/neoforge.mods.toml`
    Neoforge,
    /// `META-INF/mods.toml` (Forge 1.13+, early NeoForge)
    ModsToml,
    /// `mcmod.info` (legacy Forge, e.g. 1.7.10 and 1.12.2)
    LegacyForge,
    /// Manifest declares `FMLCorePlugin` (legacy Forge coremod)
    ForgeCoremod,
    /// Manifest declares `TweakClass` (LaunchWrapper tweaker)
    LaunchWrapperTweaker,
}

#[derive(Debug, Serialize)]
pub struct JarReport {
    pub schema_version: u32,
    pub entries: usize,
    pub classes: usize,
    pub interfaces: usize,
    /// Class-file major version -> class count. Major 52 = Java 8, 65 = Java 21.
    pub class_versions: BTreeMap<u16, usize>,
    /// Minimum Java feature release able to load every class, ignoring multi-release overlays.
    pub required_java: Option<u16>,
    pub multi_release: bool,
    pub loader_metadata: Vec<LoaderMetadata>,
    pub mixin_configs: Vec<String>,
    pub native_libraries: Vec<String>,
    pub native_methods: usize,
    pub classes_using_invokedynamic: usize,
    pub classes_using_constant_dynamic: usize,
    pub classes_using_method_handles: usize,
    pub invalid_classes: usize,
    pub errors: Vec<String>,
    pub analysis: &'static str,
}

/// Java feature release for a class-file major version (45 = JDK 1.1 .. 49 = Java 5).
pub fn java_release(major: u16) -> u16 {
    if major >= 49 { major - 44 } else { 1 }
}

/// Parse the structural parts of a class file (JVMS chapter 4) without verifying bytecode.
pub fn parse_class(data: &[u8]) -> Result<ClassInfo> {
    let class = ClassFile::parse(data)?;
    let (mut indy, mut condy, mut handles) = (false, false, false);
    for constant in &class.pool {
        match constant {
            Constant::InvokeDynamic { .. } => indy = true,
            Constant::Dynamic { .. } => condy = true,
            Constant::MethodHandle { .. } | Constant::MethodType(_) => handles = true,
            _ => {}
        }
    }
    Ok(ClassInfo {
        is_module_info: class.name == "module-info",
        native_methods: class
            .methods
            .iter()
            .filter(|m| m.access & ACC_NATIVE != 0)
            .count(),
        is_interface: class.access & ACC_INTERFACE != 0,
        fields: class.fields.len(),
        methods: class.methods.len(),
        name: class.name,
        super_name: class.super_name,
        interfaces: class.interfaces,
        major: class.major,
        minor: class.minor,
        uses_invokedynamic: indy,
        uses_constant_dynamic: condy,
        uses_method_handles: handles,
    })
}

/// Read an archive entry fully, failing if it exceeds `limit` bytes (zip-bomb guard).
fn read_bounded(reader: impl Read, limit: u64, name: &str) -> Result<Vec<u8>> {
    read_bounded_hint(reader, limit, 0, name)
}

fn read_bounded_hint(reader: impl Read, limit: u64, size_hint: u64, name: &str) -> Result<Vec<u8>> {
    let cap = usize::try_from(size_hint.min(limit).min(1 << 20)).unwrap_or(0);
    let mut buffer = Vec::with_capacity(cap);
    reader
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .with_context(|| format!("read {name}"))?;
    if buffer.len() as u64 > limit {
        bail!("{name} exceeds {limit} bytes");
    }
    Ok(buffer)
}

/// Whether an entry looks like a bundled native library (.dll/.so/.dylib/.jnilib).
fn is_native_library(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".dll", ".so", ".dylib", ".jnilib"]
        .iter()
        .any(|ext| lower.ends_with(ext))
        || lower.contains(".so.")
}

/// Inventory a JAR's classes and loader-facing metadata.
pub fn inspect_jar(path: &Path) -> Result<JarReport> {
    let file = File::open(path).with_context(|| format!("open jar {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("read zip {}", path.display()))?;
    if archive.len() > MAX_ENTRIES {
        bail!(
            "{} has {} entries; limit is {MAX_ENTRIES}",
            path.display(),
            archive.len()
        );
    }
    let mut report = JarReport {
        schema_version: 1,
        entries: archive.len(),
        classes: 0,
        interfaces: 0,
        class_versions: BTreeMap::new(),
        required_java: None,
        multi_release: false,
        loader_metadata: Vec::new(),
        mixin_configs: Vec::new(),
        native_libraries: Vec::new(),
        native_methods: 0,
        classes_using_invokedynamic: 0,
        classes_using_constant_dynamic: 0,
        classes_using_method_handles: 0,
        invalid_classes: 0,
        errors: Vec::new(),
        analysis: "class structure and metadata only; bytecode not verified, nested jars not opened",
    };
    let mut base_major = 0u16;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let record_error = |report: &mut JarReport, message: String| {
            report.invalid_classes += 1;
            if report.errors.len() < MAX_REPORTED_ERRORS {
                report.errors.push(format!("{name}: {message}"));
            }
        };
        if name.ends_with(".class") {
            report.classes += 1;
            let info =
                read_bounded(entry, MAX_CLASS_BYTES, &name).and_then(|bytes| parse_class(&bytes));
            match info {
                Ok(info) => {
                    *report.class_versions.entry(info.major).or_default() += 1;
                    if !name.starts_with("META-INF/versions/") && !info.is_module_info {
                        base_major = base_major.max(info.major);
                    }
                    report.interfaces += info.is_interface as usize;
                    report.native_methods += info.native_methods;
                    report.classes_using_invokedynamic += info.uses_invokedynamic as usize;
                    report.classes_using_constant_dynamic += info.uses_constant_dynamic as usize;
                    report.classes_using_method_handles += info.uses_method_handles as usize;
                }
                Err(error) => record_error(&mut report, format!("{error:#}")),
            }
            continue;
        }
        let metadata = match name.as_str() {
            "fabric.mod.json" => Some(LoaderMetadata::Fabric),
            "quilt.mod.json" => Some(LoaderMetadata::Quilt),
            "META-INF/neoforge.mods.toml" => Some(LoaderMetadata::Neoforge),
            "META-INF/mods.toml" => Some(LoaderMetadata::ModsToml),
            "mcmod.info" => Some(LoaderMetadata::LegacyForge),
            _ => None,
        };
        if let Some(kind) = metadata {
            report.loader_metadata.push(kind);
        } else if name.eq_ignore_ascii_case("META-INF/MANIFEST.MF") {
            let text = String::from_utf8_lossy(&read_bounded(entry, MAX_METADATA_BYTES, &name)?)
                .into_owned();
            for line in text.lines() {
                let key = line.split(':').next().unwrap_or("").trim();
                match key {
                    "Multi-Release" => {
                        report.multi_release = line.to_ascii_lowercase().contains("true")
                    }
                    "FMLCorePlugin" => report.loader_metadata.push(LoaderMetadata::ForgeCoremod),
                    "TweakClass" => report
                        .loader_metadata
                        .push(LoaderMetadata::LaunchWrapperTweaker),
                    _ => {}
                }
            }
        } else if !name.contains('/')
            && (name.ends_with(".mixins.json") || name.ends_with(".mixin.json"))
        {
            report.mixin_configs.push(name);
        } else if is_native_library(&name) {
            report.native_libraries.push(name);
        }
    }
    report.loader_metadata.sort();
    report.loader_metadata.dedup();
    report.mixin_configs.sort();
    report.native_libraries.sort();
    report.required_java = (base_major > 0).then(|| java_release(base_major));
    Ok(report)
}

/// Compilation coverage for every class in a JAR, with blockers ranked by how many methods they stop.
#[derive(Debug, Serialize)]
pub struct Coverage {
    pub schema_version: u32,
    pub classes: usize,
    pub unparsable_classes: usize,
    pub methods: usize,
    pub compiled_methods: usize,
    /// Compiled methods that never reach a not-compiled stub.
    pub self_contained_methods: usize,
    /// Normalized unsupported reason -> method count, most common first.
    pub blockers: Vec<(String, usize)>,
    /// What each blocker category costs beyond its own methods, most unlocked first.
    pub blast_radius: Vec<BlastRadius>,
    /// The JDK classes compiled in the same unit, when requested.
    pub jdk: Option<PartCoverage>,
    /// The JDK modules in the unit: java.base, plus every module holding a class the loaded list
    /// or the method trace names, as `compile` adds them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jdk_modules: Vec<String>,
    /// The same counts restricted to the classes a JVM run loaded, when a list is given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded: Option<LoadedCoverage>,
    /// What a traced JVM run actually executed, when a method trace is given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed: Option<ExecutedCoverage>,
}

/// The first line of a method trace written by `tools/method-trace/jabs_trace.c`.
const TRACE_HEADER: &str = "# jabs method trace 1";
/// Most methods accepted in a trace; the agent itself stops at 2^22.
const MAX_TRACED_METHODS: usize = 1 << 22;

/// A method trace from a real JVM run: every method that ran, natives included.
#[derive(Debug, Default)]
pub struct MethodTrace {
    /// (class, name, descriptor) of each method that ran, classes as internal names.
    pub methods: std::collections::BTreeSet<(String, String, String)>,
    /// Methods of hidden classes (lambda proxies and the like, defined at run time with names
    /// like `a/B$$Lambda.0x...`); no compiled unit can contain them.
    pub hidden_class_methods: usize,
}

/// Read a trace written by the JABS method-trace agent. Refused unless it is whole: the header
/// first, `# complete <n>` last with `n` equal to the method lines, every line three
/// tab-separated fields with a descriptor, and no method twice (the agent writes each once).
pub fn read_method_trace(path: &Path) -> Result<MethodTrace> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(TRACE_HEADER) {
        bail!(
            "{}: not a JABS method trace (first line is not `{TRACE_HEADER}`)",
            path.display()
        );
    }
    let mut trace = MethodTrace::default();
    let mut seen = 0usize;
    let mut complete = None;
    for (index, line) in lines.enumerate() {
        let number = index + 2;
        if complete.is_some() {
            bail!(
                "{}:{number}: text after the `# complete` line",
                path.display()
            );
        }
        if let Some(count) = line.strip_prefix("# complete ") {
            let count: usize = count
                .parse()
                .with_context(|| format!("{}:{number}: bad method count", path.display()))?;
            complete = Some(count);
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [class, name, descriptor] = fields[..] else {
            bail!(
                "{}:{number}: expected class, name and descriptor",
                path.display()
            );
        };
        if class.is_empty() || name.is_empty() || !descriptor.starts_with('(') {
            bail!("{}:{number}: malformed method `{line}`", path.display());
        }
        seen += 1;
        if seen > MAX_TRACED_METHODS {
            bail!("{}: more than {MAX_TRACED_METHODS} methods", path.display());
        }
        // A hidden class's name carries a '.' before its suffix; no class file has one.
        if class.contains('.') {
            trace.hidden_class_methods += 1;
            continue;
        }
        let key = (class.to_owned(), name.to_owned(), descriptor.to_owned());
        if !trace.methods.insert(key) {
            bail!(
                "{}:{number}: {class}.{name}{descriptor} appears twice",
                path.display()
            );
        }
    }
    let Some(count) = complete else {
        bail!(
            "{}: incomplete trace (no `# complete` line; the run was killed or the agent stopped)",
            path.display()
        );
    };
    if count != seen {
        bail!(
            "{}: trace says {count} methods but holds {seen}",
            path.display()
        );
    }
    Ok(trace)
}

/// What a traced run executed, against what JABS compiles.
#[derive(Debug, Serialize)]
pub struct ExecutedCoverage {
    /// Methods in the trace, hidden classes excluded.
    pub methods_traced: usize,
    pub hidden_class_methods: usize,
    /// Traced methods the unit does not contain (other JDK modules, other jars).
    pub methods_not_in_unit: usize,
    pub jar: ExecutedPart,
    pub jdk: Option<ExecutedPart>,
}

/// Executed methods of one part of the unit.
#[derive(Debug, Default, Serialize)]
pub struct ExecutedPart {
    pub executed: usize,
    pub compiled: usize,
    /// Executed and compiled, and never reaching code that did not compile.
    pub self_contained: usize,
    /// Executed but not compiled. This includes what the JVM's own launcher ran (loading and
    /// reflecting on the main class, starting its service threads), which a JABS program does
    /// not do, so it is the list to check rather than the list to implement.
    pub unsupported: usize,
    /// Executed methods whose class is in the unit without them. The JDK's runtime image adds
    /// generated methods to some classes (`java/lang/invoke/*$Holder`) that its jmods lack, so a
    /// few here are expected; many mean the trace came from another build of the jar or JDK.
    pub method_not_in_class: usize,
    /// Executed but abstract in the unit, which a run cannot do: the trace and the unit disagree.
    pub abstract_in_unit: usize,
    /// Executed unsupported methods per blocker category, most first.
    pub blockers: Vec<(String, usize)>,
    /// Every executed unsupported method, by category then name.
    pub unsupported_methods: Vec<UnsupportedMethod>,
}

#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnsupportedMethod {
    pub category: String,
    pub class: String,
    pub name: String,
    pub descriptor: String,
    pub reason: String,
}

fn executed_coverage(
    unit: &crate::cgen::CompiledUnit,
    trace: &MethodTrace,
    jar_set: &std::collections::HashSet<&str>,
    jdk_set: &std::collections::HashSet<&str>,
    with_jdk: bool,
) -> ExecutedCoverage {
    use crate::cgen::MethodStatus;
    let mut by_key = std::collections::HashMap::new();
    for method in &unit.methods {
        by_key.insert(
            (
                method.class.as_str(),
                method.name.as_str(),
                method.descriptor.as_str(),
            ),
            method,
        );
    }
    let (mut jar, mut jdk) = (ExecutedPart::default(), ExecutedPart::default());
    let mut not_in_unit = 0;
    for (class, name, descriptor) in &trace.methods {
        let found = by_key.get(&(class.as_str(), name.as_str(), descriptor.as_str()));
        let part = if jar_set.contains(class.as_str()) {
            &mut jar
        } else if jdk_set.contains(class.as_str()) {
            &mut jdk
        } else {
            not_in_unit += 1;
            continue;
        };
        let Some(method) = found else {
            // The class is in the unit but has no such method: another build of it.
            part.executed += 1;
            part.method_not_in_class += 1;
            continue;
        };
        part.executed += 1;
        match &method.status {
            MethodStatus::Compiled { self_contained, .. } => {
                part.compiled += 1;
                part.self_contained += usize::from(*self_contained);
            }
            MethodStatus::Unsupported { reason } => {
                part.unsupported += 1;
                part.unsupported_methods.push(UnsupportedMethod {
                    category: crate::cgen::blocker_category(reason),
                    class: class.clone(),
                    name: name.clone(),
                    descriptor: descriptor.clone(),
                    reason: reason.clone(),
                });
            }
            MethodStatus::Abstract => part.abstract_in_unit += 1,
        }
    }
    for part in [&mut jar, &mut jdk] {
        part.unsupported_methods.sort();
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for method in &part.unsupported_methods {
            *tally.entry(method.category.clone()).or_default() += 1;
        }
        part.blockers = tally.into_iter().collect();
        part.blockers
            .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
    ExecutedCoverage {
        methods_traced: trace.methods.len(),
        hidden_class_methods: trace.hidden_class_methods,
        methods_not_in_unit: not_in_unit,
        jar,
        jdk: with_jdk.then_some(jdk),
    }
}

/// Coverage of only the classes a real JVM run loaded (`-Xlog:class+load`). This is class
/// granularity: every method of a loaded class counts, whether or not the run called it.
#[derive(Debug, Serialize)]
pub struct LoadedCoverage {
    /// Distinct class names in the list.
    pub classes_listed: usize,
    /// Listed classes that are not in the unit (other JDK modules, generated at run time, or
    /// from other jars); their methods are not counted anywhere.
    pub classes_not_in_unit: usize,
    pub jar: PartCoverage,
    pub jdk: Option<PartCoverage>,
}

/// Longest line accepted in a class list; internal names are far shorter.
const MAX_CLASS_NAME: usize = 4096;
/// Most names accepted in a class list; a full modded client loads well under this.
const MAX_LISTED_CLASSES: usize = 1 << 20;

/// Read a class list: one internal name (`a/b/C`) per line, blank lines and `#` comments
/// skipped. A name that is not an internal name (dots, spaces, a leading or trailing `/`,
/// empty segments) is refused with its line number rather than silently matching nothing.
/// Repeats are allowed: a JVM log lists a class once per loader that defined it.
pub fn read_class_list(path: &Path) -> Result<std::collections::BTreeSet<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut names = std::collections::BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        if line.len() > MAX_CLASS_NAME {
            bail!(
                "{}:{number}: name longer than {MAX_CLASS_NAME} bytes",
                path.display()
            );
        }
        let bad_char = line
            .chars()
            .any(|c| c == '.' || c == ';' || c == '[' || c.is_whitespace());
        if bad_char || line.split('/').any(str::is_empty) {
            bail!(
                "{}:{number}: `{line}` is not an internal class name (a/b/C)",
                path.display()
            );
        }
        names.insert(line.to_owned());
        if names.len() > MAX_LISTED_CLASSES {
            bail!("{}: more than {MAX_LISTED_CLASSES} classes", path.display());
        }
    }
    Ok(names)
}

/// Maximum jar size read into memory for parallel entry decompression (512 MiB).
const MAX_PARALLEL_JAR_BYTES: u64 = 512 * 1024 * 1024;
/// Minimum class count before spawning worker threads for zip decompression/parsing.
const PARALLEL_CLASS_THRESHOLD: usize = 64;

fn is_target_class(name: &str, prefix: &str) -> bool {
    let Some(rel) = name.strip_prefix(prefix) else {
        return false;
    };
    rel.ends_with(".class")
        && !rel.starts_with("META-INF/versions/")
        && !rel.ends_with("module-info.class")
}

fn parse_zip_indices<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    indices: &[usize],
) -> Result<(Vec<ClassFile>, usize)> {
    let mut classes = Vec::with_capacity(indices.len());
    let mut unparsable = 0usize;
    for &index in indices {
        let entry = archive.by_index(index)?;
        let size = entry.size();
        let name = entry.name().to_owned();
        match read_bounded_hint(entry, MAX_CLASS_BYTES, size, &name)
            .and_then(|b| ClassFile::parse(&b))
        {
            Ok(class) => classes.push(class),
            Err(_) => unparsable += 1,
        }
    }
    Ok((classes, unparsable))
}

/// Parse the classes under `prefix` from an in-memory zip buffer, parallelizing decompression
/// and parsing across threads while preserving deterministic entry order.
pub(crate) fn classes_from_zip_bytes(
    zip_bytes: &[u8],
    prefix: &str,
    label: &str,
) -> Result<(Vec<ClassFile>, usize, usize)> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .with_context(|| format!("read zip {label}"))?;
    if archive.len() > MAX_ENTRIES {
        bail!(
            "{label} has {} entries; limit is {MAX_ENTRIES}",
            archive.len()
        );
    }
    let indices: Vec<usize> = (0..archive.len())
        .filter(|&i| {
            archive
                .name_for_index(i)
                .is_some_and(|name| is_target_class(name, prefix))
        })
        .collect();
    let tried = indices.len();
    if tried < PARALLEL_CLASS_THRESHOLD {
        let (classes, unparsable) = parse_zip_indices(&mut archive, &indices)?;
        return Ok((classes, tried, unparsable));
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(tried);
    let chunk_size = tried.div_ceil(workers);
    let results: Vec<Result<(Vec<ClassFile>, usize)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = indices
            .chunks(chunk_size)
            .map(|chunk| {
                let worker_archive = archive.clone();
                scope.spawn(move || {
                    let mut local_archive = worker_archive;
                    parse_zip_indices(&mut local_archive, chunk)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("zip class parser thread panicked"))
            .collect()
    });
    let mut classes = Vec::with_capacity(tried);
    let mut unparsable = 0usize;
    for res in results {
        let (chunk_classes, chunk_unparsable) = res?;
        classes.extend(chunk_classes);
        unparsable += chunk_unparsable;
    }
    Ok((classes, tried, unparsable))
}

/// Parse every class in a JAR (multi-release overlays skipped); returns (classes, entries tried, unparsable).
pub fn read_jar_classes(path: &Path) -> Result<(Vec<ClassFile>, usize, usize)> {
    let meta = std::fs::metadata(path).with_context(|| format!("open jar {}", path.display()))?;
    if meta.len() <= MAX_PARALLEL_JAR_BYTES {
        let bytes = std::fs::read(path).with_context(|| format!("open jar {}", path.display()))?;
        return classes_from_zip_bytes(&bytes, "", &path.display().to_string());
    }
    let file = File::open(path).with_context(|| format!("open jar {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("read zip {}", path.display()))?;
    classes_from_zip(&mut archive, "", &path.display().to_string())
}

/// Parse the classes under `prefix` in an open zip (jar or jmod); returns (classes, entries tried, unparsable).
pub fn classes_from_zip<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    prefix: &str,
    label: &str,
) -> Result<(Vec<ClassFile>, usize, usize)> {
    if archive.len() > MAX_ENTRIES {
        bail!(
            "{label} has {} entries; limit is {MAX_ENTRIES}",
            archive.len()
        );
    }
    let mut payloads: Vec<Option<Vec<u8>>> = Vec::new();
    let mut tried = 0usize;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let size = entry.size();
        let name = entry.name().to_owned();
        if !is_target_class(&name, prefix) {
            continue;
        }
        tried += 1;
        payloads.push(read_bounded_hint(entry, MAX_CLASS_BYTES, size, &name).ok());
    }
    if payloads.len() < PARALLEL_CLASS_THRESHOLD {
        let mut classes = Vec::with_capacity(payloads.len());
        let mut unparsable = 0usize;
        for payload in payloads {
            match payload.and_then(|b| ClassFile::parse(&b).ok()) {
                Some(class) => classes.push(class),
                None => unparsable += 1,
            }
        }
        return Ok((classes, tried, unparsable));
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(payloads.len());
    let chunk_size = payloads.len().div_ceil(workers);
    let results: Vec<(Vec<ClassFile>, usize)> = std::thread::scope(|scope| {
        let handles: Vec<_> = payloads
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut classes = Vec::with_capacity(chunk.len());
                    let mut unparsable = 0usize;
                    for payload in chunk {
                        match payload.as_deref().and_then(|b| ClassFile::parse(b).ok()) {
                            Some(class) => classes.push(class),
                            None => unparsable += 1,
                        }
                    }
                    (classes, unparsable)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("class parser thread panicked"))
            .collect()
    });
    let mut classes = Vec::with_capacity(tried);
    let mut unparsable = 0usize;
    for (chunk_classes, chunk_unparsable) in results {
        classes.extend(chunk_classes);
        unparsable += chunk_unparsable;
    }
    Ok((classes, tried, unparsable))
}

/// Load classes from `.class` files and jars, failing on anything unparsable.
/// Every non-class file in the input jars, in input order: what the app class loader serves.
/// Directories and signature files (`META-INF/*.SF`, `.RSA`, `.DSA`, `.EC`) are left out.
pub fn load_resources(inputs: &[std::path::PathBuf]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for input in inputs.iter().filter(|i| {
        !i.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("class"))
    }) {
        let mut zip = zip::ZipArchive::new(
            std::fs::File::open(input).with_context(|| format!("open {}", input.display()))?,
        )?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let name = entry.name().to_string();
            let upper = name.to_ascii_uppercase();
            let signature = upper.starts_with("META-INF/")
                && [".SF", ".RSA", ".DSA", ".EC"]
                    .iter()
                    .any(|x| upper.ends_with(x));
            if entry.is_dir() || name.ends_with(".class") || signature {
                continue;
            }
            let mut data = Vec::with_capacity(entry.size().min(1 << 26) as usize);
            entry.read_to_end(&mut data)?;
            out.push((name, data));
        }
    }
    Ok(out)
}

pub fn load_classes(inputs: &[std::path::PathBuf]) -> Result<Vec<ClassFile>> {
    let mut classes = Vec::new();
    for input in inputs {
        if input
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("class"))
        {
            let bytes =
                std::fs::read(input).with_context(|| format!("read {}", input.display()))?;
            classes.push(
                ClassFile::parse(&bytes).with_context(|| format!("parse {}", input.display()))?,
            );
        } else {
            let (found, _, unparsable) = read_jar_classes(input)?;
            if unparsable > 0 {
                bail!(
                    "{}: {unparsable} classes could not be parsed",
                    input.display()
                );
            }
            classes.extend(found);
        }
    }
    let mut names = std::collections::HashSet::new();
    if let Some(dup) = classes.iter().find(|c| !names.insert(c.name.clone())) {
        bail!("class {} appears twice in the inputs", dup.name);
    }
    Ok(classes)
}

/// Coverage of one part of a unit (the jar, or the JDK classes compiled alongside it).
#[derive(Debug, Serialize)]
pub struct PartCoverage {
    pub classes: usize,
    pub methods: usize,
    pub compiled_methods: usize,
    /// Compiled methods that never reach a not-compiled stub.
    pub self_contained_methods: usize,
    /// Normalized unsupported reason -> method count, most common first.
    pub blockers: Vec<(String, usize)>,
    /// What each blocker category costs beyond its own methods, most unlocked first.
    pub blast_radius: Vec<BlastRadius>,
}

/// One blocker category's reach through the dependency graph, within one part.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct BlastRadius {
    pub category: String,
    /// Methods of this part that are unsupported for this reason.
    pub direct: usize,
    /// Compiled methods of this part that can reach this category's not-compiled code.
    pub reaches: usize,
    /// Of those, the ones nothing else blocks: fixing this category alone makes them
    /// self-contained.
    pub unlocks: usize,
}

/// Count compiled/unsupported methods of the unit's classes in `names`, with ranked blockers.
fn part_coverage(
    unit: &crate::cgen::CompiledUnit,
    names: &std::collections::HashSet<&str>,
) -> PartCoverage {
    // category -> (direct, reaches, unlocks)
    let mut tally: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let (mut methods, mut compiled, mut self_contained) = (0, 0, 0);
    for method in unit
        .methods
        .iter()
        .filter(|m| names.contains(m.class.as_str()))
    {
        match &method.status {
            crate::cgen::MethodStatus::Compiled {
                self_contained: whole,
                blocked_by,
                ..
            } => {
                methods += 1;
                compiled += 1;
                self_contained += usize::from(*whole);
                for category in blocked_by {
                    let entry = if let Some(entry) = tally.get_mut(category.as_str()) {
                        entry
                    } else {
                        tally.entry(category.clone()).or_default()
                    };
                    entry.1 += 1;
                    if blocked_by.len() == 1 {
                        entry.2 += 1;
                    }
                }
            }
            crate::cgen::MethodStatus::Unsupported { reason } => {
                methods += 1;
                tally
                    .entry(crate::cgen::blocker_category(reason))
                    .or_default()
                    .0 += 1;
            }
            crate::cgen::MethodStatus::Abstract => {}
        }
    }
    let mut blockers: Vec<(String, usize)> = Vec::new();
    let mut blast_radius: Vec<BlastRadius> = Vec::new();
    for (category, (direct, reaches, unlocks)) in tally {
        if direct > 0 {
            blockers.push((category.clone(), direct));
        }
        blast_radius.push(BlastRadius {
            category,
            direct,
            reaches,
            unlocks,
        });
    }
    blockers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    blast_radius.sort_by(|a, b| {
        b.unlocks
            .cmp(&a.unlocks)
            .then_with(|| b.reaches.cmp(&a.reaches))
            .then_with(|| b.direct.cmp(&a.direct))
            .then_with(|| a.category.cmp(&b.category))
    });
    PartCoverage {
        classes: names.len(),
        methods,
        compiled_methods: compiled,
        self_contained_methods: self_contained,
        blockers,
        blast_radius,
    }
}

/// Compile every class in a JAR (plus, optionally, the JDK's java.base) as one unit, without
/// writing C, and tally what blocks the rest.
pub fn coverage(path: &Path, jdk: Option<&Path>) -> Result<Coverage> {
    coverage_with(path, jdk, None, None)
}

/// [`coverage`], plus the same counts over only the classes in `loaded`, and what a traced run
/// executed, when they are given.
pub fn coverage_with(
    path: &Path,
    jdk: Option<&Path>,
    loaded: Option<&std::collections::BTreeSet<String>>,
    executed: Option<&MethodTrace>,
) -> Result<Coverage> {
    let (mut classes, tried, unparsable) = read_jar_classes(path)?;
    let jar_names: Vec<String> = classes.iter().map(|c| c.name.clone()).collect();
    let jar_set: std::collections::HashSet<&str> = jar_names.iter().map(String::as_str).collect();
    let mut jdk_names = std::collections::HashSet::new();
    let mut jdk_modules = Vec::new();
    if let Some(java) = jdk {
        let home = crate::jdk::java_home(java)?;
        // java.base, plus the modules of every class a run is known to have used, so their
        // natives and blockers are counted too.
        let mut used = std::collections::BTreeSet::new();
        if let Some(listed) = loaded {
            used.extend(listed.iter().cloned());
        }
        if let Some(trace) = executed {
            used.extend(trace.methods.iter().map(|(class, _, _)| class.clone()));
        }
        jdk_modules.push("java.base".to_owned());
        if !used.is_empty() {
            for module in crate::jdk::modules_containing(&home, &used)? {
                if !jdk_modules.contains(&module) {
                    jdk_modules.push(module);
                }
            }
        }
        // A jar class with the same name as a JDK class would be shadowed by the boot loader.
        for module in &jdk_modules {
            let (library, _, library_unparsable) = crate::jdk::read_module(&home, module)?;
            if library_unparsable > 0 {
                bail!("{library_unparsable} {module} classes could not be parsed");
            }
            for class in library {
                if jar_set.contains(class.name.as_str()) {
                    continue;
                }
                if !jdk_names.insert(class.name.clone()) {
                    bail!("JDK class {} appears in two modules", class.name);
                }
                classes.push(class);
            }
        }
    }
    let unit = crate::cgen::compile_unit(&classes);
    let jar = part_coverage(&unit, &jar_set);
    let jdk_set: std::collections::HashSet<&str> = jdk_names.iter().map(String::as_str).collect();
    let jdk_part = (!jdk_names.is_empty()).then(|| part_coverage(&unit, &jdk_set));
    let loaded_part = loaded.map(|listed| {
        let mut jar_loaded = std::collections::HashSet::new();
        let mut jdk_loaded = std::collections::HashSet::new();
        let mut not_in_unit = 0;
        for name in listed {
            if let Some(found) = jar_set.get(name.as_str()) {
                jar_loaded.insert(*found);
            } else if let Some(found) = jdk_set.get(name.as_str()) {
                jdk_loaded.insert(*found);
            } else {
                not_in_unit += 1;
            }
        }
        LoadedCoverage {
            classes_listed: listed.len(),
            classes_not_in_unit: not_in_unit,
            jar: part_coverage(&unit, &jar_loaded),
            jdk: (!jdk_names.is_empty()).then(|| part_coverage(&unit, &jdk_loaded)),
        }
    });
    let executed_part = executed
        .map(|trace| executed_coverage(&unit, trace, &jar_set, &jdk_set, !jdk_names.is_empty()));
    Ok(Coverage {
        schema_version: 5,
        classes: tried,
        unparsable_classes: unparsable,
        methods: jar.methods,
        compiled_methods: jar.compiled_methods,
        self_contained_methods: jar.self_contained_methods,
        blockers: jar.blockers,
        blast_radius: jar.blast_radius,
        jdk: jdk_part,
        jdk_modules,
        loaded: loaded_part,
        executed: executed_part,
    })
}
