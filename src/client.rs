//! Vanilla client install and launch on the JVM, the way the official launcher does it:
//! every file comes from Mojang's version metadata and is checked against its published SHA-1
//! before use. Sign-in is offline only (servers must have `online-mode=false`).

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
};

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// A file Mojang publishes with its hash: fetched once, verified every time it is used.
#[derive(Clone)]
struct Download {
    url: String,
    sha1: String,
    path: PathBuf,
}

/// Fetch a URL into memory.
fn fetch(url: &str) -> Result<Vec<u8>> {
    if !url.starts_with("https://") {
        bail!("refusing non-HTTPS download {url}");
    }
    let response = ureq::get(url)
        .set("User-Agent", concat!("jabs/", env!("CARGO_PKG_VERSION")))
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(1 << 30)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Lowercase hex SHA-1 of a byte slice.
fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

/// Whether a file already exists with the expected SHA-1.
fn verified(path: &Path, sha1: &str) -> bool {
    fs::read(path).is_ok_and(|b| sha1_hex(&b) == sha1)
}

/// Download (or reuse) one file, failing if its contents do not match the published hash.
fn ensure(d: &Download) -> Result<bool> {
    if verified(&d.path, &d.sha1) {
        return Ok(false);
    }
    let bytes = fetch(&d.url)?;
    let got = sha1_hex(&bytes);
    if got != d.sha1 {
        bail!(
            "SHA-1 mismatch for {}: expected {}, got {got}",
            d.url,
            d.sha1
        );
    }
    fs::create_dir_all(d.path.parent().unwrap())?;
    let partial = d.path.with_extension("part");
    fs::write(&partial, &bytes)?;
    fs::rename(&partial, &d.path)?;
    Ok(true)
}

/// Download many files on a small worker pool; returns how many were fetched (the rest were reused).
fn ensure_all(items: Vec<Download>, label: &str) -> Result<usize> {
    let total = items.len();
    let queue = Arc::new(Mutex::new(items));
    let fetched = Arc::new(Mutex::new(0usize));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let workers: Vec<_> = (0..12)
        .map(|_| {
            let (queue, fetched, errors) = (queue.clone(), fetched.clone(), errors.clone());
            thread::spawn(move || {
                loop {
                    let Some(item) = queue.lock().unwrap().pop() else {
                        break;
                    };
                    match ensure(&item) {
                        Ok(true) => *fetched.lock().unwrap() += 1,
                        Ok(false) => {}
                        Err(e) => errors.lock().unwrap().push(format!("{e:#}")),
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("download worker panicked");
    }
    let errors = errors.lock().unwrap();
    if !errors.is_empty() {
        bail!(
            "{label}: {} of {total} files failed, first: {}",
            errors.len(),
            errors[0]
        );
    }
    let fetched = *fetched.lock().unwrap();
    eprintln!("{label}: {total} files verified ({fetched} downloaded)");
    Ok(fetched)
}

/// A relative path from metadata, rejected if it could escape its directory.
fn safe_relative(path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if path.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("unsafe path in Mojang metadata: {path:?}");
    }
    Ok(p)
}

/// A 40-character lowercase hex SHA-1 from metadata.
fn sha1_field(v: &Value) -> Result<String> {
    let s = v.as_str().context("missing sha1")?;
    if s.len() != 40
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("malformed sha1 {s:?}");
    }
    Ok(s.to_string())
}

/// Mojang's OS name for this host.
fn os_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "osx",
        _ => "linux",
    }
}

/// Evaluate a launcher `rules` list: no rules means allowed; otherwise the last matching rule wins.
fn rules_allow(rules: Option<&Value>, features: &HashMap<&str, bool>) -> bool {
    let Some(rules) = rules.and_then(Value::as_array) else {
        return true;
    };
    let mut allowed = false;
    for rule in rules {
        let mut matches = true;
        if let Some(os) = rule.get("os") {
            if os
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n != os_name())
            {
                matches = false;
            }
            if let Some(arch) = os.get("arch").and_then(Value::as_str) {
                let host = match std::env::consts::ARCH {
                    "x86" => "x86",
                    "aarch64" => "arm64",
                    other => other,
                };
                matches &= arch == host;
            }
            // OS version patterns only pick workarounds for old Java builds; treat them as not matching.
            if os.get("version").is_some() {
                matches = false;
            }
        }
        if let Some(wanted) = rule.get("features").and_then(Value::as_object) {
            for (name, value) in wanted {
                let have = features.get(name.as_str()).copied().unwrap_or(false);
                matches &= value.as_bool() == Some(have);
            }
        }
        if matches {
            allowed = rule.get("action").and_then(Value::as_str) == Some("allow");
        }
    }
    allowed
}

/// Fetch (or reuse) a version's metadata JSON, verified against the version manifest.
fn version_json(root: &Path, version: &str) -> Result<Value> {
    let manifest: Value =
        serde_json::from_slice(&fetch(MANIFEST_URL)?).context("version manifest")?;
    let entry = manifest["versions"]
        .as_array()
        .context("version manifest has no versions")?
        .iter()
        .find(|v| v["id"].as_str() == Some(version))
        .with_context(|| format!("Mojang's manifest has no version {version:?}"))?;
    let path = root
        .join("versions")
        .join(safe_relative(version)?)
        .join(format!("{version}.json"));
    ensure(&Download {
        url: entry["url"].as_str().context("version url")?.into(),
        sha1: sha1_field(&entry["sha1"])?,
        path: path.clone(),
    })?;
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

/// Libraries that apply to this host, each with its identity `group:artifact[:classifier]`.
fn libraries(root: &Path, meta: &Value) -> Result<Vec<(String, Download)>> {
    let none = HashMap::new();
    let mut out = Vec::new();
    for lib in meta["libraries"]
        .as_array()
        .context("version has no libraries")?
    {
        if !rules_allow(lib.get("rules"), &none) {
            continue;
        }
        let Some(artifact) = lib.pointer("/downloads/artifact") else {
            bail!("library {} has no artifact download", lib["name"]);
        };
        let name = lib["name"].as_str().context("library name")?;
        let parts: Vec<&str> = name.split(':').collect();
        let key = match parts.as_slice() {
            [group, artifact, _version] => format!("{group}:{artifact}"),
            [group, artifact, _version, classifier, ..] => {
                format!("{group}:{artifact}:{classifier}")
            }
            _ => bail!("malformed library name {name:?}"),
        };
        out.push((
            key,
            Download {
                url: artifact["url"].as_str().context("library url")?.into(),
                sha1: sha1_field(&artifact["sha1"])?,
                path: root.join("libraries").join(safe_relative(
                    artifact["path"].as_str().context("library path")?,
                )?),
            },
        ));
    }
    Ok(out)
}

pub struct InstallReport {
    pub version: String,
    pub java_major: u32,
    pub downloaded: usize,
}

/// Download and verify everything needed to launch a vanilla client version.
pub fn install(root: &Path, version: &str) -> Result<InstallReport> {
    let meta = version_json(root, version)?;
    let mut downloaded = 0;
    let client = &meta["downloads"]["client"];
    let jar = Download {
        url: client["url"].as_str().context("client url")?.into(),
        sha1: sha1_field(&client["sha1"])?,
        path: root
            .join("versions")
            .join(version)
            .join(format!("{version}.jar")),
    };
    downloaded += ensure_all(vec![jar], "client jar")?;
    downloaded += ensure_all(
        libraries(root, &meta)?
            .into_iter()
            .map(|(_, d)| d)
            .collect(),
        "libraries",
    )?;

    let index = &meta["assetIndex"];
    let index_id = index["id"].as_str().context("asset index id")?;
    let index_path = root
        .join("assets/indexes")
        .join(format!("{}.json", safe_relative(index_id)?.display()));
    downloaded += ensure_all(
        vec![Download {
            url: index["url"].as_str().context("asset index url")?.into(),
            sha1: sha1_field(&index["sha1"])?,
            path: index_path.clone(),
        }],
        "asset index",
    )?;
    let objects: Value = serde_json::from_slice(&fs::read(&index_path)?)?;
    let mut unique = BTreeMap::new();
    for object in objects["objects"]
        .as_object()
        .context("asset index has no objects")?
        .values()
    {
        let hash = sha1_field(&object["hash"])?;
        unique.entry(hash.clone()).or_insert_with(|| Download {
            url: format!(
                "https://resources.download.minecraft.net/{}/{hash}",
                &hash[..2]
            ),
            path: root.join("assets/objects").join(&hash[..2]).join(&hash),
            sha1: hash,
        });
    }
    downloaded += ensure_all(unique.into_values().collect(), "assets")?;
    Ok(InstallReport {
        version: version.into(),
        java_major: meta
            .pointer("/javaVersion/majorVersion")
            .and_then(Value::as_u64)
            .unwrap_or(8) as u32,
        downloaded,
    })
}

/// A mod loader version installed by another launcher, used read-only.
struct LoaderVersion {
    meta: Value,
    root: PathBuf,
    libraries: Vec<(String, Download)>,
}

/// Read a loader version (e.g. `neoforge-21.1.265`) from another launcher's install root and check
/// every library it lists against its recorded SHA-1. Nothing in that root is written.
fn foreign_loader(loader_root: &Path, id: &str) -> Result<LoaderVersion> {
    let path = loader_root
        .join("versions")
        .join(safe_relative(id)?)
        .join(format!("{id}.json"));
    let meta: Value = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    if meta["inheritsFrom"].as_str().is_none() || meta.get("arguments").is_none() {
        bail!("{id} is not a modern loader version (needs inheritsFrom and arguments)");
    }
    let libraries = libraries(loader_root, &meta)?;
    if let Some((_, d)) = libraries.iter().find(|(_, d)| !verified(&d.path, &d.sha1)) {
        bail!(
            "{} is missing or does not match its recorded SHA-1; repair the instance in its own launcher",
            d.path.display()
        );
    }
    eprintln!(
        "{id}: {} loader libraries verified (read-only, in {})",
        libraries.len(),
        loader_root.display()
    );
    eprintln!(
        "{id}: the game jars its installer generated (patched Minecraft) have no published hash and are used as found"
    );
    Ok(LoaderVersion {
        meta,
        root: loader_root.into(),
        libraries,
    })
}

/// The UUID vanilla gives an offline player: `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)`.
pub fn offline_uuid(name: &str) -> String {
    let mut b: [u8; 16] = md5::Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    b[6] = (b[6] & 0x0f) | 0x30;
    b[8] = (b[8] & 0x3f) | 0x80;
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A Java executable of exactly the required major version: `--java`, then `java` on PATH,
/// then common install folders.
fn find_java(explicit: Option<&Path>, major: u32) -> Result<PathBuf> {
    let check = |p: &Path| crate::launch::java_major(p).is_ok_and(|m| m == major);
    if let Some(p) = explicit {
        if check(p) {
            return Ok(p.into());
        }
        bail!("{} is not Java {major}", p.display());
    }
    if check(Path::new("java")) {
        return Ok("java".into());
    }
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    for parent in [
        "C:/Program Files/Microsoft",
        "C:/Program Files/Eclipse Adoptium",
        "C:/Program Files/Java",
        "C:/Program Files/Zulu",
        "/usr/lib/jvm",
    ] {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join("bin").join(exe);
            if candidate.is_file() && check(&candidate) {
                return Ok(candidate);
            }
        }
    }
    bail!("no Java {major} found; install one or pass --java")
}

pub struct ClientOptions<'a> {
    pub root: &'a Path,
    pub version: &'a str,
    pub username: &'a str,
    pub game_dir: Option<&'a Path>,
    /// `host[:port]` to join on startup (quick play).
    pub server: Option<&'a str>,
    pub java: Option<&'a Path>,
    pub max_memory: &'a str,
    /// Another launcher's install root holding `version` as a mod loader version (read-only).
    pub loader_root: Option<&'a Path>,
}

/// Install (if needed) and launch the client, waiting for it to exit; returns its exit code.
pub fn run(o: &ClientOptions) -> Result<i32> {
    if !(3..=16).contains(&o.username.len())
        || !o
            .username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("username must be 3-16 letters, digits or underscores");
    }
    let root = std::path::absolute(o.root)?;
    let loader = o
        .loader_root
        .map(|r| foreign_loader(r, o.version))
        .transpose()?;
    let base = match &loader {
        Some(l) => l.meta["inheritsFrom"].as_str().unwrap().to_string(),
        None => o.version.to_string(),
    };
    let installed = install(&root, &base)?;
    let meta: Value = serde_json::from_slice(&fs::read(
        root.join("versions")
            .join(&base)
            .join(format!("{base}.json")),
    )?)?;
    if meta.get("arguments").is_none() {
        bail!(
            "{} uses the pre-1.13 `minecraftArguments` format, which is not supported yet",
            o.version
        );
    }
    let java = find_java(o.java, installed.java_major)?;

    let game_dir = std::path::absolute(
        o.game_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("instances").join(o.version)),
    )?;
    let natives = game_dir.join(".jabs/natives");
    fs::create_dir_all(&natives)?;
    let sep = if cfg!(windows) { ";" } else { ":" };
    // Loader libraries first; a vanilla library the loader also ships (same identity) is dropped.
    let loader_libraries = loader
        .as_ref()
        .map(|l| l.libraries.as_slice())
        .unwrap_or(&[]);
    let mut classpath: Vec<String> = loader_libraries
        .iter()
        .map(|(_, d)| d.path.display().to_string())
        .collect();
    for (key, d) in libraries(&root, &meta)? {
        if !loader_libraries.iter().any(|(k, _)| *k == key) {
            classpath.push(d.path.display().to_string());
        }
    }
    // A loader provides the game itself (patched); only vanilla runs the plain client jar.
    if loader.is_none() {
        classpath.push(
            root.join("versions")
                .join(o.version)
                .join(format!("{}.jar", o.version))
                .display()
                .to_string(),
        );
    }
    let library_directory = match &loader {
        Some(l) => l.root.join("libraries"),
        None => root.join("libraries"),
    };

    let mut features = HashMap::new();
    features.insert("is_quick_play_multiplayer", o.server.is_some());
    let vars: HashMap<&str, String> = HashMap::from([
        ("auth_player_name", o.username.to_string()),
        ("version_name", o.version.to_string()),
        ("game_directory", game_dir.display().to_string()),
        ("assets_root", root.join("assets").display().to_string()),
        (
            "assets_index_name",
            meta["assetIndex"]["id"].as_str().unwrap_or("").to_string(),
        ),
        ("auth_uuid", offline_uuid(o.username)),
        ("auth_access_token", "0".into()),
        ("clientid", "0".into()),
        ("auth_xuid", "0".into()),
        ("user_type", "msa".into()),
        (
            "version_type",
            meta["type"].as_str().unwrap_or("release").to_string(),
        ),
        ("natives_directory", natives.display().to_string()),
        ("launcher_name", "jabs".into()),
        ("launcher_version", env!("CARGO_PKG_VERSION").into()),
        ("classpath", classpath.join(sep)),
        ("classpath_separator", sep.into()),
        ("library_directory", library_directory.display().to_string()),
        ("quickPlayMultiplayer", o.server.unwrap_or("").to_string()),
    ]);
    let substitute = |s: &str| {
        let mut out = s.to_string();
        for (k, v) in &vars {
            out = out.replace(&format!("${{{k}}}"), v);
        }
        out
    };
    // Expand an `arguments.jvm` / `arguments.game` list: plain strings, or rule-guarded values.
    let expand = |list: &Value| -> Vec<String> {
        let mut out = Vec::new();
        for arg in list.as_array().into_iter().flatten() {
            match arg {
                Value::String(s) => out.push(substitute(s)),
                Value::Object(_) if rules_allow(arg.get("rules"), &features) => match &arg["value"]
                {
                    Value::String(s) => out.push(substitute(s)),
                    Value::Array(values) => {
                        out.extend(values.iter().filter_map(Value::as_str).map(substitute))
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        out
    };

    let mut command = Command::new(&java);
    command
        .current_dir(&game_dir)
        .arg(format!("-Xmx{}", o.max_memory));
    // Mojang's logging config (XML events for the official launcher) is not used; the game's own text logging is.
    // Inherited arguments come first, then the loader's own (the official launcher's merge order).
    let loader_meta = loader.as_ref().map(|l| &l.meta);
    command.args(expand(&meta["arguments"]["jvm"]));
    if let Some(l) = loader_meta {
        command.args(expand(&l["arguments"]["jvm"]));
    }
    command.arg(
        loader_meta.unwrap_or(&meta)["mainClass"]
            .as_str()
            .context("version has no mainClass")?,
    );
    command.args(expand(&meta["arguments"]["game"]));
    if let Some(l) = loader_meta {
        command.args(expand(&l["arguments"]["game"]));
    }
    eprintln!(
        "launching Minecraft {} as {} ({}) with {}{}",
        o.version,
        o.username,
        offline_uuid(o.username),
        java.display(),
        o.server
            .map(|s| format!(", joining {s}"))
            .unwrap_or_default()
    );
    let status = command
        .status()
        .with_context(|| format!("start {}", java.display()))?;
    Ok(status.code().unwrap_or(-1))
}

/// What JABS uses from a CurseForge instance folder.
pub struct CurseForgeInstance {
    /// Version to launch: the mod loader's version id, or the game version when vanilla.
    pub version: String,
    pub game_version: String,
    /// CurseForge's install root holding the loader (`<instances>/../Install`, else the default).
    pub loader_root: Option<PathBuf>,
    pub max_memory: String,
}

/// Read `minecraftinstance.json` from a CurseForge instance folder.
pub fn curseforge_instance(dir: &Path) -> Result<CurseForgeInstance> {
    let path = dir.join("minecraftinstance.json");
    let meta: Value = serde_json::from_slice(
        &fs::read(&path)
            .with_context(|| format!("{} is not a CurseForge instance", dir.display()))?,
    )?;
    let game_version = meta["gameVersion"]
        .as_str()
        .context("instance has no gameVersion")?
        .to_string();
    let loader = meta["baseModLoader"]["name"].as_str().map(str::to_string);
    let loader_root = loader.as_ref().and_then(|id| {
        let mut candidates = Vec::new();
        if let Some(parent) = std::path::absolute(dir)
            .ok()
            .and_then(|d| d.parent().and_then(Path::parent).map(Path::to_path_buf))
        {
            candidates.push(parent.join("Install"));
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            candidates.push(PathBuf::from(home).join("curseforge/minecraft/Install"));
        }
        candidates.into_iter().find(|c| {
            c.join("versions")
                .join(id)
                .join(format!("{id}.json"))
                .is_file()
        })
    });
    if loader.is_some() && loader_root.is_none() {
        bail!(
            "could not find CurseForge's install folder for this instance's loader; pass --loader-root"
        );
    }
    let memory_mb = meta["allocatedMemory"].as_u64().unwrap_or(4096);
    Ok(CurseForgeInstance {
        version: loader.unwrap_or_else(|| game_version.clone()),
        game_version,
        loader_root,
        max_memory: format!("{memory_mb}M"),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn offline_uuid_matches_java() {
        // java.util.UUID.nameUUIDFromBytes("OfflinePlayer:JabsTest".getBytes(UTF_8)), computed on the JVM.
        assert_eq!(
            super::offline_uuid("JabsTest"),
            "f2121ce2a7e132b397858e1cb549e3d3"
        );
    }
}
