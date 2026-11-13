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
    let mut buffer = Vec::new();
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
    /// The JDK classes compiled in the same unit, when requested.
    pub jdk: Option<PartCoverage>,
}

/// Collapse per-method details (pcs, names, descriptors) so reasons group into categories.
fn blocker_category(reason: &str) -> String {
    let reason = reason
        .split_once(": ")
        .filter(|(pc, _)| pc.starts_with("pc "))
        .map_or(reason, |(_, r)| r);
    let rules: [(&str, &str); 17] = [
        (
            "class hierarchy incomplete",
            "superclass/interface outside the unit (JDK or other jars)",
        ),
        (
            "is inherited from outside the unit",
            "call inherited from outside the unit (e.g. Object methods)",
        ),
        (
            "is not in the compilation unit",
            "uses a class outside the unit (JDK or other jars)",
        ),
        (
            "or its hierarchy is outside the unit",
            "type relationship outside the unit",
        ),
        (
            "which cannot be initialized",
            "initializes a class that cannot be initialized",
        ),
        ("static initializer of", "static initializer not compiled"),
        (
            "type test against",
            "instanceof/checkcast against an interface or outside class",
        ),
        ("arrays in", "arrays"),
        ("interface instance methods", "interface instance methods"),
        ("synchronized", "synchronized methods"),
        ("exception handlers", "exception handlers (try/catch)"),
        ("no bytecode", "abstract or native methods"),
        (
            "native method without",
            "native method without a JABS implementation",
        ),
        ("calls ", "calls a method that is not compiled"),
        (
            "not declared in the unit",
            "field declared outside the unit",
        ),
        ("super calls", "super calls"),
        ("constructor", "constructor call shape"),
    ];
    if let Some((_, category)) = rules.iter().find(|(needle, _)| reason.contains(needle)) {
        return category.to_string();
    }
    if let Some(op) = reason.strip_prefix("opcode ") {
        return format!("opcode {}", op.split(" (").next().unwrap_or(op));
    }
    reason.to_string()
}

/// Parse every class in a JAR (multi-release overlays skipped); returns (classes, entries tried, unparsable).
pub fn read_jar_classes(path: &Path) -> Result<(Vec<ClassFile>, usize, usize)> {
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
    let (mut classes, mut tried, mut unparsable) = (Vec::new(), 0, 0);
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        let Some(rel) = name.strip_prefix(prefix) else {
            continue;
        };
        if !rel.ends_with(".class")
            || rel.starts_with("META-INF/versions/")
            || rel.ends_with("module-info.class")
        {
            continue;
        }
        tried += 1;
        match read_bounded(entry, MAX_CLASS_BYTES, &name).and_then(|b| ClassFile::parse(&b)) {
            Ok(class) => classes.push(class),
            Err(_) => unparsable += 1,
        }
    }
    Ok((classes, tried, unparsable))
}

/// Load classes from `.class` files and jars, failing on anything unparsable.
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
}

/// Count compiled/unsupported methods of the unit's classes in `names`, with ranked blockers.
fn part_coverage(
    unit: &crate::cgen::CompiledUnit,
    names: &std::collections::HashSet<&str>,
) -> PartCoverage {
    let mut blockers: BTreeMap<String, usize> = BTreeMap::new();
    let (mut methods, mut compiled, mut self_contained) = (0, 0, 0);
    for method in unit
        .methods
        .iter()
        .filter(|m| names.contains(m.class.as_str()))
    {
        match &method.status {
            crate::cgen::MethodStatus::Compiled {
                self_contained: whole,
                ..
            } => {
                methods += 1;
                compiled += 1;
                self_contained += *whole as usize;
            }
            crate::cgen::MethodStatus::Unsupported { reason } => {
                methods += 1;
                *blockers.entry(blocker_category(reason)).or_default() += 1;
            }
            crate::cgen::MethodStatus::Abstract => {}
        }
    }
    let mut blockers: Vec<_> = blockers.into_iter().collect();
    blockers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    PartCoverage {
        classes: names.len(),
        methods,
        compiled_methods: compiled,
        self_contained_methods: self_contained,
        blockers,
    }
}

/// Compile every class in a JAR (plus, optionally, the JDK's java.base) as one unit, without
/// writing C, and tally what blocks the rest.
pub fn coverage(path: &Path, jdk: Option<&Path>) -> Result<Coverage> {
    let (mut classes, tried, unparsable) = read_jar_classes(path)?;
    let jar_names: Vec<String> = classes.iter().map(|c| c.name.clone()).collect();
    let mut jdk_names = Vec::new();
    if let Some(java) = jdk {
        let home = crate::jdk::java_home(java)?;
        let (library, _, library_unparsable) = crate::jdk::read_module(&home, "java.base")?;
        if library_unparsable > 0 {
            bail!("{library_unparsable} java.base classes could not be parsed");
        }
        // A jar class with the same name as a JDK class would be shadowed by the boot loader.
        let mine: std::collections::HashSet<&str> = jar_names.iter().map(String::as_str).collect();
        for class in library {
            if !mine.contains(class.name.as_str()) {
                jdk_names.push(class.name.clone());
                classes.push(class);
            }
        }
    }
    let unit = crate::cgen::compile_unit(&classes);
    let jar_set = jar_names.iter().map(String::as_str).collect();
    let jar = part_coverage(&unit, &jar_set);
    let jdk_part = (!jdk_names.is_empty())
        .then(|| part_coverage(&unit, &jdk_names.iter().map(String::as_str).collect()));
    Ok(Coverage {
        schema_version: 2,
        classes: tried,
        unparsable_classes: unparsable,
        methods: jar.methods,
        compiled_methods: jar.compiled_methods,
        self_contained_methods: jar.self_contained_methods,
        blockers: jar.blockers,
        jdk: jdk_part,
    })
}
