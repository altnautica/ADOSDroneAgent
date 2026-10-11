use super::*;
use std::fs;

#[test]
fn under_strips_the_leading_slash_and_stays_inside_the_root() {
    let root = Path::new("/test/root");
    assert_eq!(
        under(root, "/sys/class/thermal"),
        Path::new("/test/root/sys/class/thermal")
    );
    assert_eq!(under(root, "/proc/stat"), Path::new("/test/root/proc/stat"));
}

#[test]
fn read_trimmed_drops_the_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("value");
    fs::write(&p, "45000\n").unwrap();
    assert_eq!(read_trimmed(&p).as_deref(), Some("45000"));
}

#[test]
fn read_trimmed_is_none_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_trimmed(&dir.path().join("absent")), None);
}

#[test]
fn read_integer_helpers_parse_and_reject() {
    let dir = tempfile::tempdir().unwrap();
    let num = dir.path().join("num");
    fs::write(&num, "1500000\n").unwrap();
    assert_eq!(read_i64(&num), Some(1_500_000));
    assert_eq!(read_u64(&num), Some(1_500_000));
    assert_eq!(read_u32(&num), Some(1_500_000));

    let txt = dir.path().join("txt");
    fs::write(&txt, "performance\n").unwrap();
    assert_eq!(read_i64(&txt), None);
    assert_eq!(read_u64(&txt), None);
    assert_eq!(read_u32(&txt), None);
}

#[test]
fn list_dir_is_sorted_and_empty_for_a_missing_dir() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("b")).unwrap();
    fs::create_dir_all(dir.path().join("a")).unwrap();
    fs::create_dir_all(dir.path().join("c")).unwrap();
    assert_eq!(list_dir(dir.path()), vec!["a", "b", "c"]);
    assert!(list_dir(&dir.path().join("nope")).is_empty());
}
