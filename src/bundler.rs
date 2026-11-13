//! Mojang's server "bundler" jars (1.18+): nested game and library jars plus a launcher
//! that loads them through its own class loader. Unbundling puts them on the real
//! classpath, which the JVM's AOT cache requires.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub struct Unbundled {
    pub main_class: String,
    /// Jars in the bundler's own classpath order (`META-INF/classpath-joined`), absolute.
    pub classpath: Vec<PathBuf>,
    /// Whether this call extracted files (false: verified and reused an earlier extraction).
    pub extracted: bool,
}

/// Read a small text entry from the bundler.
fn read_text(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<String> {
    let mut text = String::new();
    archive
        .by_name(name)
        .with_context(|| format!("bundler is missing {name}"))?
        .take(4 * 1024 * 1024)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// Whether a jar is a Mojang bundler (it has `META-INF/main-class` and `META-INF/classpath-joined`).
pub fn is_bundler(jar: &Path) -> Result<bool> {
    let archive =
        zip::ZipArchive::new(File::open(jar).with_context(|| format!("open {}", jar.display()))?)?;
    Ok(archive.index_for_name("META-INF/main-class").is_some()
        && archive
            .index_for_name("META-INF/classpath-joined")
            .is_some())
}

/// Reject absolute paths and `..` so archive entries cannot escape the destination.
fn safe_relative(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if path.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("unsafe path in bundler metadata: {path}");
    }
    Ok(p.to_path_buf())
}

/// SHA-256 of a file, streamed.
fn sha256_file(path: &Path) -> Result<String> {
    Ok(crate::profile::hash_file(path)?.1)
}

/// Whether a zip entry is part of a jar signature (`META-INF/*.SF`, `.RSA`, `.DSA`, `.EC`).
fn is_signature_entry(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("META-INF/")
        && !upper["META-INF/".len()..].contains('/')
        && [".SF", ".RSA", ".DSA", ".EC"]
            .iter()
            .any(|ext| upper.ends_with(ext))
}

/// Copy a jar without its signature files; entries are copied raw (no recompression).
fn write_unsigned(signed: &[u8], dest: &Path) -> Result<()> {
    let mut source = zip::ZipArchive::new(std::io::Cursor::new(signed))?;
    let mut out = zip::ZipWriter::new(File::create(dest)?);
    for i in 0..source.len() {
        let entry = source.by_index_raw(i)?;
        if !is_signature_entry(entry.name()) {
            out.raw_copy_file(entry)?;
        }
    }
    out.finish()?;
    Ok(())
}

/// Extract (or verify and reuse) the bundler's jars under `root/<bundler sha256>[-unsigned]/`.
/// Every nested jar is checked against the SHA-256 the bundler lists for it. With
/// `strip_signatures`, the game jars (`versions/`) are stored without signature files.
pub fn unbundle(
    jar: &Path,
    bundler_sha256: &str,
    root: &Path,
    strip_signatures: bool,
) -> Result<Unbundled> {
    let mut archive =
        zip::ZipArchive::new(File::open(jar).with_context(|| format!("open {}", jar.display()))?)?;
    let main_class = read_text(&mut archive, "META-INF/main-class")?
        .trim()
        .to_string();
    let mut expected: HashMap<String, String> = HashMap::new();
    for (list, prefix) in [
        ("META-INF/libraries.list", "libraries/"),
        ("META-INF/versions.list", "versions/"),
    ] {
        for line in read_text(&mut archive, list)?
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let fields: Vec<&str> = line.split('\t').collect();
            let [sha, _id, path] = fields[..] else {
                bail!("malformed {list} line: {line}")
            };
            expected.insert(format!("{prefix}{path}"), sha.to_ascii_lowercase());
        }
    }
    let order: Vec<String> = read_text(&mut archive, "META-INF/classpath-joined")?
        .trim()
        .split(';')
        .map(str::to_string)
        .collect();

    let dir = root.join(if strip_signatures {
        format!("{bundler_sha256}-unsigned")
    } else {
        bundler_sha256.to_string()
    });
    let complete = dir.join(".complete");
    let mut classpath = Vec::new();
    let reuse = complete.exists();
    for rel in &order {
        let sha = expected
            .get(rel)
            .with_context(|| format!("{rel} has no listed SHA-256"))?;
        let dest = dir.join(safe_relative(rel)?);
        let unsigned = strip_signatures && rel.starts_with("versions/");
        if reuse {
            // Unsigned copies differ from the listed hash; their recorded hash is checked instead.
            let want = if unsigned {
                fs::read_to_string(dest.with_extension("jar.sha256"))?
            } else {
                sha.clone()
            };
            if sha256_file(&dest)? != want.trim() {
                bail!(
                    "{} changed since extraction; delete {} to re-extract",
                    dest.display(),
                    dir.display()
                );
            }
        } else {
            let mut data = Vec::new();
            archive
                .by_name(&format!("META-INF/{rel}"))
                .with_context(|| format!("bundler is missing META-INF/{rel}"))?
                .take(512 * 1024 * 1024)
                .read_to_end(&mut data)?;
            let actual = format!("{:x}", Sha256::digest(&data));
            if &actual != sha {
                bail!("bundled {rel} has SHA-256 {actual}, but the bundler lists {sha}");
            }
            fs::create_dir_all(dest.parent().unwrap())?;
            if unsigned {
                write_unsigned(&data, &dest)?;
                fs::write(dest.with_extension("jar.sha256"), sha256_file(&dest)?)?;
            } else {
                File::create(&dest)?.write_all(&data)?;
            }
        }
        classpath.push(std::path::absolute(&dest)?);
    }
    if !reuse {
        fs::write(&complete, "")?;
    }
    Ok(Unbundled {
        main_class,
        classpath,
        extracted: !reuse,
    })
}
