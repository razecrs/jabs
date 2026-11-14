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
            std::io::SeekFrom::Start(n) => std::io::SeekFrom::Start(n + 4),
            other => other,
        };
        Ok(self.0.seek(shifted)?.saturating_sub(4))
    }
}

/// The JDK modules (by name) that contain at least one of `classes` (internal names), from the
/// module files' directories alone. A JVM run's loaded classes give the modules a program uses.
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
