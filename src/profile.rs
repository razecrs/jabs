use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub schema_version: u32,
    pub name: String,
    pub minecraft_version: String,
    pub java_major: u32,
    pub loader: Loader,
    pub backend: Backend,
    pub artifacts: Vec<Artifact>,
    /// How `jabs run` starts this profile; absent means the profile is analysis-only.
    #[serde(default)]
    pub launch: Option<Launch>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Client,
    Server,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub side: Side,
    /// Game directory (worlds, logs, eula.txt), relative to the profile; defaults to `run/<profile name>`.
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    /// Java executable; defaults to `java` on PATH. Its major version must equal `java_major`.
    #[serde(default)]
    pub java: Option<PathBuf>,
    /// Overrides the game artifact's manifest `Main-Class`.
    #[serde(default)]
    pub main_class: Option<String>,
    #[serde(default)]
    pub jvm_args: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Loader {
    pub kind: LoaderKind,
    pub version: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LoaderKind {
    Vanilla,
    Fabric,
    Forge,
    Neoforge,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Jvm,
    Native,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub role: Role,
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Game,
    Library,
    Loader,
    Mod,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub id: String,
    pub role: Role,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub schema_version: u32,
    pub profile: String,
    pub minecraft_version: String,
    pub java_major: u32,
    pub requested_backend: Backend,
    pub loader: Loader,
    pub artifacts: Vec<ArtifactIdentity>,
    pub analysis: &'static str,
}

/// Read and validate a profile JSON file.
pub fn load(path: &Path) -> Result<Profile> {
    let profile: Profile = serde_json::from_reader(
        File::open(path).with_context(|| format!("open profile {}", path.display()))?,
    )
    .context("invalid profile JSON")?;
    profile.validate()?;
    Ok(profile)
}

impl Profile {
    /// Check schema, loader/version pairing, a single game artifact, unique ids and SHA-256 format.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported profile schema {}; expected 1",
                self.schema_version
            );
        }
        if self.name.trim().is_empty()
            || self.minecraft_version.trim().is_empty()
            || self.java_major == 0
        {
            bail!("profile name, Minecraft version, and Java major must be specified");
        }
        match self.loader.kind {
            LoaderKind::Vanilla if self.loader.version.is_some() => {
                bail!("vanilla must not specify a loader version")
            }
            LoaderKind::Vanilla => {}
            _ if self
                .loader
                .version
                .as_ref()
                .is_none_or(|v| v.trim().is_empty()) =>
            {
                bail!("mod loader requires a pinned version")
            }
            _ => {}
        }
        if self
            .artifacts
            .iter()
            .filter(|a| a.role == Role::Game)
            .count()
            != 1
        {
            bail!("profile requires exactly one game artifact");
        }
        let mut ids = HashSet::new();
        for artifact in &self.artifacts {
            if artifact.id.trim().is_empty() || !ids.insert(&artifact.id) {
                bail!("empty or duplicate artifact id: {}", artifact.id);
            }
            if artifact.path.as_os_str().is_empty() {
                bail!("empty artifact path: {}", artifact.id);
            }
            if artifact.sha256.len() != 64
                || !artifact
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                bail!(
                    "{}: sha256 must be 64 lowercase hexadecimal characters",
                    artifact.id
                );
            }
        }
        Ok(())
    }
}

/// Stream artifacts rather than buffering game/mod JARs in memory.
pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("open artifact {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("artifact is not a regular file: {}", path.display());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

/// Load a profile and rehash every artifact against its pinned SHA-256.
pub fn inspect(path: &Path) -> Result<Inspection> {
    let profile = load(path)?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut identities = Vec::new();
    for artifact in profile.artifacts {
        let (bytes, hash) = hash_file(&base.join(&artifact.path))?;
        if hash != artifact.sha256 {
            bail!(
                "SHA-256 mismatch for {}: expected {}, got {}",
                artifact.id,
                artifact.sha256,
                hash
            );
        }
        identities.push(ArtifactIdentity {
            id: artifact.id,
            role: artifact.role,
            bytes,
            sha256: hash,
        });
    }
    Ok(Inspection {
        schema_version: 1,
        profile: profile.name,
        minecraft_version: profile.minecraft_version,
        java_major: profile.java_major,
        requested_backend: profile.backend,
        loader: profile.loader,
        artifacts: identities,
        analysis: "artifact integrity only; JAR contents, dependency completeness and runtime compatibility not yet analyzed",
    })
}
