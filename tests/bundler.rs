//! Unbundling a synthetic Mojang-style bundler jar.

use jabs::bundler;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use zip::write::SimpleFileOptions;

/// A tiny "signed" inner jar: one class-like entry plus signature files.
fn inner_jar() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in [
        (
            "META-INF/MANIFEST.MF",
            &b"Manifest-Version: 1.0\r\n\r\n"[..],
        ),
        ("META-INF/MOJANGCS.SF", b"sig"),
        ("META-INF/MOJANGCS.RSA", b"sig"),
        ("net/minecraft/server/Main.class", b"\xCA\xFE\xBA\xBE"),
    ] {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// Write a bundler holding `game` (whose listed hash is `listed_sha`) and one library.
fn write_bundler(path: &Path, game: &[u8], listed_sha: &str, game_path: &str) {
    let lib = b"library bytes";
    let lib_sha = format!("{:x}", Sha256::digest(lib));
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let files: Vec<(String, Vec<u8>)> = vec![
        (
            "META-INF/main-class".into(),
            b"net.minecraft.server.Main\n".to_vec(),
        ),
        (
            "META-INF/libraries.list".into(),
            format!("{lib_sha}\tdemo:lib:1\tdemo/lib-1.jar\n").into_bytes(),
        ),
        (
            "META-INF/versions.list".into(),
            format!("{listed_sha}\t9.9\t{game_path}\n").into_bytes(),
        ),
        (
            "META-INF/classpath-joined".into(),
            format!("libraries/demo/lib-1.jar;versions/{game_path}").into_bytes(),
        ),
        ("META-INF/libraries/demo/lib-1.jar".into(), lib.to_vec()),
        (format!("META-INF/versions/{game_path}"), game.to_vec()),
    ];
    for (name, data) in files {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(&data).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn unbundles_in_order_strips_signatures_and_reuses() {
    let dir = tempfile::tempdir().unwrap();
    let game = inner_jar();
    let sha = format!("{:x}", Sha256::digest(&game));
    let jar = dir.path().join("server.jar");
    write_bundler(&jar, &game, &sha, "9.9/server-9.9.jar");
    assert!(bundler::is_bundler(&jar).unwrap());

    let root = dir.path().join("out");
    let first = bundler::unbundle(&jar, "bundlersha", &root, true).unwrap();
    assert!(first.extracted);
    assert_eq!(first.main_class, "net.minecraft.server.Main");
    assert!(
        first.classpath[0].ends_with("demo/lib-1.jar")
            && first.classpath[1].ends_with("server-9.9.jar")
    );
    let unsigned = zip::ZipArchive::new(fs::File::open(&first.classpath[1]).unwrap()).unwrap();
    let names: Vec<_> = unsigned.file_names().collect();
    assert!(names.contains(&"net/minecraft/server/Main.class"));
    assert!(
        !names
            .iter()
            .any(|n| n.ends_with(".SF") || n.ends_with(".RSA"))
    );

    let second = bundler::unbundle(&jar, "bundlersha", &root, true).unwrap();
    assert!(!second.extracted);
    // Tampering with an extracted jar is detected on reuse.
    fs::write(&first.classpath[0], b"tampered").unwrap();
    assert!(bundler::unbundle(&jar, "bundlersha", &root, true).is_err());
}

#[test]
fn rejects_wrong_hash_and_escaping_paths() {
    let dir = tempfile::tempdir().unwrap();
    let game = inner_jar();
    let jar = dir.path().join("bad-hash.jar");
    write_bundler(&jar, &game, &"0".repeat(64), "9.9/server-9.9.jar");
    let err = bundler::unbundle(&jar, "x", &dir.path().join("a"), false)
        .err()
        .unwrap();
    assert!(err.to_string().contains("SHA-256"), "{err}");

    let sha = format!("{:x}", Sha256::digest(&game));
    let escape = dir.path().join("escape.jar");
    write_bundler(&escape, &game, &sha, "../../evil.jar");
    let err = bundler::unbundle(&escape, "y", &dir.path().join("b"), false)
        .err()
        .unwrap();
    assert!(err.to_string().contains("unsafe path"), "{err}");
    assert!(!dir.path().join("evil.jar").exists());
}
