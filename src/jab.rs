//! The `.jab`: the class path and `java.home` of a native JABS program, as one uncompressed
//! (STORED) zip that is also a valid jar.
//!
//! Layout (every name in jar form: `/` separators, directories end in `/`):
//! - the application's class files and other resources at the paths their jars had, so
//!   `ClassLoader.getResources("pkg")`, `JarFile` and `JarInputStream` (log4j's plugin scan,
//!   ASM-style class readers) see what they see on the JVM;
//! - `jabs-home/`: exactly the JDK files a recorded run of the program touched under
//!   `java.home` (see [`java_home_entries`]); the native program's `java.home` is this
//!   directory inside the archive;
//! - `META-INF/jabs/jab.properties`: the format version and the archive's id, which the
//!   program compiled with it checks so a stale `.jab` never runs with a newer program.
//!
//! Every parent directory of an entry has its own directory entry, as `jar` writes them (a
//! package lookup such as `getResources("net/minecraft")` finds the directory entry). Order is
//! `META-INF/` and `META-INF/MANIFEST.MF` first (`JarInputStream` finds the manifest only
//! there), then byte order of the names; every timestamp is 1980-01-01 00:00 (the zip epoch),
//! so the same inputs always give the same bytes.
//!
//! The archive starts at offset 0 of its own file and its offsets count from there. Appended to
//! an executable (`jabs pack`), the offsets still count from the archive's first byte, which
//! zip readers (the JDK's ZipFile included) locate from the end-of-central-directory record, as
//! for a self-extracting archive. The runtime side is runtime/jabs_jab.c.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// The directory inside the archive that is the program's `java.home`.
pub const HOME_DIR: &str = "jabs-home/";
/// The entry that identifies an archive as a `.jab` (format and id).
pub const MARKER: &str = "META-INF/jabs/jab.properties";
/// The format version in the marker; runtime/jabs_jab.c accepts only this one.
pub const FORMAT: u32 = 1;
/// Plain zip limits (no zip64): the entry count field and 32-bit offsets.
pub const MAX_ENTRIES: usize = 65_535;
pub const MAX_ARCHIVE_BYTES: u64 = u32::MAX as u64;
/// An entry name and its contents (empty for a directory).
pub type NamedBytes = (String, Vec<u8>);
/// The longest entry name a zip header holds.
const MAX_NAME_BYTES: usize = 0xFFFF;
/// Upper bounds on a java.home list, against a runaway recording.
const MAX_HOME_LINES: usize = 100_000;
const MAX_HOME_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LISTED_DIRECTORY: usize = 10_000;
/// All java.home files together: a listing of a big directory (`lib/*` holds the 140 MB module
/// image) would otherwise silently copy it all.
const MAX_HOME_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const END_SIGNATURE: u32 = 0x0605_4b50;
const LOCAL_HEADER: usize = 30;
const CENTRAL_HEADER: usize = 46;
const END_HEADER: usize = 22;
/// Version needed to extract: 1.0 (stored, no features).
const VERSION: u16 = 10;
/// General-purpose flag bit 11: names are UTF-8.
const FLAG_UTF8: u16 = 0x0800;
/// 1980-01-01 00:00:00 in MS-DOS date and time fields.
const DOS_DATE: u16 = (1 << 5) | 1;
const DOS_TIME: u16 = 0;
/// MS-DOS directory attribute, in the low byte of the external attributes.
const DOS_DIRECTORY: u32 = 0x10;

/// CRC-32 (IEEE, reflected 0xEDB88320) as zip, zlib and runtime/jabs_rt.h's jabs_crc32 compute it.
pub fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Why an entry name cannot go in a `.jab`, or None when it is a clean relative jar name.
/// Directories end in `/`; nothing else may: no empty, `.` or `..` segment, no backslash, no
/// leading `/`, no drive (`C:`), no NUL, no control character.
pub fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("is empty");
    }
    if name.len() > MAX_NAME_BYTES {
        return Some("is longer than a zip name can be");
    }
    if name.starts_with('/') {
        return Some("is absolute");
    }
    if name.contains('\\') {
        return Some("contains a backslash");
    }
    if name.chars().any(|c| c.is_control()) {
        return Some("contains a control character");
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    if body.is_empty() {
        return Some("is only a slash");
    }
    for segment in body.split('/') {
        if segment.is_empty() {
            return Some("has an empty path segment");
        }
        if segment == "." || segment == ".." {
            return Some("has a `.` or `..` segment");
        }
    }
    if body.split('/').next().is_some_and(|s| s.ends_with(':')) {
        return Some("starts with a drive");
    }
    None
}

/// The entries of a `.jab` being assembled: name -> (contents, where it came from). Directory
/// names end in `/` and have no contents.
#[derive(Default)]
pub struct Builder {
    entries: BTreeMap<String, (Vec<u8>, String)>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    /// Add a file. A `.jab` is one class-path entry, so accepting the same path twice would
    /// erase class-path ordering and make `getResources` return fewer results than the JVM.
    pub fn add_file(&mut self, name: &str, data: Vec<u8>, origin: &str) -> Result<()> {
        if let Some(problem) = name_problem(name) {
            bail!("{origin}: entry name `{name}` {problem}");
        }
        if name.ends_with('/') {
            bail!("{origin}: file entry `{name}` ends in `/`");
        }
        if name == MARKER {
            bail!("{origin}: `{name}` is reserved for the .jab's own marker");
        }
        if let Some((_, from)) = self.entries.get(name) {
            bail!("`{name}` is in both {from} and {origin}");
        }
        self.entries
            .insert(name.to_string(), (data, origin.to_string()));
        Ok(())
    }

    /// Add a directory (`name` ends in `/`); adding one twice is harmless.
    pub fn add_directory(&mut self, name: &str, origin: &str) -> Result<()> {
        if let Some(problem) = name_problem(name) {
            bail!("{origin}: entry name `{name}` {problem}");
        }
        if !name.ends_with('/') {
            bail!("{origin}: directory entry `{name}` does not end in `/`");
        }
        self.entries
            .entry(name.to_string())
            .or_insert_with(|| (Vec::new(), origin.to_string()));
        Ok(())
    }

    /// Every input's entries, as a class path of these inputs holds them: each jar's files
    /// and directories (signature files left out, as JABS's embedded resources always have
    /// been: the classes are compiled, so a signature over them would no longer be true), and
    /// each loose `.class` file at its class's path.
    pub fn add_inputs(&mut self, inputs: &[PathBuf]) -> Result<()> {
        for input in inputs {
            let label = input.display().to_string();
            if input
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("class"))
            {
                let bytes = fs::read(input).with_context(|| format!("read {label}"))?;
                let class = crate::classfile::ClassFile::parse(&bytes)
                    .with_context(|| format!("parse {label}"))?;
                self.add_file(&format!("{}.class", class.name), bytes, &label)?;
                continue;
            }
            let file = fs::File::open(input).with_context(|| format!("open {label}"))?;
            let mut zip =
                zip::ZipArchive::new(file).with_context(|| format!("read zip {label}"))?;
            if zip.len() > MAX_ENTRIES {
                bail!(
                    "{label} has {} entries; a .jab holds at most {MAX_ENTRIES}",
                    zip.len()
                );
            }
            for index in 0..zip.len() {
                let mut entry = zip.by_index(index)?;
                let name = entry.name().to_string();
                if is_signature_file(&name) {
                    continue;
                }
                if entry.is_dir() {
                    self.add_directory(&name, &label)?;
                    continue;
                }
                if entry.size() > MAX_ARCHIVE_BYTES {
                    bail!("{label}: {name} is larger than a .jab can hold");
                }
                let mut data = Vec::with_capacity(entry.size() as usize);
                std::io::Read::read_to_end(&mut entry, &mut data)
                    .with_context(|| format!("read {name} from {label}"))?;
                self.add_file(&name, data, &label)?;
            }
        }
        Ok(())
    }

    /// The archive: parent directories made explicit, the marker added, written in the
    /// canonical order. Returns (bytes, id); the id is what the program compiled with this
    /// archive embeds (cgen's `JabConfig::id`).
    pub fn finish(mut self) -> Result<(Vec<u8>, String)> {
        self.add_directory(HOME_DIR, "the .jab layout")?;
        let names: Vec<String> = self.entries.keys().cloned().collect();
        for name in &names {
            let body = name.strip_suffix('/').unwrap_or(name);
            // Bounded: a name has at most as many parents as it has slashes.
            for (at, byte) in body.bytes().enumerate() {
                if byte != b'/' {
                    continue;
                }
                if self.entries.contains_key(&body[..at]) {
                    bail!("`{}` is both a file and a directory", &body[..at]);
                }
                self.add_directory(&body[..=at], "a parent directory")?;
            }
        }
        // A file named like a directory's path without the slash ("a" and "a/b") was caught
        // above for parents; an explicit directory "a/" next to a file "a" is caught here.
        for name in self.entries.keys() {
            if let Some(body) = name.strip_suffix('/')
                && self.entries.contains_key(body)
            {
                bail!("`{body}` is both a file and a directory");
            }
        }
        let id = archive_id(&self.entries);
        let marker = format!("format={FORMAT}\nid={id}\n");
        self.entries.insert(
            MARKER.to_string(),
            (marker.into_bytes(), "the .jab layout".to_string()),
        );
        self.entries
            .insert("META-INF/jabs/".to_string(), (Vec::new(), String::new()));
        self.entries
            .insert("META-INF/".to_string(), (Vec::new(), String::new()));
        let mut ordered: Vec<(&str, &[u8])> = Vec::with_capacity(self.entries.len());
        for first in ["META-INF/", "META-INF/MANIFEST.MF"] {
            if let Some((data, _)) = self.entries.get(first) {
                ordered.push((first, data));
            }
        }
        for (name, (data, _)) in &self.entries {
            if name != "META-INF/" && name != "META-INF/MANIFEST.MF" {
                ordered.push((name, data));
            }
        }
        Ok((write(&ordered)?, id))
    }
}

/// Signature files JAR verification reads: `META-INF/*.SF`, `.RSA`, `.DSA`, `.EC`.
fn is_signature_file(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("META-INF/")
        && !upper[9..].contains('/')
        && [".SF", ".RSA", ".DSA", ".EC"]
            .iter()
            .any(|x| upper.ends_with(x))
}

/// SHA-256 over every entry's name and contents (lengths included, so no two archives with
/// different entries share it), in name order; hex.
fn archive_id(entries: &BTreeMap<String, (Vec<u8>, String)>) -> String {
    let mut hash = Sha256::new();
    for (name, (data, _)) in entries {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((data.len() as u64).to_le_bytes());
        hash.update(data);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A STORED zip of these entries, in this order, with fixed timestamps.
pub fn write(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    if entries.len() > MAX_ENTRIES {
        bail!(
            "{} entries; a .jab (a zip without zip64) holds at most {MAX_ENTRIES}",
            entries.len()
        );
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        if let Some(problem) = name_problem(name) {
            bail!("entry name `{name}` {problem}");
        }
        if !seen.insert(*name) {
            bail!("entry `{name}` appears twice");
        }
        let directory = name.ends_with('/');
        if directory && !data.is_empty() {
            bail!("directory entry `{name}` has contents");
        }
        let offset = u32::try_from(out.len()).context("the .jab is larger than 4 GiB")?;
        let size = u32::try_from(data.len()).context("an entry is larger than 4 GiB")?;
        let name_length = u16::try_from(name.len()).context("an entry name is too long")?;
        let crc = crc32(data);
        put32(&mut out, LOCAL_SIGNATURE);
        put16(&mut out, VERSION);
        put16(&mut out, FLAG_UTF8);
        put16(&mut out, 0); // stored
        put16(&mut out, DOS_TIME);
        put16(&mut out, DOS_DATE);
        put32(&mut out, crc);
        put32(&mut out, size);
        put32(&mut out, size);
        put16(&mut out, name_length);
        put16(&mut out, 0); // no extra field
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        put32(&mut central, CENTRAL_SIGNATURE);
        put16(&mut central, VERSION); // made by: MS-DOS host, version 1.0
        put16(&mut central, VERSION);
        put16(&mut central, FLAG_UTF8);
        put16(&mut central, 0);
        put16(&mut central, DOS_TIME);
        put16(&mut central, DOS_DATE);
        put32(&mut central, crc);
        put32(&mut central, size);
        put32(&mut central, size);
        put16(&mut central, name_length);
        put16(&mut central, 0); // extra
        put16(&mut central, 0); // comment
        put16(&mut central, 0); // disk
        put16(&mut central, 0); // internal attributes
        put32(&mut central, if directory { DOS_DIRECTORY } else { 0 });
        put32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = u32::try_from(out.len()).context("the .jab is larger than 4 GiB")?;
    let central_size = u32::try_from(central.len()).context("the central directory is too big")?;
    out.extend_from_slice(&central);
    let count = u16::try_from(entries.len()).context("too many entries")?;
    put32(&mut out, END_SIGNATURE);
    put16(&mut out, 0);
    put16(&mut out, 0);
    put16(&mut out, count);
    put16(&mut out, count);
    put32(&mut out, central_size);
    put32(&mut out, central_offset);
    put16(&mut out, 0); // no comment
    if out.len() as u64 > MAX_ARCHIVE_BYTES {
        bail!("the .jab is larger than 4 GiB");
    }
    Ok(out)
}

/// One entry of a read archive: its name and where its contents are in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub data_start: u64,
    pub size: u64,
    pub crc: u32,
}

/// A `.jab` found in a file: where the archive starts (0 for a `.jab`, the executable's size
/// for a packed one), its entries and its id.
#[derive(Debug)]
pub struct Archive {
    pub base: u64,
    pub entries: Vec<Entry>,
    pub id: String,
}

fn get16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Read a `.jab` that ends `bytes`, as strictly as runtime/jabs_jab.c does: an end record with
/// no comment as the last 22 bytes, only STORED entries whose local and central headers agree,
/// correct CRCs, clean unique names, every parent directory present, and the marker. `Ok(None)`
/// when `bytes` does not end in an end record at all (an executable nothing was appended to).
pub fn read(bytes: &[u8]) -> Result<Option<Archive>> {
    if bytes.len() < END_HEADER || get32(bytes, bytes.len() - END_HEADER) != END_SIGNATURE {
        return Ok(None);
    }
    let end = bytes.len() - END_HEADER;
    let count = usize::from(get16(bytes, end + 10));
    let central_size = get32(bytes, end + 12) as usize;
    let central_offset = get32(bytes, end + 16) as usize;
    if get16(bytes, end + 4) != 0 || get16(bytes, end + 6) != 0 {
        bail!("the end record names another disk (a split archive)");
    }
    if usize::from(get16(bytes, end + 8)) != count {
        bail!("the end record's two entry counts differ");
    }
    if get16(bytes, end + 20) != 0 {
        bail!("the end record has a comment (a .jab has none)");
    }
    let Some(central_start) = end.checked_sub(central_size) else {
        bail!("the central directory is larger than the file before it");
    };
    let Some(base) = central_start.checked_sub(central_offset) else {
        bail!("the central directory offset points before the start of the file");
    };
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    let mut at = central_start;
    for index in 0..count {
        if at + CENTRAL_HEADER > end || get32(bytes, at) != CENTRAL_SIGNATURE {
            bail!("central directory entry {index} is missing or damaged");
        }
        let method = get16(bytes, at + 10);
        let crc = get32(bytes, at + 16);
        let compressed = get32(bytes, at + 20) as u64;
        let size = get32(bytes, at + 24) as u64;
        let name_length = usize::from(get16(bytes, at + 28));
        let extra = usize::from(get16(bytes, at + 30));
        let comment = usize::from(get16(bytes, at + 32));
        let local = get32(bytes, at + 42) as usize;
        let name_end = at + CENTRAL_HEADER + name_length;
        if name_end + extra + comment > end {
            bail!("central directory entry {index} runs past the directory");
        }
        let Ok(name) = std::str::from_utf8(&bytes[at + CENTRAL_HEADER..name_end]) else {
            bail!("central directory entry {index}: the name is not UTF-8");
        };
        if let Some(problem) = name_problem(name) {
            bail!("entry `{name}` {problem}");
        }
        if method != 0 || compressed != size {
            bail!("entry `{name}` is compressed (a .jab stores every entry uncompressed)");
        }
        if get16(bytes, at + 8) & 0x0001 != 0 {
            bail!("entry `{name}` is encrypted");
        }
        let local_at = base + local;
        if local_at + LOCAL_HEADER > central_start || get32(bytes, local_at) != LOCAL_SIGNATURE {
            bail!("entry `{name}`: no local header where the directory says");
        }
        let local_name = usize::from(get16(bytes, local_at + 26));
        let local_extra = usize::from(get16(bytes, local_at + 28));
        let data_start = local_at + LOCAL_HEADER + local_name + local_extra;
        let same = get16(bytes, local_at + 8) == method
            && get32(bytes, local_at + 14) == crc
            && u64::from(get32(bytes, local_at + 18)) == size
            && u64::from(get32(bytes, local_at + 22)) == size
            && local_name == name_length
            && local_at + LOCAL_HEADER + local_name <= central_start
            && &bytes[local_at + LOCAL_HEADER..local_at + LOCAL_HEADER + local_name]
                == name.as_bytes();
        if !same {
            bail!("entry `{name}`: the local header disagrees with the central directory");
        }
        if get16(bytes, local_at + 6) & 0x0008 != 0 {
            bail!("entry `{name}` has a data descriptor (sizes after the data)");
        }
        let Some(data_end) = (data_start as u64).checked_add(size) else {
            bail!("entry `{name}` is too large");
        };
        if data_end > central_start as u64 {
            bail!("entry `{name}` runs into the central directory");
        }
        let data = &bytes[data_start..data_end as usize];
        if crc32(data) != crc {
            bail!("entry `{name}`: bad CRC (the .jab is damaged)");
        }
        if name.ends_with('/') && size != 0 {
            bail!("directory entry `{name}` has contents");
        }
        entries.push(Entry {
            name: name.to_string(),
            data_start: data_start as u64,
            size,
            crc,
        });
        at = name_end + extra + comment;
    }
    if at != end {
        bail!("the central directory is not the size the end record says");
    }
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    if let Some(pair) = names.windows(2).find(|w| w[0] == w[1]) {
        bail!("entry `{}` appears twice", pair[0]);
    }
    for name in &names {
        let body = name.strip_suffix('/').unwrap_or(name);
        if let Some(slash) = body.rfind('/') {
            let parent = &body[..=slash];
            if names.binary_search(&parent).is_err() {
                bail!("entry `{name}` has no directory entry for `{parent}`");
            }
        }
    }
    let Some(marker) = entries.iter().find(|e| e.name == MARKER) else {
        bail!("no {MARKER}: not a .jab");
    };
    let start = marker.data_start as usize;
    let text = std::str::from_utf8(&bytes[start..start + marker.size as usize])
        .context("the marker is not UTF-8")?;
    let format_line = format!("format={FORMAT}");
    let mut lines = text.lines();
    if lines.next() != Some(format_line.as_str()) {
        bail!("{MARKER} does not say {format_line}");
    }
    let Some(id) = lines.next().and_then(|l| l.strip_prefix("id=")) else {
        bail!("{MARKER} has no id");
    };
    Ok(Some(Archive {
        base: base as u64,
        entries,
        id: id.to_string(),
    }))
}

/// Append a `.jab` to an executable: the portable program, one file that is both the
/// executable and the archive. Refuses an executable that already ends in an archive and a
/// `.jab` that does not read back cleanly.
pub fn pack(exe: &Path, jab: &Path, out: &Path) -> Result<()> {
    let program = fs::read(exe).with_context(|| format!("read {}", exe.display()))?;
    let archive = fs::read(jab).with_context(|| format!("read {}", jab.display()))?;
    match read(&archive).with_context(|| format!("{} is not a valid .jab", jab.display()))? {
        Some(found) if found.base == 0 => {}
        Some(_) => bail!(
            "{} has bytes before its archive (already packed?)",
            jab.display()
        ),
        None => bail!("{} is not a zip (no end record)", jab.display()),
    }
    // The runtime takes an end record in the executable's last 22 bytes as a packed .jab, so an
    // executable that already ends in one (valid or not) cannot take another.
    if !matches!(read(&program), Ok(None)) {
        bail!("{} already ends in a zip end record", exe.display());
    }
    let mut joined = program;
    joined.extend_from_slice(&archive);
    match read(&joined)? {
        Some(found) if found.base == (joined.len() - archive.len()) as u64 => {}
        _ => bail!("the packed program does not read back as a .jab"),
    }
    fs::write(out, &joined).with_context(|| format!("write {}", out.display()))
}

/// The `jabs-home/` entries for the JDK files a recorded run touched under `java.home`.
///
/// The list is what a `--record-java-home` build of the program wrote while it ran (one path
/// per line, relative to `java.home`, `/`-separated; `#` comments). Each line is resolved
/// against `home` now: a regular file is copied, a directory becomes a directory entry, a path
/// that does not exist is left out (the run found it missing, and it is missing in the `.jab`
/// too). `dir/*` means the run listed that directory: its direct files are copied and its
/// subdirectories become directory entries. Returns the entries and how many listed paths were
/// missing.
pub fn java_home_entries(home: &Path, list: &Path) -> Result<(Vec<NamedBytes>, usize)> {
    let text = fs::read_to_string(list).with_context(|| format!("read {}", list.display()))?;
    let mut wanted: BTreeMap<String, bool> = BTreeMap::new(); // path -> listed (`/*`)
    for (index, raw) in text.lines().enumerate() {
        if index >= MAX_HOME_LINES {
            bail!("{}: more than {MAX_HOME_LINES} lines", list.display());
        }
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (path, listed) = match line.strip_suffix("/*") {
            Some(dir) => (dir, true),
            None => (line.strip_suffix('/').unwrap_or(line), false),
        };
        if let Some(problem) = name_problem(path) {
            bail!(
                "{}:{}: `{line}` {problem} (paths are relative to java.home, with `/`)",
                list.display(),
                index + 1
            );
        }
        let entry = wanted.entry(path.to_string()).or_insert(false);
        *entry |= listed;
    }
    let mut out: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut missing = 0;
    for (path, listed) in &wanted {
        let source = home.join(path);
        let Ok(meta) = fs::metadata(&source) else {
            missing += 1;
            continue;
        };
        if meta.is_dir() {
            out.insert(format!("{HOME_DIR}{path}/"), Vec::new());
            if !listed {
                continue;
            }
            let mut children: Vec<PathBuf> = fs::read_dir(&source)
                .with_context(|| format!("list {}", source.display()))?
                .map(|e| e.map(|e| e.path()))
                .collect::<std::io::Result<_>>()
                .with_context(|| format!("list {}", source.display()))?;
            if children.len() > MAX_LISTED_DIRECTORY {
                bail!(
                    "{} has more than {MAX_LISTED_DIRECTORY} entries",
                    source.display()
                );
            }
            children.sort();
            for child in children {
                let Some(name) = child.file_name().and_then(|n| n.to_str()) else {
                    bail!("{}: a name that is not UTF-8", source.display());
                };
                let relative = format!("{path}/{name}");
                if child.is_dir() {
                    out.insert(format!("{HOME_DIR}{relative}/"), Vec::new());
                } else {
                    out.insert(format!("{HOME_DIR}{relative}"), read_home_file(&child)?);
                }
            }
            continue;
        }
        if *listed {
            bail!(
                "{}: `{path}/*` lists a directory, but {} is a file",
                list.display(),
                source.display()
            );
        }
        out.insert(format!("{HOME_DIR}{path}"), read_home_file(&source)?);
    }
    let total: u64 = out.values().map(|d| d.len() as u64).sum();
    if total > MAX_HOME_TOTAL_BYTES {
        bail!(
            "{}: the listed java.home files come to {total} bytes, more than {MAX_HOME_TOTAL_BYTES}; a `dir/*` line copies a whole directory",
            list.display()
        );
    }
    Ok((out.into_iter().collect(), missing))
}

fn read_home_file(path: &Path) -> Result<Vec<u8>> {
    let size = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size > MAX_HOME_FILE_BYTES {
        bail!(
            "{} is larger than {MAX_HOME_FILE_BYTES} bytes",
            path.display()
        );
    }
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

/// Add the java.home entries ([`java_home_entries`]) to a builder.
pub fn add_java_home(builder: &mut Builder, entries: Vec<NamedBytes>, origin: &str) -> Result<()> {
    for (name, data) in entries {
        if name.ends_with('/') {
            builder.add_directory(&name, origin)?;
        } else {
            builder.add_file(&name, data, origin)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn names_that_escape_or_are_ambiguous_are_refused() {
        for bad in [
            "", "/abs", "a\\b", "a//b", "a/../b", "./a", "..", "C:/x", "a\u{0}b", "/",
        ] {
            assert!(name_problem(bad).is_some(), "accepted `{bad}`");
        }
        for good in ["a", "a/", "a/b.class", "META-INF/MANIFEST.MF", "x.y/z"] {
            assert_eq!(name_problem(good), None, "refused `{good}`");
        }
    }

    #[test]
    fn written_archives_read_back_with_parents_and_marker() {
        let mut b = Builder::new();
        b.add_file("a/b/C.class", vec![1, 2, 3], "test").unwrap();
        b.add_file(
            "META-INF/MANIFEST.MF",
            b"Manifest-Version: 1.0\n".to_vec(),
            "test",
        )
        .unwrap();
        let (bytes, id) = b.finish().unwrap();
        let archive = read(&bytes).unwrap().unwrap();
        assert_eq!(archive.base, 0);
        assert_eq!(archive.id, id);
        let names: Vec<&str> = archive.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "META-INF/",
                "META-INF/MANIFEST.MF",
                "META-INF/jabs/",
                "META-INF/jabs/jab.properties",
                "a/",
                "a/b/",
                "a/b/C.class",
                "jabs-home/"
            ]
        );
    }

    #[test]
    fn duplicate_files_are_refused_even_when_contents_match() {
        let mut b = Builder::new();
        b.add_file("x", vec![1], "one").unwrap();
        let error = b.add_file("x", vec![1], "two").unwrap_err().to_string();
        assert!(error.contains("one") && error.contains("two"), "{error}");
        assert!(write(&[("x", &[1][..]), ("x", &[1][..])]).is_err());
    }

    #[test]
    fn a_file_and_a_directory_of_one_name_are_refused() {
        let mut b = Builder::new();
        b.add_file("a", vec![1], "t").unwrap();
        b.add_file("a/b", vec![1], "t").unwrap();
        assert!(b.finish().is_err());
    }
}
