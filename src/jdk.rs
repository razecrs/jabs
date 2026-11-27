//! Reading the Java class library from an installed JDK.
//!
//! JDK `.jmod` files are a 4-byte header (`JM` 0x01 0x00) followed by a zip whose
//! classes live under `classes/`. The library is read from the user's own JDK and
//! anything compiled from it stays on their machine (OpenJDK is GPLv2 with the
//! Classpath Exception; distributing compiled output would carry source obligations).

use crate::classfile::ClassFile;
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

const JMOD_MAGIC: [u8; 4] = [b'J', b'M', 0x01, 0x00];
/// Upper bound on a module file; java.base.jmod is around 25 MB.
const MAX_JMOD_BYTES: u64 = 512 * 1024 * 1024;

/// `java.home` of a Java executable, as the JVM itself reports it.
pub fn java_home(java: &Path) -> Result<PathBuf> {
    let output = Command::new(java)
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .with_context(|| format!("run {}", java.display()))?;
    let text = String::from_utf8_lossy(&output.stderr);
    let home = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("java.home = "))
        .with_context(|| format!("{} did not report java.home", java.display()))?;
    Ok(PathBuf::from(home.trim()))
}

/// Classes of one JDK module (e.g. `java.base`): (parsed classes, entries tried, unparsable).
pub fn read_module(home: &Path, module: &str) -> Result<(Vec<ClassFile>, usize, usize)> {
    let path = home.join("jmods").join(format!("{module}.jmod"));
    let size = fs::metadata(&path)
        .with_context(|| {
            format!(
                "{} not found; this JDK has no jmods (JDK 24+ builds may omit them), which JABS needs for now",
                path.display()
            )
        })?
        .len();
    if size > MAX_JMOD_BYTES {
        bail!(
            "{} is {size} bytes; limit is {MAX_JMOD_BYTES}",
            path.display()
        );
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() < 4 || bytes[..4] != JMOD_MAGIC {
        bail!("{} is not a jmod file", path.display());
    }
    crate::jar::classes_from_zip_bytes(&bytes[4..], "classes/", &path.display().to_string())
}

/// A jmod file seen as its zip: every position is shifted past the 4-byte header, so the zip
/// directory can be read without loading the whole file.
struct JmodZip(fs::File);

impl std::io::Read for JmodZip {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Seek for JmodZip {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let shifted = match pos {
            std::io::SeekFrom::Start(n) => {
                std::io::SeekFrom::Start(n.checked_add(4).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "jmod seek offset overflow",
                    )
                })?)
            }
            other => other,
        };
        let actual = self.0.seek(shifted)?;
        actual.checked_sub(4).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start of jmod zip",
            )
        })
    }
}

fn jmod_paths(home: &Path, empty_err: Option<&str>) -> Result<Vec<PathBuf>> {
    let dir = home.join("jmods");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect();
    entries.sort();
    if entries.is_empty()
        && let Some(what) = empty_err
    {
        bail!("{} has no jmods: {what}", dir.display());
    }
    Ok(entries)
}

fn open_jmod_zip(path: &Path) -> Result<zip::ZipArchive<JmodZip>> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("read {}", path.display()))?;
    if magic != JMOD_MAGIC {
        bail!("{} is not a jmod", path.display());
    }
    zip::ZipArchive::new(JmodZip(file))
        .with_context(|| format!("read the zip in {}", path.display()))
}

fn collect_archive_class_names(archive: &zip::ZipArchive<JmodZip>, out: &mut Vec<String>) {
    for name in archive.file_names() {
        if let Some(class) = name
            .strip_prefix("classes/")
            .and_then(|n| n.strip_suffix(".class"))
            .filter(|n| !n.ends_with("module-info"))
        {
            out.push(class.to_string());
        }
    }
}

fn read_module_info(path: &Path, archive: &mut zip::ZipArchive<JmodZip>) -> Result<JdkModule> {
    let entry = archive
        .by_name("classes/module-info.class")
        .with_context(|| format!("{} has no classes/module-info.class", path.display()))?;
    if entry.size() > MAX_MODULE_INFO_BYTES {
        bail!(
            "{}: module-info.class is {} bytes; limit is {MAX_MODULE_INFO_BYTES}",
            path.display(),
            entry.size()
        );
    }
    let mut bytes = Vec::with_capacity(entry.size().min(MAX_MODULE_INFO_BYTES) as usize);
    entry
        .take(MAX_MODULE_INFO_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read module-info.class in {}", path.display()))?;
    if bytes.len() as u64 > MAX_MODULE_INFO_BYTES {
        bail!("{}: module-info.class is too large", path.display());
    }
    let class = ClassFile::parse(&bytes)
        .with_context(|| format!("parse module-info.class in {}", path.display()))?;
    let module = class.reflect.module;
    let Some(declaration) = module.declaration else {
        bail!(
            "{}: module-info.class has no Module attribute",
            path.display()
        );
    };
    let Some(packages) = module.packages else {
        bail!(
            "{}: module-info.class has no ModulePackages attribute",
            path.display()
        );
    };
    Ok(JdkModule {
        declaration,
        packages,
        resolution: module.resolution.unwrap_or(0),
        class_bytes: bytes,
    })
}

fn run_jmod_workers<T: Send>(
    entries: &[PathBuf],
    f: impl Fn(&Path) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    if entries.len() <= 1 {
        return entries.iter().map(|p| f(p)).collect();
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(entries.len());
    let chunk_size = entries.len().div_ceil(workers);
    let results: Vec<Result<Vec<T>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = entries
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(|| chunk.iter().map(|p| f(p)).collect::<Result<Vec<T>>>()))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("jmod worker thread panicked"))
            .collect()
    });
    let mut out = Vec::with_capacity(entries.len());
    for chunk in results {
        out.extend(chunk?);
    }
    Ok(out)
}

/// Every class name in every module of the JDK at `home` (internal names, from the jmods): the
/// evidence that a class the program names exists nowhere in the JDK.
pub fn all_class_names(home: &Path) -> Result<BTreeSet<String>> {
    let entries = jmod_paths(home, Some("cannot list the JDK's classes"))?;
    let per_jmod = run_jmod_workers(&entries, |path| {
        let archive = open_jmod_zip(path)?;
        let mut names = Vec::with_capacity(archive.len());
        collect_archive_class_names(&archive, &mut names);
        Ok(names)
    })?;
    let mut out = BTreeSet::new();
    for names in per_jmod {
        out.extend(names);
    }
    Ok(out)
}

/// Largest module-info.class accepted; java.base's is about 12 KB.
const MAX_MODULE_INFO_BYTES: u64 = 1024 * 1024;

/// A JDK module as its module-info.class declares it (the evidence for Class.getModule and the
/// Module answers a program gets; see src/cgen/modules.rs).
#[derive(Debug, Clone)]
pub struct JdkModule {
    pub declaration: crate::classfile::ModuleDeclaration,
    /// Every package of the module (ModulePackages), internal form.
    pub packages: Vec<String>,
    /// ModuleResolution flags; 0 when the attribute is absent.
    pub resolution: u16,
    /// The module-info.class itself, which the program's ModuleDescriptor.read parses again.
    pub class_bytes: Vec<u8>,
}

/// The module-info.class of every jmod of the JDK at `home`, sorted by module name. A jmod
/// without one, or one without a Module or ModulePackages attribute, is an error: every module
/// of a JDK image has both (jlink and the jmod tool write ModulePackages).
pub fn modules(home: &Path) -> Result<Vec<JdkModule>> {
    let entries = jmod_paths(home, Some("cannot read the JDK's modules"))?;
    let parsed = run_jmod_workers(&entries, |path| {
        let mut archive = open_jmod_zip(path)?;
        read_module_info(path, &mut archive)
    })?;
    collect_unique_modules(&entries, parsed)
}

/// Read both [`modules`] and [`all_class_names`] in a single parallel pass over the JDK's jmods.
pub fn modules_and_class_names(home: &Path) -> Result<(Vec<JdkModule>, BTreeSet<String>)> {
    let entries = jmod_paths(home, Some("cannot read the JDK's modules"))?;
    let per_jmod = run_jmod_workers(&entries, |path| {
        let mut archive = open_jmod_zip(path)?;
        let mut names = Vec::with_capacity(archive.len());
        collect_archive_class_names(&archive, &mut names);
        let module = read_module_info(path, &mut archive)?;
        Ok((module, names))
    })?;
    let mut raw_modules = Vec::with_capacity(per_jmod.len());
    let mut class_names = BTreeSet::new();
    for (module, names) in per_jmod {
        raw_modules.push(module);
        class_names.extend(names);
    }
    let modules = collect_unique_modules(&entries, raw_modules)?;
    Ok((modules, class_names))
}

fn collect_unique_modules(entries: &[PathBuf], parsed: Vec<JdkModule>) -> Result<Vec<JdkModule>> {
    let mut seen = HashSet::with_capacity(parsed.len());
    let mut out = Vec::with_capacity(parsed.len());
    for (path, module) in entries.iter().zip(parsed) {
        if !seen.insert(module.declaration.name.clone()) {
            bail!(
                "{}: module {} is declared twice",
                path.display(),
                module.declaration.name
            );
        }
        out.push(module);
    }
    out.sort_by(|a, b| a.declaration.name.cmp(&b.declaration.name));
    Ok(out)
}

/// The JDK modules (by name) that contain at least one of `classes` (internal names), from the
/// module files' directories alone. A JVM run's loaded classes give the modules a program uses.
pub fn modules_containing(home: &Path, classes: &BTreeSet<String>) -> Result<Vec<String>> {
    let entries = jmod_paths(home, None)?;
    if entries.is_empty() || classes.is_empty() {
        return Ok(Vec::new());
    }
    let hits = run_jmod_workers(&entries, |path| {
        let Ok(mut file) = fs::File::open(path) else {
            return Ok(false);
        };
        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_err() || magic != JMOD_MAGIC {
            return Ok(false);
        }
        let Ok(archive) = zip::ZipArchive::new(JmodZip(file)) else {
            return Ok(false);
        };
        Ok(archive.file_names().any(|name| {
            name.strip_prefix("classes/")
                .and_then(|n| n.strip_suffix(".class"))
                .is_some_and(|n| classes.contains(n))
        }))
    })?;
    let mut out = Vec::new();
    for (path, used) in entries.iter().zip(hits) {
        if used {
            let module = path.file_stem().unwrap().to_string_lossy().into_owned();
            out.push(module);
        }
    }
    Ok(out)
}
