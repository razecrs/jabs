use jabs::{benchmark, preparation, profile};
use serde_json::json;
use std::{fs, path::Path, process::Command};

/// Write a one-artifact vanilla profile with a known digest into `root`.
fn fixture(root: &Path) -> std::path::PathBuf {
    fs::write(root.join("game.jar"), b"abc").unwrap();
    let profile = json!({
        "schema_version": 1, "name": "fixture", "minecraft_version": "test", "java_major": 21,
        "loader": {"kind": "vanilla"}, "backend": "jvm",
        "artifacts": [{"id": "game", "role": "game", "path": "game.jar",
            "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}]
    });
    let path = root.join("profile.json");
    fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    path
}

#[test]
fn known_digest_and_profile_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let inspection = profile::inspect(&path).unwrap();
    assert_eq!(inspection.artifacts[0].bytes, 3);
}

#[test]
fn preparation_reuses_metadata_and_detects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let cache = dir.path().join("cache");
    assert!(preparation::prepare(&path, &cache, true).is_err());
    assert!(!cache.exists());
    let first = preparation::prepare(&path, &cache, false).unwrap();
    assert_eq!(
        (first.created, first.reused, first.compiled_units),
        (1, 0, 0)
    );
    let second = preparation::prepare(&path, &cache, false).unwrap();
    assert_eq!((second.created, second.reused), (0, 1));
    assert_eq!(first.profile_key, second.profile_key);
    preparation::prepare(&path, &cache, true).unwrap();
    fs::write(dir.path().join("game.jar"), b"tampered").unwrap();
    assert!(
        preparation::prepare(&path, &cache, false)
            .unwrap_err()
            .to_string()
            .contains("SHA-256 mismatch")
    );
}

#[test]
fn incremental_artifact_metadata_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let cache = dir.path().join("cache");
    let first = preparation::prepare(&path, &cache, false).unwrap();
    fs::write(dir.path().join("mod.jar"), b"mod").unwrap();
    let (_, digest) = profile::hash_file(&dir.path().join("mod.jar")).unwrap();
    let mut data: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    data["loader"] = json!({"kind": "fabric", "version": "fixture"});
    data["artifacts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "mod", "role": "mod", "path": "mod.jar", "sha256": digest}));
    fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
    let changed = preparation::prepare(&path, &cache, false).unwrap();
    assert_eq!((changed.created, changed.reused), (1, 1));
    assert_ne!(first.profile_key, changed.profile_key);
}

#[test]
fn corrupt_cache_is_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let cache = dir.path().join("cache");
    preparation::prepare(&path, &cache, false).unwrap();
    let record = fs::read_dir(cache.join("integrity-v1"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(record, b"{}").unwrap();
    assert!(preparation::prepare(&path, &cache, true).is_err());
}

#[test]
fn rejects_invalid_profile_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mutations = [
        ("schema_version", json!(2)),
        ("backend", json!("magic")),
        ("loader", json!({"kind": "fabric"})),
        ("artifacts", json!([])),
        ("typo", json!(true)),
        ("java_major", json!(0)),
    ];
    for (key, value) in mutations {
        let mut data = original.clone();
        data[key] = value;
        fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(profile::load(&path).is_err(), "accepted invalid {key}");
    }
    let mut data = original;
    let duplicate = data["artifacts"][0].clone();
    data["artifacts"].as_array_mut().unwrap().push(duplicate);
    fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(profile::load(&path).is_err());
}

#[test]
fn nearest_rank_percentiles_and_invalid_measurements() {
    let samples: Vec<_> = (1..=100).map(f64::from).collect();
    let result = benchmark::distribution(&samples).unwrap();
    assert_eq!(
        (result.median_ms, result.p95_ms, result.p99_ms),
        (50.0, 95.0, 99.0)
    );
    for invalid in [
        vec![],
        vec![0.0],
        vec![-1.0],
        vec![f64::NAN],
        vec![f64::INFINITY],
    ] {
        assert!(benchmark::distribution(&invalid).is_err());
    }
}

#[test]
fn report_preserves_runs_and_does_not_claim_speedup() {
    let input = json!({"schema_version": 1, "scenario": "fixture", "backend": "stock",
        "minecraft_version": "test", "hardware": "synthetic", "settings": "synthetic",
        "runs": [{"frame_ms": [1.0], "tick_ms": [2.0]}, {"frame_ms": [100.0], "tick_ms": [200.0]}]});
    let report = benchmark::summarize(serde_json::from_value(input).unwrap()).unwrap();
    assert_eq!(report.runs.len(), 2);
    assert!(!report.sufficient_repetitions);
    assert!(report.performance_claim.starts_with("none"));
}

#[test]
fn cli_success_and_unsupported_execution() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let exe = env!("CARGO_BIN_EXE_jabs");
    let inspect = Command::new(exe)
        .arg("inspect")
        .arg(&path)
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let output: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(output["profile"], "fixture");
    let run = Command::new(exe).arg("run").arg(&path).output().unwrap();
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains("not implemented"));
}
