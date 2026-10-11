use super::*;
use std::fs;

fn write_compat(root: &Path, body: &[u8]) {
    let p = root.join("proc/device-tree/compatible");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn detects_broadcom_pi() {
    let dir = tempfile::tempdir().unwrap();
    write_compat(dir.path(), b"raspberrypi,4-model-b\0brcm,bcm2711\0");
    let soc = detect_soc(dir.path());
    assert_eq!(soc.family, SocFamily::Broadcom);
    assert_eq!(soc.compat, "raspberrypi,4-model-b");
}

#[test]
fn detects_non_pi_family_as_other() {
    let dir = tempfile::tempdir().unwrap();
    write_compat(dir.path(), b"radxa,cubie-a7z\0allwinner,sun60i-a733\0");
    let soc = detect_soc(dir.path());
    assert_eq!(soc.family, SocFamily::Other);
    assert_eq!(soc.compat, "radxa,cubie-a7z");
}

#[test]
fn rockchip_is_other() {
    let dir = tempfile::tempdir().unwrap();
    write_compat(dir.path(), b"radxa,rock-5c\0rockchip,rk3588s\0");
    assert_eq!(detect_soc(dir.path()).family, SocFamily::Other);
}

#[test]
fn missing_node_is_other_with_empty_compat() {
    let dir = tempfile::tempdir().unwrap();
    let soc = detect_soc(dir.path());
    assert_eq!(soc.family, SocFamily::Other);
    assert_eq!(soc.compat, "");
}
