use super::*;
use std::io::Write;
use zip::write::SimpleFileOptions;

fn manifest_yaml() -> &'static str {
    "id: com.example.thermal\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/thermal.py\n"
}

/// Build an in-memory stored zip from (name, bytes) pairs. A matching
/// `symlink` name is written as a real symlink entry (the writer sets the
/// `S_IFLNK` mode bits in the central-directory external attr) so the
/// reader's symlink reject is exercised against a genuine symlink, not a
/// faked permission mask.
fn build_zip(entries: &[(&str, &[u8])], symlink: Option<&str>) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        for (name, bytes) in entries {
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            if symlink == Some(name) {
                let target = std::str::from_utf8(bytes).unwrap();
                w.add_symlink(*name, target, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
        }
        w.finish().unwrap();
    }
    buf
}

#[test]
fn parses_a_well_formed_archive() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("agent/py/thermal.py", b"print('hi')"),
        ],
        None,
    );
    let c = parse_archive_bytes(zip).unwrap();
    assert_eq!(c.manifest.id, "com.example.thermal");
    assert!(c.signer_id.is_none());
}

#[test]
fn missing_manifest_is_rejected() {
    let zip = build_zip(&[("agent/py/x.py", b"x")], None);
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("missing manifest.yaml"), "{}", err);
}

#[test]
fn traversal_entry_is_rejected() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("../escape.py", b"x"),
        ],
        None,
    );
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("unsafe archive entry path"), "{}", err);
}

#[test]
fn control_char_entry_rejected() {
    // One entry whose name spells two canonical lines must not parse.
    let merged = format!("agent/a.py\n{}\nagent/b.py", "0".repeat(64));
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            (merged.as_str(), b"x"),
        ],
        None,
    );
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("unsafe archive entry path"), "{}", err);
    assert!(safe_member_path("agent/x\0.py").is_err());
    assert!(safe_member_path("agent/x\r.py").is_err());
}

#[test]
fn absolute_path_entry_is_rejected() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("/etc/passwd", b"x"),
        ],
        None,
    );
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("unsafe archive entry path"), "{}", err);
}

#[test]
fn symlink_entry_is_rejected() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("link", b"/etc/shadow"),
        ],
        Some("link"),
    );
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("symlink"), "{}", err);
}

#[test]
fn signature_blob_two_lines_parses() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("SIGNATURE", b"altnautica-2026-A\nQUJD\n"),
        ],
        None,
    );
    let c = parse_archive_bytes(zip).unwrap();
    assert_eq!(c.signer_id.as_deref(), Some("altnautica-2026-A"));
    assert_eq!(c.signature_b64.as_deref(), Some("QUJD"));
}

#[test]
fn signature_blob_wrong_line_count_is_rejected() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("SIGNATURE", b"only-one-line\n"),
        ],
        None,
    );
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("2 non-blank lines"), "{}", err);
}

#[test]
fn canonical_hash_excludes_signature_and_is_path_sorted() {
    // Two orderings of the same entries must hash equal; adding SIGNATURE
    // must not change the hash.
    let mut a: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    a.insert("manifest.yaml".into(), b"m".to_vec());
    a.insert("agent/x.py".into(), b"x".to_vec());
    let h1 = canonical_payload_hash(&a);

    let mut b = a.clone();
    b.insert("SIGNATURE".into(), b"sig-noise".to_vec());
    let h2 = canonical_payload_hash(&b);
    assert_eq!(h1, h2, "SIGNATURE must be excluded from the hash");
}

#[test]
fn unpack_round_trips_files() {
    let dir = tempfile::tempdir().unwrap();
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("agent/py/thermal.py", b"print('hi')"),
        ],
        None,
    );
    unpack_to(&zip, dir.path(), &Executables::default()).unwrap();
    let got = std::fs::read(dir.path().join("agent/py/thermal.py")).unwrap();
    assert_eq!(got, b"print('hi')");
}

#[test]
fn oversized_decompressed_entry_is_rejected() {
    // A deflated entry of highly-compressible zeros that inflates past the
    // per-entry cap. The compressed bytes stay tiny (well under the archive
    // cap), so the only thing standing between this and an OOM is the
    // bounded-inflation guard, not the on-disk size checks.
    let big = (ENTRY_MAX_BYTES + 4096) as usize;
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", stored).unwrap();
        w.write_all(manifest_yaml().as_bytes()).unwrap();
        let deflated =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        w.start_file("assets/bomb.bin", deflated).unwrap();
        // Write in chunks so the test does not hold the full payload twice.
        let chunk = vec![0u8; 1024 * 1024];
        let mut written = 0usize;
        while written < big {
            let n = chunk.len().min(big - written);
            w.write_all(&chunk[..n]).unwrap();
            written += n;
        }
        w.finish().unwrap();
    }
    // The compressed archive is far smaller than the archive cap, yet the
    // entry inflates past the per-entry cap, so the parse must refuse it.
    assert!(
        (buf.len() as u64) < ARCHIVE_MAX_BYTES,
        "compressed archive should be small ({})",
        buf.len()
    );
    let err = parse_archive_bytes(buf.clone()).unwrap_err().to_string();
    assert!(
        err.contains("per-entry cap"),
        "expected a per-entry decompression cap error, got: {}",
        err
    );
    // unpack_to must enforce the same bound (it had no cap at all before).
    let dir = tempfile::tempdir().unwrap();
    let err2 = unpack_to(&buf, dir.path(), &Executables::default()).unwrap_err();
    assert!(
        err2.0.contains("per-entry cap"),
        "unpack must reject the bomb too, got: {}",
        err2.0
    );
}

#[cfg(unix)]
#[test]
fn only_files_the_manifest_runs_unpack_executable() {
    use std::os::unix::fs::PermissionsExt;
    let manifest = PluginManifest::from_yaml_text(
            "id: com.example.geo\nversion: 1.0.0\nrisk: critical\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: bin:geofence\n  runtime: rust\n  binaries:\n    geofence:\n      aarch64-linux: agent/bin/geofence\n  permissions:\n    - process.spawn\n  subprocess_spawn:\n    - helper\n",
        )
        .unwrap();
    // The zip's own mode bits say the opposite of the manifest: the
    // binary is plain and an asset carries every exec bit.
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", stored.unix_permissions(0o777))
            .unwrap();
        w.write_all(b"id: x\n").unwrap();
        w.start_file("agent/bin/geofence", stored.unix_permissions(0o644))
            .unwrap();
        w.write_all(b"#!/bin/sh\n").unwrap();
        w.start_file("vendor/helper", stored.unix_permissions(0o600))
            .unwrap();
        w.write_all(b"#!/bin/sh\n").unwrap();
        w.finish().unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    unpack_to(&buf, dir.path(), &Executables::of(&manifest)).unwrap();
    let mode = |rel: &str| {
        std::fs::metadata(dir.path().join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("agent/bin/geofence"), 0o755);
    assert_eq!(mode("vendor/helper"), 0o755);
    assert_eq!(mode("manifest.yaml"), 0o644);
}

#[test]
fn an_entry_under_the_attestation_name_is_refused() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            (".attestation.json", b"{}"),
        ],
        None,
    );
    let err = parse_archive_bytes(zip).unwrap_err().to_string();
    assert!(err.contains("reserved"), "{err}");
    // The same name below the top level is an ordinary file.
    let nested = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("assets/.attestation.json", b"{}"),
        ],
        None,
    );
    assert!(parse_archive_bytes(nested).is_ok());
}

#[test]
fn an_entry_that_expands_past_the_ratio_cap_is_refused() {
    // Two MiB of zeros deflate to a few KiB: past the floor and far past
    // the ratio, while still under the per-entry byte cap.
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", stored).unwrap();
        w.write_all(manifest_yaml().as_bytes()).unwrap();
        let deflated =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        w.start_file("assets/zeros.bin", deflated).unwrap();
        w.write_all(&vec![0u8; 2 * 1024 * 1024]).unwrap();
        w.finish().unwrap();
    }
    let err = parse_archive_bytes(buf.clone()).unwrap_err().to_string();
    assert!(err.contains("ratio cap"), "{err}");
    let dir = tempfile::tempdir().unwrap();
    let err = unpack_to(&buf, dir.path(), &Executables::default()).unwrap_err();
    assert!(err.0.contains("ratio cap"), "{}", err.0);
}

#[test]
fn dot_and_empty_segments_cannot_alias_the_manifest() {
    // `./manifest.yaml` unpacks over `manifest.yaml`, so the validated
    // manifest and the one on disk could differ.
    for alias in ["./manifest.yaml", "agent//x.py", "agent/./x.py"] {
        let zip = build_zip(
            &[
                ("manifest.yaml", manifest_yaml().as_bytes()),
                (alias, b"id: com.example.other\n"),
            ],
            None,
        );
        let err = parse_archive_bytes(zip.clone()).unwrap_err().to_string();
        assert!(err.contains("unsafe archive entry path"), "{alias}: {err}");
        let dir = tempfile::tempdir().unwrap();
        assert!(
            unpack_to(&zip, dir.path(), &Executables::default()).is_err(),
            "{alias}"
        );
    }
}

#[test]
fn malformed_signature_keeps_the_invalid_kind() {
    let zip = build_zip(
        &[
            ("manifest.yaml", manifest_yaml().as_bytes()),
            ("SIGNATURE", b"only-one-line\n"),
        ],
        None,
    );
    match parse_archive_bytes(zip) {
        Err(LifecycleError::Signature(e)) => {
            assert_eq!(e.kind, SignatureErrorKind::Invalid)
        }
        other => panic!("expected a signature error, got {other:?}"),
    }
}

#[test]
fn oversized_archive_file_is_refused_before_it_is_read() {
    // A sparse file past the cap: refused on its metadata, never buffered.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.adosplug");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(ARCHIVE_MAX_BYTES + 1)
        .unwrap();
    let err = open_archive(&path).unwrap_err().to_string();
    assert!(err.contains("cap is"), "{err}");
}
