use crate::profile::{self, ArtifactIdentity};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Record {
    schema_version: u32,
    sha256: String,
    bytes: u64,
    analysis: String,
}

#[derive(Debug, Serialize)]
pub struct Preparation {
    pub schema_version: u32,
    pub profile_key: String,
    pub artifacts: usize,
    pub reused: usize,
    pub created: usize,
    pub compiled_units: usize,
    pub stage: &'static str,
}

/// Cache record that a verified artifact identity should have.
fn expected(identity: &ArtifactIdentity) -> Record {
    Record {
        schema_version: 1,
        sha256: identity.sha256.clone(),
        bytes: identity.bytes,
        analysis: "sha256-integrity-v1".into(),
    }
}

/// Cache only verified metadata; never copy or compile game code here.
pub fn prepare(profile_path: &Path, cache: &Path, verify_only: bool) -> Result<Preparation> {
    let inspection = profile::inspect(profile_path)?;
    let profile_key = format!("{:x}", Sha256::digest(serde_json::to_vec(&inspection)?));
    let root = cache.join("integrity-v1");
    if !verify_only {
        fs::create_dir_all(&root)?;
    }
    let mut reused = 0;
    let mut created = 0;
    for identity in &inspection.artifacts {
        let record = expected(identity);
        let path = root.join(format!("{}.json", identity.sha256));
        if path.exists() {
            let actual: Record =
                serde_json::from_reader(File::open(&path)?).with_context(|| {
                    format!(
                        "invalid cache record {}; remove this record and prepare again",
                        path.display()
                    )
                })?;
            if actual != record {
                bail!("cache record mismatch: {}", path.display());
            }
            reused += 1;
        } else if verify_only {
            bail!(
                "missing prepared metadata for {}; run prepare first",
                identity.id
            );
        } else {
            // create_new refuses to overwrite a concurrent writer's record.
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| {
                    format!(
                        "create cache record {}; retry if another preparation is running",
                        path.display()
                    )
                })?;
            file.write_all(&serde_json::to_vec_pretty(&record)?)?;
            file.sync_all()?;
            created += 1;
        }
    }
    Ok(Preparation {
        schema_version: 1,
        profile_key,
        artifacts: inspection.artifacts.len(),
        reused,
        created,
        compiled_units: 0,
        stage: "verified artifact metadata; native compilation not implemented",
    })
}
