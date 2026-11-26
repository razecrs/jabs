//! Reading the Java class library from an installed JDK.
//!
//! JDK `.jmod` files are a 4-byte header (`JM` 0x01 0x00) followed by a zip whose
//! classes live under `classes/`. The library is read from the user's own JDK and
//! anything compiled from it stays on their machine (OpenJDK is GPLv2 with the
//! Classpath Exception; distributing compiled output would carry source obligations).

use crate::classfile::ClassFile;
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Cursor,
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
    let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[4..]))
        .with_context(|| format!("read zip in {}", path.display()))?;
    crate::jar::classes_from_zip(&mut archive, "classes/", &path.display().to_string())
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

/// The JDK modules (by name) that contain at least one of `classes` (internal names), from the
/// module files' directories alone. A JVM run's loaded classes give the modules a program uses.
/// Every class name in every module of the JDK at `home` (internal names, from the jmods): the
/// evidence that a class the program names exists nowhere in the JDK.
pub fn all_class_names(home: &Path) -> Result<std::collections::BTreeSet<String>> {
    use std::io::Read;
    let mut out = std::collections::BTreeSet::new();
    let dir = home.join("jmods");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect();
    entries.sort();
    if entries.is_empty() {
        anyhow::bail!(
            "{} has no jmods: cannot list the JDK's classes",
            dir.display()
        );
    }
    for path in entries {
        let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)
            .with_context(|| format!("read {}", path.display()))?;
        if magic != JMOD_MAGIC {
            anyhow::bail!("{} is not a jmod", path.display());
        }
        let archive = zip::ZipArchive::new(JmodZip(file))
            .with_context(|| format!("read the classes of {}", path.display()))?;
        for name in archive.file_names() {
            if let Some(class) = name
                .strip_prefix("classes/")
                .and_then(|n| n.strip_suffix(".class"))
                .filter(|n| !n.ends_with("module-info"))
            {
                out.insert(class.to_string());
            }
        }
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
    use std::io::Read;
    let dir = home.join("jmods");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect();
    entries.sort();
    if entries.is_empty() {
        bail!(
            "{} has no jmods: cannot read the JDK's modules",
            dir.display()
        );
    }
    let mut out: Vec<JdkModule> = Vec::new();
    for path in entries {
        let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)
            .with_context(|| format!("read {}", path.display()))?;
        if magic != JMOD_MAGIC {
            bail!("{} is not a jmod", path.display());
        }
        let mut archive = zip::ZipArchive::new(JmodZip(file))
            .with_context(|| format!("read the zip in {}", path.display()))?;
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
        let mut bytes = Vec::new();
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
        if out.iter().any(|m| m.declaration.name == declaration.name) {
            bail!(
                "{}: module {} is declared twice",
                path.display(),
                declaration.name
            );
        }
        out.push(JdkModule {
            declaration,
            packages,
            resolution: module.resolution.unwrap_or(0),
            class_bytes: bytes,
        });
    }
    out.sort_by(|a, b| a.declaration.name.cmp(&b.declaration.name));
    Ok(out)
}

pub fn modules_containing(
    home: &Path,
    classes: &std::collections::BTreeSet<String>,
) -> Result<Vec<String>> {
    use std::io::Read;
    let mut out = Vec::new();
    let dir = home.join("jmods");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("list {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
        .collect();
    entries.sort();
    for path in entries {
        let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_err() || magic != JMOD_MAGIC {
            continue;
        }
        let Ok(archive) = zip::ZipArchive::new(JmodZip(file)) else {
            continue;
        };
        let used = archive.file_names().any(|name| {
            name.strip_prefix("classes/")
                .and_then(|n| n.strip_suffix(".class"))
                .is_some_and(|n| classes.contains(n))
        });
        if used {
            let module = path.file_stem().unwrap().to_string_lossy().into_owned();
            out.push(module);
        }
    }
    Ok(out)
}
