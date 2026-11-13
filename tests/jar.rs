use jabs::jar::{self, LoaderMetadata};
use std::{fs, io::Write, path::Path, process::Command};
use zip::write::SimpleFileOptions;

/// Minimal valid class: `public class demo/Hello extends java/lang/Object` with given major.
fn minimal_class(major: u16) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0];
    b.extend(major.to_be_bytes());
    b.extend(5u16.to_be_bytes()); // constant_pool_count
    let utf8 = |b: &mut Vec<u8>, s: &str| {
        b.push(1);
        b.extend((s.len() as u16).to_be_bytes());
        b.extend(s.as_bytes());
    };
    utf8(&mut b, "demo/Hello"); // #1
    b.extend([7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend([7, 0, 3]); // #4 Class #3
    b.extend([0x00, 0x21, 0, 2, 0, 4]); // access, this, super
    b.extend([0, 0, 0, 0, 0, 0, 0, 0]); // interfaces, fields, methods, attributes
    b
}

/// Write a stored-entry ZIP with the given entries.
fn write_jar(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn parses_minimal_class_across_eras() {
    for (major, java) in [(50, 6), (52, 8), (65, 21), (69, 25)] {
        let info = jar::parse_class(&minimal_class(major)).unwrap();
        assert_eq!(info.name, "demo/Hello");
        assert_eq!(info.super_name.as_deref(), Some("java/lang/Object"));
        assert_eq!(jar::java_release(info.major), java);
        assert!(!info.uses_invokedynamic);
    }
}

#[test]
fn rejects_malformed_classes() {
    let good = minimal_class(52);
    assert!(jar::parse_class(&good[..good.len() - 1]).is_err());
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(jar::parse_class(&trailing).is_err());
    let mut bad_magic = good.clone();
    bad_magic[0] = 0;
    assert!(jar::parse_class(&bad_magic).is_err());
    let mut bad_tag = good;
    bad_tag[10] = 99;
    assert!(jar::parse_class(&bad_tag).is_err());
}

#[test]
fn legacy_forge_and_fabric_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("legacy.jar");
    write_jar(
        &legacy,
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\r\nFMLCorePlugin: demo.Core\r\n",
            ),
            ("mcmod.info", b"[]"),
            ("demo/Hello.class", &minimal_class(50)),
            ("natives/lwjgl.dll", b"MZ"),
        ],
    );
    let report = jar::inspect_jar(&legacy).unwrap();
    assert_eq!(
        report.loader_metadata,
        [LoaderMetadata::LegacyForge, LoaderMetadata::ForgeCoremod]
    );
    assert_eq!(report.required_java, Some(6));
    assert_eq!(report.native_libraries, ["natives/lwjgl.dll"]);

    let modern = dir.path().join("modern.jar");
    write_jar(
        &modern,
        &[
            ("fabric.mod.json", b"{}"),
            ("demo.mixins.json", b"{}"),
            ("demo/Hello.class", &minimal_class(65)),
            ("demo/Broken.class", b"\xCA\xFE"),
        ],
    );
    let report = jar::inspect_jar(&modern).unwrap();
    assert_eq!(report.loader_metadata, [LoaderMetadata::Fabric]);
    assert_eq!(report.mixin_configs, ["demo.mixins.json"]);
    assert_eq!((report.classes, report.invalid_classes), (2, 1));
    assert_eq!(report.required_java, Some(21));
}

/// Cross-check against classes produced by the installed reference javac, when present.
#[test]
fn reference_javac_output() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Demo.java");
    fs::write(
        &source,
        "public class Demo implements Runnable {\n\
        long l = 1L; double d = 2.0; String s = \"x\";\n\
        public native int nat();\n\
        public void run() { Runnable r = () -> {}; r.run(); }\n}\n",
    )
    .unwrap();
    let Ok(status) = Command::new("javac")
        .arg("--release")
        .arg("8")
        .arg("-d")
        .arg(dir.path())
        .arg(&source)
        .status()
    else {
        eprintln!("javac unavailable; skipping reference check");
        return;
    };
    assert!(status.success());
    let info = jar::parse_class(&fs::read(dir.path().join("Demo.class")).unwrap()).unwrap();
    assert_eq!((info.name.as_str(), info.major), ("Demo", 52));
    assert_eq!(info.interfaces, ["java/lang/Runnable"]);
    assert_eq!(info.native_methods, 1);
    assert!(info.uses_invokedynamic && info.uses_method_handles);
}

#[test]
fn coverage_counts_classes_and_skips_overlays() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cov.jar");
    write_jar(
        &path,
        &[
            ("demo/Hello.class", &minimal_class(52)),
            ("META-INF/versions/21/demo/Hello.class", &minimal_class(65)),
            ("demo/Broken.class", b"\xCA\xFE"),
        ],
    );
    let report = jar::coverage(&path, None).unwrap();
    assert_eq!(
        (report.classes, report.unparsable_classes, report.methods),
        (2, 1, 0)
    );
}
