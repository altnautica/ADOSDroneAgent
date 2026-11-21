use super::*;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use zip::write::SimpleFileOptions;

enum M<'a> {
    File(&'a str, &'a [u8]),
    Dir(&'a str),
    Link(&'a str, &'a str),
}

fn zip_of(members: &[M]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for m in members {
            match m {
                M::File(name, body) => {
                    w.start_file(*name, opts).unwrap();
                    w.write_all(body).unwrap();
                }
                M::Dir(name) => w.add_directory(*name, opts).unwrap(),
                M::Link(name, target) => w.add_symlink(*name, *target, opts).unwrap(),
            }
        }
        w.finish().unwrap();
    }
    buf
}

/// Rewrite the unix mode the central directory records for member `index`.
fn with_mode(mut zip: Vec<u8>, index: usize, mode: u32) -> Vec<u8> {
    let at = zip
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"PK\x01\x02")
        .map(|(i, _)| i)
        .nth(index)
        .expect("central directory header");
    zip[at + 38..at + 42].copy_from_slice(&(mode << 16).to_le_bytes());
    zip
}

struct Case {
    _dir: tempfile::TempDir,
    zip: PathBuf,
    dest: PathBuf,
}

fn case(zip: &[u8]) -> Case {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.zip");
    std::fs::write(&path, zip).unwrap();
    let dest = dir.path().join("tree/lib/rt");
    Case {
        zip: path,
        dest,
        _dir: dir,
    }
}

fn unpack(c: &Case, caps: PayloadCaps) -> Result<Vec<UnpackedFile>, ArchiveError> {
    unpack_payload_zip_capped(&c.zip, &c.dest, "lib/rt", &Executables::default(), caps)
}

/// A refusal leaves neither the destination nor the hidden work dir.
fn refused(zip: &[u8], caps: PayloadCaps) -> String {
    let c = case(zip);
    let err = unpack(&c, caps).unwrap_err().0;
    assert!(!c.dest.exists(), "{err}");
    assert!(!c.dest.with_file_name(".rt.unpack").exists(), "{err}");
    err
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[test]
fn unpacks_files_and_dirs_with_their_modes_and_digests() {
    let zip = zip_of(&[
        M::Dir("bin/"),
        M::File("bin/python3", b"\x7fELF\x02\x01"),
        M::File("lib/python3.12/os.py", b"import sys\n"),
        M::Dir("share/empty/"),
    ]);
    let c = case(&zip);
    let files = unpack(&c, PayloadCaps::DEFAULT).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["lib/rt/bin/python3", "lib/rt/lib/python3.12/os.py"]);
    assert_eq!(
        files[1].sha256,
        hex::encode(Sha256::digest(b"import sys\n"))
    );
    assert_eq!(mode(&c.dest.join("bin/python3")), 0o755);
    assert_eq!(mode(&c.dest.join("lib/python3.12/os.py")), 0o644);
    for d in ["", "bin", "lib", "lib/python3.12", "share/empty"] {
        assert_eq!(mode(&c.dest.join(d)), 0o755, "{d}");
    }
    assert!(!c.dest.with_file_name(".rt.unpack").exists());
}

#[test]
fn a_symlink_member_is_refused() {
    let zip = zip_of(&[M::File("a", b"x"), M::Link("evil", "/etc/passwd")]);
    assert!(refused(&zip, PayloadCaps::DEFAULT).contains("not a regular file"));
}

#[test]
fn device_fifo_and_socket_members_are_refused() {
    for kind in [0o020000u32, 0o060000, 0o010000, 0o140000] {
        let zip = with_mode(zip_of(&[M::File("dev", b"")]), 0, kind | 0o644);
        let err = refused(&zip, PayloadCaps::DEFAULT);
        assert!(err.contains("not a regular file"), "{kind:o}: {err}");
    }
}

#[test]
fn a_directory_mode_on_a_file_member_is_refused() {
    let zip = with_mode(zip_of(&[M::File("f", b"x")]), 0, 0o040755);
    assert!(refused(&zip, PayloadCaps::DEFAULT).contains("not a regular file"));
}

#[test]
fn traversal_and_absolute_members_are_refused() {
    for name in [
        "../escape",
        "/etc/escape",
        "a/../../escape",
        "a/./b",
        "a\\b",
    ] {
        let err = refused(&zip_of(&[M::File(name, b"x")]), PayloadCaps::DEFAULT);
        assert!(err.contains("unsafe"), "{name}: {err}");
    }
}

#[test]
fn a_member_twice_is_refused() {
    let zip = zip_of(&[M::File("a", b"x"), M::Dir("a/")]);
    assert!(refused(&zip, PayloadCaps::DEFAULT).contains("appears twice"));
}

#[test]
fn a_member_under_a_file_is_refused() {
    let zip = zip_of(&[M::File("a", b"x"), M::File("a/b", b"y")]);
    assert!(refused(&zip, PayloadCaps::DEFAULT).contains("runs through a file"));
}

#[test]
fn an_existing_destination_is_refused_and_kept() {
    let c = case(&zip_of(&[M::File("a", b"x")]));
    std::fs::create_dir_all(&c.dest).unwrap();
    std::fs::write(c.dest.join("old"), b"old").unwrap();
    let err = unpack(&c, PayloadCaps::DEFAULT).unwrap_err().0;
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(std::fs::read(c.dest.join("old")).unwrap(), b"old");
}

#[test]
fn the_member_count_and_size_caps_are_enforced() {
    let caps = PayloadCaps {
        entries: 3,
        entry_bytes: 1024,
        total_bytes: 2048,
        directory_bytes: 1 << 20,
    };
    let many = zip_of(&[
        M::File("a", b"1"),
        M::File("b", b"2"),
        M::File("c", b"3"),
        M::File("d", b"4"),
    ]);
    assert!(refused(&many, caps).contains("members; cap is 3"));
    let big = vec![7u8; 1025];
    assert!(refused(&zip_of(&[M::File("big", &big)]), caps).contains("per-member cap"));
    let part = vec![7u8; 900];
    let sum = zip_of(&[
        M::File("a", &part),
        M::File("b", &part),
        M::File("c", &part),
    ]);
    assert!(refused(&sum, caps).contains("total cap"));
}

#[test]
fn a_decompression_bomb_member_is_refused() {
    let zeros = vec![0u8; 4 * 1024 * 1024];
    let err = refused(&zip_of(&[M::File("bomb", &zeros)]), PayloadCaps::DEFAULT);
    assert!(err.contains("ratio cap"), "{err}");
}

#[test]
fn a_file_that_is_not_a_zip_is_refused() {
    assert!(refused(b"not a zip at all", PayloadCaps::DEFAULT).contains("not a valid zip"));
}

#[test]
fn a_refusal_midway_leaves_no_partial_tree() {
    let zip = zip_of(&[
        M::File("bin/python3", b"\x7fELF"),
        M::File("lib/ok.py", b"x = 1\n"),
        M::Link("lib/evil", "/"),
    ]);
    refused(&zip, PayloadCaps::DEFAULT);
}

#[test]
fn a_work_dir_left_by_an_interrupted_unpack_is_refused_and_kept() {
    let c = case(&zip_of(&[M::File("a", b"x")]));
    let stale = c.dest.with_file_name(".rt.unpack");
    std::fs::create_dir_all(stale.join("junk")).unwrap();
    let err = unpack(&c, PayloadCaps::DEFAULT).unwrap_err().0;
    assert!(err.contains("left"), "{err}");
    assert!(stale.join("junk").is_dir());
    assert!(!c.dest.exists());
}

#[test]
fn long_and_deep_member_names_are_refused() {
    let long = format!("{}/x", "a".repeat(256));
    assert!(refused(&zip_of(&[M::File(&long, b"x")]), PayloadCaps::DEFAULT).contains("too long"));
    let deep = format!("{}x", "d/".repeat(32));
    assert!(refused(&zip_of(&[M::File(&deep, b"x")]), PayloadCaps::DEFAULT).contains("too deep"));
    let huge = format!("{}/x", vec!["b".repeat(200); 6].join("/"));
    assert!(refused(&zip_of(&[M::File(&huge, b"x")]), PayloadCaps::DEFAULT).contains("too long"));
}

#[test]
fn directories_created_count_against_the_member_cap() {
    let caps = PayloadCaps {
        entries: 4,
        ..PayloadCaps::DEFAULT
    };
    // Two members, but they create five directories and two files.
    let zip = zip_of(&[M::File("a/b/c/f", b"x"), M::File("d/e/g", b"y")]);
    assert!(refused(&zip, caps).contains("files and directories"));
}

#[test]
fn an_oversized_central_directory_is_refused_before_parsing() {
    let caps = PayloadCaps {
        directory_bytes: 64,
        ..PayloadCaps::DEFAULT
    };
    let zip = zip_of(&[M::File("a", b"x"), M::File("b", b"y")]);
    assert!(refused(&zip, caps).contains("central directory"));
}

#[test]
fn a_declared_count_the_directory_disagrees_with_is_refused() {
    let mut zip = zip_of(&[M::File("a", b"x"), M::File("b", b"y")]);
    let at = zip.len() - 22;
    assert_eq!(&zip[at..at + 4], b"PK\x05\x06");
    zip[at + 8..at + 10].copy_from_slice(&1u16.to_le_bytes());
    zip[at + 10..at + 12].copy_from_slice(&1u16.to_le_bytes());
    let err = refused(&zip, PayloadCaps::DEFAULT);
    assert!(
        err.contains("declares 1 members") || err.contains("not a valid zip"),
        "{err}"
    );
}

#[test]
fn bytes_after_the_end_record_or_a_second_disk_are_refused() {
    let mut trailing = zip_of(&[M::File("a", b"x")]);
    trailing.extend_from_slice(b"junk");
    assert!(refused(&trailing, PayloadCaps::DEFAULT).contains("does not end the file"));
    let mut disk = zip_of(&[M::File("a", b"x")]);
    let at = disk.len() - 22;
    disk[at + 4..at + 6].copy_from_slice(&1u16.to_le_bytes());
    assert!(refused(&disk, PayloadCaps::DEFAULT).contains("multi-disk"));
}

#[test]
fn the_directory_parse_reads_no_further_than_its_budget() {
    let data = vec![7u8; 1000];
    let left = std::rc::Rc::new(std::cell::Cell::new(Some(100)));
    let mut r = Budgeted {
        inner: Cursor::new(data),
        left: left.clone(),
    };
    let mut buf = [0u8; 64];
    assert_eq!(r.read(&mut buf).unwrap(), 64);
    assert!(r.read(&mut buf).is_err(), "past the budget");
    left.set(None);
    assert_eq!(r.read(&mut buf).unwrap(), 64, "lifted for member reads");
}

#[test]
fn a_member_larger_than_it_declares_is_refused() {
    let mut zip = zip_of(&[M::File("a", &[1u8; 300])]);
    // The central directory's uncompressed size field of the one member.
    let at = zip
        .windows(4)
        .position(|w| w == b"PK\x01\x02")
        .expect("central directory header");
    zip[at + 24..at + 28].copy_from_slice(&10u32.to_le_bytes());
    refused(&zip, PayloadCaps::DEFAULT);
}

#[test]
fn an_adosplug_whose_count_disagrees_is_refused() {
    let mut zip = zip_of(&[M::File("manifest.yaml", b"id: x\n"), M::File("b", b"y")]);
    let at = zip.len() - 22;
    zip[at + 8..at + 10].copy_from_slice(&1u16.to_le_bytes());
    zip[at + 10..at + 12].copy_from_slice(&1u16.to_le_bytes());
    assert!(read_entries(&zip).is_err());
    let dir = tempfile::tempdir().unwrap();
    assert!(unpack_to(&zip, dir.path(), &Executables::default()).is_err());
}
